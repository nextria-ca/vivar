//! `vivar publish`: uploads built distributions to a package index through
//! the legacy upload API every index speaks (PyPI, devpi, Artifactory, Nexus,
//! GitLab). The metadata the index wants comes out of the file itself; the
//! request goes through curl, with credentials handed over on stdin so they
//! never sit on a command line.

use crate::{sha256, upgrade, zip};
use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_URL: &str = "https://upload.pypi.org/legacy/";

pub struct Options {
    pub repository: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Treat "already exists" as success, for re-run pipelines.
    pub skip_existing: bool,
    pub dry_run: bool,
}

/// Credentials from the flags or the environment: `VIVAR_PUBLISH_TOKEN`
/// (user `__token__`), or `VIVAR_PUBLISH_USERNAME` + `VIVAR_PUBLISH_PASSWORD`.
/// With neither, curl's `~/.netrc` entry for the host is used.
pub fn credentials(username: Option<String>, password: Option<String>) -> Option<(String, String)> {
    match (username, password) {
        (Some(u), Some(p)) => return Some((u, p)),
        (None, Some(p)) => return Some(("__token__".to_owned(), p)),
        _ => {}
    }
    if let Ok(token) = std::env::var("VIVAR_PUBLISH_TOKEN") {
        return Some(("__token__".to_owned(), token));
    }
    match (
        std::env::var("VIVAR_PUBLISH_USERNAME"),
        std::env::var("VIVAR_PUBLISH_PASSWORD"),
    ) {
        (Ok(u), Ok(p)) => Some((u, p)),
        _ => None,
    }
}

/// The distributions in `dist/`: wheels and source archives, newest last.
pub fn dist_files(root: &Path) -> Result<Vec<PathBuf>> {
    let dist = root.join("dist");
    let mut files: Vec<PathBuf> = fs::read_dir(&dist)
        .with_context(|| format!("no {} (run `vivar build` first)", dist.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_distribution(p))
        .collect();
    files.sort();
    if files.is_empty() {
        bail!(
            "nothing to publish in {} (run `vivar build` first)",
            dist.display()
        );
    }
    Ok(files)
}

fn is_distribution(p: &Path) -> bool {
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    p.is_file() && (name.ends_with(".whl") || name.ends_with(".tar.gz") || name.ends_with(".zip"))
}

/// What the upload form says about one file.
#[derive(Debug, PartialEq, Eq)]
pub struct Upload {
    pub path: PathBuf,
    pub filetype: &'static str,
    pub pyversion: String,
    /// Metadata headers, in order, multi-valued ones repeated.
    pub headers: Vec<(String, String)>,
    /// The long description: everything after the headers.
    pub description: String,
    pub sha256: String,
}

impl Upload {
    fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    pub fn name(&self) -> &str {
        self.header("Name").unwrap_or("")
    }

    pub fn version(&self) -> &str {
        self.header("Version").unwrap_or("")
    }
}

/// Reads a wheel's METADATA or a source archive's PKG-INFO.
pub fn inspect(path: &Path) -> Result<Upload> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    let (filetype, pyversion, text) = if let Some(stem) = name.strip_suffix(".whl") {
        let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let metadata = crate::index::metadata_from_wheel(&data)
            .with_context(|| format!("{name} is not a wheel"))?;
        // `name-ver(-build)-py-abi-plat`: the python tag is the third from the end.
        let pyversion = stem
            .rsplitn(3, '-')
            .nth(2)
            .and_then(|s| s.rsplit('-').next())
            .unwrap_or("py3")
            .to_owned();
        ("bdist_wheel", pyversion, metadata)
    } else if name.ends_with(".tar.gz") || name.ends_with(".zip") {
        ("sdist", "source".to_owned(), pkg_info(path)?)
    } else {
        bail!("{name}: not a wheel or source distribution");
    };
    let (headers, description) = split_metadata(&text);
    let upload = Upload {
        path: path.to_path_buf(),
        filetype,
        pyversion,
        headers,
        description,
        sha256: sha256::file(path)?,
    };
    if upload.name().is_empty() || upload.version().is_empty() {
        bail!("{name}: metadata has no Name or Version");
    }
    Ok(upload)
}

/// The PKG-INFO at the top of a source distribution.
fn pkg_info(archive: &Path) -> Result<String> {
    let name = archive.file_name().unwrap_or_default().to_string_lossy();
    if name.ends_with(".zip") {
        let data = fs::read(archive)?;
        let entries = zip::entries(&data)?;
        let entry = entries
            .iter()
            .find(|e| e.name.ends_with("/PKG-INFO") && e.name.matches('/').count() == 1)
            .ok_or_else(|| anyhow!("{name} has no PKG-INFO"))?;
        return Ok(String::from_utf8_lossy(&zip::read(&data, entry)?).into_owned());
    }
    let out = Command::new("tar")
        .arg("-xzOf")
        .arg(archive)
        .arg("--wildcards")
        .arg("*/PKG-INFO")
        .output()
        .context("running tar")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && !text.trim().is_empty() {
        // `*/PKG-INFO` can match nested copies (egg-info); the first one is the sdist's own.
        return Ok(first_pkg_info(&text));
    }
    // bsdtar (macOS, Windows) has no --wildcards: unpack and look.
    let scratch = std::env::temp_dir().join(format!("vivar-publish-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch)?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(&scratch)
        .status()
        .context("running tar")?;
    let found = (|| -> Result<String> {
        if !status.success() {
            bail!("tar could not read {name}");
        }
        for entry in fs::read_dir(&scratch)?.flatten() {
            let info = entry.path().join("PKG-INFO");
            if info.is_file() {
                return Ok(fs::read_to_string(info)?);
            }
        }
        bail!("{name} has no PKG-INFO")
    })();
    let _ = fs::remove_dir_all(&scratch);
    found
}

/// `tar -O` concatenates every match; a metadata file starts with its version line.
fn first_pkg_info(text: &str) -> String {
    let mut starts = text.match_indices("Metadata-Version:").map(|(i, _)| i);
    let first = starts.next().unwrap_or(0);
    let end = starts.next().unwrap_or(text.len());
    text[first..end].to_owned()
}

/// Headers (continuation lines folded) and the body after the blank line.
pub fn split_metadata(text: &str) -> (Vec<(String, String)>, String) {
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut lines = text.lines();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some((_, v)) = headers.last_mut() {
                v.push('\n');
                v.push_str(line.trim_start_matches([' ', '\t']));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let body: Vec<&str> = lines.collect();
    // A `Description:` header is the old way of carrying the body.
    let description = match headers.iter().position(|(k, _)| k == "Description") {
        Some(i) if body.join("\n").trim().is_empty() => headers.remove(i).1,
        _ => body.join("\n"),
    };
    (headers, description)
}

/// The upload form's fields for a file, as `(field, value)`; the file itself
/// goes in `content`.
pub fn form_fields(u: &Upload) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = vec![
        (":action".into(), "file_upload".into()),
        ("protocol_version".into(), "1".into()),
        ("filetype".into(), u.filetype.into()),
        ("pyversion".into(), u.pyversion.clone()),
        ("sha256_digest".into(), u.sha256.clone()),
    ];
    for (k, v) in &u.headers {
        // The form names a header with underscores and in lowercase; the
        // few that differ are spelled out.
        let field = match k.to_lowercase().as_str() {
            "metadata-version" => "metadata_version",
            "name" => "name",
            "version" => "version",
            "summary" => "summary",
            "description-content-type" => "description_content_type",
            "keywords" => "keywords",
            "home-page" => "home_page",
            "download-url" => "download_url",
            "author" => "author",
            "author-email" => "author_email",
            "maintainer" => "maintainer",
            "maintainer-email" => "maintainer_email",
            "license" => "license",
            "license-expression" => "license_expression",
            "license-file" => "license_files",
            "classifier" => "classifiers",
            "platform" => "platform",
            "supported-platform" => "supported_platform",
            "requires-dist" => "requires_dist",
            "requires-python" => "requires_python",
            "requires-external" => "requires_external",
            "provides-extra" => "provides_extras",
            "provides-dist" => "provides_dist",
            "obsoletes-dist" => "obsoletes_dist",
            "project-url" => "project_urls",
            "dynamic" => "dynamic",
            _ => continue,
        };
        fields.push((field.to_owned(), v.clone()));
    }
    if !u.description.trim().is_empty() {
        fields.push(("description".into(), u.description.clone()));
    }
    fields
}

/// Uploads every file. Returns how many were sent and how many skipped.
pub fn run(files: &[PathBuf], opts: &Options) -> Result<(usize, usize)> {
    let uploads: Vec<Upload> = files.iter().map(|f| inspect(f)).collect::<Result<_>>()?;
    let creds = credentials(opts.username.clone(), opts.password.clone());
    let mut sent = 0;
    let mut skipped = 0;
    for u in &uploads {
        let file = u.path.file_name().unwrap_or_default().to_string_lossy();
        if opts.dry_run {
            println!(
                "would upload {file} ({} {} {}) to {}",
                u.name(),
                u.version(),
                u.filetype,
                opts.repository
            );
            continue;
        }
        match upload(u, opts, creds.as_ref())? {
            Outcome::Sent => {
                println!("uploaded {file}");
                sent += 1;
            }
            Outcome::Exists => {
                println!("skipped {file}: already on the index");
                skipped += 1;
            }
        }
    }
    Ok((sent, skipped))
}

enum Outcome {
    Sent,
    Exists,
}

fn upload(u: &Upload, opts: &Options, creds: Option<&(String, String)>) -> Result<Outcome> {
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-L", "-o", "-", "-w", "\n%{http_code}"]);
    if let Some(ca) = upgrade::ca_bundle() {
        cmd.arg("--cacert").arg(ca);
    }
    // Credentials come from a config read on stdin, not from argv.
    let mut config = String::new();
    match creds {
        Some((user, pass)) => config.push_str(&format!(
            "user = \"{}:{}\"\n",
            curl_escape(user),
            curl_escape(pass)
        )),
        None => config.push_str("netrc-optional\n"),
    }
    cmd.args(["-K", "-"]).stdin(Stdio::piped());
    let scratch = std::env::temp_dir().join(format!(
        "vivar-publish-{}-{}.txt",
        std::process::id(),
        u.sha256.get(..12).unwrap_or("x")
    ));
    for (field, value) in form_fields(u) {
        if field == "description" {
            // The body can be long and holds anything; curl reads it from a file.
            fs::write(&scratch, &value)?;
            cmd.arg("-F")
                .arg(format!("description=<{}", scratch.display()));
        } else {
            cmd.arg("--form-string").arg(format!("{field}={value}"));
        }
    }
    cmd.arg("-F").arg(format!(
        "content=@{};filename={};type=application/octet-stream",
        u.path.display(),
        u.path.file_name().unwrap_or_default().to_string_lossy()
    ));
    cmd.arg(&opts.repository);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running curl (is it installed?)")?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(config.as_bytes())?;
    let out = child.wait_with_output()?;
    let _ = fs::remove_file(&scratch);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let (body, code) = stdout
        .trim_end()
        .rsplit_once('\n')
        .unwrap_or(("", stdout.trim()));
    let code: u16 = code.trim().parse().unwrap_or(0);
    let file = u.path.file_name().unwrap_or_default().to_string_lossy();
    if !out.status.success() && code == 0 {
        bail!(
            "uploading {file} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let body = body.trim();
    let exists = body.to_lowercase().contains("already exist") || code == 409;
    match code {
        200 | 201 => Ok(Outcome::Sent),
        _ if exists && opts.skip_existing => Ok(Outcome::Exists),
        401 | 403 => bail!(
            "uploading {file}: the index refused the credentials (HTTP {code}); set VIVAR_PUBLISH_TOKEN or --username/--password{}",
            trailer(body)
        ),
        _ => bail!("uploading {file} failed with HTTP {code}{}", trailer(body)),
    }
}

fn trailer(body: &str) -> String {
    let line = body.lines().last().unwrap_or("").trim();
    if line.is_empty() || line.starts_with('<') {
        String::new()
    } else {
        format!(": {line}")
    }
}

/// Escapes a value for a quoted string in a curl config file.
fn curl_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_headers_from_the_body() {
        let text = "Metadata-Version: 2.4\nName: minipkg\nVersion: 1.0\nClassifier: A\nClassifier: B\nSummary: one\n  line folded\n\n# Readme\n\nbody\n";
        let (headers, body) = split_metadata(text);
        assert_eq!(headers[0], ("Metadata-Version".into(), "2.4".into()));
        assert_eq!(headers[3], ("Classifier".into(), "A".into()));
        assert_eq!(headers[5], ("Summary".into(), "one\nline folded".into()));
        assert_eq!(body, "# Readme\n\nbody");
        let (headers, body) =
            split_metadata("Metadata-Version: 1.0\nName: x\nDescription: legacy body\n");
        assert_eq!(body, "legacy body");
        assert!(!headers.iter().any(|(k, _)| k == "Description"));
        assert_eq!(
            first_pkg_info("Metadata-Version: 2.1\nName: a\n\nMetadata-Version: 2.1\nName: b\n"),
            "Metadata-Version: 2.1\nName: a\n\n"
        );
    }

    #[test]
    fn form_fields_follow_the_legacy_api() {
        let u = Upload {
            path: PathBuf::from("minipkg-1.0-py3-none-any.whl"),
            filetype: "bdist_wheel",
            pyversion: "py3".into(),
            headers: vec![
                ("Metadata-Version".into(), "2.1".into()),
                ("Name".into(), "minipkg".into()),
                ("Version".into(), "1.0".into()),
                ("Classifier".into(), "A".into()),
                ("Classifier".into(), "B".into()),
                ("Unknown-Thing".into(), "x".into()),
            ],
            description: "body".into(),
            sha256: "ab".repeat(32),
        };
        let fields = form_fields(&u);
        let get = |k: &str| -> Vec<&str> {
            fields
                .iter()
                .filter(|(f, _)| f == k)
                .map(|(_, v)| v.as_str())
                .collect()
        };
        assert_eq!(get(":action"), vec!["file_upload"]);
        assert_eq!(get("metadata_version"), vec!["2.1"]);
        assert_eq!(get("classifiers"), vec!["A", "B"]);
        assert_eq!(get("filetype"), vec!["bdist_wheel"]);
        assert_eq!(get("description"), vec!["body"]);
        assert!(get("unknown_thing").is_empty());
        assert_eq!(curl_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn credentials_prefer_flags_then_tokens() {
        assert_eq!(
            credentials(Some("u".into()), Some("p".into())),
            Some(("u".into(), "p".into()))
        );
        assert_eq!(
            credentials(None, Some("tok".into())),
            Some(("__token__".into(), "tok".into()))
        );
    }
}
