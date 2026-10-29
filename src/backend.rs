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
pub trait Backend {
    fn add(&self, dir: &Path, packages: &[String], group: Option<&str>) -> Result<()>;
    fn remove(&self, dir: &Path, packages: &[String]) -> Result<()>;
    fn lock(&self, dir: &Path) -> Result<()>;
    fn sync(&self, dir: &Path) -> Result<()>;
    /// Like `sync`, also installing the named dependency group.
    fn sync_group(&self, dir: &Path, group: Option<&str>) -> Result<()>;
    /// Re-resolves `packages` (all when empty) to their newest allowed versions;
    /// `dry_run` only reports what would change.
    fn upgrade(&self, dir: &Path, packages: &[String], dry_run: bool) -> Result<()>;
    /// Builds the project's sdist and wheel into `dist/`.
    fn build(&self, dir: &Path) -> Result<()>;
    /// Installs requirement specs into the environment whose interpreter is `python`.
    fn install(&self, python: &Path, specs: &[String]) -> Result<()>;
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
    fn add(&self, dir: &Path, packages: &[String], group: Option<&str>) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("add");
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

    fn sync_group(&self, dir: &Path, group: Option<&str>) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("sync");
        if let Some(group) = group {
            cmd.args(["--group", group]);
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

    fn build(&self, dir: &Path) -> Result<()> {
        let mut cmd = self.uv(dir);
        cmd.arg("build");
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
        if let Some(group) = group {
            export.args(["--group", group]);
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
