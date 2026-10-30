//! A client for the simple repository API (PEP 503 HTML, PEP 691 JSON) and
//! wheel metadata (PEP 658), through curl like every other download.

use crate::json::{self, Value};
use crate::pep::{Requirement, Version};
use crate::{dirs, sha256, upgrade, zip};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;

pub const PYPI: &str = "https://pypi.org/simple";

/// One downloadable file of a project on the index.
#[derive(Debug, Clone)]
pub struct File {
    pub filename: String,
    pub url: String,
    pub sha256: Option<String>,
    pub requires_python: Option<String>,
    pub yanked: bool,
    /// PEP 658: `<url>.metadata` exists and is this file's core metadata.
    pub has_metadata: bool,
}

impl File {
    pub fn is_wheel(&self) -> bool {
        self.filename.ends_with(".whl")
    }

    /// The version in the file name (`name-1.2.3-py3-none-any.whl`, `name-1.2.3.tar.gz`).
    pub fn version(&self, name: &str) -> Option<Version> {
        let stem = self
            .filename
            .trim_end_matches(".whl")
            .trim_end_matches(".tar.gz")
            .trim_end_matches(".zip");
        let mut parts = stem.split('-');
        let file_name = parts.next()?;
        if crate::project::normalize(file_name) != crate::project::normalize(name) {
            return None;
        }
        Version::parse(parts.next()?).ok()
    }
}

/// Parsed core metadata: what the resolver needs from METADATA.
#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub requires_dist: Vec<Requirement>,
    pub requires_python: Option<String>,
    pub provides_extra: Vec<String>,
}

pub struct Index {
    base: String,
    /// A wheelhouse directory instead of a server: the only source, offline.
    local: Option<std::path::PathBuf>,
    listings: std::cell::RefCell<BTreeMap<String, Vec<File>>>,
    /// Names whose listing (and newest metadata) are being fetched ahead.
    prefetched: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

/// Where this index's listings are cached on disk.
fn listing_cache_dir(base: &str) -> Result<std::path::PathBuf> {
    let mut h = sha256::Sha256::new();
    h.update(base.as_bytes());
    Ok(dirs::cache()?
        .join("index")
        .join(&sha256::hex(&h.finish())[..16]))
}

fn listing_ttl() -> u64 {
    std::env::var("VIVAR_INDEX_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A temporary name no other writer in this process shares.
fn tmp_name(stem: &str) -> String {
    format!(
        "{stem}.{}-{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Prefetch work shared by every call: one bounded pool for the process.
type PrefetchJob = (String, String, String, Option<crate::wheelhouse::Target>);
static PREFETCH_QUEUE: std::sync::Mutex<Vec<PrefetchJob>> = std::sync::Mutex::new(Vec::new());
static PREFETCH_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const PREFETCH_PARALLELISM: usize = 8;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fetches a listing page and caches it (whole, then renamed), the way
/// `listing_body` does, from any thread. Errors are the caller's to decide on.
fn fetch_listing_to_cache(base: &str, authed_page: &str, name: &str) -> Result<String> {
    let cache = listing_cache_dir(base)?;
    let path = cache.join(format!("{name}.listing"));
    let body = upgrade::fetch(
        authed_page,
        Some("application/vnd.pypi.simple.v1+json, text/html;q=0.1"),
    );
    let text = match body {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if format!("{e:#}").contains(" 404") => String::new(),
        Err(e) => return Err(e),
    };
    if listing_ttl() > 0 {
        let _ = fs::create_dir_all(&cache);
        let tmp = cache.join(tmp_name(&format!("{name}.listing")));
        if fs::write(&tmp, format!("{}\n{text}", unix_now())).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
        let _ = fs::remove_file(&tmp);
    }
    Ok(text)
}

/// One prefetch job: the listing of `name` into the cache (unless fresh), then
/// the PEP 658 metadata of its newest wheel for `target`. Quiet on failure;
/// the real lookup reports what matters.
fn prefetch_one(
    base: &str,
    name: &str,
    authed_page: &str,
    target: Option<&crate::wheelhouse::Target>,
) {
    let cached = listing_cache_dir(base)
        .ok()
        .and_then(|d| fs::read_to_string(d.join(format!("{name}.listing"))).ok())
        .and_then(|t| {
            let (stamp, body) = t.split_once('\n')?;
            let stamp: u64 = stamp.parse().ok()?;
            (unix_now().saturating_sub(stamp) < listing_ttl()).then(|| body.to_owned())
        });
    let text = match cached {
        Some(t) => t,
        None => match fetch_listing_to_cache(base, authed_page, name) {
            Ok(t) => t,
            Err(_) => return,
        },
    };
    let Some(target) = target else { return };
    let page_plain = format!("{base}/{name}/");
    let files = if text.trim_start().starts_with('{') {
        parse_json(&text, &page_plain).unwrap_or_default()
    } else {
        parse_html(&text, &page_plain)
    };
    let mut by_version: BTreeMap<Version, Vec<&File>> = BTreeMap::new();
    for f in files.iter().filter(|f| f.is_wheel() && !f.yanked) {
        if let Some(v) = f.version(name) {
            by_version.entry(v).or_default().push(f);
        }
    }
    let Some((_, wheels)) = by_version.iter().next_back() else {
        return;
    };
    let Some(file) = wheels
        .iter()
        .find(|f| target.score(&f.filename).is_some() && f.has_metadata)
        .or_else(|| wheels.iter().find(|f| f.has_metadata))
    else {
        return;
    };
    let Ok(path) = metadata_cache_path(file) else {
        return;
    };
    if path.is_file() {
        return;
    }
    let url = format!("{}.metadata", file.url);
    let url = if same_origin(&url, base) {
        crate::backend::with_credentials(&url)
    } else {
        url
    };
    if let Ok(body) = upgrade::fetch(&url, None) {
        let text = String::from_utf8_lossy(&body);
        if text.starts_with("Metadata-Version:") {
            let tmp = path.with_file_name(tmp_name(
                &path.file_name().unwrap_or_default().to_string_lossy(),
            ));
            if fs::write(&tmp, text.as_bytes()).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
            let _ = fs::remove_file(&tmp);
        }
    }
}

/// The cached metadata file for an artifact.
fn metadata_cache_path(file: &File) -> Result<std::path::PathBuf> {
    let key = file.sha256.clone().unwrap_or_else(|| {
        sha256::hex(&{
            let mut h = sha256::Sha256::new();
            h.update(file.url.as_bytes());
            h.finish()
        })
    });
    let cache = dirs::cache()?.join("metadata");
    fs::create_dir_all(&cache)?;
    Ok(cache.join(format!("{key}.metadata")))
}

impl Index {
    /// `index_url` from policy: a mirror URL, or a wheelhouse directory that
    /// is then the only source (never PyPI behind it); PyPI when unset.
    pub fn new(index_url: Option<&str>) -> Index {
        match index_url {
            Some(u) if u.starts_with("file://") => {
                let dir = local_path(u)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| u.trim_start_matches("file://").to_owned());
                Index {
                    base: dir.clone(),
                    local: Some(std::path::PathBuf::from(dir)),
                    listings: Default::default(),
                    prefetched: Default::default(),
                }
            }
            Some(u) if u.contains("://") => Index {
                base: u.trim_end_matches('/').to_owned(),
                local: None,
                listings: Default::default(),
                prefetched: Default::default(),
            },
            Some(dir) => Index {
                base: dir.to_owned(),
                local: Some(std::path::PathBuf::from(dir)),
                listings: Default::default(),
                prefetched: Default::default(),
            },
            None => Index {
                base: PYPI.to_owned(),
                local: None,
                listings: Default::default(),
                prefetched: Default::default(),
            },
        }
    }

    pub fn url(&self) -> &str {
        &self.base
    }

    /// Fetches the listings of `names`, and the newest release's PEP 658
    /// metadata for `series`, into the on-disk cache on background threads
    /// (eight at a time), so the resolver's next lookups are local reads. A
    /// name already asked for, or a wheelhouse, is skipped; failures are left
    /// for the real lookup to report.
    pub fn prefetch(&self, names: &[String], series: &str) {
        if self.local.is_some() || listing_ttl() == 0 {
            return;
        }
        let fresh: Vec<String> = {
            let mut seen = self.prefetched.borrow_mut();
            names
                .iter()
                .map(|n| crate::project::normalize(n))
                .filter(|n| seen.insert(n.clone()))
                .collect()
        };
        if fresh.is_empty() {
            return;
        }
        let base = self.base.clone();
        let jobs: Vec<(String, String)> = fresh
            .into_iter()
            .map(|n| {
                let page = format!("{base}/{n}/");
                (n.clone(), self.authed(&page))
            })
            .collect();
        let target = crate::wheelhouse::Target::new(None, series).ok();
        let queued = jobs.len();
        if let Ok(mut q) = PREFETCH_QUEUE.lock() {
            for (name, page) in jobs {
                q.push((base.clone(), name, page, target.clone()));
            }
        }
        // Workers are shared across calls: at most eight in the process, each
        // draining the queue until it is empty. A worker retires under the
        // queue lock, so a job pushed under that lock is either seen by it or
        // counted against a spawn here; a lost compare-exchange is retried.
        use std::sync::atomic::Ordering;
        let mut spawned = 0;
        while spawned < queued {
            let active = PREFETCH_WORKERS.load(Ordering::SeqCst);
            if active >= PREFETCH_PARALLELISM {
                break;
            }
            if PREFETCH_WORKERS
                .compare_exchange(active, active + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            spawned += 1;
            std::thread::spawn(move || loop {
                let job = match PREFETCH_QUEUE.lock() {
                    Ok(mut q) => match q.pop() {
                        Some(j) => j,
                        None => {
                            PREFETCH_WORKERS.fetch_sub(1, Ordering::SeqCst);
                            return;
                        }
                    },
                    Err(_) => {
                        PREFETCH_WORKERS.fetch_sub(1, Ordering::SeqCst);
                        return;
                    }
                };
                let (base, name, page, target) = job;
                prefetch_one(&base, &name, &page, target.as_ref());
            });
        }
    }

    /// Credentials for the index go to the index's own host only.
    fn authed(&self, url: &str) -> String {
        if same_origin(url, &self.base) {
            crate::backend::with_credentials(url)
        } else {
            url.to_owned()
        }
    }

    /// Every file of `name`, newest listings cached for the process.
    pub fn files(&self, name: &str) -> Result<Vec<File>> {
        let name = crate::project::normalize(name);
        if let Some(cached) = self.listings.borrow().get(&name) {
            return Ok(cached.clone());
        }
        let files = match &self.local {
            Some(dir) => local_files(dir, &name)?,
            None => {
                // Links resolve against the page without credentials: what the
                // lock records must never carry them.
                let page = format!("{}/{name}/", self.base);
                let text = self.listing_body(&name, &page)?;
                if text.trim_start().starts_with('{') {
                    parse_json(&text, &page)?
                } else {
                    parse_html(&text, &page)
                }
            }
        };
        self.listings.borrow_mut().insert(name, files.clone());
        Ok(files)
    }

    /// A project's listing: from the on-disk cache while it is fresh
    /// (`VIVAR_INDEX_TTL` seconds, default 600, 0 to bypass), else fetched
    /// and cached. A stale copy still serves when the index is unreachable,
    /// so a resolve on a flaky link does not re-download the world.
    fn listing_body(&self, name: &str, page: &str) -> Result<String> {
        let ttl: u64 = std::env::var("VIVAR_INDEX_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600);
        let cache = dirs::cache()?.join("index").join(
            &{
                let mut h = sha256::Sha256::new();
                h.update(self.base.as_bytes());
                sha256::hex(&h.finish())
            }[..16],
        );
        let path = cache.join(format!("{name}.listing"));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cached = fs::read_to_string(&path).ok().and_then(|t| {
            let (stamp, body) = t.split_once('\n')?;
            Some((stamp.parse::<u64>().ok()?, body.to_owned()))
        });
        if let Some((stamp, body)) = &cached {
            if ttl > 0 && now.saturating_sub(*stamp) < ttl {
                return Ok(body.clone());
            }
        }
        match upgrade::fetch(
            &self.authed(page),
            Some("application/vnd.pypi.simple.v1+json, text/html;q=0.1"),
        ) {
            Ok(body) => {
                let text = String::from_utf8_lossy(&body).into_owned();
                if ttl > 0 {
                    // Written whole, then renamed: a full disk or a concurrent
                    // refresh never leaves a half entry behind a valid stamp.
                    let _ = fs::create_dir_all(&cache);
                    let tmp = cache.join(tmp_name(&format!("{name}.listing")));
                    if fs::write(&tmp, format!("{now}\n{text}")).is_ok() {
                        let _ = fs::rename(&tmp, &path);
                    }
                    let _ = fs::remove_file(&tmp);
                }
                Ok(text)
            }
            // An index that has never heard of the name answers 404: that is an
            // empty listing, not a failure to fall back from.
            Err(e) if format!("{e:#}").contains(" 404") => {
                if ttl > 0 {
                    let _ = fs::create_dir_all(&cache);
                    let _ = fs::write(&path, format!("{now}\n"));
                }
                Ok(String::new())
            }
            Err(e) => match cached {
                Some((stamp, body)) if ttl > 0 => {
                    eprintln!(
                        "note: {} is unreachable ({e:#}); using the listing for {name} cached {}s ago",
                        self.base,
                        now.saturating_sub(stamp)
                    );
                    Ok(body)
                }
                _ => Err(e).with_context(|| format!("looking up {name} on {}", self.base)),
            },
        }
    }

    /// Core metadata for a wheel: the PEP 658 sidecar when offered, else the
    /// wheel itself (cached by hash so each is read at most once; a cached
    /// copy that does not look like metadata is fetched again).
    pub fn metadata(&self, file: &File) -> Result<Metadata> {
        let cache = dirs::cache()?.join("metadata");
        fs::create_dir_all(&cache)?;
        let key = file.sha256.clone().unwrap_or_else(|| {
            sha256::hex(&{
                let mut h = sha256::Sha256::new();
                h.update(file.url.as_bytes());
                h.finish()
            })
        });
        let cached = cache.join(format!("{key}.metadata"));
        let valid = fs::read_to_string(&cached)
            .ok()
            .filter(|t| t.starts_with("Metadata-Version:"));
        let text = if let Some(t) = valid {
            t
        } else {
            let text = if let Some(path) = local_path(&file.url) {
                metadata_from_wheel(&fs::read(&path)?)
                    .with_context(|| format!("reading METADATA from {}", path.display()))?
            } else if file.has_metadata {
                String::from_utf8_lossy(&upgrade::fetch(
                    &self.authed(&format!("{}.metadata", file.url)),
                    None,
                )?)
                .into_owned()
            } else {
                let data = upgrade::fetch(&self.authed(&file.url), None).with_context(|| {
                    format!("downloading {} to read its metadata", file.filename)
                })?;
                if let Some(h) = &file.sha256 {
                    let mut hasher = sha256::Sha256::new();
                    hasher.update(&data);
                    if sha256::hex(&hasher.finish()) != *h {
                        bail!(
                            "{} does not match the hash the index advertises",
                            file.filename
                        );
                    }
                }
                metadata_from_wheel(&data)
                    .with_context(|| format!("reading METADATA from {}", file.filename))?
            };
            let _ = fs::write(&cached, &text);
            text
        };
        Ok(parse_metadata(&text))
    }
}

/// Whether two URLs share scheme and host (and port).
pub fn same_origin(a: &str, b: &str) -> bool {
    fn origin(u: &str) -> Option<String> {
        let (scheme, rest) = u.split_once("://")?;
        let host = rest.split('/').next()?;
        let host = host.rsplit('@').next()?; // never compare userinfo
        Some(format!(
            "{}://{}",
            scheme.to_lowercase(),
            host.to_lowercase()
        ))
    }
    matches!((origin(a), origin(b)), (Some(x), Some(y)) if x == y)
}

/// A `file://` URL for a wheelhouse member: absolute, percent-encoded, and
/// decoded back by `local_path`.
fn file_url(path: &std::path::Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let p = abs.display().to_string().replace('\\', "/");
    let mut out = String::from("file:///");
    for b in p.trim_start_matches('/').bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/:".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn local_path(url: &str) -> Option<std::path::PathBuf> {
    let rest = url.strip_prefix("file:///")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let Some(v) = std::str::from_utf8(&raw[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                bytes.push(v);
                i += 3;
                continue;
            }
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let decoded = String::from_utf8(bytes).ok()?;
    if cfg!(windows) {
        Some(std::path::PathBuf::from(decoded.replace('/', "\\")))
    } else {
        Some(std::path::PathBuf::from(format!("/{decoded}")))
    }
}

/// The wheelhouse's files for `name`, hashed so the lock can pin them.
fn local_files(dir: &std::path::Path, name: &str) -> Result<Vec<File>> {
    let mut out = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading wheelhouse {}", dir.display()))?
    {
        let entry = entry?;
        let filename = entry.file_name().to_string_lossy().into_owned();
        if !(filename.ends_with(".whl")
            || filename.ends_with(".tar.gz")
            || filename.ends_with(".zip"))
        {
            continue;
        }
        let probe = File {
            filename: filename.clone(),
            url: String::new(),
            sha256: None,
            requires_python: None,
            yanked: false,
            has_metadata: false,
        };
        if probe.version(name).is_none() {
            continue;
        }
        out.push(File {
            url: file_url(&entry.path()),
            sha256: Some(sha256::file(&entry.path())?),
            ..probe
        });
    }
    Ok(out)
}

fn parse_json(text: &str, page_url: &str) -> Result<Vec<File>> {
    let doc = json::parse(text)?;
    let files = doc
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("index response has no `files`"))?;
    Ok(files
        .iter()
        .filter_map(|f| {
            let filename = f.get("filename")?.as_str()?.to_owned();
            let url = absolute(page_url, f.get("url")?.as_str()?);
            let sha256 = f
                .get("hashes")
                .and_then(|h| h.get("sha256"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let requires_python = f
                .get("requires-python")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let yanked = match f.get("yanked") {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(_)) => true,
                _ => false,
            };
            let has_metadata = ["core-metadata", "data-dist-info-metadata"]
                .iter()
                .any(|k| matches!(f.get(k), Some(Value::Bool(true)) | Some(Value::Object(_))));
            Some(File {
                filename,
                url,
                sha256,
                requires_python,
                yanked,
                has_metadata,
            })
        })
        .collect())
}

/// PEP 503: `<a href="…#sha256=…" data-requires-python="…" data-dist-info-metadata>name</a>`.
fn parse_html(text: &str, page_url: &str) -> Vec<File> {
    let mut out = Vec::new();
    for chunk in text.split("<a ").skip(1) {
        let Some(tag_end) = chunk.find('>') else {
            continue;
        };
        let tag = &chunk[..tag_end];
        let filename = chunk[tag_end + 1..]
            .split('<')
            .next()
            .unwrap_or("")
            .trim()
            .to_owned();
        let attr = |name: &str| -> Option<String> {
            let pos = tag.find(&format!("{name}=\""))?;
            let rest = &tag[pos + name.len() + 2..];
            Some(html_unescape(&rest[..rest.find('"')?]))
        };
        let Some(href) = attr("href") else { continue };
        let (url, fragment) = href.split_once('#').unwrap_or((&href, ""));
        let url = absolute(page_url, url);
        out.push(File {
            filename,
            url,
            sha256: fragment.strip_prefix("sha256=").map(str::to_owned),
            requires_python: attr("data-requires-python"),
            yanked: tag.contains("data-yanked"),
            has_metadata: tag.contains("data-core-metadata")
                || tag.contains("data-dist-info-metadata"),
        });
    }
    out
}

fn html_unescape(s: &str) -> String {
    s.replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn absolute(page: &str, url: &str) -> String {
    if url.contains("://") {
        url.to_owned()
    } else {
        join_url(page, url)
    }
}

fn join_url(page: &str, rel: &str) -> String {
    if let Some(path) = rel.strip_prefix('/') {
        let scheme_end = page.find("://").map(|i| i + 3).unwrap_or(0);
        let host_end = page[scheme_end..]
            .find('/')
            .map(|i| scheme_end + i)
            .unwrap_or(page.len());
        return format!("{}/{path}", &page[..host_end]);
    }
    let base = page.trim_end_matches('/');
    let mut parts: Vec<&str> = base.split('/').collect();
    let mut rel_parts = rel.split('/').peekable();
    while let Some(p) = rel_parts.peek() {
        match *p {
            ".." => {
                parts.pop();
                rel_parts.next();
            }
            "." => {
                rel_parts.next();
            }
            _ => break,
        }
    }
    format!(
        "{}/{}",
        parts.join("/"),
        rel_parts.collect::<Vec<_>>().join("/")
    )
}

/// METADATA out of a wheel's `.dist-info`.
pub fn metadata_from_wheel(data: &[u8]) -> Result<String> {
    let entries = zip::entries(data)?;
    let entry = entries
        .iter()
        .find(|e| e.name.ends_with(".dist-info/METADATA") && e.name.matches('/').count() == 1)
        .ok_or_else(|| anyhow!("wheel has no METADATA"))?;
    Ok(String::from_utf8_lossy(&zip::read(data, entry)?).into_owned())
}

pub fn parse_metadata(text: &str) -> Metadata {
    let mut m = Metadata::default();
    let mut last: Option<&mut String> = None;
    let mut lines: Vec<String> = Vec::new();
    // Headers end at the first blank line; continuation lines start with whitespace.
    for line in text.lines() {
        if line.is_empty() {
            break;
        }
        if line.starts_with([' ', '\t']) {
            if let Some(l) = last.as_deref_mut() {
                l.push(' ');
                l.push_str(line.trim());
            }
            continue;
        }
        lines.push(line.to_owned());
        last = lines.last_mut();
    }
    for line in &lines {
        if let Some(v) = line.strip_prefix("Requires-Dist:") {
            if let Ok(r) = Requirement::parse(v.trim()) {
                m.requires_dist.push(r);
            }
        } else if let Some(v) = line.strip_prefix("Requires-Python:") {
            m.requires_python = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Provides-Extra:") {
            m.provides_extra.push(crate::project::normalize(v.trim()));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_and_html_listings() {
        let json = r#"{"files":[{"filename":"six-1.17.0-py2.py3-none-any.whl","url":"https://files/six.whl","hashes":{"sha256":"abc"},"requires-python":">=2.7","yanked":false,"core-metadata":{"sha256":"d"}},{"filename":"six-1.17.0.tar.gz","url":"../../packages/six.tar.gz","hashes":{},"yanked":"broken"}]}"#;
        let files = parse_json(json, "https://mirror.example/simple/six/").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].url, "https://mirror.example/packages/six.tar.gz");
        assert!(same_origin(
            "https://User:pw@Mirror.example/simple/",
            "https://mirror.example/packages/x.whl"
        ));
        assert!(!same_origin(
            "https://mirror.example/simple/",
            "https://files.pythonhosted.org/x.whl"
        ));
        assert!(!same_origin(
            "https://mirror.example/",
            "http://mirror.example/"
        ));
        assert!(files[0].is_wheel() && files[0].has_metadata && !files[0].yanked);
        assert_eq!(files[0].sha256.as_deref(), Some("abc"));
        assert_eq!(files[0].version("Six").unwrap().to_string(), "1.17.0");
        assert!(files[1].yanked && files[1].sha256.is_none());

        let html = r#"<html><body><a href="../../packages/six-1.16.0.tar.gz#sha256=1e61" data-requires-python="&gt;=2.7">six-1.16.0.tar.gz</a><br/><a href="/packages/six-1.17.0-py2.py3-none-any.whl#sha256=4f1d" data-dist-info-metadata="sha256=x" data-yanked="">six-1.17.0-py2.py3-none-any.whl</a></body></html>"#;
        let files = parse_html(html, "https://mirror.example/simple/six/");
        assert_eq!(files.len(), 2);
        assert_eq!(
            files[0].url,
            "https://mirror.example/packages/six-1.16.0.tar.gz"
        );
        assert_eq!(files[0].requires_python.as_deref(), Some(">=2.7"));
        assert_eq!(
            files[1].url,
            "https://mirror.example/packages/six-1.17.0-py2.py3-none-any.whl"
        );
        assert!(files[1].has_metadata && files[1].yanked);
    }

    #[test]
    fn a_wheelhouse_directory_is_the_whole_index() {
        let dir = std::env::temp_dir().join(format!("vivar wh#{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("six-1.17.0-py2.py3-none-any.whl"), b"not really").unwrap();
        fs::write(dir.join("Six-1.16.0.tar.gz"), b"src").unwrap();
        fs::write(dir.join("other-1.0-py3-none-any.whl"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        let index = Index::new(Some(dir.to_str().unwrap()));
        let files = index.files("six").unwrap();
        assert_eq!(files.len(), 2, "{files:?}");
        assert!(files
            .iter()
            .all(|f| f.sha256.is_some() && f.url.starts_with("file:///")));
        assert!(
            files[0].url.contains("%23") && !files[0].url.contains(' '),
            "{}",
            files[0].url
        );
        assert_eq!(
            local_path(&files[0].url).unwrap(),
            dir.join(&files[0].filename)
        );
        assert_eq!(
            crate::lock::Artifact {
                url: files[0].url.clone(),
                hash: None
            }
            .filename(),
            files[0].filename
        );
        assert!(index.files("missing").unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_metadata() {
        let m = parse_metadata("Metadata-Version: 2.1\nName: x\nRequires-Python: >=3.9\nRequires-Dist: numpy>=1.20\nRequires-Dist: rich ; extra == 'cli'\nRequires-Dist: long-one>=1,\n  <2\nProvides-Extra: cli\n\nRequires-Dist: not-a-header\n");
        assert_eq!(m.requires_python.as_deref(), Some(">=3.9"));
        assert_eq!(m.requires_dist.len(), 3);
        assert_eq!(m.requires_dist[1].marker.as_deref(), Some("extra == 'cli'"));
        assert_eq!(m.requires_dist[2].specifier.to_string(), ">=1,<2");
        assert_eq!(m.provides_extra, vec!["cli"]);
    }
}
