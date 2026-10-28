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
#[cfg(windows)]
const ACTIVATE_BAT: &str = include_str!("venv/activate.bat");
#[cfg(windows)]
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

    let fill = |template: &str| {
        template
            .replace("__VENV_DIR__", &venv.to_string_lossy())
            .replace("__VENV_PROMPT__", prompt)
            .replace("__VENV_BIN__", BIN)
    };
    fs::write(bin.join("activate"), fill(ACTIVATE_SH))?;
    fs::write(bin.join("Activate.ps1"), fill(ACTIVATE_PS1))?;
    #[cfg(windows)]
    {
        fs::write(bin.join("activate.bat"), fill(ACTIVATE_BAT))?;
        fs::write(bin.join("deactivate.bat"), DEACTIVATE_BAT)?;
    }
    Ok(())
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

/// Runs `args` with the env's bin directory first on PATH. On Unix this execs,
/// so the command owns the terminal and signals; Windows waits for it.
pub fn run(root: &Path, args: &[String]) -> Result<i32> {
    let venv = root.join(".venv");
    let bin = venv.join(BIN);
    if !venv.join("pyvenv.cfg").exists() {
        bail!("{} has no .venv yet; run `vivar sync`", root.display());
    }
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
        .env("VIRTUAL_ENV", &venv)
        .env("PATH", path)
        .env_remove("PYTHONHOME");

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
