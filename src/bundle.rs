//! Portable bundles: `vivar pack` archives a synced `.venv` together with the
//! exact Python it uses; `vivar unpack` extracts it on another machine of the
//! same OS/arch and rewrites the few absolute paths a venv bakes in, so
//! `source .venv/bin/activate` (or `.venv\Scripts\activate`) just works there.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use toml_edit::{value, DocumentMut};

pub const MANIFEST: &str = "vivar-bundle.toml";
/// Where a venv keeps its interpreter and scripts.
pub const VENV_BIN: &str = if cfg!(windows) { "Scripts" } else { "bin" };

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Absolute path without symlink resolution and without Windows' `\\?\`
/// prefix, which Python and activate scripts don't cope with.
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(strip_verbatim(std::path::absolute(path)?))
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path,
    }
}

/// A `key = value` line of `.venv/pyvenv.cfg`.
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

/// The python-build-standalone root behind a venv's `home`: `home` itself on
/// Windows, its parent (`…/bin`) elsewhere. Resolves uv's `cpython-3.12-…`
/// minor-version link to the real `cpython-3.12.x-…` directory.
fn python_root(home: &Path) -> Result<PathBuf> {
    let real = fs::canonicalize(home)
        .map(strip_verbatim)
        .with_context(|| format!("resolving Python home {}", home.display()))?;
    real.ancestors()
        .take(2)
        .find(|dir| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("cpython-"))
        })
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            anyhow!(
                "the environment uses {}, which is not a relocatable vivar-managed Python; \
                 run `vivar python install <version>` and `vivar sync`, then pack again",
                home.display()
            )
        })
}

/// Archives `<root>/.venv`, the project files, and the Python install the venv
/// points at. Returns the archive path.
pub fn pack(root: &Path, name: &str, output: Option<&Path>) -> Result<PathBuf> {
    let venv = root.join(".venv");
    let home = PathBuf::from(pyvenv_cfg(&venv, "home")?);
    let version = pyvenv_cfg(&venv, "version_info")?;
    let python_root = python_root(&home)?;
    let python_dir = python_root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("unexpected Python root {}", python_root.display()))?
        .to_owned();

    let output = absolute(
        &output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(format!("{name}-{}.tar.gz", platform()))),
    )?;

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

    let mm = major_minor(&version);
    let mut tar = Command::new("tar");
    tar.arg("-czf").arg(&output);
    // A bundle is a frozen environment: no bytecode caches, tool caches, or the
    // runtime's own installer/IDE/test baggage. `lib/pythonX.Y` is the Unix
    // layout, `Lib` the Windows one.
    let junk = [
        "__pycache__",
        "*.pyc",
        "*.pyo",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        ".hypothesis",
        ".venv/CACHEDIR.TAG",
        ".venv/.gitignore",
        ".venv/.lock",
    ];
    for pattern in junk {
        tar.arg(format!("--exclude={pattern}"));
    }
    for stdlib in [format!("lib/python{mm}"), "Lib".to_owned()] {
        // test suite (~25 MB), pip/ensurepip (a uv venv never uses them), IDLE
        for dir in ["test", "site-packages", "ensurepip", "idlelib"] {
            tar.arg(format!("--exclude={python_dir}/{stdlib}/{dir}"));
        }
    }
    for demos in ["lib/*/demos", "tcl/*/demos"] {
        tar.arg(format!("--exclude={python_dir}/{demos}"));
    }
    tar.arg(format!("--exclude={python_dir}/share/man"))
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
/// with it. Returns the absolute destination.
pub fn unpack(archive: &Path, dest: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let dest = absolute(dest)?;
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

    let manifest: DocumentMut = fs::read_to_string(dest.join(MANIFEST))
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
    let python_dir = field("python-dir")?;
    let new_venv = dest.join(".venv");
    let new_root = dest.join(&python_dir);
    let new_home = if cfg!(windows) {
        new_root.clone()
    } else {
        new_root.join("bin")
    };
    let bin = new_venv.join(VENV_BIN);

    // 1. `python`, `python3`, … symlinks pointed at the old Python install (Unix).
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
    let new_venv_s = new_venv.to_string_lossy().into_owned();
    let new_home_s = new_home.to_string_lossy().into_owned();
    let mut replacements = vec![
        (old_venv.clone(), new_venv_s.clone()),
        (old_home.clone(), new_home_s.clone()),
    ];
    if cfg!(windows) {
        // Some activate scripts spell Windows paths with forward slashes.
        replacements.push((old_venv.replace('\\', "/"), new_venv_s.replace('\\', "/")));
        replacements.push((old_home.replace('\\', "/"), new_home_s.replace('\\', "/")));
    }
    let mut files: Vec<PathBuf> = fs::read_dir(&bin)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && !p.is_symlink())
        .collect();
    files.push(new_venv.join("pyvenv.cfg"));
    for path in &files {
        rewrite(path, &replacements)?;
    }

    // 3. Windows launchers embed the interpreter path in a PE resource.
    if cfg!(windows) {
        patch_launchers(&bin, &old_venv, &old_home, &python_dir)?;
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
        bail!(
            "unexpected symlink {} in a Windows environment",
            link.display()
        )
    }
}

/// Replaces every occurrence of each old string in a text file.
/// Binaries (anything with a NUL in the first KiB) are left alone.
fn rewrite(path: &Path, replacements: &[(String, String)]) -> Result<()> {
    let original = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if original.iter().take(1024).any(|&b| b == 0) {
        return Ok(());
    }
    let mut data = original.clone();
    for (old, new) in replacements {
        data = replace_bytes(&data, old.as_bytes(), new.as_bytes());
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

/// uv's Windows launchers (`python.exe`, `pythonw.exe`, and every console
/// script) store the interpreter path in a PE resource (`UV_PYTHON_PATH`)
/// whose length lives in the resource table. A *relative* path is resolved
/// next to the launcher, so writing a short relative one in place makes the
/// launchers location-independent for good.
fn patch_launchers(bin: &Path, old_venv: &str, old_home: &str, python_dir: &str) -> Result<()> {
    for entry in fs::read_dir(bin)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".exe") {
            continue;
        }
        let candidates: Vec<(String, String)> = if name == "python.exe" || name == "pythonw.exe" {
            vec![(
                format!("{old_home}\\{name}"),
                format!("..\\..\\{python_dir}\\{name}"),
            )]
        } else {
            ["python.exe", "pythonw.exe"]
                .iter()
                .map(|py| (format!("{old_venv}\\Scripts\\{py}"), (*py).to_owned()))
                .collect()
        };
        for (old, new) in candidates {
            if patch_launcher(&path, old.as_bytes(), &new)? {
                break;
            }
        }
    }
    Ok(())
}

/// Returns `false` when the file doesn't contain `old` (not a uv launcher we know).
fn patch_launcher(exe: &Path, old: &[u8], new: &str) -> Result<bool> {
    let mut data = fs::read(exe)?;
    let Some(off) = find(&data, old) else {
        return Ok(false);
    };
    if new.len() > old.len() {
        bail!(
            "{}: launcher path {new} is longer than the original; unpack into a shorter directory",
            exe.display()
        );
    }
    let rva = rva_of(&data, off)
        .ok_or_else(|| anyhow!("{} is not a PE file vivar understands", exe.display()))?;
    let mut entry_key = rva.to_le_bytes().to_vec();
    entry_key.extend_from_slice(&(old.len() as u32).to_le_bytes());
    let entry = find(&data, &entry_key)
        .ok_or_else(|| anyhow!("{}: launcher resource entry not found", exe.display()))?;
    data[entry + 4..entry + 8].copy_from_slice(&(new.len() as u32).to_le_bytes());
    data[off..off + old.len()].fill(0);
    data[off..off + new.len()].copy_from_slice(new.as_bytes());
    fs::write(exe, data).with_context(|| format!("rewriting {}", exe.display()))?;
    Ok(true)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    (!needle.is_empty())
        .then(|| hay.windows(needle.len()).position(|w| w == needle))
        .flatten()
}

fn u16le(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32le(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Maps a file offset to a relative virtual address via the PE section table.
fn rva_of(pe: &[u8], off: usize) -> Option<u32> {
    let e_lfanew = u32le(pe, 0x3c)? as usize;
    if pe.get(e_lfanew..e_lfanew + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16le(pe, e_lfanew + 6)? as usize;
    let optional = u16le(pe, e_lfanew + 20)? as usize;
    let table = e_lfanew + 24 + optional;
    (0..sections).find_map(|i| {
        let s = table + i * 40;
        let vaddr = u32le(pe, s + 12)? as usize;
        let raw_size = u32le(pe, s + 16)? as usize;
        let raw_ptr = u32le(pe, s + 20)? as usize;
        (off >= raw_ptr && off < raw_ptr + raw_size).then(|| (vaddr + off - raw_ptr) as u32)
    })
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

    #[test]
    fn strips_windows_verbatim_prefix() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\x")),
            PathBuf::from(r"C:\x")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\srv\s")),
            PathBuf::from(r"\\?\UNC\srv\s")
        );
        assert_eq!(strip_verbatim(PathBuf::from("/x")), PathBuf::from("/x"));
    }

    /// A minimal PE: one section mapped file 0x200.. to RVA 0x1000, holding a
    /// resource data entry followed by the path it describes.
    fn fake_launcher(path: &[u8]) -> Vec<u8> {
        let mut pe = vec![0u8; 0x400];
        pe[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        pe[0x40..0x44].copy_from_slice(b"PE\0\0");
        pe[0x46..0x48].copy_from_slice(&1u16.to_le_bytes()); // sections
        pe[0x54..0x56].copy_from_slice(&0u16.to_le_bytes()); // optional header size
        let s = 0x40 + 24;
        pe[s + 12..s + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        pe[s + 16..s + 20].copy_from_slice(&0x200u32.to_le_bytes());
        pe[s + 20..s + 24].copy_from_slice(&0x200u32.to_le_bytes());
        let path_off = 0x210;
        let entry = 0x200;
        pe[entry..entry + 4].copy_from_slice(&(0x1000 + (path_off - 0x200) as u32).to_le_bytes());
        pe[entry + 4..entry + 8].copy_from_slice(&(path.len() as u32).to_le_bytes());
        pe[path_off..path_off + path.len()].copy_from_slice(path);
        pe
    }

    #[test]
    fn patches_launcher_path_and_resource_size_in_place() {
        let dir = std::env::temp_dir().join(format!("vivar-launcher-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("tool.exe");
        let old = br"C:\old\.venv\Scripts\python.exe";
        fs::write(&exe, fake_launcher(old)).unwrap();

        assert!(patch_launcher(&exe, old, "python.exe").unwrap());
        let data = fs::read(&exe).unwrap();
        assert_eq!(u32le(&data, 0x204), Some(10));
        assert_eq!(&data[0x210..0x21a], b"python.exe");
        assert!(data[0x21a..0x210 + old.len()].iter().all(|&b| b == 0));

        assert!(!patch_launcher(&exe, b"not there", "x").unwrap());
        assert!(patch_launcher(&exe, b"python.exe", "a-much-longer-path").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
