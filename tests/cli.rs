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
