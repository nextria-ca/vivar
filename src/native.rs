//! The in-house backend: resolves with `resolve.rs`, installs environments
//! straight from `uv.lock` (wheels only, every artifact hash-checked), and
//! builds through PEP 517, with no uv process. Projects using uv-specific
//! features (workspaces, per-package sources, profiles) are refused rather
//! than resolved differently.

use crate::backend::{Backend, Change};
use crate::index::Index;
use crate::index::{File, Metadata};
use crate::lock::{Lock, Package};
use crate::pep::{eval_marker, MarkerEnv, Requirement, Version};
use crate::project::{self, Policy, Project};
use crate::resolve::{
    resolve_with_forks, write_lock, Direct, Fetch, ForkNeeded, Lockable, Resolution, Resolver,
    Roots, Source as ResolvedSource,
};
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
        let root = project.venv_root().to_path_buf();
        // In a workspace the lock belongs to the root and covers every member.
        let ws_root;
        let project = match project.workspace.as_deref() {
            Some(_) => {
                ws_root = Project::require(&root)?;
                &ws_root
            }
            None => project,
        };
        let members = project.members()?;
        for p in std::iter::once(project).chain(members.iter()) {
            let unsupported = p.uv_only_features();
            if !unsupported.is_empty() {
                bail!(
                    "{} uses {}, which vivar's resolver does not handle yet; install uv for it",
                    p.root.join(project::PYPROJECT).display(),
                    unsupported.join(" and ")
                );
            }
        }
        let mut roots = roots_of(project)?;
        // Members: roots of their own, and their Python requirements narrow
        // the lock's. Their sources and indexes join the root's (root first).
        let mut rp_clauses: Vec<String> = roots
            .requires_python
            .iter()
            .flat_map(|r| r.split(',').map(|c| c.trim().to_owned()))
            .filter(|c| !c.is_empty())
            .collect();
        let mut sources = project.uv_sources();
        let mut indexes = project.uv_indexes();
        let mut workspace_bound = project.uv_workspace_sources();
        for m in &members {
            let rel = m
                .root
                .strip_prefix(&project.root)
                .unwrap_or(&m.root)
                .display()
                .to_string()
                .replace('\\', "/");
            let mr = roots_of(m)?;
            for c in mr.requires_python.iter().flat_map(|r| r.split(',')) {
                let c = c.trim().to_owned();
                if !c.is_empty() && !rp_clauses.contains(&c) {
                    rp_clauses.push(c);
                }
            }
            for (pkg, entries) in m.uv_sources() {
                sources.entry(pkg).or_insert(entries);
            }
            for i in m.uv_indexes() {
                // Another name for a URL already declared stays: sources refer to names.
                if !indexes.iter().any(|x| x.url == i.url && x.name == i.name) {
                    indexes.push(i);
                }
            }
            workspace_bound.extend(m.uv_workspace_sources());
            roots.members.push(crate::resolve::Member {
                roots: mr,
                path: rel,
            });
        }
        if !rp_clauses.is_empty() {
            roots.requires_python = Some(rp_clauses.join(", "));
        }
        // `{ workspace = true }` must name a member (or the root); otherwise the
        // package would quietly come from an index instead.
        {
            let local: BTreeSet<String> = std::iter::once(project::normalize(&roots.name))
                .chain(
                    roots
                        .members
                        .iter()
                        .map(|m| project::normalize(&m.roots.name)),
                )
                .collect();
            if let Some(missing) = workspace_bound.iter().find(|p| !local.contains(*p)) {
                bail!("[tool.uv.sources] binds {missing} to the workspace, but no member is named {missing}");
            }
        }
        let machine = python_floor(project)?;
        // The lock's floor: the highest lower bound any root declares.
        let floor = std::iter::once(project)
            .chain(members.iter())
            .filter_map(declared_floor)
            .max()
            .unwrap_or(machine);
        let old: BTreeMap<String, String> = Lock::read(&root)
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

        // Indexes: a policy wheelhouse is the only source, full stop. Otherwise
        // the policy index, else the project's `default = true` index, else
        // PyPI is the index of last resort; non-explicit project indexes are
        // searched before it; `[tool.uv.sources]` routes packages to one.
        let wheelhouse = self.wheelhouse_dir().is_some();
        if wheelhouse {
            indexes.clear();
            sources.clear();
        }
        let main_url = self
            .index_url
            .clone()
            .or_else(|| indexes.iter().find(|i| i.default).map(|i| i.url.clone()));
        let index = Index::new(main_url.as_deref());
        let mut index_urls = vec![index.url().to_owned()];
        index_urls.extend(indexes.iter().map(|i| i.url.clone()));
        let builder = Builder {
            backend: self,
            series: format!("{}.{}", machine[0], machine[1]),
            index_urls: index_urls.clone(),
        };
        let index_url = |name: &str| -> Result<String> {
            indexes
                .iter()
                .find(|i| i.name.as_deref() == Some(name))
                .map(|i| i.url.clone())
                .ok_or_else(|| anyhow!("[tool.uv.sources] names index {name:?}, which [[tool.uv.index]] does not declare"))
        };
        let conflicts: Vec<Vec<String>> = project.conflict_groups();
        let profile_groups: Vec<String> = conflicts.iter().flatten().cloned().collect();
        // The route for a package when resolving `group` (None: the base or a
        // single resolution). A group-specific entry applies inside its group,
        // or on its own when it is the only one and nothing conflicts yet
        // (the first profile, before a second declares the conflict).
        let routes_for = |group: Option<&str>| -> Result<Vec<(String, Index)>> {
            let mut out = Vec::new();
            for (pkg, entries) in &sources {
                let exact = entries
                    .iter()
                    .find(|(_, g)| g.is_some() && g.as_deref() == group);
                let plain = entries.iter().find(|(_, g)| g.is_none());
                let grouped: BTreeSet<&String> = entries
                    .iter()
                    .filter_map(|(i, g)| g.as_ref().map(|_| i))
                    .collect();
                let pick = match (exact, plain) {
                    (Some(e), _) => Some(e),
                    (None, Some(p)) => Some(p),
                    (None, None) if group.is_none() && profile_groups.is_empty() => {
                        if grouped.len() > 1 {
                            bail!("{pkg} is routed to several indexes by group but no conflicts are declared between those groups; `vivar profile add` declares them");
                        }
                        entries.first()
                    }
                    (None, None) => None,
                };
                if let Some((idx, _)) = pick {
                    out.push((pkg.clone(), Index::new(Some(&index_url(idx)?))));
                }
            }
            Ok(out)
        };
        let defaults = || -> Vec<Index> {
            indexes
                .iter()
                .filter(|i| !i.explicit && !i.default)
                .map(|i| Index::new(Some(&i.url)))
                .collect()
        };
        let build = |roots: &Roots,
                     group: Option<&str>,
                     prefer: BTreeMap<String, Version>,
                     restrictions: &[(String, bool)]|
         -> Result<Resolver<'_>> {
            let mut r = Resolver::new(&index, floor, self.no_build)
                .fetch(&builder)
                .machine(machine)
                .requires_python(roots.requires_python.as_deref())
                .routes(routes_for(group)?)
                .defaults(defaults())
                .prefer(prefer)
                .upgrade(upgrade, upgrade_all);
            for (m, holds) in restrictions {
                r = r.restrict(m, *holds);
            }
            Ok(r)
        };
        let resolve = |roots: &Roots,
                       group: Option<&str>,
                       prefer: BTreeMap<String, Version>|
         -> Result<Resolution> {
            build(roots, group, prefer, &[])?
                .resolve(roots)
                .map_err(|e| match e.downcast_ref::<ForkNeeded>() {
                    Some(f) => anyhow!("{f}; a lock with profiles cannot also fork (uv can)"),
                    None => e,
                })
        };
        // Without profiles the lock may fork on a marker where the graph needs
        // it. Profiles (conflicting groups) resolve one at a time on top of
        // the base, each preferring what the base chose, and merge into one lock.
        let lockable = if profile_groups.is_empty() {
            let prefer_ref = &prefer;
            let sides = resolve_with_forks(
                &|restrictions| build(&roots, None, prefer_ref.clone(), restrictions),
                &roots,
                Vec::new(),
            )?;
            if sides.len() == 1 {
                Lockable::single(sides.into_iter().next().map(|(_, r)| r).unwrap())
            } else {
                println!(
                    "lock forks on {}",
                    sides
                        .iter()
                        .map(|(m, _)| format!("`{m}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                Lockable::merge_forks(sides)
            }
        } else {
            let mut base_roots = roots.clone();
            base_roots.retain_groups(&profile_groups, false);
            let base = resolve(&base_roots, None, prefer.clone())?;
            let mut profiles = Vec::new();
            for g in &profile_groups {
                let declared = roots.groups.contains_key(g)
                    || roots.members.iter().any(|m| m.roots.groups.contains_key(g));
                if !declared {
                    continue;
                }
                let mut pr = base_roots.clone();
                pr.restore_group(&roots, g);
                let mut keep = prefer.clone();
                keep.extend(base.assigned.clone());
                profiles.push((g.clone(), resolve(&pr, Some(g), keep)?));
            }
            Lockable::merge(base, profiles)?
        };
        let mut new: BTreeMap<String, String> = BTreeMap::new();
        for p in &lockable.packages {
            new.entry(p.name.clone())
                .and_modify(|v| {
                    v.push_str(", ");
                    v.push_str(&p.version.to_string());
                })
                .or_insert_with(|| p.version.to_string());
        }
        if !dry_run {
            let text = write_lock(&roots, &lockable);
            fs::write(root.join(crate::lock::LOCKFILE), text)?;
            println!(
                "resolved {} package(s) (native resolver, wheels only)",
                lockable.packages.len()
            );
        }
        Ok((old, new))
    }
}

/// A project's own requirements as resolver roots.
fn roots_of(project: &Project) -> Result<Roots> {
    let (deps, extras, groups) = project.requirement_sections();
    let parse_all = |v: &[String]| -> Result<Vec<Requirement>> {
        v.iter().map(|r| Requirement::parse(r)).collect()
    };
    Ok(Roots {
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
        members: Vec::new(),
    })
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

/// The Python this machine installs with, when the project says: the env,
/// else the pin, else the `requires-python` floor. No lookups, no installs.
fn known_python(project: &Project) -> Option<[u32; 3]> {
    let root = project.venv_root();
    if let Ok(v) = env_version(&root.join(".venv")) {
        return Some(v);
    }
    if let Ok(pin) = fs::read_to_string(root.join(".python-version")) {
        let mut it = pin.trim().split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        return Some([
            it.next().unwrap_or(3),
            it.next().unwrap_or(12),
            it.next().unwrap_or(0),
        ]);
    }
    declared_floor(project)
}

/// `known_python`, else the newest installed build (installing one if needed).
fn python_floor(project: &Project) -> Result<[u32; 3]> {
    match known_python(project) {
        Some(v) => Ok(v),
        None => Ok(venv::interpreter(None)?.version),
    }
}

/// The differences between two name → version maps, as the lock would change.
fn changes_between(old: &BTreeMap<String, String>, new: &BTreeMap<String, String>) -> Vec<Change> {
    let mut changes = Vec::new();
    for (name, version) in new {
        match old.get(name) {
            Some(o) if o == version => {}
            other => changes.push(Change {
                name: name.clone(),
                from: other.cloned(),
                to: Some(version.clone()),
            }),
        }
    }
    for (name, version) in old.iter().filter(|(n, _)| !new.contains_key(*n)) {
        changes.push(Change {
            name: name.clone(),
            from: Some(version.clone()),
            to: None,
        });
    }
    changes
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
                .filter(|p| dep.source.as_ref().is_none_or(|s| *s == p.source))
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

/// A bare file name from a URL or archive listing, fit to join onto a cache
/// directory: nothing empty, no `.`/`..`, no separators.
fn safe_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\'])
}

/// `https://host/x.whl#sha256=<64 hex>` → (`https://host/x.whl`, the digest).
/// Anything else after `sha256=` is refused: the digest names a cache path.
fn split_hash_fragment(url: &str) -> Result<(String, Option<String>)> {
    match url.split_once('#') {
        Some((bare, frag)) => match frag.strip_prefix("sha256=") {
            Some(h) if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) => {
                Ok((bare.to_owned(), Some(h.to_lowercase())))
            }
            Some(h) => bail!("{url}: `#sha256={h}` is not a 64-character hex digest"),
            None => Ok((bare.to_owned(), None)),
        },
        None => Ok((url.to_owned(), None)),
    }
}

/// Where wheels come from: the configured index (credentials go to its host
/// only, never to whatever a lock names) and an optional wheelhouse
/// directory that is tried first.
struct Source<'a> {
    /// Every configured index (policy's and `[[tool.uv.index]]`): credentials
    /// go to these hosts and no other.
    index_urls: Vec<String>,
    wheelhouse_dir: Option<&'a Path>,
    /// Builds wheels from source distributions and git checkouts, for this
    /// machine only (a cross-target layout gets none).
    builder: Option<Builder<'a>>,
}

/// The native backend's builder: PEP 517 in a throwaway environment on a
/// Python of `series`, with results cached under `~/.cache/vivar/built`.
struct Builder<'a> {
    backend: &'a NativeBackend,
    series: String,
    /// Every configured index: credentials go to these hosts only.
    index_urls: Vec<String>,
}

/// Fetches the wheel for `pkg` on `target`, hash-checked, from the cache or
/// the URL the lock records.
fn fetch_wheel(pkg: &Package, target: &wheelhouse::Target, source: &Source) -> Result<PathBuf> {
    if pkg.source_kind == "git" {
        let builder = source.builder.as_ref().ok_or_else(|| {
            anyhow!(
                "{} comes from git and cannot be built for another machine",
                pkg.name
            )
        })?;
        return builder.wheel_from_git(&pkg.name, &pkg.source);
    }
    let (artifact, is_wheel) = target.pick(pkg).ok_or_else(|| {
        anyhow!(
            "{} {} has no artifact for this platform in uv.lock",
            pkg.name,
            pkg.version
        )
    })?;
    if !is_wheel {
        let builder = source.builder.as_ref().ok_or_else(|| {
            anyhow!(
                "{} {} is only available as a source distribution, which cannot be built for another machine",
                pkg.name,
                pkg.version
            )
        })?;
        if builder.backend.no_build {
            bail!(
                "{} {} is only available as a source distribution and policy requires wheels",
                pkg.name,
                pkg.version
            );
        }
        let sdist = File {
            filename: artifact.filename(),
            url: artifact.url.clone(),
            sha256: artifact.sha256().map(str::to_owned),
            requires_python: None,
            yanked: false,
            has_metadata: false,
        };
        return builder.wheel_from_sdist(&pkg.name, &sdist);
    }
    let name = artifact.filename();
    if !safe_file_name(&name) {
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
    let url = if source
        .index_urls
        .iter()
        .any(|u| crate::index::same_origin(&artifact.url, u))
    {
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
        let keep = wanted.iter().any(|p| same_install(p, dist));
        if !keep && (want_names.contains(dist.name.as_str()) || !dist.name.is_empty()) {
            install::uninstall(dist)?;
            removed += 1;
        }
    }
    let present = install::installed(&layout.site)?;
    let mut installed = 0;
    for pkg in wanted {
        if present.iter().any(|d| same_install(pkg, d)) {
            continue;
        }
        match pkg.source_kind.as_str() {
            "registry" | "url" | "git" => {
                let wheel = fetch_wheel(pkg, target, source)?;
                install::install_wheel_from(&wheel, layout, direct_url_of(pkg).as_deref())?;
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

/// The PEP 610 record a lock package installs with: none for a registry
/// package, the URL and hash for a direct one, the repository and commit for
/// git. Same name and version from another source is a different install.
fn direct_url_of(pkg: &Package) -> Option<String> {
    match pkg.source_kind.as_str() {
        "url" => {
            let hash = pkg
                .wheels
                .first()
                .or(pkg.sdist.as_ref())
                .and_then(|a| a.sha256())
                .map(|h| format!(",\"archive_info\":{{\"hash\":\"sha256={h}\"}}"))
                .unwrap_or_default();
            Some(format!(
                "{{\"url\":{}{hash}}}",
                crate::json::quote(&pkg.source)
            ))
        }
        "git" => {
            let (repo_with_rev, commit) = pkg.source.split_once('#')?;
            let (repo, query) = repo_with_rev
                .split_once('?')
                .map(|(r, q)| (r, Some(q)))
                .unwrap_or((repo_with_rev, None));
            let rev = query
                .and_then(|q| q.strip_prefix("rev="))
                .map(|r| format!(",\"requested_revision\":{}", crate::json::quote(r)))
                .unwrap_or_default();
            Some(format!(
                "{{\"url\":{},\"vcs_info\":{{\"vcs\":\"git\",\"commit_id\":{}{rev}}}}}",
                crate::json::quote(repo),
                crate::json::quote(commit)
            ))
        }
        _ => None,
    }
}

/// The parts of a PEP 610 record that identify a source: the URL, the
/// archive hash, the VCS commit. Records written by pip, uv or vivar differ
/// in whitespace, order and extras; these do not.
/// (url, archive hash, vcs commit, subdirectory)
type SourceIdentity = (String, Option<String>, Option<String>, Option<String>);

fn direct_identity(json: &str) -> Option<SourceIdentity> {
    let doc = crate::json::parse(json).ok()?;
    let url = doc.get("url")?.as_str()?.to_owned();
    // A build from a repository subdirectory is another package than the
    // root's, even at the same commit; vivar never writes one itself.
    let subdirectory = doc
        .get("subdirectory")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let hash = doc
        .get("archive_info")
        .and_then(|a| a.get("hash"))
        .and_then(|h| h.as_str())
        .map(|h| h.trim_start_matches("sha256=").to_lowercase());
    let commit = doc
        .get("vcs_info")
        .and_then(|v| v.get("commit_id"))
        .and_then(|c| c.as_str())
        .map(str::to_owned);
    Some((url, hash, commit, subdirectory))
}

/// Whether what is installed is exactly this lock package: name, version and
/// (for direct sources) the same URL, hash or commit.
fn same_install(pkg: &Package, dist: &install::Installed) -> bool {
    if pkg.name != dist.name || pkg.version != dist.version {
        return false;
    }
    match (direct_url_of(pkg), &dist.direct_url) {
        (None, None) => true,
        (Some(want), Some(have)) => direct_identity(&want) == direct_identity(have),
        _ => false,
    }
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

    /// The policy index plus the project's `[[tool.uv.index]]` URLs, when a
    /// project is at hand (sync runs inside one; tool envs do not).
    fn index_urls_with(&self, main: &str) -> Vec<String> {
        let mut urls = vec![main.to_owned()];
        if let Ok(cwd) = std::env::current_dir() {
            if let Ok(project) = Project::require(&cwd) {
                urls.extend(project.uv_indexes().into_iter().map(|i| i.url));
            }
        }
        urls
    }

    /// Runs `f` with this backend's artifact source.
    fn with_source<T>(
        &self,
        series: Option<&str>,
        f: impl FnOnce(&Source) -> Result<T>,
    ) -> Result<T> {
        let index = self.index();
        let wheelhouse = self.wheelhouse_dir();
        f(&Source {
            index_urls: self.index_urls_with(index.url()),
            wheelhouse_dir: wheelhouse.as_deref(),
            builder: series.map(|s| Builder {
                backend: self,
                series: s.to_owned(),
                index_urls: self.index_urls_with(index.url()),
            }),
        })
    }

    fn sync_into(&self, lock_root: &Path, venv_dir: &Path, groups: &[&str]) -> Result<()> {
        let lock = Lock::read(lock_root)?;
        let version = env_version(venv_dir)?;
        let env = MarkerEnv::current(version);
        let wanted = plan(&lock, &env, groups)?;
        let series = format!("{}.{}", version[0], version[1]);
        let target = wheelhouse::Target::new(None, &series)?;
        let layout = install::Layout::venv(
            venv_dir,
            &format!("{}.{}", version[0], version[1]),
            cfg!(windows),
        );
        let (installed, removed) = self.with_source(Some(&series), |source| {
            apply(lock_root, &wanted, &layout, &target, source)
        })?;
        println!(
            "native sync: {} package(s) installed, {} removed, {} in the environment",
            installed,
            removed,
            wanted.len().saturating_sub(1)
        );
        Ok(())
    }
}

impl NativeBackend {
    /// Runs a project's PEP 517 backend in a throwaway environment on Python
    /// `series`, building `modes` (`sdist`, `wheel`) into `dist`; returns the
    /// built file names.
    fn pep517(
        &self,
        root: &Path,
        dist: &Path,
        series: &str,
        modes: &[&str],
    ) -> Result<Vec<String>> {
        let doc: toml_edit::DocumentMut = fs::read_to_string(root.join(project::PYPROJECT))
            .unwrap_or_default()
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
        let canonical_root = fs::canonicalize(root)?;
        for p in &backend_path {
            let full = fs::canonicalize(root.join(p))
                .with_context(|| format!("build-system.backend-path entry {p:?}"))?;
            if Path::new(p).is_absolute() || !full.starts_with(&canonical_root) {
                bail!("build-system.backend-path must stay inside the project ({p:?} does not)");
            }
        }

        let python = venv::interpreter(Some(series))?;
        let build_env = std::env::temp_dir().join(format!(
            "vivar-build-{}-{}",
            std::process::id(),
            BUILD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&build_env);
        venv::create(&build_env, &python, "build")?;
        let result = (|| -> Result<Vec<String>> {
            let py = venv::python(&build_env);
            if !requires.is_empty() {
                self.install(&py, &requires)?;
            }
            fs::create_dir_all(dist)?;
            // Build tools the backend shells out to (ninja, meson, cython)
            // live in the build env's bin: it goes first on PATH.
            let mut dirs = vec![build_env.join(venv::BIN)];
            if let Some(p) = std::env::var_os("PATH") {
                dirs.extend(std::env::split_paths(&p));
            }
            let path = std::env::join_paths(dirs).context("PATH")?;
            let hook = |mode: &str| -> Result<String> {
                let mut cmd = std::process::Command::new(&py);
                cmd.current_dir(root)
                    .env("PATH", &path)
                    .args(["-c", BUILD_HOOK, mode, &backend])
                    .arg(dist)
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
            let mut built = Vec::new();
            for mode in modes {
                let name = hook(mode)?;
                if name.is_empty() {
                    bail!("the build backend `{backend}` returned no {mode} name");
                }
                built.push(name);
            }
            Ok(built)
        })();
        let _ = fs::remove_dir_all(&build_env);
        result
    }
}

static BUILD_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Builder<'_> {
    /// `~/.cache/vivar/built/<key>/`: one directory per source, holding the wheel.
    fn built_dir(key: &str) -> Result<PathBuf> {
        Ok(dirs::cache()?.join("built").join(key))
    }

    fn cached_wheel(dir: &Path) -> Option<PathBuf> {
        fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "whl"))
    }

    /// Downloads a source distribution (hash-checked when a hash is known,
    /// computed otherwise) and returns its path and hash. The file is written
    /// whole, then renamed: a failed download leaves nothing to be reused.
    fn fetch_sdist(&self, file: &File) -> Result<(PathBuf, String)> {
        if !safe_file_name(&file.filename) {
            bail!(
                "refusing artifact with unsafe file name {:?}",
                file.filename
            );
        }
        let key = file.sha256.clone().unwrap_or_else(|| {
            let mut h = sha256::Sha256::new();
            h.update(file.url.as_bytes());
            sha256::hex(&h.finish())
        });
        let dir = dirs::cache()?.join("sdists").join(&key[..16]);
        fs::create_dir_all(&dir)?;
        let dest = dir.join(&file.filename);
        if !dest.is_file() {
            println!("downloading {}", file.filename);
            let url = if self
                .index_urls
                .iter()
                .any(|u| crate::index::same_origin(&file.url, u))
            {
                crate::backend::with_credentials(&file.url)
            } else {
                file.url.clone()
            };
            let part = dir.join(format!("{}.part-{}", file.filename, std::process::id()));
            let fetched = upgrade::fetch_to(&url, &part);
            if let Err(e) = fetched {
                let _ = fs::remove_file(&part);
                return Err(e);
            }
            fs::rename(&part, &dest)?;
        }
        let actual = sha256::file(&dest)?;
        if let Some(expected) = &file.sha256 {
            if actual != *expected {
                let _ = fs::remove_file(&dest);
                bail!(
                    "{} does not match its hash (got {actual}, expected {expected})",
                    file.filename
                );
            }
        }
        Ok((dest, actual))
    }

    /// Cache key for a build: the source's identity plus what it was built
    /// for, since a compiled wheel is good for one Python and platform.
    fn build_key(&self, identity: &str) -> Result<String> {
        let target = wheelhouse::Target::new(None, &self.series)?;
        let mut h = sha256::Sha256::new();
        h.update(identity.as_bytes());
        Ok(format!(
            "{}-{}",
            &sha256::hex(&h.finish())[..16],
            target.label()
        ))
    }

    /// A cached wheel is reused only when it fits this target.
    fn fitting_cached_wheel(&self, dir: &Path) -> Option<PathBuf> {
        let target = wheelhouse::Target::new(None, &self.series).ok()?;
        Self::cached_wheel(dir).filter(|w| {
            w.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| target.score(n))
                .is_some()
        })
    }

    /// Unpacks a `.tar.gz`/`.zip` source distribution into `into` and returns
    /// the project directory inside it.
    fn unpack_sdist(archive: &Path, into: &Path) -> Result<PathBuf> {
        let _ = fs::remove_dir_all(into);
        fs::create_dir_all(into)?;
        let name = archive.file_name().unwrap_or_default().to_string_lossy();
        if name.ends_with(".zip") {
            let data = fs::read(archive)?;
            for entry in crate::zip::entries(&data)? {
                if entry.name.ends_with('/') {
                    continue;
                }
                let rel = Path::new(&entry.name);
                if rel.is_absolute()
                    || rel
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    bail!("{name} contains an unsafe path {:?}", entry.name);
                }
                let out = into.join(rel);
                fs::create_dir_all(out.parent().context("entry parent")?)?;
                fs::write(out, crate::zip::read(&data, &entry)?)?;
            }
        } else {
            let status = std::process::Command::new("tar")
                .arg("-xzf")
                .arg(archive)
                .arg("-C")
                .arg(into)
                .status()
                .context("running tar")?;
            if !status.success() {
                bail!("tar could not unpack {name}");
            }
        }
        // The one directory the archive holds, or the archive root itself.
        let mut dirs: Vec<PathBuf> = fs::read_dir(into)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        match dirs.len() {
            1 => Ok(dirs.remove(0)),
            _ => Ok(into.to_path_buf()),
        }
    }

    /// Builds (or finds the cached) wheel for a source tree.
    fn wheel_from_tree(&self, name: &str, src: &Path, key: &str) -> Result<PathBuf> {
        if self.backend.no_build {
            bail!("{name} would have to be built from source and policy requires wheels");
        }
        let out = Self::built_dir(key)?;
        if let Some(w) = self.fitting_cached_wheel(&out) {
            return Ok(w);
        }
        fs::create_dir_all(&out)?;
        println!("building {name} from source");
        let built = self.backend.pep517(src, &out, &self.series, &["wheel"])?;
        let wheel = out.join(built.first().context("no wheel built")?);
        if !wheel.is_file() {
            bail!(
                "the build backend reported {} but did not write it",
                wheel.display()
            );
        }
        Ok(wheel)
    }

    /// A git checkout at a commit: `source` is the lock's
    /// `https://host/repo?rev=<rev>#<commit>`.
    fn wheel_from_git(&self, name: &str, source: &str) -> Result<PathBuf> {
        let (repo_with_rev, commit) = source
            .split_once('#')
            .ok_or_else(|| anyhow!("{name}: git source {source:?} has no commit"))?;
        let repo = repo_with_rev.split('?').next().unwrap_or(repo_with_rev);
        if self.backend.no_build {
            bail!("{name} comes from git and would have to be built; policy requires wheels");
        }
        let key = self.build_key(&format!("{repo}#{commit}"))?;
        if let Some(w) = self.fitting_cached_wheel(&Self::built_dir(&key)?) {
            return Ok(w);
        }
        let src = Self::clone(repo, Some(commit))?.0;
        let wheel = self.wheel_from_tree(name, &src, &key);
        let _ = fs::remove_dir_all(&src);
        wheel
    }

    /// Clones `repo` at `rev` (a branch, tag or commit; HEAD when none) into a
    /// scratch directory; returns it and the commit it ended up at.
    fn clone(repo: &str, rev: Option<&str>) -> Result<(PathBuf, String)> {
        let dir = std::env::temp_dir().join(format!(
            "vivar-git-{}-{}",
            std::process::id(),
            BUILD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let git = |args: &[&str], cwd: Option<&Path>| -> Result<String> {
            let mut cmd = std::process::Command::new("git");
            cmd.args(args);
            if let Some(c) = cwd {
                cmd.current_dir(c);
            }
            let out = cmd.output().context("running git (is it installed?)")?;
            if !out.status.success() {
                bail!(
                    "git {} failed:\n{}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim_end()
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        git(
            &[
                "clone",
                "--quiet",
                repo,
                dir.to_str().context("scratch path")?,
            ],
            None,
        )?;
        if let Some(rev) = rev {
            git(&["checkout", "--quiet", rev], Some(&dir))?;
        }
        let commit = git(&["rev-parse", "HEAD"], Some(&dir))?;
        Ok((dir, commit))
    }
}

impl Fetch for Builder<'_> {
    fn wheel_from_sdist(&self, name: &str, sdist: &File) -> Result<PathBuf> {
        if self.backend.no_build {
            bail!("{name} ships only a source distribution and policy requires wheels");
        }
        let (archive, hash) = self.fetch_sdist(sdist)?;
        let key = self.build_key(&hash)?;
        if let Some(w) = self.fitting_cached_wheel(&Self::built_dir(&key)?) {
            return Ok(w);
        }
        let scratch =
            std::env::temp_dir().join(format!("vivar-sdist-{}-{key}", std::process::id()));
        let result = Self::unpack_sdist(&archive, &scratch)
            .and_then(|src| self.wheel_from_tree(name, &src, &key));
        let _ = fs::remove_dir_all(&scratch);
        result
    }

    fn direct(&self, req: &Requirement) -> Result<Direct> {
        let url = req.url.clone().context("not a direct reference")?;
        let name = project::normalize(&req.name);
        let metadata_of = |wheel: &Path| -> Result<(Version, Metadata)> {
            let text = crate::index::metadata_from_wheel(&fs::read(wheel)?)
                .with_context(|| format!("reading METADATA from {}", wheel.display()))?;
            let filename = wheel
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let probe = File {
                filename,
                url: String::new(),
                sha256: None,
                requires_python: None,
                yanked: false,
                has_metadata: false,
            };
            let version = probe.version(&name).ok_or_else(|| {
                anyhow!(
                    "{} is not a wheel for {name} (the file name says otherwise)",
                    wheel.display()
                )
            })?;
            Ok((version, crate::index::parse_metadata(&text)))
        };
        if let Some(spec) = url.strip_prefix("git+") {
            if spec.contains("#subdirectory=") {
                bail!("{name}: git references with a subdirectory are not supported yet");
            }
            if self.backend.no_build {
                bail!("{name} comes from git and would have to be built; policy requires wheels");
            }
            let (repo, rev) = crate::resolve::git_parts(&url);
            let (src, commit) = Self::clone(&repo, rev.as_deref())?;
            let key = self.build_key(&format!("{repo}#{commit}"))?;
            let wheel = self.wheel_from_tree(&name, &src, &key);
            let _ = fs::remove_dir_all(&src);
            let (version, metadata) = metadata_of(&wheel?)?;
            return Ok(Direct {
                version,
                metadata,
                files: Vec::new(),
                source: ResolvedSource::Git {
                    url: repo,
                    rev,
                    commit,
                },
            });
        }
        // `#sha256=…` on the URL is a promise to hold the download to.
        let (url, expected) = split_hash_fragment(&url)?;
        let filename = crate::lock::Artifact {
            url: url.clone(),
            hash: None,
        }
        .filename();
        if !safe_file_name(&filename) {
            bail!("{name}: {url} does not name a file");
        }
        let file = File {
            filename: filename.clone(),
            url: url.clone(),
            sha256: expected,
            requires_python: None,
            yanked: false,
            has_metadata: false,
        };
        if filename.ends_with(".whl") {
            let (path, hash) = self.fetch_sdist(&file)?; // same download path; it is just a file
            let (version, metadata) = metadata_of(&path)?;
            // The wheel lands in the wheel cache under its hash, where sync finds it.
            let dest = cache_dir()?.join(&hash[..16]).join(&filename);
            if !dest.is_file() {
                fs::create_dir_all(dest.parent().context("cache path")?)?;
                fs::copy(&path, &dest)?;
            }
            return Ok(Direct {
                version,
                metadata,
                files: vec![File {
                    sha256: Some(hash),
                    ..file
                }],
                source: ResolvedSource::Url(url),
            });
        }
        let (archive, hash) = self.fetch_sdist(&file)?;
        let hashed = File {
            sha256: Some(hash),
            ..file
        };
        let wheel = self.wheel_from_sdist(&name, &hashed)?;
        let _ = archive;
        let (version, metadata) = metadata_of(&wheel)?;
        Ok(Direct {
            version,
            metadata,
            files: vec![hashed],
            source: ResolvedSource::Url(url),
        })
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
        // Like `uv sync`: no lockfile, or one that no longer covers what the
        // project declares, is resolved first.
        if !root.join(crate::lock::LOCKFILE).is_file() || self.lock_check(dir).is_err() {
            self.relock(&project, &[], false, false)?;
        }
        let mut all: Vec<&str> = vec!["dev"];
        all.extend(groups.iter().copied().filter(|g| *g != "dev"));
        self.sync_into(&root, &root.join(".venv"), &all)
    }

    fn upgrade(&self, dir: &Path, packages: &[String], dry_run: bool) -> Result<()> {
        let project = Project::require(dir)?;
        let (old, new) = self.relock(&project, packages, packages.is_empty(), dry_run)?;
        let changes = changes_between(&old, &new);
        for c in &changes {
            println!("{}", c.describe(!dry_run));
        }
        if changes.is_empty() {
            println!("No lockfile changes detected");
        }
        Ok(())
    }

    fn outdated(&self, dir: &Path) -> Result<Vec<Change>> {
        let project = Project::require(dir)?;
        let (old, new) = self.relock(&project, &[], true, true)?;
        Ok(changes_between(&old, &new))
    }

    /// PEP 517 by hand: a throwaway environment with `build-system.requires`
    /// (plus whatever the backend asks for), then `build_sdist` and
    /// `build_wheel` into `dist/`.
    fn build(&self, dir: &Path) -> Result<()> {
        let project = Project::require(dir)?;
        let version = python_floor(&project)?;
        let series = format!("{}.{}", version[0], version[1]);
        let dist = project.root.join("dist");
        for name in self.pep517(&project.root, &dist, &series, &["sdist", "wheel"])? {
            println!("built dist/{name}");
        }
        Ok(())
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
        let builder = Builder {
            backend: self,
            series: format!("{}.{}", version[0], version[1]),
            index_urls: self.index_urls_with(index.url()),
        };
        let resolution = Resolver::new(&index, version, self.no_build)
            .fetch(&builder)
            .single()
            .resolve(&roots)?;
        let lock_text = write_lock(&roots, &Lockable::single(resolution));
        let lock = Lock::parse(&lock_text)?;
        let env = MarkerEnv::current(version);
        let wanted = plan(&lock, &env, &[])?;
        let series = format!("{}.{}", version[0], version[1]);
        let target = wheelhouse::Target::new(None, &series)?;
        let layout = install::Layout::venv(&venv_dir, &series, cfg!(windows));
        // Additive: a tool env only ever gains what it asked for.
        let present = install::installed(&layout.site)?;
        let mut installed = 0;
        for pkg in wanted
            .iter()
            .filter(|p| matches!(p.source_kind.as_str(), "registry" | "url" | "git"))
        {
            if present.iter().any(|d| same_install(pkg, d)) {
                continue;
            }
            if let Some(old) = present.iter().find(|d| d.name == pkg.name) {
                install::uninstall(old)?;
            }
            let wheel =
                self.with_source(Some(&series), |source| fetch_wheel(pkg, &target, source))?;
            install::install_wheel_from(&wheel, &layout, direct_url_of(pkg).as_deref())?;
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
        // A check never provisions Python: with no known interpreter, marked
        // requirements are given the benefit of the doubt.
        let here = known_python(&project).map(MarkerEnv::current);
        let mut missing = Vec::new();
        for req in project.declared_requirements() {
            let name = crate::project::normalize(crate::project::requirement_name(&req));
            let applies_here = match (Requirement::parse(&req).ok().and_then(|r| r.marker), &here) {
                (Some(m), Some(env)) => eval_marker(&m, env).unwrap_or(true),
                (Some(_), None) => false,
                (None, _) => true,
            };
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
        let (installed, _) = self.with_source(None, |source| {
            apply(project.venv_root(), &registry, &layout, &t, source)
        })?;
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
