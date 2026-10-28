//! Managed CPython builds (python-build-standalone): discovery, download, listing.
//!
//! They live in uv's directory so both tools see the same interpreters and the
//! offline installer already drops runtimes there.

use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const RELEASES: &str = "https://github.com/astral-sh/python-build-standalone/releases";

pub struct Installed {
    pub key: String,
    pub version: [u32; 3],
    pub root: PathBuf,
}

impl Installed {
    pub fn executable(&self) -> PathBuf {
        if cfg!(windows) {
            self.root.join("python.exe")
        } else {
            let [major, minor, _] = self.version;
            self.root.join("bin").join(format!("python{major}.{minor}"))
        }
    }

    pub fn version_string(&self) -> String {
        let [a, b, c] = self.version;
        format!("{a}.{b}.{c}")
    }
}

/// `UV_PYTHON_INSTALL_DIR`, else uv's default location.
pub fn dir() -> Result<PathBuf> {
    if let Some(dir) = env::var_os("UV_PYTHON_INSTALL_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if cfg!(windows) {
        let appdata = env::var_os("APPDATA").ok_or_else(|| anyhow!("APPDATA is not set"))?;
        return Ok(PathBuf::from(appdata).join("uv").join("python"));
    }
    let data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(data.join("uv").join("python"))
}

/// `(os, arch, libc)` as they appear in install directory names.
fn host() -> Result<(&'static str, &'static str, &'static str)> {
    let os = match env::consts::OS {
        os @ ("linux" | "macos" | "windows") => os,
        other => bail!("no standalone Python builds for {other}"),
    };
    let arch = match env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => arch,
        other => bail!("no standalone Python builds for {other}"),
    };
    Ok((os, arch, if os == "linux" { "gnu" } else { "none" }))
}

/// The python-build-standalone target triple for this machine.
fn triple() -> Result<String> {
    let (os, arch, _) = host()?;
    Ok(match os {
        "linux" => format!("{arch}-unknown-linux-gnu"),
        "macos" => format!("{arch}-apple-darwin"),
        _ => format!("{arch}-pc-windows-msvc"),
    })
}

fn key(version: [u32; 3]) -> Result<String> {
    let (os, arch, libc) = host()?;
    let [a, b, c] = version;
    Ok(format!("cpython-{a}.{b}.{c}-{os}-{arch}-{libc}"))
}

/// Parses `3`, `3.12` or `3.12.4` (a `cpython@`/`python` prefix is tolerated).
pub fn parse_request(request: &str) -> Result<Vec<u32>> {
    let s = request
        .trim()
        .trim_start_matches("cpython@")
        .trim_start_matches("cpython-")
        .trim_start_matches("python");
    match s
        .split('.')
        .map(str::parse)
        .collect::<Result<Vec<u32>, _>>()
    {
        Ok(parts) if !parts.is_empty() && parts.len() <= 3 => Ok(parts),
        _ => bail!("invalid Python version {request:?}; use e.g. 3.12 or 3.12.4"),
    }
}

fn satisfies(version: [u32; 3], request: &[u32]) -> bool {
    request.iter().zip(version.iter()).all(|(a, b)| a == b)
}

fn parse_version(s: &str) -> Option<[u32; 3]> {
    let mut parts = s.split('.').map(|p| p.parse::<u32>().ok());
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(version)
}

/// Installed builds for this platform, newest first.
pub fn installed() -> Result<Vec<Installed>> {
    let (os, arch, libc) = host()?;
    let suffix = format!("-{os}-{arch}-{libc}");
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir()?) else {
        return Ok(found);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(middle) = name
            .strip_prefix("cpython-")
            .and_then(|n| n.strip_suffix(&suffix))
        else {
            continue;
        };
        // Skips uv's minor-version links (`cpython-3.12-…`) and variants (`+freethreaded`).
        let Some(version) = parse_version(middle) else {
            continue;
        };
        let root = entry.path();
        if root.is_dir() {
            found.push(Installed {
                key: name,
                version,
                root,
            });
        }
    }
    found.sort_by_key(|p| std::cmp::Reverse(p.version));
    Ok(found)
}

/// The newest installed build satisfying `request` (any build when `None`).
pub fn find(request: Option<&str>) -> Result<Option<Installed>> {
    let request = request.map(parse_request).transpose()?.unwrap_or_default();
    Ok(installed()?
        .into_iter()
        .find(|p| satisfies(p.version, &request)))
}

/// One clause of a PEP 440 specifier on release segments only.
fn clause_ok(version: [u32; 3], clause: &str) -> Result<bool> {
    let clause = clause.trim();
    let (op, rest) = ["===", "==", "!=", "<=", ">=", "~=", "<", ">"]
        .iter()
        .find_map(|op| clause.strip_prefix(op).map(|r| (*op, r.trim())))
        .ok_or_else(|| anyhow!("unsupported Python specifier {clause:?}"))?;
    let wildcard = rest.ends_with(".*");
    let parts: Vec<u32> = rest
        .trim_end_matches(".*")
        .split('.')
        .map(|p| {
            p.trim_start_matches(|c: char| !c.is_ascii_digit())
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
        })
        .collect::<Result<_, _>>()
        .map_err(|_| anyhow!("unsupported Python version in specifier {clause:?}"))?;
    let padded = [
        *parts.first().unwrap_or(&0),
        *parts.get(1).unwrap_or(&0),
        *parts.get(2).unwrap_or(&0),
    ];
    let prefix = |n: usize| version.iter().take(n).eq(padded.iter().take(n));
    Ok(match op {
        "==" | "===" if wildcard => prefix(parts.len()),
        "==" | "===" => version == padded,
        "!=" if wildcard => !prefix(parts.len()),
        "!=" => version != padded,
        ">=" => version >= padded,
        ">" => version > padded,
        "<=" => version <= padded,
        "<" => version < padded,
        // `~=3.12` means `>=3.12, ==3.*`; `~=3.12.1` means `>=3.12.1, ==3.12.*`.
        "~=" => version >= padded && prefix(parts.len().saturating_sub(1).max(1)),
        _ => unreachable!(),
    })
}

/// Whether `version` satisfies a full specifier like `>=3.10,<3.13`.
pub fn satisfies_spec(version: [u32; 3], spec: &str) -> Result<bool> {
    for clause in spec.split(',').filter(|c| !c.trim().is_empty()) {
        if !clause_ok(version, clause)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The newest installed build satisfying `spec` (a PEP 440 specifier), or the
/// newest downloadable one, installed. No spec means the newest installed.
pub fn select(spec: Option<&str>) -> Result<Installed> {
    let Some(spec) = spec.filter(|s| !s.trim().is_empty()) else {
        return crate::venv::interpreter(None);
    };
    for p in installed()? {
        if satisfies_spec(p.version, spec)? {
            return Ok(p);
        }
    }
    for (version, _) in available()? {
        if satisfies_spec(version, spec)? {
            let [a, b, c] = version;
            return install(&format!("{a}.{b}.{c}"));
        }
    }
    bail!("no CPython build satisfies requires-python = {spec:?}")
}

/// Downloads for this machine in the latest python-build-standalone release:
/// `(version, file name)`, newest first. Pre-releases are left out.
fn available() -> Result<Vec<([u32; 3], String)>> {
    let sums = crate::upgrade::fetch(&format!("{RELEASES}/latest/download/SHA256SUMS"), None)
        .context("fetching the list of Python downloads")?;
    let suffix = format!("-{}-install_only_stripped.tar.gz", triple()?);
    let mut out = Vec::new();
    for line in String::from_utf8_lossy(&sums).lines() {
        let Some(file) = line.split_whitespace().nth(1) else {
            continue;
        };
        let version = file
            .strip_prefix("cpython-")
            .and_then(|f| f.strip_suffix(&suffix))
            .and_then(|f| f.split_once('+'))
            .and_then(|(v, _)| parse_version(v));
        if let Some(version) = version {
            out.push((version, file.to_owned()));
        }
    }
    out.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
    Ok(out)
}

/// Installs the newest build satisfying `request`, unless one already is.
pub fn install(request: &str) -> Result<Installed> {
    let wanted = parse_request(request)?;
    if let Some(have) = installed()?
        .into_iter()
        .find(|p| satisfies(p.version, &wanted))
    {
        println!("{} is already installed", have.key);
        return Ok(have);
    }
    let triple = triple()?;
    let (version, file) = available()?
        .into_iter()
        .find(|(v, _)| satisfies(*v, &wanted))
        .ok_or_else(|| anyhow!("no standalone CPython {request} build for {triple}"))?;
    // The release tag is the date after `+` in the file name.
    let tag = file
        .split('+')
        .nth(1)
        .and_then(|rest| rest.split('-').next())
        .ok_or_else(|| anyhow!("unexpected download name {file}"))?;
    let url = format!("{RELEASES}/download/{tag}/{file}");

    let dir = dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".vivar-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let result = (|| {
        println!("downloading {file}");
        let archive = tmp.join("python.tar.gz");
        crate::upgrade::fetch_to(&url, &archive)?;
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&tmp)
            .status()
            .context("failed to run `tar`")?;
        if !status.success() {
            bail!("could not extract {file}");
        }
        let key = key(version)?;
        let root = dir.join(&key);
        fs::rename(tmp.join("python"), &root)?;
        println!("installed {key} to {}", root.display());
        Ok(Installed { key, version, root })
    })();
    let _ = fs::remove_dir_all(&tmp);
    result
}

pub fn list(all: bool) -> Result<()> {
    let installed = installed()?;
    for p in &installed {
        println!("{:<44} {}", p.key, p.executable().display());
    }
    if all {
        for (version, _) in available()? {
            if !installed.iter().any(|p| p.version == version) {
                println!("{:<44} <download available>", key(version)?);
            }
        }
    } else if installed.is_empty() {
        println!("no Python installed yet; `vivar python install 3.12` fetches one, `--all` lists downloads");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests_and_versions() {
        assert_eq!(parse_request("3.12").unwrap(), vec![3, 12]);
        assert_eq!(parse_request("cpython@3.12.4").unwrap(), vec![3, 12, 4]);
        assert_eq!(parse_request("python3").unwrap(), vec![3]);
        assert!(parse_request("latest").is_err());
        assert!(parse_request("3.12.4.1").is_err());
        assert_eq!(parse_version("3.12.4"), Some([3, 12, 4]));
        assert_eq!(parse_version("3.12"), None);
        assert_eq!(parse_version("3.13.0a1"), None);
        assert!(satisfies([3, 12, 4], &[3, 12]));
        assert!(satisfies([3, 12, 4], &[]));
        assert!(!satisfies([3, 13, 0], &[3, 12]));
    }

    #[test]
    fn evaluates_specifiers() {
        let v = [3, 12, 4];
        assert!(satisfies_spec(v, ">=3.12").unwrap());
        assert!(satisfies_spec(v, ">=3.12,<3.13").unwrap());
        assert!(!satisfies_spec(v, ">=3.13").unwrap());
        assert!(satisfies_spec(v, "==3.12.*").unwrap());
        assert!(!satisfies_spec(v, "==3.11.*").unwrap());
        assert!(satisfies_spec(v, "==3.12.4").unwrap());
        assert!(!satisfies_spec(v, "==3.12.3").unwrap());
        assert!(satisfies_spec(v, "~=3.12").unwrap());
        assert!(satisfies_spec(v, "~=3.12.1").unwrap());
        assert!(!satisfies_spec(v, "~=3.11.9").unwrap());
        assert!(satisfies_spec(v, "!=3.12.3, <4").unwrap());
        assert!(!satisfies_spec(v, "!=3.12.*").unwrap());
        assert!(satisfies_spec(v, " >3.11 ").unwrap());
        assert!(satisfies_spec(v, "").unwrap());
        assert!(satisfies_spec([3, 12, 0], ">=3.12").unwrap());
        assert!(satisfies_spec(v, ">=3.12.0rc1").unwrap());
        assert!(satisfies_spec(v, "<3.12.5").unwrap());
        assert!(satisfies_spec(v, "<3.13").unwrap());
        assert!(!satisfies_spec(v, "<=3.12").unwrap());
        assert!(satisfies_spec([3, 12, 0], "<=3.12").unwrap());
        assert!(satisfies_spec(v, "~=3.12.1, !=3.12.3").unwrap());
        assert!(satisfies_spec(v, ">=3.12.4").unwrap());
        assert!(satisfies_spec(v, "<3.12.5,>=3.12.4").unwrap());
        assert!(!satisfies_spec(v, "==3").unwrap());
        assert!(!satisfies_spec(v, "==3.12").unwrap());
        assert!(satisfies_spec([3, 12, 0], "==3.12").unwrap());
        assert!(satisfies_spec(v, "latest").is_err());
    }

    #[test]
    fn install_dir_honours_override() {
        let _guard = std::env::var_os("UV_PYTHON_INSTALL_DIR");
        std::env::set_var("UV_PYTHON_INSTALL_DIR", "/tmp/pys");
        assert_eq!(dir().unwrap(), PathBuf::from("/tmp/pys"));
        match _guard {
            Some(v) => std::env::set_var("UV_PYTHON_INSTALL_DIR", v),
            None => std::env::remove_var("UV_PYTHON_INSTALL_DIR"),
        }
    }
}
