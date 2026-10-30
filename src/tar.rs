//! A tar reader (ustar, pax extended headers, GNU long names) over bytes
//! held in memory, for the archives inside `.conda` packages.

use anyhow::{anyhow, bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Hardlink,
}

#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub name: String,
    pub kind: Kind,
    pub mode: u32,
    /// The file's bytes (empty for anything but a file).
    pub data: &'a [u8],
    /// The target of a symlink or hard link.
    pub link: String,
}

/// Every entry of the archive, in order.
pub fn entries(data: &[u8]) -> Result<Vec<Entry<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    // Overrides from pax / GNU headers for the entry that follows.
    let mut next_name: Option<String> = None;
    let mut next_link: Option<String> = None;
    let mut next_size: Option<u64> = None;
    while pos + 512 <= data.len() {
        let header = &data[pos..pos + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let field = |from: usize, len: usize| -> String {
            let raw = &header[from..from + len];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into_owned()
        };
        let size = match next_size.take() {
            Some(s) => s,
            None => number(&header[124..136])?,
        };
        let typeflag = header[156];
        let mut name = field(0, 100);
        if &header[257..262] == b"ustar" {
            let prefix = field(345, 155);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let body_start = pos + 512;
        let body_end = body_start + size as usize;
        let body = data
            .get(body_start..body_end)
            .ok_or_else(|| anyhow!("truncated tar entry {name}"))?;
        pos = body_start + (size as usize).div_ceil(512) * 512;
        match typeflag {
            b'x' => {
                // pax: `<len> key=value\n` records for the next entry.
                for (key, value) in pax_records(body) {
                    match key.as_str() {
                        "path" => next_name = Some(value),
                        "linkpath" => next_link = Some(value),
                        "size" => next_size = value.parse().ok(),
                        _ => {}
                    }
                }
                continue;
            }
            b'g' => continue, // global pax header: nothing we use
            b'L' => {
                next_name = Some(cstring(body));
                continue;
            }
            b'K' => {
                next_link = Some(cstring(body));
                continue;
            }
            _ => {}
        }
        let name = next_name.take().unwrap_or(name);
        let link = next_link.take().unwrap_or_else(|| field(157, 100));
        let kind = match typeflag {
            b'0' | 0 | b'7' => Kind::File,
            b'5' => Kind::Dir,
            b'2' => Kind::Symlink,
            b'1' => Kind::Hardlink,
            other => bail!(
                "tar entry {name} has an unsupported type {:?}",
                other as char
            ),
        };
        let mode = u32::from_str_radix(field(100, 8).trim(), 8).unwrap_or(0o644);
        let name = name.trim_end_matches('/').to_owned();
        out.push(Entry {
            name,
            kind: if typeflag == 0 && !link.is_empty() {
                Kind::File
            } else {
                kind
            },
            mode,
            data: if typeflag == b'5' { &[] } else { body },
            link,
        });
    }
    Ok(out)
}

/// A size field: octal text, or base-256 when the high bit is set (GNU).
fn number(raw: &[u8]) -> Result<u64> {
    if raw[0] & 0x80 != 0 {
        let mut v: u64 = 0;
        for &b in &raw[1..] {
            v = (v << 8) | b as u64;
        }
        return Ok(v);
    }
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| anyhow!("bad size field in tar header: {text:?}"))
}

fn cstring(body: &[u8]) -> String {
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
}

fn pax_records(body: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < body.len() {
        let Some(space) = body[pos..].iter().position(|&b| b == b' ') else {
            break;
        };
        let Ok(len) = String::from_utf8_lossy(&body[pos..pos + space]).parse::<usize>() else {
            break;
        };
        if len == 0 || pos + len > body.len() {
            break;
        }
        let record = &body[pos + space + 1..pos + len];
        let record = String::from_utf8_lossy(record);
        let record = record.trim_end_matches('\n');
        if let Some((k, v)) = record.split_once('=') {
            out.push((k.to_owned(), v.to_owned()));
        }
        pos += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(name: &str, size: usize, typeflag: u8, link: &str, mode: &str) -> Vec<u8> {
        let mut h = vec![0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..100 + mode.len()].copy_from_slice(mode.as_bytes());
        let size = format!("{size:011o}\0");
        h[124..136].copy_from_slice(size.as_bytes());
        h[156] = typeflag;
        h[157..157 + link.len()].copy_from_slice(link.as_bytes());
        h[257..262].copy_from_slice(b"ustar");
        h
    }

    fn entry(name: &str, body: &[u8], typeflag: u8, link: &str, mode: &str) -> Vec<u8> {
        let mut out = header(name, body.len(), typeflag, link, mode);
        out.extend_from_slice(body);
        out.resize(512 + body.len().div_ceil(512) * 512, 0);
        out
    }

    #[test]
    fn reads_files_links_and_long_names() {
        let mut tar = Vec::new();
        tar.extend(entry("info/index.json", b"{}", b'0', "", "0000644"));
        tar.extend(entry("bin/", b"", b'5', "", "0000755"));
        tar.extend(entry("bin/tool", b"#!/bin/sh\n", b'0', "", "0000755"));
        tar.extend(entry("bin/alias", b"", b'2', "tool", "0000777"));
        tar.extend(entry("bin/same", b"", b'1', "bin/tool", "0000644"));
        let long = "lib/".to_owned() + &"x".repeat(120) + "/deep.txt";
        // A pax record's length counts its own digits and the space.
        let record = format!("path={long}\n");
        let mut len = record.len() + 1;
        while len.to_string().len() + 1 + record.len() != len {
            len = len.to_string().len() + 1 + record.len();
        }
        let pax = format!("{len} {record}");
        tar.extend(entry("./PaxHeaders/0", pax.as_bytes(), b'x', "", "0000644"));
        tar.extend(entry("truncated", b"deep", b'0', "", "0000644"));
        tar.extend(entry(
            "././@LongLink",
            b"lib/gnu-long-name\0",
            b'L',
            "",
            "0000644",
        ));
        tar.extend(entry("short", b"gnu", b'0', "", "0000644"));
        tar.extend(vec![0u8; 1024]);
        let list = entries(&tar).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "info/index.json",
                "bin",
                "bin/tool",
                "bin/alias",
                "bin/same",
                long.as_str(),
                "lib/gnu-long-name"
            ]
        );
        assert_eq!(list[0].data, b"{}");
        assert_eq!(list[1].kind, Kind::Dir);
        assert_eq!(list[2].mode, 0o755);
        assert_eq!(list[3].kind, Kind::Symlink);
        assert_eq!(list[3].link, "tool");
        assert_eq!(list[4].kind, Kind::Hardlink);
        assert_eq!(list[4].link, "bin/tool");
        assert_eq!(list[5].data, b"deep");
        assert_eq!(list[6].data, b"gnu");
        assert_eq!(number(b"00000000012\0").unwrap(), 10);
        assert_eq!(
            number(&[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]).unwrap(),
            256
        );
    }
}
