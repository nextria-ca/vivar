//! `vivar export wheelhouse`: the lockfile's exact wheels for a target machine,
//! downloaded and checksummed into a directory that `index-url` can point at.

use crate::lock::{Artifact, Lock, Package};
use crate::{sha256, upgrade};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;

/// A machine to export for: `linux-x86_64`, `windows-aarch64`, `macos-arm64` …
pub struct Target {
    os: String,
    arch: String,
    /// `cp312`
    py: String,
    /// 12, for abi3 comparisons
    minor: u32,
}

impl Target {
    /// `name` as `<os>-<arch>` (defaults to this machine), `python` as `3.12`.
    pub fn new(name: Option<&str>, python: &str) -> Result<Target> {
        let (os, arch) = match name {
            Some(n) => n
                .split_once('-')
                .map(|(o, a)| (o.to_owned(), a.to_owned()))
                .ok_or_else(|| anyhow!("target must look like linux-x86_64, got {n:?}"))?,
            None => (
                std::env::consts::OS.to_owned(),
                std::env::consts::ARCH.to_owned(),
            ),
        };
        let os = match os.as_str() {
            "linux" | "windows" => os,
            "macos" | "darwin" | "osx" => "macos".to_owned(),
            other => bail!("unsupported target OS {other:?} (linux, windows, macos)"),
        };
        let arch = match arch.as_str() {
            "x86_64" | "amd64" | "x64" => "x86_64".to_owned(),
            "aarch64" | "arm64" => "aarch64".to_owned(),
            other => bail!("unsupported target architecture {other:?} (x86_64, aarch64)"),
        };
        let mut parts = python.split('.');
        let (major, minor) = (
            parts.next().and_then(|p| p.parse::<u32>().ok()),
            parts.next().and_then(|p| p.parse::<u32>().ok()),
        );
        let (Some(major), Some(minor)) = (major, minor) else {
            bail!("python must look like 3.12, got {python:?}");
        };
        Ok(Target {
            os,
            arch,
            py: format!("cp{major}{minor}"),
            minor,
        })
    }

    /// Whether a wheel (by file name) runs on this target. Higher is a better fit.
    fn score(&self, filename: &str) -> Option<u32> {
        let stem = filename.strip_suffix(".whl")?;
        let mut parts = stem.rsplitn(3, '-');
        let plat = parts.next()?;
        let abi = parts.next()?;
        let py = parts.next()?.rsplit('-').next()?;

        let py_ok = py.split('.').any(|tag| {
            tag == self.py
                || tag == "py3"
                || tag == "py2.py3"
                || (abi.split('.').any(|a| a == "abi3")
                    && tag
                        .strip_prefix("cp3")
                        .and_then(|m| m.parse::<u32>().ok())
                        .is_some_and(|m| m <= self.minor))
        });
        if !py_ok {
            return None;
        }
        let specific = plat.split('.').any(|p| self.platform_ok(p));
        let any = plat == "any";
        if !specific && !any {
            return None;
        }
        // A native wheel for the exact interpreter beats pure-Python beats abi3.
        let mut score = if specific { 100 } else { 50 };
        if py.contains(&self.py) {
            score += 10;
        }
        Some(score)
    }

    fn platform_ok(&self, tag: &str) -> bool {
        match self.os.as_str() {
            "linux" => {
                (tag.starts_with("manylinux") || tag.starts_with("linux_"))
                    && tag.ends_with(&format!("_{}", self.arch))
            }
            "windows" => match self.arch.as_str() {
                "x86_64" => tag == "win_amd64",
                _ => tag == "win_arm64",
            },
            _ => {
                tag.starts_with("macosx_")
                    && (tag.ends_with("_universal2")
                        || tag.ends_with(if self.arch == "aarch64" {
                            "_arm64"
                        } else {
                            "_x86_64"
                        }))
            }
        }
    }

    /// The best wheel for this target, else the sdist.
    fn pick<'a>(&self, pkg: &'a Package) -> Option<(&'a Artifact, bool)> {
        pkg.wheels
            .iter()
            .filter_map(|w| self.score(w.filename()).map(|s| (s, w)))
            .max_by_key(|(s, _)| *s)
            .map(|(_, w)| (w, true))
            .or(pkg.sdist.as_ref().map(|s| (s, false)))
    }
}

/// Downloads every locked registry package into `out`. Returns a summary line.
pub fn export(root: &Path, target: &Target, out: &Path) -> Result<String> {
    let lock = Lock::read(root)?;
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let (mut wheels, mut sdists, mut skipped) = (0, Vec::new(), Vec::new());
    for pkg in lock.packages.iter().filter(|p| p.source_kind == "registry") {
        let Some((artifact, is_wheel)) = target.pick(pkg) else {
            skipped.push(format!("{} {}", pkg.name, pkg.version));
            continue;
        };
        let dest = out.join(artifact.filename());
        let expected = artifact.sha256();
        let cached = expected.is_some_and(|h| sha256::file(&dest).ok().as_deref() == Some(h));
        if !cached {
            println!("downloading {}", artifact.filename());
            upgrade::fetch_to(&artifact.url, &dest)?;
            if let Some(h) = expected {
                let actual = sha256::file(&dest)?;
                if actual != h {
                    let _ = fs::remove_file(&dest);
                    bail!(
                        "{} does not match the hash in uv.lock (got {actual}, expected {h})",
                        artifact.filename()
                    );
                }
            }
        }
        if is_wheel {
            wheels += 1;
        } else {
            sdists.push(pkg.name.clone());
        }
    }
    let mut summary = format!("{wheels} wheel(s) in {}", out.display());
    if !sdists.is_empty() {
        summary.push_str(&format!(
            "; {} source-only (will need a compiler on the target): {}",
            sdists.len(),
            sdists.join(", ")
        ));
    }
    if !skipped.is_empty() {
        summary.push_str(&format!("; skipped (no artifacts): {}", skipped.join(", ")));
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_wheels_for_targets() {
        let linux = Target::new(Some("linux-x86_64"), "3.12").unwrap();
        assert!(linux
            .score("numpy-2.5.4-cp312-cp312-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl")
            .is_some());
        assert!(linux
            .score("numpy-2.5.4-cp313-cp313-manylinux_2_28_x86_64.whl")
            .is_none());
        assert!(linux
            .score("numpy-2.5.4-cp312-cp312-win_amd64.whl")
            .is_none());
        assert!(linux
            .score("numpy-2.5.4-cp312-cp312-musllinux_1_2_x86_64.whl")
            .is_none());
        assert!(linux.score("six-1.17.0-py2.py3-none-any.whl").is_some());
        assert!(linux
            .score("cryptography-43.0.0-cp39-abi3-manylinux_2_28_x86_64.whl")
            .is_some());
        assert!(linux
            .score("old-1.0-cp313-abi3-manylinux_2_28_x86_64.whl")
            .is_none());
        assert!(linux
            .score("numpy-2.5.4-cp312-cp312-manylinux_2_28_aarch64.whl")
            .is_none());
        assert!(
            linux.score("numpy-2.5.4-cp312-cp312-manylinux_2_28_x86_64.whl")
                > linux.score("six-1.17.0-py3-none-any.whl")
        );

        let win = Target::new(Some("windows-amd64"), "3.12").unwrap();
        assert!(win.score("numpy-2.5.4-cp312-cp312-win_amd64.whl").is_some());
        assert!(win.score("numpy-2.5.4-cp312-cp312-win_arm64.whl").is_none());
        let mac = Target::new(Some("macos-arm64"), "3.13").unwrap();
        assert!(mac
            .score("numpy-2.5.4-cp313-cp313-macosx_14_0_arm64.whl")
            .is_some());
        assert!(mac
            .score("numpy-2.5.4-cp313-cp313-macosx_10_13_x86_64.whl")
            .is_none());
        assert!(mac
            .score("x-1.0-cp313-cp313-macosx_11_0_universal2.whl")
            .is_some());
        assert!(Target::new(Some("plan9-mips"), "3.12").is_err());
        assert!(Target::new(None, "three").is_err());
    }
}
