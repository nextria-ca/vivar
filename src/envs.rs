//! `vivar env list` and `vivar clean`: what vivar keeps on disk and how big it is.

use crate::{dirs, install, project, python, tool};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

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
    let store = dirs::cache()?.join(install::STORE);
    let entries = each_dir(&store);
    if !entries.is_empty() {
        let unused = entries.iter().filter(|e| !linked_elsewhere(e)).count();
        println!(
            "store     {:<40} {}",
            format!(
                "{} wheel(s) in {}{}",
                entries.len(),
                store.display(),
                match unused {
                    0 => String::new(),
                    n => format!(", {n} unused"),
                }
            ),
            mb(size_of(&store))
        );
    }
    let downloads = download_caches()?;
    let size: u64 = downloads.iter().map(|d| size_of(d)).sum();
    if size > 0 {
        println!(
            "downloads {:<40} {}",
            format!("wheels, sdists and builds in {}", dirs::cache()?.display()),
            mb(size)
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

/// The native backend's download and build caches: safe to delete, refilled
/// on demand.
fn download_caches() -> Result<Vec<PathBuf>> {
    let cache = dirs::cache()?;
    Ok(["wheels", "sdists", "built", "index", "metadata"]
        .iter()
        .map(|d| cache.join(d))
        .collect())
}

/// Whether some environment still hard-links a file of this store entry.
/// On Unix a link count above one says so; Windows has no cheap answer, so
/// every entry counts as linked and `clean --store` removes the store whole
/// (environments keep their own links).
fn linked_elsewhere(entry: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fn any_linked(dir: &Path) -> bool {
            fs::read_dir(dir)
                .map(|rd| {
                    rd.flatten().any(|e| {
                        let path = e.path();
                        match fs::symlink_metadata(&path) {
                            Ok(m) if m.is_dir() => any_linked(&path),
                            Ok(m) if m.is_file() => m.nlink() > 1,
                            _ => false,
                        }
                    })
                })
                .unwrap_or(true)
        }
        any_linked(entry)
    }
    #[cfg(not(unix))]
    {
        let _ = entry;
        true
    }
}

/// Removes store entries no environment links any more (`store` removes the
/// whole store; the environments keep their files). Returns bytes freed and
/// entries removed.
pub fn collect_store(whole: bool) -> Result<(u64, usize)> {
    let store = dirs::cache()?.join(install::STORE);
    let mut freed = 0;
    let mut removed = 0;
    for entry in each_dir(&store) {
        if !whole && linked_elsewhere(&entry) {
            continue;
        }
        freed += size_of(&entry);
        // Files are read-only on Unix, which is no obstacle to unlinking
        // them; the directories are ours.
        fs::remove_dir_all(&entry)?;
        removed += 1;
    }
    Ok((freed, removed))
}

/// Removes script environments, download caches, unlinked store entries and
/// the uv cache; `tools` also removes tool envs, `store` the whole
/// unpacked-wheel store.
pub fn clean(tools: bool, store: bool) -> Result<()> {
    let scripts = dirs::cache()?.join("scripts");
    if scripts.exists() {
        let size = size_of(&scripts);
        fs::remove_dir_all(&scripts)?;
        println!("removed script environments ({})", mb(size));
    }
    let mut downloads = 0;
    for dir in download_caches()? {
        if dir.exists() {
            downloads += size_of(&dir);
            fs::remove_dir_all(&dir)?;
        }
    }
    if downloads > 0 {
        println!(
            "removed downloaded wheels and build caches ({})",
            mb(downloads)
        );
    }
    if tools {
        let dir = tool::tools_dir()?;
        if dir.exists() {
            let size = size_of(&dir);
            for env in each_dir(&dir) {
                tool::remove_links(&env);
            }
            fs::remove_dir_all(&dir)?;
            println!(
                "removed tool environments and their linked commands ({})",
                mb(size)
            );
        }
    }
    // After the tool environments: wheels only they linked are unused now.
    let (freed, entries) = collect_store(store || cfg!(not(unix)))?;
    if entries > 0 {
        println!(
            "removed {entries} unpacked wheel(s) from the store ({})",
            mb(freed)
        );
    }
    let conda = dirs::cache()?.join("conda");
    if conda.exists() {
        let size = size_of(&conda);
        fs::remove_dir_all(&conda)?;
        println!(
            "removed the conda package and repodata cache ({})",
            mb(size)
        );
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
