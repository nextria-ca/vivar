//! The in-house backend: installs environments straight from `uv.lock`
//! (wheels only, every artifact hash-checked), with no uv process. Resolution
//! (`add`, `lock`, `upgrade`) still needs uv until the resolver lands; those
//! operations say so.

use crate::backend::Backend;
use crate::lock::{Lock, Package};
use crate::pep::{eval_marker, MarkerEnv};
use crate::project::Policy;
use crate::{dirs, install, sha256, upgrade, wheelhouse};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

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

    fn needs_uv(what: &str) -> anyhow::Error {
        anyhow!("`{what}` needs dependency resolution, which vivar still delegates to uv; install uv (vivar's offline bundles include it) or sync from an existing uv.lock")
    }
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

/// Fetches the wheel for `pkg` on `target`, hash-checked, from the cache or
/// the URL the lock records. The wheelhouse (`index-url` directory) is tried first.
fn fetch_wheel(
    pkg: &Package,
    target: &wheelhouse::Target,
    wheelhouse_dir: Option<&Path>,
) -> Result<PathBuf> {
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
    if let Some(dir) = wheelhouse_dir {
        let local = dir.join(&name);
        if local.is_file() && sha256::file(&local)? == expected {
            fs::copy(&local, &dest)?;
            return Ok(dest);
        }
    }
    println!("downloading {name}");
    upgrade::fetch_to(&artifact.url, &dest)?;
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
    wheelhouse_dir: Option<&Path>,
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
                let wheel = fetch_wheel(pkg, target, wheelhouse_dir)?;
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
        let (installed, removed) = apply(
            lock_root,
            &wanted,
            &layout,
            &target,
            self.wheelhouse_dir().as_deref(),
        )?;
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
    fn add(&self, _dir: &Path, _packages: &[String], _group: Option<&str>) -> Result<()> {
        Err(Self::needs_uv("vivar add"))
    }

    fn remove(&self, _dir: &Path, _packages: &[String]) -> Result<()> {
        Err(Self::needs_uv("vivar remove"))
    }

    fn lock(&self, _dir: &Path) -> Result<()> {
        Err(Self::needs_uv("vivar lock"))
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

    fn upgrade(&self, _dir: &Path, _packages: &[String], _dry_run: bool) -> Result<()> {
        Err(Self::needs_uv("vivar upgrade-deps / outdated"))
    }

    fn build(&self, _dir: &Path) -> Result<()> {
        Err(Self::needs_uv("vivar build"))
    }

    fn install(&self, python: &Path, specs: &[String]) -> Result<()> {
        let _ = (python, specs, self.no_build);
        Err(Self::needs_uv("installing packages by name"))
    }

    fn add_requirements(&self, _dir: &Path, _file: &Path, _group: Option<&str>) -> Result<()> {
        Err(Self::needs_uv("vivar import"))
    }

    fn lock_check(&self, dir: &Path) -> Result<()> {
        // Without a resolver: the lock exists and declares every requirement the project does.
        let project = crate::project::Project::require(dir)?;
        let lock = Lock::read(project.venv_root())?;
        let locked: BTreeSet<String> = lock.packages.iter().map(|p| p.name.clone()).collect();
        let mut missing = Vec::new();
        for req in project.declared_requirements() {
            let name = crate::project::normalize(crate::project::requirement_name(&req));
            if !locked.contains(&name) {
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
        let (installed, _) = apply(
            project.venv_root(),
            &wanted
                .iter()
                .copied()
                .filter(|p| p.source_kind == "registry")
                .collect::<Vec<_>>(),
            &layout,
            &t,
            self.wheelhouse_dir().as_deref(),
        )?;
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
