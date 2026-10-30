//! A MessagePack reader, for conda's sharded repodata (CEP 16): every
//! format in the spec, extensions skipped.

use anyhow::{anyhow, bail, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Bin(Vec<u8>),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
}

impl Value {
    /// A map's value under a string key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(items) => items
                .iter()
                .find(|(k, _)| matches!(k, Value::Str(s) if s == key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::UInt(n) => Some(*n),
            Value::Int(n) if *n >= 0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bin(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    /// A string list, skipping anything that is not a string.
    pub fn strings(&self) -> Vec<String> {
        self.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Decodes one value from the start of `data`.
pub fn parse(data: &[u8]) -> Result<Value> {
    let mut r = Reader { data, pos: 0 };
    let v = r.value(0)?;
    Ok(v)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let s = self
            .data
            .get(self.pos..self.pos + n)
            .ok_or_else(|| anyhow!("truncated msgpack data"))?;
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes(b.try_into().unwrap()))
    }

    fn str(&mut self, n: usize) -> Result<Value> {
        let b = self.take(n)?;
        Ok(Value::Str(String::from_utf8_lossy(b).into_owned()))
    }

    fn bin(&mut self, n: usize) -> Result<Value> {
        Ok(Value::Bin(self.take(n)?.to_vec()))
    }

    fn array(&mut self, n: usize, depth: usize) -> Result<Value> {
        let mut out = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            out.push(self.value(depth + 1)?);
        }
        Ok(Value::Array(out))
    }

    fn map(&mut self, n: usize, depth: usize) -> Result<Value> {
        let mut out = Vec::with_capacity(n.min(1 << 16));
        for _ in 0..n {
            let k = self.value(depth + 1)?;
            let v = self.value(depth + 1)?;
            out.push((k, v));
        }
        Ok(Value::Map(out))
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > 64 {
            bail!("msgpack data nested too deeply");
        }
        let b = self.u8()?;
        Ok(match b {
            0x00..=0x7f => Value::UInt(b as u64),
            0x80..=0x8f => self.map((b & 0x0f) as usize, depth)?,
            0x90..=0x9f => self.array((b & 0x0f) as usize, depth)?,
            0xa0..=0xbf => self.str((b & 0x1f) as usize)?,
            0xc0 => Value::Nil,
            0xc1 => bail!("invalid msgpack byte 0xc1"),
            0xc2 => Value::Bool(false),
            0xc3 => Value::Bool(true),
            0xc4 => {
                let n = self.u8()? as usize;
                self.bin(n)?
            }
            0xc5 => {
                let n = self.u16()? as usize;
                self.bin(n)?
            }
            0xc6 => {
                let n = self.u32()? as usize;
                self.bin(n)?
            }
            0xc7 => {
                let n = self.u8()? as usize;
                self.take(1 + n)?;
                Value::Nil
            }
            0xc8 => {
                let n = self.u16()? as usize;
                self.take(1 + n)?;
                Value::Nil
            }
            0xc9 => {
                let n = self.u32()? as usize;
                self.take(1 + n)?;
                Value::Nil
            }
            0xca => Value::Float(f32::from_bits(self.u32()?) as f64),
            0xcb => Value::Float(f64::from_bits(self.u64()?)),
            0xcc => Value::UInt(self.u8()? as u64),
            0xcd => Value::UInt(self.u16()? as u64),
            0xce => Value::UInt(self.u32()? as u64),
            0xcf => Value::UInt(self.u64()?),
            0xd0 => Value::Int(self.u8()? as i8 as i64),
            0xd1 => Value::Int(self.u16()? as i16 as i64),
            0xd2 => Value::Int(self.u32()? as i32 as i64),
            0xd3 => Value::Int(self.u64()? as i64),
            0xd4..=0xd8 => {
                let n = 1usize << (b - 0xd4);
                self.take(1 + n)?;
                Value::Nil
            }
            0xd9 => {
                let n = self.u8()? as usize;
                self.str(n)?
            }
            0xda => {
                let n = self.u16()? as usize;
                self.str(n)?
            }
            0xdb => {
                let n = self.u32()? as usize;
                self.str(n)?
            }
            0xdc => {
                let n = self.u16()? as usize;
                self.array(n, depth)?
            }
            0xdd => {
                let n = self.u32()? as usize;
                self.array(n, depth)?
            }
            0xde => {
                let n = self.u16()? as usize;
                self.map(n, depth)?
            }
            0xdf => {
                let n = self.u32()? as usize;
                self.map(n, depth)?
            }
            0xe0..=0xff => Value::Int(b as i8 as i64),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_format() {
        // {"name": "tzdata", "n": 7, "neg": -3, "big": 70000, "f": 1.5,
        //  "bin": b"\x01\x02", "list": [true, nil], "ext": fixext1}
        let data: Vec<u8> = vec![
            0x88, 0xa4, b'n', b'a', b'm', b'e', 0xa6, b't', b'z', b'd', b'a', b't', b'a', 0xa1,
            b'n', 0x07, 0xa3, b'n', b'e', b'g', 0xfd, 0xa3, b'b', b'i', b'g', 0xce, 0x00, 0x01,
            0x11, 0x70, 0xa1, b'f', 0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0, 0xa3, b'b', b'i', b'n',
            0xc4, 0x02, 0x01, 0x02, 0xa4, b'l', b'i', b's', b't', 0x92, 0xc3, 0xc0, 0xa3, b'e',
            b'x', b't', 0xd4, 0x05, 0x09,
        ];
        let v = parse(&data).unwrap();
        assert_eq!(v.get("name").and_then(Value::as_str), Some("tzdata"));
        assert_eq!(v.get("n").and_then(Value::as_u64), Some(7));
        assert_eq!(v.get("neg"), Some(&Value::Int(-3)));
        assert_eq!(v.get("big").and_then(Value::as_u64), Some(70000));
        assert_eq!(v.get("f"), Some(&Value::Float(1.5)));
        assert_eq!(v.get("bin").and_then(Value::as_bytes), Some(&[1u8, 2][..]));
        assert_eq!(
            v.get("list").and_then(Value::as_array),
            Some(&[Value::Bool(true), Value::Nil][..])
        );
        assert_eq!(v.get("ext"), Some(&Value::Nil));
        assert!(parse(&[0x92, 0xc3]).is_err());
        assert!(parse(&[0xc1]).is_err());
        let strs = Value::Array(vec![Value::Str("a".into()), Value::UInt(1)]);
        assert_eq!(strs.strings(), vec!["a".to_owned()]);
    }
}
