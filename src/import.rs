//! `vivar import`: turn a requirements.txt, conda environment.yml, Pipfile or
//! poetry pyproject into a vivar project, or add their dependencies to one.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use toml_edit::{DocumentMut, Item};

/// What an import found: requirement strings per group (`None` = main).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Imported {
    pub requirements: Vec<(Option<String>, String)>,
    /// A Python series such as `3.12`, when the source pins one.
    pub python: Option<String>,
    /// A CUDA version, when a conda env named one (`pytorch-cuda=12.1`).
    pub cuda: Option<String>,
    /// Things vivar can't carry over (conda-only packages, editable installs …).
    pub skipped: Vec<String>,
    pub name: Option<String>,
}

pub fn read(path: &Path) -> Result<Imported> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if file == "pipfile" {
        pipfile(&text)
    } else if file.ends_with(".yml") || file.ends_with(".yaml") {
        conda(&text)
    } else if file == "pyproject.toml" {
        poetry(&text)
    } else {
        requirements(&text, path.parent().unwrap_or(Path::new(".")))
    }
}

/// pip requirements: one requirement per line, `-r` includes, comments.
fn requirements(text: &str, base: &Path) -> Result<Imported> {
    let mut out = Imported::default();
    // Backslash continuations are one logical line.
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    for raw in joined.lines() {
        let line = raw.split(" #").next().unwrap_or(raw).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("-r ")
            .or_else(|| line.strip_prefix("--requirement "))
        {
            let nested = base.join(rest.trim());
            let inner = requirements(
                &fs::read_to_string(&nested)
                    .with_context(|| format!("reading {}", nested.display()))?,
                nested.parent().unwrap_or(base),
            )?;
            out.requirements.extend(inner.requirements);
            out.skipped.extend(inner.skipped);
            continue;
        }
        if line.starts_with('-') {
            // -e ./pkg, --index-url, --hash … : policy and the lock handle those.
            out.skipped.push(line.to_owned());
            continue;
        }
        out.requirements.push((
            None,
            line.split(" --hash")
                .next()
                .unwrap_or(line)
                .trim()
                .to_owned(),
        ));
    }
    Ok(out)
}

/// Conda-only packages that have no PyPI counterpart (or are the runtime itself).
const CONDA_ONLY: &[&str] = &[
    "python",
    "pip",
    "setuptools",
    "wheel",
    "conda",
    "mkl",
    "openssl",
    "ca-certificates",
    "certifi-conda",
    "libgcc-ng",
    "libstdcxx-ng",
    "libgomp",
    "_libgcc_mutex",
    "_openmp_mutex",
    "ld_impl_linux-64",
    "ncurses",
    "readline",
    "tk",
    "xz",
    "zlib",
    "sqlite",
    "libffi",
    "bzip2",
    "tzdata",
    "expat",
    "libuuid",
    "libnsl",
    "libxcrypt",
    "libzlib",
    "libsqlite",
    "libexpat",
    "cudatoolkit",
    "cuda-toolkit",
    "cudnn",
    "nccl",
    "blas",
    "libblas",
    "libcblas",
    "liblapack",
    "intel-openmp",
    "vc",
    "vs2015_runtime",
    "ucrt",
];

/// conda `environment.yml`: `dependencies:` with `name=version` specs and a
/// nested `- pip:` list. Indentation-insensitive, which is all these files need.
fn conda(text: &str) -> Result<Imported> {
    let mut out = Imported::default();
    let mut section = "";
    let mut in_pip = false;
    for raw in text.lines() {
        let line = raw.split(" #").next().unwrap_or(raw).trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('-') {
            if let Some((key, value)) = trimmed.split_once(':') {
                section = key.trim();
                in_pip = false;
                if section == "name" && !value.trim().is_empty() {
                    out.name = Some(value.trim().trim_matches('"').trim_matches('\'').to_owned());
                }
                continue;
            }
        }
        if section != "dependencies" {
            continue;
        }
        let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix('-'))
        else {
            continue;
        };
        let item = item.trim().trim_matches('"').trim_matches('\'');
        if item == "pip:" {
            in_pip = true;
            continue;
        }
        if in_pip
            && !raw.trim_start().starts_with("- pip")
            && raw.len() - raw.trim_start().len() > 2
        {
            out.requirements.push((None, item.to_owned()));
            continue;
        }
        in_pip = false;
        // conda spec: name[=version[=build]] or name>=version; channel::name too.
        let spec = item.rsplit("::").next().unwrap_or(item);
        let (name, rest) = spec
            .find(|c: char| "=<>!~".contains(c))
            .map(|i| (&spec[..i], &spec[i..]))
            .unwrap_or((spec, ""));
        let name_l = name.to_lowercase();
        if name_l == "python" {
            let v = rest
                .trim_start_matches(['=', '>'])
                .split('=')
                .next()
                .unwrap_or("");
            let series: String = v.split('.').take(2).collect::<Vec<_>>().join(".");
            if !series.is_empty() {
                out.python = Some(series);
            }
            continue;
        }
        if name_l == "pytorch-cuda" || name_l == "cuda-version" {
            let v = rest
                .trim_start_matches(['=', '>'])
                .split('=')
                .next()
                .unwrap_or("");
            if !v.is_empty() {
                out.cuda = Some(v.to_owned());
            }
            continue;
        }
        if CONDA_ONLY.contains(&name_l.as_str())
            || name_l.starts_with("lib")
            || name_l.starts_with("cuda-")
        {
            out.skipped.push(item.to_owned());
            continue;
        }
        let pypi = match name_l.as_str() {
            "pytorch" => "torch",
            "pytables" => "tables",
            "py-opencv" | "opencv" => "opencv-python",
            "pillow-simd" => "pillow",
            "msgpack-python" => "msgpack",
            other => other,
        };
        // name=1.2 means that version series; name=1.2=build drops the build string.
        let req = if let Some(v) = rest.strip_prefix("==") {
            format!("{pypi}=={}", v.split('=').next().unwrap_or(v))
        } else if let Some(v) = rest.strip_prefix('=') {
            let v = v.split('=').next().unwrap_or(v);
            if v.contains('*') {
                format!("{pypi}=={v}")
            } else {
                format!("{pypi}=={v}.*")
            }
        } else {
            format!("{pypi}{rest}")
        };
        out.requirements.push((None, req));
    }
    Ok(out)
}

/// Pipfile: `[packages]`, `[dev-packages]`, `[requires] python_version`.
fn pipfile(text: &str) -> Result<Imported> {
    let doc: DocumentMut = text.parse().context("parsing Pipfile")?;
    let mut out = Imported::default();
    for (table, group) in [("packages", None), ("dev-packages", Some("dev".to_owned()))] {
        let Some(t) = doc.get(table).and_then(Item::as_table_like) else {
            continue;
        };
        for (name, item) in t.iter() {
            out.requirements
                .push((group.clone(), pipfile_spec(name, item)));
        }
    }
    out.python = doc
        .get("requires")
        .and_then(|r| r.get("python_version"))
        .and_then(Item::as_str)
        .map(str::to_owned);
    Ok(out)
}

fn pipfile_spec(name: &str, item: &Item) -> String {
    if let Some(v) = item.as_str() {
        return if v == "*" {
            name.to_owned()
        } else {
            format!("{name}{v}")
        };
    }
    let extras = item
        .get("extras")
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|e| !e.is_empty())
        .map(|e| format!("[{e}]"))
        .unwrap_or_default();
    let version = item
        .get("version")
        .and_then(Item::as_str)
        .filter(|v| *v != "*")
        .unwrap_or("");
    format!("{name}{extras}{version}")
}

/// poetry's `[tool.poetry.dependencies]` and `[tool.poetry.group.*.dependencies]`.
fn poetry(text: &str) -> Result<Imported> {
    let doc: DocumentMut = text.parse().context("parsing pyproject.toml")?;
    let Some(poetry) = doc.get("tool").and_then(|t| t.get("poetry")) else {
        bail!(
            "this pyproject.toml has no [tool.poetry] section; a PEP 621 project needs no import"
        );
    };
    let mut out = Imported {
        name: poetry.get("name").and_then(Item::as_str).map(str::to_owned),
        ..Default::default()
    };
    let mut collect = |deps: Option<&Item>, group: Option<String>| {
        let Some(t) = deps.and_then(Item::as_table_like) else {
            return;
        };
        for (name, item) in t.iter() {
            if name == "python" {
                if let Some(v) = item.as_str() {
                    out.python = Some(poetry_python_series(v));
                }
                continue;
            }
            if item.get("optional").and_then(Item::as_bool) == Some(true) {
                out.skipped.push(format!("{name} (optional)"));
                continue;
            }
            if item.get("path").is_some() || item.get("git").is_some() || item.get("url").is_some()
            {
                out.skipped.push(format!("{name} (path/git/url source)"));
                continue;
            }
            let constraint = item
                .as_str()
                .or_else(|| item.get("version").and_then(Item::as_str))
                .unwrap_or("*");
            let extras = item
                .get("extras")
                .and_then(Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|e| !e.is_empty())
                .map(|e| format!("[{e}]"))
                .unwrap_or_default();
            out.requirements.push((
                group.clone(),
                format!("{name}{extras}{}", poetry_constraint(constraint)),
            ));
        }
    };
    collect(poetry.get("dependencies"), None);
    collect(poetry.get("dev-dependencies"), Some("dev".to_owned()));
    if let Some(groups) = poetry.get("group").and_then(Item::as_table_like) {
        for (g, item) in groups.iter() {
            collect(item.get("dependencies"), Some(g.to_owned()));
        }
    }
    Ok(out)
}

/// `^1.2.3` → `>=1.2.3,<2`, `~1.2` → `>=1.2,<1.3`, `*` → nothing, `1.2` → `==1.2`.
pub fn poetry_constraint(c: &str) -> String {
    let c = c.trim();
    let parts = |v: &str| -> Vec<u64> {
        v.split('.')
            .filter_map(|p| p.trim_end_matches('*').parse().ok())
            .collect()
    };
    if c == "*" || c.is_empty() {
        return String::new();
    }
    if let Some(v) = c.strip_prefix('^') {
        let p = parts(v);
        let upper = match p.as_slice() {
            [0, 0, z] => format!("0.0.{}", z + 1),
            [0, y, ..] => format!("0.{}", y + 1),
            [x, ..] => format!("{}", x + 1),
            [] => return format!(">={v}"),
        };
        return format!(">={v},<{upper}");
    }
    if let Some(v) = c.strip_prefix('~') {
        let p = parts(v);
        let upper = match p.as_slice() {
            [x] => format!("{}", x + 1),
            [x, y, ..] => format!("{x}.{}", y + 1),
            [] => return format!(">={v}"),
        };
        return format!(">={v},<{upper}");
    }
    if c.starts_with(|ch: char| ch.is_ascii_digit()) {
        return format!("=={c}");
    }
    c.replace(' ', "")
}

fn poetry_python_series(c: &str) -> String {
    let v = c.trim().trim_start_matches(['^', '~', '>', '=', ' ']);
    v.split(',')
        .next()
        .unwrap_or(v)
        .trim()
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(i: &Imported) -> Vec<String> {
        i.requirements
            .iter()
            .map(|(g, r)| format!("{}:{r}", g.as_deref().unwrap_or("main")))
            .collect()
    }

    #[test]
    fn reads_requirements_with_includes() {
        let dir = std::env::temp_dir().join(format!("vivar-import-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("base.txt"), "six>=1.16  # comment\n").unwrap();
        fs::write(dir.join("requirements.txt"), "-r base.txt\n# all\nrequests[socks]==2.31.0 \\\n    --hash=sha256:abc\n-e ./local\n--index-url https://x/simple\n\npycodestyle\n").unwrap();
        let i = read(&dir.join("requirements.txt")).unwrap();
        assert_eq!(
            names(&i),
            vec![
                "main:six>=1.16",
                "main:requests[socks]==2.31.0",
                "main:pycodestyle"
            ]
        );
        assert_eq!(i.skipped.len(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_conda_environments() {
        let i = conda("name: ml\nchannels:\n  - conda-forge\ndependencies:\n  - python=3.11\n  - numpy=1.26\n  - pytorch=2.3.0=cuda*\n  - pytorch-cuda=12.1\n  - pandas>=2.0\n  - libgcc-ng\n  - pip\n  - pip:\n    - rich\n    - typer>=0.9\n").unwrap();
        assert_eq!(i.name.as_deref(), Some("ml"));
        assert_eq!(i.python.as_deref(), Some("3.11"));
        assert_eq!(i.cuda.as_deref(), Some("12.1"));
        assert_eq!(
            names(&i),
            vec![
                "main:numpy==1.26.*",
                "main:torch==2.3.0.*",
                "main:pandas>=2.0",
                "main:rich",
                "main:typer>=0.9"
            ]
        );
        assert_eq!(i.skipped, vec!["libgcc-ng", "pip"]);
    }

    #[test]
    fn reads_pipfiles_and_poetry() {
        let i = pipfile("[[source]]\nurl = \"https://pypi.org/simple\"\n\n[packages]\nrequests = \"*\"\nflask = \">=2\"\nboto3 = {version = \"~=1.30\", extras = [\"crt\"]}\n\n[dev-packages]\npytest = \"*\"\n\n[requires]\npython_version = \"3.12\"\n").unwrap();
        assert_eq!(
            names(&i),
            vec![
                "main:requests",
                "main:flask>=2",
                "main:boto3[crt]~=1.30",
                "dev:pytest"
            ]
        );
        assert_eq!(i.python.as_deref(), Some("3.12"));

        let p = poetry("[tool.poetry]\nname = \"svc\"\n\n[tool.poetry.dependencies]\npython = \"^3.10\"\nrequests = \"^2.31\"\nnumpy = \"~1.26\"\nrich = \"*\"\npydantic = {version = \"2.5\", extras = [\"email\"]}\nlocal = {path = \"../local\"}\nextra = {version = \"1\", optional = true}\n\n[tool.poetry.group.dev.dependencies]\npytest = \"^8\"\n").unwrap();
        assert_eq!(p.name.as_deref(), Some("svc"));
        assert_eq!(p.python.as_deref(), Some("3.10"));
        assert_eq!(
            names(&p),
            vec![
                "main:requests>=2.31,<3",
                "main:numpy>=1.26,<1.27",
                "main:rich",
                "main:pydantic[email]==2.5",
                "dev:pytest>=8,<9"
            ]
        );
        assert_eq!(p.skipped.len(), 2);
        assert_eq!(poetry_constraint("^0.2.1"), ">=0.2.1,<0.3");
        assert_eq!(poetry_constraint("^0.0.3"), ">=0.0.3,<0.0.4");
        assert_eq!(poetry_constraint(">=1.0, <2.0"), ">=1.0,<2.0");
        assert!(poetry("[project]\nname = \"x\"\n").is_err());
    }
}
