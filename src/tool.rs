//! Isolated tool environments (`vivar tool …`) and throwaway environments for
//! PEP 723 single-file scripts. Both are vivar venvs filled by the backend.

use crate::backend::Backend;
use crate::{dirs, project, sha256, venv};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn tools_dir() -> Result<PathBuf> {
    Ok(dirs::data()?.join("tools"))
}

fn tool_env(name: &str) -> Result<PathBuf> {
    Ok(tools_dir()?.join(project::normalize(name)))
}

/// Creates (if needed) a tool env for `package` and returns it.
pub fn ensure(backend: &dyn Backend, package: &str, python: Option<&str>) -> Result<PathBuf> {
    let name = project::normalize(project::requirement_name(package));
    let env = tool_env(&name)?;
    if !env.join("pyvenv.cfg").exists() {
        let interpreter = venv::interpreter(python)?;
        venv::create(&env, &interpreter, &name)?;
        if let Err(e) = backend.install(&venv::python(&env), &[package.to_owned()]) {
            let _ = fs::remove_dir_all(&env);
            return Err(e);
        }
    }
    Ok(env)
}

/// `vivar tool install`: the env plus its console scripts linked into `~/.local/bin`.
pub fn install(backend: &dyn Backend, package: &str, python: Option<&str>) -> Result<()> {
    let env = ensure(backend, package, python)?;
    let bin = dirs::bin()?;
    fs::create_dir_all(&bin)?;
    let scripts = console_scripts(&env)?;
    if scripts.is_empty() {
        println!(
            "installed {package} into {} (it provides no commands)",
            env.display()
        );
        return Ok(());
    }
    for script in &scripts {
        let target = env.join(venv::BIN).join(exe(script));
        let link = bin.join(exe(script));
        let _ = fs::remove_file(&link);
        link_or_copy(&target, &link)?;
    }
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
    let bin = dirs::bin()?;
    for script in console_scripts(&env).unwrap_or_default() {
        let _ = fs::remove_file(bin.join(exe(&script)));
    }
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
        let scripts = console_scripts(&dir.join(&name)).unwrap_or_default();
        println!("{name:<24} {}", scripts.join(", "));
    }
    Ok(())
}

/// Runs `cmd` from the tool env of `package` (the command itself by default).
pub fn run(
    backend: &dyn Backend,
    package: Option<&str>,
    cmd: &str,
    args: &[String],
    python: Option<&str>,
) -> Result<i32> {
    let env = ensure(backend, package.unwrap_or(cmd), python)?;
    let mut full = vec![cmd.to_owned()];
    full.extend_from_slice(args);
    venv::exec(&env, &full)
}

/// Console scripts of the packages in an env, from `entry_points.txt`.
fn console_scripts(env: &Path) -> Result<Vec<String>> {
    let site = venv::site_packages(env)?;
    let mut scripts = Vec::new();
    for entry in fs::read_dir(&site)?.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "dist-info") {
            continue;
        }
        let Ok(text) = fs::read_to_string(path.join("entry_points.txt")) else {
            continue;
        };
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
    scripts.sort();
    scripts.dedup();
    Ok(scripts)
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
    let Some(_) = lines.by_ref().find(|l| l.trim_end() == "# /// script") else {
        return Ok(None);
    };
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
pub fn script_env(backend: &dyn Backend, meta: &ScriptMeta) -> Result<PathBuf> {
    // `>=3.12` → `3.12`; anything fancier just takes the newest installed.
    let python = meta
        .requires_python
        .as_deref()
        .and_then(|r| r.strip_prefix(">="))
        .map(|v| v.trim().to_owned());
    let interpreter = venv::interpreter(python.as_deref())?;
    let mut hasher = sha256::Sha256::new();
    hasher.update(interpreter.key.as_bytes());
    for dep in &meta.dependencies {
        hasher.update(b"\n");
        hasher.update(dep.as_bytes());
    }
    let key = &sha256::hex(&hasher.finish())[..16];
    let env = dirs::cache()?.join("scripts").join(key);
    if !env.join("pyvenv.cfg").exists() {
        venv::create(&env, &interpreter, "script")?;
        if !meta.dependencies.is_empty() {
            if let Err(e) = backend.install(&venv::python(&env), &meta.dependencies) {
                let _ = fs::remove_dir_all(&env);
                return Err(e);
            }
        }
    }
    Ok(env)
}

/// `vivar run script.py` for a file with inline metadata.
pub fn run_script(backend: &dyn Backend, script: &Path, args: &[String]) -> Result<i32> {
    let meta = script_metadata(script)?
        .ok_or_else(|| anyhow!("{} has no `# /// script` metadata block", script.display()))?;
    let env = script_env(backend, &meta)?;
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
