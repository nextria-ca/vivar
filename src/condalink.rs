//! Linking conda packages (`.conda` archives) into a prefix: fetching by
//! hash, unpacking once into the cache, hard-linking files in, rewriting the
//! build-time prefix placeholder, generating noarch entry points, and
//! recording each package under `conda-meta/` so conda tools recognise the
//! prefix. The same records relocate a packed prefix on another machine.

use crate::condaspec::Record;
use crate::{dirs, json, sha256, tar, upgrade, zip, zstd};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A package as `conda-meta/<name>-<version>-<build>.json` records it.
#[derive(Debug, Clone)]
pub struct Linked {
    pub name: String,
    pub version: String,
    pub build: String,
    pub license: Option<String>,
    /// Files relative to the prefix, forward slashes.
    pub files: Vec<String>,
    /// (path, placeholder, text or binary) for files that hold the prefix.
    pub placeholders: Vec<(String, String, bool)>,
}

impl Linked {
    fn meta_name(&self) -> String {
        format!("{}-{}-{}.json", self.name, self.version, self.build)
    }
}

/// Where a package's files go inside a prefix.
pub struct PrefixLayout {
    pub prefix: PathBuf,
    pub windows: bool,
    /// `3.12`, for noarch python packages.
    pub python_mm: Option<String>,
}

impl PrefixLayout {
    fn site_packages(&self) -> Result<PathBuf> {
        let mm = self
            .python_mm
            .as_deref()
            .ok_or_else(|| anyhow!("a noarch python package needs python in the environment"))?;
        Ok(if self.windows {
            self.prefix.join("Lib").join("site-packages")
        } else {
            self.prefix
                .join("lib")
                .join(format!("python{mm}"))
                .join("site-packages")
        })
    }

    fn bin(&self) -> PathBuf {
        self.prefix
            .join(if self.windows { "Scripts" } else { "bin" })
    }

    pub fn python(&self) -> PathBuf {
        if self.windows {
            self.prefix.join("python.exe")
        } else {
            self.prefix.join("bin").join("python")
        }
    }
}

fn pkgs_dir() -> Result<PathBuf> {
    Ok(dirs::cache()?.join("conda").join("pkgs"))
}

/// Downloads a package into the cache (hash-checked) and returns its path.
pub fn fetch(record: &Record) -> Result<PathBuf> {
    if record.filename.is_empty() || record.filename.contains(['/', '\\']) {
        bail!("{}: not a downloadable package", record.name);
    }
    let dir = pkgs_dir()?;
    fs::create_dir_all(&dir)?;
    let dest = dir.join(&record.filename);
    let expected = record.sha256.as_deref().ok_or_else(|| {
        anyhow!(
            "{} has no sha256 in the channel's repodata",
            record.filename
        )
    })?;
    if dest.is_file() && sha256::file(&dest)? == expected {
        return Ok(dest);
    }
    eprintln!("downloading {}", record.filename);
    let part = dir.join(format!("{}.part-{}", record.filename, std::process::id()));
    if let Err(e) = upgrade::fetch_to(&record.url, &part) {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    let actual = sha256::file(&part)?;
    if actual != expected {
        let _ = fs::remove_file(&part);
        bail!(
            "{} does not match the channel's hash (got {actual}, expected {expected})",
            record.filename
        );
    }
    fs::rename(&part, &dest)?;
    Ok(dest)
}

/// One file of a package, as unpacked.
struct Member {
    path: String,
    kind: tar::Kind,
    mode: u32,
    data: Vec<u8>,
    link: String,
}

/// Unpacks a `.conda` archive: the `info/` members and the package files.
fn unpack(archive: &Path) -> Result<(Vec<Member>, Vec<Member>)> {
    let data = fs::read(archive)?;
    let entries = zip::entries(&data)
        .with_context(|| format!("{} is not a .conda archive", archive.display()))?;
    let mut info = Vec::new();
    let mut pkg = Vec::new();
    for e in &entries {
        let is_info = e.name.starts_with("info-") && e.name.ends_with(".tar.zst");
        let is_pkg = e.name.starts_with("pkg-") && e.name.ends_with(".tar.zst");
        if !is_info && !is_pkg {
            continue;
        }
        let tar_bytes = zstd::decompress(&zip::read(&data, e)?)
            .with_context(|| format!("decompressing {} in {}", e.name, archive.display()))?;
        let members: Vec<Member> = tar::entries(&tar_bytes)?
            .into_iter()
            .map(|t| Member {
                path: t.name.trim_start_matches("./").to_owned(),
                kind: t.kind,
                mode: t.mode,
                data: t.data.to_vec(),
                link: t.link,
            })
            .collect();
        if is_info {
            info.extend(members);
        } else {
            pkg.extend(members);
        }
    }
    if info.is_empty() {
        bail!("{} holds no info/ members", archive.display());
    }
    Ok((info, pkg))
}

fn safe_relative(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains(':')
        && !p.split('/').any(|c| c == ".." || c.is_empty())
}

/// What `info/paths.json` (or the older `info/has_prefix`) says about the
/// files that embed the build prefix: path → (placeholder, text?).
fn placeholders(info: &[Member]) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    if let Some(m) = info.iter().find(|m| m.path == "info/paths.json") {
        if let Ok(doc) = json::parse(&String::from_utf8_lossy(&m.data)) {
            for p in doc.get("paths").and_then(|p| p.as_array()).unwrap_or(&[]) {
                let (Some(path), Some(ph)) = (
                    p.get("_path").and_then(|v| v.as_str()),
                    p.get("prefix_placeholder").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let text = p.get("file_mode").and_then(|v| v.as_str()) != Some("binary");
                out.push((path.to_owned(), ph.to_owned(), text));
            }
        }
        return out;
    }
    if let Some(m) = info.iter().find(|m| m.path == "info/has_prefix") {
        for line in String::from_utf8_lossy(&m.data).lines() {
            // `<placeholder> <mode> <path>`, the path possibly quoted; or just a path.
            let parts: Vec<&str> = line.splitn(3, ' ').collect();
            match parts.as_slice() {
                [ph, mode, path] => out.push((
                    path.trim_matches('"').to_owned(),
                    ph.trim_matches('"').to_owned(),
                    *mode != "binary",
                )),
                [path] if !path.is_empty() => out.push((
                    path.to_string(),
                    "/opt/anaconda1anaconda2anaconda3".to_owned(),
                    true,
                )),
                _ => {}
            }
        }
    }
    out
}

/// Replaces `old` with `new` in a text file.
fn text_replace(data: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
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

/// Replaces `old` with `new` in a binary, as conda does: the whole
/// NUL-terminated string the prefix starts (`<old>/share/terminfo`) is
/// rewritten with `new`, its suffix kept, and NUL-padded so the string's
/// room (`budget` bytes for the prefix part) stays the same size. The same
/// call moves a prefix again later, `old` being the previous prefix.
fn binary_replace(data: &[u8], old: &[u8], new: &[u8], budget: usize) -> Result<Vec<u8>> {
    if new.len() > budget {
        bail!(
            "the environment path is {} bytes long; binaries built for conda allow {budget}",
            new.len()
        );
    }
    let slack = budget.saturating_sub(old.len());
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if !data[i..].starts_with(old) {
            out.push(data[i]);
            i += 1;
            continue;
        }
        let string_end = data[i + old.len()..]
            .iter()
            .position(|&b| b == 0)
            .map(|n| i + old.len() + n)
            .unwrap_or(data.len());
        // After an earlier move the string is followed by its padding.
        let region_end = string_end + slack;
        if region_end > data.len() || data[string_end..region_end].iter().any(|&b| b != 0) {
            bail!("unexpected layout around an embedded path in a binary file");
        }
        out.extend_from_slice(new);
        out.extend_from_slice(&data[i + old.len()..string_end]);
        out.extend(std::iter::repeat_n(0u8, budget - new.len()));
        i = region_end;
    }
    Ok(out)
}

/// Replaces the placeholder at link time.
fn replace_prefix(data: &[u8], placeholder: &[u8], prefix: &[u8], text: bool) -> Result<Vec<u8>> {
    if text {
        Ok(text_replace(data, placeholder, prefix))
    } else {
        binary_replace(data, placeholder, prefix, placeholder.len())
    }
}

/// Whether a symlink at `at` (relative to the prefix) pointing at `target`
/// stays inside the prefix, judged lexically.
fn link_stays_inside(at: &str, target: &str) -> bool {
    if target.starts_with('/') || target.contains(':') {
        return false;
    }
    let mut depth: i64 = at.matches('/').count() as i64; // the link's directory depth
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

/// The destination of a package member inside the prefix: noarch python
/// packages keep their files under `site-packages/` and `python-scripts/`.
fn destination(path: &str, noarch_python: bool, layout: &PrefixLayout) -> Result<PathBuf> {
    if noarch_python {
        if let Some(rest) = path.strip_prefix("site-packages/") {
            return Ok(layout.site_packages()?.join(rest));
        }
        if let Some(rest) = path.strip_prefix("python-scripts/") {
            return Ok(layout.bin().join(rest));
        }
    }
    Ok(layout.prefix.join(path))
}

/// Links `record` (already fetched to `archive`) into the prefix.
pub fn link(record: &Record, archive: &Path, layout: &PrefixLayout) -> Result<Linked> {
    let (info, pkg) = unpack(archive)?;
    let noarch_python = record.noarch.as_deref() == Some("python");
    let holders = placeholders(&info);
    let prefix_text = layout.prefix.to_string_lossy().replace('\\', "/");
    let prefix_bytes = prefix_text.as_bytes();
    fs::create_dir_all(&layout.prefix)?;
    let prefix_real = fs::canonicalize(&layout.prefix)?;
    // Nothing is written through a directory that leads outside the prefix
    // (a symlink an earlier member planted).
    let parent_inside = |dest: &Path| -> Result<()> {
        let parent = dest.parent().context("member parent")?;
        fs::create_dir_all(parent)?;
        let real = fs::canonicalize(parent)?;
        if !real.starts_with(&prefix_real) {
            bail!(
                "{}: refusing to write {} through a link outside the prefix",
                record.filename,
                dest.display()
            );
        }
        Ok(())
    };
    let mut files: Vec<String> = Vec::new();
    let mut written: Vec<(String, String, bool)> = Vec::new();
    let rel_of = |dest: &Path| -> String {
        dest.strip_prefix(&layout.prefix)
            .unwrap_or(dest)
            .to_string_lossy()
            .replace('\\', "/")
    };
    for m in &pkg {
        if !safe_relative(&m.path) {
            bail!(
                "{}: refusing to write {} outside the prefix",
                record.filename,
                m.path
            );
        }
        let dest = destination(&m.path, noarch_python, layout)?;
        match m.kind {
            tar::Kind::Dir => {
                parent_inside(&dest)?;
                fs::create_dir_all(&dest)?;
                continue;
            }
            tar::Kind::Symlink => {
                if !link_stays_inside(&rel_of(&dest), &m.link) {
                    bail!(
                        "{}: refusing link {} -> {} (outside the prefix)",
                        record.filename,
                        m.path,
                        m.link
                    );
                }
                parent_inside(&dest)?;
                let _ = fs::remove_file(&dest);
                #[cfg(unix)]
                std::os::unix::fs::symlink(&m.link, &dest)
                    .with_context(|| format!("linking {}", dest.display()))?;
                #[cfg(not(unix))]
                {
                    // No symlinks to rely on: copy what the link points at.
                    let target = dest.parent().context("member parent")?.join(&m.link);
                    if target.is_file() {
                        fs::copy(&target, &dest)?;
                    }
                }
                files.push(rel_of(&dest));
                continue;
            }
            tar::Kind::Hardlink => {
                if !safe_relative(&m.link) {
                    bail!("{}: refusing hard link to {}", record.filename, m.link);
                }
                let source = destination(&m.link, noarch_python, layout)?;
                parent_inside(&dest)?;
                let _ = fs::remove_file(&dest);
                if fs::hard_link(&source, &dest).is_err() {
                    fs::copy(&source, &dest)?;
                }
                files.push(rel_of(&dest));
                continue;
            }
            tar::Kind::File => {}
        }
        parent_inside(&dest)?;
        // Binaries keep the build prefix on Windows, as conda leaves them
        // (Windows binaries do not load by that path); text files are rewritten.
        let holder = holders
            .iter()
            .find(|(p, _, text)| *p == m.path && (*text || !layout.windows));
        let mut bytes = m.data.clone();
        let mut mode = m.mode;
        if let Some((_, placeholder, text)) = holder {
            bytes = replace_prefix(&bytes, placeholder.as_bytes(), prefix_bytes, *text)?;
            written.push((rel_of(&dest), placeholder.clone(), *text));
        }
        if noarch_python && m.path.starts_with("python-scripts/") && bytes.starts_with(b"#!") {
            // A script's shebang points at this environment's python.
            let end = bytes
                .iter()
                .position(|b| *b == b'\n')
                .unwrap_or(bytes.len());
            let mut fresh = format!("#!{}\n", layout.python().display()).into_bytes();
            fresh.extend_from_slice(bytes.get(end + 1..).unwrap_or(&[]));
            bytes = fresh;
            mode |= 0o755;
        }
        let _ = fs::remove_file(&dest);
        fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if mode & 0o777 != 0 {
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o777))?;
            }
        }
        #[cfg(not(unix))]
        let _ = mode;
        files.push(rel_of(&dest));
    }
    // Entry points of a noarch python package become console scripts.
    if noarch_python {
        if let Some(m) = info.iter().find(|m| m.path == "info/link.json") {
            if let Ok(doc) = json::parse(&String::from_utf8_lossy(&m.data)) {
                let eps = doc
                    .get("noarch")
                    .and_then(|n| n.get("entry_points"))
                    .and_then(|e| e.as_array())
                    .unwrap_or(&[]);
                let site = layout.site_packages()?;
                let il = crate::install::Layout {
                    site: site.clone(),
                    bin: layout.bin(),
                    headers: layout.prefix.join("include"),
                    prefix: layout.prefix.clone(),
                    python: layout.python().to_string_lossy().into_owned(),
                    windows: layout.windows,
                };
                for ep in eps {
                    let Some(text) = ep.as_str() else { continue };
                    let Some((name, target)) = text.split_once('=') else {
                        continue;
                    };
                    let (module, attr) = target
                        .trim()
                        .split_once(':')
                        .unwrap_or((target.trim(), "main"));
                    for (path, bytes) in crate::install::console_script_files(
                        &il,
                        name.trim(),
                        module.trim(),
                        attr.trim(),
                    ) {
                        fs::create_dir_all(&il.bin)?;
                        fs::write(&path, &bytes)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                        }
                        files.push(rel_of(&path));
                    }
                }
            }
        }
    }
    let linked = Linked {
        name: record.name.clone(),
        version: record.version.clone(),
        build: record.build.clone(),
        license: record.license.clone(),
        files,
        placeholders: written,
    };
    write_meta(&layout.prefix, record, &linked)?;
    Ok(linked)
}

fn write_meta(prefix: &Path, record: &Record, linked: &Linked) -> Result<()> {
    let dir = prefix.join("conda-meta");
    fs::create_dir_all(&dir)?;
    let list = |items: &[String]| -> String {
        items
            .iter()
            .map(|s| json::quote(s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let paths: Vec<String> = linked
        .placeholders
        .iter()
        .map(|(p, ph, text)| {
            format!(
                "{{\"_path\": {}, \"path_type\": \"hardlink\", \"prefix_placeholder\": {}, \"file_mode\": {}}}",
                json::quote(p),
                json::quote(ph),
                json::quote(if *text { "text" } else { "binary" })
            )
        })
        .collect();
    let opt = |v: &Option<String>| {
        v.as_deref()
            .map(json::quote)
            .unwrap_or_else(|| "null".to_owned())
    };
    let text = format!(
        "{{\n  \"name\": {},\n  \"version\": {},\n  \"build\": {},\n  \"build_number\": {},\n  \"channel\": {},\n  \"subdir\": {},\n  \"fn\": {},\n  \"url\": {},\n  \"sha256\": {},\n  \"md5\": {},\n  \"size\": {},\n  \"depends\": [{}],\n  \"constrains\": [{}],\n  \"license\": {},\n  \"noarch\": {},\n  \"timestamp\": {},\n  \"files\": [{}],\n  \"paths_data\": {{\"paths_version\": 1, \"paths\": [{}]}},\n  \"link\": {{\"source\": \"\", \"type\": 1}},\n  \"requested_spec\": \"\",\n  \"installed_by\": \"vivar\"\n}}\n",
        json::quote(&record.name),
        json::quote(&record.version),
        json::quote(&record.build),
        record.build_number,
        json::quote(record.url.rsplit_once('/').map(|(c, _)| c).unwrap_or("")),
        json::quote(&record.subdir),
        json::quote(&record.filename),
        json::quote(&record.url),
        opt(&record.sha256),
        opt(&record.md5),
        record.size,
        list(&record.depends),
        list(&record.constrains),
        opt(&record.license),
        opt(&record.noarch),
        record.timestamp,
        list(&linked.files),
        paths.join(", ")
    );
    fs::write(dir.join(linked.meta_name()), text)?;
    Ok(())
}

/// Every package recorded under `conda-meta/`.
pub fn installed(prefix: &Path) -> Result<Vec<Linked>> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(prefix.join("conda-meta")) else {
        return Ok(out);
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(doc) = json::parse(&text) else {
            continue;
        };
        let field = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        let (Some(name), Some(version), Some(build)) =
            (field("name"), field("version"), field("build"))
        else {
            continue;
        };
        let files = doc
            .get("files")
            .and_then(|f| f.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let placeholders = doc
            .get("paths_data")
            .and_then(|p| p.get("paths"))
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let path = p.get("_path")?.as_str()?.to_owned();
                        let ph = p.get("prefix_placeholder")?.as_str()?.to_owned();
                        let text = p.get("file_mode").and_then(|m| m.as_str()) != Some("binary");
                        Some((path, ph, text))
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.push(Linked {
            name,
            version,
            build,
            license: field("license"),
            files,
            placeholders,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// The `*.dist-info` directories (by name) that native packages put into
/// site-packages: Python packages the prefix provides, not the PyPI
/// installer's to remove.
pub fn owned_dist_infos(prefix: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for l in installed(prefix).unwrap_or_default() {
        for f in &l.files {
            if let Some(i) = f.find(".dist-info/") {
                let dir = &f[..i + ".dist-info".len()];
                if let Some(name) = dir.rsplit('/').next() {
                    out.insert(name.to_owned());
                }
            }
        }
    }
    out
}

/// Removes a package's files, emptied directories and its record.
pub fn unlink(prefix: &Path, linked: &Linked) -> Result<()> {
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for f in &linked.files {
        if !safe_relative(f) {
            continue;
        }
        let path = prefix.join(f);
        let _ = fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            dirs.insert(parent.to_path_buf());
        }
    }
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let mut cur = d;
        while cur.starts_with(prefix) && cur != prefix {
            let _ = fs::remove_dir_all(cur.join("__pycache__"));
            if fs::remove_dir(&cur).is_err() {
                break;
            }
            cur = cur.parent().map(Path::to_path_buf).unwrap_or_default();
        }
    }
    let _ = fs::remove_file(prefix.join("conda-meta").join(linked.meta_name()));
    Ok(())
}

/// After a prefix moved from `old` to where it is now: rewrites every file
/// that holds the old path (text as is, binaries keeping their length).
/// Returns how many files changed.
pub fn relocate(prefix: &Path, old: &str) -> Result<usize> {
    let new = prefix.to_string_lossy().replace('\\', "/");
    if new == old {
        return Ok(0);
    }
    let mut changed = 0;
    for linked in installed(prefix)? {
        for (path, placeholder, text) in &linked.placeholders {
            let file = prefix.join(path);
            let Ok(data) = fs::read(&file) else { continue };
            let rewritten = if *text {
                text_replace(&data, old.as_bytes(), new.as_bytes())
            } else {
                binary_replace(&data, old.as_bytes(), new.as_bytes(), placeholder.len())
                    .with_context(|| format!("relocating {}", file.display()))?
            };
            if rewritten != data {
                fs::write(&file, rewritten)?;
                changed += 1;
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_replacement_keeps_binary_length_and_suffixes() {
        let ph = b"/opt/anaconda1anaconda2anaconda3";
        let data = b"x=/opt/anaconda1anaconda2anaconda3/lib;y";
        let text = replace_prefix(data, ph, b"/home/me/.venv", true).unwrap();
        assert_eq!(text, b"x=/home/me/.venv/lib;y");
        // A binary keeps the string whole: prefix, suffix, then the padding.
        let data = b"\0/opt/anaconda1anaconda2anaconda3/share/terminfo\0next\0";
        let bin = replace_prefix(data, ph, b"/home/me/.venv", false).unwrap();
        assert_eq!(bin.len(), data.len());
        let want = {
            let mut w = b"\0/home/me/.venv/share/terminfo".to_vec();
            w.extend(std::iter::repeat_n(0u8, ph.len() - "/home/me/.venv".len()));
            w.extend_from_slice(b"\0next\0");
            w
        };
        assert_eq!(bin, want);
        // Moving it again keeps the layout; a path too long is refused.
        let moved = binary_replace(&bin, b"/home/me/.venv", b"/srv/app/.venv", ph.len()).unwrap();
        assert_eq!(moved.len(), data.len());
        assert!(moved.starts_with(b"\0/srv/app/.venv/share/terminfo\0"));
        assert!(replace_prefix(data, ph, &[b'a'; 40], false).is_err());
        assert!(link_stays_inside("lib/libz.so", "libz.so.1"));
        assert!(link_stays_inside(
            "bin/python",
            "../lib/python3.12/bin/python"
        ));
        assert!(!link_stays_inside("bin/python", "../../etc/passwd"));
        assert!(!link_stays_inside("bin/x", "/etc/passwd"));
    }

    #[test]
    fn links_a_noarch_package_and_records_it() {
        let dir = std::env::temp_dir().join(format!("vivar-condalink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // A .conda archive built by hand: stored zip members holding zstd
        // raw-block frames (no compression) of hand-made tars.
        let tar_of = |entries: &[(&str, &[u8], u8)]| -> Vec<u8> {
            let mut out = Vec::new();
            for (name, body, flag) in entries {
                let mut h = vec![0u8; 512];
                h[..name.len()].copy_from_slice(name.as_bytes());
                h[100..107].copy_from_slice(b"0000755");
                let size = format!("{:011o}\0", body.len());
                h[124..136].copy_from_slice(size.as_bytes());
                h[156] = *flag;
                h[257..262].copy_from_slice(b"ustar");
                out.extend(h);
                out.extend_from_slice(body);
                out.resize(out.len() + (512 - body.len() % 512) % 512, 0);
            }
            out.extend(vec![0u8; 1024]);
            out
        };
        let zstd_raw = |payload: &[u8]| -> Vec<u8> {
            // Magic, descriptor (single segment, 4-byte content size, no
            // checksum), the size, then one raw last block.
            let mut f = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0];
            f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            let header = (payload.len() as u32) << 3 | 1;
            f.extend_from_slice(&header.to_le_bytes()[..3]);
            f.extend_from_slice(payload);
            f
        };
        let paths = br#"{"paths": [{"_path": "site-packages/hello/__init__.py", "path_type": "hardlink", "prefix_placeholder": "/opt/anaconda1anaconda2anaconda3", "file_mode": "text"}], "paths_version": 1}"#;
        let link_json = br#"{"noarch": {"type": "python", "entry_points": ["hello = hello:main"]}, "package_metadata_version": 1}"#;
        let info = tar_of(&[
            ("info/paths.json", paths, b'0'),
            ("info/link.json", link_json, b'0'),
            ("info/index.json", b"{}", b'0'),
        ]);
        let pkg = tar_of(&[
            ("site-packages/hello/", b"", b'5'),
            (
                "site-packages/hello/__init__.py",
                b"PREFIX = '/opt/anaconda1anaconda2anaconda3'\ndef main():\n    print(PREFIX)\n",
                b'0',
            ),
            (
                "python-scripts/hello-cli",
                b"#!/opt/anaconda1anaconda2anaconda3/bin/python\nprint(1)\n",
                b'0',
            ),
        ]);
        // A stored zip around the two frames.
        let mut zipped = Vec::new();
        let mut central = Vec::new();
        for (name, body) in [
            ("info-hello-1.0-0.tar.zst", zstd_raw(&info)),
            ("pkg-hello-1.0-0.tar.zst", zstd_raw(&pkg)),
        ] {
            let offset = zipped.len() as u32;
            let mut local = vec![
                0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            local.extend_from_slice(&(body.len() as u32).to_le_bytes());
            local.extend_from_slice(&(body.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name.len() as u16).to_le_bytes());
            local.extend_from_slice(&[0, 0]);
            local.extend_from_slice(name.as_bytes());
            local.extend_from_slice(&body);
            zipped.extend(local);
            let mut c = vec![
                0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            c.extend_from_slice(&(body.len() as u32).to_le_bytes());
            c.extend_from_slice(&(body.len() as u32).to_le_bytes());
            c.extend_from_slice(&(name.len() as u16).to_le_bytes());
            c.extend_from_slice(&[0; 8]);
            c.extend_from_slice(&0u32.to_le_bytes());
            c.extend_from_slice(&offset.to_le_bytes());
            c.extend_from_slice(name.as_bytes());
            central.extend(c);
        }
        let cd = zipped.len() as u32;
        zipped.extend_from_slice(&central);
        zipped.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 2, 0, 2, 0]);
        zipped.extend_from_slice(&(central.len() as u32).to_le_bytes());
        zipped.extend_from_slice(&cd.to_le_bytes());
        zipped.extend_from_slice(&[0, 0]);
        let archive = dir.join("hello-1.0-0.conda");
        fs::write(&archive, &zipped).unwrap();

        let mut record = crate::condaindex::virtual_record("hello", "1.0", "0", "noarch");
        record.filename = "hello-1.0-0.conda".into();
        record.url = "https://c/conda-forge/noarch/hello-1.0-0.conda".into();
        record.noarch = Some("python".into());
        record.license = Some("MIT".into());
        let prefix = dir.join("env");
        let layout = PrefixLayout {
            prefix: prefix.clone(),
            windows: cfg!(windows),
            python_mm: Some("3.12".into()),
        };
        let linked = link(&record, &archive, &layout).unwrap();
        let site = layout.site_packages().unwrap();
        let init = fs::read_to_string(site.join("hello").join("__init__.py")).unwrap();
        assert!(
            init.contains(&format!(
                "PREFIX = '{}'",
                prefix.to_string_lossy().replace('\\', "/")
            )),
            "{init}"
        );
        let script = fs::read_to_string(layout.bin().join("hello-cli")).unwrap();
        assert!(
            script.starts_with(&format!("#!{}\n", layout.python().display())),
            "{script}"
        );
        let ep = if cfg!(windows) {
            "hello-script.py"
        } else {
            "hello"
        };
        assert!(layout.bin().join(ep).is_file());
        assert_eq!(linked.placeholders.len(), 1);
        let meta = fs::read_to_string(prefix.join("conda-meta").join("hello-1.0-0.json")).unwrap();
        assert!(
            meta.contains("\"channel\": \"https://c/conda-forge/noarch\"")
                && meta.contains("\"license\": \"MIT\""),
            "{meta}"
        );
        let found = installed(&prefix).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].files.len(), linked.files.len());

        // Relocation rewrites the recorded files for a new path.
        let moved = dir.join("moved");
        fs::rename(&prefix, &moved).unwrap();
        let changed = relocate(&moved, &prefix.to_string_lossy().replace('\\', "/")).unwrap();
        assert_eq!(changed, 1);
        let init = fs::read_to_string(
            moved
                .join(site.strip_prefix(&prefix).unwrap())
                .join("hello")
                .join("__init__.py"),
        )
        .unwrap();
        assert!(
            init.contains(&moved.to_string_lossy().replace('\\', "/")),
            "{init}"
        );

        unlink(&moved, &found[0]).unwrap();
        assert!(!moved.join("conda-meta").join("hello-1.0-0.json").exists());
        assert!(installed(&moved).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
