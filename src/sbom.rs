//! `vivar export sbom`: a software bill of materials from the lockfile, in
//! CycloneDX 1.5 or SPDX 2.3 JSON. Licenses come from the installed metadata
//! when the environment exists.

use crate::json::quote;
use crate::licenses;
use crate::lock::Lock;
use crate::project::Project;
use anyhow::Result;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    CycloneDx,
    Spdx,
}

pub fn generate(project: &Project, format: Format) -> Result<String> {
    let root = project.venv_root();
    let lock = Lock::read(root)?;
    let licenses: BTreeMap<String, String> = licenses::inventory(&root.join(".venv"))
        .map(|dists| {
            dists
                .into_iter()
                .map(|d| (crate::project::normalize(&d.name), d.license))
                .collect()
        })
        .unwrap_or_default();
    let now = timestamp();
    let tool = format!("vivar {}", env!("CARGO_PKG_VERSION"));
    let name = project.name();
    let packages: Vec<&crate::lock::Package> = lock
        .packages
        .iter()
        .filter(|p| p.source_kind != "virtual" && p.name != name)
        .collect();

    Ok(match format {
        Format::CycloneDx => {
            let components: Vec<String> = packages
                .iter()
                .map(|p| {
                    let purl = format!("pkg:pypi/{}@{}", p.name, p.version);
                    let license = licenses
                        .get(&crate::project::normalize(&p.name))
                        .filter(|l| !l.is_empty())
                        .map(|l| format!(",\"licenses\":[{{\"expression\":{}}}]", quote(l)))
                        .unwrap_or_default();
                    format!(
                        "{{\"type\":\"library\",\"bom-ref\":{purl_q},\"name\":{},\"version\":{},\"purl\":{purl_q}{license}}}",
                        quote(&p.name),
                        quote(&p.version),
                        purl_q = quote(&purl)
                    )
                })
                .collect();
            format!(
                "{{\"bomFormat\":\"CycloneDX\",\"specVersion\":\"1.5\",\"version\":1,\"metadata\":{{\"timestamp\":{},\"tools\":[{{\"name\":\"vivar\",\"version\":{}}}],\"component\":{{\"type\":\"application\",\"name\":{}}}}},\"components\":[{}]}}\n",
                quote(&now),
                quote(env!("CARGO_PKG_VERSION")),
                quote(&name),
                components.join(",")
            )
        }
        Format::Spdx => {
            let mut pkgs = Vec::new();
            let mut rels = Vec::new();
            for p in &packages {
                // Forked locks carry several versions of one package: the id needs both.
                let id = format!(
                    "SPDXRef-Package-{}-{}",
                    p.name.replace(['.', '_'], "-"),
                    p.version.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
                );
                let purl = format!("pkg:pypi/{}@{}", p.name, p.version);
                let declared = licenses
                    .get(&crate::project::normalize(&p.name))
                    .filter(|l| !l.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "NOASSERTION".to_owned());
                let download = if p.source_kind == "registry" {
                    p.sdist
                        .as_ref()
                        .or(p.wheels.first())
                        .map(|a| a.url.clone())
                        .unwrap_or_else(|| "NOASSERTION".to_owned())
                } else {
                    "NOASSERTION".to_owned()
                };
                pkgs.push(format!(
                    "{{\"SPDXID\":{},\"name\":{},\"versionInfo\":{},\"downloadLocation\":{},\"filesAnalyzed\":false,\"licenseConcluded\":\"NOASSERTION\",\"licenseDeclared\":{},\"externalRefs\":[{{\"referenceCategory\":\"PACKAGE-MANAGER\",\"referenceType\":\"purl\",\"referenceLocator\":{}}}]}}",
                    quote(&id),
                    quote(&p.name),
                    quote(&p.version),
                    quote(&download),
                    quote(&declared),
                    quote(&purl)
                ));
                rels.push(format!(
                    "{{\"spdxElementId\":\"SPDXRef-DOCUMENT\",\"relatedSpdxElement\":{},\"relationshipType\":\"DESCRIBES\"}}",
                    quote(&id)
                ));
            }
            format!(
                "{{\"spdxVersion\":\"SPDX-2.3\",\"dataLicense\":\"CC0-1.0\",\"SPDXID\":\"SPDXRef-DOCUMENT\",\"name\":{},\"documentNamespace\":{},\"creationInfo\":{{\"created\":{},\"creators\":[{}]}},\"packages\":[{}],\"relationships\":[{}]}}\n",
                quote(&name),
                quote(&format!("https://vivar.local/sbom/{name}/{}", now.replace(':', "-"))),
                quote(&now),
                quote(&format!("Tool: {tool}")),
                pkgs.join(","),
                rels.join(",")
            )
        }
    })
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`, no crate needed.
pub fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_iso8601() {
        let t = timestamp();
        assert_eq!(t.len(), 20, "{t}");
        assert!(
            t.starts_with("20") && t.ends_with('Z') && &t[10..11] == "T",
            "{t}"
        );
    }
}
