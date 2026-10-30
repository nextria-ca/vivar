//! Dependency resolution without uv: a backtracking resolver over the simple
//! index, newest acceptable versions first, writing a uv.lock-compatible
//! lockfile. Markers are kept on edges; a dependency is included when its
//! marker can hold on any of the platforms vivar targets, so the lock serves
//! more than the machine it was made on. One version per package: a graph
//! that needs different versions under different markers (a fork) is
//! reported as such, not as a contradiction.

use crate::index::{File, Index, Metadata};
use crate::pep::{eval_marker, MarkerEnv, Requirement, Specifier, Version};
use crate::project::{GitRef, Selector, SourceKind};
use crate::wheelhouse::Target;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

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
    /// Workspace members, each a root of its own: resolved together, laid
    /// out in the lock beside the root, installed editable from `path`.
    pub members: Vec<Member>,
    /// Where `[tool.uv.sources]` sends the roots' requirements, for the
    /// lock's `requires-dist` metadata.
    pub sources: Vec<RootSource>,
    /// Python packages the native (conda) environment provides, by PyPI
    /// name with their versions: satisfied without an index.
    pub provided: BTreeMap<String, String>,
}

/// A root requirement's declared source: an index, git (no commit yet), a
/// URL, a file or directory; for one group or extra when it is specific to
/// one, under a marker when it is conditional.
#[derive(Debug, Clone)]
pub struct RootSource {
    pub name: String,
    pub source: Source,
    pub selector: Option<Selector>,
    pub marker: Option<String>,
}

/// A workspace member: its requirements and where it lives under the root.
#[derive(Debug, Clone, Default)]
pub struct Member {
    pub roots: Roots,
    /// Relative to the lock's root, forward slashes.
    pub path: String,
}

impl Roots {
    /// Puts group `g` back into the root and members from `full`.
    pub fn restore_group(&mut self, full: &Roots, g: &str) {
        if let Some(reqs) = full.groups.get(g) {
            self.groups.insert(g.to_owned(), reqs.clone());
        }
        for (m, fm) in self.members.iter_mut().zip(&full.members) {
            m.roots.restore_group(&fm.roots, g);
        }
    }

    /// Drops the groups and extras in `selectors` from the root and every
    /// member (a profile's leave the base), or keeps only them.
    pub fn retain_selectors(&mut self, selectors: &[Selector], keep: bool) {
        let groups: Vec<String> = selectors
            .iter()
            .filter(|s| s.is_group())
            .map(|s| s.name().to_owned())
            .collect();
        let extras: Vec<String> = selectors
            .iter()
            .filter(|s| !s.is_group())
            .map(|s| s.name().to_owned())
            .collect();
        self.groups.retain(|g, _| groups.contains(g) == keep);
        self.extras.retain(|e, _| extras.contains(e) == keep);
        for m in &mut self.members {
            m.roots.retain_selectors(selectors, keep);
        }
    }

    /// Puts one group or extra back into the root and members from `full`.
    pub fn restore_selector(&mut self, full: &Roots, s: &Selector) {
        match s {
            Selector::Group(g) => self.restore_group(full, g),
            Selector::Extra(e) => {
                if let Some(reqs) = full.extras.get(e) {
                    self.extras.insert(e.clone(), reqs.clone());
                }
                for (m, fm) in self.members.iter_mut().zip(&full.members) {
                    m.roots.restore_selector(&fm.roots, s);
                }
            }
        }
    }

    /// Every requirement of the project and its members, with the root
    /// package that declares it.
    fn all_reqs(&self) -> Vec<(String, &Requirement)> {
        let mut out: Vec<(String, &Requirement)> = Vec::new();
        let name = crate::project::normalize(&self.name);
        for r in self
            .dependencies
            .iter()
            .chain(self.extras.values().flatten())
            .chain(self.groups.values().flatten())
        {
            out.push((name.clone(), r));
        }
        for m in &self.members {
            out.extend(m.roots.all_reqs());
        }
        out
    }

    /// Names (normalized) of the root and its members: requirements on these
    /// are met by the workspace itself, not an index.
    fn local_names(&self) -> BTreeMap<String, Version> {
        let mut out = BTreeMap::new();
        if self.editable {
            if let Ok(v) = Version::parse(&self.version) {
                out.insert(crate::project::normalize(&self.name), v);
            }
        }
        for m in &self.members {
            out.extend(m.roots.local_names());
        }
        for (name, version) in &self.provided {
            if let Ok(v) = Version::parse(version) {
                out.insert(name.clone(), v);
            }
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    pub files: Vec<File>,
    /// Dependencies with the markers they carry, as they go into the lock,
    /// each with what it resolved to alongside this package.
    pub dependencies: Vec<Edge>,
    pub source: Source,
    /// The fork sides this variant belongs to (empty: all of them).
    pub resolution_markers: Vec<String>,
}

/// One dependency edge: the requirement and the variant it resolved to, so
/// a lock holding several variants of a name can pin the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub req: Requirement,
    pub pin: Option<Pin>,
}

impl Edge {
    pub fn plain(req: Requirement) -> Edge {
        Edge { req, pin: None }
    }

    /// Same requirement on the same variant.
    fn same(&self, other: &Edge) -> bool {
        self.req.name == other.req.name
            && self.req.extras == other.req.extras
            && self.req.marker == other.req.marker
            && self.pin == other.pin
    }
}

/// One chosen variant of a package: its version and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub version: Version,
    pub source: Source,
}

/// Where a resolved package comes from: the lock's `source` table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// A package index, by URL (or a wheelhouse directory).
    Registry(String),
    /// A wheel or source archive at a URL (`name @ https://…`), built from
    /// `subdirectory` of the archive when given.
    Url {
        url: String,
        subdirectory: Option<String>,
    },
    /// A git checkout (`name @ git+https://…@rev#subdirectory=sub`), pinned
    /// to a commit.
    Git {
        url: String,
        reference: Option<GitRef>,
        commit: String,
        subdirectory: Option<String>,
    },
    /// A wheel or source archive on disk, relative to the lock's root.
    Path(String),
    /// A project directory on disk, built and installed as a package.
    Directory(String),
    /// A project directory on disk, installed editable.
    Editable(String),
}

impl Source {
    /// The lock's `source` table for this source.
    pub fn lock_table(&self) -> toml_edit::InlineTable {
        let mut src = toml_edit::InlineTable::new();
        match self {
            Source::Registry(u) => src.insert("registry", u.as_str().into()),
            Source::Url { url, subdirectory } => {
                src.insert("url", url.as_str().into());
                if let Some(sub) = subdirectory {
                    src.insert("subdirectory", sub.as_str().into());
                }
                None
            }
            Source::Git {
                url,
                reference,
                commit,
                subdirectory,
            } => src.insert(
                "git",
                git_lock_url(url, reference, commit, subdirectory).into(),
            ),
            Source::Path(p) => src.insert("path", p.as_str().into()),
            Source::Directory(p) => src.insert("directory", p.as_str().into()),
            Source::Editable(p) => src.insert("editable", p.as_str().into()),
        };
        src
    }
}

/// A `[tool.uv.sources]` entry handed to the resolver: the package is
/// fetched or built from it instead of looked up on an index.
#[derive(Debug, Clone)]
pub struct Sourced {
    pub kind: SourceKind,
    /// The directory the entry's path is relative to.
    pub base: PathBuf,
    /// The same path relative to the lock's root, for the lock.
    pub rel: Option<String>,
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
    /// A package from a `[tool.uv.sources]` entry (git, url, path).
    fn sourced(&self, name: &str, source: &Sourced) -> Result<Direct>;
    /// A wheel built from `sdist` (cached by its hash).
    fn wheel_from_sdist(&self, name: &str, sdist: &File) -> Result<std::path::PathBuf>;
}

pub struct Resolution {
    pub packages: Vec<ResolvedPackage>,
    /// Every assignment with its source, by name.
    pub pins: BTreeMap<String, Pin>,
}

/// What goes into a lock: one resolution, the merged sides of a fork, or a
/// base plus one per profile (conflicting groups or extras) merged, where a
/// name may have several variants told apart by version and source (edges
/// pin them) and by `resolution-markers` (forks).
pub struct Lockable {
    pub packages: Vec<ResolvedPackage>,
    /// What the roots' edges resolved to: `None` for the dependencies and
    /// extras, `Some(selector)` for a group or extra resolved on its own.
    /// A name pinned differently on the sides of a fork lists every pin.
    pub root_pins: BTreeMap<Option<Selector>, BTreeMap<String, Vec<Pin>>>,
    /// The markers the lock forks on, when it does.
    pub fork_markers: Vec<String>,
}

impl Lockable {
    pub fn single(r: Resolution) -> Lockable {
        Lockable {
            packages: r.packages,
            root_pins: BTreeMap::from([(None, pins_as_sets(r.pins))]),
            fork_markers: Vec::new(),
        }
    }

    /// Merges the sides of a fork: a release present on several sides is one
    /// package carrying each side's marker; a name with a single variant
    /// across all sides needs no markers at all. One side is just itself.
    pub fn merge_forks(sides: Vec<(String, Resolution)>) -> Lockable {
        if sides.len() == 1 {
            return Lockable::single(sides.into_iter().next().map(|(_, r)| r).unwrap());
        }
        let mut packages: Vec<ResolvedPackage> = Vec::new();
        let mut root_pins: BTreeMap<String, Vec<Pin>> = BTreeMap::new();
        let mut fork_markers = Vec::new();
        // A release seen from several sides may list different dependencies
        // (another platform's wheel) or pin them differently: an edge one
        // side lacks is scoped to the sides that have it.
        // An `extra == 'x'` clause stays at the top level, where the lock
        // writer looks for it.
        let scope = |e: &Edge, side: &str| -> Edge {
            let mut r = e.req.clone();
            r.marker = Some(match &r.marker {
                Some(m) => match split_extra(m) {
                    (Some(x), Some(rest)) => format!("extra == '{x}' and ({rest}) and ({side})"),
                    (Some(x), None) => format!("extra == '{x}' and ({side})"),
                    (None, _) => format!("({m}) and ({side})"),
                },
                None => side.to_owned(),
            });
            Edge {
                req: r,
                pin: e.pin.clone(),
            }
        };
        for (marker, r) in sides {
            for (name, pin) in r.pins {
                let set = root_pins.entry(name).or_default();
                if !set.contains(&pin) {
                    set.push(pin);
                }
            }
            for mut p in r.packages {
                if let Some(existing) = packages
                    .iter_mut()
                    .find(|q| q.name == p.name && q.version == p.version && q.source == p.source)
                {
                    let only_mine: Vec<Edge> = p
                        .dependencies
                        .iter()
                        .filter(|d| !existing.dependencies.iter().any(|e| e.same(d)))
                        .cloned()
                        .collect();
                    let only_theirs: Vec<usize> = existing
                        .dependencies
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| !p.dependencies.iter().any(|d| e.same(d)))
                        .map(|(i, _)| i)
                        .collect();
                    if !only_theirs.is_empty() {
                        let theirs = existing.resolution_markers.join(" or ");
                        for i in only_theirs {
                            existing.dependencies[i] = scope(&existing.dependencies[i], &theirs);
                        }
                    }
                    for d in only_mine {
                        existing.dependencies.push(scope(&d, &marker));
                    }
                    existing.resolution_markers.push(marker.clone());
                } else {
                    p.resolution_markers = vec![marker.clone()];
                    packages.push(p);
                }
            }
            fork_markers.push(marker);
        }
        let counts: BTreeMap<&str, usize> = packages.iter().fold(BTreeMap::new(), |mut m, p| {
            *m.entry(p.name.as_str()).or_default() += 1;
            m
        });
        let single: Vec<String> = counts
            .iter()
            .filter(|(_, n)| **n == 1)
            .map(|(k, _)| k.to_string())
            .collect();
        for p in &mut packages {
            if single.contains(&p.name) {
                p.resolution_markers.clear();
            }
        }
        packages.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        Lockable {
            packages,
            root_pins: BTreeMap::from([(None, root_pins)]),
            fork_markers,
        }
    }

    /// Merges a base lock with the locks of conflicting groups or extras
    /// (each possibly forked). Packages identical in name, version and
    /// source collapse into one. An edge is pinned once, so a shared package
    /// whose dependencies, or a base dependency whose version, differ between
    /// profiles cannot be written and is reported (uv's conflict markers can
    /// express it).
    pub fn merge(base: Lockable, profiles: Vec<(Selector, Lockable)>) -> Result<Lockable> {
        let mut packages = base.packages;
        let mut root_pins = base.root_pins;
        let mut fork_markers = base.fork_markers;
        let base_pins = root_pins.get(&None).cloned().unwrap_or_default();
        for (selector, lk) in profiles {
            let group = selector.name().to_owned();
            let theirs = lk.root_pins.get(&None).cloned().unwrap_or_default();
            for (name, pins) in &base_pins {
                if let Some(other) = theirs.get(name) {
                    if other != pins {
                        bail!(
                            "{name} resolves to {} with profile {group} but {} without; a base dependency must agree across profiles (uv can fork it)",
                            describe_pins(other),
                            describe_pins(pins)
                        );
                    }
                }
            }
            for p in lk.packages {
                if let Some(existing) = packages
                    .iter_mut()
                    .find(|q| q.name == p.name && q.version == p.version && q.source == p.source)
                {
                    // The same release seen from another profile: keep every
                    // edge it saw, and insist its pins agree.
                    for d in p.dependencies {
                        let clash = existing.dependencies.iter().find(|e| {
                            e.req.name == d.req.name
                                && e.req.extras == d.req.extras
                                && e.req.marker == d.req.marker
                                && e.pin != d.pin
                        });
                        if let Some(e) = clash {
                            bail!(
                                "{} {} needs {} {} in profile {group} but {} elsewhere; one lock cannot hold both (uv can fork it)",
                                p.name,
                                p.version,
                                d.req.name,
                                d.pin.as_ref().map_or("?".to_owned(), |x| x.version.to_string()),
                                e.pin.as_ref().map_or("?".to_owned(), |x| x.version.to_string())
                            );
                        }
                        if !existing.dependencies.iter().any(|e| e.same(&d)) {
                            existing.dependencies.push(d);
                        }
                    }
                    // Markers say where a variant applies: present everywhere
                    // in one resolution, it applies everywhere.
                    if existing.resolution_markers.is_empty() || p.resolution_markers.is_empty() {
                        existing.resolution_markers.clear();
                    } else {
                        for m in p.resolution_markers {
                            if !existing.resolution_markers.contains(&m) {
                                existing.resolution_markers.push(m);
                            }
                        }
                    }
                } else {
                    packages.push(p);
                }
            }
            for m in lk.fork_markers {
                if !fork_markers.contains(&m) {
                    fork_markers.push(m);
                }
            }
            root_pins.insert(Some(selector), theirs);
        }
        packages.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        Ok(Lockable {
            packages,
            root_pins,
            fork_markers,
        })
    }

    /// Whether several packages share this name and version (from different
    /// sources), so an edge must also say which source.
    fn same_version_variants(&self, name: &str, version: &Version) -> usize {
        self.packages
            .iter()
            .filter(|p| p.name == name && p.version == *version)
            .count()
    }

    fn variants(&self, name: &str) -> usize {
        self.packages.iter().filter(|p| p.name == name).count()
    }

    /// Every version assigned to a name (one, or one per fork side).
    pub fn assigned(&self) -> BTreeMap<String, Version> {
        let mut out = BTreeMap::new();
        for p in &self.packages {
            out.entry(p.name.clone())
                .or_insert_with(|| p.version.clone());
        }
        out
    }
}

fn pins_as_sets(pins: BTreeMap<String, Pin>) -> BTreeMap<String, Vec<Pin>> {
    pins.into_iter().map(|(n, p)| (n, vec![p])).collect()
}

fn describe_pins(pins: &[Pin]) -> String {
    pins.iter()
        .map(|p| p.version.to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// Two requirements on one package that can never hold together; the lock
/// has to fork on `marker` (resolved once where it holds, once where it
/// does not) to satisfy both.
#[derive(Debug, Clone)]
pub struct ForkNeeded {
    pub name: String,
    pub marker: String,
}

impl std::fmt::Display for ForkNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is required at incompatible versions on either side of `{}`",
            self.name, self.marker
        )
    }
}

impl std::error::Error for ForkNeeded {}

/// The negation of a PEP 508 marker, for the other side of a fork: operators
/// invert, `and`/`or` swap (De Morgan). Anything the splitter does not
/// recognise is an error rather than a guess.
pub fn negate_marker(marker: &str) -> Result<String> {
    // Markers may space their operators any way; one space makes the
    // and/or splitter and the operator table see them all.
    let normalized = marker.split_whitespace().collect::<Vec<_>>().join(" ");
    let m = normalized.as_str();
    let inner = m
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .filter(|r| balanced(r));
    if let Some(inner) = inner {
        return negate_marker(inner);
    }
    for (op, swapped) in [(" or ", " and "), (" and ", " or ")] {
        let parts = split_top(m, op);
        if parts.len() > 1 {
            let negated: Result<Vec<String>> = parts
                .iter()
                .map(|p| negate_marker(p).map(|n| format!("({n})")))
                .collect();
            return Ok(negated?.join(swapped));
        }
    }
    for (op, inv) in [
        ("===", "!=="),
        ("<=", ">"),
        (">=", "<"),
        ("==", "!="),
        ("!=", "=="),
        ("<", ">="),
        (">", "<="),
        (" not in ", " in "),
        (" in ", " not in "),
    ] {
        if let Some((l, r)) = m.split_once(op) {
            if inv == "!==" {
                bail!("cannot negate `{m}` for a fork");
            }
            return Ok(format!("{} {} {}", l.trim(), inv.trim(), r.trim()));
        }
    }
    bail!("cannot negate `{m}` for a fork")
}

fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Splits on `sep` outside parentheses and quotes.
fn split_top<'s>(s: &'s str, sep: &str) -> Vec<&'s str> {
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let c = bytes[i] as char;
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '(' => depth += 1,
                ')' => depth -= 1,
                _ if depth == 0 && s[i..].starts_with(sep) => {
                    parts.push(s[start..i].trim());
                    i += sep.len();
                    start = i;
                    continue;
                }
                _ => {}
            },
        }
        i += 1;
    }
    parts.push(s[start..].trim());
    parts
}

/// Resolves `roots`, forking on markers where the graph needs it: each side
/// is resolved with the environments narrowed to that side, recursively, and
/// comes back with the marker that selects it.
/// Builds a resolver for one side of a fork, given the restrictions so far.
pub type ForkBuilder<'a> = dyn Fn(&[(String, bool)]) -> Result<Resolver<'a>> + 'a;

pub fn resolve_with_forks<'a>(
    build: &ForkBuilder<'a>,
    roots: &Roots,
    restrictions: Vec<(String, bool)>,
) -> Result<Vec<(String, Resolution)>> {
    match build(&restrictions)?.resolve(roots) {
        Ok(r) => {
            // Each side's marker, compound ones in parentheses, joined by `and`.
            let paren = |m: String| {
                if split_top(&m, " or ").len() > 1 || split_top(&m, " and ").len() > 1 {
                    format!("({m})")
                } else {
                    m
                }
            };
            let mut clauses: Vec<String> = Vec::new();
            for (m, holds) in &restrictions {
                let clause = if *holds {
                    paren(m.clone())
                } else {
                    paren(negate_marker(m).unwrap_or_else(|_| m.clone()))
                };
                if !clauses.contains(&clause) {
                    clauses.push(clause);
                }
            }
            let marker = clauses.join(" and ");
            Ok(vec![(marker, r)])
        }
        Err(e) => match e.downcast_ref::<ForkNeeded>() {
            Some(f) if restrictions.len() < 4 => {
                negate_marker(&f.marker)?;
                let mut yes = restrictions.clone();
                yes.push((f.marker.clone(), true));
                let mut no = restrictions;
                no.push((f.marker.clone(), false));
                let mut out = resolve_with_forks(build, roots, yes)?;
                out.extend(resolve_with_forks(build, roots, no)?);
                Ok(out)
            }
            _ => Err(e),
        },
    }
}

pub struct Resolver<'a> {
    index: &'a Index,
    /// Whether environment 0 is this machine (a fork may leave it out).
    machine_included: bool,
    /// Packages routed to another index (`[tool.uv.sources]`).
    routes: Vec<(String, Index)>,
    /// Packages that come from git, a URL or a path (`[tool.uv.sources]`).
    sourced: BTreeMap<String, Sourced>,
    /// Indexes searched before the main one for everything else
    /// (`[[tool.uv.index]]` without `explicit`), first match wins.
    defaults: Vec<Index>,
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
    /// Direct references already fetched or built, by (name, URL): the name
    /// is part of the key because the artifact is checked against it.
    direct_cache: BTreeMap<(String, String), Direct>,
    /// A fork the graph seemed to need: raised once every other choice is
    /// exhausted, since an earlier candidate may avoid it.
    fork_hint: Option<ForkNeeded>,
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
    /// Direct references in force, by name: part of the state so backtracking
    /// past the requirement that introduced one drops it too.
    directs: BTreeMap<String, Direct>,
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
            machine_included: true,
            routes: Vec::new(),
            sourced: BTreeMap::new(),
            defaults: Vec::new(),
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
            direct_cache: BTreeMap::new(),
            fork_hint: None,
        };
        r.set_envs(python_floor, python_floor);
        r
    }

    /// Enables direct references and source builds.
    pub fn fetch(mut self, fetch: &'a dyn Fetch) -> Self {
        self.fetch = Some(fetch);
        self
    }

    /// Routes packages to indexes of their own.
    pub fn routes(mut self, routes: Vec<(String, Index)>) -> Self {
        self.routes = routes;
        self
    }

    /// Indexes to search before the main one, in order.
    pub fn defaults(mut self, defaults: Vec<Index>) -> Self {
        self.defaults = defaults;
        self
    }

    /// Packages fetched or built from a declared source instead of an index.
    pub fn sources(mut self, sources: BTreeMap<String, Sourced>) -> Self {
        self.sourced = sources;
        self
    }

    /// How many environments this resolver serves; none after restrictions
    /// that contradict each other.
    pub fn env_count(&self) -> usize {
        self.envs.len()
    }

    /// The index `name` comes from: its route, else the first default index
    /// that lists it, else the main one. A default index that cannot be
    /// asked is an error, not a reason to look further (that could hand a
    /// private name to a public index).
    fn index_of(&self, name: &str) -> Result<&Index> {
        if let Some((_, i)) = self.routes.iter().find(|(n, _)| n == name) {
            return Ok(i);
        }
        for i in &self.defaults {
            if !i.files(name)?.is_empty() {
                return Ok(i);
            }
        }
        Ok(self.index)
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

    /// One side of a fork: keeps the environments where `marker` evaluates to
    /// `holds` (this machine included only when it is on that side).
    pub fn restrict(mut self, marker: &str, holds: bool) -> Self {
        let keep: Vec<bool> = self
            .envs
            .iter()
            .map(|(e, _)| eval_marker(marker, e).unwrap_or(false) == holds)
            .collect();
        if !keep.first().copied().unwrap_or(false) {
            self.machine_included = false;
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
    fn candidates(
        &self,
        name: &str,
        constraints: &[Constraint],
        directs: &BTreeMap<String, Direct>,
    ) -> Result<Vec<Version>> {
        if let Some(d) = directs.get(name) {
            let ok = constraints
                .iter()
                .all(|c| c.specifier.matches_allowing_pre(&d.version));
            return Ok(if ok {
                vec![d.version.clone()]
            } else {
                Vec::new()
            });
        }
        let files = self.index_of(name)?.files(name)?;
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
        if self.machine_included && envs.contains(&0) {
            return fits(0);
        }
        envs.iter().any(|&i| fits(i))
    }

    /// The wheel whose metadata describes `version` (one for this machine if
    /// possible; metadata rarely differs between wheels of one release).
    fn metadata(
        &mut self,
        name: &str,
        version: &Version,
        directs: &BTreeMap<String, Direct>,
    ) -> Result<Metadata> {
        if let Some(d) = directs.get(name) {
            return Ok(d.metadata.clone());
        }
        let key = (name.to_owned(), version.to_string());
        if let Some(m) = self.cache.get(&key) {
            return Ok(m.clone());
        }
        let files = self.index_of(name)?.files(name)?;
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
            Some(file) => self.index_of(name)?.metadata(file)?,
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
        let locals = roots.local_names();
        let all = self.all_envs();
        let all_reqs = roots.all_reqs();
        // A root's own requirement on a member is fixed on both ends: nothing
        // to backtrack over, so it is checked before anything is searched,
        // where it applies at all.
        for (from, r) in &all_reqs {
            if let Some(v) = locals.get(&r.name) {
                let applies = self
                    .where_holds(r.marker.as_deref(), &Extras::new(), &all)
                    .map(|e| !e.is_empty())
                    .unwrap_or(true);
                if applies && !r.specifier.matches_allowing_pre(v) {
                    bail!(
                        "{from} requires {} {}, but the workspace member is at version {v}",
                        r.name,
                        r.specifier
                    );
                }
            }
        }
        let root_reqs: Vec<&Requirement> = all_reqs.iter().map(|(_, r)| *r).collect();
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
        let mut ordered: Vec<((String, &Requirement), BTreeSet<usize>)> =
            all_reqs.into_iter().zip(root_envs).collect();
        ordered.sort_by_key(|((_, r), _)| r.url.is_none());
        for ((from, r), envs) in ordered {
            if envs.is_empty() {
                continue;
            }
            state.edges.push((from, r.clone()));
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
                if let Some(f) = self.fork_hint.take() {
                    return Err(anyhow::Error::new(f));
                }
                bail!("resolution did not converge after {steps} steps; the constraints may be contradictory");
            }
            let Some(Pending { req, envs }) = state.pending.pop_front() else {
                break; // everything satisfied
            };
            // A requirement on the root or a member is met by the workspace
            // itself; its version is checked, nothing is fetched.
            if let Some(v) = locals.get(&req.name) {
                // What the native environment provides comes as built: an
                // extra would want dependencies no one resolves.
                if !req.extras.is_empty() && roots.provided.contains_key(&req.name) {
                    bail!(
                        "{}[{}] asks for an extra of a package the native environment provides; add the extra's dependencies to native-dependencies or drop the extra",
                        req.name,
                        req.extras.join(",")
                    );
                }
                if !req.specifier.matches_allowing_pre(v) {
                    // The member's version is fixed; the candidate that asked
                    // for another may not be. Try older ones first.
                    if frames.is_empty() {
                        bail!(
                            "{} is a workspace member at version {v}, which does not satisfy {}",
                            req.name,
                            req.specifier
                        );
                    }
                    state = self.backtrack(&mut frames, &req.name)?;
                }
                continue;
            }
            // A direct reference is its own single candidate: fetched once,
            // then treated as an exact pin for every other requirement. The
            // selection lives in the state, the fetched artifact in a cache.
            let mut req = req;
            let mut reexpand_direct = false;
            // A declared source (`[tool.uv.sources]`) makes a plain
            // requirement direct too, unless the requirement names a URL itself.
            let sourced = match &req.url {
                None => self.sourced.get(&req.name).cloned(),
                Some(_) => None,
            };
            if req.url.is_some() || sourced.is_some() {
                let fetch = self.fetch.ok_or_else(|| {
                    anyhow!(
                        "{}: direct references need the native backend's builder",
                        req.name
                    )
                })?;
                let key = match (&req.url, &sourced) {
                    (Some(u), _) => u.clone(),
                    (None, Some(src)) => format!("source:{:?}", src.kind),
                    (None, None) => unreachable!(),
                };
                let cache_key = (req.name.clone(), key);
                let d = match self.direct_cache.get(&cache_key) {
                    Some(d) => d.clone(),
                    None => {
                        let d = match &sourced {
                            Some(src) => fetch.sourced(&req.name, src)?,
                            None => fetch.direct(&req)?,
                        };
                        self.direct_cache.insert(cache_key, d.clone());
                        d
                    }
                };
                if let Some(active) = state.directs.get(&req.name) {
                    if active.source != d.source {
                        bail!(
                            "{} is required from two different places: {:?} and {:?}",
                            req.name,
                            active.source,
                            d.source
                        );
                    }
                }
                // The source says which version there is; the requirement
                // still has to be satisfied by it.
                if !req.specifier.matches_allowing_pre(&d.version) {
                    bail!(
                        "{} is version {} at its declared source, which does not satisfy `{}{}`",
                        req.name,
                        d.version,
                        req.name,
                        req.specifier
                    );
                }
                let switching = !state.directs.contains_key(&req.name);
                match state.assigned.iter().find(|(n, _)| *n == req.name) {
                    // Chosen from the index before the URL arrived: the same
                    // version keeps the assignment but takes the direct
                    // source's metadata (once); another version is a conflict.
                    Some((_, v)) if *v == d.version => reexpand_direct = switching,
                    Some(_) => {
                        state = self.backtrack(&mut frames, &req.name)?;
                        continue;
                    }
                    None => {}
                }
                req.specifier = Specifier::parse(&format!("=={}", d.version))?;
                state.directs.insert(req.name.clone(), d);
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
                    if (grew || reexpand_direct) && !self.enqueue_deps(&mut state, &req.name, &v)? {
                        state = self.backtrack(&mut frames, &req.name)?;
                    }
                    continue;
                }
                // Conflict with an earlier choice. When no version could ever
                // satisfy every constraint and two of them never apply
                // together, the lock has to fork; otherwise backtrack.
                let constraints = state
                    .constraints
                    .get(&req.name)
                    .cloned()
                    .unwrap_or_default();
                if self
                    .candidates(&req.name, &constraints, &state.directs)?
                    .is_empty()
                {
                    if let Some(marker) = self.fork(&constraints) {
                        self.fork_hint.get_or_insert(ForkNeeded {
                            name: req.name.clone(),
                            marker,
                        });
                    }
                }
                state = self.backtrack(&mut frames, &req.name)?;
                continue;
            }
            let constraints = state
                .constraints
                .get(&req.name)
                .cloned()
                .unwrap_or_default();
            let candidates = self.candidates(&req.name, &constraints, &state.directs)?;
            if candidates.is_empty() {
                if let Some(marker) = self.fork(&constraints) {
                    self.fork_hint.get_or_insert(ForkNeeded {
                        name: req.name.clone(),
                        marker,
                    });
                }
                if frames.is_empty() {
                    if let Some(f) = self.fork_hint.take() {
                        return Err(anyhow::Error::new(f));
                    }
                    bail!(
                        "no version of {} satisfies {} (wheels-only: {}) on {}",
                        req.name,
                        constraints
                            .iter()
                            .map(|c| c.specifier.to_string())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        self.wheels_only,
                        self.index_of(&req.name)?.url()
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
            let files: Vec<File> = if state.directs.contains_key(name) {
                Vec::new()
            } else {
                self.index_of(name)?
                    .files(name)?
                    .into_iter()
                    .filter(|f| {
                        f.version(name).as_ref() == Some(version) && (!f.yanked || allow_yanked)
                    })
                    .collect()
            };
            let mut dependencies: Vec<Edge> = Vec::new();
            for (_, r) in state.edges.iter().filter(|(from, _)| from == name) {
                if !dependencies.iter().any(|d| {
                    d.req.name == r.name && d.req.extras == r.extras && d.req.marker == r.marker
                }) {
                    dependencies.push(Edge::plain(r.clone()));
                }
            }
            let (files, source) = match state.directs.get(name) {
                Some(d) => (d.files.clone(), d.source.clone()),
                None => (
                    files,
                    Source::Registry(self.index_of(name)?.url().to_owned()),
                ),
            };
            packages.push(ResolvedPackage {
                name: name.clone(),
                version: version.clone(),
                files,
                dependencies,
                source,
                resolution_markers: Vec::new(),
            });
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        let pins: BTreeMap<String, Pin> = packages
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    Pin {
                        version: p.version.clone(),
                        source: p.source.clone(),
                    },
                )
            })
            .collect();
        for p in &mut packages {
            for d in &mut p.dependencies {
                d.pin = pins.get(&d.req.name).cloned();
            }
        }
        Ok(Resolution { packages, pins })
    }

    /// Two constraints that never apply on the same environment: the
    /// description of the fork, if the constraints form one.
    /// The marker a fork would split on: that of the first of two constraints
    /// that never apply together.
    fn fork(&self, constraints: &[Constraint]) -> Option<String> {
        for (i, a) in constraints.iter().enumerate() {
            for b in &constraints[i + 1..] {
                if !a.envs.is_empty() && !b.envs.is_empty() && a.envs.is_disjoint(&b.envs) {
                    return a.marker.clone().or_else(|| b.marker.clone());
                }
            }
        }
        None
    }

    /// Queues the dependencies of `name == version`. `false` when the release
    /// cannot be used after all (its metadata requires another Python than
    /// some environment it applies in has).
    fn enqueue_deps(&mut self, state: &mut State, name: &str, version: &Version) -> Result<bool> {
        let meta = self.metadata(name, version, &state.directs)?;
        let extras = state.extras.get(name).cloned().unwrap_or_default();
        let within: BTreeSet<usize> = state
            .constraints
            .get(name)
            .map(|cs| cs.iter().flat_map(|c| c.envs.iter().copied()).collect())
            .unwrap_or_else(|| self.all_envs());
        if !self.python_ok(meta.requires_python.as_deref(), &within) {
            return Ok(false);
        }
        // Listings and newest metadata of everything this release pulls in are
        // fetched ahead, in parallel, while the resolver works through the queue.
        let ahead: Vec<String> = meta
            .requires_dist
            .iter()
            .filter(|d| {
                d.url.is_none()
                    && !self.routes.iter().any(|(n, _)| *n == d.name)
                    && !self.sourced.contains_key(&d.name)
            })
            .map(|d| d.name.clone())
            .collect();
        // Only when the main index is the one these names would come from: with
        // other indexes ahead of it, a private name must never reach it early.
        if !ahead.is_empty() && self.defaults.is_empty() {
            let series = self.envs[0].0.python_version.clone();
            self.index.prefetch(&ahead, &series);
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
                if let Some(f) = self.fork_hint.take() {
                    return Err(anyhow::Error::new(f));
                }
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
/// The lock's form of a git source, as uv writes it: the repository, then
/// `?subdirectory=…` and `&rev=…` (or `tag=`, `branch=`) as asked, then
/// `#commit` (left off when there is none yet, as in `requires-dist`).
pub fn git_lock_url(
    url: &str,
    reference: &Option<GitRef>,
    commit: &str,
    subdirectory: &Option<String>,
) -> String {
    let mut query = Vec::new();
    if let Some(s) = subdirectory {
        query.push(format!("subdirectory={s}"));
    }
    if let Some(r) = reference {
        query.push(format!("{}={}", r.key(), r.value()));
    }
    let mut out = url.to_owned();
    if !query.is_empty() {
        out.push('?');
        out.push_str(&query.join("&"));
    }
    if !commit.is_empty() {
        out.push('#');
        out.push_str(commit);
    }
    out
}

/// A direct reference's URL without its fragment, plus the `sha256=` and
/// `subdirectory=` the fragment carries (`&`-separated, either order).
pub fn url_fragment(url: &str) -> (String, Option<String>, Option<String>) {
    let Some((bare, fragment)) = url.split_once('#') else {
        return (url.to_owned(), None, None);
    };
    let mut sha = None;
    let mut sub = None;
    for part in fragment.split('&') {
        if let Some(v) = part.strip_prefix("sha256=") {
            sha = Some(v.to_owned());
        } else if let Some(v) = part.strip_prefix("subdirectory=") {
            sub = Some(v.to_owned());
        }
    }
    (bare.to_owned(), sha, sub)
}

/// The `subdirectory` named in a direct git reference's fragment
/// (`git+https://…@rev#subdirectory=sub`), checked to stay inside the checkout.
pub fn git_subdirectory(url: &str) -> Result<Option<String>> {
    let Some((_, fragment)) = url.split_once('#') else {
        return Ok(None);
    };
    for part in fragment.split('&') {
        if let Some(sub) = part.strip_prefix("subdirectory=") {
            let sub = sub.trim_matches('/');
            let clean = !sub.is_empty()
                && !sub.contains('\\')
                && !sub
                    .split('/')
                    .any(|c| c.is_empty() || c == "." || c == "..");
            if !clean {
                bail!("{url}: `subdirectory={sub}` must be a relative path inside the repository");
            }
            return Ok(Some(sub.to_owned()));
        }
    }
    Ok(None)
}

pub fn git_parts(url: &str) -> (String, Option<String>) {
    let u = url.strip_prefix("git+").unwrap_or(url);
    let u = u.split('#').next().unwrap_or(u);
    // The rev's `@` sits in the path, after the authority; an `@` inside the
    // authority is userinfo. Revs may contain slashes (`feature/x`).
    let path_start = u
        .find("://")
        .map(|i| i + 3)
        .and_then(|a| u[a..].find('/').map(|p| a + p))
        .unwrap_or(0);
    match u[path_start..].find('@') {
        Some(i) => {
            let at = path_start + i;
            (u[..at].to_owned(), Some(u[at + 1..].to_owned()))
        }
        None => (u.to_owned(), None),
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
pub fn write_lock(roots: &Roots, lock: &Lockable) -> String {
    use toml_edit::{value, Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table};
    // Root requirements that apply in no targeted environment were not
    // resolved; the graph leaves them out while the metadata keeps them.
    let mut resolved: BTreeSet<String> = lock.packages.iter().map(|p| p.name.clone()).collect();
    resolved.insert(crate::project::normalize(&roots.name));
    for m in &roots.members {
        resolved.insert(crate::project::normalize(&m.roots.name));
    }
    let mut doc = DocumentMut::new();
    doc["version"] = value(1);
    doc["revision"] = value(3);
    if let Some(rp) = &roots.requires_python {
        doc["requires-python"] = value(rp.as_str());
    }
    if !lock.fork_markers.is_empty() {
        let mut a = Array::new();
        for m in &lock.fork_markers {
            a.push(m.as_str());
        }
        doc["resolution-markers"] = value(a);
    }
    let extras_array = |r: &Requirement| {
        let mut a = Array::new();
        for e in &r.extras {
            a.push(e.as_str());
        }
        a
    };
    // The entries for one edge. The variants it may point at are those
    // its pins allow (every variant when it has none). One variant: pinned
    // by version (and source when the version is shared) whenever the name
    // has several. Several, from a fork: one entry per variant, pinned and
    // scoped by the variant's side, as uv writes forked locks.
    let entries_for = |r: &Requirement, pins: &[Pin], from_sides: &[String]| -> Vec<InlineTable> {
        let variants: Vec<&ResolvedPackage> = lock
            .packages
            .iter()
            .filter(|p| p.name == r.name)
            .filter(|p| {
                pins.is_empty()
                    || pins
                        .iter()
                        .any(|pin| pin.version == p.version && pin.source == p.source)
            })
            // An edge from one side of a fork points only at variants on that side.
            .filter(|p| {
                from_sides.is_empty()
                    || p.resolution_markers.is_empty()
                    || p.resolution_markers.iter().any(|m| from_sides.contains(m))
            })
            .collect();
        let total = lock.variants(&r.name);
        let mut out = Vec::new();
        if variants.len() <= 1 {
            let mut t = InlineTable::new();
            t.insert("name", r.name.as_str().into());
            if let Some(v) = variants.first().filter(|_| total > 1) {
                t.insert("version", v.version.to_string().into());
                if lock.same_version_variants(&v.name, &v.version) > 1 {
                    t.insert("source", v.source.lock_table().into());
                }
            }
            if !r.extras.is_empty() {
                t.insert("extra", extras_array(r).into());
            }
            if let Some(m) = &r.marker {
                t.insert("marker", m.as_str().into());
            }
            out.push(t);
            return out;
        }
        for v in variants {
            let side = if v.resolution_markers.len() == 1 {
                v.resolution_markers[0].clone()
            } else {
                v.resolution_markers
                    .iter()
                    .map(|m| format!("({m})"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            };
            // An edge is left out only when its own marker is exactly the
            // negation of the variant's side; anything less certain stays.
            if let Some(m) = &r.marker {
                let opposite = negate_marker(m).ok().as_deref() == Some(side.as_str())
                    || negate_marker(&side).ok().as_deref() == Some(m.as_str());
                if opposite {
                    continue;
                }
            }
            let marker = match &r.marker {
                Some(m) if *m == side || side.is_empty() => m.clone(),
                Some(m) => format!("({m}) and ({side})"),
                None => side.clone(),
            };
            let mut t = InlineTable::new();
            t.insert("name", r.name.as_str().into());
            t.insert("version", v.version.to_string().into());
            if lock.same_version_variants(&v.name, &v.version) > 1 {
                t.insert("source", v.source.lock_table().into());
            }
            if !r.extras.is_empty() {
                t.insert("extra", extras_array(r).into());
            }
            if !marker.is_empty() {
                t.insert("marker", marker.into());
            }
            out.push(t);
        }
        out
    };
    let no_pins: Vec<Pin> = Vec::new();
    let no_sides: Vec<String> = Vec::new();
    // A root's edges, pinned by what its group or extra resolved to.
    let root_array = |reqs: &[Requirement], selector: Option<&Selector>| {
        let pins = lock
            .root_pins
            .get(&selector.cloned())
            .or_else(|| lock.root_pins.get(&None));
        let mut a = Array::new();
        for r in reqs.iter().filter(|r| resolved.contains(&r.name)) {
            let own = pins.and_then(|p| p.get(&r.name)).unwrap_or(&no_pins);
            for t in entries_for(r, own, &no_sides) {
                a.push(t);
            }
        }
        a
    };
    // `requires-dist`: what the project declares, with its declared source
    // (an index, git, a URL, a file, a directory) and the group or extra
    // that source is specific to.
    let root_name = crate::project::normalize(&roots.name);
    let spec_entries =
        |r: &Requirement, extra: Option<&str>, group: Option<&str>| -> Vec<InlineTable> {
            let selector = match (group, extra) {
                (Some(g), _) => Some(Selector::Group(g.to_owned())),
                (None, Some(e)) => Some(Selector::Extra(e.to_owned())),
                (None, None) => None,
            };
            let declared: Vec<&RootSource> = roots
                .sources
                .iter()
                .filter(|s| s.name == r.name)
                .filter(|s| s.selector.is_none() || s.selector == selector)
                .collect();
            // The most specific entries win: the selector's own, else the plain ones.
            let specific: Vec<&RootSource> = declared
                .iter()
                .copied()
                .filter(|s| s.selector.is_some())
                .collect();
            let chosen: Vec<Option<&RootSource>> = if !specific.is_empty() {
                specific.into_iter().map(Some).collect()
            } else if !declared.is_empty() {
                declared.into_iter().map(Some).collect()
            } else {
                vec![None]
            };
            chosen
                .into_iter()
                .map(|src| {
                    let mut t = InlineTable::new();
                    t.insert("name", r.name.as_str().into());
                    if !r.extras.is_empty() {
                        t.insert("extras", extras_array(r).into());
                    }
                    let spec = r.specifier.to_string();
                    match (r.url.as_deref(), src) {
                        (Some(u), _) if u.starts_with("git+") => {
                            let (repo, rev) = git_parts(u);
                            let sub = git_subdirectory(u).ok().flatten();
                            t.insert(
                                "git",
                                git_lock_url(&repo, &rev.map(GitRef::Rev), "", &sub).into(),
                            );
                        }
                        (Some(u), _) => {
                            let (bare, _, sub) = url_fragment(u);
                            t.insert("url", bare.into());
                            if let Some(sub) = sub {
                                t.insert("subdirectory", sub.into());
                            }
                        }
                        (None, Some(s)) => {
                            match &s.source {
                                Source::Registry(url) => {
                                    if !spec.is_empty() {
                                        t.insert("specifier", spec.as_str().into());
                                    }
                                    t.insert("index", url.as_str().into());
                                }
                                Source::Git {
                                    url,
                                    reference,
                                    subdirectory,
                                    ..
                                } => {
                                    t.insert(
                                        "git",
                                        git_lock_url(url, reference, "", subdirectory).into(),
                                    );
                                }
                                Source::Url { url, subdirectory } => {
                                    t.insert("url", url.as_str().into());
                                    if let Some(sub) = subdirectory {
                                        t.insert("subdirectory", sub.as_str().into());
                                    }
                                }
                                Source::Path(p) => {
                                    t.insert("path", p.as_str().into());
                                }
                                Source::Directory(p) => {
                                    t.insert("directory", p.as_str().into());
                                }
                                Source::Editable(p) => {
                                    t.insert("editable", p.as_str().into());
                                }
                            }
                            if let Some(sel) = &s.selector {
                                let mut c = InlineTable::new();
                                c.insert("package", root_name.as_str().into());
                                c.insert(
                                    if sel.is_group() { "group" } else { "extra" },
                                    sel.name().into(),
                                );
                                t.insert("conflict", c.into());
                            }
                        }
                        (None, None) => {
                            if !spec.is_empty() {
                                t.insert("specifier", spec.as_str().into());
                            }
                        }
                    }
                    let source_marker = src.and_then(|s| s.marker.clone());
                    let mut clauses: Vec<String> = Vec::new();
                    for m in [r.marker.clone(), source_marker].into_iter().flatten() {
                        clauses.push(if m.contains(" or ") {
                            format!("({m})")
                        } else {
                            m
                        });
                    }
                    if let Some(e) = extra {
                        clauses.push(format!("extra == '{e}'"));
                    }
                    if !clauses.is_empty() {
                        t.insert("marker", clauses.join(" and ").into());
                    }
                    t
                })
                .collect()
        };

    let mut packages = ArrayOfTables::new();
    // The project itself, then each workspace member: all roots of the lock.
    let mut root_table = |r: &Roots, path: &str| {
        let mut root = Table::new();
        root["name"] = value(crate::project::normalize(&r.name));
        root["version"] = value(r.version.as_str());
        let mut src = InlineTable::new();
        src.insert(if r.editable { "editable" } else { "virtual" }, path.into());
        root["source"] = value(src);
        if !r.dependencies.is_empty() {
            root["dependencies"] = value(root_array(&r.dependencies, None));
        }
        if !r.extras.is_empty() {
            let mut t = Table::new();
            for (k, v) in &r.extras {
                t[k.as_str()] = value(root_array(v, Some(&Selector::Extra(k.clone()))));
            }
            root["optional-dependencies"] = Item::Table(t);
        }
        if !r.groups.is_empty() {
            let mut t = Table::new();
            for (k, v) in &r.groups {
                t[k.as_str()] = value(root_array(v, Some(&Selector::Group(k.clone()))));
            }
            root["dev-dependencies"] = Item::Table(t);
        }
        let mut meta = Table::new();
        let mut rd = Array::new();
        for d in &r.dependencies {
            for e in spec_entries(d, None, None) {
                rd.push(e);
            }
        }
        for (k, v) in &r.extras {
            for d in v {
                for e in spec_entries(d, Some(k), None) {
                    rd.push(e);
                }
            }
        }
        meta["requires-dist"] = value(rd);
        if !r.groups.is_empty() {
            let mut t = Table::new();
            for (k, v) in &r.groups {
                let mut a = Array::new();
                for d in v {
                    for e in spec_entries(d, None, Some(k)) {
                        a.push(e);
                    }
                }
                t[k.as_str()] = value(a);
            }
            meta["requires-dev"] = Item::Table(t);
        }
        root["metadata"] = Item::Table(meta);
        packages.push(root);
    };
    root_table(roots, ".");
    for m in &roots.members {
        root_table(&m.roots, &m.path);
    }
    if !roots.members.is_empty() {
        let mut names: Vec<String> = roots
            .members
            .iter()
            .map(|m| crate::project::normalize(&m.roots.name))
            .collect();
        names.push(crate::project::normalize(&roots.name));
        names.sort();
        names.dedup();
        let mut manifest = Table::new();
        let mut a = Array::new();
        for n in names {
            a.push(n);
        }
        manifest["members"] = value(a);
        doc["manifest"] = Item::Table(manifest);
    }

    for p in &lock.packages {
        let mut t = Table::new();
        t["name"] = value(p.name.as_str());
        t["version"] = value(p.version.to_string());
        if !p.resolution_markers.is_empty() {
            let mut a = Array::new();
            for m in &p.resolution_markers {
                a.push(m.as_str());
            }
            t["resolution-markers"] = value(a);
        }
        t["source"] = value(p.source.lock_table());
        // Plain edges go to `dependencies`; `extra == 'x'` edges under the
        // extra, so an installer following `extra = [...]` finds them.
        let mut plain = Array::new();
        let mut by_extra: BTreeMap<String, Array> = BTreeMap::new();
        for d in &p.dependencies {
            // An edge into a package the native environment provides has no
            // lock package to point at: left out, as the root's are.
            if roots.provided.contains_key(&d.req.name) {
                continue;
            }
            let pins: Vec<Pin> = d.pin.iter().cloned().collect();
            let (extra, req) = match d.req.marker.as_deref().map(split_extra) {
                Some((Some(extra), rest)) => {
                    let mut r = d.req.clone();
                    r.marker = rest;
                    (Some(extra), r)
                }
                _ => (None, d.req.clone()),
            };
            let target = match extra {
                Some(e) => by_extra.entry(e).or_default(),
                None => &mut plain,
            };
            for e in entries_for(&req, &pins, &p.resolution_markers) {
                target.push(e);
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
        // A file on disk is listed by name and hash, as uv does; anything
        // fetched by URL keeps the URL.
        let local = matches!(p.source, Source::Path(_));
        if let Some(sd) = p.files.iter().find(|f| !f.is_wheel()) {
            let mut e = InlineTable::new();
            if !local {
                e.insert("url", sd.url.as_str().into());
            }
            if let Some(h) = &sd.sha256 {
                e.insert("hash", format!("sha256:{h}").into());
            }
            t["sdist"] = value(e);
        }
        let mut wheels = Array::new();
        for f in p.files.iter().filter(|f| f.is_wheel()) {
            let mut e = InlineTable::new();
            if local {
                e.insert("filename", f.filename.as_str().into());
            } else {
                e.insert("url", f.url.as_str().into());
            }
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
    fn merged_profiles_pin_edges_into_variants() {
        let reg = Source::Registry("https://pypi.org/simple".into());
        let pin = |v: &str| Pin {
            version: Version::parse(v).unwrap(),
            source: reg.clone(),
        };
        let pkg =
            |name: &str, version: &str, deps: &[&str], resolved: &[(&str, &str)]| ResolvedPackage {
                name: name.into(),
                version: Version::parse(version).unwrap(),
                files: vec![],
                dependencies: deps
                    .iter()
                    .map(|d| {
                        let req = Requirement::parse(d).unwrap();
                        let pin = resolved
                            .iter()
                            .find(|(n, _)| *n == req.name)
                            .map(|(_, v)| pin(v));
                        Edge { req, pin }
                    })
                    .collect(),
                source: reg.clone(),
                resolution_markers: Vec::new(),
            };
        let pins = |pairs: &[(&str, &str)]| -> BTreeMap<String, Pin> {
            pairs.iter().map(|(n, v)| (n.to_string(), pin(v))).collect()
        };
        let base = Resolution {
            packages: vec![pkg("numpy", "2.0", &[], &[])],
            pins: pins(&[("numpy", "2.0")]),
        };
        let cpu = Resolution {
            packages: vec![
                pkg("numpy", "2.0", &[], &[]),
                pkg("torch", "2.5.0+cpu", &["numpy"], &[("numpy", "2.0")]),
            ],
            pins: pins(&[("numpy", "2.0"), ("torch", "2.5.0+cpu")]),
        };
        let cu = Resolution {
            packages: vec![
                pkg("numpy", "2.0", &[], &[]),
                pkg(
                    "torch",
                    "2.5.0+cu124",
                    &["numpy", "nvidia-cublas"],
                    &[("numpy", "2.0"), ("nvidia-cublas", "12.4")],
                ),
                pkg("nvidia-cublas", "12.4", &[], &[]),
            ],
            pins: pins(&[
                ("numpy", "2.0"),
                ("torch", "2.5.0+cu124"),
                ("nvidia-cublas", "12.4"),
            ]),
        };
        let lock = Lockable::merge(
            Lockable::single(base),
            vec![
                (Selector::Group("cpu".into()), Lockable::single(cpu)),
                (Selector::Group("cu124".into()), Lockable::single(cu)),
            ],
        )
        .unwrap();
        assert_eq!(
            lock.packages.iter().filter(|p| p.name == "torch").count(),
            2
        );
        assert_eq!(
            lock.packages.iter().filter(|p| p.name == "numpy").count(),
            1
        );
        let roots = Roots {
            name: "app".into(),
            version: "0.1.0".into(),
            dependencies: vec![Requirement::parse("numpy").unwrap()],
            groups: BTreeMap::from([
                ("cpu".to_owned(), vec![Requirement::parse("torch").unwrap()]),
                (
                    "cu124".to_owned(),
                    vec![Requirement::parse("torch").unwrap()],
                ),
            ]),
            ..Default::default()
        };
        let text = write_lock(&roots, &lock);
        assert!(
            text.contains("cpu = [{ name = \"torch\", version = \"2.5.0+cpu\" }]"),
            "{text}"
        );
        assert!(
            text.contains("cu124 = [{ name = \"torch\", version = \"2.5.0+cu124\" }]"),
            "{text}"
        );
        assert!(
            text.contains("dependencies = [{ name = \"numpy\" }]"),
            "{text}"
        );
        let parsed = crate::lock::Lock::parse(&text).unwrap();
        let cpu_env = MarkerEnv::current([3, 12, 0]);
        let names = |groups: &[&str]| -> Vec<String> {
            crate::native::plan(&parsed, &cpu_env, groups)
                .unwrap()
                .iter()
                .map(|p| format!("{} {}", p.name, p.version))
                .collect()
        };
        assert_eq!(
            names(&["cpu"]),
            vec!["app 0.1.0", "numpy 2.0", "torch 2.5.0+cpu"]
        );
        assert_eq!(
            names(&["cu124"]),
            vec![
                "app 0.1.0",
                "numpy 2.0",
                "nvidia-cublas 12.4",
                "torch 2.5.0+cu124"
            ]
        );
    }

    /// A zip with stored (uncompressed) entries: enough for a wheel the
    /// resolver only reads METADATA from.
    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let name_b = name.as_bytes();
            let mut local = vec![0x50, 0x4b, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            local.extend_from_slice(&0u32.to_le_bytes()); // crc (not checked)
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(data.len() as u32).to_le_bytes());
            local.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(name_b);
            local.extend_from_slice(data);
            out.extend_from_slice(&local);
            let mut c = vec![0x50, 0x4b, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            c.extend_from_slice(&0u32.to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            c.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
            c.extend_from_slice(&0u32.to_le_bytes());
            c.extend_from_slice(&offset.to_le_bytes());
            c.extend_from_slice(name_b);
            central.extend_from_slice(&c);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        let mut eocd = vec![0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0];
        eocd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        eocd.extend_from_slice(&(central.len() as u32).to_le_bytes());
        eocd.extend_from_slice(&cd_offset.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&eocd);
        out
    }

    #[test]
    fn markers_negate_and_locks_fork() {
        assert_eq!(
            negate_marker("python_version < '3.13'").unwrap(),
            "python_version >= '3.13'"
        );
        assert_eq!(
            negate_marker("sys_platform == 'win32' and python_version >= '3.12'").unwrap(),
            "(sys_platform != 'win32') or (python_version < '3.12')"
        );
        assert_eq!(
            negate_marker("(extra == 'a' or extra == 'b')").unwrap(),
            "(extra != 'a') and (extra != 'b')"
        );

        let dir = std::env::temp_dir().join(format!("vivar-fork-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for v in ["1.0", "2.0"] {
            let meta = format!("Metadata-Version: 2.1\nName: fk\nVersion: {v}\n");
            let whl = stored_zip(&[
                (&format!("fk-{v}.dist-info/METADATA"), meta.as_bytes()),
                (
                    &format!("fk-{v}.dist-info/WHEEL"),
                    b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
                ),
            ]);
            std::fs::write(dir.join(format!("fk-{v}-py3-none-any.whl")), whl).unwrap();
        }
        let index = Index::new(Some(dir.to_str().unwrap()));
        let roots = Roots {
            name: "app".into(),
            version: "0.1.0".into(),
            requires_python: Some(">=3.12".into()),
            dependencies: vec![
                Requirement::parse("fk<2; python_version < '3.13'").unwrap(),
                Requirement::parse("fk>=2; python_version >= '3.13'").unwrap(),
            ],
            ..Default::default()
        };
        let build = |restrictions: &[(String, bool)]| -> Result<Resolver<'_>> {
            let mut r = Resolver::new(&index, [3, 12, 0], true).requires_python(Some(">=3.12"));
            for (m, holds) in restrictions {
                r = r.restrict(m, *holds);
            }
            Ok(r)
        };
        // One version per package cannot satisfy both: a plain resolve says so.
        let err = match build(&[]).unwrap().resolve(&roots) {
            Ok(_) => panic!("resolved without a fork"),
            Err(e) => e,
        };
        assert!(err.downcast_ref::<ForkNeeded>().is_some(), "{err:#}");
        let sides = resolve_with_forks(&build, &roots, Vec::new()).unwrap();
        assert_eq!(
            sides.len(),
            2,
            "{:?}",
            sides.iter().map(|(m, _)| m).collect::<Vec<_>>()
        );
        let lock = Lockable::merge_forks(sides);
        assert_eq!(lock.packages.len(), 2);
        let text = write_lock(&roots, &lock);
        assert!(text.contains("resolution-markers = ["), "{text}");
        assert!(
            text.contains("dependencies = [{ name = \"fk\", version = \"1.0\", marker = \"python_version < '3.13'\" }, { name = \"fk\", version = \"2.0\", marker = \"python_version >= '3.13'\" }]"),
            "{text}"
        );
        assert_eq!(
            negate_marker("sys_platform == 'win32'\tand  python_version >= '3.12'").unwrap(),
            "(sys_platform != 'win32') or (python_version < '3.12')"
        );
        let parsed = crate::lock::Lock::parse(&text).unwrap();
        let names = |v: [u32; 3]| -> Vec<String> {
            crate::native::plan(&parsed, &MarkerEnv::for_target("linux", "x86_64", v), &[])
                .unwrap()
                .iter()
                .map(|p| format!("{} {}", p.name, p.version))
                .collect()
        };
        assert_eq!(names([3, 12, 0]), vec!["app 0.1.0", "fk 1.0"]);
        assert_eq!(names([3, 13, 0]), vec!["app 0.1.0", "fk 2.0"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wheelhouse with pure-Python wheels `name` at each of `versions`.
    fn wheelhouse(dir: &std::path::Path, name: &str, versions: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for v in versions {
            let meta = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {v}\n");
            let whl = stored_zip(&[
                (&format!("{name}-{v}.dist-info/METADATA"), meta.as_bytes()),
                (
                    &format!("{name}-{v}.dist-info/WHEEL"),
                    b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n",
                ),
            ]);
            std::fs::write(dir.join(format!("{name}-{v}-py3-none-any.whl")), whl).unwrap();
        }
    }

    #[test]
    fn profiles_and_forks_share_one_lock() {
        let dir = std::env::temp_dir().join(format!("vivar-forkprof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        wheelhouse(&dir, "fk", &["1.0", "2.0"]);
        wheelhouse(&dir, "variant", &["1.0", "2.0"]);
        let index = Index::new(Some(dir.to_str().unwrap()));
        // The base forks on the Python version; two conflicting groups pin
        // another package differently.
        let roots = Roots {
            name: "app".into(),
            version: "0.1.0".into(),
            requires_python: Some(">=3.12".into()),
            dependencies: vec![
                Requirement::parse("fk<2; python_version < '3.13'").unwrap(),
                Requirement::parse("fk>=2; python_version >= '3.13'").unwrap(),
            ],
            groups: BTreeMap::from([
                (
                    "slow".to_owned(),
                    vec![Requirement::parse("variant==1.0").unwrap()],
                ),
                (
                    "fast".to_owned(),
                    vec![Requirement::parse("variant==2.0").unwrap()],
                ),
            ]),
            ..Default::default()
        };
        let profiles = [
            Selector::Group("slow".into()),
            Selector::Group("fast".into()),
        ];
        let resolve = |roots: &Roots| -> Lockable {
            let build = |restrictions: &[(String, bool)]| -> Result<Resolver<'_>> {
                let mut r = Resolver::new(&index, [3, 12, 0], true).requires_python(Some(">=3.12"));
                for (m, holds) in restrictions {
                    r = r.restrict(m, *holds);
                }
                Ok(r)
            };
            Lockable::merge_forks(resolve_with_forks(&build, roots, Vec::new()).unwrap())
        };
        let mut base_roots = roots.clone();
        base_roots.retain_selectors(&profiles, false);
        assert!(base_roots.groups.is_empty());
        let base = resolve(&base_roots);
        assert_eq!(base.packages.len(), 2, "the base forks on fk");
        let mut sides = Vec::new();
        for sel in &profiles {
            let mut pr = base_roots.clone();
            pr.restore_selector(&roots, sel);
            sides.push((sel.clone(), resolve(&pr)));
        }
        let lock = Lockable::merge(base, sides).unwrap();
        assert_eq!(lock.packages.iter().filter(|p| p.name == "fk").count(), 2);
        assert_eq!(
            lock.packages.iter().filter(|p| p.name == "variant").count(),
            2
        );
        assert!(lock.fork_markers.len() == 2, "{:?}", lock.fork_markers);
        let text = write_lock(&roots, &lock);
        assert!(text.contains("resolution-markers = ["), "{text}");
        assert!(
            text.contains("dependencies = [{ name = \"fk\", version = \"1.0\", marker = \"python_version < '3.13'\" }, { name = \"fk\", version = \"2.0\", marker = \"python_version >= '3.13'\" }]"),
            "{text}"
        );
        assert!(
            text.contains("slow = [{ name = \"variant\", version = \"1.0\" }]")
                && text.contains("fast = [{ name = \"variant\", version = \"2.0\" }]"),
            "{text}"
        );
        let parsed = crate::lock::Lock::parse(&text).unwrap();
        let names = |v: [u32; 3], groups: &[&str]| -> Vec<String> {
            crate::native::plan(&parsed, &MarkerEnv::current(v), groups)
                .unwrap()
                .iter()
                .map(|p| format!("{} {}", p.name, p.version))
                .collect()
        };
        assert_eq!(
            names([3, 12, 0], &["slow"]),
            vec!["app 0.1.0", "fk 1.0", "variant 1.0"]
        );
        assert_eq!(
            names([3, 13, 0], &["fast"]),
            vec!["app 0.1.0", "fk 2.0", "variant 2.0"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requires_dist_names_declared_sources() {
        let reg = Source::Registry("https://pypi.org/simple".into());
        let roots = Roots {
            name: "app".into(),
            version: "0.1.0".into(),
            dependencies: vec![
                Requirement::parse("torch").unwrap(),
                Requirement::parse("minipkg").unwrap(),
                Requirement::parse("gitpkg").unwrap(),
                Requirement::parse("lib").unwrap(),
            ],
            groups: BTreeMap::from([(
                "cu124".to_owned(),
                vec![Requirement::parse("torchvision").unwrap()],
            )]),
            sources: vec![
                RootSource {
                    name: "torch".into(),
                    source: Source::Registry("https://download.pytorch.org/whl/cpu".into()),
                    selector: None,
                    marker: Some("sys_platform == 'linux'".into()),
                },
                RootSource {
                    name: "torch".into(),
                    source: Source::Registry("https://download.pytorch.org/whl/cu124".into()),
                    selector: None,
                    marker: Some("sys_platform != 'linux'".into()),
                },
                RootSource {
                    name: "minipkg".into(),
                    source: Source::Path("wheels/minipkg-1.0-py3-none-any.whl".into()),
                    selector: None,
                    marker: None,
                },
                RootSource {
                    name: "gitpkg".into(),
                    source: Source::Git {
                        url: "https://h/r.git".into(),
                        reference: Some(GitRef::Tag("v1".into())),
                        commit: String::new(),
                        subdirectory: Some("sub".into()),
                    },
                    selector: None,
                    marker: None,
                },
                RootSource {
                    name: "lib".into(),
                    source: Source::Editable("../lib".into()),
                    selector: None,
                    marker: None,
                },
                RootSource {
                    name: "torchvision".into(),
                    source: Source::Registry("https://download.pytorch.org/whl/cu124".into()),
                    selector: Some(Selector::Group("cu124".into())),
                    marker: None,
                },
            ],
            ..Default::default()
        };
        let mini = ResolvedPackage {
            name: "minipkg".into(),
            version: Version::parse("1.0").unwrap(),
            files: vec![File {
                filename: "minipkg-1.0-py3-none-any.whl".into(),
                url: "file:///x/wheels/minipkg-1.0-py3-none-any.whl".into(),
                sha256: Some("ab".repeat(32)),
                requires_python: None,
                yanked: false,
                has_metadata: false,
            }],
            dependencies: vec![],
            source: Source::Path("wheels/minipkg-1.0-py3-none-any.whl".into()),
            resolution_markers: Vec::new(),
        };
        let text = write_lock(
            &roots,
            &Lockable::single(Resolution {
                packages: vec![mini],
                pins: BTreeMap::from([(
                    "minipkg".to_owned(),
                    Pin {
                        version: Version::parse("1.0").unwrap(),
                        source: reg.clone(),
                    },
                )]),
            }),
        );
        for needle in [
            "{ name = \"torch\", index = \"https://download.pytorch.org/whl/cpu\", marker = \"sys_platform == 'linux'\" }",
            "{ name = \"torch\", index = \"https://download.pytorch.org/whl/cu124\", marker = \"sys_platform != 'linux'\" }",
            "{ name = \"minipkg\", path = \"wheels/minipkg-1.0-py3-none-any.whl\" }",
            "{ name = \"gitpkg\", git = \"https://h/r.git?subdirectory=sub&tag=v1\" }",
            "{ name = \"lib\", editable = \"../lib\" }",
            "{ name = \"torchvision\", index = \"https://download.pytorch.org/whl/cu124\", conflict = { package = \"app\", group = \"cu124\" } }",
            "source = { path = \"wheels/minipkg-1.0-py3-none-any.whl\" }",
            "wheels = [{ filename = \"minipkg-1.0-py3-none-any.whl\", hash = \"sha256:abab",
        ] {
            assert!(text.contains(needle), "{needle} missing from\n{text}");
        }
        let parsed = crate::lock::Lock::parse(&text).unwrap();
        let p = parsed.get("minipkg").unwrap();
        assert_eq!(p.source_kind, "path");
        assert_eq!(p.wheels[0].filename(), "minipkg-1.0-py3-none-any.whl");
        assert!(p.hashed);
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
                Edge::plain(Requirement::parse("urllib3<3").unwrap()),
                Edge::plain(Requirement::parse("PySocks>=1.5.6; extra == 'socks'").unwrap()),
            ],
            source: Source::Registry("https://pypi.org/simple".into()),
            resolution_markers: Vec::new(),
        };
        let direct = ResolvedPackage {
            name: "minipkg".into(),
            version: Version::parse("1.0").unwrap(),
            files: vec![],
            dependencies: vec![],
            source: Source::Git {
                url: "https://example.com/minipkg.git".into(),
                reference: Some(GitRef::Branch("main".into())),
                commit: "abc123".into(),
                subdirectory: Some("sub".into()),
            },
            resolution_markers: Vec::new(),
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
        assert_eq!(
            git_parts("git+https://user@example.com/r.git@feature/test"),
            (
                "https://user@example.com/r.git".into(),
                Some("feature/test".into())
            )
        );
        let text = write_lock(
            &roots,
            &Lockable::single(Resolution {
                packages: vec![direct],
                pins: BTreeMap::new(),
            }),
        );
        assert!(
            text.contains("source = { git = \"https://example.com/minipkg.git?subdirectory=sub&branch=main#abc123\" }"),
            "{text}"
        );
        let text = write_lock(
            &roots,
            &Lockable::single(Resolution {
                packages: vec![pkg],
                pins: BTreeMap::new(),
            }),
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
