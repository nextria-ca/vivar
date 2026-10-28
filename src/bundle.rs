//! Portable bundles: `vivar pack` archives a synced `.venv` together with the
//! exact Python it uses; `vivar unpack` extracts it on another machine of the
//! same OS/arch and rewrites the few absolute paths a venv bakes in, so
//! `source .venv/bin/activate` just works there.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{value, DocumentMut};

pub const MANIFEST: &str = "vivar-bundle.toml";

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The `key = value` lines of `.venv/pyvenv.cfg`.
fn pyvenv_cfg(venv: &Path, key: &str) -> Result<String> {
    let path = venv.join("pyvenv.cfg");
    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading {}; is the environment synced?", path.display()))?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_owned())
        .ok_or_else(|| anyhow!("{} has no `{key}` entry", path.display()))
}

/// Archives `<root>/.venv`, the project files, and the Python install the venv
/// points at. Returns the archive path.
pub fn pack(root: &Path, name: &str, output: Option<&Path>) -> Result<PathBuf> {
    let venv = root.join(".venv");
    let home = PathBuf::from(pyvenv_cfg(&venv, "home")?);
    let version = pyvenv_cfg(&venv, "version_info")?;

    // `home` is `<python root>/bin`, often through a `cpython-3.12-…` symlink
    // directory that uv maintains; the real directory is what we ship.
    let python_root = fs::canonicalize(&home)
        .with_context(|| format!("resolving Python home {}", home.display()))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("unexpected Python home {}", home.display()))?;
    let python_dir = python_root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("unexpected Python root {}", python_root.display()))?
        .to_owned();
    if !python_dir.starts_with("cpython-") {
        bail!(
            "the environment uses {}, which is not a relocatable vivar-managed Python; \
             run `vivar python install {}` and `vivar sync`, then pack again",
            home.display(),
            major_minor(&version)
        );
    }

    let output = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("{name}-{}.tar.gz", platform())));
    let output = if output.is_absolute() {
        output
    } else {
        std::env::current_dir()?.join(output)
    };

    let mut manifest = DocumentMut::new();
    manifest["name"] = value(name);
    manifest["platform"] = value(platform());
    manifest["python"] = value(&version);
    manifest["python-dir"] = value(&python_dir);
    manifest["python-home"] = value(home.to_string_lossy().as_ref());
    manifest["venv"] = value(venv.to_string_lossy().as_ref());
    manifest["created-by"] = value(concat!("vivar ", env!("CARGO_PKG_VERSION")));

    let staging = std::env::temp_dir().join(format!("vivar-pack-{}", std::process::id()));
    fs::create_dir_all(&staging)?;
    fs::write(staging.join(MANIFEST), manifest.to_string())?;

    let mut tar = Command::new("tar");
    tar.arg("-czf")
        .arg(&output)
        .arg("--exclude=__pycache__")
        // CPython's own test suite: ~25 MB nobody runs from a bundle.
        .arg(format!(
            "--exclude={python_dir}/lib/python{}/test",
            major_minor(&version)
        ))
        .arg("-C")
        .arg(&staging)
        .arg(MANIFEST)
        .arg("-C")
        .arg(root)
        .arg(".venv");
    for file in ["pyproject.toml", "uv.lock", ".python-version"] {
        if root.join(file).is_file() {
            tar.arg(file);
        }
    }
    tar.arg("-C")
        .arg(python_root.parent().unwrap_or(&python_root))
        .arg(&python_dir);

    let status = tar.status().context("failed to run `tar`")?;
    let _ = fs::remove_dir_all(&staging);
    if !status.success() {
        bail!("tar exited with status {}", status.code().unwrap_or(1));
    }
    Ok(output)
}

/// Extracts a bundle into `dest` and points the venv at the Python that came
/// with it. Returns the canonical destination.
pub fn unpack(archive: &Path, dest: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let dest = fs::canonicalize(dest)?;
    if dest.join(".venv").exists() {
        bail!(
            "{} already contains a .venv; unpack into an empty directory",
            dest.display()
        );
    }

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&dest)
        .status()
        .context("failed to run `tar`")?;
    if !status.success() {
        bail!("tar exited with status {}", status.code().unwrap_or(1));
    }

    let manifest_path = dest.join(MANIFEST);
    let manifest: DocumentMut = fs::read_to_string(&manifest_path)
        .with_context(|| {
            format!(
                "{} is not a vivar bundle (no {MANIFEST})",
                archive.display()
            )
        })?
        .parse()
        .context("parsing bundle manifest")?;
    let field = |key: &str| -> Result<String> {
        manifest
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("bundle manifest is missing `{key}`"))
    };

    let bundle_platform = field("platform")?;
    if bundle_platform != platform() {
        bail!(
            "bundle was packed on {bundle_platform} but this machine is {}",
            platform()
        );
    }

    let old_venv = field("venv")?;
    let old_home = field("python-home")?;
    let new_venv = dest.join(".venv");
    let new_home = dest.join(field("python-dir")?).join("bin");
    let bin = new_venv.join("bin");

    // 1. `python`, `python3`, … symlinks pointed at the old Python install.
    for entry in fs::read_dir(&bin)? {
        let path = entry?.path();
        if !path.is_symlink() {
            continue;
        }
        let target = fs::read_link(&path)?;
        if let Ok(rest) = target.strip_prefix(&old_home) {
            relink(&path, &new_home.join(rest))?;
        }
    }

    // 2. Shebangs, activate scripts, and pyvenv.cfg carry the old paths as text.
    let replacements = [
        (old_venv.as_bytes(), new_venv.to_string_lossy().into_owned()),
        (old_home.as_bytes(), new_home.to_string_lossy().into_owned()),
    ];
    let mut files: Vec<PathBuf> = fs::read_dir(&bin)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && !p.is_symlink())
        .collect();
    files.push(new_venv.join("pyvenv.cfg"));
    for path in files {
        rewrite(&path, &replacements)?;
    }
    Ok(dest)
}

fn major_minor(version: &str) -> String {
    version.splitn(3, '.').take(2).collect::<Vec<_>>().join(".")
}

fn relink(link: &Path, target: &Path) -> Result<()> {
    fs::remove_file(link)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("linking {} -> {}", link.display(), target.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        bail!("vivar unpack is not supported on this platform yet")
    }
}

/// Replaces every occurrence of each old byte string in a text file.
/// Binaries (anything with a NUL in the first KiB) are left alone.
fn rewrite(path: &Path, replacements: &[(&[u8], String)]) -> Result<()> {
    let original = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if original.iter().take(1024).any(|&b| b == 0) {
        return Ok(());
    }
    let mut data = original.clone();
    for (old, new) in replacements {
        data = replace_bytes(&data, old, new.as_bytes());
    }
    if data != original {
        fs::write(path, data).with_context(|| format!("rewriting {}", path.display()))?;
    }
    Ok(())
}

fn replace_bytes(data: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(old) {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_all_occurrences() {
        assert_eq!(replace_bytes(b"a/x/b/x", b"/x", b"/yy"), b"a/yy/b/yy");
        assert_eq!(replace_bytes(b"nothing", b"zzz", b"y"), b"nothing");
        assert_eq!(replace_bytes(b"abc", b"", b"y"), b"abc");
    }

    #[test]
    fn major_minor_from_version_info() {
        assert_eq!(major_minor("3.12.12"), "3.12");
        assert_eq!(major_minor("3.13"), "3.13");
    }
}
