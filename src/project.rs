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

/// Settings read from `[tool.vivar.policy]`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Normalized package names that `vivar add`/`lock`/`sync`/`run` refuse.
    pub deny: Vec<String>,
    /// Package index to use instead of PyPI: an internal mirror URL, or a local
    /// directory of wheels (an offline wheelhouse).
    pub index_url: Option<String>,
}

/// A project rooted at the directory containing `pyproject.toml`.
pub struct Project {
    pub root: PathBuf,
    doc: DocumentMut,
}

impl Project {
    /// Walks up from `start` looking for a `pyproject.toml`, the same way uv does,
    /// so policy applies no matter which subdirectory a command runs from.
    pub fn find(start: &Path) -> Result<Option<Project>> {
        for dir in start.ancestors() {
            if dir.join(PYPROJECT).is_file() {
                return Self::load(dir).map(Some);
            }
        }
        Ok(None)
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

    pub fn policy(&self) -> Policy {
        let table = self
            .doc
            .get("tool")
            .and_then(|t| t.get("vivar"))
            .and_then(|v| v.get("policy"));
        let deny = table
            .and_then(|p| p.get("deny"))
            .and_then(Item::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(normalize).collect())
            .unwrap_or_default();
        let index_url = table
            .and_then(|p| p.get("index-url"))
            .and_then(Item::as_str)
            .map(|s| match s.contains("://") {
                true => s.to_owned(),
                // A wheelhouse directory, relative to the project if not absolute.
                false => self.root.join(s).to_string_lossy().into_owned(),
            });
        Policy { deny, index_url }
    }

    /// Refuses requirements whose package name is on the deny list.
    pub fn check_requirements<S: AsRef<str>>(&self, reqs: &[S]) -> Result<()> {
        let policy = self.policy();
        let blocked: Vec<&str> = reqs
            .iter()
            .map(AsRef::as_ref)
            .filter(|r| policy.deny.contains(&normalize(requirement_name(r))))
            .collect();
        if !blocked.is_empty() {
            bail!(
                "blocked by [tool.vivar.policy] deny list: {}",
                blocked.join(", ")
            );
        }
        Ok(())
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

/// `"pandas[excel]>=2; python_version>'3.9'"` -> `"pandas"`
fn requirement_name(req: &str) -> &str {
    let end = req
        .find(|c: char| "<>=!~[;@ ".contains(c))
        .unwrap_or(req.len());
    &req[..end]
}

/// PEP 503 name normalization (simplified): case-insensitive, `_`/`.` == `-`.
fn normalize(name: &str) -> String {
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
        assert_eq!(project.policy(), Policy::default());

        let project = project_with(
            &tmp.0,
            "[tool.vivar.policy]\ndeny = [\"a\"]\nindex-url = \"https://pypi.internal/simple\"\n",
        );
        assert_eq!(
            project.policy(),
            Policy {
                deny: vec!["a".into()],
                index_url: Some("https://pypi.internal/simple".into()),
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
}
