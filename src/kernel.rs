//! `vivar kernel`: register the project environment as a Jupyter kernel.

use crate::{dirs, venv};
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Jupyter's per-user kernelspec directory.
fn kernels_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("JUPYTER_DATA_DIR") {
        return Ok(PathBuf::from(dir).join("kernels"));
    }
    if cfg!(windows) {
        return Ok(
            PathBuf::from(std::env::var_os("APPDATA").context("APPDATA is not set")?)
                .join("jupyter")
                .join("kernels"),
        );
    }
    if cfg!(target_os = "macos") {
        return Ok(
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
                .join("Library")
                .join("Jupyter")
                .join("kernels"),
        );
    }
    // dirs::data() is ~/.local/share/vivar; Jupyter wants its sibling.
    Ok(dirs::data()?
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default()
        .join("jupyter")
        .join("kernels"))
}

fn slug(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

pub fn install(root: &Path, project_name: &str, name: Option<&str>) -> Result<()> {
    let venv_dir = root.join(".venv");
    if !venv_dir.join("pyvenv.cfg").exists() {
        bail!("{} has no .venv yet; run `vivar sync`", root.display());
    }
    let site = venv::site_packages(&venv_dir)?;
    let has_ipykernel = fs::read_dir(&site)?
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with("ipykernel-"));
    if !has_ipykernel {
        bail!("ipykernel is not installed in this environment; run `vivar add --group dev ipykernel` first");
    }
    let name = slug(name.unwrap_or(project_name));
    let dir = kernels_dir()?.join(&name);
    fs::create_dir_all(&dir)?;
    let python = venv::python(&venv_dir);
    let spec = format!(
        "{{\n  \"argv\": [{}, \"-m\", \"ipykernel_launcher\", \"-f\", \"{{connection_file}}\"],\n  \"display_name\": \"{} (vivar)\",\n  \"language\": \"python\",\n  \"metadata\": {{\"vivar\": {}}}\n}}\n",
        crate::json::quote(&python.to_string_lossy()),
        project_name,
        crate::json::quote(&root.to_string_lossy())
    );
    fs::write(dir.join("kernel.json"), spec)?;
    println!("installed Jupyter kernel `{name}` at {}", dir.display());
    Ok(())
}

pub fn uninstall(name: &str) -> Result<()> {
    let dir = kernels_dir()?.join(slug(name));
    if !dir.join("kernel.json").exists() {
        bail!("no Jupyter kernel named {}", slug(name));
    }
    fs::remove_dir_all(&dir)?;
    println!("removed Jupyter kernel `{}`", slug(name));
    Ok(())
}

pub fn list() -> Result<()> {
    let dir = kernels_dir()?;
    let mut any = false;
    if let Ok(entries) = fs::read_dir(&dir) {
        let mut names: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        names.sort();
        for kernel in names {
            let Ok(spec) = fs::read_to_string(kernel.join("kernel.json")) else {
                continue;
            };
            let managed = spec.contains("\"vivar\"");
            any = true;
            println!(
                "{:<24} {}{}",
                kernel.file_name().unwrap_or_default().to_string_lossy(),
                kernel.display(),
                if managed { " (vivar)" } else { "" }
            );
        }
    }
    if !any {
        println!("no Jupyter kernels in {}", dir.display());
    }
    Ok(())
}
