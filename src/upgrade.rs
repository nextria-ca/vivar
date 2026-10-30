//! `vivar upgrade`: replace the running binary with a release build.
//!
//! Downloads go through `gh` when it is logged in (the repo is private), else
//! `curl` with an optional `GITHUB_TOKEN`. `--from` installs from a release
//! archive, an extracted offline bundle, or a URL instead, for machines without
//! GitHub access.

use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "nextria-ca/vivar";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const EXE: &str = if cfg!(windows) { "vivar.exe" } else { "vivar" };

/// The release target triple for this machine.
pub fn target() -> Result<String> {
    let os = match env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        other => bail!("no release builds for {other}"),
    };
    match env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => Ok(format!("{arch}-{os}")),
        other => bail!("no release builds for {other}"),
    }
}

/// `vivar upgrade --check`: report the latest release without installing it.
pub fn check() -> Result<()> {
    let api = format!("{}/repos/{REPO}/releases/latest", api_base());
    let release = String::from_utf8_lossy(&fetch(&api, None)?).into_owned();
    let tag = json_str(&release, "tag_name").ok_or_else(|| anyhow!("no release found at {api}"))?;
    if tag.trim_start_matches('v') == VERSION {
        println!("vivar {VERSION} is the latest release");
    } else {
        println!("vivar {VERSION} installed; {tag} is available (run `vivar upgrade`)");
    }
    Ok(())
}

pub fn run(version: Option<&str>, from: Option<&str>) -> Result<()> {
    let current = env::current_exe().context("locating the running vivar")?;
    let tmp = env::temp_dir().join(format!("vivar-upgrade-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let result = upgrade(&current, version, from, &tmp);
    let _ = fs::remove_dir_all(&tmp);
    result
}

fn upgrade(current: &Path, version: Option<&str>, from: Option<&str>, tmp: &Path) -> Result<()> {
    let new = match from {
        Some(src) if src.starts_with("http://") || src.starts_with("https://") => {
            let archive = tmp.join(archive_name(src));
            fs::write(&archive, fetch(src, None)?)?;
            extract(&archive, tmp)?
        }
        Some(src) => {
            let src = Path::new(src);
            if src.is_dir() {
                find_binary(src)?
            } else {
                extract(src, tmp)?
            }
        }
        None => {
            let target = target()?;
            let asset = format!(
                "vivar-{target}.{}",
                if cfg!(windows) { "zip" } else { "tar.gz" }
            );
            let api = match version {
                Some(tag) => format!("{}/repos/{REPO}/releases/tags/{tag}", api_base()),
                None => format!("{}/repos/{REPO}/releases/latest", api_base()),
            };
            let release = String::from_utf8_lossy(&fetch(&api, None)?).into_owned();
            let tag = json_str(&release, "tag_name")
                .ok_or_else(|| anyhow!("no release found at {api}"))?;
            if version.is_none() && tag.trim_start_matches('v') == VERSION {
                println!("vivar {VERSION} is already the latest release");
                return Ok(());
            }
            let url = asset_url(&release, &asset)
                .ok_or_else(|| anyhow!("release {tag} has no {asset}"))?;
            println!("downloading vivar {tag} for {target}");
            let archive = tmp.join(&asset);
            fs::write(&archive, fetch(&url, Some("application/octet-stream"))?)?;
            // Every release ships SHA256SUMS; the archive must match it.
            if let Some(sums_url) = asset_url(&release, "SHA256SUMS") {
                let sums =
                    String::from_utf8_lossy(&fetch(&sums_url, Some("application/octet-stream"))?)
                        .into_owned();
                let expected = sums
                    .lines()
                    .filter_map(|l| l.split_once("  "))
                    .find(|(_, name)| *name == asset)
                    .map(|(digest, _)| digest.to_owned())
                    .ok_or_else(|| anyhow!("SHA256SUMS of {tag} does not list {asset}"))?;
                let actual = crate::sha256::file(&archive)?;
                if actual != expected {
                    bail!("{asset} failed its checksum (got {actual}, expected {expected}); not installing it");
                }
            } else {
                println!("warning: release {tag} has no SHA256SUMS; archive not verified");
            }
            extract(&archive, tmp)?
        }
    };

    let out = Command::new(&new)
        .arg("--version")
        .output()
        .with_context(|| format!("running {}", new.display()))?;
    if !out.status.success() {
        bail!("{} is not a working vivar binary", new.display());
    }
    let new_version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    replace(current, &new).with_context(|| {
        format!(
            "replacing {} (is it writable? re-run with the permissions that installed it)",
            current.display()
        )
    })?;
    println!("vivar {VERSION} -> {new_version} at {}", current.display());
    Ok(())
}

/// GETs a URL through `gh api` (logged in, api.github.com only) or `curl` (+ `GITHUB_TOKEN`).
pub fn fetch(url: &str, accept: Option<&str>) -> Result<Vec<u8>> {
    fetcher(url, accept, None, None)
}

/// GETs a URL straight to a file (large downloads).
pub fn fetch_to(url: &str, path: &Path) -> Result<()> {
    fetcher(url, None, Some(path), None).map(|_| ())
}

/// `fetch` for a download policy restricts: `allow` is asked about the URL
/// and about every redirect before it is followed, so nothing is fetched
/// from a host it refuses.
pub fn fetch_guarded(
    url: &str,
    accept: Option<&str>,
    allow: &dyn Fn(&str) -> bool,
) -> Result<Vec<u8>> {
    fetcher(url, accept, None, Some(allow))
}

/// `fetch_to` under the same guard.
pub fn fetch_to_guarded(url: &str, path: &Path, allow: &dyn Fn(&str) -> bool) -> Result<()> {
    fetcher(url, None, Some(path), Some(allow)).map(|_| ())
}

fn fetcher(
    url: &str,
    accept: Option<&str>,
    output: Option<&Path>,
    allow: Option<&dyn Fn(&str) -> bool>,
) -> Result<Vec<u8>> {
    if let Some(allow) = allow {
        return fetch_hops(url, accept, output, allow);
    }
    let gh_ok = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let mut cmd;
    if gh_ok && url.starts_with("https://api.github.com/") {
        cmd = Command::new("gh");
        cmd.arg("api");
        if let Some(accept) = accept {
            cmd.args(["-H", &format!("Accept: {accept}")]);
        }
        cmd.arg(url);
        if let Some(path) = output {
            cmd.arg("--output").arg(path);
        }
    } else {
        cmd = curl(url, accept, output, true);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running {:?} (install gh or curl)", cmd.get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        let hint = if err.contains("404") {
            " (no release yet, or no access to the private repo: log in with gh or set GITHUB_TOKEN)"
        } else {
            ""
        };
        bail!("fetching {url} failed: {err}{hint}");
    }
    Ok(out.stdout)
}

/// A curl command for `url`; `follow` lets curl take redirects itself.
fn curl(url: &str, accept: Option<&str>, output: Option<&Path>, follow: bool) -> Command {
    let mut cmd = Command::new("curl");
    if follow {
        cmd.args(["-LsSf", url]);
    } else {
        // `-q` first: no .curlrc may turn redirects (or anything else)
        // back on behind the policy check; `--globoff`: the URL checked is
        // the URL requested, with no `{..}` expansion on the way.
        // `--netrc-optional`: a mirror's ~/.netrc entry still applies (per
        // host, so a redirect elsewhere gets that host's own, or none).
        cmd.args(["-q", "-sS", "--globoff", "--netrc-optional", url]);
    }
    if let Some(ca) = ca_bundle() {
        cmd.arg("--cacert").arg(ca);
    }
    if let Some(path) = output {
        cmd.arg("-o").arg(path);
    }
    if let Some(accept) = accept {
        cmd.args(["-H", &format!("Accept: {accept}")]);
    }
    // The token is for the private repo only: never send it to package hosts.
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        if is_github(url) {
            cmd.args(["-H", &format!("Authorization: Bearer {token}")]);
        }
    }
    cmd
}

/// Downloads hop by hop: curl takes no redirect itself; each `Location`
/// is checked with `allow` first, and the body lands where it is wanted.
fn fetch_hops(
    url: &str,
    accept: Option<&str>,
    output: Option<&Path>,
    allow: &dyn Fn(&str) -> bool,
) -> Result<Vec<u8>> {
    let mut url = url.to_owned();
    // Credentials the request was given (userinfo in the URL) follow it to
    // that origin only, however many hops later; none are ever added that
    // it did not have.
    let origin = url.clone();
    let userinfo = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .and_then(|a| a.rsplit_once('@'))
        .map(|(u, _)| u.to_owned());
    for _ in 0..10 {
        if !allow(&url) {
            bail!("{url} is outside the hosts policy allows downloads from");
        }
        // The body goes to a file either way: the trailer (status and
        // redirect target) is all that comes back on stdout.
        let body_file = match output {
            Some(path) => path.to_path_buf(),
            None => private_temp_file()?,
        };
        let mut cmd = curl(&url, accept, Some(&body_file), false);
        cmd.args(["-w", "%{http_code}\n%{redirect_url}"]);
        let out = cmd.output().context("running curl (install curl)")?;
        let trailer = String::from_utf8_lossy(&out.stdout);
        let mut lines = trailer.lines();
        let code: u16 = lines.next().unwrap_or("").trim().parse().unwrap_or(0);
        let redirect = lines.next().unwrap_or("").trim().to_owned();
        if !out.status.success() {
            let _ = fs::remove_file(&body_file);
            bail!(
                "fetching {url} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        if (300..400).contains(&code) && !redirect.is_empty() {
            if output.is_none() {
                let _ = fs::remove_file(&body_file);
            }
            // Credentials stay with their origin: a same-origin redirect
            // keeps them (the Location carries none), any other loses them.
            url = match (&userinfo, crate::index::same_origin(&redirect, &origin)) {
                (Some(creds), true)
                    if !redirect.split_once("://").is_some_and(|(_, r)| {
                        r.split(['/', '?', '#'])
                            .next()
                            .is_some_and(|a| a.contains('@'))
                    }) =>
                {
                    match redirect.split_once("://") {
                        Some((scheme, rest)) => format!("{scheme}://{creds}@{rest}"),
                        None => redirect,
                    }
                }
                _ => redirect,
            };
            continue;
        }
        if code >= 400 {
            let _ = fs::remove_file(&body_file);
            bail!("fetching {url} failed: HTTP {code}");
        }
        return match output {
            Some(_) => Ok(Vec::new()),
            None => {
                let body = fs::read(&body_file);
                let _ = fs::remove_file(&body_file);
                Ok(body?)
            }
        };
    }
    bail!("fetching {url}: too many redirects")
}

/// An empty temporary file only this user can read (an authenticated
/// download lands in it), created before curl is handed the path.
fn private_temp_file() -> Result<PathBuf> {
    let path = env::temp_dir().join(format!("vivar-fetch-{}-{}", std::process::id(), seq()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    Ok(path)
}

fn seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Whether a URL points at GitHub itself (API, web, release assets).
fn is_github(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .map(|a| a.rsplit('@').next().unwrap_or(a))
        .unwrap_or("");
    host == "api.github.com"
        || host == "github.com"
        || host == "objects.githubusercontent.com"
        || host == "release-assets.githubusercontent.com"
        || host == "codeload.github.com"
}

/// GitHub's API base; `VIVAR_GITHUB_API` redirects it (tests, mirrors).
fn api_base() -> String {
    env::var("VIVAR_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".to_owned())
}

/// A private CA bundle for curl: `VIVAR_CA_BUNDLE`, else `SSL_CERT_FILE` (which
/// uv and Python honour too, so one variable covers every download).
pub fn ca_bundle() -> Option<std::ffi::OsString> {
    env::var_os("VIVAR_CA_BUNDLE").or_else(|| env::var_os("SSL_CERT_FILE"))
}

/// Value of the first `"key": "value"` in a JSON document (no escapes needed
/// for GitHub's tag names and URLs).
fn json_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let rest = &json[json.find(&needle)? + needle.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    Some(rest[..rest.find('"')?].to_owned())
}

/// API URL of the named asset: the `"url"` that precedes its `"name"`.
fn asset_url(release: &str, name: &str) -> Option<String> {
    let at = release
        .find(&format!("\"name\":\"{name}\""))
        .or_else(|| release.find(&format!("\"name\": \"{name}\"")))?;
    let before = &release[..at];
    let start = before.rfind("\"url\":")?;
    json_str(&release[start..], "url")
}

fn archive_name(url: &str) -> String {
    url.rsplit('/').next().unwrap_or("vivar-archive").to_owned()
}

/// Unpacks a release/offline archive and returns the vivar binary inside.
fn extract(archive: &Path, into: &Path) -> Result<PathBuf> {
    let dir = into.join("extracted");
    fs::create_dir_all(&dir)?;
    // bsdtar on Windows and macOS reads zips too; GNU tar only sees tarballs.
    let status = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(&dir)
        .status()
        .context("failed to run `tar`")?;
    if !status.success() {
        bail!("could not extract {}", archive.display());
    }
    find_binary(&dir)
}

/// `vivar` at the top of `dir` or one directory down.
fn find_binary(dir: &Path) -> Result<PathBuf> {
    let direct = dir.join(EXE);
    if direct.is_file() {
        return Ok(direct);
    }
    for entry in fs::read_dir(dir)?.flatten() {
        let candidate = entry.path().join(EXE);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("no {EXE} found in {}", dir.display())
}

/// Swaps `new` in for `current`, which is the running executable.
fn replace(current: &Path, new: &Path) -> Result<()> {
    // Copy next to the target first so the final step is a same-filesystem rename.
    let staged = current.with_extension("new");
    fs::copy(new, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(&staged, current)?;
    }
    #[cfg(windows)]
    {
        // A running exe can't be overwritten but can be renamed; the leftover
        // is removed on the next start (see `cleanup`).
        let old = current.with_extension("old");
        let _ = fs::remove_file(&old);
        fs::rename(current, &old)?;
        if let Err(e) = fs::rename(&staged, current) {
            let _ = fs::rename(&old, current);
            return Err(e.into());
        }
    }
    Ok(())
}

/// Removes the previous binary a Windows upgrade had to leave behind.
pub fn cleanup() {
    if cfg!(windows) {
        if let Ok(exe) = env::current_exe() {
            let _ = fs::remove_file(exe.with_extension("old"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = r#"{"url":"https://api.github.com/repos/o/r/releases/1","tag_name":"v0.2.0","assets":[{"url":"https://api.github.com/repos/o/r/releases/assets/11","name":"SHA256SUMS","browser_download_url":"https://github.com/o/r/releases/download/v0.2.0/SHA256SUMS"},{"url":"https://api.github.com/repos/o/r/releases/assets/12","name":"vivar-x86_64-unknown-linux-musl.tar.gz","browser_download_url":"https://github.com/o/r/releases/download/v0.2.0/vivar-x86_64-unknown-linux-musl.tar.gz"}]}"#;

    #[test]
    fn reads_tag_and_asset_url() {
        assert_eq!(json_str(RELEASE, "tag_name").unwrap(), "v0.2.0");
        assert_eq!(
            asset_url(RELEASE, "vivar-x86_64-unknown-linux-musl.tar.gz").unwrap(),
            "https://api.github.com/repos/o/r/releases/assets/12"
        );
        assert!(asset_url(RELEASE, "vivar-aarch64-apple-darwin.tar.gz").is_none());
    }

    #[test]
    fn pretty_printed_json_works_too() {
        let pretty = RELEASE.replace(":\"", ": \"").replace(",\"", ",\n  \"");
        assert_eq!(json_str(&pretty, "tag_name").unwrap(), "v0.2.0");
        assert_eq!(
            asset_url(&pretty, "SHA256SUMS").unwrap(),
            "https://api.github.com/repos/o/r/releases/assets/11"
        );
    }

    #[test]
    fn credentials_only_go_to_github() {
        assert!(is_github(
            "https://api.github.com/repos/x/y/releases/latest"
        ));
        assert!(is_github(
            "https://github.com/x/y/releases/download/v1/a.tar.gz"
        ));
        assert!(is_github(
            "https://objects.githubusercontent.com/github-production-release-asset/1"
        ));
        assert!(!is_github(
            "https://files.pythonhosted.org/packages/a/b/six.whl"
        ));
        assert!(!is_github(
            "https://download.pytorch.org/whl/cu124/torch.whl"
        ));
        assert!(!is_github("https://api.github.com.evil.example/x"));
        assert!(!is_github("https://evil.example/api.github.com/x"));
    }

    #[test]
    fn target_matches_release_names() {
        let t = target().unwrap();
        assert!(t.starts_with("x86_64-") || t.starts_with("aarch64-"), "{t}");
    }
}
