//! `vivar env list` and `vivar clean`: what vivar keeps on disk and how big it is.

use crate::{dirs, project, python, tool};
use anyhow::Result;
use std::fs;
use std::path::Path;

fn size_of(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        fs::read_dir(path)
            .map(|rd| rd.flatten().map(|e| size_of(&e.path())).sum())
            .unwrap_or(0)
    } else if meta.is_file() {
        meta.len()
    } else {
        0
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn each_dir(path: &Path) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<_> = fs::read_dir(path)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

pub fn list(cwd: &Path) -> Result<()> {
    if let Some(project) = project::Project::find(cwd)? {
        let venv = project.venv_root().join(".venv");
        if venv.exists() {
            println!("project   {:<40} {}", venv.display(), mb(size_of(&venv)));
        }
    }
    for p in python::installed()? {
        println!("python    {:<40} {}", p.key, mb(size_of(&p.root)));
    }
    for dir in each_dir(&tool::tools_dir()?) {
        println!(
            "tool      {:<40} {}",
            dir.file_name().unwrap_or_default().to_string_lossy(),
            mb(size_of(&dir))
        );
    }
    let scripts = dirs::cache()?.join("scripts");
    let script_envs = each_dir(&scripts);
    if !script_envs.is_empty() {
        println!(
            "scripts   {:<40} {}",
            format!("{} env(s) in {}", script_envs.len(), scripts.display()),
            mb(size_of(&scripts))
        );
    }
    if let Ok(out) = std::process::Command::new("uv")
        .args(["cache", "dir"])
        .output()
    {
        if out.status.success() {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            println!("uv cache  {:<40} {}", dir, mb(size_of(Path::new(&dir))));
        }
    }
    Ok(())
}

/// Removes script environments and the uv cache; `tools` also removes tool envs.
pub fn clean(tools: bool) -> Result<()> {
    let scripts = dirs::cache()?.join("scripts");
    if scripts.exists() {
        let size = size_of(&scripts);
        fs::remove_dir_all(&scripts)?;
        println!("removed script environments ({})", mb(size));
    }
    if tools {
        let dir = tool::tools_dir()?;
        if dir.exists() {
            let size = size_of(&dir);
            fs::remove_dir_all(&dir)?;
            println!("removed tool environments ({})", mb(size));
        }
    }
    if let Ok(status) = std::process::Command::new("uv")
        .args(["cache", "clean", "--quiet"])
        .status()
    {
        if status.success() {
            println!("cleared the uv cache");
        }
    }
    Ok(())
}
