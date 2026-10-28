//! License inventory and allowlist enforcement from installed package metadata.

use anyhow::{bail, Result};
use std::fs;
use std::path::Path;

pub struct Dist {
    pub name: String,
    pub version: String,
    /// An SPDX expression when the package declares one, a classifier-derived
    /// identifier otherwise, or empty when nothing usable was declared.
    pub license: String,
}

/// Reads `*.dist-info/METADATA` in the environment's site-packages.
pub fn inventory(venv: &Path) -> Result<Vec<Dist>> {
    let site = crate::venv::site_packages(venv)?;
    let mut dists = Vec::new();
    for entry in fs::read_dir(&site)?.flatten() {
        let path = entry.path();
        let Some(dir) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !dir.ends_with(".dist-info") {
            continue;
        }
        let Ok(metadata) = fs::read_to_string(path.join("METADATA")) else {
            continue;
        };
        let header = |key: &str| {
            metadata
                .lines()
                .take_while(|l| !l.is_empty())
                .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(':')))
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let name = header("Name").unwrap_or_else(|| dir.trim_end_matches(".dist-info").to_owned());
        let version = header("Version").unwrap_or_default();
        dists.push(Dist {
            name,
            version,
            license: license_of(&metadata),
        });
    }
    dists.sort_by_key(|d| d.name.to_lowercase());
    Ok(dists)
}

/// Best available license statement from a METADATA document.
fn license_of(metadata: &str) -> String {
    let headers = metadata.lines().take_while(|l| !l.is_empty());
    let mut expression = None;
    let mut license_field = None;
    let mut classifiers = Vec::new();
    for line in headers {
        if let Some(v) = line.strip_prefix("License-Expression:") {
            expression = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("License:") {
            license_field = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Classifier: License ::") {
            if let Some(id) = classifier_id(v.trim()) {
                classifiers.push(id);
            }
        }
    }
    if let Some(e) = expression.filter(|e| !e.is_empty()) {
        return e;
    }
    if !classifiers.is_empty() {
        return classifiers.join(" OR ");
    }
    match license_field {
        // Full license texts get pasted into this field; those are useless here.
        Some(f) if !f.is_empty() && f.len() <= 64 && !f.eq_ignore_ascii_case("unknown") => f,
        _ => String::new(),
    }
}

/// Trove classifier → SPDX-style identifier.
fn classifier_id(classifier: &str) -> Option<&'static str> {
    let name = classifier.rsplit("::").next()?.trim();
    Some(match name {
        "MIT License" => "MIT",
        "Apache Software License" => "Apache-2.0",
        "BSD License" => "BSD-3-Clause",
        "ISC License (ISCL)" => "ISC",
        "Python Software Foundation License" => "PSF-2.0",
        "Mozilla Public License 2.0 (MPL 2.0)" => "MPL-2.0",
        "The Unlicense (Unlicense)" => "Unlicense",
        "zlib/libpng License" => "Zlib",
        "GNU General Public License v2 (GPLv2)" => "GPL-2.0-only",
        "GNU General Public License v2 or later (GPLv2+)" => "GPL-2.0-or-later",
        "GNU General Public License v3 (GPLv3)" => "GPL-3.0-only",
        "GNU General Public License v3 or later (GPLv3+)" => "GPL-3.0-or-later",
        "GNU Lesser General Public License v2 (LGPLv2)" => "LGPL-2.0-only",
        "GNU Lesser General Public License v2 or later (LGPLv2+)" => "LGPL-2.0-or-later",
        "GNU Lesser General Public License v3 (LGPLv3)" => "LGPL-3.0-only",
        "GNU Lesser General Public License v3 or later (LGPLv3+)" => "LGPL-3.0-or-later",
        "GNU Affero General Public License v3" => "AGPL-3.0-only",
        "Eclipse Public License 2.0 (EPL-2.0)" => "EPL-2.0",
        "Public Domain" => "Public-Domain",
        "OSI Approved" => return None,
        _ => return None,
    })
}

/// Whether an SPDX expression is satisfied by the allowlist: every AND
/// operand allowed, any OR operand allowed, `X WITH y` judged by `X`.
pub fn allowed(expression: &str, allow: &[String]) -> bool {
    let tokens: Vec<String> = expression
        .replace('(', " ( ")
        .replace(')', " ) ")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if tokens.is_empty() {
        return false;
    }
    let mut pos = 0;
    let ok = or_expr(&tokens, &mut pos, allow);
    ok && pos == tokens.len()
}

fn or_expr(t: &[String], pos: &mut usize, allow: &[String]) -> bool {
    let mut ok = and_expr(t, pos, allow);
    while t.get(*pos).is_some_and(|x| x.eq_ignore_ascii_case("OR")) {
        *pos += 1;
        ok |= and_expr(t, pos, allow);
    }
    ok
}

fn and_expr(t: &[String], pos: &mut usize, allow: &[String]) -> bool {
    let mut ok = atom(t, pos, allow);
    while t.get(*pos).is_some_and(|x| x.eq_ignore_ascii_case("AND")) {
        *pos += 1;
        ok &= atom(t, pos, allow);
    }
    ok
}

fn atom(t: &[String], pos: &mut usize, allow: &[String]) -> bool {
    let Some(tok) = t.get(*pos) else {
        return false;
    };
    *pos += 1;
    if tok == "(" {
        let ok = or_expr(t, pos, allow);
        if t.get(*pos).is_some_and(|x| x == ")") {
            *pos += 1;
        }
        return ok;
    }
    if t.get(*pos).is_some_and(|x| x.eq_ignore_ascii_case("WITH")) {
        *pos += 2;
    }
    allow.iter().any(|a| a.eq_ignore_ascii_case(tok))
}

/// Packages whose license is not allowed (or not declared). Exceptions are
/// normalized package names.
pub fn violations<'a>(dists: &'a [Dist], allow: &[String], exceptions: &[String]) -> Vec<&'a Dist> {
    dists
        .iter()
        .filter(|d| !exceptions.contains(&crate::project::normalize(&d.name)))
        .filter(|d| !allowed(&d.license, allow))
        .collect()
}

pub fn check(venv: &Path, allow: Option<&[String]>, exceptions: &[String]) -> Result<()> {
    let Some(allow) = allow else {
        return Ok(());
    };
    let dists = inventory(venv)?;
    let bad = violations(&dists, allow, exceptions);
    if bad.is_empty() {
        return Ok(());
    }
    let lines: Vec<String> = bad
        .iter()
        .map(|d| {
            format!(
                "  {} {}: {}",
                d.name,
                d.version,
                if d.license.is_empty() {
                    "no license declared"
                } else {
                    &d.license
                }
            )
        })
        .collect();
    bail!(
        "licenses outside [tool.vivar.policy] allow-licenses {:?}:\n{}\n(add a package to `license-exceptions` to accept it anyway)",
        allow,
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn evaluates_spdx_expressions() {
        let a = allow(&["MIT", "Apache-2.0", "BSD-3-Clause"]);
        assert!(allowed("MIT", &a));
        assert!(allowed("mit", &a));
        assert!(allowed("Apache-2.0 OR GPL-3.0-only", &a));
        assert!(!allowed("Apache-2.0 AND GPL-3.0-only", &a));
        assert!(allowed("(MIT OR GPL-2.0-only) AND BSD-3-Clause", &a));
        assert!(allowed("Apache-2.0 WITH LLVM-exception", &a));
        assert!(!allowed("", &a));
        assert!(!allowed("GPL-3.0-only", &a));
    }

    #[test]
    fn picks_the_best_license_statement() {
        assert_eq!(
            license_of("Name: a\nLicense-Expression: MIT AND Zlib\nLicense: blah\n\nbody"),
            "MIT AND Zlib"
        );
        assert_eq!(
            license_of("Name: a\nClassifier: License :: OSI Approved :: BSD License\nClassifier: License :: OSI Approved :: MIT License\n"),
            "BSD-3-Clause OR MIT"
        );
        assert_eq!(license_of("Name: a\nLicense: Apache-2.0\n"), "Apache-2.0");
        assert_eq!(license_of("Name: a\nLicense: UNKNOWN\n"), "");
        let long = format!("Name: a\nLicense: {}\n", "x".repeat(100));
        assert_eq!(license_of(&long), "");
        // Headers end at the first blank line; the body must not be scanned.
        assert_eq!(license_of("Name: a\n\nLicense-Expression: MIT\n"), "");
    }

    #[test]
    fn reports_violations_minus_exceptions() {
        let dists = vec![
            Dist {
                name: "Good_Pkg".into(),
                version: "1".into(),
                license: "MIT".into(),
            },
            Dist {
                name: "bad".into(),
                version: "2".into(),
                license: "GPL-3.0-only".into(),
            },
            Dist {
                name: "tolerated".into(),
                version: "3".into(),
                license: String::new(),
            },
        ];
        let bad = violations(&dists, &allow(&["MIT"]), &allow(&["tolerated"]));
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].name, "bad");
    }
}
