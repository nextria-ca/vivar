#![allow(dead_code)]
//! conda channels: the sharded repodata (CEP 16) every modern channel and
//! mirror serves, cached per package shard, with the whole `repodata.json`
//! as the fallback; and this machine's virtual packages (`__glibc`,
//! `__cuda` …), which solves run against.

use crate::condaspec::Record;
use crate::{dirs, msgpack, sha256, upgrade, zstd};
use anyhow::{anyhow, bail, Context, Result};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// The conda subdirs vivar targets, in the order it names platforms.
pub const SUBDIRS: &[(&str, &str, &str)] = &[
    ("linux-64", "linux", "x86_64"),
    ("linux-aarch64", "linux", "aarch64"),
    ("osx-64", "macos", "x86_64"),
    ("osx-arm64", "macos", "aarch64"),
    ("win-64", "windows", "x86_64"),
    ("win-arm64", "windows", "aarch64"),
];

/// The subdir for `<os>-<arch>` as vivar spells platforms, or a subdir given as is.
pub fn subdir_for(platform: &str) -> Result<String> {
    if SUBDIRS.iter().any(|(s, _, _)| *s == platform) {
        return Ok(platform.to_owned());
    }
    let (os, arch) = platform
        .split_once('-')
        .ok_or_else(|| anyhow!("platform must look like linux-x86_64, got {platform:?}"))?;
    let os = match os {
        "macos" | "darwin" | "osx" => "macos",
        "linux" | "windows" => os,
        other => bail!("unsupported platform OS {other:?}"),
    };
    let arch = match arch {
        "x86_64" | "amd64" | "x64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => bail!("unsupported platform architecture {other:?}"),
    };
    SUBDIRS
        .iter()
        .find(|(_, o, a)| *o == os && *a == arch)
        .map(|(s, _, _)| s.to_string())
        .ok_or_else(|| anyhow!("no conda subdir for {platform}"))
}

/// This machine's subdir.
pub fn host_subdir() -> String {
    subdir_for(&format!(
        "{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
    .unwrap_or_else(|_| "linux-64".to_owned())
}

pub fn platform_of_subdir(subdir: &str) -> String {
    SUBDIRS
        .iter()
        .find(|(s, _, _)| *s == subdir)
        .map(|(_, o, a)| format!("{o}-{a}"))
        .unwrap_or_else(|| subdir.to_owned())
}

/// One channel at one subdir.
pub struct Channel {
    /// `https://conda.anaconda.org/conda-forge`
    pub base: String,
    pub subdir: String,
    index: RefCell<Option<ShardIndex>>,
    /// Records by name, once a shard (or the whole repodata) was read.
    records: RefCell<BTreeMap<String, Vec<Record>>>,
    /// The whole `repodata.json`, for channels without shards.
    whole: RefCell<Option<BTreeMap<String, Vec<Record>>>>,
}

struct ShardIndex {
    /// Package name → shard hash (hex).
    shards: BTreeMap<String, String>,
    /// Where shards live (`<base>/<subdir>/<shards_base_url>`).
    shards_url: String,
    /// Where packages are downloaded from.
    base_url: String,
}

fn ttl() -> u64 {
    std::env::var("VIVAR_CONDA_TTL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolves a relative URL against `page` (the subdir's URL with a slash).
fn join(page: &str, rel: &str) -> String {
    if rel.contains("://") {
        return rel.to_owned();
    }
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    format!("{}/{rel}", page.trim_end_matches('/'))
}

impl Channel {
    pub fn new(base: &str, subdir: &str) -> Channel {
        Channel {
            base: base.trim_end_matches('/').to_owned(),
            subdir: subdir.to_owned(),
            index: RefCell::new(None),
            records: RefCell::new(BTreeMap::new()),
            whole: RefCell::new(None),
        }
    }

    fn subdir_url(&self) -> String {
        format!("{}/{}/", self.base, self.subdir)
    }

    fn cache_dir(&self) -> Result<PathBuf> {
        let mut h = sha256::Sha256::new();
        h.update(self.subdir_url().as_bytes());
        Ok(dirs::cache()?
            .join("conda")
            .join(&sha256::hex(&h.finish())[..16]))
    }

    /// A file fetched from the channel, cached for `ttl` seconds (forever
    /// when `immutable`), the stale copy serving when the network is down.
    fn cached(&self, url: &str, name: &str, immutable: bool) -> Result<Vec<u8>> {
        let dir = self.cache_dir()?;
        let path = dir.join(name);
        let stamp = dir.join(format!("{name}.stamp"));
        if path.is_file() {
            let fresh = immutable
                || fs::read_to_string(&stamp)
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .is_some_and(|t| unix_now().saturating_sub(t) < ttl());
            if fresh {
                return Ok(fs::read(&path)?);
            }
        }
        fs::create_dir_all(&dir)?;
        let part = dir.join(format!("{name}.part-{}", std::process::id()));
        match upgrade::fetch_to(url, &part) {
            Ok(()) => {
                fs::rename(&part, &path)?;
                let _ = fs::write(&stamp, unix_now().to_string());
                Ok(fs::read(&path)?)
            }
            Err(e) => {
                let _ = fs::remove_file(&part);
                if path.is_file() {
                    eprintln!("note: {url} unreachable; using the cached copy ({e:#})");
                    return Ok(fs::read(&path)?);
                }
                Err(e)
            }
        }
    }

    fn load_index(&self) -> Result<bool> {
        if self.index.borrow().is_some() || self.whole.borrow().is_some() {
            return Ok(self.index.borrow().is_some());
        }
        let page = self.subdir_url();
        let url = format!("{page}repodata_shards.msgpack.zst");
        match self.cached(&url, "repodata_shards.msgpack.zst", false) {
            Ok(bytes) => {
                let doc = msgpack::parse(&zstd::decompress(&bytes)?)
                    .context("reading the channel's shard index")?;
                let info = doc.get("info");
                let shards_base = info
                    .and_then(|i| i.get("shards_base_url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let base_url = info
                    .and_then(|i| i.get("base_url"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut shards = BTreeMap::new();
                for (k, v) in doc
                    .get("shards")
                    .and_then(|s| s.as_map())
                    .ok_or_else(|| anyhow!("the shard index lists no shards"))?
                {
                    if let (Some(name), Some(hash)) = (k.as_str(), v.as_bytes()) {
                        shards.insert(
                            name.to_lowercase(),
                            hash.iter().map(|b| format!("{b:02x}")).collect(),
                        );
                    }
                }
                *self.index.borrow_mut() = Some(ShardIndex {
                    shards,
                    shards_url: if shards_base.is_empty() {
                        page.clone()
                    } else {
                        join(&page, shards_base)
                    },
                    base_url: if base_url.is_empty() {
                        page
                    } else {
                        join(&page, base_url)
                    },
                });
                Ok(true)
            }
            Err(e) if format!("{e:#}").contains(" 404") || format!("{e:#}").contains("404") => {
                // No shards: the whole repodata, once per TTL.
                self.load_whole()?;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    fn load_whole(&self) -> Result<()> {
        let page = self.subdir_url();
        let bytes = match self.cached(
            &format!("{page}repodata.json.zst"),
            "repodata.json.zst",
            false,
        ) {
            Ok(b) => zstd::decompress(&b)?,
            Err(_) => self.cached(&format!("{page}repodata.json"), "repodata.json", false)?,
        };
        let text = String::from_utf8_lossy(&bytes);
        let doc = crate::json::parse(&text).context("reading repodata.json")?;
        let mut by_name: BTreeMap<String, Vec<Record>> = BTreeMap::new();
        for key in ["packages", "packages.conda"] {
            if let Some(crate::json::Value::Object(map)) = doc.get(key) {
                for (filename, rec) in map {
                    if let Some(r) = record_from_json(filename, rec, &self.subdir, &page) {
                        by_name.entry(r.name.clone()).or_default().push(r);
                    }
                }
            }
        }
        for recs in by_name.values_mut() {
            prefer_conda_format(recs);
        }
        *self.whole.borrow_mut() = Some(by_name);
        Ok(())
    }

    /// Every record of `name` in this channel, newest first.
    pub fn records(&self, name: &str) -> Result<Vec<Record>> {
        let name = name.to_lowercase();
        if let Some(r) = self.records.borrow().get(&name) {
            return Ok(r.clone());
        }
        let sharded = self.load_index()?;
        let mut out = if sharded {
            let (hash, shards_url, base_url) = {
                let idx = self.index.borrow();
                let idx = idx.as_ref().unwrap();
                match idx.shards.get(&name) {
                    Some(h) => (h.clone(), idx.shards_url.clone(), idx.base_url.clone()),
                    None => {
                        self.records.borrow_mut().insert(name, Vec::new());
                        return Ok(Vec::new());
                    }
                }
            };
            let url = format!("{shards_url}{hash}.msgpack.zst");
            let bytes = self.cached(&url, &format!("{hash}.msgpack.zst"), true)?;
            let doc = msgpack::parse(&zstd::decompress(&bytes)?)
                .with_context(|| format!("reading the shard of {name}"))?;
            let mut recs = Vec::new();
            for key in ["packages", "packages.conda"] {
                if let Some(map) = doc.get(key).and_then(|m| m.as_map()) {
                    for (k, v) in map {
                        if let Some(filename) = k.as_str() {
                            if let Some(r) =
                                Record::from_msgpack(filename, v, &self.subdir, &base_url)
                            {
                                recs.push(r);
                            }
                        }
                    }
                }
            }
            prefer_conda_format(&mut recs);
            recs
        } else {
            self.whole
                .borrow()
                .as_ref()
                .and_then(|w| w.get(&name))
                .cloned()
                .unwrap_or_default()
        };
        out.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));
        self.records.borrow_mut().insert(name, out.clone());
        Ok(out)
    }
}

/// The same build in both formats: keep the `.conda` one.
fn prefer_conda_format(recs: &mut Vec<Record>) {
    let conda: std::collections::BTreeSet<(String, String, String)> = recs
        .iter()
        .filter(|r| r.filename.ends_with(".conda"))
        .map(|r| (r.name.clone(), r.version.clone(), r.build.clone()))
        .collect();
    recs.retain(|r| {
        r.filename.ends_with(".conda")
            || !conda.contains(&(r.name.clone(), r.version.clone(), r.build.clone()))
    });
}

fn record_from_json(
    filename: &str,
    v: &crate::json::Value,
    subdir: &str,
    base_url: &str,
) -> Option<Record> {
    use crate::json::Value;
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
    let num = |k: &str| match v.get(k) {
        Some(Value::Number(n)) => *n as u64,
        _ => 0,
    };
    let list = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(Record {
        name: text("name")?.to_lowercase(),
        version: text("version")?,
        build: text("build").unwrap_or_default(),
        build_number: num("build_number"),
        depends: list("depends"),
        constrains: list("constrains"),
        subdir: text("subdir").unwrap_or_else(|| subdir.to_owned()),
        filename: filename.to_owned(),
        url: format!("{base_url}{filename}"),
        sha256: text("sha256"),
        md5: text("md5"),
        size: num("size"),
        license: text("license"),
        noarch: text("noarch"),
        timestamp: num("timestamp"),
        track_features: text("track_features")
            .map(|t| t.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Virtual packages

/// A virtual package record (`__glibc 2.35`), as conda would list it.
pub fn virtual_record(name: &str, version: &str, build: &str, subdir: &str) -> Record {
    Record {
        name: name.to_owned(),
        version: version.to_owned(),
        build: build.to_owned(),
        build_number: 0,
        depends: Vec::new(),
        constrains: Vec::new(),
        subdir: subdir.to_owned(),
        filename: String::new(),
        url: String::new(),
        sha256: None,
        md5: None,
        size: 0,
        license: None,
        noarch: None,
        timestamp: 0,
        track_features: Vec::new(),
    }
}

/// The virtual packages of this machine: detected where it runs, from
/// conservative defaults for another platform (`CONDA_OVERRIDE_*` wins).
pub fn virtual_packages(subdir: &str) -> Vec<Record> {
    let host = subdir == host_subdir();
    let os = platform_of_subdir(subdir);
    let (os, arch) = os.split_once('-').unwrap_or(("linux", "x86_64"));
    let over = |k: &str| std::env::var(format!("CONDA_OVERRIDE_{k}")).ok();
    let mut out = Vec::new();
    match os {
        "linux" => {
            out.push(virtual_record("__unix", "0", "0", subdir));
            let kernel = over("LINUX")
                .or_else(|| host.then(linux_kernel).flatten())
                .unwrap_or_else(|| "5.10".to_owned());
            out.push(virtual_record("__linux", &kernel, "0", subdir));
            let glibc = over("GLIBC")
                .or_else(|| host.then(glibc_version).flatten())
                .unwrap_or_else(|| "2.17".to_owned());
            out.push(virtual_record("__glibc", &glibc, "0", subdir));
        }
        "macos" => {
            out.push(virtual_record("__unix", "0", "0", subdir));
            let v = over("OSX")
                .or_else(|| host.then(macos_version).flatten())
                .unwrap_or_else(|| if arch == "aarch64" { "11.0" } else { "10.13" }.to_owned());
            out.push(virtual_record("__osx", &v, "0", subdir));
        }
        _ => out.push(virtual_record("__win", "0", "0", subdir)),
    }
    out.push(virtual_record("__archspec", "1", arch, subdir));
    let cuda = over("CUDA").or_else(|| {
        if !host {
            return None;
        }
        match crate::gpu::detect() {
            crate::gpu::Gpu::Nvidia {
                cuda: Some((a, b)), ..
            } => Some(format!("{a}.{b}")),
            _ => None,
        }
    });
    if let Some(c) = cuda {
        out.push(virtual_record("__cuda", &c, "0", subdir));
    }
    out
}

fn linux_kernel() -> Option<String> {
    let out = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let v: String = text
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    (!v.is_empty()).then_some(v)
}

fn glibc_version() -> Option<String> {
    for (cmd, args) in [
        ("getconf", vec!["GNU_LIBC_VERSION"]),
        ("ldd", vec!["--version"]),
    ] {
        if let Ok(out) = std::process::Command::new(cmd).args(&args).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            let first = text.lines().next().unwrap_or("");
            if let Some(v) = first.split_whitespace().last() {
                if v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return Some(v.to_owned());
                }
            }
        }
    }
    None
}

fn macos_version() -> Option<String> {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!v.is_empty()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdirs_and_virtual_packages() {
        assert_eq!(subdir_for("linux-x86_64").unwrap(), "linux-64");
        assert_eq!(subdir_for("macos-arm64").unwrap(), "osx-arm64");
        assert_eq!(subdir_for("windows-x86_64").unwrap(), "win-64");
        assert_eq!(subdir_for("osx-64").unwrap(), "osx-64");
        assert!(subdir_for("plan9-mips").is_err());
        assert_eq!(platform_of_subdir("win-arm64"), "windows-aarch64");
        std::env::set_var("CONDA_OVERRIDE_GLIBC", "2.28");
        std::env::set_var("CONDA_OVERRIDE_CUDA", "12.4");
        let v = virtual_packages("linux-aarch64");
        let names: Vec<&str> = v.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["__unix", "__linux", "__glibc", "__archspec", "__cuda"]
        );
        assert_eq!(v[2].version, "2.28");
        assert_eq!(v[3].build, "aarch64");
        assert_eq!(v[4].version, "12.4");
        std::env::remove_var("CONDA_OVERRIDE_GLIBC");
        std::env::remove_var("CONDA_OVERRIDE_CUDA");
        let w = virtual_packages("win-64");
        assert_eq!(w[0].name, "__win");
        assert_eq!(
            join("https://c/conda-forge/noarch/", "./shards/"),
            "https://c/conda-forge/noarch/shards/"
        );
        assert_eq!(join("https://c/x/", "https://cdn/x/"), "https://cdn/x/");
    }
}
