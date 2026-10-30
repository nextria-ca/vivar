#![allow(dead_code)]
//! Zstandard decompression (RFC 8878), written out in full: frames, raw,
//! RLE and compressed blocks, Huffman literals (one or four streams, FSE
//! compressed weights), FSE sequences with the predefined tables, repeat
//! offsets. No dictionaries. conda packages (`.conda`) and sharded repodata
//! are zstd streams; without this, native dependencies would need a zstd
//! binary on every machine.

use anyhow::{anyhow, bail, Result};

const MAGIC: u32 = 0xFD2F_B528;

/// Decompresses every frame in `data` and concatenates the results.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let magic = u32le(data, pos)?;
        if (0x184D_2A50..=0x184D_2A5F).contains(&magic) {
            // A skippable frame: 4 bytes of size, then that many to skip.
            let size = u32le(data, pos + 4)? as usize;
            pos += 8 + size;
            continue;
        }
        if magic != MAGIC {
            bail!("not a zstd stream (bad magic number)");
        }
        pos = frame(data, pos + 4, &mut out)?;
    }
    Ok(out)
}

fn u32le(d: &[u8], at: usize) -> Result<u32> {
    d.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| anyhow!("truncated zstd stream"))
}

fn byte(d: &[u8], at: usize) -> Result<u8> {
    d.get(at)
        .copied()
        .ok_or_else(|| anyhow!("truncated zstd stream"))
}

/// Decodes one frame starting after its magic number; returns the position
/// after it.
fn frame(data: &[u8], mut pos: usize, out: &mut Vec<u8>) -> Result<usize> {
    let descriptor = byte(data, pos)?;
    pos += 1;
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    let checksum = descriptor & 0x04 != 0;
    let dict_flag = descriptor & 0x03;
    if !single_segment {
        pos += 1; // window descriptor: the output buffer is the window
    }
    let dict_size = [0usize, 1, 2, 4][dict_flag as usize];
    if dict_size > 0 {
        let id = match dict_size {
            1 => byte(data, pos)? as u32,
            2 => u16::from_le_bytes([byte(data, pos)?, byte(data, pos + 1)?]) as u32,
            _ => u32le(data, pos)?,
        };
        if id != 0 {
            bail!("zstd stream needs dictionary {id}, which is not supported");
        }
        pos += dict_size;
    }
    let fcs_size = match fcs_flag {
        0 => usize::from(single_segment),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let content_size: Option<u64> = match fcs_size {
        0 => None,
        1 => Some(byte(data, pos)? as u64),
        2 => Some(u16::from_le_bytes([byte(data, pos)?, byte(data, pos + 1)?]) as u64 + 256),
        4 => Some(u32le(data, pos)? as u64),
        _ => Some(u32le(data, pos)? as u64 | ((u32le(data, pos + 4)? as u64) << 32)),
    };
    pos += fcs_size;
    if let Some(n) = content_size {
        out.reserve(n as usize);
    }
    let start = out.len();
    let mut ctx = FrameContext::default();
    loop {
        let header = byte(data, pos)? as u32
            | (byte(data, pos + 1)? as u32) << 8
            | (byte(data, pos + 2)? as u32) << 16;
        pos += 3;
        let last = header & 1 != 0;
        let block_type = (header >> 1) & 3;
        let size = (header >> 3) as usize;
        match block_type {
            0 => {
                out.extend_from_slice(
                    data.get(pos..pos + size)
                        .ok_or_else(|| anyhow!("truncated zstd block"))?,
                );
                pos += size;
            }
            1 => {
                let b = byte(data, pos)?;
                out.resize(out.len() + size, b);
                pos += 1;
            }
            2 => {
                let block = data
                    .get(pos..pos + size)
                    .ok_or_else(|| anyhow!("truncated zstd block"))?;
                compressed_block(block, out, &mut ctx)?;
                pos += size;
            }
            _ => bail!("reserved zstd block type"),
        }
        if last {
            break;
        }
    }
    if let Some(n) = content_size {
        if (out.len() - start) as u64 != n {
            bail!(
                "zstd frame decoded to {} bytes, header says {n}",
                out.len() - start
            );
        }
    }
    if checksum {
        pos += 4; // xxhash of the content: not verified
    }
    Ok(pos)
}

/// What later blocks of a frame may reuse from earlier ones.
#[derive(Default)]
struct FrameContext {
    huffman: Option<Huffman>,
    ll: Option<FseTable>,
    of: Option<FseTable>,
    ml: Option<FseTable>,
    rep: Option<[usize; 3]>,
}

// ---------------------------------------------------------------------------
// Bit reading. Huffman and FSE streams are read backwards from their last
// byte, whose highest set bit marks the end; the FSE table description is
// read forwards.

struct Backward<'a> {
    data: &'a [u8],
    /// Bits still unread, counted from the start of the stream; negative
    /// once a read went past the start (the reference decoder's "overflow").
    bits: isize,
}

impl<'a> Backward<'a> {
    fn new(data: &'a [u8]) -> Result<Backward<'a>> {
        let last = *data.last().ok_or_else(|| anyhow!("empty zstd bitstream"))?;
        if last == 0 {
            bail!("zstd bitstream has no end marker");
        }
        let marker = 7 - last.leading_zeros() as isize;
        Ok(Backward {
            data,
            bits: (data.len() as isize - 1) * 8 + marker,
        })
    }

    /// The next `n` bits, highest first; bits past the start read as zero.
    fn peek(&self, n: usize) -> u32 {
        let mut v = 0u32;
        for k in 0..n {
            let idx = self.bits - n as isize + k as isize;
            if idx >= 0 {
                let i = idx as usize;
                v |= (((self.data[i / 8] >> (i % 8)) & 1) as u32) << k;
            }
        }
        v
    }

    fn read(&mut self, n: usize) -> u32 {
        let v = self.peek(n);
        self.bits -= n as isize;
        v
    }

    /// Whether a read went past the start of the stream.
    fn overflowed(&self) -> bool {
        self.bits < 0
    }
}

struct Forward<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Forward<'_> {
    fn read(&mut self, n: usize) -> Result<u32> {
        let mut v = 0u32;
        for k in 0..n {
            let i = self.bit + k;
            let b = *self
                .data
                .get(i / 8)
                .ok_or_else(|| anyhow!("truncated FSE table description"))?;
            v |= (((b >> (i % 8)) & 1) as u32) << k;
        }
        self.bit += n;
        Ok(v)
    }

    fn bytes_used(&self) -> usize {
        self.bit.div_ceil(8)
    }
}

// ---------------------------------------------------------------------------
// FSE

#[derive(Clone)]
struct FseTable {
    log: usize,
    /// Per state: symbol, bits to read for the next state, base of it.
    symbol: Vec<u8>,
    nbits: Vec<u8>,
    base: Vec<u16>,
}

impl FseTable {
    /// Builds the decoding table from normalized probabilities (`-1`: less
    /// than one), as the spec lays it out.
    fn from_probabilities(probs: &[i16], log: usize) -> Result<FseTable> {
        let size = 1usize << log;
        let mut symbol = vec![0u8; size];
        let mut high = size;
        // Symbols of probability "less than 1" take the top cells, one each.
        for (s, &p) in probs.iter().enumerate() {
            if p == -1 {
                high -= 1;
                symbol[high] = s as u8;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mask = size - 1;
        let mut position = 0usize;
        for (s, &p) in probs.iter().enumerate() {
            for _ in 0..p.max(0) {
                symbol[position] = s as u8;
                position = (position + step) & mask;
                while position >= high {
                    position = (position + step) & mask;
                }
            }
        }
        if position != 0 {
            bail!("corrupt FSE table");
        }
        let mut next: Vec<u16> = probs
            .iter()
            .map(|&p| if p == -1 { 1 } else { p as u16 })
            .collect();
        let mut nbits = vec![0u8; size];
        let mut base = vec![0u16; size];
        for i in 0..size {
            let s = symbol[i] as usize;
            let x = next[s];
            next[s] += 1;
            let bits = log as u32 - (15 - x.leading_zeros());
            nbits[i] = bits as u8;
            base[i] = ((x as u32) << bits) as u16 - size as u16;
        }
        Ok(FseTable {
            log,
            symbol,
            nbits,
            base,
        })
    }

    /// Reads a table description (forward bitstream): accuracy log, then the
    /// probabilities. Returns the table and the bytes consumed.
    fn parse(data: &[u8], max_log: usize, max_symbol: usize) -> Result<(FseTable, usize)> {
        let mut r = Forward { data, bit: 0 };
        let log = r.read(4)? as usize + 5;
        if log > max_log {
            bail!("FSE accuracy log {log} over the limit {max_log}");
        }
        // As the reference decoder reads it: a value takes `nbits - 1` bits
        // unless it falls in the range that needs one more, and the range
        // shrinks (`threshold`) as probabilities are given out.
        let mut remaining = (1i32 << log) + 1;
        let mut threshold = 1i32 << log;
        let mut nbits = log + 1;
        let mut probs: Vec<i16> = Vec::new();
        while remaining > 1 && probs.len() <= max_symbol {
            let max = 2 * threshold - 1 - remaining;
            let low = r.read(nbits - 1)? as i32;
            let mut count = if low < max {
                low
            } else {
                let v = low | ((r.read(1)? as i32) << (nbits - 1));
                if v >= threshold {
                    v - max
                } else {
                    v
                }
            };
            count -= 1;
            remaining -= count.abs().max(if count == -1 { 1 } else { 0 });
            probs.push(count as i16);
            if count == 0 {
                // A run of zero-probability symbols: 2-bit counts, 3 meaning "more".
                loop {
                    let n = r.read(2)?;
                    probs.extend(std::iter::repeat_n(0, n as usize));
                    if n < 3 {
                        break;
                    }
                }
            }
            while remaining < threshold {
                nbits -= 1;
                threshold >>= 1;
            }
        }
        if remaining != 1 || probs.len() > max_symbol + 1 {
            bail!("corrupt FSE table description");
        }
        Ok((FseTable::from_probabilities(&probs, log)?, r.bytes_used()))
    }
}

/// One FSE decoding state over a backward stream.
struct FseState<'t> {
    table: &'t FseTable,
    state: usize,
}

impl<'t> FseState<'t> {
    fn init(table: &'t FseTable, r: &mut Backward) -> FseState<'t> {
        let state = r.read(table.log) as usize;
        FseState { table, state }
    }

    fn symbol(&self) -> u8 {
        self.table.symbol[self.state]
    }

    fn update(&mut self, r: &mut Backward) {
        let bits = self.table.nbits[self.state] as usize;
        self.state = self.table.base[self.state] as usize + r.read(bits) as usize;
    }
}

// ---------------------------------------------------------------------------
// Huffman literals

#[derive(Clone)]
struct Huffman {
    max_bits: usize,
    /// Indexed by the next `max_bits` bits: (symbol, code length).
    table: Vec<(u8, u8)>,
}

impl Huffman {
    /// Reads a tree description; returns it and the bytes consumed.
    fn parse(data: &[u8]) -> Result<(Huffman, usize)> {
        let header = byte(data, 0)? as usize;
        let mut weights: Vec<u8> = Vec::new();
        let used = if header < 128 {
            // FSE-compressed weights: two interleaved states, until the
            // stream runs dry.
            let (table, desc_len) = FseTable::parse(&data[1..1 + header], 6, 255)?;
            let stream = data
                .get(1 + desc_len..1 + header)
                .ok_or_else(|| anyhow!("truncated Huffman weights"))?;
            let mut r = Backward::new(stream)?;
            let mut a = FseState::init(&table, &mut r);
            let mut b = FseState::init(&table, &mut r);
            // Two states take turns; when an update runs past the start of
            // the stream, the other state gives the last symbol.
            loop {
                weights.push(a.symbol());
                a.update(&mut r);
                if r.overflowed() {
                    weights.push(b.symbol());
                    break;
                }
                weights.push(b.symbol());
                b.update(&mut r);
                if r.overflowed() {
                    weights.push(a.symbol());
                    break;
                }
                if weights.len() > 255 {
                    bail!("too many Huffman weights");
                }
            }
            1 + header
        } else {
            let n = header - 127;
            for i in 0..n {
                let b = byte(data, 1 + i / 2)?;
                weights.push(if i % 2 == 0 { b >> 4 } else { b & 15 });
            }
            1 + n.div_ceil(2)
        };
        // The last weight is implied: it completes the sum to a power of two.
        let sum: u32 = weights
            .iter()
            .map(|&w| if w > 0 { 1u32 << (w - 1) } else { 0 })
            .sum();
        if sum == 0 {
            bail!("corrupt Huffman tree");
        }
        let max_bits = 32 - (sum - 1).leading_zeros() as usize; // next power of two's log
        let total = 1u32 << max_bits;
        let left = total - sum;
        if left == 0 || !left.is_power_of_two() {
            bail!("corrupt Huffman tree weights");
        }
        weights.push(left.trailing_zeros() as u8 + 1);
        if max_bits > 11 {
            bail!("Huffman code longer than 11 bits");
        }
        let mut table = vec![(0u8, 0u8); 1 << max_bits];
        let mut pos = 0usize;
        for w in 1..=max_bits as u8 {
            for (s, &ws) in weights.iter().enumerate() {
                if ws == w {
                    let len = max_bits as u8 + 1 - w;
                    let count = 1usize << (w - 1);
                    for cell in &mut table[pos..pos + count] {
                        *cell = (s as u8, len);
                    }
                    pos += count;
                }
            }
        }
        Ok((Huffman { max_bits, table }, used))
    }

    /// Decodes exactly `n` symbols from one backward stream.
    fn decode(&self, stream: &[u8], n: usize, out: &mut Vec<u8>) -> Result<()> {
        let mut r = Backward::new(stream)?;
        for _ in 0..n {
            let (symbol, len) = self.table[r.peek(self.max_bits) as usize];
            r.read(len as usize);
            out.push(symbol);
        }
        Ok(())
    }
}

/// Decodes the literals section; returns the literals and the bytes used.
fn literals(block: &[u8], ctx: &mut FrameContext) -> Result<(Vec<u8>, usize)> {
    let h0 = byte(block, 0)?;
    let kind = h0 & 3;
    let size_format = (h0 >> 2) & 3;
    match kind {
        0 | 1 => {
            let (size, header) = match size_format {
                0 | 2 => ((h0 >> 3) as usize, 1),
                1 => (((h0 >> 4) as usize) | ((byte(block, 1)? as usize) << 4), 2),
                _ => (
                    ((h0 >> 4) as usize)
                        | ((byte(block, 1)? as usize) << 4)
                        | ((byte(block, 2)? as usize) << 12),
                    3,
                ),
            };
            if kind == 0 {
                let lits = block
                    .get(header..header + size)
                    .ok_or_else(|| anyhow!("truncated literals"))?;
                Ok((lits.to_vec(), header + size))
            } else {
                Ok((vec![byte(block, header)?; size], header + 1))
            }
        }
        _ => {
            let (regenerated, compressed, header, streams) = match size_format {
                0 | 1 => {
                    let v = (h0 as usize >> 4)
                        | ((byte(block, 1)? as usize) << 4)
                        | ((byte(block, 2)? as usize) << 12);
                    (v & 0x3ff, v >> 10, 3, if size_format == 0 { 1 } else { 4 })
                }
                2 => {
                    let v = (h0 as usize >> 4)
                        | ((byte(block, 1)? as usize) << 4)
                        | ((byte(block, 2)? as usize) << 12)
                        | ((byte(block, 3)? as usize) << 20);
                    (v & 0x3fff, v >> 14, 4, 4)
                }
                _ => {
                    let v = (h0 as usize >> 4)
                        | ((byte(block, 1)? as usize) << 4)
                        | ((byte(block, 2)? as usize) << 12)
                        | ((byte(block, 3)? as usize) << 20)
                        | ((byte(block, 4)? as usize) << 28);
                    (v & 0x3ffff, v >> 18, 5, 4)
                }
            };
            let section = block
                .get(header..header + compressed)
                .ok_or_else(|| anyhow!("truncated compressed literals"))?;
            let mut at = 0;
            if kind == 2 {
                let (tree, used) = Huffman::parse(section)?;
                ctx.huffman = Some(tree);
                at = used;
            }
            let tree = ctx
                .huffman
                .as_ref()
                .ok_or_else(|| anyhow!("treeless literals without an earlier tree"))?;
            let mut lits = Vec::with_capacity(regenerated);
            if streams == 1 {
                tree.decode(&section[at..], regenerated, &mut lits)?;
            } else {
                let sizes = [
                    u16::from_le_bytes([byte(section, at)?, byte(section, at + 1)?]) as usize,
                    u16::from_le_bytes([byte(section, at + 2)?, byte(section, at + 3)?]) as usize,
                    u16::from_le_bytes([byte(section, at + 4)?, byte(section, at + 5)?]) as usize,
                ];
                at += 6;
                let each = regenerated.div_ceil(4);
                let mut start = at;
                for (i, &len) in sizes.iter().enumerate() {
                    let stream = section
                        .get(start..start + len)
                        .ok_or_else(|| anyhow!("truncated literal stream {}", i + 1))?;
                    tree.decode(stream, each, &mut lits)?;
                    start += len;
                }
                let last = section
                    .get(start..)
                    .ok_or_else(|| anyhow!("truncated literal stream 4"))?;
                tree.decode(last, regenerated - 3 * each, &mut lits)?;
            }
            Ok((lits, header + compressed))
        }
    }
}

// ---------------------------------------------------------------------------
// Sequences

const LL_PREDEF: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const OF_PREDEF: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
const ML_PREDEF: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 64, 128,
    256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072,
];
const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 5, 6, 7, 8, 9, 10,
    11, 12, 13, 14, 15,
];
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Reads one symbol table for the sequences section according to its mode.
fn sequence_table(
    data: &[u8],
    mode: u8,
    predefined: &[i16],
    predefined_log: usize,
    max_log: usize,
    previous: &mut Option<FseTable>,
) -> Result<usize> {
    match mode {
        0 => {
            *previous = Some(FseTable::from_probabilities(predefined, predefined_log)?);
            Ok(0)
        }
        1 => {
            let s = byte(data, 0)?;
            // A single symbol: a one-cell table that never reads a bit.
            *previous = Some(FseTable {
                log: 0,
                symbol: vec![s],
                nbits: vec![0],
                base: vec![0],
            });
            Ok(1)
        }
        2 => {
            let (t, used) = FseTable::parse(data, max_log, predefined.len() - 1)?;
            *previous = Some(t);
            Ok(used)
        }
        _ => {
            if previous.is_none() {
                bail!("sequence table repeat without an earlier table");
            }
            Ok(0)
        }
    }
}

fn compressed_block(block: &[u8], out: &mut Vec<u8>, ctx: &mut FrameContext) -> Result<()> {
    let (lits, used) = literals(block, ctx)?;
    let mut pos = used;
    let b0 = byte(block, pos)? as usize;
    let count = if b0 == 0 {
        pos += 1;
        0
    } else if b0 < 128 {
        pos += 1;
        b0
    } else if b0 < 255 {
        let v = ((b0 - 128) << 8) + byte(block, pos + 1)? as usize;
        pos += 2;
        v
    } else {
        let v = byte(block, pos + 1)? as usize + ((byte(block, pos + 2)? as usize) << 8) + 0x7f00;
        pos += 3;
        v
    };
    if count == 0 {
        out.extend_from_slice(&lits);
        return Ok(());
    }
    let modes = byte(block, pos)?;
    pos += 1;
    pos += sequence_table(&block[pos..], modes >> 6, &LL_PREDEF, 6, 9, &mut ctx.ll)?;
    pos += sequence_table(
        &block[pos..],
        (modes >> 4) & 3,
        &OF_PREDEF,
        5,
        8,
        &mut ctx.of,
    )?;
    pos += sequence_table(
        &block[pos..],
        (modes >> 2) & 3,
        &ML_PREDEF,
        6,
        9,
        &mut ctx.ml,
    )?;
    let (ll_t, of_t, ml_t) = (
        ctx.ll.as_ref().unwrap(),
        ctx.of.as_ref().unwrap(),
        ctx.ml.as_ref().unwrap(),
    );
    let stream = block
        .get(pos..)
        .ok_or_else(|| anyhow!("truncated sequences"))?;
    let mut r = Backward::new(stream)?;
    let mut ll = FseState::init(ll_t, &mut r);
    let mut of = FseState::init(of_t, &mut r);
    let mut ml = FseState::init(ml_t, &mut r);
    let mut rep = ctx.rep.unwrap_or([1, 4, 8]);
    let mut lit_pos = 0usize;
    for i in 0..count {
        let of_code = of.symbol() as usize;
        let ml_code = ml.symbol() as usize;
        let ll_code = ll.symbol() as usize;
        if of_code > 31 || ml_code > 52 || ll_code > 35 {
            bail!("corrupt sequence codes");
        }
        let offset_value = (1usize << of_code) + r.read(of_code) as usize;
        let match_len = ML_BASE[ml_code] as usize + r.read(ML_BITS[ml_code] as usize) as usize;
        let lit_len = LL_BASE[ll_code] as usize + r.read(LL_BITS[ll_code] as usize) as usize;
        let offset = if offset_value > 3 {
            let o = offset_value - 3;
            rep = [o, rep[0], rep[1]];
            o
        } else {
            let idx = offset_value - 1 + usize::from(lit_len == 0);
            match idx {
                0 => rep[0],
                1 => {
                    rep.swap(0, 1);
                    rep[0]
                }
                2 => {
                    rep = [rep[2], rep[0], rep[1]];
                    rep[0]
                }
                _ => {
                    let o = rep[0].checked_sub(1).filter(|&o| o > 0);
                    let o = o.ok_or_else(|| anyhow!("corrupt repeat offset"))?;
                    rep = [o, rep[0], rep[1]];
                    o
                }
            }
        };
        let lit_end = lit_pos + lit_len;
        out.extend_from_slice(
            lits.get(lit_pos..lit_end)
                .ok_or_else(|| anyhow!("sequence asks for more literals than there are"))?,
        );
        lit_pos = lit_end;
        if offset > out.len() {
            bail!("zstd match offset points before the start of the output");
        }
        let start = out.len() - offset;
        for k in 0..match_len {
            out.push(out[start + k]);
        }
        if i + 1 < count {
            ll.update(&mut r);
            ml.update(&mut r);
            of.update(&mut r);
        }
    }
    out.extend_from_slice(&lits[lit_pos..]);
    ctx.rep = Some(rep);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `zstd -19` of "hello hello hello hello hello hello hello!": raw
    /// literals and one repeated sequence, predefined tables.
    const HELLO: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x68, 0x6d, 0x00, 0x00, 0x38, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
        0x20, 0x21, 0x01, 0x00, 0x42, 0x96, 0x22, 0xaf, 0x82, 0x26, 0xc7,
    ];
    /// `zstd -19` of a line of repeated words: several sequences with
    /// repeat offsets and FSE-compressed tables.
    const WORDS: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x68, 0x9d, 0x02, 0x00, 0x44, 0x03, 0x63, 0x6f, 0x6e, 0x64,
        0x61, 0x20, 0x66, 0x6f, 0x72, 0x67, 0x65, 0x20, 0x70, 0x79, 0x74, 0x68, 0x6f, 0x6e, 0x20,
        0x6e, 0x75, 0x6d, 0x70, 0x79, 0x20, 0x67, 0x64, 0x61, 0x6c, 0x20, 0x63, 0x75, 0x64, 0x61,
        0x20, 0x77, 0x68, 0x65, 0x65, 0x6c, 0x20, 0x6c, 0x6f, 0x63, 0x6b, 0x20, 0x31, 0x32, 0x33,
        0x34, 0x35, 0x0a, 0x0b, 0x20, 0x40, 0x82, 0xe4, 0x3d, 0x2c, 0x87, 0xc7, 0x00, 0xf1, 0x81,
        0xc2, 0x82, 0x93, 0xd0, 0xe7, 0x79, 0xd4, 0x84, 0x25, 0x0b, 0x09, 0x10, 0x07, 0x38, 0x5e,
        0xa2, 0x0c, 0x38, 0x79, 0xba, 0x37,
    ];

    #[test]
    fn decodes_small_streams() {
        let out = decompress(HELLO).unwrap();
        assert_eq!(out, b"hello hello hello hello hello hello hello!");
        assert_eq!(
            decompress(WORDS).unwrap(),
            b"conda forge python numpy gdal cuda wheel lock conda numpy python gdal numpy conda forge 12345 python cuda lock wheel conda forge\n"
        );
        assert!(decompress(b"not zstd at all").is_err());
        // A skippable frame ahead of a real one is skipped.
        let mut with_skip = vec![0x50, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 9, 9, 9];
        with_skip.extend_from_slice(HELLO);
        assert_eq!(decompress(&with_skip).unwrap(), out);
    }

    #[test]
    fn round_trips_through_the_reference_encoder_when_present() {
        let Ok(probe) = std::process::Command::new("zstd").arg("--version").output() else {
            return;
        };
        if !probe.status.success() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("vivar-zstd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Text with structure (Huffman + sequences), bytes with little
        // (raw blocks), and enough of both for several blocks.
        let mut seed = 12345u64;
        let mut rand = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        let words = [
            "conda", "forge", "python", "numpy", "gdal", "cuda", "wheel", "lock",
        ];
        let mut text = String::new();
        while text.len() < 400_000 {
            text.push_str(words[rand() as usize % words.len()]);
            text.push(if rand() % 7 == 0 { '\n' } else { ' ' });
            if rand() % 11 == 0 {
                text.push_str(&format!("{}", rand()));
            }
        }
        let noise: Vec<u8> = (0..300_000).map(|_| rand() as u8).collect();
        for (name, data) in [
            ("text", text.as_bytes()),
            ("noise", &noise[..]),
            ("tiny", b"x"),
        ] {
            for level in ["-1", "-5", "-19", "--ultra", "--long"] {
                let input = dir.join(format!("{name}-{level}.bin"));
                std::fs::write(&input, data).unwrap();
                let output = dir.join(format!("{name}-{level}.zst"));
                let st = std::process::Command::new("zstd")
                    .args(["-q", "-f", level])
                    .arg(&input)
                    .arg("-o")
                    .arg(&output)
                    .status()
                    .unwrap();
                assert!(st.success());
                let compressed = std::fs::read(&output).unwrap();
                let out = decompress(&compressed).unwrap_or_else(|e| panic!("{name} {level}: {e}"));
                assert!(out == data, "{name} at {level} did not round-trip");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
