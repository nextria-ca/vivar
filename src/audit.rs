//! `vivar audit`: known vulnerabilities for every locked package, from OSV.

use crate::json::{self, Value};
use crate::lock::Lock;
use anyhow::{anyhow, Context, Result};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

const OSV: &str = "https://api.osv.dev/v1/querybatch";

pub struct Finding {
    pub name: String,
    pub version: String,
    pub ids: Vec<String>,
}

/// Queries OSV (or `VIVAR_OSV_URL`, for a mirror) for the lock's packages.
pub fn run(root: &Path) -> Result<Vec<Finding>> {
    let lock = Lock::read(root)?;
    let packages: Vec<_> = lock
        .packages
        .iter()
        .filter(|p| p.source_kind == "registry")
        .collect();
    let url = env::var("VIVAR_OSV_URL").unwrap_or_else(|_| OSV.to_owned());
    let mut findings = Vec::new();
    for chunk in packages.chunks(500) {
        let queries: Vec<String> = chunk
            .iter()
            .map(|p| {
                format!(
                    r#"{{"package":{{"name":{},"ecosystem":"PyPI"}},"version":{}}}"#,
                    json::quote(&p.name),
                    json::quote(&p.version)
                )
            })
            .collect();
        let body = format!(r#"{{"queries":[{}]}}"#, queries.join(","));
        let response = post(&url, &body)?;
        let results = response
            .get("results")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("unexpected OSV response (no `results`)"))?;
        for (pkg, result) in chunk.iter().zip(results) {
            let ids: Vec<String> = result
                .get("vulns")
                .and_then(Value::as_array)
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.get("id").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            if !ids.is_empty() {
                findings.push(Finding {
                    name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    ids,
                });
            }
        }
    }
    Ok(findings)
}

fn post(url: &str, body: &str) -> Result<Value> {
    let tmp = env::temp_dir().join(format!("vivar-audit-{}.json", std::process::id()));
    fs::write(&tmp, body)?;
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sSf",
        "-X",
        "POST",
        "-H",
        "Content-Type: application/json",
        "--data-binary",
    ])
    .arg(format!("@{}", tmp.display()))
    .arg(url);
    if let Some(ca) = crate::upgrade::ca_bundle() {
        cmd.arg("--cacert").arg(ca);
    }
    let out = cmd
        .output()
        .context("running curl (needed to reach the vulnerability database)")?;
    let _ = fs::remove_file(&tmp);
    if !out.status.success() {
        anyhow::bail!(
            "querying {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    json::parse(&String::from_utf8_lossy(&out.stdout)).context("parsing the OSV response")
}
