//! Reading `uv.lock`: the dependency graph behind `tree`, `why`, `audit`,
//! `diff` and the hash policy.

use anyhow::{Context, Result};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use toml_edit::{DocumentMut, Item};

pub const LOCKFILE: &str = "uv.lock";

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// `registry`, `virtual`, `editable`, `git`, `url`, `path`, `directory` …
    pub source_kind: String,
    /// The registry URL for registry packages.
    pub source: String,
    /// For a URL archive built from one of its directories.
    pub subdirectory: Option<String>,
    /// Direct dependencies (all extras and groups flattened). Forked lockfiles
    /// pin an edge to a version when several variants of a package exist.
    pub dependencies: Vec<Dep>,
    /// Only the unconditional dependencies (no extra, no group).
    pub base_dependencies: Vec<Dep>,
    /// `[package.optional-dependencies]` by extra name.
    pub extras: std::collections::BTreeMap<String, Vec<Dep>>,
    /// `[package.dev-dependencies]` by group name.
    pub groups: std::collections::BTreeMap<String, Vec<Dep>>,
    /// Markers under which this variant of the package applies (forks).
    pub resolution_markers: Vec<String>,
    /// Every artifact has a hash (registry packages always do with uv).
    pub hashed: bool,
    pub wheels: Vec<Artifact>,
    pub sdist: Option<Artifact>,
}

/// A downloadable file the lock pins, with its `sha256:` hash when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub hash: Option<String>,
}

impl Artifact {
    /// The file name at the end of the URL, percent-decoded (`%2B` → `+` in
    /// local versions such as `torch-2.6.0+cu124`).
    pub fn filename(&self) -> String {
        let raw = self
            .url
            .split(['?', '#'])
            .next()
            .unwrap_or(&self.url)
            .rsplit('/')
            .next()
            .unwrap_or(&self.url);
        let bytes = raw.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The hex digest without the `sha256:` prefix.
    pub fn sha256(&self) -> Option<&str> {
        self.hash.as_deref().and_then(|h| h.strip_prefix("sha256:"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dep {
    pub name: String,
    pub version: Option<String>,
    /// The variant's source (a registry URL, say) when several packages share
    /// the name and version.
    pub source: Option<String>,
    /// PEP 508 marker gating this edge, when the lock has one.
    pub marker: Option<String>,
    /// Extras requested of the dependency.
    pub extras: Vec<String>,
}

pub struct Lock {
    pub packages: Vec<Package>,
}

impl Lock {
    pub fn read(root: &Path) -> Result<Lock> {
        let path = root.join(LOCKFILE);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("reading {}; run `vivar lock` first", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Lock> {
        let doc: DocumentMut = text.parse()?;
        let mut packages = Vec::new();
        let Some(list) = doc.get("package").and_then(Item::as_array_of_tables) else {
            return Ok(Lock { packages });
        };
        for table in list {
            let str_of = |key: &str| {
                table
                    .get(key)
                    .and_then(Item::as_str)
                    .unwrap_or("")
                    .to_owned()
            };
            let source_table = table.get("source").and_then(Item::as_inline_table);
            let (source_kind, source) = source_table
                .and_then(|t| t.iter().next())
                .map(|(k, v)| (k.to_owned(), v.as_str().unwrap_or("").to_owned()))
                .unwrap_or_default();
            let subdirectory = source_table
                .and_then(|t| t.get("subdirectory"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);

            let deps_of = |item: Option<&Item>| -> Vec<Dep> {
                let mut out = Vec::new();
                if let Some(arr) = item.and_then(Item::as_array) {
                    for t in arr.iter().filter_map(|v| v.as_inline_table()) {
                        if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                            out.push(Dep {
                                name: name.to_owned(),
                                version: t
                                    .get("version")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned),
                                source: t
                                    .get("source")
                                    .and_then(|v| v.as_inline_table())
                                    .and_then(|s| s.iter().next())
                                    .and_then(|(_, v)| v.as_str())
                                    .map(str::to_owned),
                                marker: t.get("marker").and_then(|v| v.as_str()).map(str::to_owned),
                                extras: t
                                    .get("extra")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|e| e.as_str())
                                            .map(str::to_owned)
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            });
                        }
                    }
                }
                out
            };
            let base_dependencies = deps_of(table.get("dependencies"));
            let mut extras = std::collections::BTreeMap::new();
            let mut groups = std::collections::BTreeMap::new();
            if let Some(t) = table
                .get("optional-dependencies")
                .and_then(Item::as_table_like)
            {
                for (k, item) in t.iter() {
                    extras.insert(k.to_owned(), deps_of(Some(item)));
                }
            }
            if let Some(t) = table.get("dev-dependencies").and_then(Item::as_table_like) {
                for (k, item) in t.iter() {
                    groups.insert(k.to_owned(), deps_of(Some(item)));
                }
            }
            let mut dependencies: Vec<Dep> = base_dependencies
                .iter()
                .chain(extras.values().flatten())
                .chain(groups.values().flatten())
                .cloned()
                .collect();
            dependencies.sort();
            dependencies.dedup();
            let resolution_markers = table
                .get("resolution-markers")
                .and_then(Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();

            // A file on disk (a `path` source) is listed by name, not URL.
            let artifact = |t: &toml_edit::InlineTable| Artifact {
                url: t
                    .get("url")
                    .or_else(|| t.get("filename"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned(),
                hash: t.get("hash").and_then(|v| v.as_str()).map(str::to_owned),
            };
            let sdist = table
                .get("sdist")
                .and_then(Item::as_inline_table)
                .map(artifact);
            let wheels: Vec<Artifact> = table
                .get("wheels")
                .and_then(Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_inline_table())
                        .map(artifact)
                        .collect()
                })
                .unwrap_or_default();
            let artifacts: Vec<&Artifact> = sdist.iter().chain(wheels.iter()).collect();
            packages.push(Package {
                name: str_of("name"),
                version: str_of("version"),
                source_kind,
                source,
                subdirectory,
                dependencies,
                base_dependencies,
                extras,
                groups,
                resolution_markers,
                hashed: !artifacts.is_empty() && artifacts.iter().all(|a| a.hash.is_some()),
                wheels,
                sdist,
            });
        }
        Ok(Lock { packages })
    }

    pub fn get(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// The package an edge points at: the pinned variant, else the only one.
    fn resolve(&self, dep: &Dep) -> Option<&Package> {
        match &dep.version {
            Some(v) => self.packages.iter().find(|p| {
                p.name == dep.name
                    && &p.version == v
                    && dep.source.as_ref().is_none_or(|s| *s == p.source)
            }),
            None => self.get(&dep.name),
        }
    }

    fn edge_hits(dep: &Dep, target: &Package) -> bool {
        dep.name == target.name && dep.version.as_deref().is_none_or(|v| v == target.version)
    }

    /// The project itself and workspace members: what everything hangs off.
    pub fn roots(&self) -> Vec<&Package> {
        let roots: Vec<&Package> = self
            .packages
            .iter()
            .filter(|p| matches!(p.source_kind.as_str(), "virtual" | "editable"))
            .collect();
        if roots.is_empty() {
            // Nothing depends on these: top-level by elimination.
            let depended: BTreeSet<&str> = self
                .packages
                .iter()
                .flat_map(|p| p.dependencies.iter().map(|d| d.name.as_str()))
                .collect();
            return self
                .packages
                .iter()
                .filter(|p| !depended.contains(p.name.as_str()))
                .collect();
        }
        roots
    }

    /// The dependency tree as JSON: nested `{name, version, dependencies}`;
    /// a package already expanded elsewhere appears with `"repeat": true` and
    /// no children, as the text tree marks it with `(*)`.
    pub fn tree_json(&self, depth: usize) -> String {
        let mut out = String::from("[");
        let mut seen = BTreeSet::new();
        for (i, root) in self.roots().into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            self.render_json(root, depth, &mut seen, &mut out);
        }
        out.push_str("]\n");
        out
    }

    fn render_json(
        &self,
        pkg: &Package,
        depth: usize,
        seen: &mut BTreeSet<String>,
        out: &mut String,
    ) {
        let repeat =
            !seen.insert(format!("{}@{}", pkg.name, pkg.version)) && !pkg.dependencies.is_empty();
        out.push_str(&format!(
            "{{\"name\":{},\"version\":{}",
            crate::json::quote(&pkg.name),
            crate::json::quote(&pkg.version)
        ));
        if repeat {
            out.push_str(",\"repeat\":true}");
            return;
        }
        if depth > 0 {
            out.push_str(",\"dependencies\":[");
            let mut first = true;
            for dep in &pkg.dependencies {
                let Some(child) = self.resolve(dep) else {
                    continue;
                };
                if !first {
                    out.push(',');
                }
                first = false;
                self.render_json(child, depth - 1, seen, out);
            }
            out.push(']');
        }
        out.push('}');
    }

    /// Renders the dependency tree as text, one line per edge.
    pub fn tree(&self, depth: usize) -> String {
        let mut out = String::new();
        let mut seen = BTreeSet::new();
        for root in self.roots() {
            self.render(root, "", true, depth, &mut seen, &mut out);
        }
        out
    }

    fn render(
        &self,
        pkg: &Package,
        prefix: &str,
        is_root: bool,
        depth: usize,
        seen: &mut BTreeSet<String>,
        out: &mut String,
    ) {
        let repeat =
            !seen.insert(format!("{}@{}", pkg.name, pkg.version)) && !pkg.dependencies.is_empty();
        out.push_str(&format!(
            "{prefix}{} v{}{}\n",
            pkg.name,
            pkg.version,
            if repeat { " (*)" } else { "" }
        ));
        if repeat || depth == 0 {
            return;
        }
        let child_prefix = if is_root {
            String::new()
        } else {
            // Replace this node's branch with a continuation or a blank.
            let mut p = prefix.to_owned();
            if let Some(stripped) = p.strip_suffix("└── ") {
                p = format!("{stripped}    ");
            } else if let Some(stripped) = p.strip_suffix("├── ") {
                p = format!("{stripped}│   ");
            }
            p
        };
        let last = pkg.dependencies.len().saturating_sub(1);
        for (i, dep) in pkg.dependencies.iter().enumerate() {
            let Some(child) = self.resolve(dep) else {
                continue;
            };
            let branch = if i == last {
                "└── "
            } else {
                "├── "
            };
            self.render(
                child,
                &format!("{child_prefix}{branch}"),
                false,
                depth - 1,
                seen,
                out,
            );
        }
    }

    /// Every chain from a root down to any variant of `name`, shortest first.
    pub fn why(&self, name: &str) -> Vec<Vec<&Package>> {
        let targets: Vec<&Package> = self.packages.iter().filter(|p| p.name == name).collect();
        let mut paths = Vec::new();
        let mut queue: VecDeque<Vec<&Package>> = targets.into_iter().map(|t| vec![t]).collect();
        while let Some(path) = queue.pop_front() {
            let top = path[0];
            let parents: Vec<&Package> = self
                .packages
                .iter()
                .filter(|p| p.dependencies.iter().any(|d| Self::edge_hits(d, top)))
                .collect();
            if parents.is_empty() {
                paths.push(path);
                if paths.len() >= 20 {
                    break;
                }
                continue;
            }
            for parent in parents {
                if path.iter().any(|p| std::ptr::eq(*p, parent)) {
                    continue; // cycle
                }
                let mut next = vec![parent];
                next.extend(path.iter().copied());
                queue.push_back(next);
            }
        }
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 1
[[package]]
name = "app"
version = "0.1.0"
source = { virtual = "." }
dependencies = [{ name = "pytest" }]

[package.dev-dependencies]
dev = [{ name = "six" }]

[[package]]
name = "pytest"
version = "9.0.0"
source = { registry = "https://pypi.org/simple" }
dependencies = [{ name = "iniconfig" }, { name = "pluggy" }]
sdist = { url = "u", hash = "sha256:a" }
wheels = [{ url = "w", hash = "sha256:b" }]

[[package]]
name = "pluggy"
version = "1.0.0"
source = { registry = "https://pypi.org/simple" }
wheels = [{ url = "w" }]

[[package]]
name = "iniconfig"
version = "2.0.0"
source = { git = "https://github.com/x/iniconfig?rev=abc" }

[[package]]
name = "six"
version = "1.17.0"
source = { registry = "https://pypi.org/simple" }
wheels = [{ url = "w", hash = "sha256:c" }]
"#;

    #[test]
    fn reads_graph_and_hashes() {
        let lock = Lock::parse(LOCK).unwrap();
        assert_eq!(lock.packages.len(), 5);
        let app = lock.get("app").unwrap();
        assert_eq!(app.source_kind, "virtual");
        assert_eq!(app.base_dependencies.len(), 1);
        assert_eq!(app.groups["dev"][0].name, "six");
        let names: Vec<&str> = app.dependencies.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["pytest", "six"]);
        assert!(lock.get("pytest").unwrap().hashed);
        let pytest = lock.get("pytest").unwrap();
        assert_eq!(pytest.wheels.len(), 1);
        assert_eq!(pytest.sdist.as_ref().unwrap().sha256(), Some("a"));
        assert_eq!(pytest.wheels[0].filename(), "w");
        let torch = Artifact {
            url: "https://download.pytorch.org/whl/cu124/torch-2.6.0%2Bcu124-cp312-cp312-linux_x86_64.whl#sha256=abc".into(),
            hash: None,
        };
        assert_eq!(
            torch.filename(),
            "torch-2.6.0+cu124-cp312-cp312-linux_x86_64.whl"
        );
        assert!(!lock.get("pluggy").unwrap().hashed);
        assert!(!lock.get("iniconfig").unwrap().hashed);
        assert_eq!(lock.get("iniconfig").unwrap().source_kind, "git");
        assert_eq!(lock.roots().len(), 1);
    }

    #[test]
    fn tree_and_why() {
        let lock = Lock::parse(LOCK).unwrap();
        let tree = lock.tree(10);
        assert_eq!(
            tree,
            "app v0.1.0\n├── pytest v9.0.0\n│   ├── iniconfig v2.0.0\n│   └── pluggy v1.0.0\n└── six v1.17.0\n"
        );
        let why = lock.why("pluggy");
        assert_eq!(why.len(), 1);
        let names: Vec<&str> = why[0].iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["app", "pytest", "pluggy"]);
        assert!(lock.why("nope").is_empty());
        assert_eq!(lock.tree(0), "app v0.1.0\n");
    }

    #[test]
    fn forked_variants_keep_their_identity() {
        let lock = Lock::parse(
            r#"
[[package]]
name = "app"
version = "0.1.0"
source = { virtual = "." }
dependencies = [
    { name = "numpy", version = "1.26.4", marker = "python_full_version < '3.12'" },
    { name = "numpy", version = "2.1.0", marker = "python_full_version >= '3.12'" },
]

[[package]]
name = "numpy"
version = "1.26.4"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "numpy"
version = "2.1.0"
source = { registry = "https://pypi.org/simple" }
"#,
        )
        .unwrap();
        let tree = lock.tree(5);
        assert!(
            tree.contains("numpy v1.26.4") && tree.contains("numpy v2.1.0"),
            "{tree}"
        );
        assert_eq!(lock.why("numpy").len(), 2);
    }
}
