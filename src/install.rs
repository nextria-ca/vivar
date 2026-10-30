//! Installing and removing wheels by hand (PEP 427 / PEP 376): no uv, no pip,
//! no Python process. Used for native `sync`, tool and script environments
//! and cross-target bundles. A wheel's files are unpacked once into
//! `~/.cache/vivar/unpacked/<sha256>/` and hard-linked into every
//! environment that wants them (copied where linking is impossible, or with
//! `VIVAR_LINK_MODE=copy`), so ten environments with torch cost one torch.

use crate::{dirs, sha256, zip};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Where a wheel's files go.
pub struct Layout {
    /// site-packages
    pub site: PathBuf,
    /// bin (Scripts on Windows)
    pub bin: PathBuf,
    /// include/pythonX.Y
    pub headers: PathBuf,
    /// the environment root, for `.data/data` files
    pub prefix: PathBuf,
    /// what console scripts should run, e.g. `/env/bin/python`
    pub python: String,
    /// generate Windows `.cmd` shims instead of shebang scripts
    pub windows: bool,
}

impl Layout {
    /// The layout of a vivar/uv venv at `venv` for Python `major.minor`.
    pub fn venv(venv: &Path, python_mm: &str, windows: bool) -> Layout {
        let bin = venv.join(if windows { "Scripts" } else { "bin" });
        let python = if windows {
            bin.join("python.exe")
        } else {
            bin.join("python")
        };
        Layout {
            site: if windows {
                venv.join("Lib").join("site-packages")
            } else {
                venv.join("lib")
                    .join(format!("python{python_mm}"))
                    .join("site-packages")
            },
            bin,
            headers: venv.join("include").join(format!("python{python_mm}")),
            prefix: venv.to_path_buf(),
            python: python.to_string_lossy().into_owned(),
            windows,
        }
    }
}

/// A distribution found in site-packages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub dist_info: PathBuf,
    /// PEP 610 `direct_url.json`, for packages installed from a URL or git.
    pub direct_url: Option<String>,
}

/// Every `*.dist-info` in `site`.
pub fn installed(site: &Path) -> Result<Vec<Installed>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(site) else {
        return Ok(out);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(dir) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = dir.strip_suffix(".dist-info") else {
            continue;
        };
        let metadata = fs::read_to_string(path.join("METADATA")).unwrap_or_default();
        let header = |key: &str| {
            metadata
                .lines()
                .take_while(|l| !l.is_empty())
                .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(':')))
                .map(|v| v.trim().to_owned())
        };
        let (fallback_name, fallback_version) = stem.rsplit_once('-').unwrap_or((stem, ""));
        let direct_url = fs::read_to_string(path.join("direct_url.json"))
            .ok()
            .map(|t| t.trim().to_owned());
        out.push(Installed {
            name: crate::project::normalize(
                &header("Name").unwrap_or_else(|| fallback_name.to_owned()),
            ),
            version: header("Version").unwrap_or_else(|| fallback_version.to_owned()),
            dist_info: path,
            direct_url,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Unpacks `wheel` into `layout`. Returns the installed distribution. For a
/// package that came from a URL or git, `direct_url` is the PEP 610 record
/// to write beside METADATA, so a later sync can tell this install from the
/// same version out of an index or another commit.
pub fn install_wheel_from(
    wheel: &Path,
    layout: &Layout,
    direct_url: Option<&str>,
) -> Result<Installed> {
    let data = fs::read(wheel).with_context(|| format!("reading {}", wheel.display()))?;
    let entries =
        zip::entries(&data).with_context(|| format!("{} is not a wheel", wheel.display()))?;
    let store = Store::for_wheel(&data);
    let dist_info = entries
        .iter()
        .find_map(|e| {
            e.name
                .split('/')
                .next()
                .filter(|d| d.ends_with(".dist-info"))
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow!("{} has no .dist-info directory", wheel.display()))?;
    let stem = dist_info.trim_end_matches(".dist-info");
    let data_dir = format!("{stem}.data/");
    let wheel_meta = entries
        .iter()
        .find(|e| e.name == format!("{dist_info}/WHEEL"))
        .map(|e| zip::read(&data, e))
        .transpose()?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    if wheel_meta
        .lines()
        .any(|l| l.trim().starts_with("Wheel-Version:") && !l.contains("1."))
    {
        bail!("{}: unsupported wheel version", wheel.display());
    }

    let mut record: Vec<(String, String, u64)> = Vec::new(); // (path relative to site, hash, size)
    let rel_to_site = |target: &Path| -> String { relative(&layout.site, target) };
    for entry in &entries {
        if entry.is_dir() || entry.name == format!("{dist_info}/RECORD") {
            continue;
        }
        let target = if let Some(rest) = entry.name.strip_prefix(&data_dir) {
            let (scheme, path) = rest.split_once('/').unwrap_or((rest, ""));
            if path.is_empty() {
                continue;
            }
            match scheme {
                "purelib" | "platlib" => layout.site.join(path),
                "scripts" => layout.bin.join(path),
                "headers" => layout.headers.join(path),
                "data" => layout.prefix.join(path),
                other => bail!("{}: unknown wheel data scheme {other}", wheel.display()),
            }
        } else {
            layout.site.join(&entry.name)
        };
        if !safe_relative(&entry.name) {
            bail!(
                "{}: refusing to write {} outside the environment",
                wheel.display(),
                entry.name
            );
        }
        let mut bytes = zip::read(&data, entry)?;
        let is_script = entry.name.starts_with(&data_dir)
            && entry.name[data_dir.len()..].starts_with("scripts/");
        if is_script && bytes.starts_with(b"#!python") {
            // "#!python" / "#!pythonw" placeholders point at this environment's interpreter.
            let first_end = bytes
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(bytes.len());
            let mut rewritten = format!("#!{}\n", layout.python).into_bytes();
            let rest = &bytes[first_end.min(bytes.len())..];
            rewritten.extend_from_slice(rest.strip_prefix(b"\n").unwrap_or(rest));
            bytes = rewritten;
        }
        fs::create_dir_all(target.parent().context("target has no parent")?)?;
        let mode = if is_script { 0o755 } else { entry.mode & 0o777 };
        // Scripts are rewritten for this environment, so they are its own;
        // everything else is the wheel's and comes out of the shared store.
        match (&store, is_script) {
            (Some(store), false) => store.place(&entry.name, &bytes, mode, &target)?,
            _ => write_file(&target, &bytes, mode)?,
        }
        record.push((
            rel_to_site(&target),
            record_hash(&bytes),
            bytes.len() as u64,
        ));
    }

    // Console scripts from entry_points.txt.
    let entry_points = fs::read_to_string(layout.site.join(&dist_info).join("entry_points.txt"))
        .unwrap_or_default();
    for (name, module, attr) in console_scripts(&entry_points) {
        for (path, bytes) in script_files(layout, &name, &module, &attr) {
            fs::create_dir_all(&layout.bin)?;
            fs::write(&path, &bytes)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
            }
            record.push((rel_to_site(&path), record_hash(&bytes), bytes.len() as u64));
        }
    }

    let info = layout.site.join(&dist_info);
    fs::write(info.join("INSTALLER"), "vivar\n")?;
    record.push((
        rel_to_site(&info.join("INSTALLER")),
        record_hash(b"vivar\n"),
        6,
    ));
    if let Some(d) = direct_url {
        let text = format!("{}\n", d.trim());
        fs::write(info.join("direct_url.json"), &text)?;
        record.push((
            rel_to_site(&info.join("direct_url.json")),
            record_hash(text.as_bytes()),
            text.len() as u64,
        ));
    }
    record.push((rel_to_site(&info.join("RECORD")), String::new(), 0));
    let mut text = String::new();
    for (path, hash, size) in &record {
        if hash.is_empty() {
            text.push_str(&format!("{path},,\n"));
        } else {
            text.push_str(&format!("{},{hash},{size}\n", csv_field(path)));
        }
    }
    fs::write(info.join("RECORD"), text)?;

    let (name, version) = stem.split_once('-').unwrap_or((stem, ""));
    Ok(Installed {
        name: crate::project::normalize(name),
        version: version.to_owned(),
        dist_info: info,
        direct_url: direct_url.map(|d| d.trim().to_owned()),
    })
}

/// Writes `bytes` to `path` (replacing whatever is there) with `mode` on Unix.
fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let _ = fs::remove_file(path);
    fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    Ok(())
}

/// The unpacked-wheel store: one directory per wheel, by hash, that
/// environments hard-link from.
struct Store {
    dir: PathBuf,
}

impl Store {
    /// None when linking is switched off (`VIVAR_LINK_MODE=copy`) or there
    /// is no cache directory to use.
    fn for_wheel(data: &[u8]) -> Option<Store> {
        if std::env::var("VIVAR_LINK_MODE").is_ok_and(|m| m == "copy") {
            return None;
        }
        let mut h = sha256::Sha256::new();
        h.update(data);
        let dir = dirs::cache()
            .ok()?
            .join("unpacked")
            .join(sha256::hex(&h.finish()));
        fs::create_dir_all(&dir).ok()?;
        Some(Store { dir })
    }

    /// Puts the wheel member `name` at `target`: a hard link to the store's
    /// copy (made on first use), or a plain file when the link cannot be made
    /// (another filesystem, a filesystem without links).
    fn place(&self, name: &str, bytes: &[u8], mode: u32, target: &Path) -> Result<()> {
        let cached = self.dir.join(name);
        // A copy from before files were made read-only is replaced by a new
        // one (renamed over the entry), so the inode other environments
        // link to is left alone.
        #[cfg(unix)]
        let usable = |m: &fs::Metadata| {
            use std::os::unix::fs::PermissionsExt;
            m.permissions().mode() & 0o222 == 0
        };
        #[cfg(not(unix))]
        let usable = |_: &fs::Metadata| true;
        let fresh = match fs::metadata(&cached) {
            Ok(m) if m.len() == bytes.len() as u64 && usable(&m) => false,
            _ => {
                // Written whole under another name, then renamed: a parallel
                // install never links a half-written file.
                let part = self.dir.join(format!("{name}.part-{}", std::process::id()));
                fs::create_dir_all(cached.parent().context("store path")?)?;
                // Read-only: a linked file is shared by every environment,
                // so an edit in one would be an edit in all. (Not on
                // Windows, where the attribute also blocks deletion.)
                let shared_mode = if cfg!(unix) { mode & !0o222 } else { mode };
                write_file(&part, bytes, shared_mode)?;
                fs::rename(&part, &cached)?;
                true
            }
        };
        let _ = fs::remove_file(target);
        if fs::hard_link(&cached, target).is_ok() {
            return Ok(());
        }
        if !fresh {
            // A stale or foreign store entry is no reason to fail the install.
            let _ = fs::remove_file(&cached);
        }
        write_file(target, bytes, mode)
    }
}

/// Removes a distribution: every file in its RECORD, then empty directories.
pub fn uninstall(dist: &Installed) -> Result<()> {
    let site = dist
        .dist_info
        .parent()
        .context("dist-info has no parent")?
        .to_path_buf();
    let record = fs::read_to_string(dist.dist_info.join("RECORD")).unwrap_or_default();
    let mut dirs = std::collections::BTreeSet::new();
    for line in record.lines() {
        let Some(path) = csv_first(line) else {
            continue;
        };
        if !safe_relative(&path) {
            continue;
        }
        let full = site.join(&path);
        let _ = fs::remove_file(&full);
        if let Some(parent) = full.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    let _ = fs::remove_dir_all(&dist.dist_info);
    // Bytecode caches are made at import time and never recorded; a package
    // directory holding only one of those is gone too. Deepest first, then
    // every emptied ancestor up to site-packages.
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let mut cur = d;
        while cur.starts_with(&site) && cur != site {
            let _ = fs::remove_dir_all(cur.join("__pycache__"));
            if fs::remove_dir(&cur).is_err() {
                break;
            }
            cur = cur
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| site.clone());
        }
    }
    Ok(())
}

fn console_scripts(entry_points: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut in_console = false;
    for line in entry_points.lines().map(str::trim) {
        if line.starts_with('[') {
            in_console = line == "[console_scripts]";
        } else if in_console {
            if let Some((name, target)) = line.split_once('=') {
                let target = target.split(['[', '#']).next().unwrap_or(target).trim();
                let (module, attr) = target.split_once(':').unwrap_or((target, "main"));
                out.push((
                    name.trim().to_owned(),
                    module.trim().to_owned(),
                    attr.trim().to_owned(),
                ));
            }
        }
    }
    out
}

/// The files that make `name` runnable: a shebang script on Unix, a `.cmd`
/// shim plus the script on Windows.
pub fn console_script_files(
    layout: &Layout,
    name: &str,
    module: &str,
    attr: &str,
) -> Vec<(PathBuf, Vec<u8>)> {
    script_files(layout, name, module, attr)
}

fn script_files(layout: &Layout, name: &str, module: &str, attr: &str) -> Vec<(PathBuf, Vec<u8>)> {
    let func = attr.split('.').next().unwrap_or(attr);
    let body = format!(
        "# -*- coding: utf-8 -*-\nimport re\nimport sys\nfrom {module} import {func}\nif __name__ == \"__main__\":\n    sys.argv[0] = re.sub(r\"(-script\\.pyw|\\.exe)?$\", \"\", sys.argv[0])\n    sys.exit({attr}())\n"
    );
    if layout.windows {
        let script = layout.bin.join(format!("{name}-script.py"));
        let shim = layout.bin.join(format!("{name}.cmd"));
        let cmd = format!("@echo off\r\n\"%~dp0python.exe\" \"%~dp0{name}-script.py\" %*\r\n");
        vec![
            (script, format!("#!{}\n{body}", layout.python).into_bytes()),
            (shim, cmd.into_bytes()),
        ]
    } else {
        vec![(
            layout.bin.join(name),
            format!("#!{}\n{body}", layout.python).into_bytes(),
        )]
    }
}

fn record_hash(bytes: &[u8]) -> String {
    let mut h = sha256::Sha256::new();
    h.update(bytes);
    format!("sha256={}", base64url(&h.finish()))
}

/// URL-safe base64 without padding, as RECORD wants.
fn base64url(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        }
    }
    out
}

fn relative(base: &Path, target: &Path) -> String {
    let mut up = 0;
    let mut b = base.to_path_buf();
    loop {
        if let Ok(rest) = target.strip_prefix(&b) {
            let prefix = "../".repeat(up);
            return format!("{prefix}{}", rest.to_string_lossy().replace('\\', "/"));
        }
        if !b.pop() {
            return target.to_string_lossy().replace('\\', "/");
        }
        up += 1;
    }
}

fn safe_relative(p: &str) -> bool {
    !p.starts_with('/') && !p.contains(':') && !p.split('/').any(|c| c == "..")
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

fn csv_first(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix('"') {
        let end = rest.find("\",")?;
        return Some(rest[..end].replace("\"\"", "\""));
    }
    line.split(',')
        .next()
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(base64url(b"hello world"), "aGVsbG8gd29ybGQ");
        assert_eq!(base64url(&[0xff, 0xfe]), "__4");
        assert_eq!(
            relative(
                Path::new("/e/lib/python3.12/site-packages"),
                Path::new("/e/bin/tool")
            ),
            "../../../bin/tool"
        );
        assert_eq!(
            relative(Path::new("/e/site"), Path::new("/e/site/pkg/a.py")),
            "pkg/a.py"
        );
        assert!(safe_relative("pkg/a.py") && !safe_relative("../x") && !safe_relative("/abs"));
        assert_eq!(csv_first("\"a,b\",sha256=x,1").as_deref(), Some("a,b"));
        assert_eq!(csv_first("plain,,").as_deref(), Some("plain"));
        let eps = console_scripts(
            "[console_scripts]\npycodestyle = pycodestyle:_main\n[gui_scripts]\nx = y:z\n",
        );
        assert_eq!(
            eps,
            vec![("pycodestyle".into(), "pycodestyle".into(), "_main".into())]
        );
    }
}
