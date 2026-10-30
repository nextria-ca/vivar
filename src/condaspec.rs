#![allow(dead_code)]
//! conda's version ordering and match specs (`name >=1.2,<2 *_cp312`), as
//! conda-forge's `depends` and `constrains` spell them, plus the package
//! record read from repodata.

use anyhow::{anyhow, bail, Result};
use std::cmp::Ordering;

/// One piece of a version component: a number, or a string that sorts
/// below numbers (`dev` below every other string, `post` above everything).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Dev,
    Str(String),
    Num(u64),
    Post,
}

impl Piece {
    fn rank(&self) -> u8 {
        match self {
            Piece::Dev => 0,
            Piece::Str(_) => 1,
            Piece::Num(_) => 2,
            Piece::Post => 3,
        }
    }
}

impl Ord for Piece {
    fn cmp(&self, other: &Piece) -> Ordering {
        match (self, other) {
            (Piece::Str(a), Piece::Str(b)) => a.cmp(b),
            (Piece::Num(a), Piece::Num(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for Piece {
    fn partial_cmp(&self, other: &Piece) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A conda version: epoch, components (each a run of numbers and strings,
/// always starting with a number), and a local part. Equality is by
/// ordering (`0.4 == 0.4.0`), not by spelling.
#[derive(Debug, Clone)]
pub struct Version {
    epoch: u64,
    parts: Vec<Vec<Piece>>,
    local: Vec<Vec<Piece>>,
    text: String,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn split_component(part: &str) -> Result<Vec<Piece>> {
    if part.is_empty() {
        bail!("empty version component");
    }
    let mut pieces = Vec::new();
    let chars: Vec<char> = part.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let digit = chars[i].is_ascii_digit();
        let star = chars[i] == '*';
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() == digit && (chars[i] == '*') == star {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();
        pieces.push(if digit {
            Piece::Num(
                run.parse()
                    .map_err(|_| anyhow!("version number too large: {run}"))?,
            )
        } else if run == "post" {
            Piece::Post
        } else if run == "dev" {
            Piece::Dev
        } else {
            Piece::Str(run)
        });
    }
    // Components start with a number, so numbers and strings stay in phase.
    if !matches!(pieces.first(), Some(Piece::Num(_))) {
        pieces.insert(0, Piece::Num(0));
    }
    Ok(pieces)
}

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let text = s.trim().to_lowercase();
        if text.is_empty() {
            bail!("empty version");
        }
        if !text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "*.+!_".contains(c))
        {
            bail!("invalid character in version {s:?}");
        }
        let (epoch, rest) = match text.split_once('!') {
            Some((e, r)) => (
                e.parse()
                    .map_err(|_| anyhow!("invalid epoch in version {s:?}"))?,
                r,
            ),
            None => (0, text.as_str()),
        };
        let (main, local) = match rest.split_once('+') {
            Some((m, l)) => (m, Some(l)),
            None => (rest, None),
        };
        let parts = main
            .split('.')
            .map(split_component)
            .collect::<Result<Vec<_>>>()
            .map_err(|e| anyhow!("{e} in version {s:?}"))?;
        let local = match local {
            Some(l) => l
                .replace('_', ".")
                .split('.')
                .map(split_component)
                .collect::<Result<Vec<_>>>()
                .map_err(|e| anyhow!("{e} in version {s:?}"))?,
            None => Vec::new(),
        };
        Ok(Version {
            epoch,
            parts,
            local,
            text,
        })
    }

    /// Whether this version starts with `prefix`'s components (`1.2.*`).
    pub fn starts_with(&self, prefix: &Version) -> bool {
        if self.epoch != prefix.epoch {
            return false;
        }
        let n = prefix.parts.len();
        if n == 0 {
            return true;
        }
        for i in 0..n - 1 {
            if cmp_components(self.parts.get(i), Some(&prefix.parts[i])) != Ordering::Equal {
                return false;
            }
        }
        // The last component of the prefix may be a prefix of ours.
        let last = &prefix.parts[n - 1];
        let own = self
            .parts
            .get(n - 1)
            .cloned()
            .unwrap_or_else(|| vec![Piece::Num(0)]);
        own.len() >= last.len() && own[..last.len()] == last[..]
    }
}

fn cmp_components(a: Option<&Vec<Piece>>, b: Option<&Vec<Piece>>) -> Ordering {
    let zero = vec![Piece::Num(0)];
    let a = a.unwrap_or(&zero);
    let b = b.unwrap_or(&zero);
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).cloned().unwrap_or(Piece::Num(0));
        let y = b.get(i).cloned().unwrap_or(Piece::Num(0));
        match x.cmp(&y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        if self.epoch != other.epoch {
            return self.epoch.cmp(&other.epoch);
        }
        for (a, b) in [(&self.parts, &other.parts), (&self.local, &other.local)] {
            let n = a.len().max(b.len());
            for i in 0..n {
                match cmp_components(a.get(i), b.get(i)) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Version) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// One clause of a version spec.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Clause {
    Any,
    Exact(Version),
    Fuzzy(Version),
    NotFuzzy(Version),
    Op(Ordering, bool, Version), // (ordering, or-equal, version): >, >=, <, <=
    NotEqual(Version),
    Compatible(Version),
}

/// A version spec: `|`-separated alternatives of `,`-separated clauses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSpec {
    alternatives: Vec<Vec<Clause>>,
    text: String,
}

impl std::fmt::Display for VersionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl VersionSpec {
    pub fn any() -> VersionSpec {
        VersionSpec {
            alternatives: vec![vec![Clause::Any]],
            text: "*".to_owned(),
        }
    }

    pub fn parse(s: &str) -> Result<VersionSpec> {
        let text = s.trim().to_owned();
        if text.is_empty() || text == "*" {
            return Ok(VersionSpec::any());
        }
        let mut alternatives = Vec::new();
        for alt in text.split('|') {
            let mut clauses = Vec::new();
            for c in alt.split(',') {
                clauses.push(Clause::parse(c.trim())?);
            }
            alternatives.push(clauses);
        }
        Ok(VersionSpec { alternatives, text })
    }

    pub fn matches(&self, v: &Version) -> bool {
        self.alternatives
            .iter()
            .any(|clauses| clauses.iter().all(|c| c.matches(v)))
    }

    pub fn is_any(&self) -> bool {
        self.alternatives == vec![vec![Clause::Any]]
    }
}

impl Clause {
    fn parse(s: &str) -> Result<Clause> {
        if s.is_empty() || s == "*" {
            return Ok(Clause::Any);
        }
        let fuzzy = |v: &str| -> Result<Version> {
            let v = v
                .strip_suffix(".*")
                .or_else(|| v.strip_suffix('*'))
                .unwrap_or(v);
            Version::parse(v)
        };
        if let Some(v) = s.strip_prefix("==") {
            return Ok(if v.ends_with('*') {
                Clause::Fuzzy(fuzzy(v)?)
            } else {
                Clause::Exact(Version::parse(v)?)
            });
        }
        if let Some(v) = s.strip_prefix("!=") {
            return Ok(if v.ends_with('*') {
                Clause::NotFuzzy(fuzzy(v)?)
            } else {
                Clause::NotEqual(Version::parse(v)?)
            });
        }
        if let Some(v) = s.strip_prefix(">=") {
            return Ok(Clause::Op(Ordering::Greater, true, Version::parse(v)?));
        }
        if let Some(v) = s.strip_prefix("<=") {
            return Ok(Clause::Op(Ordering::Less, true, Version::parse(v)?));
        }
        if let Some(v) = s.strip_prefix("~=") {
            return Ok(Clause::Compatible(Version::parse(v)?));
        }
        if let Some(v) = s.strip_prefix('>') {
            return Ok(Clause::Op(Ordering::Greater, false, Version::parse(v)?));
        }
        if let Some(v) = s.strip_prefix('<') {
            return Ok(Clause::Op(Ordering::Less, false, Version::parse(v)?));
        }
        if let Some(v) = s.strip_prefix('=') {
            return Ok(Clause::Fuzzy(fuzzy(v)?));
        }
        if s.ends_with('*') {
            return Ok(Clause::Fuzzy(fuzzy(s)?));
        }
        Ok(Clause::Exact(Version::parse(s)?))
    }

    fn matches(&self, v: &Version) -> bool {
        match self {
            Clause::Any => true,
            Clause::Exact(x) => v == x,
            Clause::Fuzzy(x) => v.starts_with(x),
            Clause::NotFuzzy(x) => !v.starts_with(x),
            Clause::NotEqual(x) => v != x,
            Clause::Op(ord, or_equal, x) => {
                let c = v.cmp(x);
                c == *ord || (*or_equal && c == Ordering::Equal)
            }
            Clause::Compatible(x) => {
                // `~=1.2.3`: at least 1.2.3, within 1.2.*
                if v < x {
                    return false;
                }
                let mut prefix = x.clone();
                if prefix.parts.len() > 1 {
                    prefix.parts.pop();
                }
                v.starts_with(&prefix)
            }
        }
    }
}

/// `name [version [build]]`, as a `depends` entry spells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSpec {
    pub name: String,
    pub version: VersionSpec,
    /// A build string, with `*` wildcards.
    pub build: Option<String>,
}

impl std::fmt::Display for MatchSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if !self.version.is_any() || self.build.is_some() {
            write!(f, " {}", self.version)?;
        }
        if let Some(b) = &self.build {
            write!(f, " {b}")?;
        }
        Ok(())
    }
}

impl MatchSpec {
    pub fn parse(s: &str) -> Result<MatchSpec> {
        let s = s.trim();
        // A channel or subdir prefix (`conda-forge::name`) is not ours to honour.
        let s = s.rsplit("::").next().unwrap_or(s);
        // `name[version='>=1',build='*']` bracket form.
        if let Some((name, rest)) = s.split_once('[') {
            let inner = rest.trim_end_matches(']');
            let mut version = VersionSpec::any();
            let mut build = None;
            for kv in inner.split(',') {
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow!("bad match spec {s:?}"))?;
                let v = v.trim().trim_matches(['\'', '"']);
                match k.trim() {
                    "version" => version = VersionSpec::parse(v)?,
                    "build" => build = Some(v.to_owned()),
                    _ => {}
                }
            }
            return Ok(MatchSpec {
                name: name.trim().to_lowercase(),
                version,
                build,
            });
        }
        let mut words = s.split_whitespace();
        let name = words
            .next()
            .ok_or_else(|| anyhow!("empty match spec"))?
            .to_lowercase();
        // `name=1.2=build` and `name==1.2` forms.
        let (name, inline) = match name.split_once('=') {
            Some((n, v)) if !v.is_empty() => {
                (n.to_owned(), Some(v.trim_start_matches('=').to_owned()))
            }
            _ => (name, None),
        };
        let (version_text, build) = match inline {
            Some(v) => match v.split_once('=') {
                Some((ver, b)) => (Some(format!("={ver}")), Some(b.to_owned())),
                None => (Some(format!("={v}")), None),
            },
            None => (
                words.next().map(str::to_owned),
                words.next().map(str::to_owned),
            ),
        };
        if words.next().is_some() {
            bail!("too many fields in match spec {s:?}");
        }
        let version = match version_text {
            Some(v) => VersionSpec::parse(&v)?,
            None => VersionSpec::any(),
        };
        Ok(MatchSpec {
            name,
            version,
            build,
        })
    }

    pub fn matches(&self, r: &Record) -> bool {
        self.name == r.name
            && r.parsed_version()
                .map(|v| self.version.matches(&v))
                .unwrap_or(false)
            && self
                .build
                .as_deref()
                .is_none_or(|b| glob_match(b, &r.build))
    }
}

/// `*` matches any run of characters; everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => (0..=t.len()).any(|i| go(&p[1..], &t[i..])),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// A package as repodata describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub version: String,
    pub build: String,
    pub build_number: u64,
    pub depends: Vec<String>,
    pub constrains: Vec<String>,
    pub subdir: String,
    pub filename: String,
    pub url: String,
    pub sha256: Option<String>,
    pub md5: Option<String>,
    pub size: u64,
    pub license: Option<String>,
    /// `python` or `generic` for noarch packages.
    pub noarch: Option<String>,
    pub timestamp: u64,
    pub track_features: Vec<String>,
}

impl Record {
    pub fn parsed_version(&self) -> Result<Version> {
        Version::parse(&self.version)
    }

    /// Newer first: version, then build number, then timestamp; a package
    /// with track features sorts below one without (they mark alternatives).
    pub fn sort_key(&self) -> (bool, Version, u64, u64) {
        (
            self.track_features.is_empty(),
            self.parsed_version()
                .unwrap_or_else(|_| Version::parse("0").unwrap()),
            self.build_number,
            self.timestamp,
        )
    }

    /// Reads a record from a shard entry (`filename` → fields).
    pub fn from_msgpack(
        filename: &str,
        v: &crate::msgpack::Value,
        subdir: &str,
        base_url: &str,
    ) -> Option<Record> {
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_owned);
        let hex = |k: &str| {
            v.get(k).and_then(|x| match x {
                crate::msgpack::Value::Bin(b) => {
                    Some(b.iter().map(|b| format!("{b:02x}")).collect())
                }
                crate::msgpack::Value::Str(s) => Some(s.clone()),
                _ => None,
            })
        };
        Some(Record {
            name: text("name")?.to_lowercase(),
            version: text("version")?,
            build: text("build").unwrap_or_default(),
            build_number: v.get("build_number").and_then(|x| x.as_u64()).unwrap_or(0),
            depends: v.get("depends").map(|d| d.strings()).unwrap_or_default(),
            constrains: v.get("constrains").map(|d| d.strings()).unwrap_or_default(),
            subdir: text("subdir").unwrap_or_else(|| subdir.to_owned()),
            filename: filename.to_owned(),
            url: format!("{base_url}{filename}"),
            sha256: hex("sha256"),
            md5: hex("md5"),
            size: v.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            license: text("license"),
            noarch: text("noarch"),
            timestamp: v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0),
            track_features: text("track_features")
                .map(|t| t.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn orders_versions_like_conda() {
        // conda's documented ordering, in order.
        let ordered = [
            "0.4",
            "0.4.0",
            "0.4.1.rc",
            "0.4.1",
            "0.5a1",
            "0.5b3",
            "0.5C1",
            "0.5",
            "0.9.6",
            "0.960923",
            "1.0",
            "1.1dev1",
            "1.1_",
            "1.1a1",
            "1.1.0dev1",
            "1.1.a1",
            "1.1.0rc1",
            "1.1.0",
            "1.1.0post1",
            "1.1post1",
            "1996.07.12",
            "1!0.4.1",
            "1!3.1.1.6",
            "2!0.4.1",
        ];
        for pair in ordered.windows(2) {
            assert!(
                v(pair[0]) <= v(pair[1]),
                "{} should not sort above {}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(v("0.4"), v("0.4.0"));
        assert_eq!(v("0.4.1.rc"), v("0.4.1.RC"));
        assert_eq!(v("1.1.0dev1"), v("1.1.dev1"));
        assert_eq!(v("1.1.0"), v("1.1"));
        assert_eq!(v("1.1.0post1"), v("1.1.post1"));
        assert!(v("1.0+local.2") > v("1.0+local.1"));
        assert!(v("1.0.1") > v("1.0.1_") && v("1.0.1_") < v("1.0.1a"));
        assert!(Version::parse("").is_err());
        assert!(Version::parse("1.0 beta").is_err());
    }

    #[test]
    fn version_specs_and_match_specs() {
        let ok = |spec: &str, ver: &str| VersionSpec::parse(spec).unwrap().matches(&v(ver));
        assert!(ok(">=1.19,<2.0a0", "1.26.4"));
        assert!(!ok(">=1.19,<2.0a0", "2.0.0"));
        assert!(!ok(">=1.19,<2.0a0", "2.0.0a1"));
        assert!(ok("3.12.*", "3.12.7") && !ok("3.12.*", "3.13.0") && ok("3.12.*", "3.12"));
        assert!(ok("3.*", "3.12") && !ok("3.*", "30.1"));
        assert!(ok("=1.2", "1.2.5") && ok("==1.2", "1.2.0") && !ok("==1.2", "1.2.5"));
        assert!(ok("1.2", "1.2.0") && !ok("1.2", "1.2.1"));
        assert!(ok("1.0|2.0", "2.0") && !ok("1.0|2.0", "3.0"));
        assert!(ok("~=1.2.3", "1.2.9") && !ok("~=1.2.3", "1.3.0") && !ok("~=1.2.3", "1.2.2"));
        assert!(ok("!=1.2.*", "1.3") && !ok("!=1.2.*", "1.2.1"));
        assert!(ok("*", "anything") && ok("", "1"));

        let spec = MatchSpec::parse("python_abi 3.12.* *_cp312").unwrap();
        assert_eq!(spec.name, "python_abi");
        assert_eq!(spec.build.as_deref(), Some("*_cp312"));
        let rec = |name: &str, version: &str, build: &str| Record {
            name: name.into(),
            version: version.into(),
            build: build.into(),
            build_number: 0,
            depends: vec![],
            constrains: vec![],
            subdir: "noarch".into(),
            filename: String::new(),
            url: String::new(),
            sha256: None,
            md5: None,
            size: 0,
            license: None,
            noarch: None,
            timestamp: 0,
            track_features: vec![],
        };
        assert!(spec.matches(&rec("python_abi", "3.12", "7_cp312")));
        assert!(!spec.matches(&rec("python_abi", "3.12", "7_cp313")));
        assert!(!spec.matches(&rec("python_abi", "3.13", "7_cp312")));
        let bare = MatchSpec::parse("conda-forge::tzdata").unwrap();
        assert_eq!(bare.name, "tzdata");
        assert!(bare.version.is_any());
        let eq = MatchSpec::parse("numpy=1.26=py312h*").unwrap();
        assert!(eq.matches(&rec("numpy", "1.26.4", "py312hab2b1f5_0")));
        let eqeq = MatchSpec::parse("numpy==1.26.4").unwrap();
        assert!(
            eqeq.matches(&rec("numpy", "1.26.4", "x")) && !eqeq.matches(&rec("numpy", "1.26", "x"))
        );
        let bracket = MatchSpec::parse("libblas[version='>=3.9', build='*netlib']").unwrap();
        assert!(bracket.matches(&rec("libblas", "3.9.0", "20_netlib")));
        assert!(!bracket.matches(&rec("libblas", "3.9.0", "20_mkl")));
        assert_eq!(
            MatchSpec::parse("openssl >=3.3.1,<4.0a0")
                .unwrap()
                .to_string(),
            "openssl >=3.3.1,<4.0a0"
        );
        assert!(glob_match("*_cp312", "7_cp312") && !glob_match("py*_0", "py312_1"));
        assert!(MatchSpec::parse("a b c d").is_err());
    }
}
