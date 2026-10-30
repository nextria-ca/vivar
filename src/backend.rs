//! Engines that do the actual resolving/installing.
//!
//! Phase 0 delegates to the `uv` binary for add/remove/lock/sync. Later phases
//! add an in-process backend (pubgrub + rattler) behind the same trait.

use crate::project::Policy;
use anyhow::{anyhow, bail, Result};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

/// Dependency resolution and installation. Everything else (projects, venvs,
/// Python builds, running, bundles) is vivar's own.
/// One difference a re-resolve would make to the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    /// The locked version, or none when the package would be added.
    pub from: Option<String>,
    /// The new version, or none when the package would be removed.
    pub to: Option<String>,
}

impl Change {
    /// `Would update six v1.16.0 -> v1.17.0` (or `Updated`, when done).
    pub fn describe(&self, done: bool) -> String {
        match (&self.from, &self.to) {
            (Some(f), Some(t)) => format!(
                "{} {} v{f} -> v{t}",
                if done { "Updated" } else { "Would update" },
                self.name
            ),
            (None, Some(t)) => format!(
                "{} {} v{t}",
                if done { "Added" } else { "Would add" },
                self.name
            ),
            (Some(f), None) => format!(
                "{} {} v{f}",
                if done { "Removed" } else { "Would remove" },
                self.name
            ),
            (None, None) => self.name.clone(),
        }
    }

    pub fn json(&self) -> String {
        let opt = |v: &Option<String>| v.as_deref().map_or("null".to_owned(), crate::json::quote);
        format!(
            "{{\"name\":{},\"from\":{},\"to\":{}}}",
            crate::json::quote(&self.name),
            opt(&self.from),
            opt(&self.to)
        )
    }
}

pub trait Backend {
    /// Adds requirements (or local paths) to the project; `editable` installs
    /// a local directory editable.
    fn add(
        &self,
        dir: &Path,
        packages: &[String],
        group: Option<&str>,
        editable: bool,
    ) -> Result<()>;
    fn remove(&self, dir: &Path, packages: &[String]) -> Result<()>;
    fn lock(&self, dir: &Path) -> Result<()>;
    fn sync(&self, dir: &Path) -> Result<()>;
    /// Like `sync`, also installing the named dependency group.
    fn sync_group(&self, dir: &Path, group: Option<&str>) -> Result<()> {
        self.sync_groups(dir, &group.into_iter().collect::<Vec<_>>())
    }
    /// Syncs with these dependency groups and these extras of the project.
    fn sync_with(&self, dir: &Path, groups: &[&str], extras: &[&str]) -> Result<()>;
    /// Like `sync`, also installing these dependency groups.
    fn sync_groups(&self, dir: &Path, groups: &[&str]) -> Result<()> {
        self.sync_with(dir, groups, &[])
    }
    /// Re-resolves `packages` (all when empty) to their newest allowed versions;
    /// `dry_run` only reports what would change.
    fn upgrade(&self, dir: &Path, packages: &[String], dry_run: bool) -> Result<()>;
    /// What a full re-resolve would change, without touching the lock.
    fn outdated(&self, dir: &Path) -> Result<Vec<Change>>;
    /// Builds the project's sdist and wheel into `dist/`.
    fn build(&self, dir: &Path) -> Result<()>;
    /// Installs requirement specs into the environment whose interpreter is `python`.
    fn install(&self, python: &Path, specs: &[String]) -> Result<()>;
    /// Adds every requirement in a pip requirements file (`vivar import`).
    fn add_requirements(&self, dir: &Path, file: &Path, group: Option<&str>) -> Result<()>;
    /// Fails when `uv.lock` is missing or would change on re-resolve.
    fn lock_check(&self, dir: &Path) -> Result<()>;
    /// Writes the lock as a pinned, hashed `requirements.txt`.
    fn export_requirements(&self, dir: &Path, output: &Path) -> Result<()>;
    /// Lays out the lock's packages for another machine (`triple`, `python`
    /// as `3.12.4`) into `target`: site-packages plus a `bin/` of scripts,
    /// optionally with a dependency group (profile). Wheels only; nothing is
    /// executed; each package comes from the artifact the lock pins.
    fn install_for_target(
        &self,
        dir: &Path,
        triple: &str,
        python: &str,
        group: Option<&str>,
        target: &Path,
    ) -> Result<()>;
}

const UV_INSTALL_HINT: &str = "install it with `curl -LsSf https://astral.sh/uv/install.sh | sh`";

/// Credentials for the mirror, from the environment: `VIVAR_INDEX_TOKEN` (sent
/// as the password with user `__token__`, Artifactory/Nexus style) or
/// `VIVAR_INDEX_USERNAME` + `VIVAR_INDEX_PASSWORD`. A `~/.netrc` entry works
/// too, since both uv and curl read it.
pub fn index_credentials() -> Option<(String, String)> {
    if let Ok(token) = std::env::var("VIVAR_INDEX_TOKEN") {
        return Some(("__token__".to_owned(), token));
    }
    match (
        std::env::var("VIVAR_INDEX_USERNAME"),
        std::env::var("VIVAR_INDEX_PASSWORD"),
    ) {
        (Ok(u), Ok(p)) => Some((u, p)),
        _ => None,
    }
}

/// The index URL with the configured credentials in its userinfo, if any.
pub fn with_credentials(url: &str) -> String {
    match (index_credentials(), url.split_once("://")) {
        (Some((user, pass)), Some((scheme, rest))) if !rest.contains('@') => {
            format!("{scheme}://{}:{}@{rest}", percent(&user), percent(&pass))
        }
        _ => url.to_owned(),
    }
}

fn percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether the `uv` binary is reachable on PATH.
pub fn uv_available() -> bool {
    Command::new("uv")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub struct UvBackend {
    /// Replaces PyPI when set (`[tool.vivar.policy] index-url`): a mirror URL, or a
    /// local directory of wheels that is then the only source.
    index_url: Option<String>,
    /// `require-wheels`: never build from source.
    no_build: bool,
}

impl UvBackend {
    pub fn new(policy: &Policy) -> Self {
        UvBackend {
            index_url: policy.index_url.clone(),
            no_build: policy.require_wheels,
        }
    }

    fn uv(&self, dir: &Path) -> Command {
        let mut cmd = Command::new("uv");
        cmd.current_dir(dir);
        match &self.index_url {
            // A directory of wheels (an offline wheelhouse) is the only package source.
            Some(dir) if Path::new(dir).is_dir() => {
                cmd.env("UV_FIND_LINKS", dir).env("UV_NO_INDEX", "1");
            }
            Some(url) => {
                cmd.env("UV_DEFAULT_INDEX", with_credentials(url));
            }
            None => {}
        }
        if self.no_build {
            cmd.env("UV_NO_BUILD", "1");
        }
        cmd
    }

    /// Runs uv and returns everything it printed (both streams), failing on a
    /// non-zero exit with that output as the message.
    fn capture(mut cmd: Command) -> Result<String> {
        let out = cmd.output().map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                anyhow!("`uv` was not found on PATH; vivar currently delegates dependency resolution to it. {UV_INSTALL_HINT}")
            } else {
                anyhow!(e).context("failed to run `uv`")
            }
        })?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if !out.status.success() {
            bail!(
                "uv exited with status {}:\n{}",
                out.status.code().unwrap_or(1),
                text.trim_end()
            );
        }
        Ok(text)
    }

    fn check(mut cmd: Command) -> Result<()> {
        let status = cmd.status().map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                anyhow!("`uv` was not found on PATH; vivar currently delegates dependency resolution to it. {UV_INSTALL_HINT}")
            } else {
                anyhow!(e).context("failed to run `uv`")
            }
        })?;
        match status.code().unwrap_or(1) {
            0 => Ok(()),
            code => bail!("uv exited with status {code}"),
        }
    }
}

impl Backend for UvBackend {
    fn add(
        &self,
        dir: &Path,
        packages: &[String],
        group: Option<&str>,
        editable: bool,
    ) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("add");
        if editable {
            cmd.arg("--editable");
        }
        if let Some(group) = group {
            cmd.args(["--group", group]);
        }
        cmd.args(packages);
        Self::check(cmd)
    }

    fn remove(&self, dir: &Path, packages: &[String]) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("remove").args(packages);
        Self::check(cmd)
    }

    fn lock(&self, dir: &Path) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("lock");
        Self::check(cmd)
    }

    fn sync(&self, dir: &Path) -> Result<()> {
        self.sync_group(dir, None)
    }

    fn sync_with(&self, dir: &Path, groups: &[&str], extras: &[&str]) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("sync");
        for group in groups {
            cmd.args(["--group", group]);
        }
        for extra in extras {
            cmd.args(["--extra", extra]);
        }
        Self::check(cmd)
    }

    fn upgrade(&self, dir: &Path, packages: &[String], dry_run: bool) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("lock");
        if packages.is_empty() {
            cmd.arg("--upgrade");
        }
        for p in packages {
            cmd.args(["--upgrade-package", p]);
        }
        if dry_run {
            cmd.arg("--dry-run");
        }
        Self::check(cmd)
    }

    fn outdated(&self, dir: &Path) -> Result<Vec<Change>> {
        let mut cmd = self.uv(dir);
        cmd.args(["lock", "--upgrade", "--dry-run"]);
        // uv prints one line per change: `Update six v1.16.0 -> v1.17.0`,
        // `Add x v1.0`, `Remove y v2.0`.
        // A forked lock lists several versions a side: `v1.26.4, v2.1.0 -> v1.26.4, v2.2.0`.
        let mut changes = Vec::new();
        for line in Self::capture(cmd)?.lines() {
            let line = line.trim();
            let Some((action, rest)) = line.split_once(' ') else {
                continue;
            };
            let Some((name, versions)) = rest.split_once(' ') else {
                continue;
            };
            let strip = |v: &str| {
                v.split(',')
                    .map(|p| p.trim().trim_start_matches('v'))
                    .filter(|p| !p.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let name = crate::project::normalize(name);
            match action {
                "Update" | "Updated" => {
                    if let Some((from, to)) = versions.split_once(" -> ") {
                        changes.push(Change {
                            name,
                            from: Some(strip(from)),
                            to: Some(strip(to)),
                        });
                    }
                }
                "Add" | "Added" => changes.push(Change {
                    name,
                    from: None,
                    to: Some(strip(versions)),
                }),
                "Remove" | "Removed" => changes.push(Change {
                    name,
                    from: Some(strip(versions)),
                    to: None,
                }),
                _ => {}
            }
        }
        Ok(changes)
    }

    fn build(&self, dir: &Path) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("build");
        Self::check(cmd)
    }

    fn add_requirements(&self, dir: &Path, file: &Path, group: Option<&str>) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.args(["add", "--requirements"]).arg(file);
        if let Some(group) = group {
            cmd.args(["--group", group]);
        }
        Self::check(cmd)
    }

    fn lock_check(&self, dir: &Path) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.args(["lock", "--check"]);
        Self::check(cmd)
    }

    fn export_requirements(&self, dir: &Path, output: &Path) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.args([
            "export",
            "--format",
            "requirements.txt",
            "--no-dev",
            "--frozen",
            "--quiet",
            "--output-file",
        ])
        .arg(output);
        Self::check(cmd)
    }

    fn install_for_target(
        &self,
        dir: &Path,
        triple: &str,
        python: &str,
        group: Option<&str>,
        target: &Path,
    ) -> Result<()> {
        // pylock.toml carries each package's artifact URLs, so per-package
        // index routing (the PyTorch indexes) survives the trip.
        let lock = std::env::temp_dir().join(format!("pylock.vivar-{}.toml", std::process::id()));
        let mut export = self.uv(dir);
        export
            .args([
                "export",
                "--format",
                "pylock.toml",
                "--no-dev",
                "--frozen",
                "--no-emit-project",
                "--quiet",
                "--output-file",
            ])
            .arg(&lock);
        if let Some(profile) = group {
            // A profile is a group, or an extra declared as one.
            let project = crate::project::Project::require(dir)?;
            if project.profile_selector(profile).is_group() {
                export.args(["--group", profile]);
            } else {
                export.args(["--extra", profile]);
            }
        }
        Self::check(export)?;
        // The host interpreter only drives the layout (same series is enough);
        // `--python-version` is the exact target version markers resolve against.
        let series = python.split('.').take(2).collect::<Vec<_>>().join(".");
        let mut cmd = self.uv(dir);
        cmd.args([
            "pip",
            "install",
            "--reinstall",
            "--only-binary",
            ":all:",
            "--python",
            &series,
            "--python-platform",
            triple,
            "--python-version",
            python,
            "--target",
        ])
        .arg(target)
        .arg("--requirements")
        .arg(&lock);
        let result = Self::check(cmd);
        let _ = std::fs::remove_file(&lock);
        result
    }

    fn install(&self, python: &Path, specs: &[String]) -> Result<()> {
        let dir = python.parent().unwrap_or(python);
        let mut cmd = self.uv(dir);
        cmd.args(["pip", "install", "--python"])
            .arg(python)
            .args(specs);
        Self::check(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_go_into_the_index_url() {
        std::env::remove_var("VIVAR_INDEX_TOKEN");
        std::env::set_var("VIVAR_INDEX_USERNAME", "svc user");
        std::env::set_var("VIVAR_INDEX_PASSWORD", "p@ss/w");
        assert_eq!(
            with_credentials("https://mirror.example/simple"),
            "https://svc%20user:p%40ss%2Fw@mirror.example/simple"
        );
        // An URL that already carries credentials is left alone.
        assert_eq!(
            with_credentials("https://a:b@m/simple"),
            "https://a:b@m/simple"
        );
        std::env::remove_var("VIVAR_INDEX_USERNAME");
        std::env::remove_var("VIVAR_INDEX_PASSWORD");
        assert_eq!(with_credentials("https://m/simple"), "https://m/simple");
    }
}
