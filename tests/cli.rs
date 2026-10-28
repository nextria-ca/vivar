//! End-to-end tests against the real `uv` binary. They need `uv` on PATH and
//! network access the first time a Python version or package is fetched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        "vivar {} failed:\n{}",
        args.join(" "),
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

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

fn init(dir: &Path) {
    ok(dir, &["init", "demo", "--python", "3.12"]);
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello from vivar"));
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
    // pycodestyle ships a console script, so its shebang must survive the move.
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
    assert!(listing.contains(".venv/bin/activate"), "{listing}");
    assert!(
        !listing.contains("__pycache__"),
        "pycache should be excluded"
    );
    assert!(
        !listing.contains("/lib/python3.12/test/"),
        "stdlib tests should be excluded"
    );

    // "Other machine": a different directory, and the source project is gone.
    let dest = tmp.0.join("dest-machine").join("app");
    ok(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
    );
    fs::remove_dir_all(&src).unwrap();

    let python = dest.join(".venv").join("bin").join("python");
    let out = Command::new(&python)
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    let dest_str = dest.canonicalize().unwrap().to_string_lossy().into_owned();
    for line in stdout.lines() {
        assert!(
            line.starts_with(&dest_str),
            "{line} is outside the unpacked bundle"
        );
    }

    // Console script shebang points at the new venv.
    let out = Command::new(dest.join(".venv/bin/pycodestyle"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Activation works in a plain POSIX shell.
    let script = format!(
        ". '{0}/.venv/bin/activate' && printf '%s\\n' \"$VIRTUAL_ENV\" && command -v python && python -c 'import six'",
        dest.display()
    );
    let out = Command::new("sh").args(["-c", &script]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    assert_eq!(lines.next().unwrap(), format!("{}/.venv", dest.display()));
    assert!(lines.next().unwrap().starts_with(dest.to_str().unwrap()));

    fails_with(
        &tmp.0,
        &["unpack", archive.to_str().unwrap(), dest.to_str().unwrap()],
        "already contains",
    );

    // verify works on the bundle with no uv anywhere on PATH.
    let out = Command::new(env!("CARGO_BIN_EXE_vivar"))
        .current_dir(&tmp.0)
        .env("PATH", "/usr/bin:/bin")
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

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
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

    let site = fs::read_dir(dir.join(".venv/lib"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("python")
        })
        .unwrap()
        .join("site-packages");
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
        site.join("gpu_thing/__init__.py"),
        "import ctypes\nctypes.CDLL('libcudart.so.99')\n",
    )
    .unwrap();
    // An environment that declares CUDA packages.
    dist_info("nvidia-cuda-runtime-cu99", "");
    // A script copied from somewhere else.
    fs::write(
        dir.join(".venv/bin/stale"),
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
        stdout.contains("ERR  gpu-thing: import gpu_thing failed: OSError: libcudart.so.99"),
        "{stdout}"
    );
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
