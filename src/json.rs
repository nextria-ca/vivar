//! A small JSON reader for the few API responses vivar consumes (GitHub
//! releases, OSV). Parsing only; no serialization beyond what callers format.

use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(map) => map.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }
}

pub fn parse(text: &str) -> Result<Value> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = p.value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        bail!("trailing data in JSON at byte {}", p.pos);
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            bail!("expected {:?} at byte {} of JSON", b as char, self.pos)
        }
    }

    fn value(&mut self) -> Result<Value> {
        self.skip_ws();
        match self
            .peek()
            .ok_or_else(|| anyhow!("unexpected end of JSON"))?
        {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::String(self.string()?)),
            b't' => self.literal("true", Value::Bool(true)),
            b'f' => self.literal("false", Value::Bool(false)),
            b'n' => self.literal("null", Value::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            bail!("invalid JSON literal at byte {}", self.pos)
        }
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_digit() || b"+-.eE".contains(&b))
        {
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Value::Number)
            .ok_or_else(|| anyhow!("invalid JSON number at byte {start}"))
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = Vec::new();
        loop {
            let b = self
                .peek()
                .ok_or_else(|| anyhow!("unterminated JSON string"))?;
            self.pos += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let e = self
                        .peek()
                        .ok_or_else(|| anyhow!("unterminated JSON escape"))?;
                    self.pos += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code)
                                && self.bytes[self.pos..].starts_with(b"\\u")
                            {
                                self.pos += 2;
                                let low = self.hex4()?;
                                code = 0x10000
                                    + ((code - 0xD800) << 10)
                                    + (low.wrapping_sub(0xDC00) & 0x3FF);
                            }
                            let ch = char::from_u32(code).unwrap_or('\u{FFFD}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => bail!("invalid JSON escape at byte {}", self.pos),
                    }
                }
                _ => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| anyhow!("invalid UTF-8 in JSON string"))
    }

    fn hex4(&mut self) -> Result<u32> {
        let end = self.pos + 4;
        let hex = self
            .bytes
            .get(self.pos..end)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| anyhow!("invalid \\u escape at byte {}", self.pos))?;
        self.pos = end;
        Ok(hex)
    }

    fn array(&mut self) -> Result<Value> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => bail!("expected , or ] at byte {} of JSON", self.pos),
            }
        }
    }

    fn object(&mut self) -> Result<Value> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.value()?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => bail!("expected , or }} at byte {} of JSON", self.pos),
            }
        }
    }
}

/// Quotes a string for embedding in a JSON document.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_documents() {
        let v = parse(r#"{"results":[{"vulns":[{"id":"GHSA-1","n":1.5,"ok":true,"x":null}]},{}], "s":"a\"bé😀"}"#).unwrap();
        let first = &v.get("results").unwrap().as_array().unwrap()[0];
        let vuln = &first.get("vulns").unwrap().as_array().unwrap()[0];
        assert_eq!(vuln.get("id").unwrap().as_str(), Some("GHSA-1"));
        assert_eq!(vuln.get("n"), Some(&Value::Number(1.5)));
        assert_eq!(vuln.get("ok"), Some(&Value::Bool(true)));
        assert_eq!(vuln.get("x"), Some(&Value::Null));
        assert_eq!(v.get("s").unwrap().as_str(), Some("a\"bé😀"));
        assert!(parse("[1,]").is_err());
        assert!(parse("{} x").is_err());
        assert_eq!(quote("a\"b\n"), r#""a\"b\n""#);
    }
}
