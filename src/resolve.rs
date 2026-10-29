//! Dependency resolution without uv: a backtracking resolver over the simple
//! index, newest acceptable versions first, writing a uv.lock-compatible
//! lockfile. Markers are kept on edges; a dependency is included when its
//! marker can hold on any of the platforms vivar targets, so the lock serves
//! more than the machine it was made on. One version per package: a graph
//! that needs different versions under different markers (a fork) is
//! reported as such, not as a contradiction.

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
    /// The project is a package (it has a build system): the lock installs
    /// it editable, as uv does, instead of treating it as a virtual root.
    pub editable: bool,
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
    /// Environments a marker may hold on; entry 0 is this machine.
    envs: Vec<(MarkerEnv, Target)>,
    /// The Python of each environment, for `requires-python` checks.
    pythons: Vec<Version>,
    wheels_only: bool,
    /// Versions to keep when still acceptable (the current lock).
    prefer: BTreeMap<String, Version>,
    /// Names whose preference is ignored (`upgrade-deps x`), or all when `upgrade_all`.
    upgrade: BTreeSet<String>,
    upgrade_all: bool,
    python_floor: Version,
    cache: BTreeMap<(String, String), Metadata>,
}

/// A requirement on a package together with the environments it applies to.
#[derive(Clone, Debug)]
struct Constraint {
    specifier: Specifier,
    marker: Option<String>,
    envs: BTreeSet<usize>,
}

#[derive(Clone)]
struct Pending {
    req: Requirement,
    envs: BTreeSet<usize>,
}

#[derive(Clone, Default)]
struct State {
    assigned: Vec<(String, Version)>,
    constraints: BTreeMap<String, Vec<Constraint>>,
    extras: BTreeMap<String, BTreeSet<String>>,
    pending: VecDeque<Pending>,
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
    /// `python_floor` is the lowest Python the lock must serve (the project's
    /// `requires-python` bound); this machine's Python is set with `machine`
    /// and defaults to the floor.
    pub fn new(index: &'a Index, python_floor: [u32; 3], wheels_only: bool) -> Resolver<'a> {
        let mut r = Resolver {
            index,
            envs: Vec::new(),
            pythons: Vec::new(),
            wheels_only,
            prefer: BTreeMap::new(),
            upgrade: BTreeSet::new(),
            upgrade_all: false,
            python_floor: Version::parse(&format!(
                "{}.{}.{}",
                python_floor[0], python_floor[1], python_floor[2]
            ))
            .expect("numeric version"),
            cache: BTreeMap::new(),
        };
        r.set_envs(python_floor, python_floor);
        r
    }

    fn set_envs(&mut self, floor: [u32; 3], machine: [u32; 3]) {
        let series = format!("{}.{}", machine[0], machine[1]);
        let here = Target::new(None, &series).expect("current platform is supported");
        let mut envs = vec![(MarkerEnv::current(machine), here)];
        let [maj, min, _] = floor;
        for os in ["linux", "macos", "windows"] {
            for arch in ["x86_64", "aarch64"] {
                for minor in min..=(min + 3).max(14) {
                    if let Ok(t) =
                        Target::new(Some(&format!("{os}-{arch}")), &format!("{maj}.{minor}"))
                    {
                        envs.push((MarkerEnv::for_target(os, arch, [maj, minor, 0]), t));
                    }
                }
            }
        }
        self.pythons = envs
            .iter()
            .map(|(e, _)| Version::parse(&e.python_full_version).expect("numeric version"))
            .collect();
        self.envs = envs;
    }

    /// Whether a `requires-python` holds on every environment in `envs`: one
    /// version per package means it has to serve all of them.
    fn python_ok(&self, requires_python: Option<&str>, envs: &BTreeSet<usize>) -> bool {
        let Some(rp) = requires_python else {
            return true;
        };
        let Ok(spec) = Specifier::parse(rp) else {
            return true;
        };
        envs.iter()
            .all(|&i| spec.matches_allowing_pre(&self.pythons[i]))
    }

    /// The Python this machine installs with, when it differs from the floor.
    pub fn machine(mut self, version: [u32; 3]) -> Self {
        let floor = {
            let f = self.python_floor.to_string();
            let mut it = f.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
            [
                it.next().unwrap_or(3),
                it.next().unwrap_or(0),
                it.next().unwrap_or(0),
            ]
        };
        self.set_envs(floor, version);
        self
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

    /// The environments (indices) where `marker` holds, given the extras
    /// requested of the package that carries it, within `within`.
    fn where_holds(
        &self,
        marker: Option<&str>,
        extras: &BTreeSet<String>,
        within: &BTreeSet<usize>,
    ) -> BTreeSet<usize> {
        let Some(m) = marker else {
            return within.clone();
        };
        within
            .iter()
            .copied()
            .filter(|&i| {
                let mut e = self.envs[i].0.clone();
                e.extra = extras.iter().cloned().collect();
                eval_marker(m, &e).unwrap_or(false)
            })
            .collect()
    }

    fn all_envs(&self) -> BTreeSet<usize> {
        (0..self.envs.len()).collect()
    }

    /// Candidate versions of `name`, newest first, acceptable under `constraints`
    /// and installable wherever they apply.
    fn candidates(&self, name: &str, constraints: &[Constraint]) -> Result<Vec<Version>> {
        let files = self.index.files(name)?;
        let mut by_version: BTreeMap<Version, Vec<&File>> = BTreeMap::new();
        for f in &files {
            if let Some(v) = f.version(name) {
                by_version.entry(v).or_default().push(f);
            }
        }
        let exact_pin = exact_pin(constraints);
        let envs: BTreeSet<usize> = constraints
            .iter()
            .flat_map(|c| c.envs.iter().copied())
            .collect();
        let mut out: Vec<Version> = by_version
            .into_iter()
            .rev()
            .filter(|(v, fs)| {
                constraints.iter().all(|c| {
                    c.specifier.matches(v) || (exact_pin && c.specifier.matches_allowing_pre(v))
                }) && fs.iter().any(|f| {
                    (!f.yanked || exact_pin) && self.python_ok(f.requires_python.as_deref(), &envs)
                })
            })
            .filter(|(_, fs)| self.installable(fs, &envs, exact_pin))
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

    /// An artifact for every place the package is needed: this machine when it
    /// applies here, and at least one of the other targeted environments
    /// otherwise. Source distributions count when builds are allowed.
    fn installable(&self, files: &[&File], envs: &BTreeSet<usize>, allow_yanked: bool) -> bool {
        let usable: Vec<&&File> = files.iter().filter(|f| !f.yanked || allow_yanked).collect();
        if !self.wheels_only
            && usable
                .iter()
                .any(|f| f.filename.ends_with(".tar.gz") || f.filename.ends_with(".zip"))
        {
            return true;
        }
        let fits = |i: usize| {
            usable
                .iter()
                .any(|f| f.is_wheel() && self.envs[i].1.score(&f.filename).is_some())
        };
        if envs.contains(&0) {
            return fits(0);
        }
        envs.iter().any(|&i| fits(i))
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
            .find(|f| self.envs[0].1.score(&f.filename).is_some())
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
        let all = self.all_envs();
        for r in roots
            .dependencies
            .iter()
            .chain(roots.extras.values().flatten())
            .chain(roots.groups.values().flatten())
        {
            let envs = self.where_holds(r.marker.as_deref(), &BTreeSet::new(), &all);
            if envs.is_empty() {
                continue;
            }
            state.edges.push((root_name.clone(), r.clone()));
            state.pending.push_back(Pending {
                req: r.clone(),
                envs,
            });
        }
        let mut frames: Vec<Frame> = Vec::new();
        let mut steps = 0usize;
        loop {
            steps += 1;
            if steps > 20_000 {
                bail!("resolution did not converge after {steps} steps; the constraints may be contradictory");
            }
            let Some(Pending { req, envs }) = state.pending.pop_front() else {
                break; // everything satisfied
            };
            if req.url.is_some() {
                bail!(
                    "{}: direct URL requirements are not supported by the native resolver",
                    req.name
                );
            }
            // Record the constraint and extras. A package already assigned is
            // expanded again when it now applies in more environments (or with
            // more extras), so its conditional dependencies there are not lost.
            let known: BTreeSet<usize> = state
                .constraints
                .get(&req.name)
                .map(|cs| cs.iter().flat_map(|c| c.envs.iter().copied()).collect())
                .unwrap_or_default();
            let grew = !envs.is_subset(&known);
            state
                .constraints
                .entry(req.name.clone())
                .or_default()
                .push(Constraint {
                    specifier: req.specifier.clone(),
                    marker: req.marker.clone(),
                    envs,
                });
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
                    if (!new_extras.is_empty() || grew)
                        && !self.enqueue_deps(&mut state, &req.name, &v)?
                    {
                        state = self.backtrack(&mut frames, &req.name)?;
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
                if let Some(fork) = self.fork(&constraints) {
                    bail!(
                        "{} is required at incompatible versions under different conditions ({fork}); vivar's resolver keeps one version per package, uv can fork the lock",
                        req.name
                    );
                }
                if frames.is_empty() {
                    bail!(
                        "no version of {} satisfies {} (wheels-only: {}) on {}",
                        req.name,
                        constraints
                            .iter()
                            .map(|c| c.specifier.to_string())
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
            if !self.enqueue_deps(&mut state, &req.name, &version)? {
                state = self.backtrack(&mut frames, &req.name)?;
            }
        }

        // Build the result from the final state.
        let mut packages = Vec::new();
        for (name, version) in &state.assigned {
            let allow_yanked = state.constraints.get(name).is_some_and(|c| exact_pin(c));
            let files: Vec<File> = self
                .index
                .files(name)?
                .into_iter()
                .filter(|f| {
                    f.version(name).as_ref() == Some(version) && (!f.yanked || allow_yanked)
                })
                .collect();
            let mut dependencies: Vec<Requirement> = Vec::new();
            for (_, r) in state.edges.iter().filter(|(from, _)| from == name) {
                if !dependencies
                    .iter()
                    .any(|d| d.name == r.name && d.extras == r.extras && d.marker == r.marker)
                {
                    dependencies.push(r.clone());
                }
            }
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

    /// Two constraints that never apply on the same environment: the
    /// description of the fork, if the constraints form one.
    fn fork(&self, constraints: &[Constraint]) -> Option<String> {
        for (i, a) in constraints.iter().enumerate() {
            for b in &constraints[i + 1..] {
                if !a.envs.is_empty() && !b.envs.is_empty() && a.envs.is_disjoint(&b.envs) {
                    return Some(format!(
                        "{}{} vs {}{}",
                        a.specifier,
                        a.marker
                            .as_deref()
                            .map(|m| format!(" when {m}"))
                            .unwrap_or_default(),
                        b.specifier,
                        b.marker
                            .as_deref()
                            .map(|m| format!(" when {m}"))
                            .unwrap_or_default()
                    ));
                }
            }
        }
        None
    }

    /// Queues the dependencies of `name == version`. `false` when the release
    /// cannot be used after all (its metadata requires another Python than
    /// some environment it applies in has).
    fn enqueue_deps(&mut self, state: &mut State, name: &str, version: &Version) -> Result<bool> {
        let meta = self.metadata(name, version)?;
        let extras = state.extras.get(name).cloned().unwrap_or_default();
        let within: BTreeSet<usize> = state
            .constraints
            .get(name)
            .map(|cs| cs.iter().flat_map(|c| c.envs.iter().copied()).collect())
            .unwrap_or_else(|| self.all_envs());
        if !self.python_ok(meta.requires_python.as_deref(), &within) {
            return Ok(false);
        }
        for dep in &meta.requires_dist {
            let envs = self.where_holds(dep.marker.as_deref(), &extras, &within);
            if envs.is_empty() {
                continue;
            }
            state.edges.push((name.to_owned(), dep.clone()));
            state.pending.push_back(Pending {
                req: dep.clone(),
                envs,
            });
        }
        Ok(true)
    }

    /// Chronological backtracking: the newest frame with an untried candidate
    /// takes its next version; frames with nothing left are dropped. Every
    /// combination is reachable, so a solution is found when one exists.
    fn backtrack(&mut self, frames: &mut Vec<Frame>, culprit: &str) -> Result<State> {
        loop {
            let Some(frame) = frames.last_mut() else {
                bail!("dependency conflict involving {culprit}: no combination of versions satisfies every constraint");
            };
            if frame.next >= frame.candidates.len() {
                frames.pop();
                continue;
            }
            let version = frame.candidates[frame.next].clone();
            frame.next += 1;
            let mut state = frame.before.clone();
            let name = frame.name.clone();
            state.assigned.push((name.clone(), version.clone()));
            if self.enqueue_deps(&mut state, &name, &version)? {
                return Ok(state);
            }
        }
    }
}

/// An actual pin to one release (`==1.2.3`, `===x`), not `==1.*` or a range.
fn exact_pin(constraints: &[Constraint]) -> bool {
    constraints.iter().any(|c| {
        let s = c.specifier.to_string();
        s.starts_with("==") && !s.contains(',') && !s.ends_with(".*")
    })
}

/// Splits `extra == 'x'` out of a marker that is a top-level conjunction:
/// `(Some("x"), rest)`. Anything else comes back untouched as the rest.
pub fn split_extra(marker: &str) -> (Option<String>, Option<String>) {
    let mut parts: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut cur = String::new();
    let chars: Vec<char> = marker.chars().collect();
    let mut i = 0;
    let mut has_or = false;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                _ if depth == 0 && marker[i..].starts_with(" and ") && !cur.trim().is_empty() => {
                    parts.push(cur.trim().to_owned());
                    cur.clear();
                    i += 4;
                }
                _ if depth == 0 && marker[i..].starts_with(" or ") => {
                    has_or = true;
                    cur.push(c);
                }
                _ => cur.push(c),
            },
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_owned());
    }
    if has_or {
        return (None, Some(marker.to_owned()));
    }
    let mut extra = None;
    let mut rest = Vec::new();
    for p in parts {
        let found = p
            .strip_prefix("extra")
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix("=="))
            .map(str::trim)
            .and_then(|v| {
                let v = v.strip_prefix(['\'', '"'])?;
                let v = v.strip_suffix(['\'', '"'])?;
                Some(crate::project::normalize(v))
            });
        match (found, &extra) {
            (Some(e), None) => extra = Some(e),
            _ => rest.push(p),
        }
    }
    let rest = if rest.is_empty() {
        None
    } else {
        Some(rest.join(" and "))
    };
    (extra, rest)
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
    let dep_entry = |r: &Requirement, marker: Option<&str>| {
        let mut t = InlineTable::new();
        t.insert("name", r.name.as_str().into());
        if !r.extras.is_empty() {
            let mut a = Array::new();
            for e in &r.extras {
                a.push(e.as_str());
            }
            t.insert("extra", a.into());
        }
        if let Some(m) = marker {
            t.insert("marker", m.into());
        }
        t
    };
    let dep_array = |reqs: &[Requirement]| {
        let mut a = Array::new();
        for r in reqs {
            a.push(dep_entry(r, r.marker.as_deref()));
        }
        a
    };
    let spec_entry = |r: &Requirement, extra: Option<&str>| {
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
        let marker = match (r.marker.as_deref(), extra) {
            (Some(m), Some(e)) if m.contains(" or ") => Some(format!("({m}) and extra == '{e}'")),
            (Some(m), Some(e)) => Some(format!("{m} and extra == '{e}'")),
            (Some(m), None) => Some(m.to_owned()),
            (None, Some(e)) => Some(format!("extra == '{e}'")),
            (None, None) => None,
        };
        if let Some(m) = marker {
            t.insert("marker", m.into());
        }
        t
    };

    let mut packages = ArrayOfTables::new();
    // The project itself.
    let mut root = Table::new();
    root["name"] = value(crate::project::normalize(&roots.name));
    root["version"] = value(roots.version.as_str());
    let mut src = InlineTable::new();
    src.insert(
        if roots.editable {
            "editable"
        } else {
            "virtual"
        },
        ".".into(),
    );
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
    for r in &roots.dependencies {
        rd.push(spec_entry(r, None));
    }
    for (k, v) in &roots.extras {
        for r in v {
            rd.push(spec_entry(r, Some(k)));
        }
    }
    meta["requires-dist"] = value(rd);
    if !roots.groups.is_empty() {
        let mut t = Table::new();
        for (k, v) in &roots.groups {
            let mut a = Array::new();
            for r in v {
                a.push(spec_entry(r, None));
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
        // Plain edges go to `dependencies`; `extra == 'x'` edges under the
        // extra, so an installer following `extra = [...]` finds them.
        let mut plain = Array::new();
        let mut by_extra: BTreeMap<String, Array> = BTreeMap::new();
        for d in &p.dependencies {
            match d.marker.as_deref().map(split_extra) {
                Some((Some(extra), rest)) => by_extra
                    .entry(extra)
                    .or_default()
                    .push(dep_entry(d, rest.as_deref())),
                _ => plain.push(dep_entry(d, d.marker.as_deref())),
            }
        }
        if !plain.is_empty() {
            t["dependencies"] = value(plain);
        }
        if !by_extra.is_empty() {
            let mut ot = Table::new();
            for (k, v) in by_extra {
                ot[k.as_str()] = value(v);
            }
            t["optional-dependencies"] = Item::Table(ot);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_split_out_of_markers() {
        assert_eq!(
            split_extra("extra == 'socks'"),
            (Some("socks".to_owned()), None)
        );
        assert_eq!(
            split_extra("python_full_version < '3.11' and extra == \"Toml\""),
            (
                Some("toml".to_owned()),
                Some("python_full_version < '3.11'".to_owned())
            )
        );
        assert_eq!(
            split_extra("extra == 'a' and sys_platform == 'win32' and python_version >= '3.9'"),
            (
                Some("a".to_owned()),
                Some("sys_platform == 'win32' and python_version >= '3.9'".to_owned())
            )
        );
        // A disjunction is not pulled apart.
        let m = "extra == 'a' or sys_platform == 'win32'";
        assert_eq!(split_extra(m), (None, Some(m.to_owned())));
        assert_eq!(
            split_extra("(sys_platform == 'win32' and extra == 'x')"),
            (
                None,
                Some("(sys_platform == 'win32' and extra == 'x')".to_owned())
            )
        );
        assert_eq!(
            split_extra("sys_platform == 'win32'"),
            (None, Some("sys_platform == 'win32'".to_owned()))
        );
    }

    #[test]
    fn lock_puts_extra_edges_under_the_extra() {
        let roots = Roots {
            name: "app".into(),
            version: "0.1.0".into(),
            requires_python: Some(">=3.12".into()),
            dependencies: vec![Requirement::parse("requests[socks]").unwrap()],
            extras: BTreeMap::from([(
                "test".to_owned(),
                vec![Requirement::parse("pytest; python_version < '4'").unwrap()],
            )]),
            editable: true,
            ..Default::default()
        };
        let pkg = ResolvedPackage {
            name: "requests".into(),
            version: Version::parse("2.32.0").unwrap(),
            files: vec![],
            dependencies: vec![
                Requirement::parse("urllib3<3").unwrap(),
                Requirement::parse("PySocks>=1.5.6; extra == 'socks'").unwrap(),
            ],
        };
        let text = write_lock(
            &roots,
            &Resolution {
                packages: vec![pkg],
            },
            "https://pypi.org/simple",
        );
        assert!(text.contains("source = { editable = \".\" }"), "{text}");
        assert!(
            text.contains("[package.optional-dependencies]\nsocks = [{ name = \"pysocks\" }]"),
            "{text}"
        );
        assert!(
            text.contains("dependencies = [{ name = \"urllib3\" }]"),
            "{text}"
        );
        assert!(
            text.contains(
                "{ name = \"pytest\", marker = \"python_version < '4' and extra == 'test'\" }"
            ),
            "{text}"
        );
        let lock = crate::lock::Lock::parse(&text).unwrap();
        let requests = lock.packages.iter().find(|p| p.name == "requests").unwrap();
        assert_eq!(requests.extras["socks"][0].name, "pysocks");
        assert_eq!(requests.base_dependencies.len(), 1);
    }
}
