//! Engines that do the actual resolving/installing.
//!
//! Phase 0 delegates to the `uv` binary. Later phases add an in-process
//! backend (pubgrub + rattler) behind the same trait, so the CLI doesn't change.

use crate::project::Policy;
use anyhow::{anyhow, bail, Context, Result};
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;

pub trait Backend {
    fn init(&self, dir: &Path, name: Option<&str>, python: Option<&str>) -> Result<()>;
    fn add(&self, dir: &Path, packages: &[String], group: Option<&str>) -> Result<()>;
    fn remove(&self, dir: &Path, packages: &[String]) -> Result<()>;
    fn lock(&self, dir: &Path) -> Result<()>;
    fn sync(&self, dir: &Path) -> Result<()>;
    /// Returns the child's exit code.
    fn run(&self, dir: &Path, args: &[String]) -> Result<i32>;
    fn python_install(&self, version: &str) -> Result<()>;
    fn python_list(&self) -> Result<()>;
}

const UV_INSTALL_HINT: &str = "install it with `curl -LsSf https://astral.sh/uv/install.sh | sh`";

pub struct UvBackend {
    /// Replaces PyPI as the default index when set (`[tool.vivar.policy] index-url`).
    index_url: Option<String>,
}

impl UvBackend {
    pub fn new(policy: &Policy) -> Self {
        UvBackend {
            index_url: policy.index_url.clone(),
        }
    }

    fn uv(&self, dir: Option<&Path>) -> Command {
        let mut cmd = Command::new("uv");
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        if let Some(url) = &self.index_url {
            cmd.env("UV_DEFAULT_INDEX", url);
        }
        cmd
    }

    fn exec(mut cmd: Command) -> Result<i32> {
        let status = cmd.status().map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                anyhow!("`uv` was not found on PATH; vivar currently delegates to it. {UV_INSTALL_HINT}")
            } else {
                anyhow!(e).context("failed to run `uv`")
            }
        })?;
        Ok(status.code().unwrap_or(1))
    }

    fn check(cmd: Command) -> Result<()> {
        match Self::exec(cmd)? {
            0 => Ok(()),
            code => bail!("uv exited with status {code}"),
        }
    }
}

impl Backend for UvBackend {
    fn init(&self, dir: &Path, name: Option<&str>, python: Option<&str>) -> Result<()> {
        let mut cmd = self.uv(Some(dir));
        cmd.args(["init", "--app"]);
        if let Some(name) = name {
            cmd.args(["--name", name]);
        }
        if let Some(python) = python {
            cmd.args(["--python", python]);
        }
        Self::check(cmd).context("`vivar init` failed")
    }

    fn add(&self, dir: &Path, packages: &[String], group: Option<&str>) -> Result<()> {
        let mut cmd = self.uv(Some(dir));
        cmd.arg("add");
        if let Some(group) = group {
            cmd.args(["--group", group]);
        }
        cmd.args(packages);
        Self::check(cmd)
    }

    fn remove(&self, dir: &Path, packages: &[String]) -> Result<()> {
        let mut cmd = self.uv(Some(dir));
        cmd.arg("remove").args(packages);
        Self::check(cmd)
    }

    fn lock(&self, dir: &Path) -> Result<()> {
        let mut cmd = self.uv(Some(dir));
        cmd.arg("lock");
        Self::check(cmd)
    }

    fn sync(&self, dir: &Path) -> Result<()> {
        let mut cmd = self.uv(Some(dir));
        cmd.arg("sync");
        Self::check(cmd)
    }

    fn run(&self, dir: &Path, args: &[String]) -> Result<i32> {
        let mut cmd = self.uv(Some(dir));
        cmd.args(["run", "--"]).args(args);
        Self::exec(cmd)
    }

    fn python_install(&self, version: &str) -> Result<()> {
        let mut cmd = self.uv(None);
        cmd.args(["python", "install", version]);
        Self::check(cmd)
    }

    fn python_list(&self) -> Result<()> {
        let mut cmd = self.uv(None);
        cmd.args(["python", "list"]);
        Self::check(cmd)
    }
}
