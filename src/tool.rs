//! Isolated tool environments (`vivar tool …`) and throwaway environments for
//! PEP 723 single-file scripts. Both are vivar venvs filled by the backend and
//! held to the same policy as projects.

use crate::backend::Backend;
use crate::project::{self, Policy};
use crate::python::Installed;
use crate::{dirs, licenses, python, sha256, venv};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Commands this env linked into `~/.local/bin`, one per line.
const LINKS: &str = "vivar-links.txt";
/// The requirement and Python request the env was built from.
const SPEC: &str = "vivar-tool.txt";

pub fn tools_dir() -> Result<PathBuf> {
    Ok(dirs::data()?.join("tools"))
}

fn tool_env(name: &str) -> Result<PathBuf> {
    Ok(tools_dir()?.join(project::normalize(name)))
}

/// Builds `env` on `interpreter` and installs `specs` into it, under `policy`.
/// A failed or non-compliant install leaves nothing behind.
fn fill(
    backend: &dyn Backend,
    policy: &Policy,
    env: &Path,
    interpreter: &Installed,
    prompt: &str,
    specs: &[String],
) -> Result<()> {
    policy.check_deny(specs)?;
    venv::create(env, interpreter, prompt)?;
    let result = (|| {
        if !specs.is_empty() {
            backend.install(&venv::python(env), specs)?;
        }
        licenses::check(
            env,
            policy.allow_licenses.as_deref(),
            &policy.license_exceptions,
        )
    })();
    if let Err(e) = result {
        let _ = fs::remove_dir_all(env);
        return Err(e);
    }
    Ok(())
}

/// The tool env for `package` (a requirement spec), built or rebuilt as needed.
pub fn ensure(
    backend: &dyn Backend,
    policy: &Policy,
    package: &str,
    python_request: Option<&str>,
) -> Result<PathBuf> {
    let name = project::normalize(project::requirement_name(package));
    let env = tool_env(&name)?;
    let wanted = format!("{package}\n{}\n", python_request.unwrap_or(""));
    if env.join("pyvenv.cfg").exists()
        && fs::read_to_string(env.join(SPEC)).ok() == Some(wanted.clone())
    {
        // Reused envs still have to pass today's policy.
        policy.check_deny(&[package])?;
        licenses::check(
            &env,
            policy.allow_licenses.as_deref(),
            &policy.license_exceptions,
        )?;
        return Ok(env);
    }
    if env.exists() {
        remove_links(&env);
        fs::remove_dir_all(&env)?;
    }
    let interpreter = venv::interpreter(python_request)?;
    fill(
        backend,
        policy,
        &env,
        &interpreter,
        &name,
        &[package.to_owned()],
    )?;
    fs::write(env.join(SPEC), wanted)?;
    Ok(env)
}

/// `vivar tool install`: the env plus its commands linked into `~/.local/bin`.
pub fn install(
    backend: &dyn Backend,
    policy: &Policy,
    package: &str,
    python: Option<&str>,
) -> Result<()> {
    let env = ensure(backend, policy, package, python)?;
    let bin = dirs::bin()?;
    fs::create_dir_all(&bin)?;
    let scripts = commands_of(&env)?;
    if scripts.is_empty() {
        println!(
            "installed {package} into {} (it provides no commands)",
            env.display()
        );
        return Ok(());
    }
    let mine = links_of(&env);
    for script in &scripts {
        let link = bin.join(exe(script));
        if link.exists() && !mine.contains(script) && !owned_by_a_tool(script)? {
            bail!(
                "refusing to replace {}: it was not installed by vivar",
                link.display()
            );
        }
        let _ = fs::remove_file(&link);
        link_or_copy(&env.join(venv::BIN).join(exe(script)), &link)?;
    }
    fs::write(env.join(LINKS), scripts.join("\n") + "\n")?;
    println!(
        "installed {}: {} in {}",
        package,
        scripts.join(", "),
        bin.display()
    );
    if !std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == bin))
        .unwrap_or(false)
    {
        println!("note: {} is not on your PATH", bin.display());
    }
    Ok(())
}

pub fn uninstall(package: &str) -> Result<()> {
    let name = project::normalize(package);
    let env = tool_env(&name)?;
    if !env.exists() {
        bail!("tool {name} is not installed");
    }
    remove_links(&env);
    fs::remove_dir_all(&env)?;
    println!("uninstalled {name}");
    Ok(())
}

pub fn list() -> Result<()> {
    let dir = tools_dir()?;
    let mut names: Vec<String> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().join("pyvenv.cfg").exists())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    if names.is_empty() {
        println!("no tools installed; try `vivar tool install ruff`");
    }
    for name in names {
        let env = dir.join(&name);
        let linked = links_of(&env);
        let commands = commands_of(&env).unwrap_or_default();
        let shown: Vec<String> = commands
            .iter()
            .map(|c| {
                if linked.contains(c) {
                    format!("{c} (linked)")
                } else {
                    c.clone()
                }
            })
            .collect();
        println!("{name:<24} {}", shown.join(", "));
    }
    Ok(())
}

/// Runs `cmd` from the tool env of `package` (the command itself by default).
pub fn run(
    backend: &dyn Backend,
    policy: &Policy,
    package: Option<&str>,
    cmd: &str,
    args: &[String],
    python: Option<&str>,
) -> Result<i32> {
    let env = ensure(backend, policy, package.unwrap_or(cmd), python)?;
    let mut full = vec![cmd.to_owned()];
    full.extend_from_slice(args);
    venv::exec(&env, &full)
}

/// Commands an env provides: `[console_scripts]` entry points plus any
/// executables wheels dropped straight into the bin directory (recorded in RECORD).
fn commands_of(env: &Path) -> Result<Vec<String>> {
    let site = venv::site_packages(env)?;
    let bin = fs::canonicalize(env.join(venv::BIN))?;
    let mut scripts = Vec::new();
    for entry in fs::read_dir(&site)?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "dist-info") {
            continue;
        }
        if let Ok(text) = fs::read_to_string(path.join("entry_points.txt")) {
            let mut in_console = false;
            for line in text.lines().map(str::trim) {
                if line.starts_with('[') {
                    in_console = line == "[console_scripts]";
                } else if in_console {
                    if let Some((name, _)) = line.split_once('=') {
                        scripts.push(name.trim().to_owned());
                    }
                }
            }
        }
        if let Ok(record) = fs::read_to_string(path.join("RECORD")) {
            for line in record.lines() {
                let Some(file) = line.split(',').next() else {
                    continue;
                };
                if !file.starts_with("..") {
                    continue;
                }
                let Ok(full) = fs::canonicalize(site.join(file)) else {
                    continue;
                };
                if full.parent() == Some(bin.as_path()) {
                    if let Some(name) = full.file_name().and_then(|n| n.to_str()) {
                        let name = name
                            .strip_suffix(std::env::consts::EXE_SUFFIX)
                            .unwrap_or(name);
                        if !name.starts_with("python") && !name.starts_with("activate") {
                            scripts.push(name.to_owned());
                        }
                    }
                }
            }
        }
    }
    scripts.sort();
    scripts.dedup();
    Ok(scripts)
}

fn links_of(env: &Path) -> Vec<String> {
    fs::read_to_string(env.join(LINKS))
        .map(|t| {
            t.lines()
                .map(str::to_owned)
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether some tool env (not necessarily this one) linked `command`.
fn owned_by_a_tool(command: &str) -> Result<bool> {
    let Ok(entries) = fs::read_dir(tools_dir()?) else {
        return Ok(false);
    };
    Ok(entries
        .flatten()
        .any(|e| links_of(&e.path()).iter().any(|l| l == command)))
}

/// Removes the commands this env linked, and only those.
pub fn remove_links(env: &Path) {
    let Ok(bin) = dirs::bin() else { return };
    for command in links_of(env) {
        let _ = fs::remove_file(bin.join(exe(&command)));
    }
    let _ = fs::remove_file(env.join(LINKS));
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn link_or_copy(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
            .with_context(|| format!("linking {}", link.display()))
    }
    #[cfg(windows)]
    {
        fs::copy(target, link)
            .map(|_| ())
            .with_context(|| format!("copying {}", link.display()))
    }
}

/// PEP 723 inline metadata of a script, if it has a `# /// script` block.
pub struct ScriptMeta {
    pub dependencies: Vec<String>,
    pub requires_python: Option<String>,
}

pub fn script_metadata(path: &Path) -> Result<Option<ScriptMeta>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = text.lines();
    if lines
        .by_ref()
        .find(|l| l.trim_end() == "# /// script")
        .is_none()
    {
        return Ok(None);
    }
    let mut toml = String::new();
    for line in lines {
        if line.trim_end() == "# ///" {
            let doc: toml_edit::DocumentMut = toml
                .parse()
                .with_context(|| format!("parsing the script metadata in {}", path.display()))?;
            let dependencies = doc
                .get("dependencies")
                .and_then(toml_edit::Item::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let requires_python = doc
                .get("requires-python")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_owned);
            return Ok(Some(ScriptMeta {
                dependencies,
                requires_python,
            }));
        }
        let Some(rest) = line.strip_prefix('#') else {
            bail!("unterminated `# /// script` block in {}", path.display());
        };
        toml.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        toml.push('\n');
    }
    bail!("unterminated `# /// script` block in {}", path.display())
}

/// A cached environment for the script's dependencies, keyed by their hash.
pub fn script_env(backend: &dyn Backend, policy: &Policy, meta: &ScriptMeta) -> Result<PathBuf> {
    let interpreter = python::select(meta.requires_python.as_deref())?;
    let mut hasher = sha256::Sha256::new();
    hasher.update(interpreter.key.as_bytes());
    for dep in &meta.dependencies {
        hasher.update(b"\n");
        hasher.update(dep.as_bytes());
    }
    let key = &sha256::hex(&hasher.finish())[..16];
    let env = dirs::cache()?.join("scripts").join(key);
    if env.join("pyvenv.cfg").exists() {
        policy.check_deny(&meta.dependencies)?;
        licenses::check(
            &env,
            policy.allow_licenses.as_deref(),
            &policy.license_exceptions,
        )?;
    } else {
        fill(
            backend,
            policy,
            &env,
            &interpreter,
            "script",
            &meta.dependencies,
        )?;
    }
    Ok(env)
}

/// `vivar run script.py` for a file with inline metadata.
pub fn run_script(
    backend: &dyn Backend,
    policy: &Policy,
    script: &Path,
    args: &[String],
) -> Result<i32> {
    let meta = script_metadata(script)?
        .ok_or_else(|| anyhow!("{} has no `# /// script` metadata block", script.display()))?;
    let env = script_env(backend, policy, &meta)?;
    let mut full = vec!["python".to_owned(), script.to_string_lossy().into_owned()];
    full.extend_from_slice(args);
    venv::exec(&env, &full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inline_metadata() {
        let dir = std::env::temp_dir().join(format!("vivar-pep723-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.py");
        fs::write(
            &path,
            "#!/usr/bin/env python\n# /// script\n# requires-python = \">=3.12\"\n# dependencies = [\n#   \"six\",\n#   \"rich>=13\",\n# ]\n# ///\nimport six\n",
        )
        .unwrap();
        let meta = script_metadata(&path).unwrap().unwrap();
        assert_eq!(meta.dependencies, vec!["six", "rich>=13"]);
        assert_eq!(meta.requires_python.as_deref(), Some(">=3.12"));

        fs::write(&path, "print('plain')\n").unwrap();
        assert!(script_metadata(&path).unwrap().is_none());
        fs::write(&path, "# /// script\n# dependencies = []\n").unwrap();
        assert!(script_metadata(&path).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
