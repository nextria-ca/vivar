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

/// Command files this env linked into `~/.local/bin`, one per line.
const LINKS: &str = "vivar-links.txt";
/// The requirement and Python request the env was built from.
const SPEC: &str = "vivar-tool.txt";

pub fn tools_dir() -> Result<PathBuf> {
    Ok(dirs::data()?.join("tools"))
}

/// A tool's env directory. Only bare package names make keys: a path or URL
/// requirement must say which package it is (`name @ ./path`).
fn tool_env(name: &str) -> Result<PathBuf> {
    let name = project::normalize(name);
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if !plain {
        bail!("cannot name a tool after {name:?}; use `--from \"<name> @ <path-or-url>\"`");
    }
    let dir = tools_dir()?;
    let env = dir.join(&name);
    if !env.starts_with(&dir) {
        bail!("refusing to use {} as a tool environment", env.display());
    }
    Ok(env)
}

/// A command an env provides: how it's shown, and the file in its bin directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Cmd {
    name: String,
    file: String,
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
    let env = tool_env(project::requirement_name(package))?;
    let name = env
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
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
    // Only commands that are still ours get re-linked after the rebuild.
    let bin = dirs::bin()?;
    let previously_linked: Vec<String> = links_of(&env)
        .into_iter()
        .filter(|f| is_tool_link(&bin.join(f)).unwrap_or(false))
        .collect();
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
    if !previously_linked.is_empty() {
        // An installed tool stays installed when a run rebuilds its env.
        let commands: Vec<Cmd> = commands_of(&env)?
            .into_iter()
            .filter(|c| previously_linked.contains(&c.file))
            .collect();
        link_commands(&env, &commands)?;
    }
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
    let commands = commands_of(&env)?;
    if commands.is_empty() {
        println!(
            "installed {package} into {} (it provides no commands)",
            env.display()
        );
        return Ok(());
    }
    link_commands(&env, &commands)?;
    let bin = dirs::bin()?;
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    println!(
        "installed {}: {} in {}",
        package,
        names.join(", "),
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

/// Links `commands` from `env` into `~/.local/bin`. Every collision is checked
/// before anything is touched, and the ownership manifest is written last.
fn link_commands(env: &Path, commands: &[Cmd]) -> Result<()> {
    let bin = dirs::bin()?;
    fs::create_dir_all(&bin)?;
    for c in commands {
        let link = bin.join(&c.file);
        if link.symlink_metadata().is_ok() && !is_tool_link(&link)? {
            bail!(
                "refusing to replace {}: it was not installed by vivar",
                link.display()
            );
        }
    }
    for c in commands {
        let link = bin.join(&c.file);
        let _ = fs::remove_file(&link);
        let target = env.join(venv::BIN).join(&c.file);
        match native_shim(env, &c.name, &c.file) {
            // vivar's own shim resolves python relative to itself; the one in
            // ~/.local/bin names the tool env outright instead.
            Some(text) => {
                fs::write(&link, text).with_context(|| format!("writing {}", link.display()))?
            }
            None => link_or_copy(&target, &link)?,
        }
    }
    let files: Vec<&str> = commands.iter().map(|c| c.file.as_str()).collect();
    fs::write(env.join(LINKS), files.join("\n") + "\n")?;
    Ok(())
}

/// Whether a file in `~/.local/bin` points into (Unix) or is a copy of a
/// command in (Windows) one of vivar's tool environments.
fn is_tool_link(link: &Path) -> Result<bool> {
    let tools = tools_dir()?;
    #[cfg(unix)]
    {
        Ok(fs::read_link(link)
            .map(|target| target.starts_with(&tools))
            .unwrap_or(false))
    }
    #[cfg(windows)]
    {
        let Some(name) = link.file_name() else {
            return Ok(false);
        };
        let Ok(mine) = fs::read(link) else {
            return Ok(false);
        };
        let Ok(entries) = fs::read_dir(&tools) else {
            return Ok(false);
        };
        // A copied launcher matches a tool env's byte for byte; a shim vivar
        // wrote is exactly what it would write for that env again.
        let file = name.to_string_lossy();
        let stem = file.strip_suffix(".cmd").unwrap_or(&file);
        Ok(entries.flatten().any(|e| {
            let env = e.path();
            fs::read(env.join(venv::BIN).join(name))
                .map(|theirs| theirs == mine)
                .unwrap_or(false)
                || native_shim(&env, stem, &file).is_some_and(|text| text.as_bytes() == mine)
        }))
    }
}

pub fn uninstall(package: &str) -> Result<()> {
    let env = tool_env(package)?;
    if !env.exists() {
        bail!("tool {} is not installed", project::normalize(package));
    }
    remove_links(&env);
    fs::remove_dir_all(&env)?;
    println!("uninstalled {}", project::normalize(package));
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
                if linked.contains(&c.file) {
                    format!("{} (linked)", c.name)
                } else {
                    c.name.clone()
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
/// executables wheels dropped straight into the bin directory (per RECORD).
fn commands_of(env: &Path) -> Result<Vec<Cmd>> {
    let site = venv::site_packages(env)?;
    let bin = fs::canonicalize(env.join(venv::BIN))?;
    let mut scripts: Vec<Cmd> = Vec::new();
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
                        let name = name.trim().to_owned();
                        // uv makes `name.exe` launchers on Windows; vivar's own
                        // installer makes `name.cmd` shims.
                        let shim = format!("{name}.cmd");
                        let file = if cfg!(windows)
                            && !bin.join(exe(&name)).exists()
                            && bin.join(&shim).exists()
                        {
                            shim
                        } else {
                            exe(&name)
                        };
                        scripts.push(Cmd { file, name });
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
                if full.parent() != Some(bin.as_path()) {
                    continue;
                }
                if let Some(file) = full.file_name().and_then(|n| n.to_str()) {
                    if let Some(stem) = file.strip_suffix("-script.py") {
                        // The body behind one of vivar's `.cmd` shims, not a command.
                        if cfg!(windows) && bin.join(format!("{stem}.cmd")).exists() {
                            continue;
                        }
                    }
                    let name = file
                        .strip_suffix(std::env::consts::EXE_SUFFIX)
                        .or_else(|| file.strip_suffix(".cmd"))
                        .unwrap_or(file);
                    if !name.starts_with("python") && !name.starts_with("activate") {
                        scripts.push(Cmd {
                            name: name.to_owned(),
                            file: file.to_owned(),
                        });
                    }
                }
            }
        }
    }
    scripts.sort();
    scripts.dedup();
    Ok(scripts)
}

/// Linked command files. Manifests written before file names were recorded
/// hold bare command names; those map onto the file the env actually has.
fn links_of(env: &Path) -> Vec<String> {
    let bin = env.join(venv::BIN);
    fs::read_to_string(env.join(LINKS))
        .map(|t| {
            t.lines()
                .filter(|l| !l.is_empty())
                .map(|l| {
                    if !bin.join(l).exists() && bin.join(exe(l)).exists() {
                        exe(l)
                    } else {
                        l.to_owned()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Removes the commands this env linked, and only those that still belong to it.
pub fn remove_links(env: &Path) {
    let Ok(bin) = dirs::bin() else { return };
    for file in links_of(env) {
        let link = bin.join(&file);
        if is_tool_link(&link).unwrap_or(false) {
            let _ = fs::remove_file(&link);
        }
    }
    let _ = fs::remove_file(env.join(LINKS));
}

/// The `~/.local/bin` shim for a command that vivar's installer made as a
/// `name.cmd` + `name-script.py` pair in `env` (Windows); `None` for uv's
/// launchers and for batch files a wheel ships itself, which are copied.
fn native_shim(env: &Path, name: &str, file: &str) -> Option<String> {
    if !cfg!(windows) || !file.ends_with(".cmd") {
        return None;
    }
    let scripts = env.join(venv::BIN);
    let body = scripts.join(format!("{name}-script.py"));
    if !body.is_file() {
        return None;
    }
    Some(format!(
        "@echo off\r\n\"{}\" \"{}\" %*\r\n",
        scripts.join("python.exe").display(),
        body.display()
    ))
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
    extra: &[(String, String)],
) -> Result<i32> {
    let meta = script_metadata(script)?
        .ok_or_else(|| anyhow!("{} has no `# /// script` metadata block", script.display()))?;
    let env = script_env(backend, policy, &meta)?;
    let mut full = vec!["python".to_owned(), script.to_string_lossy().into_owned()];
    full.extend_from_slice(args);
    venv::exec_with(&env, &full, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_must_be_plain() {
        assert!(tool_env("Ruff").is_ok());
        assert!(tool_env("my_tool.cli").is_ok());
        assert!(tool_env("/tmp/my-tool").is_err());
        assert!(tool_env("../x").is_err());
        assert!(tool_env("C:\\tools\\x").is_err());
        assert!(tool_env("").is_err());
        assert!(tool_env("git+https://x/y").is_err());
    }

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
