//! `vivar doctor`: what's installed, what's configured, what's broken.

use crate::{backend, dirs, project, python, venv};
use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub fn run(cwd: &Path) -> Result<i32> {
    let mut problems = 0;
    let mut line = |ok: bool, text: String| {
        println!("{} {text}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            problems += 1;
        }
    };

    let exe = std::env::current_exe().unwrap_or_default();
    line(
        true,
        format!("vivar {} at {}", env!("CARGO_PKG_VERSION"), exe.display()),
    );
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| exe.parent() == Some(d.as_path())))
        .unwrap_or(false);
    if !on_path {
        println!(
            "warn {} is not on PATH",
            exe.parent().unwrap_or(&exe).display()
        );
    }

    for tool in ["tar", "curl"] {
        let found = Command::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        line(
            found,
            format!("{tool} available (needed for bundles and downloads)"),
        );
    }
    match Command::new("uv").arg("--version").output() {
        Ok(o) if o.status.success() => line(
            true,
            format!(
                "uv: {} (resolves and installs packages)",
                String::from_utf8_lossy(&o.stdout).trim()
            ),
        ),
        _ => println!("warn uv not found: add/remove/lock/sync won't work until it is installed"),
    }
    let _ = backend::uv_available();

    match python::installed() {
        Ok(list) if !list.is_empty() => {
            line(
                true,
                format!(
                    "{} managed Python build(s) in {}",
                    list.len(),
                    python::dir()?.display()
                ),
            );
            for p in &list {
                let runs = Command::new(p.executable())
                    .arg("-c")
                    .arg("pass")
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                line(runs, format!("  {} runs", p.key));
            }
        }
        _ => println!(
            "warn no managed Python in {}; `vivar python install 3.12`",
            python::dir()?.display()
        ),
    }

    match project::org_policy()? {
        Some((path, policy)) => line(
            true,
            format!(
                "org policy {} (deny {}, licenses {}, require-hashes {})",
                path.display(),
                policy.deny.len(),
                policy.allow_licenses.as_ref().map_or(0, Vec::len),
                policy.require_hashes
            ),
        ),
        None => println!(
            "info no org policy file (VIVAR_POLICY or {})",
            dirs::config()?.join("policy.toml").display()
        ),
    }

    match project::Project::find(cwd)? {
        None => println!("info not inside a project"),
        Some(project) => {
            line(
                true,
                format!("project `{}` at {}", project.name(), project.root.display()),
            );
            let venv_dir = project.venv_root().join(".venv");
            if venv_dir.join("pyvenv.cfg").is_file() {
                let runs = Command::new(venv::python(&venv_dir))
                    .arg("-c")
                    .arg("import sys; sys.exit(0)")
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                line(
                    runs,
                    format!(".venv interpreter runs ({})", venv_dir.display()),
                );
            } else {
                println!("warn no .venv yet; run `vivar sync`");
            }
            let policy = project.policy()?;
            if let Some(index) = &policy.index_url {
                let ok =
                    !index.contains("://") && Path::new(index).is_dir() || index.contains("://");
                line(ok, format!("index-url {index}"));
            }
            match project.check_declared() {
                Ok(()) => line(true, "declared dependencies pass the deny list".to_owned()),
                Err(e) => line(false, e.to_string()),
            }
        }
    }
    Ok(if problems == 0 { 0 } else { 1 })
}
