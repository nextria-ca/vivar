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
pub use crate::venv::BIN as VENV_BIN;
/// `sha256  path` for every file whose bytes must survive the trip. Files that
/// `unpack` rewrites for the new location are left out.
pub const CHECKSUMS: &str = "vivar-bundle.sha256";
/// `sha256  path` for everything CHECKSUMS leaves out, as packed: the venv's
/// launchers and activate scripts, pyvenv.cfg, every symlink (hash of its
/// target) and the manifest. Checked before relocation; covered by the signature.
pub const INVENTORY: &str = "vivar-bundle.inventory";

/// Mirrors the tar excludes in `pack` so the checksum list and the archive agree.
fn excluded(rel: &Path, python_dir: &str, mm: &str, opts: PackOptions) -> bool {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    let name = parts.last().copied().unwrap_or("");
    if parts.iter().any(|p| {
        matches!(
            *p,
            "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".ruff_cache" | ".hypothesis"
        )
    }) || name.ends_with(".pyc")
        || name.ends_with(".pyo")
    {
        return true;
    }
    if parts.first() == Some(&".venv") {
        if opts.host_cuda {
            let nvidia = parts.windows(4).any(|w| {
                w[0] == "site-packages" && w[1] == "nvidia" && matches!(w[3], "lib" | "bin")
            });
            if nvidia {
                return true;
            }
        }
        return parts.len() == 2 && matches!(parts[1], "CACHEDIR.TAG" | ".gitignore" | ".lock");
    }
    if parts.first() == Some(&python_dir) {
        let unix = format!("python{mm}");
        let stdlib = parts.len() > 3 && parts[1] == "lib" && parts[2] == unix;
        let windows = parts.len() > 2 && parts[1] == "Lib";
        let dir = if stdlib {
            parts[3]
        } else if windows {
            parts[2]
        } else {
            ""
        };
        if matches!(dir, "test" | "site-packages" | "ensurepip" | "idlelib") {
            return true;
        }
        if parts.len() > 3 && matches!(parts[1], "lib" | "tcl") && parts[3] == "demos" {
            return true;
        }
        if parts.len() > 2 && parts[1] == "share" && parts[2] == "man" {
            return true;
        }
        if opts.slim && parts.len() == 3 && parts[1] == "lib" && name.starts_with("libpython") {
            return true;
        }
    }
    false
}

/// Files `unpack` relocates by rewriting: never hashed. The venv's bin is
/// `bin` or `Scripts` by the bundle's platform, not this machine's.
fn relocated(rel: &Path) -> bool {
    let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
    parts.first() == Some(&".venv")
        && (parts.get(1) == Some(&"pyvenv.cfg")
            || (matches!(parts.get(1), Some(&"bin") | Some(&"Scripts")) && parts.len() == 3))
}

fn walk(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(base.join(rel))?.flatten() {
        let child = rel.join(entry.file_name());
        let meta = entry.metadata()?;
        if meta.is_symlink() {
            continue;
        } else if meta.is_dir() {
            walk(base, &child, out)?;
        } else {
            out.push(child);
        }
    }
    Ok(())
}

/// Checksum list for everything `pack` archives, as members would appear.
fn checksums(
    root: &Path,
    python_root: &Path,
    python_dir: &str,
    mm: &str,
    opts: PackOptions,
) -> Result<String> {
    let mut files = Vec::new();
    walk(root, Path::new(".venv"), &mut files)?;
    for file in ["pyproject.toml", "uv.lock", ".python-version"] {
        if root.join(file).is_file() {
            files.push(PathBuf::from(file));
        }
    }
    let python_parent = python_root.parent().unwrap_or(python_root);
    walk(python_parent, Path::new(python_dir), &mut files)?;

    let mut out = String::new();
    for rel in files {
        if excluded(&rel, python_dir, mm, opts) || relocated(&rel) {
            continue;
        }
        let base = if rel.starts_with(python_dir) {
            python_parent
        } else {
            root
        };
        let digest = crate::sha256::file(&base.join(&rel))
            .with_context(|| format!("hashing {}", rel.display()))?;
        let member = rel.to_string_lossy().replace('\\', "/");
        out.push_str(&format!("{digest}  {member}\n"));
    }
    Ok(out)
}

/// The pre-relocation inventory for `root` (see [`INVENTORY`]).
fn inventory(
    root: &Path,
    python_root: &Path,
    python_dir: &str,
    mm: &str,
    opts: PackOptions,
    manifest: &[u8],
) -> Result<String> {
    let mut lines = Vec::new();
    let venv = root.join(".venv");
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if venv.join("pyvenv.cfg").is_file() {
        files.push((".venv/pyvenv.cfg".to_owned(), venv.join("pyvenv.cfg")));
    }
    for bin_name in ["bin", "Scripts"] {
        let Ok(entries) = fs::read_dir(venv.join(bin_name)) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if e.path().is_file() && !e.path().is_symlink() {
                files.push((
                    format!(".venv/{bin_name}/{}", e.file_name().to_string_lossy()),
                    e.path(),
                ));
            }
        }
    }
    for (member, path) in files {
        let digest = crate::sha256::file(&path).with_context(|| format!("hashing {member}"))?;
        lines.push(format!("{digest}  {member}"));
    }
    let mut digest = crate::sha256::Sha256::new();
    digest.update(manifest);
    lines.push(format!(
        "{}  {MANIFEST}",
        crate::sha256::hex(&digest.finish())
    ));
    // Symlinks: the target string is what matters.
    let python_parent = python_root.parent().unwrap_or(python_root);
    for (base, start) in [
        (root, Path::new(".venv")),
        (python_parent, Path::new(python_dir)),
    ] {
        let mut links = Vec::new();
        symlinks(base, start, &mut links)?;
        links.sort();
        for rel in links {
            if excluded(&rel, python_dir, mm, opts) {
                continue;
            }
            let target = fs::read_link(base.join(&rel))?;
            let mut h = crate::sha256::Sha256::new();
            h.update(target.to_string_lossy().as_bytes());
            lines.push(format!(
                "{}  link:{}",
                crate::sha256::hex(&h.finish()),
                rel.to_string_lossy().replace('\\', "/")
            ));
        }
    }
    Ok(lines.join("\n") + "\n")
}

fn symlinks(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(base.join(rel))?.flatten() {
        let child = rel.join(entry.file_name());
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_symlink() {
            out.push(child);
        } else if meta.is_dir() {
            symlinks(base, &child, out)?;
        }
    }
    Ok(())
}

/// Checks an extracted, not yet relocated bundle against its inventory.
fn verify_inventory(dir: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(dir.join(INVENTORY))?;
    let mut bad = Vec::new();
    let hash_bytes = |b: &[u8]| {
        let mut h = crate::sha256::Sha256::new();
        h.update(b);
        crate::sha256::hex(&h.finish())
    };
    for (member, digest) in parse_checksums(&text) {
        let actual = if let Some(link) = member.strip_prefix("link:") {
            fs::read_link(dir.join(link))
                .ok()
                .map(|t| hash_bytes(t.to_string_lossy().as_bytes()))
        } else if member == MANIFEST {
            fs::read(dir.join(MANIFEST)).ok().map(|m| hash_bytes(&m))
        } else {
            crate::sha256::file(&dir.join(&member)).ok()
        };
        match actual {
            Some(a) if a == digest => {}
            Some(_) => bad.push(format!("{member} (modified)")),
            None => bad.push(format!("{member} (missing)")),
        }
    }
    // Launchers that aren't in the inventory were added after packing.
    if let Ok(entries) = fs::read_dir(dir.join(".venv").join(VENV_BIN)) {
        let listed: std::collections::HashSet<String> =
            parse_checksums(&text).into_iter().map(|(m, _)| m).collect();
        for e in entries.flatten() {
            let member = format!(".venv/{VENV_BIN}/{}", e.file_name().to_string_lossy());
            if e.path().is_file() && !e.path().is_symlink() && !listed.contains(&member) {
                bad.push(format!("{member} (not in the bundle)"));
            }
        }
    }
    Ok(bad)
}

/// Checks every file in a bundle directory against its checksum list. Returns
/// the mismatching or missing members; unlisted regular files count too.
pub fn verify_checksums(dir: &Path) -> Result<Vec<String>> {
    let list = fs::read_to_string(dir.join(CHECKSUMS))
        .with_context(|| format!("{} has no {CHECKSUMS}", dir.display()))?;
    let mut bad = Vec::new();
    let mut listed = std::collections::BTreeSet::new();
    for line in list.lines() {
        let Some((digest, member)) = line.split_once("  ") else {
            continue;
        };
        listed.insert(member.to_owned());
        match crate::sha256::file(&dir.join(member)) {
            Ok(actual) if actual == digest => {}
            Ok(_) => bad.push(format!("{member} (modified)")),
            Err(_) => bad.push(format!("{member} (missing)")),
        }
    }
    let manifest: DocumentMut = fs::read_to_string(dir.join(MANIFEST))?.parse()?;
    let python_dir = manifest
        .get("python-dir")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mm = major_minor(
        manifest
            .get("python")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
    );
    let mut files = Vec::new();
    walk(dir, Path::new(".venv"), &mut files)?;
    if !python_dir.is_empty() && dir.join(python_dir).is_dir() {
        walk(dir, Path::new(python_dir), &mut files)?;
    }
    for rel in files {
        let member = rel.to_string_lossy().replace('\\', "/");
        // A slim bundle legitimately lacks libpython; anything else unlisted is foreign.
        let lenient = PackOptions {
            slim: true,
            host_cuda: true,
            ..PackOptions::default()
        };
        if !listed.contains(&member)
            && !excluded(&rel, python_dir, &mm, lenient)
            && !relocated(&rel)
        {
            bad.push(format!("{member} (not in the bundle)"));
        }
    }
    Ok(bad)
}

/// `vivar diff`: what changed between two bundles (archives or unpacked/project dirs).
pub fn diff(a: &Path, b: &Path) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("vivar-diff-{}", std::process::id()));
    let result = (|| {
        let (ma, la) = metadata_of(a, &tmp.join("a"))?;
        let (mb, lb) = metadata_of(b, &tmp.join("b"))?;
        let mut out = String::new();
        for key in ["python", "platform"] {
            let (va, vb) = (
                ma.get(key).cloned().unwrap_or_default(),
                mb.get(key).cloned().unwrap_or_default(),
            );
            if va != vb {
                out.push_str(&format!("{key}: {va} -> {vb}\n"));
            }
        }
        // Every variant counts: a forked lock can pin two versions of one package.
        let versions = |lock: &crate::lock::Lock| {
            let mut map: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
                Default::default();
            for p in &lock.packages {
                map.entry(p.name.clone())
                    .or_default()
                    .insert(p.version.clone());
            }
            map
        };
        let (va, vb) = (versions(&la), versions(&lb));
        let joined = |set: &std::collections::BTreeSet<String>| {
            set.iter().cloned().collect::<Vec<_>>().join("/")
        };
        for (name, new) in &vb {
            match va.get(name) {
                None => out.push_str(&format!("+ {name} {}\n", joined(new))),
                Some(old) if old != new => {
                    out.push_str(&format!("~ {name} {} -> {}\n", joined(old), joined(new)))
                }
                _ => {}
            }
        }
        for (name, old) in &va {
            if !vb.contains_key(name) {
                out.push_str(&format!("- {name} {}\n", joined(old)));
            }
        }
        Ok(out)
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// Manifest fields and lockfile of a bundle archive or a directory.
fn metadata_of(
    path: &Path,
    scratch: &Path,
) -> Result<(
    std::collections::BTreeMap<String, String>,
    crate::lock::Lock,
)> {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        fs::create_dir_all(scratch)?;
        let status = Command::new("tar")
            .arg("-xf")
            .arg(path)
            .arg("-C")
            .arg(scratch)
            .args([MANIFEST, crate::lock::LOCKFILE])
            .status()
            .context("failed to run `tar`")?;
        if !status.success() {
            bail!("{} is not a vivar bundle", path.display());
        }
        scratch.to_path_buf()
    };
    let mut fields = std::collections::BTreeMap::new();
    if let Ok(text) = fs::read_to_string(dir.join(MANIFEST)) {
        let doc: DocumentMut = text.parse().context("parsing bundle manifest")?;
        for (k, v) in doc.iter() {
            if let Some(v) = v.as_str() {
                fields.insert(k.to_owned(), v.to_owned());
            }
        }
    } else if let Ok(version) = pyvenv_cfg(&dir.join(".venv"), "version_info") {
        fields.insert("python".to_owned(), version);
        fields.insert("platform".to_owned(), platform());
    }
    let lock = crate::lock::Lock::read(&dir)?;
    Ok((fields, lock))
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Absolute path without symlink resolution and without Windows' `\\?\`
/// prefix, which Python and activate scripts don't cope with.
pub fn absolute(path: &Path) -> Result<PathBuf> {
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
/// What to leave out of a bundle and how to compress it.
#[derive(Debug, Clone, Copy, Default)]
pub struct PackOptions {
    /// Drop libpython (only embedders need it).
    pub slim: bool,
    /// zstd instead of gzip: several times faster on multi-GB environments.
    pub zstd: bool,
    /// Drop the NVIDIA libraries from `nvidia-*` wheels; the target supplies CUDA.
    pub host_cuda: bool,
    /// Authenticate the bundle with the org's bundle key (`vivar key generate`).
    pub sign: bool,
}

/// The files a bundle is made from.
struct Tree {
    root: PathBuf,
    python_root: PathBuf,
    python_dir: String,
    version: String,
    /// The venv and Python paths as recorded in the manifest, which `unpack`
    /// rewrites to the destination.
    venv: String,
    home: String,
    platform: String,
}

pub fn pack(root: &Path, name: &str, output: Option<&Path>, opts: PackOptions) -> Result<PathBuf> {
    let venv = root.join(".venv");
    let home = PathBuf::from(pyvenv_cfg(&venv, "home")?);
    let version = pyvenv_cfg(&venv, "version_info")?;
    let python_root = python_root(&home)?;
    let python_dir = python_root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("unexpected Python root {}", python_root.display()))?
        .to_owned();
    let tree = Tree {
        root: root.to_path_buf(),
        python_root,
        python_dir,
        version,
        venv: venv.to_string_lossy().into_owned(),
        home: home.to_string_lossy().into_owned(),
        platform: platform(),
    };
    archive(&tree, name, output, opts)
}

/// Paths inside a cross-built bundle are recorded under this root; `unpack`
/// rewrites them, exactly as it does for a bundle packed in place.
const FAKE_ROOT: &str = "/vivar-bundle";

/// Builds a bundle for another Linux or macOS machine: its Python build is
/// downloaded, its wheels laid out by the backend, and the venv assembled by
/// hand. Nothing from the target platform is executed.
#[allow(clippy::too_many_arguments)]
pub fn pack_for_target(
    backend: &dyn crate::backend::Backend,
    policy: &crate::project::Policy,
    root: &Path,
    name: &str,
    output: Option<&Path>,
    os: &str,
    arch: &str,
    python: &str,
    profile: Option<&str>,
    opts: PackOptions,
) -> Result<PathBuf> {
    if !cfg!(unix) {
        bail!("cross-target bundles can be built on Linux or macOS hosts only");
    }
    let stage = std::env::temp_dir().join(format!("vivar-xpack-{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage)?;
    let result = build_for_target(
        backend, policy, root, name, output, os, arch, python, profile, opts, &stage,
    );
    let _ = fs::remove_dir_all(&stage);
    result
}

#[allow(clippy::too_many_arguments)]
fn build_for_target(
    backend: &dyn crate::backend::Backend,
    policy: &crate::project::Policy,
    root: &Path,
    name: &str,
    output: Option<&Path>,
    os: &str,
    arch: &str,
    python: &str,
    profile: Option<&str>,
    opts: PackOptions,
    stage: &Path,
) -> Result<PathBuf> {
    // 1. The interpreter the bundle will run on.
    let (key, version) = crate::python::download_for(os, arch, python, stage)?;
    let [major, minor, patch] = version;
    let mm = format!("{major}.{minor}");

    // 2. The locked packages, laid out for the target by the backend. A
    // Windows bundle always uses vivar's own layout: its console scripts are
    // `.cmd` shims beside `python.exe`, which need no launcher binaries.
    let venv_dir = stage.join(".venv");
    let windows = os == "windows";
    let site = if windows {
        venv_dir.join("Lib").join("site-packages")
    } else {
        venv_dir
            .join("lib")
            .join(format!("python{mm}"))
            .join("site-packages")
    };
    fs::create_dir_all(&site)?;
    let full = format!("{major}.{minor}.{patch}");
    let native;
    let backend: &dyn crate::backend::Backend = if windows {
        native = crate::native::NativeBackend::new(policy);
        &native
    } else {
        backend
    };
    backend.install_for_target(
        root,
        &crate::python::triple_for(os, arch),
        &full,
        profile,
        &site,
    )?;

    // 3. A venv around them, with the paths unpack will rewrite.
    let fake_venv = format!("{FAKE_ROOT}/.venv");
    let fake_home = if windows {
        format!("{FAKE_ROOT}/{key}")
    } else {
        format!("{FAKE_ROOT}/{key}/bin")
    };
    let bin_name = if windows { "Scripts" } else { "bin" };
    let bin = venv_dir.join(bin_name);
    fs::create_dir_all(&bin)?;
    if windows {
        // CPython's own venv launchers from the bundled Python: they read
        // pyvenv.cfg and start `home\python.exe`, wherever the bundle lands.
        let launchers = stage
            .join(&key)
            .join("Lib")
            .join("venv")
            .join("scripts")
            .join("nt");
        for exe in ["python.exe", "pythonw.exe"] {
            fs::copy(launchers.join(exe), bin.join(exe)).with_context(|| {
                format!("copying the venv launcher from {}", launchers.display())
            })?;
        }
        // Console-script shims and their bodies move beside python.exe as they
        // are: a shim finds python.exe relative to itself.
        if let Ok(entries) = fs::read_dir(site.join("bin")) {
            for entry in entries.flatten() {
                fs::rename(entry.path(), bin.join(entry.file_name()))?;
            }
            fs::remove_dir_all(site.join("bin"))?;
        }
    }
    #[cfg(unix)]
    if !windows {
        use std::os::unix::fs::symlink;
        symlink(format!("{fake_home}/python{mm}"), bin.join("python"))?;
        symlink("python", bin.join("python3"))?;
        symlink("python", bin.join(format!("python{mm}")))?;
        if os == "linux" {
            symlink("lib", venv_dir.join("lib64"))?;
        }
    }
    {
        // Wheel data that belongs beside site-packages, not inside it. RECORD
        // files follow, so metadata lookups and uninstalls still find it.
        // site-packages sits two levels below the venv on Windows, three elsewhere.
        let up = if windows { "../../" } else { "../../../" };
        let mut moved = Vec::new();
        for dir in ["share", "include", "etc"] {
            let from = site.join(dir);
            if from.is_dir() && !from.join("__init__.py").exists() {
                let to = venv_dir.join(dir);
                fs::create_dir_all(&to)?;
                for entry in fs::read_dir(&from)?.flatten() {
                    fs::rename(entry.path(), to.join(entry.file_name()))?;
                }
                fs::remove_dir_all(&from)?;
                moved.push(dir);
            }
        }
        if !moved.is_empty() {
            for entry in fs::read_dir(&site)?.flatten() {
                let record = entry.path().join("RECORD");
                let Ok(text) = fs::read_to_string(&record) else {
                    continue;
                };
                let rewritten: String = text
                    .lines()
                    .map(
                        |line| match moved.iter().find(|d| line.starts_with(&format!("{d}/"))) {
                            Some(_) => format!("{up}{line}\n"),
                            None => format!("{line}\n"),
                        },
                    )
                    .collect();
                if rewritten != text {
                    fs::write(&record, rewritten)?;
                }
            }
        }
    }
    #[cfg(unix)]
    if !windows {
        use std::os::unix::fs::PermissionsExt;
        // uv's --target writes console scripts under site-packages/bin with a
        // host shebang; they belong in the venv's bin, pointing at its python.
        if let Ok(entries) = fs::read_dir(site.join("bin")) {
            for entry in entries.flatten() {
                let dest = bin.join(entry.file_name());
                let data = fs::read(entry.path())?;
                let first_line = data.split(|b| *b == b'\n').next().unwrap_or(&[]);
                let python_shebang =
                    data.starts_with(b"#!") && first_line.windows(6).any(|w| w == b"python");
                // A host path with spaces makes uv emit a 3-line `#!/bin/sh` trampoline
                // (`'''exec' "<python>" "$0" "$@"`) ahead of the Python code.
                let sh_trampoline =
                    data.starts_with(b"#!/bin/sh") && data.windows(8).any(|w| w == b"'''exec'");
                // Only Python launchers point at the venv; a real shell script stays as is.
                if python_shebang || sh_trampoline {
                    let skip = if sh_trampoline { 3 } else { 1 };
                    let body: Vec<u8> = data
                        .split_inclusive(|b| *b == b'\n')
                        .skip(skip)
                        .flatten()
                        .copied()
                        .collect();
                    let mut script = format!("#!{fake_venv}/bin/python\n").into_bytes();
                    script.extend_from_slice(&body);
                    fs::write(&dest, script)?;
                } else {
                    fs::write(&dest, data)?;
                }
                fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
            }
            fs::remove_dir_all(site.join("bin"))?;
        }
    }
    fs::write(
        venv_dir.join("pyvenv.cfg"),
        format!(
            "home = {fake_home}\nimplementation = CPython\nversion = {v}\nversion_info = {v}\n\
             include-system-site-packages = false\nprompt = {name}\ncreated-by = vivar {}\n",
            env!("CARGO_PKG_VERSION"),
            v = format!("{major}.{minor}.{patch}"),
        ),
    )?;
    crate::venv::write_activate_scripts(&bin, &fake_venv, name, bin_name, windows)?;
    crate::venv::mark_profile(stage, profile);
    // The same license policy a local sync enforces, on what was actually staged.
    crate::licenses::check_site(
        &site,
        policy.allow_licenses.as_deref(),
        &policy.license_exceptions,
    )?;

    // 4. The project files, so the bundle is a project too.
    for file in ["pyproject.toml", "uv.lock", ".python-version"] {
        if root.join(file).is_file() {
            fs::copy(root.join(file), stage.join(file))?;
        }
    }

    let tree = Tree {
        root: stage.to_path_buf(),
        python_root: stage.join(&key),
        python_dir: key,
        version: format!("{major}.{minor}.{patch}"),
        venv: fake_venv,
        home: fake_home,
        platform: format!("{os}-{arch}"),
    };
    archive(&tree, name, output, opts)
}

fn archive(tree: &Tree, name: &str, output: Option<&Path>, opts: PackOptions) -> Result<PathBuf> {
    let ext = if opts.zstd { "tar.zst" } else { "tar.gz" };
    let output = absolute(
        &output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(format!("{name}-{}.{ext}", tree.platform))),
    )?;
    let python_dir = &tree.python_dir;

    let mut manifest = DocumentMut::new();
    manifest["name"] = value(name);
    manifest["platform"] = value(&tree.platform);
    manifest["python"] = value(&tree.version);
    manifest["python-dir"] = value(python_dir);
    manifest["python-home"] = value(&tree.home);
    manifest["venv"] = value(&tree.venv);
    manifest["created-by"] = value(concat!("vivar ", env!("CARGO_PKG_VERSION")));
    if opts.host_cuda {
        manifest["host-cuda"] = value(true);
    }

    let staging = std::env::temp_dir().join(format!("vivar-pack-{}", std::process::id()));
    fs::create_dir_all(&staging)?;
    fs::write(staging.join(MANIFEST), manifest.to_string())?;
    let mm = major_minor(&tree.version);
    let sums = checksums(&tree.root, &tree.python_root, python_dir, &mm, opts)?;
    fs::write(staging.join(CHECKSUMS), &sums)?;
    let manifest_bytes = manifest.to_string().into_bytes();
    let inv = inventory(
        &tree.root,
        &tree.python_root,
        python_dir,
        &mm,
        opts,
        &manifest_bytes,
    )?;
    fs::write(staging.join(INVENTORY), &inv)?;
    if opts.sign {
        let key = crate::sign::require_key()?;
        fs::write(
            staging.join(crate::sign::SIGNATURE),
            crate::sign::sign(sums.as_bytes(), inv.as_bytes(), &key),
        )?;
    }

    let mut tar = Command::new("tar");
    tar.arg("-c");
    if opts.zstd {
        tar.arg("--zstd");
    } else {
        tar.arg("-z");
    }
    tar.arg("-f").arg(&output);
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
    if opts.slim {
        // The interpreter is statically linked; libpython only serves embedders.
        tar.arg(format!("--exclude={python_dir}/lib/libpython*"));
    }
    if opts.host_cuda {
        for sub in ["lib", "bin"] {
            tar.arg(format!("--exclude=site-packages/nvidia/*/{sub}"));
        }
    }
    tar.arg(format!("--exclude={python_dir}/share/man"))
        .arg("-C")
        .arg(&staging)
        .arg(MANIFEST)
        .arg(CHECKSUMS)
        .arg(INVENTORY);
    if opts.sign {
        tar.arg(crate::sign::SIGNATURE);
    }
    tar.arg("-C").arg(&tree.root).arg(".venv");
    for file in ["pyproject.toml", "uv.lock", ".python-version"] {
        if tree.root.join(file).is_file() {
            tar.arg(file);
        }
    }
    tar.arg("-C")
        .arg(tree.python_root.parent().unwrap_or(&tree.python_root))
        .arg(python_dir);

    let status = tar.status().context("failed to run `tar`")?;
    let _ = fs::remove_dir_all(&staging);
    if !status.success() {
        if opts.zstd {
            bail!("tar exited with status {} (is zstd installed? `vivar pack` without --zstd uses gzip)", status.code().unwrap_or(1));
        }
        bail!("tar exited with status {}", status.code().unwrap_or(1));
    }
    Ok(output)
}

/// Extracts a bundle into `dest` and points the venv at the Python that came
/// with it. Returns the absolute destination.
pub fn unpack(
    archive: &Path,
    dest: &Path,
    require_signature: bool,
    trusted_keys: &[String],
    dedupe_with: Option<&Path>,
) -> Result<PathBuf> {
    fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let dest = absolute(dest)?;
    if dest.join(".venv").exists() {
        bail!(
            "{} already contains a .venv; unpack into an empty directory",
            dest.display()
        );
    }
    extract(archive, &dest)?;
    if !dest.join(MANIFEST).is_file() {
        bail!(
            "{} is not a vivar bundle (no {MANIFEST})",
            archive.display()
        );
    }
    check_signature(&dest, require_signature, trusted_keys)?;
    let mut bad = verify_checksums(&dest)?;
    if dest.join(INVENTORY).is_file() {
        bad.extend(verify_inventory(&dest)?);
    }
    if !bad.is_empty() {
        bail!(
            "{} failed its integrity check; {} file(s) differ from when it was packed:\n  {}",
            archive.display(),
            bad.len(),
            bad.iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    relocate(&dest)?;
    if let Some(other) = dedupe_with {
        let saved = dedupe(&dest, other)?;
        println!(
            "deduplicated {:.1} MB against {}",
            saved as f64 / 1e6,
            other.display()
        );
    }
    Ok(dest)
}

fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("failed to run `tar`")?;
    if !status.success() {
        bail!("tar exited with status {}", status.code().unwrap_or(1));
    }
    Ok(())
}

/// Verifies the signature when there is one or when one is required, against
/// the keys this machine trusts (policy, `VIVAR_TRUSTED_KEYS`, its own).
fn check_signature(dir: &Path, required: bool, trusted_keys: &[String]) -> Result<()> {
    let has_sig = dir.join(crate::sign::SIGNATURE).is_file();
    if !has_sig && !required {
        return Ok(());
    }
    if !has_sig {
        bail!("this bundle is not signed and policy (or --require-signature) demands it");
    }
    let trusted = crate::sign::trusted_keys(trusted_keys)?;
    crate::sign::verify(dir, &trusted)?;
    println!("signature verified with a trusted key");
    Ok(())
}

/// A member name from a checksum list or delta that is safe to join onto a
/// bundle directory: relative, no `..`, no absolute prefix.
fn safe_member(member: &str) -> bool {
    let p = Path::new(member);
    !member.is_empty()
        && p.is_relative()
        && !member.starts_with(['/', '\\'])
        && !member.contains(':')
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn manifest_of(dir: &Path) -> Result<DocumentMut> {
    fs::read_to_string(dir.join(MANIFEST))
        .with_context(|| format!("{} has no {MANIFEST}", dir.display()))?
        .parse()
        .context("parsing bundle manifest")
}

/// Points an extracted bundle's venv at its own Python: symlinks, text paths
/// and (Windows) launcher resources. Idempotent, so a delta can re-run it.
fn relocate(dest: &Path) -> Result<()> {
    let manifest = manifest_of(dest)?;
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
    Ok(())
}

/// Parses a checksum list into `(member, sha256)` pairs.
fn parse_checksums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.split_once("  "))
        .map(|(d, m)| (m.to_owned(), d.to_owned()))
        .collect()
}

/// Replaces files in `dest` that are byte-identical (per both checksum lists)
/// to files in `other` with hard links to them. Returns the bytes saved.
fn dedupe(dest: &Path, other: &Path) -> Result<u64> {
    let theirs: std::collections::HashMap<String, String> = parse_checksums(
        &fs::read_to_string(other.join(CHECKSUMS))
            .with_context(|| format!("{} is not an unpacked vivar bundle", other.display()))?,
    )
    .into_iter()
    .collect();
    let mut saved = 0;
    for (member, digest) in parse_checksums(&fs::read_to_string(dest.join(CHECKSUMS))?) {
        if theirs.get(&member) != Some(&digest) {
            continue;
        }
        let (mine, existing) = (dest.join(&member), other.join(&member));
        let (Ok(a), Ok(b)) = (fs::metadata(&mine), fs::metadata(&existing)) else {
            continue;
        };
        if !a.is_file() || !b.is_file() || a.len() != b.len() || !safe_member(&member) {
            continue;
        }
        // The other bundle's list says what the file was; check what it is.
        if crate::sha256::file(&existing).ok().as_deref() != Some(digest.as_str()) {
            continue;
        }
        let tmp = mine.with_extension("vivar-dedupe");
        if fs::hard_link(&existing, &tmp).is_err() {
            // Different filesystem or no hard-link support: stop quietly.
            let _ = fs::remove_file(&tmp);
            break;
        }
        fs::rename(&tmp, &mine)?;
        saved += a.len();
    }
    Ok(saved)
}

const DELTA: &str = "vivar-delta.toml";

/// `vivar diff --patch old new -o delta`: an archive holding what changed
/// between two bundles of the same environment line, plus the new manifest and
/// checksums. `unpack --apply` turns an unpacked `old` into `new`.
pub fn make_delta(old: &Path, new: &Path, output: &Path) -> Result<String> {
    let tmp = std::env::temp_dir().join(format!("vivar-delta-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    let result = (|| {
        let (old_dir, new_dir, stage) = (tmp.join("old"), tmp.join("new"), tmp.join("stage"));
        for d in [&old_dir, &new_dir, &stage] {
            fs::create_dir_all(d)?;
        }
        // Only the checksum lists of the old bundle are needed; everything of the new one.
        let status = Command::new("tar")
            .arg("-xf")
            .arg(old)
            .arg("-C")
            .arg(&old_dir)
            .args([MANIFEST, CHECKSUMS])
            .status()
            .context("failed to run `tar`")?;
        if !status.success() {
            bail!("{} is not a vivar bundle", old.display());
        }
        // Older bundles have no inventory; a missing one just means "all differ".
        let _ = Command::new("tar")
            .arg("-xf")
            .arg(old)
            .arg("-C")
            .arg(&old_dir)
            .arg(INVENTORY)
            .output();
        extract(new, &new_dir)?;
        let old_m = manifest_of(&old_dir)?;
        let new_m = manifest_of(&new_dir)?;
        for key in ["platform", "python-dir"] {
            let (a, b) = (
                old_m.get(key).and_then(|v| v.as_str()),
                new_m.get(key).and_then(|v| v.as_str()),
            );
            if key == "platform" && a != b {
                bail!("bundles are for different platforms ({:?} vs {:?})", a, b);
            }
        }
        let old_sums = fs::read(old_dir.join(CHECKSUMS))?;
        let old_inv = fs::read_to_string(old_dir.join(INVENTORY)).unwrap_or_default();
        let new_inv = fs::read_to_string(new_dir.join(INVENTORY))?;
        // Both lists together are the complete member inventory, symlinks included.
        let strip = |m: String| m.strip_prefix("link:").map(str::to_owned).unwrap_or(m);
        let old_list: std::collections::HashMap<String, String> =
            parse_checksums(&String::from_utf8_lossy(&old_sums))
                .into_iter()
                .chain(parse_checksums(&old_inv))
                .map(|(m, d)| (strip(m), d))
                .collect();
        let new_list: Vec<(String, String)> =
            parse_checksums(&fs::read_to_string(new_dir.join(CHECKSUMS))?)
                .into_iter()
                .chain(parse_checksums(&new_inv))
                .map(|(m, d)| (strip(m), d))
                .collect();

        let mut changed = Vec::new();
        for (member, digest) in &new_list {
            if member != MANIFEST && old_list.get(member) != Some(digest) {
                changed.push(member.clone());
            }
        }
        let new_names: std::collections::HashSet<&String> =
            new_list.iter().map(|(m, _)| m).collect();
        let removed: Vec<&String> = old_list
            .keys()
            .filter(|m| !new_names.contains(m) && *m != MANIFEST && safe_member(m))
            .collect();

        // Everything in the inventory always travels: launchers, activate scripts,
        // pyvenv.cfg and symlinks are small, get rewritten by relocation, and
        // this way the whole executable surface is replaced and verified.
        let always: Vec<String> = parse_checksums(&new_inv)
            .into_iter()
            .map(|(m, _)| strip(m))
            .filter(|m| m != MANIFEST && !changed.contains(m))
            .collect();
        for member in changed.iter().chain(always.iter()) {
            let (from, to) = (new_dir.join(member), stage.join(member));
            let Ok(meta) = fs::symlink_metadata(&from) else {
                continue;
            };
            fs::create_dir_all(to.parent().context("member has no parent")?)?;
            if meta.is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(fs::read_link(&from)?, &to)?;
            } else {
                fs::copy(&from, &to)?;
            }
        }
        fs::copy(new_dir.join(MANIFEST), stage.join(MANIFEST))?;
        fs::copy(new_dir.join(CHECKSUMS), stage.join(CHECKSUMS))?;
        fs::copy(new_dir.join(INVENTORY), stage.join(INVENTORY))?;
        if new_dir.join(crate::sign::SIGNATURE).is_file() {
            fs::copy(
                new_dir.join(crate::sign::SIGNATURE),
                stage.join(crate::sign::SIGNATURE),
            )?;
        }
        let mut delta = DocumentMut::new();
        delta["base-checksums"] = value(crate::sha256::hex(&{
            let mut h = crate::sha256::Sha256::new();
            h.update(&old_sums);
            h.finish()
        }));
        let mut removed_arr = toml_edit::Array::new();
        for r in &removed {
            removed_arr.push(r.as_str());
        }
        delta["removed"] = value(removed_arr);
        delta["created-by"] = value(concat!("vivar ", env!("CARGO_PKG_VERSION")));
        fs::write(stage.join(DELTA), delta.to_string())?;

        let output = absolute(output)?;
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&output)
            .arg("-C")
            .arg(&stage)
            .arg(".")
            .status()
            .context("failed to run `tar`")?;
        if !status.success() {
            bail!("tar exited with status {}", status.code().unwrap_or(1));
        }
        let size = fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
        Ok(format!(
            "{} changed, {} removed; delta {} ({:.1} MB)",
            changed.len(),
            removed.len(),
            output.display(),
            size as f64 / 1e6
        ))
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// `vivar unpack --apply delta dir`: brings an unpacked bundle up to the new
/// one. The directory must still match the delta's base checksum list.
pub fn apply_delta(
    delta: &Path,
    dir: &Path,
    require_signature: bool,
    trusted_keys: &[String],
) -> Result<()> {
    let dir = absolute(dir)?;
    if !dir.join(CHECKSUMS).is_file() {
        bail!("{} is not an unpacked vivar bundle", dir.display());
    }
    let tmp = std::env::temp_dir().join(format!("vivar-apply-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    let result = (|| {
        extract(delta, &tmp)?;
        let meta: DocumentMut = fs::read_to_string(tmp.join(DELTA))
            .with_context(|| format!("{} is not a vivar delta", delta.display()))?
            .parse()?;
        let base = meta
            .get("base-checksums")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let current = crate::sha256::file(&dir.join(CHECKSUMS))?;
        if base != current {
            bail!(
                "this delta was made against a different bundle than the one in {}",
                dir.display()
            );
        }
        // The base must be intact before anything is touched, or a failed update
        // could never be retried.
        let damaged = verify_checksums(&dir)?;
        if !damaged.is_empty() {
            bail!(
                "{} no longer matches the bundle it was unpacked from ({} file(s)); re-unpack it before applying a delta",
                dir.display(),
                damaged.len()
            );
        }
        check_signature(&tmp, require_signature, trusted_keys)?;
        // Only members the base actually had can be removed, and only inside it.
        let base_members: std::collections::HashSet<String> =
            parse_checksums(&fs::read_to_string(dir.join(CHECKSUMS))?)
                .into_iter()
                .chain(parse_checksums(
                    &fs::read_to_string(dir.join(INVENTORY)).unwrap_or_default(),
                ))
                .map(|(m, _)| m.strip_prefix("link:").map(str::to_owned).unwrap_or(m))
                .collect();
        let removed: Vec<String> = meta
            .get("removed")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        for member in &removed {
            if !safe_member(member) || !base_members.contains(member) {
                bail!("delta asks to remove {member:?}, which is not part of the bundle; refusing");
            }
        }
        for member in &removed {
            let _ = fs::remove_file(dir.join(member));
        }
        // Copy the delta's files over; the old bundle's relocated files are replaced.
        copy_tree(&tmp, &dir)?;
        let _ = fs::remove_file(dir.join(DELTA));
        if !tmp.join(crate::sign::SIGNATURE).is_file() {
            let _ = fs::remove_file(dir.join(crate::sign::SIGNATURE));
        }
        let mut bad = verify_checksums(&dir)?;
        // Every inventory member travelled in the delta and was just copied in,
        // so the destination is checked against the new inventory before relocation.
        bad.extend(verify_inventory(&dir)?);
        if !bad.is_empty() {
            bail!(
                "the updated bundle fails its integrity check ({} file(s)):\n  {}",
                bad.len(),
                bad.iter()
                    .take(20)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
        }
        relocate(&dir)?;
        println!(
            "applied {} to {}: {} file(s) removed",
            delta.display(),
            dir.display(),
            removed.len()
        );
        Ok(())
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)?.flatten() {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let meta = fs::symlink_metadata(&src)?;
        if meta.is_dir() {
            fs::create_dir_all(&dst)?;
            copy_tree(&src, &dst)?;
        } else if meta.is_symlink() {
            let _ = fs::remove_file(&dst);
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&src)?, &dst)?;
            #[cfg(windows)]
            fs::copy(&src, &dst)?;
        } else {
            let _ = fs::remove_file(&dst);
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
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
    if new.len() > old.len() {
        bail!(
            "{}: launcher path {new} is longer than the original; unpack into a shorter directory",
            exe.display()
        );
    }
    // The path can occur more than once (the embedded script's shebang holds a
    // copy); only the occurrence backed by a resource entry is the launcher's
    // interpreter path. Skipping the others keeps a second relocation harmless.
    let mut from = 0;
    while let Some(rel) = find(&data[from..], old) {
        let off = from + rel;
        let Some(rva) = rva_of(&data, off) else {
            bail!("{} is not a PE file vivar understands", exe.display());
        };
        let mut entry_key = rva.to_le_bytes().to_vec();
        entry_key.extend_from_slice(&(old.len() as u32).to_le_bytes());
        if let Some(entry) = find(&data, &entry_key) {
            data[entry + 4..entry + 8].copy_from_slice(&(new.len() as u32).to_le_bytes());
            data[off..off + old.len()].fill(0);
            data[off..off + new.len()].copy_from_slice(new.as_bytes());
            fs::write(exe, data).with_context(|| format!("rewriting {}", exe.display()))?;
            return Ok(true);
        }
        from = off + 1;
    }
    Ok(false)
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
