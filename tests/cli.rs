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
