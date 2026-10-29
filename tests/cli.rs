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
    let why = stdout_of(&ok(dir, &["why", "pluggy"]));
    assert!(
        why.contains("demo v0.1.0 -> pytest v") && why.ends_with("pluggy v1.6.0\n")
            || why.contains("-> pluggy v"),
        "{why}"
    );
    fails_with(dir, &["why", "nope"], "not in the lockfile");

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

    // A clean unpack verifies; a tampered one is caught at unpack and by verify.
    let good = tmp.0.join("good");
    ok(
        &tmp.0,
        &["unpack", a.to_str().unwrap(), good.to_str().unwrap()],
    );
    assert!(stdout_of(&ok(&good, &["verify"])).contains("integrity: all bundled files match"));
    let six = site_packages(&good).join("six.py");
    let mut text = read(&six);
    text.push_str("\n# tampered\n");
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
}

#[test]
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
        assert!(String::from_utf8_lossy(&out.stderr).contains("Windows"));
        return;
    }
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
