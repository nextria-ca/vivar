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
use anyhow::{anyhow, bail, Context, Result};
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
    pub source: Source,
}

/// Where a resolved package comes from: the lock's `source` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Registry,
    /// A wheel or source distribution at a URL (`name @ https://…`).
    Url(String),
    /// A git checkout (`name @ git+https://…@rev`), pinned to a commit.
    Git {
        url: String,
        rev: Option<String>,
        commit: String,
    },
}

/// What a direct requirement (`name @ url`) resolves to.
#[derive(Debug, Clone)]
pub struct Direct {
    pub version: Version,
    pub metadata: Metadata,
    pub files: Vec<File>,
    pub source: Source,
}

/// What the resolver needs beyond the index: direct references, and wheels
/// built from source distributions. Without one, both are refused.
pub trait Fetch {
    fn direct(&self, req: &Requirement) -> Result<Direct>;
    /// A wheel built from `sdist` (cached by its hash).
    fn wheel_from_sdist(&self, name: &str, sdist: &File) -> Result<std::path::PathBuf>;
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
    /// For environments standing in for a whole minor: the lowest and highest
    /// patch of that minor inside the project's range (none for this machine).
    patch_bounds: Vec<Option<(Version, Version)>>,
    /// The last release rejected for a marker that would split a minor, for
    /// the final diagnostic when no alternative worked out.
    last_split: Option<String>,
    wheels_only: bool,
    /// Versions to keep when still acceptable (the current lock).
    prefer: BTreeMap<String, Version>,
    /// Names whose preference is ignored (`upgrade-deps x`), or all when `upgrade_all`.
    upgrade: BTreeSet<String>,
    upgrade_all: bool,
    python_floor: Version,
    cache: BTreeMap<(String, String), Metadata>,
    fetch: Option<&'a dyn Fetch>,
    /// Direct references seen so far, by name: one URL per package.
    directs: BTreeMap<String, Direct>,
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

/// Extras requested of a package, each with the environments requesting it.
type Extras = BTreeMap<String, BTreeSet<usize>>;

#[derive(Clone, Default)]
struct State {
    assigned: Vec<(String, Version)>,
    constraints: BTreeMap<String, Vec<Constraint>>,
    extras: BTreeMap<String, Extras>,
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
            patch_bounds: Vec::new(),
            last_split: None,
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
            fetch: None,
            directs: BTreeMap::new(),
        };
        r.set_envs(python_floor, python_floor);
        r
    }

    /// Enables direct references and source builds.
    pub fn fetch(mut self, fetch: &'a dyn Fetch) -> Self {
        self.fetch = Some(fetch);
        self
    }

    fn set_envs(&mut self, floor: [u32; 3], machine: [u32; 3]) {
        let series = format!("{}.{}", machine[0], machine[1]);
        let here = Target::new(None, &series).expect("current platform is supported");
        let mut envs = vec![(MarkerEnv::current(machine), here)];
        let mut bounds = vec![None];
        let [maj, min, _] = floor;
        for os in ["linux", "macos", "windows"] {
            for arch in ["x86_64", "aarch64"] {
                for minor in min..=(min + 3).max(14) {
                    if let Ok(t) =
                        Target::new(Some(&format!("{os}-{arch}")), &format!("{maj}.{minor}"))
                    {
                        // One sample per minor, at its lowest patch (the floor
                        // keeps its own, `>=3.10.1`); the environment stands
                        // for the minor's whole patch range, see `variants`.
                        let low = if minor == min { floor[2] } else { 0 };
                        envs.push((MarkerEnv::for_target(os, arch, [maj, minor, low]), t));
                        bounds.push(Some((
                            Version::parse(&format!("{maj}.{minor}.{low}")).expect("numeric"),
                            Version::parse(&format!("{maj}.{minor}.99")).expect("numeric"),
                        )));
                    }
                }
            }
        }
        self.pythons = envs
            .iter()
            .map(|(e, _)| Version::parse(&e.python_full_version).expect("numeric version"))
            .collect();
        self.envs = envs;
        self.patch_bounds = bounds;
    }

    /// The Pythons environment `i` may run: its sample, and for a whole-minor
    /// environment both ends of its patch range. A patch-level comparison
    /// must agree on all of them: the resolver works per minor and reports a
    /// marker that would split one rather than guess.
    fn variants(&self, i: usize) -> Vec<Version> {
        let mut v = vec![self.pythons[i].clone()];
        if let Some((lo, hi)) = &self.patch_bounds[i] {
            for p in [lo, hi] {
                if !v.contains(p) {
                    v.push(p.clone());
                }
            }
        }
        v
    }

    /// Whether a `requires-python` holds on every environment in `envs`, at
    /// every patch each stands for: one version per package means it has to
    /// serve all of them.
    fn python_ok(&self, requires_python: Option<&str>, envs: &BTreeSet<usize>) -> bool {
        let Some(rp) = requires_python else {
            return true;
        };
        let Ok(spec) = Specifier::parse(rp) else {
            return true;
        };
        envs.iter().all(|&i| {
            self.variants(i)
                .iter()
                .all(|p| spec.matches_allowing_pre(p))
        })
    }

    /// Keeps only the environments inside the project's `requires-python`
    /// (this machine always stays), each narrowed to the patches of its minor
    /// that are in range: a `>=3.12,<3.13` project is not held to what 3.13
    /// would need, and `!=3.13.0` does not rule 3.13 out.
    pub fn requires_python(mut self, spec: Option<&str>) -> Self {
        let Some(spec) = spec.and_then(|s| Specifier::parse(s).ok()) else {
            return self;
        };
        let mut keep = Vec::with_capacity(self.pythons.len());
        for i in 0..self.pythons.len() {
            let Some((lo, _)) = self.patch_bounds[i].clone() else {
                keep.push(true);
                continue;
            };
            let s = lo.to_string();
            let mut it = s.split('.');
            let (maj, min) = (
                it.next().unwrap_or("3").to_owned(),
                it.next().unwrap_or("0").to_owned(),
            );
            let at = |patch: u32| Version::parse(&format!("{maj}.{min}.{patch}")).ok();
            let first = (0..=99)
                .filter_map(at)
                .find(|v| spec.matches_allowing_pre(v));
            let last = (0..=99)
                .rev()
                .filter_map(at)
                .find(|v| spec.matches_allowing_pre(v));
            match (first, last) {
                (Some(lo), Some(hi)) => {
                    self.envs[i].0.python_full_version = lo.to_string();
                    self.pythons[i] = lo.clone();
                    self.patch_bounds[i] = Some((lo, hi));
                    keep.push(true);
                }
                _ => keep.push(false),
            }
        }
        let mut i = 0;
        self.envs.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        let mut j = 0;
        self.pythons.retain(|_| {
            j += 1;
            keep[j - 1]
        });
        let mut k = 0;
        self.patch_bounds.retain(|_| {
            k += 1;
            keep[k - 1]
        });
        self
    }

    /// One environment only (this machine, at its exact patch): for installs
    /// into an existing interpreter, where nothing else has to be served.
    pub fn single(mut self) -> Self {
        self.envs.truncate(1);
        self.pythons.truncate(1);
        self.patch_bounds.truncate(1);
        self
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
    /// `Err` carries the diagnostic for a marker that would split a minor.
    fn where_holds(
        &self,
        marker: Option<&str>,
        extras: &Extras,
        within: &BTreeSet<usize>,
    ) -> std::result::Result<BTreeSet<usize>, String> {
        let Some(m) = marker else {
            return Ok(within.clone());
        };
        let mut out = BTreeSet::new();
        for &i in within {
            let mut e = self.envs[i].0.clone();
            // Only the extras requested in this very environment count, and
            // the package is also installed bare (a plain `a` next to an
            // optional `a[fast]`), so a marker that mentions extras is tried
            // with them and without: `b; extra != 'fast'` stays in.
            let requested: Vec<String> = extras
                .iter()
                .filter(|(_, envs)| envs.contains(&i))
                .map(|(k, _)| k.clone())
                .collect();
            let mut extra_sets = vec![requested];
            if m.contains("extra") && !extra_sets[0].is_empty() {
                extra_sets.push(Vec::new());
            }
            let holds = |e: &mut MarkerEnv| {
                extra_sets.iter().any(|set| {
                    e.extra = set.clone();
                    eval_marker(m, e).unwrap_or(false)
                })
            };
            if !m.contains("python_full_version") {
                if holds(&mut e) {
                    out.insert(i);
                }
                continue;
            }
            let verdicts: Vec<bool> = self
                .variants(i)
                .iter()
                .map(|p| {
                    e.python_full_version = p.to_string();
                    holds(&mut e)
                })
                .collect();
            if verdicts.iter().all(|v| *v) {
                out.insert(i);
            } else if verdicts.iter().any(|v| *v) {
                return Err(format!(
                    "the marker `{m}` distinguishes patch releases of Python {}; vivar's resolver works per minor (uv can fork such a lock)",
                    e.python_version
                ));
            }
        }
        Ok(out)
    }

    fn all_envs(&self) -> BTreeSet<usize> {
        (0..self.envs.len()).collect()
    }

    /// Candidate versions of `name`, newest first, acceptable under `constraints`
    /// and installable wherever they apply.
    fn candidates(&self, name: &str, constraints: &[Constraint]) -> Result<Vec<Version>> {
        if let Some(d) = self.directs.get(name) {
            let ok = constraints
                .iter()
                .all(|c| c.specifier.matches_allowing_pre(&d.version));
            return Ok(if ok {
                vec![d.version.clone()]
            } else {
                Vec::new()
            });
        }
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
        if let Some(d) = self.directs.get(name) {
            return Ok(d.metadata.clone());
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
            .copied();
        let m = match file {
            Some(file) => self.index.metadata(file)?,
            None => {
                // No wheel at all: build the source distribution once and read
                // its metadata, unless builds are ruled out.
                let sdist = files
                    .iter()
                    .find(|f| {
                        !f.is_wheel() && f.version(name).as_ref() == Some(version) && !f.yanked
                    })
                    .ok_or_else(|| {
                        anyhow!("{name} {version} has no artifact to read dependencies from")
                    })?;
                if self.wheels_only {
                    bail!("{name} {version} ships only a source distribution and policy requires wheels");
                }
                let fetch = self.fetch.ok_or_else(|| {
                    anyhow!("{name} {version} ships only a source distribution; building it needs the native backend's builder")
                })?;
                let wheel = fetch.wheel_from_sdist(name, sdist)?;
                let text = crate::index::metadata_from_wheel(&std::fs::read(&wheel)?)
                    .with_context(|| format!("reading METADATA from {}", wheel.display()))?;
                crate::index::parse_metadata(&text)
            }
        };
        self.cache.insert(key, m.clone());
        Ok(m)
    }

    pub fn resolve(mut self, roots: &Roots) -> Result<Resolution> {
        let mut state = State::default();
        let root_name = crate::project::normalize(&roots.name);
        let all = self.all_envs();
        let root_reqs: Vec<&Requirement> = roots
            .dependencies
            .iter()
            .chain(roots.extras.values().flatten())
            .chain(roots.groups.values().flatten())
            .collect();
        let mut root_envs = Vec::with_capacity(root_reqs.len());
        for r in &root_reqs {
            // A root requirement has no alternative: a split here is final.
            let envs = self
                .where_holds(r.marker.as_deref(), &Extras::new(), &all)
                .map_err(|m| anyhow!("{m}"))?;
            // Extras asked of a package anywhere in the roots are known before
            // its first expansion, so the outcome does not depend on order.
            for e in &r.extras {
                state
                    .extras
                    .entry(r.name.clone())
                    .or_default()
                    .entry(e.clone())
                    .or_default()
                    .extend(envs.iter().copied());
            }
            root_envs.push(envs);
        }
        // Direct references go first: a URL settles a package's version before
        // anything else can pick one from the index.
        let mut ordered: Vec<(&Requirement, BTreeSet<usize>)> =
            root_reqs.iter().copied().zip(root_envs).collect();
        ordered.sort_by_key(|(r, _)| r.url.is_none());
        for (r, envs) in ordered {
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
            // A direct reference is its own single candidate: fetched once,
            // then treated as an exact pin for every other requirement.
            let mut req = req;
            if let Some(url) = req.url.clone() {
                let fetch = self.fetch.ok_or_else(|| {
                    anyhow!(
                        "{}: direct references need the native backend's builder",
                        req.name
                    )
                })?;
                if !self.directs.contains_key(&req.name) {
                    let d = fetch.direct(&req)?;
                    self.directs.insert(req.name.clone(), d);
                }
                let d = &self.directs[&req.name];
                if d.source != Source::Url(url.clone())
                    && !matches!((&d.source, url.strip_prefix("git+")), (Source::Git { url: u, .. }, Some(g)) if g.starts_with(u.as_str()))
                {
                    bail!(
                        "{} is required from two different places: {url} and {:?}",
                        req.name,
                        d.source
                    );
                }
                if let Some((_, v)) = state.assigned.iter().find(|(n, _)| *n == req.name) {
                    if *v != d.version {
                        bail!(
                            "{} is pinned to {} by {url} but version {v} was already chosen for it; direct references must come before index versions",
                            req.name,
                            d.version
                        );
                    }
                }
                req.specifier = Specifier::parse(&format!("=={}", d.version))?;
            }
            // Record the constraint and extras. A package already assigned is
            // expanded again when it now applies in more environments (or with
            // more extras), so its conditional dependencies there are not lost.
            let known: BTreeSet<usize> = state
                .constraints
                .get(&req.name)
                .map(|cs| cs.iter().flat_map(|c| c.envs.iter().copied()).collect())
                .unwrap_or_default();
            let mut grew = !envs.is_subset(&known);
            let requested = state.extras.entry(req.name.clone()).or_default();
            for e in &req.extras {
                let scope = requested.entry(e.clone()).or_default();
                if !envs.is_subset(scope) {
                    scope.extend(envs.iter().copied());
                    grew = true;
                }
            }
            state
                .constraints
                .entry(req.name.clone())
                .or_default()
                .push(Constraint {
                    specifier: req.specifier.clone(),
                    marker: req.marker.clone(),
                    envs,
                });

            if let Some((_, v)) = state.assigned.iter().find(|(n, _)| *n == req.name) {
                let v = v.clone();
                if req.specifier.matches(&v)
                    || req.specifier.matches_allowing_pre(&v) && v.is_prerelease()
                {
                    if grew && !self.enqueue_deps(&mut state, &req.name, &v)? {
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
            // A direct reference brings its own files; the index knows nothing of it.
            let files: Vec<File> = if self.directs.contains_key(name) {
                Vec::new()
            } else {
                self.index
                    .files(name)?
                    .into_iter()
                    .filter(|f| {
                        f.version(name).as_ref() == Some(version) && (!f.yanked || allow_yanked)
                    })
                    .collect()
            };
            let mut dependencies: Vec<Requirement> = Vec::new();
            for (_, r) in state.edges.iter().filter(|(from, _)| from == name) {
                if !dependencies
                    .iter()
                    .any(|d| d.name == r.name && d.extras == r.extras && d.marker == r.marker)
                {
                    dependencies.push(r.clone());
                }
            }
            let (files, source) = match self.directs.get(name) {
                Some(d) => (d.files.clone(), d.source.clone()),
                None => (files, Source::Registry),
            };
            packages.push(ResolvedPackage {
                name: name.clone(),
                version: version.clone(),
                files,
                dependencies,
                source,
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
            // A release whose edge would split a minor is one candidate down,
            // not the end of the resolution: older releases may not have it.
            let envs = match self.where_holds(dep.marker.as_deref(), &extras, &within) {
                Ok(envs) => envs,
                Err(split) => {
                    self.last_split = Some(format!("{name} {version}: {split}"));
                    return Ok(false);
                }
            };
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
                let split = self
                    .last_split
                    .as_deref()
                    .map(|s| format!("; also rejected {s}"))
                    .unwrap_or_default();
                bail!("dependency conflict involving {culprit}: no combination of versions satisfies every constraint{split}");
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
/// `git+https://host/repo.git@rev` → (`https://host/repo.git`, `Some("rev")`).
pub fn git_parts(url: &str) -> (String, Option<String>) {
    let u = url.strip_prefix("git+").unwrap_or(url);
    let u = u.split('#').next().unwrap_or(u);
    // The `@` that separates the rev comes after the last `/`; one before it
    // is userinfo (`git@github.com:`-style URLs are not accepted anyway).
    match u.rfind('@') {
        Some(i) if i > u.rfind('/').unwrap_or(0) => {
            (u[..i].to_owned(), Some(u[i + 1..].to_owned()))
        }
        _ => (u.to_owned(), None),
    }
}

fn exact_pin(constraints: &[Constraint]) -> bool {
    constraints.iter().any(|c| {
        c.specifier
            .to_string()
            .split(',')
            .any(|clause| clause.starts_with("==") && !clause.ends_with(".*"))
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
    // Root requirements that apply in no targeted environment were not
    // resolved; the graph leaves them out while the metadata keeps them.
    let resolved: BTreeSet<&str> = resolution
        .packages
        .iter()
        .map(|p| p.name.as_str())
        .collect();
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
        for r in reqs.iter().filter(|r| resolved.contains(r.name.as_str())) {
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
        match r.url.as_deref() {
            Some(u) if u.starts_with("git+") => {
                let (repo, _) = git_parts(u);
                t.insert("git", repo.into());
            }
            Some(u) => {
                t.insert("url", u.into());
            }
            None => {
                let spec = r.specifier.to_string();
                if !spec.is_empty() {
                    t.insert("specifier", spec.into());
                }
            }
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
        match &p.source {
            Source::Registry => src.insert("registry", registry.into()),
            Source::Url(u) => src.insert("url", u.as_str().into()),
            Source::Git { url, rev, commit } => src.insert(
                "git",
                match rev {
                    Some(r) => format!("{url}?rev={r}#{commit}"),
                    None => format!("{url}#{commit}"),
                }
                .into(),
            ),
        };
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
    fn python_range_keeps_minors_with_an_in_range_patch() {
        let index = Index::new(None);
        let r =
            Resolver::new(&index, [3, 12, 0], true).requires_python(Some(">=3.12,!=3.13.0,<3.14"));
        let pythons: BTreeSet<String> = r.pythons.iter().skip(1).map(|p| p.to_string()).collect();
        assert_eq!(
            pythons,
            BTreeSet::from(["3.12.0".to_owned(), "3.13.1".to_owned()]),
            "{pythons:?}"
        );
        let none = Extras::new();
        let all = r.all_envs();
        let others: BTreeSet<usize> = all.iter().copied().filter(|&i| i != 0).collect();
        // Patch-level comparisons must agree across a minor's in-range
        // patches; one that would split a minor is reported, not guessed.
        assert!(r
            .where_holds(Some("python_full_version < '3.12'"), &none, &all)
            .unwrap()
            .is_empty());
        assert_eq!(
            r.where_holds(Some("python_full_version >= '3.12'"), &none, &all)
                .unwrap(),
            all
        );
        let err = r
            .where_holds(Some("python_full_version >= '3.12.10'"), &none, &all)
            .unwrap_err()
            .to_string();
        assert!(err.contains("patch releases of Python 3.12"), "{err}");
        assert!(r.python_ok(Some(">=3.12"), &others));
        assert!(!r.python_ok(Some(">=3.12.1"), &others)); // not on 3.12.0
        assert!(r
            .where_holds(Some("python_version >= '3.14'"), &none, &all)
            .unwrap()
            .is_empty());
        // Nothing outside the project's range is considered.
        let narrow =
            Resolver::new(&index, [3, 12, 0], true).requires_python(Some(">=3.12,<3.12.1"));
        assert!(narrow
            .where_holds(
                Some("python_full_version >= '3.12.1'"),
                &none,
                &narrow.all_envs()
            )
            .unwrap()
            .is_empty());
        let bounded =
            Resolver::new(&index, [3, 12, 5], true).requires_python(Some(">=3.12.5,<3.12.20"));
        assert_eq!(
            bounded
                .where_holds(
                    Some("python_full_version >= '3.12.5'"),
                    &none,
                    &bounded.all_envs()
                )
                .unwrap(),
            bounded.all_envs()
        );
        assert!(bounded
            .where_holds(
                Some("python_full_version >= '3.12.20'"),
                &none,
                &bounded.all_envs()
            )
            .unwrap()
            .is_empty());
        // A requested extra never hides what the bare package needs.
        let fast = Extras::from([("fast".to_owned(), all.clone())]);
        assert_eq!(
            r.where_holds(Some("extra != 'fast'"), &fast, &all).unwrap(),
            all
        );
        assert_eq!(
            r.where_holds(Some("extra == 'fast'"), &fast, &all).unwrap(),
            all
        );
        assert!(r
            .where_holds(Some("extra == 'fast'"), &none, &all)
            .unwrap()
            .is_empty());
        // A single interpreter is exactly its patch.
        let single = Resolver::new(&index, [3, 12, 0], true).single();
        assert_eq!(single.envs.len(), 1);
        assert!(single
            .where_holds(
                Some("python_full_version >= '3.12.1'"),
                &none,
                &single.all_envs()
            )
            .unwrap()
            .is_empty());
        assert!(!single.python_ok(Some(">=3.12.1"), &single.all_envs()));
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
            source: Source::Registry,
        };
        let direct = ResolvedPackage {
            name: "minipkg".into(),
            version: Version::parse("1.0").unwrap(),
            files: vec![],
            dependencies: vec![],
            source: Source::Git {
                url: "https://example.com/minipkg.git".into(),
                rev: Some("main".into()),
                commit: "abc123".into(),
            },
        };
        assert_eq!(
            git_parts("git+https://example.com/minipkg.git@main#subdirectory=x"),
            (
                "https://example.com/minipkg.git".into(),
                Some("main".into())
            )
        );
        assert_eq!(
            git_parts("git+ssh://git@example.com/r.git"),
            ("ssh://git@example.com/r.git".into(), None)
        );
        let text = write_lock(
            &roots,
            &Resolution {
                packages: vec![direct],
            },
            "https://pypi.org/simple",
        );
        assert!(
            text.contains("source = { git = \"https://example.com/minipkg.git?rev=main#abc123\" }"),
            "{text}"
        );
        let text = write_lock(
            &roots,
            &Resolution {
                packages: vec![pkg],
            },
            "https://pypi.org/simple",
        );
        assert!(text.contains("source = { editable = \".\" }"), "{text}");
        // pytest was never resolved (its marker ruled it out): the graph skips
        // it, the metadata still declares it.
        assert!(
            text.contains("[package.optional-dependencies]\ntest = []"),
            "{text}"
        );
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
