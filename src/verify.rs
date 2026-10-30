//! `vivar verify`: runs `verify.py` with the environment's own interpreter to
//! check every installed package works on *this* machine. Needs no uv, so it
//! works on an unpacked bundle.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SCRIPT: &str = include_str!("verify.py");

/// Accepts a project/bundle directory or a venv itself.
pub fn find_venv(path: &Path) -> Result<PathBuf> {
    for candidate in [path.to_path_buf(), path.join(".venv")] {
        if candidate.join("pyvenv.cfg").is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "no environment found at {}; expected a .venv (run `vivar sync` or `vivar unpack` first)",
        path.display()
    )
}

/// Returns the exit code (0 when everything works) and, in JSON mode, the
/// report itself so the caller can extend it; text mode streams to stdout.
pub fn run(
    venv: &Path,
    allow_missing_gpu: bool,
    verbose: bool,
    json: bool,
    min_vram_gb: Option<f64>,
) -> Result<(i32, String)> {
    let python = if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    };
    let mut cmd = Command::new(&python);
    cmd.args(["-I", "-"]).stdin(Stdio::piped());
    if allow_missing_gpu {
        cmd.arg("--allow-missing-gpu");
    }
    if verbose {
        cmd.arg("--verbose");
    }
    if let Some(gb) = min_vram_gb {
        cmd.arg(format!("--min-vram={gb}"));
    }
    if json {
        cmd.arg("--json").stdout(Stdio::piped());
    }
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "the environment's Python ({}) does not start; the env is broken on this machine",
            python.display()
        )
    })?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(SCRIPT.as_bytes())?;
    let out = child.wait_with_output()?;
    Ok((
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}
