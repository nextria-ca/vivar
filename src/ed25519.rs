//! Ed25519 (RFC 8032) in plain Rust: keypairs, signatures, verification.
//! Used for public-key bundle signatures. Not constant time: signing happens
//! on the key owner's own machine, never in a network-facing service.

use crate::sha512::Sha512;

type Fe = [u64; 5]; // 51-bit limbs, little endian

const P_MINUS_2_BITS: u32 = 255;
/// 2^252 + 27742317777372353535851937790883648493
const L: [u8; 32] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];
const D: Fe = [
    929955233495203,
    466365720129213,
    1662059464998953,
    2033849074728123,
    1442794654840575,
];
const D2: Fe = [
    1859910466990425,
    932731440258426,
    1072319116312658,
    1815898335770999,
    633789495995903,
];
const SQRT_M1: Fe = [
    1718705420411056,
    234908883556509,
    2233514472574048,
    2117202627021982,
    765476049583133,
];

const MASK: u64 = (1 << 51) - 1;

fn fe_zero() -> Fe {
    [0; 5]
}
fn fe_one() -> Fe {
    [1, 0, 0, 0, 0]
}

fn fe_add(a: &Fe, b: &Fe) -> Fe {
    let mut r = *a;
    for i in 0..5 {
        r[i] += b[i];
    }
    r
}

fn fe_sub(a: &Fe, b: &Fe) -> Fe {
    // a + 2p - b, keeping limbs positive
    let mut r = fe_zero();
    r[0] = a[0] + 0xfffffffffffda - b[0];
    for i in 1..5 {
        r[i] = a[i] + 0xffffffffffffe - b[i];
    }
    fe_carry(&r)
}

fn fe_carry(a: &Fe) -> Fe {
    let mut r = *a;
    for i in 0..4 {
        r[i + 1] += r[i] >> 51;
        r[i] &= MASK;
    }
    r[0] += 19 * (r[4] >> 51);
    r[4] &= MASK;
    r[1] += r[0] >> 51;
    r[0] &= MASK;
    r
}

fn fe_mul(a: &Fe, b: &Fe) -> Fe {
    let a = *a;
    let b19 = [b[0], b[1] * 19, b[2] * 19, b[3] * 19, b[4] * 19];
    let mut t = [0u128; 5];
    for (i, ai) in a.iter().enumerate() {
        for j in 0..5 {
            let k = (i + j) % 5;
            let m = if i + j >= 5 { b19[j] } else { b[j] };
            t[k] += *ai as u128 * m as u128;
        }
    }
    let mut r = fe_zero();
    let mut carry: u128 = 0;
    for (ri, ti) in r.iter_mut().zip(t) {
        let v = ti + carry;
        *ri = (v as u64) & MASK;
        carry = v >> 51;
    }
    r[0] += (carry as u64) * 19;
    fe_carry(&r)
}

fn fe_sq(a: &Fe) -> Fe {
    fe_mul(a, a)
}

fn fe_pow(a: &Fe, exp_bits: &[u8]) -> Fe {
    // Square-and-multiply over a little-endian bit string.
    let mut r = fe_one();
    for &bit in exp_bits.iter().rev() {
        r = fe_sq(&r);
        if bit == 1 {
            r = fe_mul(&r, a);
        }
    }
    r
}

fn p_minus(n: u8) -> Vec<u8> {
    // bits of 2^255 - 19 - n, little endian
    let mut bytes = [0xffu8; 32];
    bytes[31] = 0x7f;
    bytes[0] = 0xed - n;
    let mut bits = Vec::with_capacity(P_MINUS_2_BITS as usize);
    for i in 0..255 {
        bits.push((bytes[i / 8] >> (i % 8)) & 1);
    }
    bits
}

fn fe_invert(a: &Fe) -> Fe {
    fe_pow(a, &p_minus(2))
}

fn fe_pow_p58(a: &Fe) -> Fe {
    // a^((p-5)/8)
    let mut bytes = [0xffu8; 32];
    bytes[31] = 0x0f;
    bytes[0] = 0xfd;
    let mut bits = Vec::new();
    for i in 0..252 {
        bits.push((bytes[i / 8] >> (i % 8)) & 1);
    }
    fe_pow(a, &bits)
}

fn fe_to_bytes(a: &Fe) -> [u8; 32] {
    // Fully reduce mod p, then pack 5 x 51 bits into 4 little-endian words.
    let mut r = fe_carry(a);
    let mut q = (r[0] + 19) >> 51;
    q = (r[1] + q) >> 51;
    q = (r[2] + q) >> 51;
    q = (r[3] + q) >> 51;
    q = (r[4] + q) >> 51;
    r[0] += 19 * q;
    for i in 0..4 {
        r[i + 1] += r[i] >> 51;
        r[i] &= MASK;
    }
    r[4] &= MASK;
    let w0 = r[0] | (r[1] << 51);
    let w1 = (r[1] >> 13) | (r[2] << 38);
    let w2 = (r[2] >> 26) | (r[3] << 25);
    let w3 = (r[3] >> 39) | (r[4] << 12);
    let mut out = [0u8; 32];
    for (i, w) in [w0, w1, w2, w3].iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn fe_from_bytes(b: &[u8; 32]) -> Fe {
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (w0, w1, w2, w3) = (w(0), w(1), w(2), w(3));
    [
        w0 & MASK,
        ((w0 >> 51) | (w1 << 13)) & MASK,
        ((w1 >> 38) | (w2 << 26)) & MASK,
        ((w2 >> 25) | (w3 << 39)) & MASK,
        (w3 >> 12) & MASK, // drops bit 255 (the sign bit of a point encoding)
    ]
}

fn fe_is_negative(a: &Fe) -> bool {
    fe_to_bytes(a)[0] & 1 == 1
}

fn fe_eq(a: &Fe, b: &Fe) -> bool {
    fe_to_bytes(a) == fe_to_bytes(b)
}

/// Extended coordinates (X, Y, Z, T) with x = X/Z, y = Y/Z, xy = T/Z.
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

fn identity() -> Point {
    Point {
        x: fe_zero(),
        y: fe_one(),
        z: fe_one(),
        t: fe_zero(),
    }
}

fn add(p: &Point, q: &Point) -> Point {
    let a = fe_mul(&fe_sub(&p.y, &p.x), &fe_sub(&q.y, &q.x));
    let b = fe_mul(&fe_add(&p.y, &p.x), &fe_add(&q.y, &q.x));
    let c = fe_mul(&fe_mul(&p.t, &q.t), &D2);
    let d = fe_mul(&fe_add(&p.z, &p.z), &q.z);
    let e = fe_sub(&b, &a);
    let f = fe_sub(&d, &c);
    let g = fe_add(&d, &c);
    let h = fe_add(&b, &a);
    Point {
        x: fe_mul(&e, &f),
        y: fe_mul(&g, &h),
        z: fe_mul(&f, &g),
        t: fe_mul(&e, &h),
    }
}

fn double(p: &Point) -> Point {
    add(p, p)
}

fn scalar_mul(k: &[u8; 32], p: &Point) -> Point {
    let mut r = identity();
    for i in (0..256).rev() {
        r = double(&r);
        if (k[i / 8] >> (i % 8)) & 1 == 1 {
            r = add(&r, p);
        }
    }
    r
}

fn base_point() -> Point {
    let y = fe_from_bytes(&{
        let mut b = [0u8; 32];
        // 4/5 mod p
        let inv5 = fe_invert(&[5, 0, 0, 0, 0]);
        let four_fifths = fe_mul(&[4, 0, 0, 0, 0], &inv5);
        b.copy_from_slice(&fe_to_bytes(&four_fifths));
        b
    });
    let x = recover_x(&y, false).expect("base point");
    Point {
        x,
        y,
        z: fe_one(),
        t: fe_mul(&x, &y),
    }
}

/// x from y: x^2 = (y^2 - 1) / (d y^2 + 1)
fn recover_x(y: &Fe, sign: bool) -> Option<Fe> {
    let y2 = fe_sq(y);
    let u = fe_sub(&y2, &fe_one());
    let v = fe_add(&fe_mul(&D, &y2), &fe_one());
    // x = u v^3 (u v^7)^((p-5)/8)
    let v3 = fe_mul(&fe_sq(&v), &v);
    let v7 = fe_mul(&fe_sq(&v3), &v);
    let mut x = fe_mul(&fe_mul(&u, &v3), &fe_pow_p58(&fe_mul(&u, &v7)));
    let vx2 = fe_mul(&v, &fe_sq(&x));
    if !fe_eq(&vx2, &u) {
        let neg_u = fe_sub(&fe_zero(), &u);
        if fe_eq(&vx2, &neg_u) {
            x = fe_mul(&x, &SQRT_M1);
        } else {
            return None;
        }
    }
    if fe_eq(&x, &fe_zero()) && sign {
        return None;
    }
    if fe_is_negative(&x) != sign {
        x = fe_sub(&fe_zero(), &x);
    }
    Some(x)
}

fn encode(p: &Point) -> [u8; 32] {
    let zi = fe_invert(&p.z);
    let x = fe_mul(&p.x, &zi);
    let y = fe_mul(&p.y, &zi);
    let mut out = fe_to_bytes(&y);
    if fe_is_negative(&x) {
        out[31] |= 0x80;
    }
    out
}

fn decode(bytes: &[u8; 32]) -> Option<Point> {
    let sign = bytes[31] >> 7 == 1;
    let mut yb = *bytes;
    yb[31] &= 0x7f;
    let y = fe_from_bytes(&yb);
    // reject non-canonical y
    if fe_to_bytes(&y) != yb {
        return None;
    }
    let x = recover_x(&y, sign)?;
    Some(Point {
        x,
        y,
        z: fe_one(),
        t: fe_mul(&x, &y),
    })
}

// --- scalar arithmetic mod L ---
// A small fixed-width bignum (32 x 32-bit limbs, 1024 bits) is plenty for
// reducing a SHA-512 output and one 256x256-bit product.

type Big = [u32; 32];

fn big_from_le(bytes: &[u8]) -> Big {
    let mut b = [0u32; 32];
    for (i, chunk) in bytes.chunks(4).enumerate().take(32) {
        let mut w = [0u8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        b[i] = u32::from_le_bytes(w);
    }
    b
}

fn big_bits(a: &Big) -> usize {
    for i in (0..32).rev() {
        if a[i] != 0 {
            return i * 32 + (32 - a[i].leading_zeros() as usize);
        }
    }
    0
}

fn big_shl(a: &Big, shift: usize) -> Big {
    let (limbs, bits) = (shift / 32, shift % 32);
    let mut r = [0u32; 32];
    for i in (limbs..32).rev() {
        let mut v = (a[i - limbs] as u64) << bits;
        if bits > 0 && i > limbs {
            v |= (a[i - limbs - 1] as u64) >> (32 - bits);
        }
        r[i] = v as u32;
    }
    r
}

fn big_ge(a: &Big, b: &Big) -> bool {
    for i in (0..32).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn big_sub(a: &Big, b: &Big) -> Big {
    let mut r = [0u32; 32];
    let mut borrow = 0u64;
    for i in 0..32 {
        let v = (a[i] as u64).wrapping_sub(b[i] as u64).wrapping_sub(borrow);
        r[i] = v as u32;
        borrow = (v >> 63) & 1;
    }
    r
}

fn big_mul(a: &Big, b: &Big) -> Big {
    let mut r = [0u32; 32];
    for i in 0..16 {
        let mut carry = 0u64;
        for j in 0..16 {
            let t = r[i + j] as u64 + a[i] as u64 * b[j] as u64 + carry;
            r[i + j] = t as u32;
            carry = t >> 32;
        }
        if i + 16 < 32 {
            r[i + 16] = carry as u32;
        }
    }
    r
}

fn big_add(a: &Big, b: &Big) -> Big {
    let mut r = [0u32; 32];
    let mut carry = 0u64;
    for i in 0..32 {
        let t = a[i] as u64 + b[i] as u64 + carry;
        r[i] = t as u32;
        carry = t >> 32;
    }
    r
}

fn big_mod_l(a: &Big) -> [u8; 32] {
    let l = big_from_le(&L);
    let mut n = *a;
    let top = big_bits(&n);
    if top >= 253 {
        for shift in (0..=(top - 253)).rev() {
            let shifted = big_shl(&l, shift);
            if big_ge(&n, &shifted) {
                n = big_sub(&n, &shifted);
            }
        }
    }
    if big_ge(&n, &l) {
        n = big_sub(&n, &l);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[i * 4..i * 4 + 4].copy_from_slice(&n[i].to_le_bytes());
    }
    out
}

fn sc_reduce(input: &[u8; 64]) -> [u8; 32] {
    big_mod_l(&big_from_le(input))
}

/// (a * b + c) mod L
fn sc_muladd(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> [u8; 32] {
    let prod = big_mul(&big_from_le(a), &big_from_le(b));
    big_mod_l(&big_add(&prod, &big_from_le(c)))
}

fn sc_lt_l(s: &[u8; 32]) -> bool {
    for i in (0..32).rev() {
        if s[i] != L[i] {
            return s[i] < L[i];
        }
    }
    false
}

// --- public API ---

pub struct Keypair {
    pub secret: [u8; 32],
    pub public: [u8; 32],
}

fn expand(secret: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let h = Sha512::digest(secret);
    let mut a = [0u8; 32];
    a.copy_from_slice(&h[..32]);
    a[0] &= 248;
    a[31] &= 63;
    a[31] |= 64;
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&h[32..]);
    (a, prefix)
}

pub fn keypair(secret: [u8; 32]) -> Keypair {
    let (a, _) = expand(&secret);
    let public = encode(&scalar_mul(&a, &base_point()));
    Keypair { secret, public }
}

pub fn sign(secret: &[u8; 32], message: &[u8]) -> [u8; 64] {
    let (a, prefix) = expand(secret);
    let public = encode(&scalar_mul(&a, &base_point()));
    let mut h = Sha512::new();
    h.update(&prefix);
    h.update(message);
    let r = sc_reduce(&h.finish());
    let big_r = encode(&scalar_mul(&r, &base_point()));
    let mut h2 = Sha512::new();
    h2.update(&big_r);
    h2.update(&public);
    h2.update(message);
    let k = sc_reduce(&h2.finish());
    let s = sc_muladd(&k, &a, &r);
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&big_r);
    out[32..].copy_from_slice(&s);
    out
}

/// Whether a point is one of the eight of small order: a key or nonce
/// there would make signatures that say nothing about the message.
fn small_order(p: &Point) -> bool {
    let mut eight = [0u8; 32];
    eight[0] = 8;
    let mut identity = [0u8; 32];
    identity[0] = 1;
    encode(&scalar_mul(&eight, p)) == identity
}

pub fn verify(public: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    let Some(a) = decode(public) else {
        return false;
    };
    if small_order(&a) {
        return false;
    }
    let mut rb = [0u8; 32];
    rb.copy_from_slice(&signature[..32]);
    let mut s = [0u8; 32];
    s.copy_from_slice(&signature[32..]);
    if !sc_lt_l(&s) {
        return false;
    }
    let Some(r) = decode(&rb) else { return false };
    if small_order(&r) {
        return false;
    }
    let mut h = Sha512::new();
    h.update(&rb);
    h.update(public);
    h.update(message);
    let k = sc_reduce(&h.finish());
    // s*B == R + k*A
    let lhs = scalar_mul(&s, &base_point());
    let rhs = add(&r, &scalar_mul(&k, &a));
    encode(&lhs) == encode(&rhs)
}

pub fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::sha256::hex;

    #[test]
    fn rejects_what_a_forger_would_try() {
        let secret = [7u8; 32];
        let kp = keypair(secret);
        let msg = b"bundle checksums";
        let sig = sign(&secret, msg);
        assert!(verify(&kp.public, msg, &sig));
        // A changed message, a flipped signature bit, another key.
        assert!(!verify(&kp.public, b"bundle checksumz", &sig));
        let mut flipped = sig;
        flipped[3] ^= 1;
        assert!(!verify(&kp.public, msg, &flipped));
        assert!(!verify(&keypair([8u8; 32]).public, msg, &sig));
        // S + L is the same scalar spelled non-canonically: refused.
        let mut malleable = sig;
        let mut carry = 0u16;
        for i in 0..32 {
            let v = malleable[32 + i] as u16 + L[i] as u16 + carry;
            malleable[32 + i] = v as u8;
            carry = v >> 8;
        }
        assert!(!verify(&kp.public, msg, &malleable));
        // A small-order public key or nonce proves nothing.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(!verify(&identity, msg, &sig));
        let mut small_r = sig;
        small_r[..32].copy_from_slice(&identity);
        assert!(!verify(&kp.public, msg, &small_r));
    }

    #[test]
    fn rfc8032_vectors() {
        // Test 1: empty message
        let sk = unhex::<32>("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
            .unwrap();
        let kp = keypair(sk);
        assert_eq!(
            hex(&kp.public),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        let sig = sign(&sk, b"");
        assert_eq!(hex(&sig), "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b");
        assert!(verify(&kp.public, b"", &sig));
        assert!(!verify(&kp.public, b"x", &sig));

        // Test 2: one byte
        let sk = unhex::<32>("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb")
            .unwrap();
        let kp = keypair(sk);
        assert_eq!(
            hex(&kp.public),
            "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
        );
        let sig = sign(&sk, &[0x72]);
        assert_eq!(hex(&sig), "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00");
        assert!(verify(&kp.public, &[0x72], &sig));

        // Test 3: two bytes
        let sk = unhex::<32>("c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7")
            .unwrap();
        let kp = keypair(sk);
        assert_eq!(
            hex(&kp.public),
            "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025"
        );
        let sig = sign(&sk, &[0xaf, 0x82]);
        assert_eq!(hex(&sig), "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a");
        assert!(verify(&kp.public, &[0xaf, 0x82], &sig));
        // A flipped bit anywhere fails.
        let mut bad = sig;
        bad[10] ^= 1;
        assert!(!verify(&kp.public, &[0xaf, 0x82], &bad));
    }
}
