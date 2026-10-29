//! Dependency resolution without uv: a backtracking resolver over the simple
//! index, newest acceptable versions first, writing a uv.lock-compatible
//! lockfile. Markers are kept on edges; a dependency is included when its
//! marker can hold on any of the platforms vivar targets, so the lock serves
//! more than the machine it was made on.

use crate::index::{File, Index, Metadata};
use crate::pep::{eval_marker, MarkerEnv, Requirement, Specifier, Version};
use crate::wheelhouse::Target;
use anyhow::{anyhow, bail, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// What to resolve: the project's own requirements by section.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    pub name: String,
    pub version: String,
    pub requires_python: Option<String>,
    pub dependencies: Vec<Requirement>,
    pub extras: BTreeMap<String, Vec<Requirement>>,
    pub groups: BTreeMap<String, Vec<Requirement>>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub files: Vec<File>,
    /// Dependencies with the markers they carry, as they go into the lock.
    pub dependencies: Vec<Requirement>,
}

pub struct Resolution {
    pub packages: Vec<ResolvedPackage>,
}

pub struct Resolver<'a> {
    index: &'a Index,
    /// Platforms a marker may hold on.
    envs: Vec<MarkerEnv>,
    /// This machine: a version must have an artifact installable here.
    here: Target,
    wheels_only: bool,
    /// Versions to keep when still acceptable (the current lock).
    prefer: BTreeMap<String, Version>,
    /// Names whose preference is ignored (`upgrade-deps x`), or all when `upgrade_all`.
    upgrade: BTreeSet<String>,
    upgrade_all: bool,
    python_floor: [u32; 3],
    cache: BTreeMap<(String, String), Metadata>,
}

#[derive(Clone, Default)]
struct State {
    assigned: Vec<(String, Version)>,
    constraints: BTreeMap<String, Vec<Specifier>>,
    extras: BTreeMap<String, BTreeSet<String>>,
    pending: VecDeque<Requirement>,
    /// Edges (from, requirement) recorded for the lock.
    edges: Vec<(String, Requirement)>,
}

struct Frame {
    name: String,
    candidates: Vec<Version>,
    next: usize,
    before: State,
}

impl<'a> Resolver<'a> {
    pub fn new(index: &'a Index, python_floor: [u32; 3], wheels_only: bool) -> Resolver<'a> {
        let [maj, min, _] = python_floor;
        let mut envs = Vec::new();
        for os in ["linux", "macos", "windows"] {
            for arch in ["x86_64", "aarch64"] {
                for minor in min..=(min + 2).max(13) {
                    envs.push(MarkerEnv::for_target(os, arch, [maj, minor, 0]));
                }
            }
        }
        let series = format!("{maj}.{min}");
        Resolver {
            index,
            envs,
            here: Target::new(None, &series).expect("current platform is supported"),
            wheels_only,
            prefer: BTreeMap::new(),
            upgrade: BTreeSet::new(),
            upgrade_all: false,
            python_floor,
            cache: BTreeMap::new(),
        }
    }

    pub fn prefer(mut self, versions: BTreeMap<String, Version>) -> Self {
        self.prefer = versions;
        self
    }

    pub fn upgrade(mut self, names: &[String], all: bool) -> Self {
        self.upgrade = names.iter().map(|n| crate::project::normalize(n)).collect();
        self.upgrade_all = all;
        self
    }

    /// Whether a marker can hold on at least one targeted platform (with the
    /// extras requested of its package).
    fn satisfiable(&self, marker: Option<&str>, extras: &BTreeSet<String>) -> bool {
        let Some(m) = marker else { return true };
        self.envs.iter().any(|env| {
            let mut e = env.clone();
            e.extra = extras.iter().cloned().collect();
            eval_marker(m, &e).unwrap_or(false)
        })
    }

    /// Candidate versions of `name`, newest first, acceptable under `constraints`.
    fn candidates(&self, name: &str, constraints: &[Specifier]) -> Result<Vec<Version>> {
        let files = self.index.files(name)?;
        let mut by_version: BTreeMap<Version, Vec<&File>> = BTreeMap::new();
        for f in &files {
            if let Some(v) = f.version(name) {
                by_version.entry(v).or_default().push(f);
            }
        }
        let floor = Version::parse(&format!(
            "{}.{}.{}",
            self.python_floor[0], self.python_floor[1], self.python_floor[2]
        ))?;
        let exact_pin = constraints.iter().any(|c| c.to_string().starts_with("=="));
        let mut out: Vec<Version> = by_version
            .into_iter()
            .rev()
            .filter(|(v, fs)| {
                constraints
                    .iter()
                    .all(|c| c.matches(v) || (exact_pin && c.matches_allowing_pre(v)))
                    && fs.iter().any(|f| {
                        (!f.yanked || exact_pin)
                            && f.requires_python
                                .as_deref()
                                .map(|rp| {
                                    Specifier::parse(rp)
                                        .map(|s| s.matches_allowing_pre(&floor))
                                        .unwrap_or(true)
                                })
                                .unwrap_or(true)
                    })
            })
            .filter(|(v, fs)| self.installable(fs, v))
            .map(|(v, _)| v)
            .collect();
        // Stability: the locked version first, unless an upgrade was asked for.
        if !self.upgrade_all && !self.upgrade.contains(name) {
            if let Some(p) = self.prefer.get(name) {
                if let Some(i) = out.iter().position(|v| v == p) {
                    let v = out.remove(i);
                    out.insert(0, v);
                }
            }
        }
        Ok(out)
    }

    /// At least one artifact for this machine: a matching wheel, or an sdist
    /// when source builds are allowed.
    fn installable(&self, files: &[&File], _v: &Version) -> bool {
        files.iter().any(|f| {
            if f.yanked {
                return false;
            }
            if f.is_wheel() {
                self.here.score(&f.filename).is_some()
            } else {
                !self.wheels_only
                    && (f.filename.ends_with(".tar.gz") || f.filename.ends_with(".zip"))
            }
        })
    }

    /// The wheel whose metadata describes `version` (one for this machine if
    /// possible; metadata rarely differs between wheels of one release).
    fn metadata(&mut self, name: &str, version: &Version) -> Result<Metadata> {
        let key = (name.to_owned(), version.to_string());
        if let Some(m) = self.cache.get(&key) {
            return Ok(m.clone());
        }
        let files = self.index.files(name)?;
        let wheels: Vec<&File> = files
            .iter()
            .filter(|f| f.is_wheel() && f.version(name).as_ref() == Some(version))
            .collect();
        let file = wheels
            .iter()
            .find(|f| self.here.score(&f.filename).is_some())
            .or_else(|| wheels.iter().find(|f| f.has_metadata))
            .or(wheels.first())
            .copied()
            .ok_or_else(|| anyhow!("{name} {version} has no wheel to read dependencies from; vivar's resolver needs wheels (uv can build from source)"))?;
        let m = self.index.metadata(file)?;
        self.cache.insert(key, m.clone());
        Ok(m)
    }

    pub fn resolve(mut self, roots: &Roots) -> Result<Resolution> {
        let mut state = State::default();
        let root_name = crate::project::normalize(&roots.name);
        for r in roots
            .dependencies
            .iter()
            .chain(roots.extras.values().flatten())
            .chain(roots.groups.values().flatten())
        {
            state.edges.push((root_name.clone(), r.clone()));
            state.pending.push_back(r.clone());
        }
        let mut frames: Vec<Frame> = Vec::new();
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 20_000 {
                bail!("resolution did not converge after {steps} steps; the constraints may be contradictory");
            }
            let Some(req) = state.pending.pop_front() else {
                break; // everything satisfied
            };
            if req.url.is_some() {
                bail!(
                    "{}: direct URL requirements are not supported by the native resolver",
                    req.name
                );
            }
            let pkg_extras = state.extras.get(&req.name).cloned().unwrap_or_default();
            let _ = pkg_extras;
            // Record the constraint and extras.
            state
                .constraints
                .entry(req.name.clone())
                .or_default()
                .push(req.specifier.clone());
            let new_extras: Vec<String> = req
                .extras
                .iter()
                .filter(|e| !state.extras.get(&req.name).is_some_and(|s| s.contains(*e)))
                .cloned()
                .collect();
            state
                .extras
                .entry(req.name.clone())
                .or_default()
                .extend(req.extras.iter().cloned());

            if let Some((_, v)) = state.assigned.iter().find(|(n, _)| *n == req.name) {
                let v = v.clone();
                if req.specifier.matches(&v)
                    || req.specifier.matches_allowing_pre(&v) && v.is_prerelease()
                {
                    if !new_extras.is_empty() {
                        self.enqueue_deps(&mut state, &req.name, &v)?;
                    }
                    continue;
                }
                // Conflict with an earlier choice: backtrack.
                state = self.backtrack(&mut frames, &req.name)?;
                continue;
            }
            let constraints = state
                .constraints
                .get(&req.name)
                .cloned()
                .unwrap_or_default();
            let candidates = self.candidates(&req.name, &constraints)?;
            if candidates.is_empty() {
                if frames.is_empty() {
                    bail!(
                        "no version of {} satisfies {} (wheels-only: {}) on {}",
                        req.name,
                        constraints
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        self.wheels_only,
                        self.index.url()
                    );
                }
                state = self.backtrack(&mut frames, &req.name)?;
                continue;
            }
            let version = candidates[0].clone();
            frames.push(Frame {
                name: req.name.clone(),
                candidates,
                next: 1,
                before: state.clone(),
            });
            state.assigned.push((req.name.clone(), version.clone()));
            self.enqueue_deps(&mut state, &req.name, &version)?;
        }

        // Build the result from the final state.
        let mut packages = Vec::new();
        for (name, version) in &state.assigned {
            let files: Vec<File> = self
                .index
                .files(name)?
                .into_iter()
                .filter(|f| f.version(name).as_ref() == Some(version) && !f.yanked)
                .collect();
            let dependencies: Vec<Requirement> = state
                .edges
                .iter()
                .filter(|(from, _)| from == name)
                .map(|(_, r)| r.clone())
                .collect();
            packages.push(ResolvedPackage {
                name: name.clone(),
                version: version.clone(),
                files,
                dependencies,
            });
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Resolution { packages })
    }

    fn enqueue_deps(&mut self, state: &mut State, name: &str, version: &Version) -> Result<()> {
        let meta = self.metadata(name, version)?;
        let extras = state.extras.get(name).cloned().unwrap_or_default();
        for dep in &meta.requires_dist {
            if !self.satisfiable(dep.marker.as_deref(), &extras) {
                continue;
            }
            // An `extra == "x"` edge only applies when x was asked for; the lock
            // records it under that extra. Plain edges go to `dependencies`.
            state.edges.push((name.to_owned(), dep.clone()));
            state.pending.push_back(dep.clone());
        }
        Ok(())
    }

    /// Undo back to the newest frame that still has an untried candidate for
    /// `culprit` (or any frame when none names it), and pick its next version.
    fn backtrack(&mut self, frames: &mut Vec<Frame>, culprit: &str) -> Result<State> {
        let pos = frames
            .iter()
            .rposition(|f| f.name == culprit && f.next < f.candidates.len())
            .or_else(|| frames.iter().rposition(|f| f.next < f.candidates.len()))
            .ok_or_else(|| anyhow!("dependency conflict involving {culprit}: no combination of versions satisfies every constraint"))?;
        frames.truncate(pos + 1);
        let frame = frames.last_mut().unwrap();
        let version = frame.candidates[frame.next].clone();
        frame.next += 1;
        let mut state = frame.before.clone();
        let name = frame.name.clone();
        state.assigned.push((name.clone(), version.clone()));
        self.enqueue_deps(&mut state, &name, &version)?;
        Ok(state)
    }
}

/// Serialises a resolution as a uv.lock document (version 1).
pub fn write_lock(roots: &Roots, resolution: &Resolution, registry: &str) -> String {
    use toml_edit::{value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};
    let mut doc = DocumentMut::new();
    doc["version"] = value(1);
    doc["revision"] = value(3);
    if let Some(rp) = &roots.requires_python {
        doc["requires-python"] = value(rp.as_str());
    }
    let dep_entry = |r: &Requirement| {
        let mut t = InlineTable::new();
        t.insert("name", r.name.as_str().into());
        if !r.extras.is_empty() {
            let mut a = Array::new();
            for e in &r.extras {
                a.push(e.as_str());
            }
            t.insert("extra", a.into());
        }
        if let Some(m) = &r.marker {
            t.insert("marker", m.as_str().into());
        }
        t
    };
    let dep_array = |reqs: &[Requirement]| {
        let mut a = Array::new();
        for r in reqs {
            a.push(dep_entry(r));
        }
        a
    };
    let spec_entry = |r: &Requirement| {
        let mut t = InlineTable::new();
        t.insert("name", r.name.as_str().into());
        if !r.extras.is_empty() {
            let mut a = Array::new();
            for e in &r.extras {
                a.push(e.as_str());
            }
            t.insert("extras", a.into());
        }
        let spec = r.specifier.to_string();
        if !spec.is_empty() {
            t.insert("specifier", spec.into());
        }
        if let Some(m) = &r.marker {
            t.insert("marker", m.as_str().into());
        }
        t
    };

    let mut packages = ArrayOfTables::new();
    // The project itself.
    let mut root = Table::new();
    root["name"] = value(crate::project::normalize(&roots.name));
    root["version"] = value(roots.version.as_str());
    let mut src = InlineTable::new();
    src.insert("virtual", ".".into());
    root["source"] = value(src);
    if !roots.dependencies.is_empty() {
        root["dependencies"] = value(dep_array(&roots.dependencies));
    }
    if !roots.extras.is_empty() {
        let mut t = Table::new();
        for (k, v) in &roots.extras {
            t[k.as_str()] = value(dep_array(v));
        }
        root["optional-dependencies"] = Item::Table(t);
    }
    if !roots.groups.is_empty() {
        let mut t = Table::new();
        for (k, v) in &roots.groups {
            t[k.as_str()] = value(dep_array(v));
        }
        root["dev-dependencies"] = Item::Table(t);
    }
    let mut meta = Table::new();
    let mut rd = Array::new();
    for r in roots
        .dependencies
        .iter()
        .chain(roots.extras.values().flatten())
    {
        rd.push(spec_entry(r));
    }
    meta["requires-dist"] = value(rd);
    if !roots.groups.is_empty() {
        let mut t = Table::new();
        for (k, v) in &roots.groups {
            let mut a = Array::new();
            for r in v {
                a.push(spec_entry(r));
            }
            t[k.as_str()] = value(a);
        }
        meta["requires-dev"] = Item::Table(t);
    }
    root["metadata"] = Item::Table(meta);
    packages.push(root);

    for p in &resolution.packages {
        let mut t = Table::new();
        t["name"] = value(p.name.as_str());
        t["version"] = value(p.version.to_string());
        let mut src = InlineTable::new();
        src.insert("registry", registry.into());
        t["source"] = value(src);
        if !p.dependencies.is_empty() {
            // Plain edges and extra-gated edges both live in `dependencies`,
            // with their markers; the installer evaluates them.
            t["dependencies"] = value(dep_array(&p.dependencies));
        }
        if let Some(sd) = p.files.iter().find(|f| !f.is_wheel()) {
            let mut e = InlineTable::new();
            e.insert("url", sd.url.as_str().into());
            if let Some(h) = &sd.sha256 {
                e.insert("hash", format!("sha256:{h}").into());
            }
            t["sdist"] = value(e);
        }
        let mut wheels = Array::new();
        for f in p.files.iter().filter(|f| f.is_wheel()) {
            let mut e = InlineTable::new();
            e.insert("url", f.url.as_str().into());
            if let Some(h) = &f.sha256 {
                e.insert("hash", format!("sha256:{h}").into());
            }
            wheels.push(e);
        }
        if !wheels.is_empty() {
            t["wheels"] = value(wheels);
        }
        packages.push(t);
    }
    doc["package"] = Item::ArrayOfTables(packages);
    doc.to_string()
}
