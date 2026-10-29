//! PEP 440 versions and specifiers, PEP 508 requirements and markers: the
//! vocabulary a lockfile and an index speak.

use anyhow::{anyhow, bail, Result};
use std::cmp::Ordering;

/// A PEP 440 version: epoch!release[{a|b|rc}N][.postN][.devN][+local].
/// Equality follows ordering, so `1.0 == 1.0.0`.
#[derive(Debug, Clone)]
pub struct Version {
    pub epoch: u64,
    pub release: Vec<u64>,
    /// `a`=0, `b`=1, `rc`=2 with its number.
    pub pre: Option<(u8, u64)>,
    pub post: Option<u64>,
    pub dev: Option<u64>,
    pub local: Vec<String>,
}

impl Version {
    pub fn parse(s: &str) -> Result<Version> {
        let s = s.trim().trim_start_matches(['v', 'V']).to_lowercase();
        let (s, local) = match s.split_once('+') {
            Some((a, l)) => (
                a.to_owned(),
                l.split(['.', '-', '_']).map(str::to_owned).collect(),
            ),
            None => (s, Vec::new()),
        };
        let (epoch, rest) = match s.split_once('!') {
            Some((e, r)) => (
                e.parse().map_err(|_| anyhow!("bad epoch in version {s}"))?,
                r.to_owned(),
            ),
            None => (0, s.clone()),
        };
        let bytes = rest.as_bytes();
        let mut i = 0;
        let mut release = Vec::new();
        loop {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if start == i {
                bail!("invalid version {s:?}");
            }
            release.push(rest[start..i].parse().unwrap());
            if i < bytes.len()
                && bytes[i] == b'.'
                && i + 1 < bytes.len()
                && bytes[i + 1].is_ascii_digit()
            {
                i += 1;
            } else {
                break;
            }
        }
        let mut v = Version {
            epoch,
            release,
            pre: None,
            post: None,
            dev: None,
            local,
        };
        // Segments: separators . - _ are interchangeable and optional.
        let tail = &rest[i..];
        let mut t = tail.trim_start_matches(['.', '-', '_']);
        while !t.is_empty() {
            let word_end = t
                .find(|c: char| !c.is_ascii_alphabetic())
                .unwrap_or(t.len());
            let word = &t[..word_end];
            let after = &t[word_end..].trim_start_matches(['.', '-', '_']);
            let num_end = after
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(after.len());
            let num: u64 = if num_end == 0 {
                0
            } else {
                after[..num_end].parse().unwrap_or(0)
            };
            match word {
                "a" | "alpha" => v.pre = Some((0, num)),
                "b" | "beta" => v.pre = Some((1, num)),
                "rc" | "c" | "pre" | "preview" => v.pre = Some((2, num)),
                "post" | "rev" | "r" => v.post = Some(num),
                "dev" => v.dev = Some(num),
                "" if num_end > 0 => v.post = Some(num), // "1.0-1" implicit post
                _ => bail!("invalid version {s:?}"),
            }
            t = after[num_end..].trim_start_matches(['.', '-', '_']);
        }
        Ok(v)
    }

    pub fn is_prerelease(&self) -> bool {
        self.pre.is_some() || self.dev.is_some()
    }

    /// Release with trailing zeros removed, for comparisons.
    fn trimmed(&self) -> &[u64] {
        let mut end = self.release.len();
        while end > 1 && self.release[end - 1] == 0 {
            end -= 1;
        }
        &self.release[..end]
    }

    /// The public part (no local segment) as text.
    pub fn public(&self) -> String {
        let mut s = String::new();
        if self.epoch > 0 {
            s.push_str(&format!("{}!", self.epoch));
        }
        s.push_str(
            &self
                .release
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join("."),
        );
        if let Some((k, n)) = self.pre {
            s.push_str(&format!("{}{n}", ["a", "b", "rc"][k as usize]));
        }
        if let Some(n) = self.post {
            s.push_str(&format!(".post{n}"));
        }
        if let Some(n) = self.dev {
            s.push_str(&format!(".dev{n}"));
        }
        s
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.public())?;
        if !self.local.is_empty() {
            write!(f, "+{}", self.local.join("."))?;
        }
        Ok(())
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // PEP 440 ordering: epoch, release, then dev < pre < final < post, then local.
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| self.trimmed().cmp(other.trimmed()))
            .then_with(|| {
                let key = |v: &Version| {
                    // (pre-rank, pre-num) where a release with .dev only sorts lowest
                    match (v.pre, v.dev, v.post) {
                        (None, Some(_), None) => (0u8, 0u64),
                        (Some((k, n)), _, _) => (1 + k, n),
                        (None, _, None) => (4, 0),
                        (None, _, Some(_)) => (5, 0),
                    }
                };
                key(self).cmp(&key(other))
            })
            .then_with(|| self.post.cmp(&other.post))
            .then_with(|| match (self.dev, other.dev) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(&b),
            })
            .then_with(|| local_cmp(&self.local, &other.local))
    }
}

fn local_cmp(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(p), Ok(q)) => p.cmp(&q),
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// A PEP 440 version specifier set such as `>=1.2,<2,!=1.5.*`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Specifier {
    clauses: Vec<(String, String)>,
}

impl Specifier {
    pub fn parse(s: &str) -> Result<Specifier> {
        let mut clauses = Vec::new();
        for raw in s.split(',') {
            let c = raw.trim();
            if c.is_empty() {
                continue;
            }
            let op = ["===", "==", "!=", "<=", ">=", "~=", "<", ">"]
                .iter()
                .find(|op| c.starts_with(*op))
                .ok_or_else(|| anyhow!("invalid version specifier {c:?}"))?;
            let ver = c[op.len()..].trim().to_owned();
            if ver.is_empty()
                || (*op != "===" && Version::parse(ver.trim_end_matches(".*")).is_err())
            {
                bail!("invalid version specifier {c:?}");
            }
            clauses.push((op.to_string(), ver));
        }
        Ok(Specifier { clauses })
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }

    /// Whether `v` satisfies every clause. Pre-releases only match when the
    /// specifier mentions one, as pip and uv do.
    #[allow(dead_code)]
    pub fn matches(&self, v: &Version) -> bool {
        let mentions_pre = self.clauses.iter().any(|(_, s)| {
            Version::parse(s)
                .map(|x| x.is_prerelease())
                .unwrap_or(false)
        });
        if v.is_prerelease() && !mentions_pre {
            return false;
        }
        self.clauses.iter().all(|(op, s)| clause(op, s, v))
    }

    /// Like `matches`, but allows pre-releases unconditionally (when nothing
    /// else is available).
    pub fn matches_allowing_pre(&self, v: &Version) -> bool {
        self.clauses.iter().all(|(op, s)| clause(op, s, v))
    }
}

impl std::fmt::Display for Specifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self
            .clauses
            .iter()
            .map(|(o, v)| format!("{o}{v}"))
            .collect();
        write!(f, "{}", parts.join(","))
    }
}

fn clause(op: &str, spec: &str, v: &Version) -> bool {
    let wildcard = spec.ends_with(".*");
    let Ok(s) = Version::parse(spec.trim_end_matches(".*")) else {
        return false;
    };
    let prefix_match = || {
        let n = s.release.len();
        let mut pv = v.release.clone();
        pv.resize(n.max(pv.len()), 0);
        pv[..n] == s.release[..] && v.epoch == s.epoch
    };
    match op {
        "==" if wildcard => prefix_match(),
        "!=" if wildcard => !prefix_match(),
        "===" => v.to_string() == spec,
        "==" => {
            // A specifier without a local segment matches any local version.
            if s.local.is_empty() {
                let mut pv = v.clone();
                pv.local.clear();
                pv == s
            } else {
                v == &s
            }
        }
        "!=" => !clause("==", spec, v),
        ">=" => v >= &s,
        ">" => {
            v > &s
                && !(v.post.is_some()
                    && s.post.is_none()
                    && v.trimmed() == s.trimmed()
                    && v.pre == s.pre)
        }
        "<=" => v <= &s,
        "<" => v < &s && !(v.is_prerelease() && !s.is_prerelease() && v.trimmed() == s.trimmed()),
        "~=" => {
            if s.release.len() < 2 {
                return false;
            }
            let mut upper = s.clone();
            upper.release.pop();
            *upper.release.last_mut().unwrap() += 1;
            upper.pre = None;
            upper.post = None;
            upper.dev = None;
            upper.local.clear();
            v >= &s && v.trimmed() < upper.trimmed()
        }
        _ => false,
    }
}

/// A PEP 508 requirement: `name[extra1,extra2]>=1,<2; marker`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // the resolver's input type; constructed there
pub struct Requirement {
    pub name: String,
    pub extras: Vec<String>,
    pub specifier: Specifier,
    pub marker: Option<String>,
    /// `name @ https://…` direct references.
    pub url: Option<String>,
}

#[allow(dead_code)]
impl Requirement {
    pub fn parse(s: &str) -> Result<Requirement> {
        let (body, marker) = match s.split_once(';') {
            Some((b, m)) => (
                b.trim(),
                Some(m.trim().to_owned()).filter(|m| !m.is_empty()),
            ),
            None => (s.trim(), None),
        };
        let (body, url) = match body.split_once('@') {
            Some((b, u)) if !b.contains(['<', '>', '=', '!', '~']) => {
                (b.trim(), Some(u.trim().to_owned()))
            }
            _ => (body, None),
        };
        let name_end = body
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
            .unwrap_or(body.len());
        let name = crate::project::normalize(&body[..name_end]);
        if name.is_empty() {
            bail!("requirement {s:?} has no package name");
        }
        let mut rest = body[name_end..].trim();
        let mut extras = Vec::new();
        if let Some(r) = rest.strip_prefix('[') {
            let close = r
                .find(']')
                .ok_or_else(|| anyhow!("unclosed extras in {s:?}"))?;
            extras = r[..close]
                .split(',')
                .map(|e| crate::project::normalize(e.trim()))
                .filter(|e| !e.is_empty())
                .collect();
            rest = r[close + 1..].trim();
        }
        let rest = rest.trim_start_matches('(').trim_end_matches(')').trim();
        Ok(Requirement {
            name,
            extras,
            specifier: Specifier::parse(rest)?,
            marker,
            url,
        })
    }
}

/// The values markers are evaluated against.
#[derive(Debug, Clone, Default)]
pub struct MarkerEnv {
    pub python_version: String,
    pub python_full_version: String,
    pub sys_platform: String,
    pub platform_system: String,
    pub platform_machine: String,
    pub os_name: String,
    pub implementation_name: String,
    pub platform_python_implementation: String,
    pub extra: Vec<String>,
}

impl MarkerEnv {
    /// For a CPython of `version` on vivar's `os`/`arch` names.
    pub fn for_target(os: &str, arch: &str, version: [u32; 3]) -> MarkerEnv {
        let [a, b, c] = version;
        let (sys_platform, system, os_name) = match os {
            "linux" => ("linux", "Linux", "posix"),
            "macos" => ("darwin", "Darwin", "posix"),
            _ => ("win32", "Windows", "nt"),
        };
        let machine = match (os, arch) {
            ("windows", "x86_64") => "AMD64",
            ("windows", "aarch64") => "ARM64",
            ("macos", "aarch64") => "arm64",
            (_, a) => a,
        };
        MarkerEnv {
            python_version: format!("{a}.{b}"),
            python_full_version: format!("{a}.{b}.{c}"),
            sys_platform: sys_platform.to_owned(),
            platform_system: system.to_owned(),
            platform_machine: machine.to_owned(),
            os_name: os_name.to_owned(),
            implementation_name: "cpython".to_owned(),
            platform_python_implementation: "CPython".to_owned(),
            extra: Vec::new(),
        }
    }

    pub fn current(version: [u32; 3]) -> MarkerEnv {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH, version)
    }

    fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "python_version" => self.python_version.clone(),
            "python_full_version" => self.python_full_version.clone(),
            "sys_platform" => self.sys_platform.clone(),
            "platform_system" => self.platform_system.clone(),
            "platform_machine" => self.platform_machine.clone(),
            "os_name" => self.os_name.clone(),
            "implementation_name" => self.implementation_name.clone(),
            "platform_python_implementation" => self.platform_python_implementation.clone(),
            "platform_release" | "platform_version" | "implementation_version" => String::new(),
            _ => return None,
        })
    }
}

/// Evaluates a PEP 508 marker expression. Unknown or malformed markers are an error.
pub fn eval_marker(marker: &str, env: &MarkerEnv) -> Result<bool> {
    let tokens = tokenize(marker)?;
    let mut pos = 0;
    let v = or_expr(&tokens, &mut pos, env)?;
    if pos != tokens.len() {
        bail!("unexpected {:?} in marker {marker:?}", tokens[pos]);
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Op(String),
    LParen,
    RParen,
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Tok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Tok::RParen);
            i += 1;
        } else if c == '"' || c == '\'' {
            let end = chars[i + 1..]
                .iter()
                .position(|x| *x == c)
                .ok_or_else(|| anyhow!("unterminated string in marker {s:?}"))?;
            out.push(Tok::Str(chars[i + 1..i + 1 + end].iter().collect()));
            i += end + 2;
        } else if c.is_alphanumeric() || c == '_' || c == '.' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "and" | "or" | "in" | "not" => out.push(Tok::Op(word)),
                _ => out.push(Tok::Ident(word)),
            }
        } else {
            let start = i;
            while i < chars.len() && "<>=!~".contains(chars[i]) {
                i += 1;
            }
            if start == i {
                bail!("unexpected character {c:?} in marker {s:?}");
            }
            out.push(Tok::Op(chars[start..i].iter().collect()));
        }
    }
    // `not in` is one operator.
    let mut merged = Vec::with_capacity(out.len());
    let mut k = 0;
    while k < out.len() {
        if out[k] == Tok::Op("not".into()) && out.get(k + 1) == Some(&Tok::Op("in".into())) {
            merged.push(Tok::Op("not in".into()));
            k += 2;
        } else {
            merged.push(out[k].clone());
            k += 1;
        }
    }
    Ok(merged)
}

fn or_expr(t: &[Tok], pos: &mut usize, env: &MarkerEnv) -> Result<bool> {
    let mut v = and_expr(t, pos, env)?;
    while t.get(*pos) == Some(&Tok::Op("or".into())) {
        *pos += 1;
        v |= and_expr(t, pos, env)?;
    }
    Ok(v)
}

fn and_expr(t: &[Tok], pos: &mut usize, env: &MarkerEnv) -> Result<bool> {
    let mut v = atom(t, pos, env)?;
    while t.get(*pos) == Some(&Tok::Op("and".into())) {
        *pos += 1;
        v &= atom(t, pos, env)?;
    }
    Ok(v)
}

fn atom(t: &[Tok], pos: &mut usize, env: &MarkerEnv) -> Result<bool> {
    if t.get(*pos) == Some(&Tok::LParen) {
        *pos += 1;
        let v = or_expr(t, pos, env)?;
        if t.get(*pos) != Some(&Tok::RParen) {
            bail!("missing ) in marker");
        }
        *pos += 1;
        return Ok(v);
    }
    let lhs = t
        .get(*pos)
        .cloned()
        .ok_or_else(|| anyhow!("marker ended early"))?;
    let op = match t.get(*pos + 1) {
        Some(Tok::Op(o)) => o.clone(),
        _ => bail!("expected an operator in marker"),
    };
    let rhs = t
        .get(*pos + 2)
        .cloned()
        .ok_or_else(|| anyhow!("marker ended early"))?;
    *pos += 3;
    let value = |tok: &Tok| -> Result<(String, bool)> {
        match tok {
            Tok::Str(s) => Ok((s.clone(), false)),
            Tok::Ident(name) if name == "extra" => Ok((String::new(), true)),
            Tok::Ident(name) => env
                .get(name)
                .map(|v| (v, false))
                .ok_or_else(|| anyhow!("unknown marker variable {name}")),
            other => bail!("unexpected {other:?} in marker"),
        }
    };
    // `extra == "x"` is true when x is one of the requested extras.
    if matches!(&lhs, Tok::Ident(n) if n == "extra")
        || matches!(&rhs, Tok::Ident(n) if n == "extra")
    {
        let wanted = match (&lhs, &rhs) {
            (Tok::Str(s), _) | (_, Tok::Str(s)) => crate::project::normalize(s),
            _ => bail!("extra must be compared with a string"),
        };
        let has = env.extra.contains(&wanted);
        return Ok(match op.as_str() {
            "==" => has,
            "!=" => !has,
            _ => bail!("unsupported operator {op} for extra"),
        });
    }
    let (l, _) = value(&lhs)?;
    let (r, _) = value(&rhs)?;
    let version_keys = [
        "python_version",
        "python_full_version",
        "implementation_version",
    ];
    let is_version = matches!(&lhs, Tok::Ident(n) if version_keys.contains(&n.as_str()))
        || matches!(&rhs, Tok::Ident(n) if version_keys.contains(&n.as_str()));
    Ok(match op.as_str() {
        "in" => r.contains(&l),
        "not in" => !r.contains(&l),
        "===" => l == r,
        _ if is_version => {
            // Version comparison; the string side may be a specifier operand like "3.*".
            let (lv, rv) = (Version::parse(&l), Version::parse(r.trim_end_matches(".*")));
            match (lv, rv) {
                (Ok(lv), Ok(_)) => {
                    let spec = Specifier::parse(&format!("{op}{r}"))?;
                    spec.matches_allowing_pre(&lv)
                }
                _ => string_cmp(&op, &l, &r)?,
            }
        }
        _ => string_cmp(&op, &l, &r)?,
    })
}

fn string_cmp(op: &str, l: &str, r: &str) -> Result<bool> {
    Ok(match op {
        "==" => l == r,
        "!=" => l != r,
        "<" => l < r,
        "<=" => l <= r,
        ">" => l > r,
        ">=" => l >= r,
        _ => bail!("unsupported marker operator {op}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn orders_versions_per_pep440() {
        let order = [
            "1.0.dev1",
            "1.0a1",
            "1.0a2",
            "1.0b1",
            "1.0rc1",
            "1.0",
            "1.0.post1",
            "1.0.1",
            "1.1",
            "2!0.1",
        ];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("1.0"), v("1.0.0"));
        assert_eq!(v("1.0+local"), v("1.0+local"));
        assert!(v("1.0+local") > v("1.0"));
        assert!(v("2.6.0+cu124") > v("2.6.0+cpu"));
        assert_eq!(v("V1.2.3.post2.dev1").to_string(), "1.2.3.post2.dev1");
        assert_eq!(v("1.0-1").post, Some(1));
        assert!(Version::parse("latest").is_err());
    }

    #[test]
    fn specifiers_match_like_pip() {
        let s = Specifier::parse(">=1.2, <2, !=1.5.*").unwrap();
        assert!(s.matches(&v("1.2")));
        assert!(s.matches(&v("1.9.9")));
        assert!(!s.matches(&v("1.5.3")));
        assert!(!s.matches(&v("2.0")));
        assert!(!s.matches(&v("2.0a1")), "pre-release excluded by default");
        assert!(Specifier::parse(">=2.0a1").unwrap().matches(&v("2.0rc1")));
        assert!(Specifier::parse("~=1.4.2").unwrap().matches(&v("1.4.9")));
        assert!(!Specifier::parse("~=1.4.2").unwrap().matches(&v("1.5.0")));
        assert!(Specifier::parse("~=1.4").unwrap().matches(&v("1.9")));
        assert!(Specifier::parse("==1.4.*").unwrap().matches(&v("1.4.7")));
        assert!(Specifier::parse("==2.6.0")
            .unwrap()
            .matches(&v("2.6.0+cu124")));
        assert!(!Specifier::parse("==2.6.0+cpu")
            .unwrap()
            .matches(&v("2.6.0+cu124")));
        assert!(Specifier::parse("").unwrap().matches(&v("0.0.1")));
        assert!(Specifier::parse(">>1").is_err());
    }

    #[test]
    fn parses_requirements() {
        let r = Requirement::parse(
            "Requests[security, socks] >= 2.8.1, == 2.8.* ; python_version < \"2.7\"",
        )
        .unwrap();
        assert_eq!(r.name, "requests");
        assert_eq!(r.extras, vec!["security", "socks"]);
        assert_eq!(r.specifier.to_string(), ">=2.8.1,==2.8.*");
        assert_eq!(r.marker.as_deref(), Some("python_version < \"2.7\""));
        let r = Requirement::parse("pip @ https://github.com/pypa/pip/archive/1.3.1.zip").unwrap();
        assert_eq!(r.name, "pip");
        assert!(r.url.is_some());
        assert_eq!(
            Requirement::parse("numpy").unwrap().specifier,
            Specifier::default()
        );
        assert!(Requirement::parse("").is_err());
    }

    #[test]
    fn evaluates_markers() {
        let env = MarkerEnv::for_target("linux", "x86_64", [3, 12, 4]);
        let yes = |m: &str| eval_marker(m, &env).unwrap();
        assert!(yes("python_version >= '3.8'"));
        assert!(yes("python_full_version < '3.13'"));
        assert!(yes("python_version == '3.*'"));
        assert!(!yes("python_version < '3.8'"));
        assert!(yes(
            "sys_platform == 'linux' and platform_machine == 'x86_64'"
        ));
        assert!(yes("sys_platform == 'win32' or os_name == 'posix'"));
        assert!(!yes(
            "(sys_platform == 'win32' or sys_platform == 'darwin') and python_version < '3.13'"
        ));
        assert!(yes(
            "implementation_name == 'cpython' and platform_python_implementation != 'PyPy'"
        ));
        assert!(yes("'linux' in sys_platform"));
        assert!(yes("extra != 'gpu'"));
        let mut with_extra = env.clone();
        with_extra.extra = vec!["gpu".into()];
        assert!(eval_marker("extra == 'GPU'", &with_extra).unwrap());
        assert!(eval_marker("nonsense_var == '1'", &env).is_err());
        let win = MarkerEnv::for_target("windows", "x86_64", [3, 12, 0]);
        assert!(eval_marker(
            "sys_platform == 'win32' and platform_machine == 'AMD64'",
            &win
        )
        .unwrap());
        let mac = MarkerEnv::for_target("macos", "aarch64", [3, 13, 1]);
        assert!(eval_marker(
            "platform_system == 'Darwin' and platform_machine == 'arm64'",
            &mac
        )
        .unwrap());
    }
}
