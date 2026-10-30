//! Native (conda-forge) dependencies: `[tool.vivar] native-dependencies`
//! solved per platform into `vivar-native.lock` beside `uv.lock`, and linked
//! into `.venv`, which becomes a conda prefix that still looks like a venv
//! (its own Python, `pyvenv.cfg`, activate scripts), so `run`, the PyPI
//! installer and `pack` work on it unchanged. Python packages the native
//! solve brings are taken as given by the PyPI resolver.

use crate::condaindex::{self, Channel};
use crate::condalink::{self, PrefixLayout};
use crate::condasolve::{Channels, Provider, Solver};
use crate::condaspec::{MatchSpec, Record};
use crate::project::{self, Policy, Project};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

pub const LOCKFILE: &str = "vivar-native.lock";

/// The native lock: every solved package, per platform.
#[derive(Debug, Default, Clone)]
pub struct NativeLock {
    pub channels: Vec<String>,
    pub platforms: Vec<String>,
    pub packages: Vec<Record>,
}

impl NativeLock {
    pub fn read(root: &Path) -> Result<Option<NativeLock>> {
        let path = root.join(LOCKFILE);
        if !path.is_file() {
            return Ok(None);
        }
        let doc: DocumentMut = fs::read_to_string(&path)?
            .parse()
            .with_context(|| format!("parsing {}", path.display()))?;
        let strings = |item: Option<&Item>| -> Vec<String> {
            item.and_then(Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut packages = Vec::new();
        for t in doc
            .get("package")
            .and_then(Item::as_array_of_tables)
            .map(|a| a.iter().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            let text = |k: &str| t.get(k).and_then(Item::as_str).map(str::to_owned);
            let num = |k: &str| t.get(k).and_then(Item::as_integer).unwrap_or(0) as u64;
            packages.push(Record {
                name: text("name").unwrap_or_default(),
                version: text("version").unwrap_or_default(),
                build: text("build").unwrap_or_default(),
                build_number: num("build-number"),
                depends: strings(t.get("depends")),
                constrains: strings(t.get("constrains")),
                subdir: text("subdir").unwrap_or_default(),
                filename: text("filename").unwrap_or_default(),
                url: text("url").unwrap_or_default(),
                sha256: text("sha256"),
                md5: text("md5"),
                size: num("size"),
                license: text("license"),
                noarch: text("noarch"),
                timestamp: num("timestamp"),
                track_features: strings(t.get("track-features")),
            });
        }
        Ok(Some(NativeLock {
            channels: strings(doc.get("channels")),
            platforms: strings(doc.get("platforms")),
            packages,
        }))
    }

    pub fn write(&self, root: &Path) -> Result<()> {
        let mut doc = DocumentMut::new();
        doc["version"] = value(1);
        let mut channels = Array::new();
        for c in &self.channels {
            channels.push(c.as_str());
        }
        doc["channels"] = value(channels);
        let mut platforms = Array::new();
        for p in &self.platforms {
            platforms.push(p.as_str());
        }
        doc["platforms"] = value(platforms);
        let mut list = ArrayOfTables::new();
        let mut sorted: Vec<&Record> = self.packages.iter().collect();
        sorted.sort_by(|a, b| {
            (&a.subdir, &a.name, &a.version).cmp(&(&b.subdir, &b.name, &b.version))
        });
        for r in sorted {
            let mut t = Table::new();
            t["name"] = value(r.name.as_str());
            t["version"] = value(r.version.as_str());
            t["build"] = value(r.build.as_str());
            t["build-number"] = value(r.build_number as i64);
            t["subdir"] = value(r.subdir.as_str());
            t["filename"] = value(r.filename.as_str());
            t["url"] = value(r.url.as_str());
            if let Some(h) = &r.sha256 {
                t["sha256"] = value(h.as_str());
            }
            if let Some(h) = &r.md5 {
                t["md5"] = value(h.as_str());
            }
            t["size"] = value(r.size as i64);
            if let Some(l) = &r.license {
                t["license"] = value(l.as_str());
            }
            if let Some(n) = &r.noarch {
                t["noarch"] = value(n.as_str());
            }
            if r.timestamp > 0 {
                t["timestamp"] = value(r.timestamp as i64);
            }
            let arr = |items: &[String]| {
                let mut a = Array::new();
                for i in items {
                    a.push(i.as_str());
                }
                a
            };
            t["depends"] = value(arr(&r.depends));
            if !r.constrains.is_empty() {
                t["constrains"] = value(arr(&r.constrains));
            }
            if !r.track_features.is_empty() {
                t["track-features"] = value(arr(&r.track_features));
            }
            list.push(t);
        }
        doc["package"] = Item::ArrayOfTables(list);
        let path = root.join(LOCKFILE);
        fs::write(&path, doc.to_string()).with_context(|| format!("writing {}", path.display()))
    }

    /// The packages locked for one subdir (noarch ones included).
    pub fn for_subdir(&self, subdir: &str) -> Vec<&Record> {
        self.packages
            .iter()
            .filter(|r| r.subdir == subdir)
            .collect()
    }

    /// The python the lock carries for a subdir, as `3.12`.
    pub fn python_series(&self, subdir: &str) -> Option<String> {
        self.for_subdir(subdir)
            .iter()
            .find(|r| r.name == "python")
            .and_then(|r| {
                let mut it = r.version.split('.');
                Some(format!("{}.{}", it.next()?, it.next()?))
            })
    }
}

/// The project that owns the environment: the workspace root for a member.
fn owner(project: &Project) -> Project {
    match project.workspace.as_deref() {
        Some(_) => Project::load(project.venv_root()).unwrap_or_else(|_| Project {
            root: project.root.clone(),
            doc: Default::default(),
            workspace: None,
        }),
        None => Project {
            root: project.root.clone(),
            doc: project.doc_clone(),
            workspace: None,
        },
    }
}

/// Whether the environment's project declares native dependencies at all
/// (the workspace root's declaration counts for every member).
pub fn declared(project: &Project) -> bool {
    !owner(project).native_dependencies().is_empty()
}

/// Drops the native lock when nothing native is declared any more. Returns
/// whether there was one.
pub fn retire(project: &Project) -> Result<bool> {
    let path = project.venv_root().join(LOCKFILE);
    if path.is_file() {
        fs::remove_file(&path)?;
        return Ok(true);
    }
    Ok(false)
}

/// The subdirs the project's lock covers: `[tool.vivar] platforms`, else
/// this machine.
pub fn subdirs(project: &Project) -> Result<Vec<String>> {
    let platforms = project.platforms();
    if platforms.is_empty() {
        return Ok(vec![condaindex::host_subdir()]);
    }
    platforms
        .iter()
        .map(|p| condaindex::subdir_for(p))
        .collect()
}

/// A channel's subdir and noarch listings, searched as one.
struct Both(Channel, Channel);

impl Provider for Both {
    fn candidates(&self, name: &str) -> Result<Vec<Record>> {
        let mut out = self.0.candidates(name)?;
        out.extend(self.1.candidates(name)?);
        Ok(out)
    }
}

/// The channel URLs the project may use, policy applied.
fn channel_urls(project: &Project, policy: &Policy) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for c in project.channels() {
        policy.check_channel(&c)?;
        out.push(policy.channel_url(&c));
    }
    Ok(out)
}

/// The Python series the environment should run: the pin, else the
/// `requires-python` floor, else 3.12.
fn python_series(project: &Project) -> String {
    let root = project.venv_root();
    if let Ok(pin) = fs::read_to_string(root.join(".python-version")) {
        let mut it = pin.trim().split('.');
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            if a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit()) {
                return format!("{a}.{b}");
            }
        }
    }
    if let Some(rp) = project.requires_python() {
        for clause in rp.split(',') {
            let c = clause.trim();
            if let Some(v) = c.strip_prefix(">=").or_else(|| c.strip_prefix("==")) {
                let mut it = v.trim().split('.');
                if let (Some(a), Some(b)) = (it.next(), it.next()) {
                    return format!("{a}.{b}");
                }
            }
        }
    }
    "3.12".to_owned()
}

/// Solves the native dependencies for every platform and writes the lock.
/// Returns one line per change against the previous lock.
pub fn lock(project: &Project, policy: &Policy, upgrade: bool) -> Result<Vec<String>> {
    let root = project.venv_root().to_path_buf();
    let project = match project.workspace.as_deref() {
        Some(_) => &Project::require(&root)?,
        None => project,
    };
    let specs: Vec<MatchSpec> = project
        .native_dependencies()
        .iter()
        .map(|s| MatchSpec::parse(s))
        .collect::<Result<_>>()?;
    for s in &specs {
        policy.check_deny(&[&s.name])?;
    }
    let urls = channel_urls(project, policy)?;
    let series = python_series(project);
    let mut roots = specs.clone();
    roots.insert(0, MatchSpec::parse(&format!("python {series}.*"))?);
    let previous = NativeLock::read(&root)?.unwrap_or_default();
    let subdirs = subdirs(project)?;
    let mut packages = Vec::new();
    for subdir in &subdirs {
        let pairs: Vec<Both> = urls
            .iter()
            .map(|u| Both(Channel::new(u, subdir), Channel::new(u, "noarch")))
            .collect();
        let providers: Vec<&dyn Provider> = pairs.iter().map(|p| p as &dyn Provider).collect();
        let channels = Channels(providers);
        let prefer: BTreeMap<String, (String, String)> = previous
            .for_subdir(subdir)
            .iter()
            .map(|r| (r.name.clone(), (r.version.clone(), r.build.clone())))
            .collect();
        let solved = Solver::new(&channels, condaindex::virtual_packages(subdir))
            .prefer(prefer, upgrade)
            .solve(&roots)
            .with_context(|| format!("solving native dependencies for {subdir}"))?;
        for r in &solved {
            policy.check_deny(&[&r.name])?;
        }
        if let Some(allow) = policy.allow_licenses.as_deref() {
            let bad: Vec<String> = solved
                .iter()
                .filter(|r| {
                    !policy
                        .license_exceptions
                        .contains(&project::normalize(&r.name))
                })
                .filter(|r| !crate::licenses::allowed(r.license.as_deref().unwrap_or(""), allow))
                .map(|r| {
                    format!(
                        "  {} {}: {}",
                        r.name,
                        r.version,
                        r.license.as_deref().unwrap_or("no license declared")
                    )
                })
                .collect();
            if !bad.is_empty() {
                bail!(
                    "native packages with licenses outside the allowlist:\n{}",
                    bad.join("\n")
                );
            }
        }
        packages.extend(solved.into_iter().map(|mut r| {
            // A noarch package serves this subdir; the lock keys it by it.
            r.subdir = subdir.clone();
            r
        }));
    }
    let lock = NativeLock {
        channels: urls,
        platforms: subdirs.clone(),
        packages,
    };
    let mut changes = Vec::new();
    for subdir in &subdirs {
        let old: BTreeMap<&str, String> = previous
            .for_subdir(subdir)
            .iter()
            .map(|r| (r.name.as_str(), format!("{} {}", r.version, r.build)))
            .collect();
        let new: BTreeMap<&str, String> = lock
            .for_subdir(subdir)
            .iter()
            .map(|r| (r.name.as_str(), format!("{} {}", r.version, r.build)))
            .collect();
        for (name, v) in &new {
            match old.get(name) {
                Some(o) if o == v => {}
                Some(o) => changes.push(format!("{subdir}: {name} {o} -> {v}")),
                None => changes.push(format!("{subdir}: + {name} {v}")),
            }
        }
        for name in old.keys().filter(|n| !new.contains_key(*n)) {
            changes.push(format!("{subdir}: - {name}"));
        }
    }
    lock.write(&root)?;
    Ok(changes)
}

/// Whether the native lock exists and satisfies what is declared for this
/// machine: every spec matched by its locked build, the Python series as
/// pinned.
pub fn lock_is_current(project: &Project) -> bool {
    let project = owner(project);
    let root = project.venv_root();
    let Ok(Some(lock)) = NativeLock::read(root) else {
        return false;
    };
    let subdir = condaindex::host_subdir();
    let locked = lock.for_subdir(&subdir);
    if locked.is_empty()
        || lock.python_series(&subdir).as_deref() != Some(python_series(&project).as_str())
    {
        return false;
    }
    project
        .native_dependencies()
        .iter()
        .filter_map(|s| MatchSpec::parse(s).ok())
        .all(|s| locked.iter().any(|r| s.matches(r)))
}

/// conda names whose PyPI name differs.
const PYPI_NAMES: &[(&str, &str)] = &[
    ("pytorch", "torch"),
    ("pytorch-cpu", "torch"),
    ("pytorch-gpu", "torch"),
    ("msgpack-python", "msgpack"),
    ("opencv", "opencv-python"),
    ("py-opencv", "opencv-python"),
    ("pytables", "tables"),
    ("matplotlib-base", "matplotlib"),
    ("pyqt", "pyqt5"),
    ("python-graphviz", "graphviz"),
    ("ruamel.yaml", "ruamel-yaml"),
    ("python-xxhash", "xxhash"),
    ("tensorflow-base", "tensorflow"),
];

/// The Python packages the native lock provides on this machine, by PyPI
/// name, with their versions: the PyPI resolver takes them as installed.
pub fn provided(project: &Project) -> BTreeMap<String, String> {
    let Ok(Some(lock)) = NativeLock::read(project.venv_root()) else {
        return BTreeMap::new();
    };
    let subdir = condaindex::host_subdir();
    lock.for_subdir(&subdir)
        .iter()
        .filter(|r| {
            r.name != "python" && !r.name.starts_with("python_") && !r.name.starts_with("__")
        })
        .filter(|r| {
            r.noarch.as_deref() == Some("python")
                || r.depends.iter().any(|d| {
                    let n = d.split_whitespace().next().unwrap_or("");
                    n == "python" || n == "python_abi"
                })
        })
        .map(|r| {
            let pypi = PYPI_NAMES
                .iter()
                .find(|(c, _)| *c == r.name)
                .map(|(_, p)| p.to_string())
                .unwrap_or_else(|| r.name.clone());
            (project::normalize(&pypi), r.version.clone())
        })
        .collect()
}

/// Makes `.venv` a prefix holding exactly the native packages locked for
/// this machine. Returns how many were linked and unlinked.
pub fn sync(project: &Project, policy: &Policy) -> Result<(usize, usize)> {
    let root = project.venv_root().to_path_buf();
    let lock =
        NativeLock::read(&root)?.ok_or_else(|| anyhow!("no {LOCKFILE}; run `vivar lock` first"))?;
    let subdir = condaindex::host_subdir();
    let wanted: Vec<&Record> = lock.for_subdir(&subdir);
    if wanted.is_empty() {
        bail!(
            "{LOCKFILE} covers {} but not this machine ({subdir}); add it to `[tool.vivar] platforms` and lock again",
            lock.platforms.join(", ")
        );
    }
    for url in &lock.channels {
        policy.check_channel(url)?;
    }
    // Policy may have changed since the lock was written.
    for r in &wanted {
        policy.check_deny(&[&r.name])?;
    }
    let prefix = root.join(".venv");
    // A plain venv in the way is replaced: the prefix brings its own Python.
    if prefix.join("pyvenv.cfg").is_file() && !prefix.join("conda-meta").is_dir() {
        println!("replacing .venv with a native (conda) prefix");
        fs::remove_dir_all(&prefix)?;
    }
    fs::create_dir_all(&prefix)?;
    let prefix = std::path::absolute(&prefix)?;
    let python = wanted
        .iter()
        .find(|r| r.name == "python")
        .ok_or_else(|| anyhow!("the native lock has no python for {subdir}"))?;
    let python_mm = lock
        .python_series(&subdir)
        .ok_or_else(|| anyhow!("python version in {LOCKFILE} is unreadable"))?;
    let layout = PrefixLayout {
        prefix: prefix.clone(),
        windows: cfg!(windows),
        python_mm: Some(python_mm.clone()),
    };
    let present = condalink::installed(&prefix)?;
    let same = |l: &condalink::Linked, r: &Record| {
        l.name == r.name && l.version == r.version && l.build == r.build
    };
    // Another Python series moves site-packages: everything is relinked.
    let series_changed = present
        .iter()
        .find(|l| l.name == "python")
        .is_some_and(|l| {
            let mut it = l.version.split('.');
            let have = format!("{}.{}", it.next().unwrap_or(""), it.next().unwrap_or(""));
            have != python_mm
        });
    let mut removed = 0;
    for l in &present {
        if series_changed || !wanted.iter().any(|r| same(l, r)) {
            condalink::unlink(&prefix, l)?;
            removed += 1;
        }
    }
    let present = condalink::installed(&prefix)?;
    // Python first: noarch packages are laid out around its site-packages.
    let mut order: Vec<&Record> = vec![python];
    order.extend(wanted.iter().copied().filter(|r| r.name != "python"));
    let mut linked = 0;
    for r in order {
        if present.iter().any(|l| same(l, r)) {
            continue;
        }
        let archive = condalink::fetch(r)?;
        condalink::link(r, &archive, &layout)?;
        linked += 1;
    }
    venv_face(&prefix, &python.version, &project.name())?;
    Ok((linked, removed))
}

/// The venv face of the prefix: `pyvenv.cfg` pointing at its own Python,
/// the Windows launchers in `Scripts/`, activate scripts, a `.gitignore`.
fn venv_face(prefix: &Path, python_version: &str, prompt: &str) -> Result<()> {
    let home = if cfg!(windows) {
        prefix.to_path_buf()
    } else {
        prefix.join("bin")
    };
    fs::write(
        prefix.join("pyvenv.cfg"),
        format!(
            "home = {}\nimplementation = CPython\nversion = {v}\nversion_info = {v}\n\
             include-system-site-packages = false\nprompt = {prompt}\ncreated-by = vivar {}\nnative = true\n",
            home.display(),
            env!("CARGO_PKG_VERSION"),
            v = python_version,
        ),
    )?;
    let bin = prefix.join(crate::venv::BIN);
    fs::create_dir_all(&bin)?;
    if cfg!(windows) {
        let launchers = prefix.join("Lib").join("venv").join("scripts").join("nt");
        for exe in ["python.exe", "pythonw.exe"] {
            let src = launchers.join(exe);
            if src.is_file() {
                fs::copy(&src, bin.join(exe))?;
            }
        }
    }
    crate::venv::write_activate_scripts(
        &bin,
        &prefix.to_string_lossy(),
        prompt,
        crate::venv::BIN,
        cfg!(windows),
    )?;
    let ignore = prefix.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n")?;
    }
    Ok(())
}

/// Whether `.venv` is a native prefix.
pub fn is_prefix(venv: &Path) -> bool {
    venv.join("conda-meta").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_round_trips() {
        let dir = std::env::temp_dir().join(format!("vivar-nlock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut r = condaindex::virtual_record("python", "3.12.7", "h1_0", "linux-64");
        r.filename = "python-3.12.7-h1_0.conda".into();
        r.url = "https://c/conda-forge/linux-64/python-3.12.7-h1_0.conda".into();
        r.sha256 = Some("ab".repeat(32));
        r.depends = vec!["libz >=1.3".into()];
        r.license = Some("Python-2.0".into());
        let mut tz = condaindex::virtual_record("tzdata", "2026e", "h5_0", "linux-64");
        tz.noarch = Some("generic".into());
        tz.filename = "tzdata-2026e-h5_0.conda".into();
        let lock = NativeLock {
            channels: vec!["https://c/conda-forge".into()],
            platforms: vec!["linux-64".into()],
            packages: vec![tz, r],
        };
        lock.write(&dir).unwrap();
        let text = fs::read_to_string(dir.join(LOCKFILE)).unwrap();
        assert!(
            text.contains("[[package]]\nname = \"python\"")
                && text.contains("noarch = \"generic\""),
            "{text}"
        );
        let back = NativeLock::read(&dir).unwrap().unwrap();
        assert_eq!(back.packages.len(), 2);
        assert_eq!(back.packages[0].name, "python");
        assert_eq!(back.packages[0].depends, vec!["libz >=1.3"]);
        assert_eq!(back.python_series("linux-64").as_deref(), Some("3.12"));
        assert!(back.for_subdir("osx-arm64").is_empty());
        assert!(NativeLock::read(&dir.join("nowhere")).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
