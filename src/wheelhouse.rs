//! `vivar export wheelhouse`: the lockfile's exact wheels for a target machine,
//! downloaded and checksummed into a directory that `index-url` can point at.

use crate::lock::{Artifact, Lock, Package};
use crate::{sha256, upgrade};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::Path;

/// A machine to export for: `linux-x86_64`, `windows-aarch64`, `macos-arm64` …
#[derive(Clone)]
pub struct Target {
    os: String,
    arch: String,
    /// `cp312`
    py: String,
    /// 12, for abi3 comparisons
    minor: u32,
}

impl Target {
    pub fn os(&self) -> &str {
        &self.os
    }

    pub fn arch(&self) -> &str {
        &self.arch
    }

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
    /// `linux-x86_64-cp312`: what a built wheel is good for.
    pub fn label(&self) -> String {
        format!("{}-{}-{}", self.os, self.arch, self.py)
    }

    pub fn score(&self, filename: &str) -> Option<u32> {
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
        // The ABI must be ours, stable, or none: no free-threaded (`cp313t`) or
        // debug builds for a regular interpreter.
        // (`cp37m` was the regular ABI before 3.8.)
        let regular = format!("{}m", self.py);
        if !abi
            .split('.')
            .any(|a| a == self.py || a == regular || a == "abi3" || a == "none")
        {
            return None;
        }
        let platforms: Vec<&str> = plat.split('.').filter(|p| self.platform_ok(p)).collect();
        let any = plat == "any";
        if platforms.is_empty() && !any {
            return None;
        }
        // A native wheel for the exact interpreter beats pure-Python beats abi3,
        // and among native wheels the one with the oldest OS/libc floor wins:
        // it runs on the most machines.
        let mut score = if any { 500 } else { 1000 };
        if py.contains(&self.py) {
            score += 100;
        }
        let floor = platforms
            .iter()
            .map(|p| platform_floor(p))
            .min()
            .unwrap_or(0);
        Some(score - floor)
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
    pub fn pick<'a>(&self, pkg: &'a Package) -> Option<(&'a Artifact, bool)> {
        pkg.wheels
            .iter()
            .filter_map(|w| self.score(&w.filename()).map(|s| (s, w)))
            .max_by_key(|(s, _)| *s)
            .map(|(_, w)| (w, true))
            .or(pkg.sdist.as_ref().map(|s| (s, false)))
    }
}

/// How demanding a platform tag is: the glibc minor for manylinux, the macOS
/// major for macosx, 0 otherwise. Lower runs on more machines.
fn platform_floor(tag: &str) -> u32 {
    let nums: Vec<u32> = tag
        .split('_')
        .skip(1)
        .take(2)
        .filter_map(|p| p.parse().ok())
        .collect();
    if tag.starts_with("manylinux_") {
        nums.get(1).copied().unwrap_or(0)
    } else if tag.starts_with("manylinux2014") {
        17
    } else if tag.starts_with("manylinux2010") {
        12
    } else if tag.starts_with("manylinux1") {
        5
    } else if tag.starts_with("macosx_") {
        nums.first().copied().unwrap_or(0)
    } else {
        0
    }
}

/// Downloads every locked registry package into `out`. Returns a summary line.
pub fn export(root: &Path, target: &Target, out: &Path) -> Result<String> {
    let lock = Lock::read(root)?;
    fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let (mut wheels, mut sdists, mut skipped) = (0, Vec::new(), Vec::new());
    // Decide everything first, then fetch in parallel: these are many small
    // files from a CDN, and serial downloads are what makes large exports slow.
    let mut jobs: Vec<(String, std::path::PathBuf, Option<String>)> = Vec::new();
    for pkg in lock.packages.iter().filter(|p| p.source_kind == "registry") {
        let Some((artifact, is_wheel)) = target.pick(pkg) else {
            skipped.push(format!("{} {}", pkg.name, pkg.version));
            continue;
        };
        let name = artifact.filename();
        // A lock entry is data from elsewhere: its file name must stay inside `out`.
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            bail!(
                "refusing to write artifact with unsafe file name {name:?} from {}",
                artifact.url
            );
        }
        if is_wheel {
            wheels += 1;
        } else {
            sdists.push(pkg.name.clone());
        }
        let dest = out.join(&name);
        let expected = artifact.sha256().map(str::to_owned);
        let cached = expected
            .as_deref()
            .is_some_and(|h| sha256::file(&dest).ok().as_deref() == Some(h));
        if !cached {
            jobs.push((artifact.url.clone(), dest, expected));
        }
    }
    download_all(&jobs)?;
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

/// Fetches `(url, destination, expected sha256)` jobs eight at a time, each
/// verified against its hash before it counts as done.
fn download_all(jobs: &[(String, std::path::PathBuf, Option<String>)]) -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    if jobs.is_empty() {
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let workers = jobs.len().min(8);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some((url, dest, expected)) = jobs.get(i) else { break };
                let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                println!("downloading {name}");
                let result = upgrade::fetch_to(url, dest).and_then(|()| {
                    if let Some(h) = expected {
                        let actual = sha256::file(dest)?;
                        if &actual != h {
                            let _ = fs::remove_file(dest);
                            bail!("{name} does not match the hash in uv.lock (got {actual}, expected {h})");
                        }
                    }
                    Ok(())
                });
                if let Err(e) = result {
                    errors.lock().unwrap().push(e.to_string());
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!(
            "{} download(s) failed:\n  {}",
            errors.len(),
            errors.join("\n  ")
        )
    }
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
        // Older floors win ties; free-threaded builds never match a regular interpreter.
        assert!(
            linux.score("numpy-2.5.4-cp312-cp312-manylinux_2_17_x86_64.whl")
                > linux.score("numpy-2.5.4-cp312-cp312-manylinux_2_28_x86_64.whl")
        );
        let py37 = Target::new(Some("linux-x86_64"), "3.7").unwrap();
        assert!(py37
            .score("old-1.0-cp37-cp37m-manylinux1_x86_64.whl")
            .is_some());
        assert!(py37
            .score("old-1.0-cp37-cp37dm-manylinux1_x86_64.whl")
            .is_none());
        let py313 = Target::new(Some("linux-x86_64"), "3.13").unwrap();
        assert!(py313
            .score("numpy-2.5.4-cp313-cp313-manylinux_2_28_x86_64.whl")
            .is_some());
        assert!(py313
            .score("numpy-2.5.4-cp313-cp313t-manylinux_2_28_x86_64.whl")
            .is_none());

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
        assert!(
            mac.score("numpy-2.5.4-cp313-cp313-macosx_11_0_arm64.whl")
                > mac.score("numpy-2.5.4-cp313-cp313-macosx_14_0_arm64.whl")
        );
        assert_eq!(platform_floor("manylinux2014_x86_64"), 17);
        assert_eq!(platform_floor("macosx_14_0_arm64"), 14);
        assert_eq!(platform_floor("win_amd64"), 0);
        assert!(Target::new(Some("plan9-mips"), "3.12").is_err());
        assert!(Target::new(None, "three").is_err());
    }
}
