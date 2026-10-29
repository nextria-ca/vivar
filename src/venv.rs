//! Virtual environments and running commands in them, without uv.

use crate::python::{self, Installed};
use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `Scripts` on Windows, `bin` elsewhere.
pub const BIN: &str = if cfg!(windows) { "Scripts" } else { "bin" };

const ACTIVATE_SH: &str = include_str!("venv/activate");
const ACTIVATE_PS1: &str = include_str!("venv/Activate.ps1");
const ACTIVATE_FISH: &str = include_str!("venv/activate.fish");
const ACTIVATE_CSH: &str = include_str!("venv/activate.csh");
/// Hash of the lockfile the env was last synced from, for drift warnings.
const SYNCED: &str = "vivar-synced.sha256";
const ACTIVATE_BAT: &str = include_str!("venv/activate.bat");
const DEACTIVATE_BAT: &str = include_str!("venv/deactivate.bat");

/// Creates a fresh `venv` on `python`. Refuses to touch an existing one.
pub fn create(venv: &Path, python: &Installed, prompt: &str) -> Result<()> {
    if venv.join("pyvenv.cfg").exists() {
        bail!("{} already exists; delete it to recreate", venv.display());
    }
    let venv = crate::bundle::absolute(venv)?;
    let bin = venv.join(BIN);
    fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    #[cfg(unix)]
    let home = {
        use std::os::unix::fs::symlink;
        let [major, minor, _] = python.version;
        symlink(python.executable(), bin.join("python"))?;
        symlink("python", bin.join("python3"))?;
        symlink("python", bin.join(format!("python{major}.{minor}")))?;
        let lib = venv.join("lib").join(format!("python{major}.{minor}"));
        fs::create_dir_all(lib.join("site-packages"))?;
        if cfg!(target_os = "linux") {
            symlink("lib", venv.join("lib64"))?;
        }
        python.root.join("bin")
    };
    #[cfg(windows)]
    let home = {
        // CPython's own venv launchers: they read pyvenv.cfg and start `home\python.exe`.
        let launchers = python
            .root
            .join("Lib")
            .join("venv")
            .join("scripts")
            .join("nt");
        for exe in ["python.exe", "pythonw.exe"] {
            fs::copy(launchers.join(exe), bin.join(exe)).with_context(|| {
                format!("copying the venv launcher from {}", launchers.display())
            })?;
        }
        fs::create_dir_all(venv.join("Lib").join("site-packages"))?;
        python.root.clone()
    };

    fs::write(
        venv.join("pyvenv.cfg"),
        format!(
            "home = {}\nimplementation = CPython\nversion = {v}\nversion_info = {v}\n\
             include-system-site-packages = false\nprompt = {prompt}\ncreated-by = vivar {}\n",
            home.display(),
            env!("CARGO_PKG_VERSION"),
            v = python.version_string(),
        ),
    )?;
    fs::write(venv.join(".gitignore"), "*\n")?;

    write_activate_scripts(&bin, &venv.to_string_lossy(), prompt, BIN, cfg!(windows))
}

/// Writes the activate scripts into `bin` for a venv at `venv_dir` (as the
/// scripts should spell it). `windows` adds the cmd scripts.
pub fn write_activate_scripts(
    bin: &Path,
    venv_dir: &str,
    prompt: &str,
    bin_name: &str,
    windows: bool,
) -> Result<()> {
    let fill = |template: &str| {
        template
            .replace("__VENV_DIR__", venv_dir)
            .replace("__VENV_PROMPT__", prompt)
            .replace("__VENV_BIN__", bin_name)
    };
    fs::write(bin.join("activate"), fill(ACTIVATE_SH))?;
    fs::write(bin.join("Activate.ps1"), fill(ACTIVATE_PS1))?;
    fs::write(bin.join("activate.fish"), fill(ACTIVATE_FISH))?;
    fs::write(bin.join("activate.csh"), fill(ACTIVATE_CSH))?;
    if windows {
        fs::write(bin.join("activate.bat"), fill(ACTIVATE_BAT))?;
        fs::write(bin.join("deactivate.bat"), DEACTIVATE_BAT)?;
    }
    Ok(())
}

/// Removes what `create` put into `venv`, leaving any other files there alone
/// (a rollback for an environment the failed operation created in place).
pub fn remove_created(venv: &Path) {
    for name in ["pyvenv.cfg", ".gitignore", SYNCED, PROFILE] {
        let _ = fs::remove_file(venv.join(name));
    }
    for dir in [BIN, "lib", "Lib", "include", "share", "etc"] {
        let _ = fs::remove_dir_all(venv.join(dir));
    }
    let _ = fs::remove_file(venv.join("lib64"));
    // Empty afterwards: it was ours entirely.
    if fs::read_dir(venv)
        .map(|mut d| d.next().is_none())
        .unwrap_or(false)
    {
        let _ = fs::remove_dir(venv);
    }
}

/// Makes sure `root/.venv` exists, creating it on the project's pinned Python
/// (`.python-version`, else the newest installed, else a fresh download).
pub fn ensure(root: &Path, prompt: &str) -> Result<()> {
    let venv = root.join(".venv");
    if venv.join("pyvenv.cfg").exists() {
        return Ok(());
    }
    let pin = fs::read_to_string(root.join(".python-version"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let python = interpreter(pin.as_deref())?;
    create(&venv, &python, prompt)?;
    println!("Created .venv on CPython {}", python.version_string());
    Ok(())
}

/// An installed build matching `request`, downloading one if needed.
pub fn interpreter(request: Option<&str>) -> Result<Installed> {
    match python::find(request)? {
        Some(p) => Ok(p),
        None => python::install(request.unwrap_or("3")),
    }
}

/// A cheap fingerprint of what is installed: the dist-info directories and
/// their modification times. Changes whenever a package is added, removed or
/// replaced, even partially.
pub fn fingerprint(venv: &Path) -> String {
    let Ok(site) = site_packages(venv) else {
        return String::new();
    };
    let mut entries: Vec<String> = fs::read_dir(&site)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".dist-info"))
                .map(|e| {
                    let mtime = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos())
                        .unwrap_or(0);
                    format!("{}@{mtime}", e.file_name().to_string_lossy())
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    entries.join("\n")
}

/// Which profile `.venv` was last synced with, so `pack` keeps it.
const PROFILE: &str = "vivar-profile";

pub fn mark_profile(root: &Path, profile: Option<&str>) {
    let path = root.join(".venv").join(PROFILE);
    match profile {
        Some(p) => {
            let _ = fs::write(path, p);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn current_profile(root: &Path) -> Option<String> {
    fs::read_to_string(root.join(".venv").join(PROFILE))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Records that `root/.venv` matches `root/uv.lock` right now.
pub fn mark_synced(root: &Path) {
    let venv = root.join(".venv");
    if let Ok(digest) = crate::sha256::file(&root.join(crate::lock::LOCKFILE)) {
        let _ = fs::write(venv.join(SYNCED), digest);
    }
}

/// One line on stderr when the lockfile changed since the last vivar sync.
/// Silent when vivar never synced this env (someone else may keep it current).
pub fn warn_if_stale(root: &Path) {
    let venv = root.join(".venv");
    let (Ok(recorded), Ok(current)) = (
        fs::read_to_string(venv.join(SYNCED)),
        crate::sha256::file(&root.join(crate::lock::LOCKFILE)),
    ) else {
        return;
    };
    if recorded.trim() != current {
        eprintln!("note: uv.lock changed since the last sync; run `vivar sync` to update .venv");
    }
}

/// `vivar shell`: an interactive shell with the env activated.
pub fn shell(root: &Path, prompt: &str, extra: &[(String, String)]) -> Result<i32> {
    let venv = root.join(".venv");
    if !venv.join("pyvenv.cfg").exists() {
        bail!("{} has no .venv yet; run `vivar sync`", root.display());
    }
    let bin = venv.join(BIN);
    let mut path = OsString::from(bin.as_os_str());
    path.push(if cfg!(windows) { ";" } else { ":" });
    path.push(env::var_os("PATH").unwrap_or_default());

    let shell = env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .or_else(|| env::var_os("COMSPEC"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }));
    let mut cmd = Command::new(&shell);
    cmd.env("VIRTUAL_ENV", &venv)
        .env("VIRTUAL_ENV_PROMPT", format!("({prompt}) "))
        .env("PATH", path)
        .env_remove("PYTHONHOME");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let name = shell.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    if matches!(name, "bash" | "zsh" | "sh" | "dash") {
        cmd.env("PS1", format!(r"({prompt}) \u@\h:\w\$ "));
    } else if name == "cmd" {
        cmd.env("PROMPT", format!("({prompt}) $P$G"));
    }
    println!(
        "entering the {} environment; exit the shell to leave",
        venv.display()
    );
    let status = cmd
        .status()
        .with_context(|| format!("failed to start {}", shell.display()))?;
    Ok(status.code().unwrap_or(1))
}

/// The env's interpreter.
pub fn python(venv: &Path) -> PathBuf {
    let bin = venv.join(BIN);
    if cfg!(windows) {
        bin.join("python.exe")
    } else {
        bin.join("python")
    }
}

/// The env's `site-packages` directory.
pub fn site_packages(venv: &Path) -> Result<PathBuf> {
    if cfg!(windows) {
        return Ok(venv.join("Lib").join("site-packages"));
    }
    let lib = venv.join("lib");
    for entry in fs::read_dir(&lib)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with("python") {
            return Ok(entry.path().join("site-packages"));
        }
    }
    bail!("no site-packages under {}", lib.display())
}

/// Runs `args` in the project's `.venv` with `extra` environment variables.
pub fn run(root: &Path, args: &[String], extra: &[(String, String)]) -> Result<i32> {
    let venv = root.join(".venv");
    if !venv.join("pyvenv.cfg").exists() {
        bail!("{} has no .venv yet; run `vivar sync`", root.display());
    }
    exec_with(&venv, args, extra)
}

/// `KEY=VALUE` lines from a dotenv-style file: `#` comments, blank lines,
/// optional `export `, single or double quotes.
pub fn read_env_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut vars = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            bail!("{}: expected KEY=VALUE, got {line:?}", path.display());
        };
        let value = value.trim();
        let value = if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            value[1..value.len() - 1].to_owned()
        } else {
            value.split(" #").next().unwrap_or(value).trim().to_owned()
        };
        vars.push((key.trim().to_owned(), value));
    }
    Ok(vars)
}

/// Runs `args` with the env's bin directory first on PATH. On Unix this execs,
/// so the command owns the terminal and signals; Windows waits for it.
pub fn exec(venv: &Path, args: &[String]) -> Result<i32> {
    exec_with(venv, args, &[])
}

pub fn exec_with(venv: &Path, args: &[String], extra: &[(String, String)]) -> Result<i32> {
    let bin = venv.join(BIN);
    let (program, rest) = args
        .split_first()
        .ok_or_else(|| anyhow!("nothing to run"))?;

    let mut path = OsString::from(bin.as_os_str());
    path.push(if cfg!(windows) { ";" } else { ":" });
    path.push(env::var_os("PATH").unwrap_or_default());

    // The env's own entry points win over anything else on PATH.
    let exe = [bin.join(program), bin.join(format!("{program}.exe"))]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(program));
    let mut cmd = Command::new(&exe);
    cmd.args(rest)
        .env("VIRTUAL_ENV", venv)
        .env("PATH", path)
        .env_remove("PYTHONHOME");
    for (k, v) in extra {
        cmd.env(k, v);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(cmd.exec()).with_context(|| format!("failed to run {program}"))
    }
    #[cfg(windows)]
    {
        ignore_ctrl_c();
        let status = cmd
            .status()
            .with_context(|| format!("failed to run {program}"))?;
        Ok(status.code().unwrap_or(1))
    }
}

/// Ctrl-C goes to the whole console; the child handles it, we just wait.
#[cfg(windows)]
fn ignore_ctrl_c() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: *const core::ffi::c_void, add: i32) -> i32;
    }
    // SAFETY: a null handler with add=1 only sets the process's ignore flag.
    unsafe {
        SetConsoleCtrlHandler(std::ptr::null(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_files() {
        let path = std::env::temp_dir().join(format!("vivar-envfile-{}", std::process::id()));
        fs::write(
            &path,
            "# keys\nOPENAI_API_KEY=sk-123\nexport HF_TOKEN='hf_abc'\nMSG=\"hello world\" \nPLAIN=value # trailing\n\n",
        )
        .unwrap();
        let vars = read_env_file(&path).unwrap();
        assert_eq!(
            vars,
            vec![
                ("OPENAI_API_KEY".to_owned(), "sk-123".to_owned()),
                ("HF_TOKEN".to_owned(), "hf_abc".to_owned()),
                ("MSG".to_owned(), "hello world".to_owned()),
                ("PLAIN".to_owned(), "value".to_owned()),
            ]
        );
        fs::write(&path, "NOEQUALS\n").unwrap();
        assert!(read_env_file(&path).is_err());
        let _ = fs::remove_file(&path);
    }
}
