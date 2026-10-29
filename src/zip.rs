//! Just enough zip to read wheels: the central directory, stored and
//! deflated entries (RFC 1951 inflate written out in full), Unix modes.

use anyhow::{anyhow, bail, Result};

/// One entry of a zip archive.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    /// Unix permission bits when the archive recorded them (0 otherwise).
    pub mode: u32,
    method: u16,
    compressed: u64,
    local_header: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }
}

/// Lists the entries of a zip held in memory.
pub fn entries(data: &[u8]) -> Result<Vec<Entry>> {
    // End of central directory: scan back for its signature (a comment may follow).
    let min = data.len().saturating_sub(22 + 65_535);
    let eocd = (min..=data.len().saturating_sub(22))
        .rev()
        .find(|&i| data[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or_else(|| anyhow!("not a zip file (no end-of-central-directory record)"))?;
    let count = u16le(data, eocd + 10)? as usize;
    let mut pos = u32le(data, eocd + 16)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if !data[pos..].starts_with(&[0x50, 0x4b, 0x01, 0x02]) {
            bail!("corrupt zip central directory");
        }
        let method = u16le(data, pos + 10)?;
        let compressed = u32le(data, pos + 20)? as u64;
        let size = u32le(data, pos + 24)? as u64;
        let name_len = u16le(data, pos + 28)? as usize;
        let extra_len = u16le(data, pos + 30)? as usize;
        let comment_len = u16le(data, pos + 32)? as usize;
        let external = u32le(data, pos + 38)?;
        let local_header = u32le(data, pos + 42)? as u64;
        let name_bytes = data
            .get(pos + 46..pos + 46 + name_len)
            .ok_or_else(|| anyhow!("corrupt zip entry name"))?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        if compressed == 0xffff_ffff || size == 0xffff_ffff {
            bail!("zip64 archives are not supported ({name})");
        }
        out.push(Entry {
            name,
            size,
            mode: external >> 16,
            method,
            compressed,
            local_header,
        });
        pos += 46 + name_len + extra_len + comment_len;
    }
    Ok(out)
}

/// The decompressed bytes of an entry.
pub fn read(data: &[u8], entry: &Entry) -> Result<Vec<u8>> {
    let h = entry.local_header as usize;
    if !data
        .get(h..h + 4)
        .is_some_and(|s| s == [0x50, 0x4b, 0x03, 0x04])
    {
        bail!("corrupt zip local header for {}", entry.name);
    }
    let name_len = u16le(data, h + 26)? as usize;
    let extra_len = u16le(data, h + 28)? as usize;
    let start = h + 30 + name_len + extra_len;
    let raw = data
        .get(start..start + entry.compressed as usize)
        .ok_or_else(|| anyhow!("truncated zip data for {}", entry.name))?;
    let out = match entry.method {
        0 => raw.to_vec(),
        8 => inflate(raw, entry.size as usize)?,
        m => bail!("{}: unsupported zip compression method {m}", entry.name),
    };
    if out.len() as u64 != entry.size {
        bail!("{}: size mismatch after decompression", entry.name);
    }
    Ok(out)
}

fn u16le(d: &[u8], at: usize) -> Result<u16> {
    d.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| anyhow!("truncated zip"))
}

fn u32le(d: &[u8], at: usize) -> Result<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| anyhow!("truncated zip"))
}

// ---------------------------------------------------------------------------
// Inflate (RFC 1951)

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32> {
        let byte = *self
            .data
            .get(self.pos)
            .ok_or_else(|| anyhow!("deflate stream ended early"))?;
        let b = (byte >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(b as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// A canonical Huffman decoder: counts per length and symbols in order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for i in 1..16 {
            offsets[i] = offsets[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= bits.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        bail!("invalid Huffman code in deflate stream")
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Decompresses a raw deflate stream (no zlib or gzip header).
pub fn inflate(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(expected);
    let mut bits = Bits {
        data,
        pos: 0,
        bit: 0,
    };
    loop {
        let last = bits.bit()? == 1;
        match bits.bits(2)? {
            0 => {
                bits.align();
                let len = u16le(data, bits.pos)? as usize;
                let nlen = u16le(data, bits.pos + 2)? as usize;
                if len != (!nlen & 0xffff) {
                    bail!("corrupt stored block in deflate stream");
                }
                let start = bits.pos + 4;
                out.extend_from_slice(
                    data.get(start..start + len)
                        .ok_or_else(|| anyhow!("deflate stream ended early"))?,
                );
                bits.pos = start + len;
            }
            1 => {
                let mut lengths = [0u8; 288];
                lengths[..144].fill(8);
                lengths[144..256].fill(9);
                lengths[256..280].fill(7);
                lengths[280..].fill(8);
                let lit = Huffman::new(&lengths);
                let dist = Huffman::new(&[5u8; 30]);
                codes(&mut bits, &mut out, &lit, &dist)?;
            }
            2 => {
                let hlit = bits.bits(5)? as usize + 257;
                let hdist = bits.bits(5)? as usize + 1;
                let hclen = bits.bits(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut cl = [0u8; 19];
                for &i in ORDER.iter().take(hclen) {
                    cl[i] = bits.bits(3)? as u8;
                }
                let cl_code = Huffman::new(&cl);
                let mut lengths = vec![0u8; hlit + hdist];
                let mut i = 0;
                while i < hlit + hdist {
                    let sym = cl_code.decode(&mut bits)?;
                    let (value, repeat) = match sym {
                        0..=15 => (sym as u8, 1),
                        16 => {
                            let prev = *lengths
                                .get(i.wrapping_sub(1))
                                .ok_or_else(|| anyhow!("corrupt code lengths"))?;
                            (prev, 3 + bits.bits(2)? as usize)
                        }
                        17 => (0, 3 + bits.bits(3)? as usize),
                        _ => (0, 11 + bits.bits(7)? as usize),
                    };
                    if i + repeat > lengths.len() {
                        bail!("corrupt code lengths in deflate stream");
                    }
                    lengths[i..i + repeat].fill(value);
                    i += repeat;
                }
                let lit = Huffman::new(&lengths[..hlit]);
                let dist = Huffman::new(&lengths[hlit..]);
                codes(&mut bits, &mut out, &lit, &dist)?;
            }
            _ => bail!("invalid deflate block type"),
        }
        if last {
            break;
        }
    }
    Ok(out)
}

fn codes(bits: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman) -> Result<()> {
    loop {
        let sym = lit.decode(bits)? as usize;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            _ => {
                let i = sym - 257;
                if i >= 29 {
                    bail!("invalid length code in deflate stream");
                }
                let len = LEN_BASE[i] as usize + bits.bits(LEN_EXTRA[i] as u32)? as usize;
                let d = dist.decode(bits)? as usize;
                if d >= 30 {
                    bail!("invalid distance code in deflate stream");
                }
                let distance = DIST_BASE[d] as usize + bits.bits(DIST_EXTRA[d] as u32)? as usize;
                if distance > out.len() {
                    bail!("deflate distance points before the start of the output");
                }
                let start = out.len() - distance;
                for k in 0..len {
                    out.push(out[start + k]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &[u8] = &[203, 72, 205, 201, 201, 87, 200, 32, 76, 42, 2, 0];
    const V2: &[u8] = &[
        99, 96, 100, 98, 102, 97, 101, 99, 231, 224, 228, 226, 230, 225, 229, 227, 23, 16, 20, 18,
        22, 17, 21, 19, 151, 144, 148, 146, 150, 145, 149, 147, 87, 80, 84, 82, 86, 81, 85, 83,
        215, 208, 212, 210, 214, 209, 213, 211, 55, 48, 52, 50, 54, 49, 53, 51, 183, 176, 180, 178,
        182, 177, 181, 179, 119, 112, 116, 114, 118, 113, 117, 115, 247, 240, 244, 242, 246, 241,
        245, 243, 15, 8, 12, 10, 14, 9, 13, 11, 143, 136, 140, 138, 142, 137, 141, 139, 79, 72, 76,
        74, 78, 73, 77, 75, 207, 200, 204, 202, 206, 201, 205, 203, 47, 40, 44, 42, 46, 41, 45, 43,
        175, 168, 172, 170, 174, 169, 173, 171, 111, 104, 108, 106, 110, 105, 109, 107, 239, 232,
        236, 234, 238, 233, 237, 235, 159, 48, 113, 210, 228, 41, 83, 167, 77, 159, 49, 115, 214,
        236, 57, 115, 231, 205, 95, 176, 112, 209, 226, 37, 75, 151, 45, 95, 177, 114, 213, 234,
        53, 107, 215, 173, 223, 176, 113, 211, 230, 45, 91, 183, 109, 223, 177, 115, 215, 238, 61,
        123, 247, 237, 63, 112, 240, 208, 225, 35, 71, 143, 29, 63, 113, 242, 212, 233, 51, 103,
        207, 157, 191, 112, 241, 210, 229, 43, 87, 175, 93, 191, 113, 243, 214, 237, 59, 119, 239,
        221, 127, 240, 240, 209, 227, 39, 79, 159, 61, 127, 241, 242, 213, 235, 55, 111, 223, 189,
        255, 240, 241, 211, 231, 47, 95, 191, 125, 255, 241, 243, 215, 239, 63, 127, 255, 253, 103,
        24, 245, 255, 168, 255, 71, 176, 255, 1,
    ];
    const ZIP: &[u8] = &[
        80, 75, 3, 4, 20, 0, 0, 0, 0, 0, 87, 117, 93, 93, 29, 115, 238, 109, 14, 0, 0, 0, 14, 0, 0,
        0, 12, 0, 0, 0, 97, 47, 115, 116, 111, 114, 101, 100, 46, 116, 120, 116, 115, 116, 111,
        114, 101, 100, 32, 99, 111, 110, 116, 101, 110, 116, 80, 75, 3, 4, 20, 0, 0, 0, 8, 0, 87,
        117, 93, 93, 243, 45, 57, 242, 42, 2, 0, 0, 194, 41, 0, 0, 14, 0, 0, 0, 98, 47, 100, 101,
        102, 108, 97, 116, 101, 100, 46, 116, 120, 116, 157, 216, 203, 113, 212, 80, 0, 69, 193,
        61, 81, 40, 4, 29, 253, 69, 54, 124, 6, 48, 12, 30, 176, 49, 96, 71, 79, 65, 6, 244, 90,
        117, 87, 58, 37, 189, 215, 215, 187, 251, 203, 48, 190, 30, 126, 124, 186, 12, 223, 159,
        238, 222, 125, 25, 222, 62, 220, 126, 221, 15, 31, 110, 191, 135, 207, 79, 95, 191, 61, 14,
        183, 159, 151, 135, 127, 143, 175, 111, 94, 158, 135, 247, 183, 143, 175, 174, 127, 55,
        193, 102, 130, 205, 12, 155, 5, 54, 43, 108, 54, 216, 236, 176, 57, 96, 115, 202, 59, 165,
        16, 164, 132, 36, 133, 164, 133, 36, 134, 164, 134, 36, 135, 164, 135, 36, 136, 164, 136,
        73, 138, 152, 232, 219, 32, 69, 76, 82, 196, 36, 69, 76, 82, 196, 36, 69, 76, 82, 196, 36,
        69, 76, 82, 196, 44, 69, 204, 82, 196, 76, 191, 11, 41, 98, 150, 34, 102, 41, 98, 150, 34,
        102, 41, 98, 150, 34, 102, 41, 98, 145, 34, 22, 41, 98, 145, 34, 22, 58, 65, 72, 17, 139,
        20, 177, 72, 17, 139, 20, 177, 72, 17, 139, 20, 177, 74, 17, 171, 20, 177, 74, 17, 171, 20,
        177, 210, 161, 82, 138, 88, 165, 136, 85, 138, 88, 165, 136, 85, 138, 216, 164, 136, 77,
        138, 216, 164, 136, 77, 138, 216, 164, 136, 141, 238, 25, 82, 196, 38, 69, 108, 82, 196,
        38, 69, 236, 82, 196, 46, 69, 236, 82, 196, 46, 69, 236, 82, 196, 46, 69, 236, 116, 245,
        148, 34, 118, 41, 98, 151, 34, 14, 41, 226, 144, 34, 14, 41, 226, 144, 34, 14, 41, 226,
        144, 34, 14, 41, 226, 32, 141, 144, 34, 14, 41, 226, 148, 34, 78, 41, 226, 148, 34, 78, 41,
        226, 148, 34, 78, 41, 226, 148, 34, 78, 41, 226, 36, 160, 50, 161, 34, 162, 26, 201, 168,
        70, 66, 170, 145, 148, 106, 36, 166, 26, 201, 169, 70, 130, 170, 145, 164, 106, 36, 170,
        26, 169, 13, 228, 75, 106, 195, 0, 211, 4, 211, 8, 211, 12, 211, 16, 211, 20, 211, 24, 147,
        28, 51, 130, 204, 72, 50, 35, 202, 140, 44, 51, 194, 204, 72, 51, 35, 206, 140, 60, 51, 2,
        205, 72, 52, 35, 210, 140, 76, 51, 66, 205, 72, 53, 35, 214, 140, 92, 51, 130, 205, 72, 54,
        35, 218, 140, 108, 51, 194, 205, 72, 55, 35, 222, 140, 124, 51, 2, 206, 72, 56, 35, 226,
        140, 140, 51, 66, 206, 72, 57, 35, 230, 140, 156, 51, 130, 206, 72, 58, 35, 234, 140, 172,
        51, 194, 206, 72, 59, 35, 238, 140, 188, 51, 2, 207, 72, 60, 35, 242, 140, 204, 51, 66,
        207, 72, 61, 35, 246, 140, 220, 51, 130, 207, 72, 62, 35, 250, 140, 236, 51, 194, 207, 72,
        63, 35, 254, 140, 252, 51, 2, 208, 72, 64, 35, 2, 141, 12, 52, 66, 208, 72, 65, 35, 6, 141,
        28, 52, 130, 208, 72, 66, 35, 10, 141, 44, 52, 194, 208, 72, 67, 35, 14, 141, 60, 52, 2,
        209, 72, 68, 35, 18, 141, 76, 52, 66, 209, 72, 69, 35, 22, 237, 127, 93, 244, 15, 80, 75,
        3, 4, 20, 0, 0, 0, 8, 0, 0, 0, 33, 0, 47, 58, 218, 233, 20, 0, 0, 0, 18, 0, 0, 0, 9, 0, 0,
        0, 99, 47, 101, 120, 101, 99, 46, 115, 104, 83, 86, 212, 79, 202, 204, 211, 47, 206, 224,
        74, 77, 206, 200, 87, 200, 200, 228, 2, 0, 80, 75, 1, 2, 20, 3, 20, 0, 0, 0, 0, 0, 87, 117,
        93, 93, 29, 115, 238, 109, 14, 0, 0, 0, 14, 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        128, 1, 0, 0, 0, 0, 97, 47, 115, 116, 111, 114, 101, 100, 46, 116, 120, 116, 80, 75, 1, 2,
        20, 3, 20, 0, 0, 0, 8, 0, 87, 117, 93, 93, 243, 45, 57, 242, 42, 2, 0, 0, 194, 41, 0, 0,
        14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 1, 56, 0, 0, 0, 98, 47, 100, 101, 102, 108, 97,
        116, 101, 100, 46, 116, 120, 116, 80, 75, 1, 2, 20, 3, 20, 0, 0, 0, 8, 0, 0, 0, 33, 0, 47,
        58, 218, 233, 20, 0, 0, 0, 18, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 237, 1, 142, 2,
        0, 0, 99, 47, 101, 120, 101, 99, 46, 115, 104, 80, 75, 5, 6, 0, 0, 0, 0, 3, 0, 3, 0, 173,
        0, 0, 0, 201, 2, 0, 0, 0, 0,
    ];

    #[test]
    fn inflates_fixed_and_dynamic_blocks() {
        assert_eq!(
            inflate(V1, 42).unwrap(),
            b"hello hello hello hello hello hello hello!"
        );
        let v2: Vec<u8> = (0..=255u8).cycle().take(1024).collect();
        assert_eq!(inflate(V2, 1024).unwrap(), v2);
        assert!(inflate(&V1[..5], 42).is_err());
    }

    #[test]
    fn reads_zip_entries() {
        let list = entries(ZIP).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a/stored.txt", "b/deflated.txt", "c/exec.sh"]);
        assert_eq!(read(ZIP, &list[0]).unwrap(), b"stored content");
        let text = String::from_utf8(read(ZIP, &list[1]).unwrap()).unwrap();
        assert!(text.starts_with("line 0: the quick brown fox") && text.lines().count() == 200);
        assert_eq!(read(ZIP, &list[2]).unwrap(), b"#!/bin/sh\necho hi\n");
        assert_eq!(list[2].mode & 0o777, 0o755);
        assert!(entries(b"not a zip at all, nope").is_err());
    }
}
