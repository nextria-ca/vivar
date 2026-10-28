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
}

const UV_INSTALL_HINT: &str = "install it with `curl -LsSf https://astral.sh/uv/install.sh | sh`";

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
}

impl UvBackend {
    pub fn new(policy: &Policy) -> Self {
        UvBackend {
            index_url: policy.index_url.clone(),
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
                cmd.env("UV_DEFAULT_INDEX", url);
            }
            None => {}
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

    fn install(&self, python: &Path, specs: &[String]) -> Result<()> {
        let dir = python.parent().unwrap_or(python);
        let mut cmd = self.uv(dir);
        cmd.args(["pip", "install", "--python"])
            .arg(python)
            .args(specs);
        Self::check(cmd)
    }
}
