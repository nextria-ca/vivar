//! The in-house backend: resolves with `resolve.rs`, installs environments
//! straight from `uv.lock` (wheels only, every artifact hash-checked), and
//! builds through PEP 517, with no uv process. Projects using uv-specific
//! features (workspaces, per-package sources, profiles) are refused rather
//! than resolved differently.

use crate::backend::Backend;
use crate::index::Index;
use crate::lock::{Lock, Package};
use crate::pep::{eval_marker, MarkerEnv, Requirement, Version};
use crate::project::{self, Policy, Project};
use crate::resolve::{write_lock, Resolver, Roots};
use crate::{dirs, install, sha256, upgrade, venv, wheelhouse};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// The PEP 517 driver run inside the build environment: argv is
/// `<mode> <backend> <dist dir> [backend-path...]`; the last stdout line is
/// the result (a JSON list for `requires`, else the built file's name).
const BUILD_HOOK: &str = r#"
import importlib, json, os, sys
mode, backend, dist = sys.argv[1:4]
sys.path[:0] = [os.path.abspath(p) for p in sys.argv[4:]]
module, _, attr = backend.partition(":")
obj = importlib.import_module(module)
for part in filter(None, attr.split(".")):
    obj = getattr(obj, part)
if mode == "requires":
    reqs = []
    for name in ("get_requires_for_build_sdist", "get_requires_for_build_wheel"):
        hook = getattr(obj, name, None)
        if hook is not None:
            reqs.extend(hook(None))
    sys.stdout.flush()
    print(json.dumps(sorted(set(reqs))))
elif mode == "sdist":
    print(obj.build_sdist(dist, None))
else:
    print(obj.build_wheel(dist, None, None))
"#;

pub struct NativeBackend {
    index_url: Option<String>,
    no_build: bool,
}

impl NativeBackend {
    pub fn new(policy: &Policy) -> Self {
        NativeBackend {
            index_url: policy.index_url.clone(),
            no_build: policy.require_wheels,
        }
    }

    fn index(&self) -> Index {
        Index::new(self.index_url.as_deref())
    }

    /// Resolves the project's requirements and writes `uv.lock`. Returns the
    /// previous lock's versions and the new ones, for reporting.
    fn relock(
        &self,
        project: &Project,
        upgrade: &[String],
        upgrade_all: bool,
        dry_run: bool,
    ) -> Result<(BTreeMap<String, String>, BTreeMap<String, String>)> {
        let unsupported = project.uv_only_features();
        if !unsupported.is_empty() {
            bail!(
                "this project uses {}, which vivar's resolver does not handle yet; install uv for it",
                unsupported.join(" and ")
            );
        }
        let root = project.venv_root();
        let (deps, extras, groups) = project.requirement_sections();
        let parse_all = |v: &[String]| -> Result<Vec<Requirement>> {
            v.iter().map(|r| Requirement::parse(r)).collect()
        };
        let roots = Roots {
            name: project.name(),
            version: project.version().unwrap_or_else(|| "0.1.0".to_owned()),
            requires_python: project.requires_python(),
            dependencies: parse_all(&deps)?,
            extras: extras
                .iter()
                .map(|(k, v)| Ok((k.clone(), parse_all(v)?)))
                .collect::<Result<_>>()?,
            groups: groups
                .iter()
                .map(|(k, v)| Ok((k.clone(), parse_all(v)?)))
                .collect::<Result<_>>()?,
            editable: project.is_package(),
        };
        let machine = python_floor(project)?;
        let floor = declared_floor(project).unwrap_or(machine);
        let old: BTreeMap<String, String> = Lock::read(root)
            .map(|l| {
                l.packages
                    .iter()
                    .filter(|p| p.source_kind == "registry")
                    .map(|p| (p.name.clone(), p.version.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let prefer: BTreeMap<String, Version> = old
            .iter()
            .filter_map(|(n, v)| Version::parse(v).ok().map(|v| (n.clone(), v)))
            .collect();
        let index = self.index();
        let resolution = Resolver::new(&index, floor, self.no_build)
            .machine(machine)
            .prefer(prefer)
            .upgrade(upgrade, upgrade_all)
            .resolve(&roots)?;
        let new: BTreeMap<String, String> = resolution
            .packages
            .iter()
            .map(|p| (p.name.clone(), p.version.to_string()))
            .collect();
        if !dry_run {
            let text = write_lock(&roots, &resolution, index.url());
            fs::write(root.join(crate::lock::LOCKFILE), text)?;
            println!(
                "resolved {} package(s) (native resolver, wheels only)",
                resolution.packages.len()
            );
        }
        Ok((old, new))
    }
}

/// The lowest Python the project declares (`requires-python = ">=3.10"`):
/// what a lock must serve, as opposed to what this machine runs.
fn declared_floor(project: &Project) -> Option<[u32; 3]> {
    let rp = project.requires_python()?;
    let v = rp.split(',').find_map(|c| {
        let c = c.trim();
        c.strip_prefix(">=")
            .or_else(|| c.strip_prefix("~="))
            .or_else(|| c.strip_prefix("=="))
    })?;
    let mut it = v
        .trim()
        .trim_end_matches(".*")
        .split('.')
        .map(|p| p.parse::<u32>().ok());
    Some([it.next()??, it.next()??, it.next().flatten().unwrap_or(0)])
}

/// The Python this machine installs with: the env, else the pin, else the
/// `requires-python` floor, else the newest installed.
fn python_floor(project: &Project) -> Result<[u32; 3]> {
    let root = project.venv_root();
    if let Ok(v) = env_version(&root.join(".venv")) {
        return Ok(v);
    }
    if let Ok(pin) = fs::read_to_string(root.join(".python-version")) {
        let mut it = pin.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        return Ok([
            it.next().unwrap_or(3),
            it.next().unwrap_or(12),
            it.next().unwrap_or(0),
        ]);
    }
    if let Some(rp) = project.requires_python() {
        if let Some(v) = rp.split(',').find_map(|c| c.trim().strip_prefix(">=")) {
            let mut it = v.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
            return Ok([
                it.next().unwrap_or(3),
                it.next().unwrap_or(12),
                it.next().unwrap_or(0),
            ]);
        }
    }
    Ok(venv::interpreter(None)?.version)
}

/// The set of lock packages an environment should contain for `env`, given
/// the root packages' groups and extras.
pub fn plan<'a>(lock: &'a Lock, env: &MarkerEnv, groups: &[&str]) -> Result<Vec<&'a Package>> {
    let mut wanted: BTreeMap<(String, String), &Package> = BTreeMap::new();
    let mut queue: VecDeque<(&Package, Vec<String>)> = VecDeque::new();
    for root in lock.roots() {
        queue.push_back((root, Vec::new()));
    }
    let mut seen: BTreeSet<(String, String, Vec<String>)> = BTreeSet::new();
    while let Some((pkg, extras)) = queue.pop_front() {
        if !seen.insert((pkg.name.clone(), pkg.version.clone(), extras.clone())) {
            continue;
        }
        wanted.insert((pkg.name.clone(), pkg.version.clone()), pkg);
        let mut edges: Vec<&crate::lock::Dep> = pkg.base_dependencies.iter().collect();
        for e in &extras {
            if let Some(d) = pkg.extras.get(e) {
                edges.extend(d.iter());
            }
        }
        let is_root = matches!(pkg.source_kind.as_str(), "virtual" | "editable");
        if is_root {
            for g in groups {
                if let Some(d) = pkg.groups.get(*g) {
                    edges.extend(d.iter());
                }
            }
        }
        for dep in edges {
            if let Some(m) = &dep.marker {
                if !eval_marker(m, env)
                    .with_context(|| format!("marker on {} -> {}", pkg.name, dep.name))?
                {
                    continue;
                }
            }
            // Pick the variant: pinned version, else the one whose fork markers match.
            let candidates: Vec<&Package> = lock
                .packages
                .iter()
                .filter(|p| p.name == dep.name)
                .filter(|p| dep.version.as_ref().is_none_or(|v| *v == p.version))
                .collect();
            let chosen = candidates
                .iter()
                .find(|p| {
                    p.resolution_markers.is_empty()
                        || p.resolution_markers
                            .iter()
                            .any(|m| eval_marker(m, env).unwrap_or(false))
                })
                .or(candidates.first())
                .copied()
                .ok_or_else(|| {
                    anyhow!(
                        "{} depends on {}, which is not in uv.lock",
                        pkg.name,
                        dep.name
                    )
                })?;
            queue.push_back((chosen, dep.extras.clone()));
        }
    }
    Ok(wanted.into_values().collect())
}

/// Wheel cache: `~/.cache/vivar/wheels/<sha256>/<file>`.
fn cache_dir() -> Result<PathBuf> {
    Ok(dirs::cache()?.join("wheels"))
}

/// Where wheels come from: the configured index (credentials go to its host
/// only, never to whatever a lock names) and an optional wheelhouse
/// directory that is tried first.
struct Source<'a> {
    index_url: &'a str,
    wheelhouse_dir: Option<&'a Path>,
}

/// Fetches the wheel for `pkg` on `target`, hash-checked, from the cache or
/// the URL the lock records.
fn fetch_wheel(pkg: &Package, target: &wheelhouse::Target, source: &Source) -> Result<PathBuf> {
    let (artifact, is_wheel) = target.pick(pkg).ok_or_else(|| {
        anyhow!(
            "{} {} has no artifact for this platform in uv.lock",
            pkg.name,
            pkg.version
        )
    })?;
    if !is_wheel {
        bail!(
            "{} {} is only available as a source distribution; vivar's native installer handles wheels (uv can build it, or set require-wheels and pick a version with wheels)",
            pkg.name,
            pkg.version
        );
    }
    let name = artifact.filename();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        bail!("refusing artifact with unsafe file name {name:?}");
    }
    let expected = artifact.sha256().ok_or_else(|| {
        anyhow!(
            "{} has no hash in uv.lock; refusing to install unverified code",
            name
        )
    })?;
    let dest = cache_dir()?.join(&expected[..16]).join(&name);
    if dest.is_file() && sha256::file(&dest)? == expected {
        return Ok(dest);
    }
    fs::create_dir_all(dest.parent().context("cache path")?)?;
    if let Some(dir) = source.wheelhouse_dir {
        let local = dir.join(&name);
        if local.is_file() && sha256::file(&local)? == expected {
            fs::copy(&local, &dest)?;
            return Ok(dest);
        }
    }
    println!("downloading {name}");
    // Index credentials go to the configured index's own host only: not to a
    // CDN, and not to whatever registry a lock happens to name.
    let url = if crate::index::same_origin(&artifact.url, source.index_url) {
        crate::backend::with_credentials(&artifact.url)
    } else {
        artifact.url.clone()
    };
    upgrade::fetch_to(&url, &dest)?;
    let actual = sha256::file(&dest)?;
    if actual != expected {
        let _ = fs::remove_file(&dest);
        bail!("{name} does not match the hash in uv.lock (got {actual}, expected {expected})");
    }
    Ok(dest)
}

/// Installs `wanted` into `layout`, removing anything else that is there.
/// Editable members of the project are installed as `.pth` entries.
fn apply(
    lock_root: &Path,
    wanted: &[&Package],
    layout: &install::Layout,
    target: &wheelhouse::Target,
    source: &Source,
) -> Result<(usize, usize)> {
    let present = install::installed(&layout.site)?;
    let want_names: BTreeSet<&str> = wanted.iter().map(|p| p.name.as_str()).collect();
    let mut removed = 0;
    for dist in &present {
        let keep = wanted
            .iter()
            .any(|p| p.name == dist.name && p.version == dist.version);
        if !keep && (want_names.contains(dist.name.as_str()) || !dist.name.is_empty()) {
            install::uninstall(dist)?;
            removed += 1;
        }
    }
    let present = install::installed(&layout.site)?;
    let mut installed = 0;
    for pkg in wanted {
        if present
            .iter()
            .any(|d| d.name == pkg.name && d.version == pkg.version)
        {
            continue;
        }
        match pkg.source_kind.as_str() {
            "registry" => {
                let wheel = fetch_wheel(pkg, target, source)?;
                install::install_wheel(&wheel, layout)?;
                installed += 1;
            }
            "virtual" => {}
            "editable" | "directory" => {
                install_editable(lock_root, pkg, layout)?;
                installed += 1;
            }
            other => bail!("{} comes from a {other} source, which the native installer does not handle yet (use uv for it)", pkg.name),
        }
    }
    Ok((installed, removed))
}

/// An editable install: a `.pth` pointing at the package's source directory
/// and a minimal dist-info so tools (and `vivar licenses`) see it.
fn install_editable(lock_root: &Path, pkg: &Package, layout: &install::Layout) -> Result<()> {
    let project_dir = lock_root.join(&pkg.source);
    let src = if project_dir.join("src").is_dir() {
        project_dir.join("src")
    } else {
        project_dir.clone()
    };
    fs::create_dir_all(&layout.site)?;
    fs::write(
        layout
            .site
            .join(format!("_{}.pth", pkg.name.replace('-', "_"))),
        format!("{}\n", src.display()),
    )?;
    let info = layout.site.join(format!(
        "{}-{}.dist-info",
        pkg.name.replace('-', "_"),
        pkg.version
    ));
    fs::create_dir_all(&info)?;
    fs::write(
        info.join("METADATA"),
        format!(
            "Metadata-Version: 2.1\nName: {}\nVersion: {}\n",
            pkg.name, pkg.version
        ),
    )?;
    fs::write(info.join("INSTALLER"), "vivar\n")?;
    fs::write(info.join("RECORD"), format!("{0}-{1}.dist-info/METADATA,,\n{0}-{1}.dist-info/INSTALLER,,\n{0}-{1}.dist-info/RECORD,,\n_{0}.pth,,\n", pkg.name.replace('-', "_"), pkg.version))?;
    Ok(())
}

/// The environment's Python version from pyvenv.cfg.
fn env_version(venv_dir: &Path) -> Result<[u32; 3]> {
    let cfg = fs::read_to_string(venv_dir.join("pyvenv.cfg"))?;
    let v = cfg
        .lines()
        .find_map(|l| {
            l.strip_prefix("version_info")
                .map(|r| r.trim_start_matches([' ', '=']).trim().to_owned())
        })
        .ok_or_else(|| anyhow!("pyvenv.cfg has no version_info"))?;
    let mut it = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    Ok([
        it.next().unwrap_or(3),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    ])
}

impl NativeBackend {
    fn wheelhouse_dir(&self) -> Option<PathBuf> {
        self.index_url
            .as_deref()
            .map(Path::new)
            .filter(|p| p.is_dir())
            .map(Path::to_path_buf)
    }

    /// Runs `f` with this backend's artifact source.
    fn with_source<T>(&self, f: impl FnOnce(&Source) -> Result<T>) -> Result<T> {
        let index = self.index();
        let wheelhouse = self.wheelhouse_dir();
        f(&Source {
            index_url: index.url(),
            wheelhouse_dir: wheelhouse.as_deref(),
        })
    }

    fn sync_into(&self, lock_root: &Path, venv_dir: &Path, groups: &[&str]) -> Result<()> {
        let lock = Lock::read(lock_root)?;
        let version = env_version(venv_dir)?;
        let env = MarkerEnv::current(version);
        let wanted = plan(&lock, &env, groups)?;
        let target = wheelhouse::Target::new(None, &format!("{}.{}", version[0], version[1]))?;
        let layout = install::Layout::venv(
            venv_dir,
            &format!("{}.{}", version[0], version[1]),
            cfg!(windows),
        );
        let (installed, removed) =
            self.with_source(|source| apply(lock_root, &wanted, &layout, &target, source))?;
        println!(
            "native sync: {} package(s) installed, {} removed, {} in the environment",
            installed,
            removed,
            wanted.len().saturating_sub(1)
        );
        Ok(())
    }
}

impl Backend for NativeBackend {
    fn add(&self, dir: &Path, packages: &[String], group: Option<&str>) -> Result<()> {
        let project = Project::require(dir)?;
        for p in packages {
            Requirement::parse(p)?; // fail before touching the file
        }
        for p in packages {
            project::add_dependency(&project.root, p, group)?;
        }
        let project = Project::require(dir)?;
        self.relock(&project, &[], false, false)?;
        let groups: Vec<&str> = group.into_iter().collect();
        self.sync_groups(dir, &groups)
    }

    fn remove(&self, dir: &Path, packages: &[String]) -> Result<()> {
        let project = Project::require(dir)?;
        // Every name is checked before the manifest changes.
        let declared: BTreeSet<String> = project
            .declared_requirements()
            .iter()
            .map(|r| project::normalize(project::requirement_name(r)))
            .collect();
        if let Some(p) = packages
            .iter()
            .find(|p| !declared.contains(&project::normalize(p)))
        {
            bail!("{p} is not a declared dependency");
        }
        for p in packages {
            project::remove_dependency(&project.root, p)?;
        }
        let project = Project::require(dir)?;
        self.relock(&project, &[], false, false)?;
        self.sync(dir)
    }

    fn lock(&self, dir: &Path) -> Result<()> {
        let project = Project::require(dir)?;
        self.relock(&project, &[], false, false)?;
        Ok(())
    }

    fn sync(&self, dir: &Path) -> Result<()> {
        self.sync_groups(dir, &["dev"])
    }

    fn sync_groups(&self, dir: &Path, groups: &[&str]) -> Result<()> {
        let project = crate::project::Project::require(dir)?;
        let root = project.venv_root().to_path_buf();
        let mut all: Vec<&str> = vec!["dev"];
        all.extend(groups.iter().copied().filter(|g| *g != "dev"));
        self.sync_into(&root, &root.join(".venv"), &all)
    }

    fn upgrade(&self, dir: &Path, packages: &[String], dry_run: bool) -> Result<()> {
        let project = Project::require(dir)?;
        let (old, new) = self.relock(&project, packages, packages.is_empty(), dry_run)?;
        let mut changes = 0;
        for (name, version) in &new {
            match old.get(name) {
                Some(o) if o != version => {
                    changes += 1;
                    println!(
                        "{} {name} v{o} -> v{version}",
                        if dry_run { "Would update" } else { "Updated" }
                    );
                }
                None => {
                    changes += 1;
                    println!(
                        "{} {name} v{version}",
                        if dry_run { "Would add" } else { "Added" }
                    );
                }
                _ => {}
            }
        }
        for name in old.keys().filter(|n| !new.contains_key(*n)) {
            changes += 1;
            println!(
                "{} {name}",
                if dry_run { "Would remove" } else { "Removed" }
            );
        }
        if changes == 0 {
            println!("No lockfile changes detected");
        }
        Ok(())
    }

    /// PEP 517 by hand: a throwaway environment with `build-system.requires`
    /// (plus whatever the backend asks for), then `build_sdist` and
    /// `build_wheel` into `dist/`.
    fn build(&self, dir: &Path) -> Result<()> {
        let project = Project::require(dir)?;
        let root = project.root.clone();
        let doc: toml_edit::DocumentMut = fs::read_to_string(root.join(project::PYPROJECT))?
            .parse()
            .context("parsing pyproject.toml")?;
        let table = doc.get("build-system").and_then(|i| i.as_table_like());
        let strings = |key: &str| -> Vec<String> {
            table
                .and_then(|t| t.get(key))
                .and_then(|i| i.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        // No [build-system]: PEP 517's legacy fallback.
        let (requires, backend) = match table {
            Some(t) => (
                strings("requires"),
                t.get("build-backend")
                    .and_then(|i| i.as_str())
                    .unwrap_or("setuptools.build_meta:__legacy__")
                    .to_owned(),
            ),
            None => (
                vec!["setuptools>=40.8.0".to_owned()],
                "setuptools.build_meta:__legacy__".to_owned(),
            ),
        };
        let backend_path = strings("backend-path");
        let canonical_root = fs::canonicalize(&root)?;
        for p in &backend_path {
            let full = fs::canonicalize(root.join(p))
                .with_context(|| format!("build-system.backend-path entry {p:?}"))?;
            if Path::new(p).is_absolute() || !full.starts_with(&canonical_root) {
                bail!("build-system.backend-path must stay inside the project ({p:?} does not)");
            }
        }

        let version = python_floor(&project)?;
        let series = format!("{}.{}", version[0], version[1]);
        let python = venv::interpreter(Some(&series))?;
        let build_env = std::env::temp_dir().join(format!("vivar-build-{}", std::process::id()));
        let _ = fs::remove_dir_all(&build_env);
        venv::create(&build_env, &python, "build")?;
        let result = (|| -> Result<()> {
            let py = venv::python(&build_env);
            if !requires.is_empty() {
                self.install(&py, &requires)?;
            }
            let dist = root.join("dist");
            fs::create_dir_all(&dist)?;
            // Build tools the backend shells out to (ninja, meson, cython)
            // live in the build env's bin: it goes first on PATH.
            let mut dirs = vec![build_env.join(venv::BIN)];
            if let Some(p) = std::env::var_os("PATH") {
                dirs.extend(std::env::split_paths(&p));
            }
            let path = std::env::join_paths(dirs).context("PATH")?;
            let hook = |mode: &str| -> Result<String> {
                let mut cmd = std::process::Command::new(&py);
                cmd.current_dir(&root)
                    .env("PATH", &path)
                    .args(["-c", BUILD_HOOK, mode, &backend])
                    .arg(&dist)
                    .args(&backend_path);
                let out = cmd
                    .output()
                    .with_context(|| format!("running {}", py.display()))?;
                if !out.status.success() {
                    bail!(
                        "the build backend `{backend}` failed during {mode}:\n{}",
                        String::from_utf8_lossy(&out.stderr).trim_end()
                    );
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                Ok(stdout.lines().last().unwrap_or_default().trim().to_owned())
            };
            // Dynamic requirements (setuptools asks for wheel, for instance),
            // resolved together with the static ones so pins hold.
            let extra = hook("requires")?;
            let extra: Vec<String> = crate::json::parse(&extra)
                .context("the build backend's dynamic requirements are not a JSON list")?
                .as_array()
                .unwrap_or(&[])
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if !extra.is_empty() {
                let mut all = requires.clone();
                all.extend(extra);
                self.install(&py, &all)?;
            }
            for mode in ["sdist", "wheel"] {
                let name = hook(mode)?;
                if name.is_empty() {
                    bail!("the build backend `{backend}` returned no {mode} name");
                }
                println!("built dist/{name}");
            }
            Ok(())
        })();
        let _ = fs::remove_dir_all(&build_env);
        result
    }

    /// Installs `specs` into the environment whose interpreter is `python`:
    /// resolves them as a throwaway project and installs the result.
    fn install(&self, python: &Path, specs: &[String]) -> Result<()> {
        let venv_dir = python
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("{} is not inside an environment", python.display()))?
            .to_path_buf();
        let version = env_version(&venv_dir)?;
        let roots = Roots {
            name: "vivar-tool".to_owned(),
            version: "0".to_owned(),
            requires_python: None,
            dependencies: specs
                .iter()
                .map(|s| Requirement::parse(s))
                .collect::<Result<_>>()?,
            ..Default::default()
        };
        let index = self.index();
        let resolution = Resolver::new(&index, version, self.no_build).resolve(&roots)?;
        let lock_text = write_lock(&roots, &resolution, index.url());
        let lock = Lock::parse(&lock_text)?;
        let env = MarkerEnv::current(version);
        let wanted = plan(&lock, &env, &[])?;
        let series = format!("{}.{}", version[0], version[1]);
        let target = wheelhouse::Target::new(None, &series)?;
        let layout = install::Layout::venv(&venv_dir, &series, cfg!(windows));
        // Additive: a tool env only ever gains what it asked for.
        let present = install::installed(&layout.site)?;
        let mut installed = 0;
        for pkg in wanted.iter().filter(|p| p.source_kind == "registry") {
            if present
                .iter()
                .any(|d| d.name == pkg.name && d.version == pkg.version)
            {
                continue;
            }
            if let Some(old) = present.iter().find(|d| d.name == pkg.name) {
                install::uninstall(old)?;
            }
            let wheel = self.with_source(|source| fetch_wheel(pkg, &target, source))?;
            install::install_wheel(&wheel, &layout)?;
            installed += 1;
        }
        println!("native install: {installed} package(s)");
        Ok(())
    }

    fn add_requirements(&self, dir: &Path, file: &Path, group: Option<&str>) -> Result<()> {
        let text = fs::read_to_string(file)?;
        let reqs: Vec<String> = text
            .lines()
            .map(|l| l.split(" #").next().unwrap_or(l).trim().to_owned())
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-'))
            .collect();
        self.add(dir, &reqs, group)
    }

    fn lock_check(&self, dir: &Path) -> Result<()> {
        // Structural: the lock exists and covers every declared requirement
        // that applies on this machine (one that applies nowhere is rightly
        // absent; one that applies elsewhere is present).
        let project = crate::project::Project::require(dir)?;
        let lock = Lock::read(project.venv_root())?;
        let locked: BTreeSet<String> = lock.packages.iter().map(|p| p.name.clone()).collect();
        let here = MarkerEnv::current(python_floor(&project)?);
        let mut missing = Vec::new();
        for req in project.declared_requirements() {
            let name = crate::project::normalize(crate::project::requirement_name(&req));
            let applies_here = Requirement::parse(&req)
                .ok()
                .and_then(|r| r.marker.map(|m| eval_marker(&m, &here).unwrap_or(true)))
                .unwrap_or(true);
            if !locked.contains(&name) && applies_here {
                missing.push(name);
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            bail!(
                "uv.lock does not cover declared dependencies: {} (run `vivar lock`)",
                missing.join(", ")
            )
        }
    }

    fn export_requirements(&self, dir: &Path, output: &Path) -> Result<()> {
        let project = crate::project::Project::require(dir)?;
        let lock = Lock::read(project.venv_root())?;
        let version = env_version(&project.venv_root().join(".venv")).unwrap_or([3, 12, 0]);
        let env = MarkerEnv::current(version);
        let mut text = String::from("# generated by vivar export requirements (native)\n");
        for pkg in plan(&lock, &env, &[])? {
            if pkg.source_kind != "registry" {
                continue;
            }
            text.push_str(&format!("{}=={}", pkg.name, pkg.version));
            let mut hashes: Vec<String> = pkg
                .wheels
                .iter()
                .chain(pkg.sdist.iter())
                .filter_map(|a| a.sha256())
                .map(|h| format!(" \\\n    --hash=sha256:{h}"))
                .collect();
            hashes.sort();
            hashes.dedup();
            text.push_str(&hashes.join(""));
            text.push('\n');
        }
        fs::write(output, text).with_context(|| format!("writing {}", output.display()))
    }

    fn install_for_target(
        &self,
        dir: &Path,
        triple: &str,
        python: &str,
        group: Option<&str>,
        target: &Path,
    ) -> Result<()> {
        let project = crate::project::Project::require(dir)?;
        let lock = Lock::read(project.venv_root())?;
        let (os, arch) = triple_parts(triple)?;
        let mut it = python.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let version = [
            it.next().unwrap_or(3),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        ];
        let env = MarkerEnv::for_target(os, arch, version);
        let groups: Vec<&str> = group.into_iter().collect();
        let wanted = plan(&lock, &env, &groups)?;
        let series = format!("{}.{}", version[0], version[1]);
        let t = wheelhouse::Target::new(Some(&format!("{os}-{arch}")), &series)?;
        // `target` is a site-packages directory; scripts go beside it in bin/, as uv's layout does.
        let layout = install::Layout {
            site: target.to_path_buf(),
            bin: target.join("bin"),
            headers: target.join("include"),
            prefix: target.to_path_buf(),
            python: "python".to_owned(),
            windows: os == "windows",
        };
        let registry: Vec<&Package> = wanted
            .iter()
            .copied()
            .filter(|p| p.source_kind == "registry")
            .collect();
        let (installed, _) =
            self.with_source(|source| apply(project.venv_root(), &registry, &layout, &t, source))?;
        println!("native layout: {installed} package(s) for {triple}");
        Ok(())
    }
}

fn triple_parts(triple: &str) -> Result<(&'static str, &'static str)> {
    let arch = if triple.starts_with("aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    let os = if triple.contains("linux") {
        "linux"
    } else if triple.contains("darwin") {
        "macos"
    } else if triple.contains("windows") {
        "windows"
    } else {
        bail!("unknown target triple {triple}");
    };
    Ok((os, arch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_follow_markers_groups_and_forks() {
        let lock = Lock::parse(
            r#"
version = 1
[[package]]
name = "app"
version = "0.1.0"
source = { virtual = "." }
dependencies = [
    { name = "numpy", version = "1.26.4", marker = "python_full_version < '3.12'" },
    { name = "numpy", version = "2.1.0", marker = "python_full_version >= '3.12'" },
    { name = "colorama", marker = "sys_platform == 'win32'" },
]

[package.dev-dependencies]
dev = [{ name = "pytest" }]

[[package]]
name = "numpy"
version = "1.26.4"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "numpy"
version = "2.1.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "colorama"
version = "0.4.6"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "pytest"
version = "9.0.0"
source = { registry = "https://pypi.org/simple" }
dependencies = [{ name = "colorama", marker = "sys_platform == 'win32'" }]
"#,
        )
        .unwrap();
        let linux = MarkerEnv::for_target("linux", "x86_64", [3, 12, 4]);
        let names = |env: &MarkerEnv, groups: &[&str]| -> Vec<String> {
            plan(&lock, env, groups)
                .unwrap()
                .iter()
                .map(|p| format!("{} {}", p.name, p.version))
                .collect()
        };
        assert_eq!(names(&linux, &[]), vec!["app 0.1.0", "numpy 2.1.0"]);
        assert_eq!(
            names(&linux, &["dev"]),
            vec!["app 0.1.0", "numpy 2.1.0", "pytest 9.0.0"]
        );
        let win_old = MarkerEnv::for_target("windows", "x86_64", [3, 11, 0]);
        assert_eq!(
            names(&win_old, &["dev"]),
            vec![
                "app 0.1.0",
                "colorama 0.4.6",
                "numpy 1.26.4",
                "pytest 9.0.0"
            ]
        );
        assert_eq!(
            triple_parts("aarch64-apple-darwin").unwrap(),
            ("macos", "aarch64")
        );
        assert!(triple_parts("wasm32-unknown-unknown").is_err());
    }
}
