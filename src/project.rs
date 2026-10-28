//! `pyproject.toml` handling: project discovery, vivar defaults, and policy checks.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{value, Array, DocumentMut, Item, Table};

pub const PYPROJECT: &str = "pyproject.toml";

/// Writes `pyproject.toml`, `.python-version` and the vivar defaults for a new project.
pub fn init_files(root: &Path, name: &str, version: [u32; 3]) -> Result<()> {
    let [major, minor, _] = version;
    let mut doc = DocumentMut::new();
    let mut project = Table::new();
    project["name"] = value(name);
    project["version"] = value("0.1.0");
    project["requires-python"] = value(format!(">={major}.{minor}"));
    project["dependencies"] = value(Array::new());
    doc["project"] = Item::Table(project);
    fs::write(root.join(PYPROJECT), doc.to_string())?;
    fs::write(root.join(".python-version"), format!("{major}.{minor}\n"))?;
    write_defaults(root)
}

/// A valid project name derived from the directory name.
pub fn default_name(root: &Path) -> String {
    let raw = root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name.trim_matches(|c| c == '-' || c == '.');
    if name.is_empty() {
        "project".to_owned()
    } else {
        name.to_owned()
    }
}

/// Settings read from `[tool.vivar.policy]`, merged with the org policy file
/// (`VIVAR_POLICY`, else `<config dir>/vivar/policy.toml`, else `/etc/vivar/policy.toml`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Normalized package names that `vivar add`/`lock`/`sync`/`run` refuse.
    pub deny: Vec<String>,
    /// Package index to use instead of PyPI: an internal mirror URL, or a local
    /// directory of wheels (an offline wheelhouse).
    pub index_url: Option<String>,
    /// SPDX identifiers every installed package must satisfy. `None` means no
    /// check; an empty list (two policies with nothing in common) allows nothing.
    pub allow_licenses: Option<Vec<String>>,
    /// Normalized package names exempt from the license check.
    pub license_exceptions: Vec<String>,
    /// Every locked package must be a hashed registry artifact from `index-url`.
    pub require_hashes: bool,
}

impl Policy {
    /// Refuses requirements whose package name is on the deny list.
    pub fn check_deny<S: AsRef<str>>(&self, reqs: &[S]) -> Result<()> {
        let blocked: Vec<&str> = reqs
            .iter()
            .map(AsRef::as_ref)
            .filter(|r| self.deny.contains(&normalize(requirement_name(r))))
            .collect();
        if !blocked.is_empty() {
            bail!(
                "blocked by [tool.vivar.policy] deny list: {}",
                blocked.join(", ")
            );
        }
        Ok(())
    }

    fn from_table(table: Option<&Item>, base: &Path) -> Policy {
        let list = |key: &str| -> Vec<String> {
            table
                .and_then(|p| p.get(key))
                .and_then(Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let index_url = table
            .and_then(|p| p.get("index-url"))
            .and_then(Item::as_str)
            .map(|s| match s.contains("://") {
                true => s.to_owned(),
                // A wheelhouse directory, relative to the file if not absolute.
                false => base.join(s).to_string_lossy().into_owned(),
            });
        let allow_licenses = table
            .and_then(|p| p.get("allow-licenses"))
            .is_some()
            .then(|| list("allow-licenses"));
        Policy {
            deny: list("deny").iter().map(|s| normalize(s)).collect(),
            index_url,
            allow_licenses,
            license_exceptions: list("license-exceptions")
                .iter()
                .map(|s| normalize(s))
                .collect(),
            require_hashes: table
                .and_then(|p| p.get("require-hashes"))
                .and_then(Item::as_bool)
                .unwrap_or(false),
        }
    }

    /// Org policy on top of a project's: denies add up, `require-hashes` is
    /// sticky, license allowlists intersect, and the project's index wins
    /// (a wheelhouse may stand in for the mirror).
    fn merge(mut self, org: Policy) -> Policy {
        for d in org.deny {
            if !self.deny.contains(&d) {
                self.deny.push(d);
            }
        }
        self.index_url = self.index_url.or(org.index_url);
        self.allow_licenses = match (self.allow_licenses, org.allow_licenses) {
            (None, org) => org,
            (mine, None) => mine,
            (Some(mine), Some(org)) => Some(
                mine.into_iter()
                    .filter(|l| org.iter().any(|o| o.eq_ignore_ascii_case(l)))
                    .collect(),
            ),
        };
        self.license_exceptions.extend(org.license_exceptions);
        self.require_hashes |= org.require_hashes;
        self
    }
}

/// The org-wide policy file, if one is configured. `[policy]` table, same keys.
pub fn org_policy() -> Result<Option<(PathBuf, Policy)>> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("VIVAR_POLICY") {
        candidates.push(PathBuf::from(p));
    } else {
        if let Ok(dir) = crate::dirs::config() {
            candidates.push(dir.join("policy.toml"));
        }
        if cfg!(unix) {
            candidates.push(PathBuf::from("/etc/vivar/policy.toml"));
        }
    }
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let doc: DocumentMut = text
            .parse()
            .with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        return Ok(Some((path, Policy::from_table(doc.get("policy"), &base))));
    }
    Ok(None)
}

/// A project rooted at the directory containing `pyproject.toml`. In a uv
/// workspace, `workspace` is the root project: that is where `.venv` lives.
pub struct Project {
    pub root: PathBuf,
    doc: DocumentMut,
    pub workspace: Option<Box<Project>>,
}

impl Project {
    /// Walks up from `start` looking for a `pyproject.toml`, the same way uv does,
    /// so policy applies no matter which subdirectory a command runs from.
    pub fn find(start: &Path) -> Result<Option<Project>> {
        let mut ancestors = start.ancestors();
        let Some(dir) = ancestors.find(|d| d.join(PYPROJECT).is_file()) else {
            return Ok(None);
        };
        let mut project = Self::load(dir)?;
        if !project.is_workspace_root() {
            for parent in ancestors {
                if parent.join(PYPROJECT).is_file() {
                    let candidate = Self::load(parent)?;
                    if candidate.is_workspace_root() && candidate.has_member(&project.root) {
                        project.workspace = Some(Box::new(candidate));
                    }
                    break;
                }
            }
        }
        Ok(Some(project))
    }

    /// Whether `dir` matches the root's `members` globs and none of `exclude`.
    fn has_member(&self, dir: &Path) -> bool {
        let Ok(rel) = dir.strip_prefix(&self.root) else {
            return false;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let patterns = |key: &str| -> Vec<String> {
            self.doc
                .get("tool")
                .and_then(|t| t.get("uv"))
                .and_then(|u| u.get("workspace"))
                .and_then(|w| w.get(key))
                .and_then(Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        patterns("members").iter().any(|p| glob_match(p, &rel))
            && !patterns("exclude").iter().any(|p| glob_match(p, &rel))
    }

    fn is_workspace_root(&self) -> bool {
        self.doc
            .get("tool")
            .and_then(|t| t.get("uv"))
            .and_then(|u| u.get("workspace"))
            .is_some()
    }

    /// Where `.venv` lives: the workspace root, or the project itself.
    pub fn venv_root(&self) -> &Path {
        self.workspace.as_deref().map_or(&self.root, |w| &w.root)
    }

    /// `[project] requires-python`, if declared.
    pub fn requires_python(&self) -> Option<String> {
        self.doc
            .get("project")
            .and_then(|p| p.get("requires-python"))
            .and_then(Item::as_str)
            .map(str::to_owned)
    }

    /// `[tool.vivar.scripts]`: named commands for `vivar run <name>`.
    pub fn script(&self, name: &str) -> Option<String> {
        self.doc
            .get("tool")
            .and_then(|t| t.get("vivar"))
            .and_then(|v| v.get("scripts"))
            .and_then(|s| s.get(name))
            .and_then(Item::as_str)
            .map(str::to_owned)
    }

    /// Like [`find`](Self::find), but a missing project is an error with a hint.
    pub fn require(start: &Path) -> Result<Project> {
        Self::find(start)?.with_context(|| {
            format!(
                "no {PYPROJECT} found in {} or any parent directory; run `vivar init` first",
                start.display()
            )
        })
    }

    pub fn load(root: &Path) -> Result<Project> {
        let path = root.join(PYPROJECT);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let doc = text
            .parse()
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(Project {
            root: root.to_path_buf(),
            doc,
            workspace: None,
        })
    }

    /// `[project] name`, falling back to the directory name.
    pub fn name(&self) -> String {
        self.doc
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(Item::as_str)
            .map(str::to_owned)
            .or_else(|| {
                self.root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "project".to_owned())
    }

    /// This project's policy, under the workspace root's and the org's.
    pub fn policy(&self) -> Result<Policy> {
        let mut policy = self.own_policy();
        if let Some(ws) = &self.workspace {
            policy = policy.merge(ws.own_policy());
        }
        // An org policy that exists but can't be read is an error, never skipped.
        if let Some((_, org)) = org_policy()? {
            policy = policy.merge(org);
        }
        Ok(policy)
    }

    fn own_policy(&self) -> Policy {
        let table = self
            .doc
            .get("tool")
            .and_then(|t| t.get("vivar"))
            .and_then(|v| v.get("policy"));
        Policy::from_table(table, &self.root)
    }

    /// Refuses requirements whose package name is on the deny list.
    pub fn check_requirements<S: AsRef<str>>(&self, reqs: &[S]) -> Result<()> {
        self.policy()?.check_deny(reqs)
    }

    /// Checks every dependency declared in `pyproject.toml` against the deny list,
    /// so hand-editing the file can't bypass `vivar add`.
    pub fn check_declared(&self) -> Result<()> {
        self.check_requirements(&self.declared_requirements())
            .map_err(|e| anyhow!("{e} (declared in {})", self.root.join(PYPROJECT).display()))
    }

    /// All requirement strings from `[project] dependencies`,
    /// `[project.optional-dependencies]` and `[dependency-groups]`.
    pub fn declared_requirements(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut push_array = |item: Option<&Item>| {
            if let Some(arr) = item.and_then(Item::as_array) {
                out.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_owned));
            }
        };

        let project = self.doc.get("project");
        push_array(project.and_then(|p| p.get("dependencies")));
        let sections = [
            project.and_then(|p| p.get("optional-dependencies")),
            self.doc.get("dependency-groups"),
        ];
        for table in sections
            .into_iter()
            .flatten()
            .filter_map(Item::as_table_like)
        {
            for (_, item) in table.iter() {
                push_array(Some(item));
            }
        }
        out
    }
}

/// Adds an empty `[tool.vivar.policy]` deny list unless the project already
/// has a `[tool.vivar]` table. Settings for later phases are added when they
/// actually do something.
pub fn write_defaults(root: &Path) -> Result<()> {
    let path = root.join(PYPROJECT);
    let mut project = Project::load(root)?;

    let mut implicit = Table::new();
    implicit.set_implicit(true);
    let tool = project
        .doc
        .entry("tool")
        .or_insert(Item::Table(implicit))
        .as_table_mut()
        .context("`tool` in pyproject.toml is not a table")?;
    if tool.contains_key("vivar") {
        return Ok(());
    }

    let mut vivar = Table::new();
    vivar.set_implicit(true);
    let mut policy = Table::new();
    policy["deny"] = value(Array::new());
    vivar["policy"] = Item::Table(policy);
    tool["vivar"] = Item::Table(vivar);

    fs::write(&path, project.doc.to_string()).with_context(|| format!("writing {}", path.display()))
}

/// uv-style workspace globs: `*` and `?` within one path segment, `[...]`
/// classes (with `!` negation and ranges), `**` across segments. Matched by
/// character, so non-ASCII segments are fine.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn segments(s: &str) -> Vec<&str> {
        s.split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect()
    }
    fn class_match(class: &[char], c: char) -> bool {
        let (negate, body) = match class.first() {
            Some('!') | Some('^') => (true, &class[1..]),
            _ => (false, class),
        };
        let mut i = 0;
        let mut hit = false;
        while i < body.len() {
            if i + 2 < body.len() && body[i + 1] == '-' {
                hit |= body[i] <= c && c <= body[i + 2];
                i += 3;
            } else {
                hit |= body[i] == c;
                i += 1;
            }
        }
        hit != negate
    }
    fn seg_match(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len()).any(|i| seg_match(&p[1..], &s[i..])),
            Some('?') => !s.is_empty() && seg_match(&p[1..], &s[1..]),
            Some('[') => {
                let Some(close) = p.iter().position(|c| *c == ']') else {
                    return !s.is_empty() && s[0] == '[' && seg_match(&p[1..], &s[1..]);
                };
                !s.is_empty()
                    && class_match(&p[1..close], s[0])
                    && seg_match(&p[close + 1..], &s[1..])
            }
            Some(c) => !s.is_empty() && s[0] == *c && seg_match(&p[1..], &s[1..]),
        }
    }
    fn go(p: &[&str], s: &[&str]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(&"**"), _) => go(&p[1..], s) || (!s.is_empty() && go(p, &s[1..])),
            (Some(pp), Some(ss)) => {
                let (pc, sc): (Vec<char>, Vec<char>) = (pp.chars().collect(), ss.chars().collect());
                seg_match(&pc, &sc) && go(&p[1..], &s[1..])
            }
            _ => false,
        }
    }
    go(&segments(pattern), &segments(path))
}

/// `"pandas[excel]>=2; python_version>'3.9'"` -> `"pandas"`
pub fn requirement_name(req: &str) -> &str {
    let end = req
        .find(|c: char| "<>=!~[;@ ".contains(c))
        .unwrap_or(req.len());
    &req[..end]
}

/// PEP 503 name normalization (simplified): case-insensitive, `_`/`.` == `-`.
pub fn normalize(name: &str) -> String {
    name.trim().to_lowercase().replace(['_', '.'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("vivar-unit-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn project_with(dir: &Path, pyproject: &str) -> Project {
        fs::write(dir.join(PYPROJECT), pyproject).unwrap();
        Project::load(dir).unwrap()
    }

    #[test]
    fn parses_requirement_names() {
        assert_eq!(requirement_name("numpy"), "numpy");
        assert_eq!(requirement_name("pandas>=2"), "pandas");
        assert_eq!(requirement_name("requests[socks]~=2.31"), "requests");
        assert_eq!(requirement_name("foo; python_version>'3.9'"), "foo");
        assert_eq!(requirement_name("pkg @ https://x/pkg.whl"), "pkg");
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize("Zope.Interface"), "zope-interface");
        assert_eq!(normalize("typing_extensions"), "typing-extensions");
    }

    #[test]
    fn deny_list_blocks_packages() {
        let tmp = TempDir::new("deny");
        let project = project_with(
            &tmp.0,
            "[project]\nname = \"t\"\n\n[tool.vivar.policy]\ndeny = [\"Bad_Pkg\"]\n",
        );
        assert!(project.check_requirements(&["numpy"]).is_ok());
        assert!(project.check_requirements(&["bad-pkg>=1"]).is_err());
    }

    #[test]
    fn reads_policy_with_defaults() {
        let tmp = TempDir::new("policy");
        let project = project_with(&tmp.0, "[project]\nname = \"t\"\n");
        assert_eq!(project.policy().unwrap(), Policy::default());

        let project = project_with(
            &tmp.0,
            "[tool.vivar.policy]\ndeny = [\"a\"]\nindex-url = \"https://pypi.internal/simple\"\n",
        );
        assert_eq!(
            project.policy().unwrap(),
            Policy {
                deny: vec!["a".into()],
                index_url: Some("https://pypi.internal/simple".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn collects_declared_requirements_from_all_sections() {
        let tmp = TempDir::new("declared");
        let project = project_with(
            &tmp.0,
            r#"
[project]
name = "t"
dependencies = ["numpy", "pandas>=2"]

[project.optional-dependencies]
excel = ["openpyxl"]

[dependency-groups]
dev = ["pytest", { include-group = "lint" }]
lint = ["ruff"]
"#,
        );
        let mut reqs = project.declared_requirements();
        reqs.sort();
        assert_eq!(reqs, ["numpy", "openpyxl", "pandas>=2", "pytest", "ruff"]);
    }

    #[test]
    fn declared_check_catches_hand_edits() {
        let tmp = TempDir::new("handedit");
        let project = project_with(
            &tmp.0,
            "[project]\nname = \"t\"\ndependencies = [\"Requests\"]\n\n[tool.vivar.policy]\ndeny = [\"requests\"]\n",
        );
        let err = project.check_declared().unwrap_err().to_string();
        assert!(err.contains("Requests"), "{err}");
    }

    #[test]
    fn finds_project_from_subdirectory() {
        let tmp = TempDir::new("find");
        fs::write(tmp.0.join(PYPROJECT), "[project]\nname = \"t\"\n").unwrap();
        let sub = tmp.0.join("src").join("pkg");
        fs::create_dir_all(&sub).unwrap();

        let found = Project::find(&sub)
            .unwrap()
            .expect("project should be found");
        assert_eq!(found.root, tmp.0);
        assert!(Project::find(&std::env::temp_dir()).unwrap().is_none());
    }

    #[test]
    fn write_defaults_is_idempotent_and_preserves_existing() {
        let tmp = TempDir::new("defaults");
        fs::write(tmp.0.join(PYPROJECT), "[project]\nname = \"t\"\n").unwrap();
        write_defaults(&tmp.0).unwrap();
        let first = fs::read_to_string(tmp.0.join(PYPROJECT)).unwrap();
        assert!(first.contains("[tool.vivar.policy]\ndeny = []"), "{first}");

        write_defaults(&tmp.0).unwrap();
        assert_eq!(fs::read_to_string(tmp.0.join(PYPROJECT)).unwrap(), first);

        let custom = "[project]\nname = \"t\"\n\n[tool.vivar.policy]\ndeny = [\"x\"]\n";
        fs::write(tmp.0.join(PYPROJECT), custom).unwrap();
        write_defaults(&tmp.0).unwrap();
        assert_eq!(fs::read_to_string(tmp.0.join(PYPROJECT)).unwrap(), custom);
    }

    #[test]
    fn org_policy_merges_on_top() {
        let project = Policy {
            deny: vec!["a".into()],
            index_url: Some("/wheelhouse".into()),
            allow_licenses: Some(vec!["MIT".into(), "GPL-3.0-only".into()]),
            ..Default::default()
        };
        let org = Policy {
            deny: vec!["b".into(), "a".into()],
            index_url: Some("https://mirror/simple".into()),
            allow_licenses: Some(vec!["mit".into(), "Apache-2.0".into()]),
            license_exceptions: vec!["legacy".into()],
            require_hashes: true,
        };
        let merged = project.merge(org);
        assert_eq!(merged.deny, vec!["a", "b"]);
        assert_eq!(merged.index_url.as_deref(), Some("/wheelhouse"));
        assert_eq!(merged.allow_licenses, Some(vec!["MIT".to_owned()]));
        assert_eq!(merged.license_exceptions, vec!["legacy"]);
        assert!(merged.require_hashes);

        let only_org = Policy::default().merge(Policy {
            allow_licenses: Some(vec!["MIT".into()]),
            ..Default::default()
        });
        assert_eq!(only_org.allow_licenses, Some(vec!["MIT".to_owned()]));

        // Nothing in common: nothing is allowed, rather than everything.
        let disjoint = Policy {
            allow_licenses: Some(vec!["GPL-3.0-only".into()]),
            ..Default::default()
        }
        .merge(Policy {
            allow_licenses: Some(vec!["MIT".into()]),
            ..Default::default()
        });
        assert_eq!(disjoint.allow_licenses, Some(Vec::new()));
    }

    #[test]
    fn workspace_globs() {
        assert!(glob_match("pkgs/*", "pkgs/a"));
        assert!(!glob_match("pkgs/*", "pkgs/a/b"));
        assert!(glob_match("pkgs/**", "pkgs/a/b"));
        assert!(glob_match("pkgs/lib-*", "pkgs/lib-x"));
        assert!(!glob_match("pkgs/lib-*", "pkgs/x"));
        assert!(glob_match("exact/path", "exact/path"));
        assert!(!glob_match("other/*", "pkgs/a"));
        assert!(glob_match("*", "a"));
        assert!(glob_match("pkgs/[ab]", "pkgs/a"));
        assert!(!glob_match("pkgs/[ab]", "pkgs/c"));
        assert!(glob_match("pkgs/[!ab]", "pkgs/c"));
        assert!(glob_match("pkgs/lib-[0-9]", "pkgs/lib-7"));
        assert!(glob_match("pkgs/a?", "pkgs/ab"));
        assert!(!glob_match("pkgs/a?", "pkgs/a"));
        assert!(glob_match("pkgs/*", "pkgs/café"));
        assert!(glob_match("pkgs/caf?", "pkgs/café"));
        assert!(glob_match("**/tests", "a/b/tests"));
    }

    #[test]
    fn reads_scripts_and_license_policy() {
        let tmp = TempDir::new("scripts");
        let project = project_with(
            &tmp.0,
            "[project]\nname = \"t\"\n[tool.vivar.scripts]\ntest = \"pytest -x\"\n[tool.vivar.policy]\nallow-licenses = [\"MIT\"]\nlicense-exceptions = [\"Legacy_Pkg\"]\nrequire-hashes = true\n",
        );
        assert_eq!(project.script("test").as_deref(), Some("pytest -x"));
        assert_eq!(project.script("nope"), None);
        let policy = project.own_policy();
        assert_eq!(policy.allow_licenses, Some(vec!["MIT".to_owned()]));
        assert_eq!(policy.license_exceptions, vec!["legacy-pkg"]);
        assert!(policy.require_hashes);
    }

    #[test]
    fn finds_workspace_root_for_members() {
        let tmp = TempDir::new("workspace");
        let root = &tmp.0;
        fs::write(
            root.join(PYPROJECT),
            "[project]\nname = \"ws\"\n[tool.uv.workspace]\nmembers = [\"pkgs/*\"]\n[tool.vivar.policy]\ndeny = [\"root-denied\"]\n",
        )
        .unwrap();
        let member = root.join("pkgs").join("a");
        fs::create_dir_all(&member).unwrap();
        fs::write(
            member.join(PYPROJECT),
            "[project]\nname = \"a\"\n[tool.vivar.policy]\ndeny = [\"member-denied\"]\n",
        )
        .unwrap();
        let project = Project::find(&member.join("src")).unwrap().unwrap();
        assert_eq!(project.root, member);
        assert_eq!(project.venv_root(), root.as_path());
        let deny = project.policy().unwrap().deny;
        assert!(
            deny.contains(&"member-denied".to_owned()) && deny.contains(&"root-denied".to_owned())
        );
        let at_root = Project::find(root).unwrap().unwrap();
        assert!(at_root.workspace.is_none());
        assert_eq!(at_root.venv_root(), root.as_path());

        // A nested project outside the member globs keeps its own env.
        let stray = root.join("tools").join("x");
        fs::create_dir_all(&stray).unwrap();
        fs::write(stray.join(PYPROJECT), "[project]\nname = \"x\"\n").unwrap();
        let stray_project = Project::find(&stray).unwrap().unwrap();
        assert!(stray_project.workspace.is_none());
        assert_eq!(stray_project.venv_root(), stray.as_path());
    }
}
