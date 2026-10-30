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

#[cfg(test)]
mod tests {
    use super::SCRIPT;
    use std::process::Command;

    /// An interpreter to run the script's functions under, if the machine has one.
    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|p| {
            Command::new(p)
                .args([
                    "-I",
                    "-c",
                    "import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)",
                ])
                .output()
                .is_ok_and(|o| o.status.success())
        })
    }

    /// Runs `gpu_fit` on a probe result and returns the `(level, detail)` lines.
    fn fit(python: &str, probe: &str, min_vram: Option<&str>) -> Vec<(String, String)> {
        let functions = SCRIPT.split("\n\nmain()").next().unwrap();
        let program = format!(
            "{functions}\nimport json\nfor level, _, detail in gpu_fit(json.loads({probe:?})):\n    print(level + '\\t' + detail)\n"
        );
        let mut cmd = Command::new(python);
        cmd.args(["-I", "-c", &program]);
        if let Some(v) = min_vram {
            cmd.arg(format!("--min-vram={v}"));
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| {
                let (level, detail) = l.split_once('\t').unwrap();
                (level.to_owned(), detail.to_owned())
            })
            .collect()
    }

    #[test]
    fn gpu_fit_checks_every_device_for_kernels_and_free_memory() {
        let Some(python) = python() else {
            eprintln!("no python on PATH; skipping");
            return;
        };
        // Compiled kernels for the card: fine. Memory is judged on what is free.
        let probe = r#"{"archs": ["sm_80", "sm_90", "compute_90"], "devices": [
            {"index": 0, "name": "H100", "capability": "sm_90", "vram_gb": 79.6, "free_gb": 20.0, "matmul_ok": true, "kernel_ok": true}]}"#;
        let lines = fit(python, probe, None);
        assert_eq!(lines[0].0, "ok");
        assert!(lines[0].1.contains("kernels present"), "{lines:?}");
        assert!(
            lines[1].1.contains("79.6 GB of memory, 20.0 GB free"),
            "{lines:?}"
        );
        let lines = fit(python, probe, Some("48"));
        assert_eq!(lines[1].0, "ERR");
        assert!(lines[1].1.contains("48 GB free"), "{lines:?}");
        // The comparison is exact: 47.96 GB free is not 48, whatever the report rounds to.
        let close = probe.replace("\"free_gb\": 20.0", "\"free_gb\": 47.96");
        let lines = fit(python, &close, Some("48"));
        assert_eq!(lines[1].0, "ERR", "{lines:?}");
        assert!(
            lines[1].1.contains("48.0 GB free; at least 48 GB free"),
            "{lines:?}"
        );
        // Free memory that could not be read is no pass against a minimum.
        let unread = probe.replace("\"free_gb\": 20.0", "\"free_gb\": null");
        let lines = fit(python, &unread, Some("48"));
        assert_eq!(lines[1].0, "ERR");
        assert!(lines[1].1.contains("could not be read"), "{lines:?}");
        assert_eq!(fit(python, &unread, None)[1].0, "ok");
        // Only PTX for a newer major: a warning (JIT on first use), not a pass.
        let probe = r#"{"archs": ["sm_80", "compute_80"], "devices": [
            {"index": 0, "name": "H100", "capability": "sm_90", "vram_gb": 40.0, "free_gb": 39.0, "matmul_ok": true, "kernel_ok": true}]}"#;
        let lines = fit(python, probe, Some("48"));
        assert_eq!(lines[0].0, "WARN");
        assert!(
            lines[0].1.contains("JIT-compile PTX (compute_80)"),
            "{lines:?}"
        );
        assert_eq!(lines[1].0, "ERR");
        assert!(lines[1].1.contains("48 GB installed"), "{lines:?}");
        // Every device is checked, and a kernel that ran wrong is an error.
        // sm_100 is 10.0: nothing for major 10 among these binaries, and
        // no PTX at or below it either.
        let probe = r#"{"archs": ["sm_80", "sm_90", "sm_120", "compute_120"], "devices": [
            {"index": 0, "name": "H100", "capability": "sm_90", "vram_gb": 80.0, "free_gb": 80.0, "matmul_ok": true, "kernel_ok": true},
            {"index": 1, "name": "B200", "capability": "sm_100", "vram_gb": 180.0, "free_gb": 180.0, "matmul_ok": true, "kernel_ok": false, "error": "got 1.0, expected 2.0"},
            {"index": 2, "name": null, "capability": null, "vram_gb": null, "free_gb": null, "matmul_ok": null, "kernel_ok": null, "error": "RuntimeError: device busy"}]}"#;
        let lines = fit(python, probe, None);
        let errors: Vec<&str> = lines
            .iter()
            .filter(|(l, _)| l == "ERR")
            .map(|(_, d)| d.as_str())
            .collect();
        assert_eq!(errors.len(), 3, "{lines:?}");
        assert!(
            errors[0].contains("no CUDA kernels for GPU 1 (B200) (sm_100)"),
            "{errors:?}"
        );
        assert!(
            errors[1].contains("wrong result on GPU 1 (B200)"),
            "{errors:?}"
        );
        // A device the probe could not even query is its own error, not a pass.
        assert!(
            errors[2].contains("could not probe GPU 2 (None): RuntimeError: device busy"),
            "{errors:?}"
        );
        assert!(lines[0].1.contains("GPU 0 (H100)"), "{lines:?}");
        // Multi-digit majors: sm_120 binaries fit a 12.0 card, compute_90
        // PTX is only a warning for a 10.0 card, and sm_100 is not 1.0.
        let probe = r#"{"archs": ["sm_90", "compute_90", "sm_120"], "devices": [
            {"index": 0, "name": "RTX PRO 6000", "capability": "sm_120", "vram_gb": 95.6, "free_gb": 93.9, "matmul_ok": true, "kernel_ok": true},
            {"index": 1, "name": "B200", "capability": "sm_100", "vram_gb": 180.0, "free_gb": 180.0, "matmul_ok": true, "kernel_ok": true}]}"#;
        let lines = fit(python, probe, None);
        assert_eq!(lines[0].0, "ok", "{lines:?}");
        assert!(
            lines[0].1.contains("kernels present for GPU 0"),
            "{lines:?}"
        );
        assert_eq!(lines[2].0, "WARN", "{lines:?}");
        assert!(
            lines[2].1.contains("JIT-compile PTX (compute_90)"),
            "{lines:?}"
        );
        let probe = r#"{"archs": ["sm_100", "compute_100"], "devices": [
            {"index": 0, "name": "RTX PRO 6000", "capability": "sm_120", "vram_gb": 95.6, "free_gb": 93.9, "matmul_ok": true, "kernel_ok": true}]}"#;
        let lines = fit(python, probe, None);
        assert_eq!(lines[0].0, "WARN", "{lines:?}");
    }
}
