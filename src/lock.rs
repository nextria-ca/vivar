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
    /// Direct dependencies (all extras and groups flattened). Forked lockfiles
    /// pin an edge to a version when several variants of a package exist.
    pub dependencies: Vec<Dep>,
    /// Every artifact has a hash (registry packages always do with uv).
    pub hashed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dep {
    pub name: String,
    pub version: Option<String>,
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
            let (source_kind, source) = table
                .get("source")
                .and_then(Item::as_inline_table)
                .and_then(|t| t.iter().next())
                .map(|(k, v)| (k.to_owned(), v.as_str().unwrap_or("").to_owned()))
                .unwrap_or_default();

            let mut dependencies = Vec::new();
            let mut push_deps = |item: Option<&Item>| {
                if let Some(arr) = item.and_then(Item::as_array) {
                    for t in arr.iter().filter_map(|v| v.as_inline_table()) {
                        if let Some(name) = t.get("name").and_then(|n| n.as_str()) {
                            dependencies.push(Dep {
                                name: name.to_owned(),
                                version: t
                                    .get("version")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_owned),
                            });
                        }
                    }
                }
            };
            push_deps(table.get("dependencies"));
            for group in ["optional-dependencies", "dev-dependencies"] {
                if let Some(t) = table.get(group).and_then(Item::as_table_like) {
                    for (_, item) in t.iter() {
                        push_deps(Some(item));
                    }
                }
            }
            dependencies.sort();
            dependencies.dedup();

            let mut artifacts = 0;
            let mut hashed = 0;
            if let Some(sdist) = table.get("sdist").and_then(Item::as_inline_table) {
                artifacts += 1;
                hashed += sdist.get("hash").is_some() as usize;
            }
            if let Some(wheels) = table.get("wheels").and_then(Item::as_array) {
                for w in wheels.iter().filter_map(|v| v.as_inline_table()) {
                    artifacts += 1;
                    hashed += w.get("hash").is_some() as usize;
                }
            }
            packages.push(Package {
                name: str_of("name"),
                version: str_of("version"),
                source_kind,
                source,
                dependencies,
                hashed: artifacts > 0 && artifacts == hashed,
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
            Some(v) => self
                .packages
                .iter()
                .find(|p| p.name == dep.name && &p.version == v),
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
        let names: Vec<&str> = app.dependencies.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["pytest", "six"]);
        assert!(lock.get("pytest").unwrap().hashed);
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
