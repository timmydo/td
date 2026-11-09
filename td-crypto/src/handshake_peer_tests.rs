//! TLS handshakes between td-crypto's sessions and a rustls peer whose
//! handshake signatures come from arithmetic written here, sharing no code
//! with td-primitives' signers: Montgomery big integers over u32 limbs, RSA
//! PKCS#1 v1.5 and PSS with CRT, ECDSA over P-256, P-384 and P-521 in
//! Jacobian coordinates, Ed25519 in extended coordinates, SHA-384 and
//! SHA-512, and the engine's SHA-256. Keys are fixtures/handshake_keys.txt,
//! which fixtures/handshake_keys.py writes with Python's big integers.
//!
//! Only signing is independent. The peer's key exchange, AEAD and transcript
//! hash are td-tls-provider's through td-crypto's policy, rustls runs both
//! ends, and the certificates' own signatures are td-primitives' P-256.
//! The hashes and Ed25519 have known answers here; the ECDSA and RSA
//! encoders have none of their own (RSA's private operation is checked by
//! RSAVP1), so their correctness rests on td-primitives' verifiers, which
//! are anchored on AWS-LC's answers, accepting them.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::needless_range_loop
)]
use crate::certificate_fixtures::{self as f, unhex, TestKey};
use crate::pem::tests::pem;
use crate::{
    ClientConfig, ClockHandle, IdentitySelection, PeerEvidence, ServerConfig, ServerIdentity,
    TlsError, TlsProgress, TlsProtocol, TlsSession, TlsVersion, TrustStore, UtcClock,
    VerificationFailure as V,
};
use rustls::{CipherSuite as C, SignatureAlgorithm, SignatureScheme as S};
use std::io::{Cursor, Read, Write};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};

const NOW: u64 = 1_800_000_000;

// ---- SHA-384 and SHA-512 (FIPS 180-4) ----

const K512: &[u64] = &[
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];
const H512: &[u64] = &[
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];
const H384: &[u64] = &[
    0xcbbb9d5dc1059ed8,
    0x629a292a367cd507,
    0x9159015a3070dd17,
    0x152fecd8f70e5939,
    0x67332667ffc00b31,
    0x8eb44a8768581511,
    0xdb0c2e0d64f98fa7,
    0x47b5481dbefa4fa4,
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hash {
    Sha256,
    Sha384,
    Sha512,
}

impl Hash {
    fn len(self) -> usize {
        match self {
            Hash::Sha256 => 32,
            Hash::Sha384 => 48,
            Hash::Sha512 => 64,
        }
    }

    fn digest(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            Hash::Sha256 => {
                let mut hash = crate::sha256_oracle::Sha256::new();
                for part in parts {
                    hash.update(part);
                }
                hash.finalize().to_vec()
            }
            Hash::Sha384 => sha512(H384, parts)[..48].to_vec(),
            Hash::Sha512 => sha512(H512, parts),
        }
    }

    /// The DER DigestInfo prefix PKCS#1 v1.5 puts before the hash.
    fn digest_info(self) -> &'static [u8] {
        match self {
            Hash::Sha256 => &[
                0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x01, 0x05, 0x00, 0x04, 0x20,
            ],
            Hash::Sha384 => &[
                0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x02, 0x05, 0x00, 0x04, 0x30,
            ],
            Hash::Sha512 => &[
                0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02,
                0x03, 0x05, 0x00, 0x04, 0x40,
            ],
        }
    }
}

fn sha512(initial: &[u64], parts: &[&[u8]]) -> Vec<u8> {
    let mut message: Vec<u8> = parts.concat();
    let bits = (message.len() as u128) * 8;
    message.push(0x80);
    while message.len() % 128 != 112 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_be_bytes());
    let mut state: Vec<u64> = initial.to_vec();
    for block in message.chunks(128) {
        let mut w = [0u64; 80];
        for t in 0..16 {
            w[t] = u64::from_be_bytes(block[8 * t..8 * t + 8].try_into().unwrap());
        }
        for t in 16..80 {
            let s0 = w[t - 15].rotate_right(1) ^ w[t - 15].rotate_right(8) ^ (w[t - 15] >> 7);
            let s1 = w[t - 2].rotate_right(19) ^ w[t - 2].rotate_right(61) ^ (w[t - 2] >> 6);
            w[t] = w[t - 16]
                .wrapping_add(s0)
                .wrapping_add(w[t - 7])
                .wrapping_add(s1);
        }
        let mut v = [0u64; 8];
        v.copy_from_slice(&state);
        for t in 0..80 {
            let s1 = v[4].rotate_right(14) ^ v[4].rotate_right(18) ^ v[4].rotate_right(41);
            let choice = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K512[t])
                .wrapping_add(w[t]);
            let s0 = v[0].rotate_right(28) ^ v[0].rotate_right(34) ^ v[0].rotate_right(39);
            let majority = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(majority);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (word, add) in state.iter_mut().zip(v) {
            *word = word.wrapping_add(add);
        }
    }
    state.iter().flat_map(|word| word.to_be_bytes()).collect()
}

// ---- Montgomery arithmetic modulo an odd number ----

type Limbs = Vec<u32>;

fn from_be(bytes: &[u8], limbs: usize) -> Limbs {
    let mut out = vec![0u32; limbs];
    for (index, &byte) in bytes.iter().rev().enumerate() {
        if index / 4 < limbs {
            out[index / 4] |= u32::from(byte) << (8 * (index % 4));
        } else {
            assert_eq!(byte, 0, "value wider than its limbs");
        }
    }
    out
}

fn to_be(value: &[u32], length: usize) -> Vec<u8> {
    let mut out = vec![0u8; length];
    for index in 0..value.len() * 4 {
        let byte = (value[index / 4] >> (8 * (index % 4))) as u8;
        if index < length {
            out[length - 1 - index] = byte;
        } else {
            assert_eq!(byte, 0, "value wider than its encoding");
        }
    }
    out
}

fn at_least(a: &[u32], b: &[u32]) -> bool {
    for index in (0..a.len()).rev() {
        if a[index] != b[index] {
            return a[index] > b[index];
        }
    }
    true
}

fn subtract(a: &mut [u32], b: &[u32]) -> bool {
    let mut borrow = 0i64;
    for index in 0..a.len() {
        let value = i64::from(a[index]) - i64::from(b[index]) - borrow;
        a[index] = value as u32;
        borrow = i64::from(value < 0);
    }
    borrow != 0
}

fn add(a: &mut [u32], b: &[u32]) -> bool {
    let mut carry = 0u64;
    for index in 0..a.len() {
        let value = u64::from(a[index]) + u64::from(b[index]) + carry;
        a[index] = value as u32;
        carry = value >> 32;
    }
    carry != 0
}

fn is_zero(a: &[u32]) -> bool {
    a.iter().all(|&limb| limb == 0)
}

struct Modulus {
    m: Limbs,
    bytes: usize,
    inverse: u32,
    r2: Limbs,
    one: Limbs,
}

impl Modulus {
    fn new(modulus: &[u8]) -> Self {
        let limbs = modulus.len().div_ceil(4);
        let m = from_be(modulus, limbs);
        assert_eq!(m[0] & 1, 1, "Montgomery needs an odd modulus");
        // Newton's iteration doubles the correct low bits each step.
        let mut inverse = 1u32;
        for _ in 0..5 {
            inverse = inverse.wrapping_mul(2u32.wrapping_sub(m[0].wrapping_mul(inverse)));
        }
        let mut one = vec![0u32; limbs];
        one[0] = 1;
        let mut this = Self {
            m,
            bytes: modulus.len(),
            inverse: inverse.wrapping_neg(),
            r2: Vec::new(),
            one: one.clone(),
        };
        let mut r2 = one;
        for _ in 0..64 * limbs {
            r2 = this.double(&r2);
        }
        this.r2 = r2;
        this
    }

    fn double(&self, a: &[u32]) -> Limbs {
        let mut out = a.to_vec();
        let mut carry = 0u32;
        for limb in out.iter_mut() {
            let next = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        if carry != 0 || at_least(&out, &self.m) {
            subtract(&mut out, &self.m);
        }
        out
    }

    fn add(&self, a: &[u32], b: &[u32]) -> Limbs {
        let mut out = a.to_vec();
        if add(&mut out, b) || at_least(&out, &self.m) {
            subtract(&mut out, &self.m);
        }
        out
    }

    fn sub(&self, a: &[u32], b: &[u32]) -> Limbs {
        let mut out = a.to_vec();
        if subtract(&mut out, b) {
            add(&mut out, &self.m);
        }
        out
    }

    /// a b / R mod m, coarsely integrated operand scanning.
    fn mul(&self, a: &[u32], b: &[u32]) -> Limbs {
        let k = self.m.len();
        let mut t = vec![0u32; k + 2];
        for i in 0..k {
            let mut carry = 0u64;
            for j in 0..k {
                let value = u64::from(t[j]) + u64::from(a[j]) * u64::from(b[i]) + carry;
                t[j] = value as u32;
                carry = value >> 32;
            }
            let value = u64::from(t[k]) + carry;
            t[k] = value as u32;
            t[k + 1] = (value >> 32) as u32;
            let q = t[0].wrapping_mul(self.inverse);
            let value = u64::from(t[0]) + u64::from(q) * u64::from(self.m[0]);
            let mut carry = value >> 32;
            for j in 1..k {
                let value = u64::from(t[j]) + u64::from(q) * u64::from(self.m[j]) + carry;
                t[j - 1] = value as u32;
                carry = value >> 32;
            }
            let value = u64::from(t[k]) + carry;
            t[k - 1] = value as u32;
            t[k] = t[k + 1] + (value >> 32) as u32;
        }
        let mut out = t[..k].to_vec();
        if t[k] != 0 || at_least(&out, &self.m) {
            subtract(&mut out, &self.m);
        }
        out
    }

    /// Any big-endian integer reduced modulo m, bit by bit.
    fn reduce(&self, bytes: &[u8]) -> Limbs {
        let mut out = vec![0u32; self.m.len()];
        for byte in bytes {
            for bit in (0..8).rev() {
                out = self.double(&out);
                if byte >> bit & 1 == 1 {
                    out = self.add(&out, &self.one);
                }
            }
        }
        out
    }

    fn enter(&self, plain: &[u32]) -> Limbs {
        self.mul(plain, &self.r2)
    }

    fn enter_bytes(&self, bytes: &[u8]) -> Limbs {
        self.enter(&self.reduce(bytes))
    }

    fn leave(&self, value: &[u32]) -> Limbs {
        self.mul(value, &self.one)
    }

    fn leave_bytes(&self, value: &[u32]) -> Vec<u8> {
        to_be(&self.leave(value), self.bytes)
    }

    fn pow(&self, base: &[u32], exponent: &[u8]) -> Limbs {
        let mut out = self.enter(&self.one);
        for byte in exponent {
            for bit in (0..8).rev() {
                out = self.mul(&out, &out);
                if byte >> bit & 1 == 1 {
                    out = self.mul(&out, base);
                }
            }
        }
        out
    }

    /// Fermat inversion; every modulus inverted here is prime.
    fn invert(&self, value: &[u32]) -> Limbs {
        let mut exponent = self.m.clone();
        let two = from_be(&[2], exponent.len());
        subtract(&mut exponent, &two);
        self.pow(value, &to_be(&exponent, self.bytes))
    }
}

fn integer(value: &[u8]) -> Vec<u8> {
    let first = value
        .iter()
        .position(|&byte| byte != 0)
        .unwrap_or(value.len() - 1);
    let mut body = Vec::new();
    if value[first] & 0x80 != 0 {
        body.push(0);
    }
    body.extend_from_slice(&value[first..]);
    f::der(2, &body)
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A value no earlier call returned, for nonces and salts.
fn fresh(label: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let count = COUNTER.fetch_add(1, Ordering::Relaxed).to_be_bytes();
    let mut all: Vec<&[u8]> = vec![label, &count];
    all.extend_from_slice(parts);
    let first = Hash::Sha512.digest(&all);
    all.push(b"second half");
    [first, Hash::Sha512.digest(&all)].concat()
}

// ---- ECDSA over the NIST curves (a = -3) ----

type Jacobian = Option<(Limbs, Limbs, Limbs)>;

struct Curve {
    p: Modulus,
    n: Modulus,
    gx: Limbs,
    gy: Limbs,
    oid: &'static [u8],
}

impl Curve {
    fn new(p: &str, n: &str, gx: &str, gy: &str, oid: &'static [u8]) -> Self {
        let p = Modulus::new(&unhex(p));
        let gx = p.enter_bytes(&unhex(gx));
        let gy = p.enter_bytes(&unhex(gy));
        Self {
            n: Modulus::new(&unhex(n)),
            p,
            gx,
            gy,
            oid,
        }
    }

    // dbl-2001-b
    fn double(&self, point: &Jacobian) -> Jacobian {
        let (x, y, z) = point.as_ref()?;
        if is_zero(y) {
            return None;
        }
        let p = &self.p;
        let delta = p.mul(z, z);
        let gamma = p.mul(y, y);
        let beta = p.mul(x, &gamma);
        let t = p.mul(&p.sub(x, &delta), &p.add(x, &delta));
        let alpha = p.add(&p.add(&t, &t), &t);
        let beta4 = p.double(&p.double(&beta));
        let x3 = p.sub(&p.mul(&alpha, &alpha), &p.double(&beta4));
        let yz = p.add(y, z);
        let z3 = p.sub(&p.sub(&p.mul(&yz, &yz), &gamma), &delta);
        let gamma2 = p.mul(&gamma, &gamma);
        let gamma8 = p.double(&p.double(&p.double(&gamma2)));
        let y3 = p.sub(&p.mul(&alpha, &p.sub(&beta4, &x3)), &gamma8);
        Some((x3, y3, z3))
    }

    // add-2007-bl
    fn add(&self, a: &Jacobian, b: &Jacobian) -> Jacobian {
        let Some((x1, y1, z1)) = a else {
            return b.clone();
        };
        let Some((x2, y2, z2)) = b else {
            return a.clone();
        };
        let p = &self.p;
        let z1z1 = p.mul(z1, z1);
        let z2z2 = p.mul(z2, z2);
        let u1 = p.mul(x1, &z2z2);
        let u2 = p.mul(x2, &z1z1);
        let s1 = p.mul(&p.mul(y1, z2), &z2z2);
        let s2 = p.mul(&p.mul(y2, z1), &z1z1);
        let h = p.sub(&u2, &u1);
        let r = p.double(&p.sub(&s2, &s1));
        if is_zero(&h) {
            return if is_zero(&r) { self.double(a) } else { None };
        }
        let h2 = p.double(&h);
        let i = p.mul(&h2, &h2);
        let j = p.mul(&h, &i);
        let v = p.mul(&u1, &i);
        let x3 = p.sub(&p.sub(&p.mul(&r, &r), &j), &p.double(&v));
        let y3 = p.sub(&p.mul(&r, &p.sub(&v, &x3)), &p.double(&p.mul(&s1, &j)));
        let z12 = p.add(z1, z2);
        let z3 = p.mul(&p.sub(&p.sub(&p.mul(&z12, &z12), &z1z1), &z2z2), &h);
        Some((x3, y3, z3))
    }

    /// k G, affine, big-endian coordinates.
    fn base_multiply(&self, k: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        let base = Some((self.gx.clone(), self.gy.clone(), self.p.enter(&self.p.one)));
        let mut out: Jacobian = None;
        for byte in k {
            for bit in (0..8).rev() {
                out = self.double(&out);
                if byte >> bit & 1 == 1 {
                    out = self.add(&out, &base);
                }
            }
        }
        let (x, y, z) = out?;
        let p = &self.p;
        let zi = p.invert(&z);
        let zi2 = p.mul(&zi, &zi);
        let zi3 = p.mul(&zi2, &zi);
        Some((
            p.leave_bytes(&p.mul(&x, &zi2)),
            p.leave_bytes(&p.mul(&y, &zi3)),
        ))
    }

    fn order_bytes(&self) -> usize {
        self.n.bytes
    }

    /// (r, s) as fixed-width big-endian values, FIPS 186-5 6.4.1 with a
    /// nonce fresh per signature.
    fn sign(&self, secret: &[u8], hash: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let n = &self.n;
        // Leftmost bits: every order here is a whole number of bytes, or
        // (P-521) longer than any hash.
        let e = n.enter_bytes(&hash[..hash.len().min(n.bytes)]);
        let d = n.enter_bytes(secret);
        loop {
            let k = n.reduce(&fresh(b"ecdsa nonce", &[secret, hash]));
            if is_zero(&k) {
                continue;
            }
            let (x, _) = self.base_multiply(&to_be(&k, n.bytes)).unwrap();
            let r = n.reduce(&x);
            if is_zero(&r) {
                continue;
            }
            let k_inverse = n.invert(&n.enter(&k));
            let s = n.mul(&k_inverse, &n.add(&e, &n.mul(&n.enter(&r), &d)));
            if is_zero(&s) {
                continue;
            }
            return (to_be(&r, n.bytes), n.leave_bytes(&s));
        }
    }
}

fn ecdsa_der(r: &[u8], s: &[u8]) -> Vec<u8> {
    f::seq(&[integer(r), integer(s)])
}

// ---- Ed25519 (RFC 8032) ----

type Extended = (Limbs, Limbs, Limbs, Limbs);

struct Edwards {
    p: Modulus,
    l: Modulus,
    d2: Limbs,
    base: Extended,
}

impl Edwards {
    fn new() -> Self {
        let p = Modulus::new(&unhex(
            "7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffed",
        ));
        let l = Modulus::new(&unhex(
            "1000000000000000000000000000000014def9dea2f79cd65812631a5cf5d3ed",
        ));
        // d = -121665 / 121666
        let minus = p.sub(&[0; 8], &p.enter(&from_be(&[0x01, 0xdb, 0x41], 8)));
        let d = p.mul(
            &minus,
            &p.invert(&p.enter(&from_be(&[0x01, 0xdb, 0x42], 8))),
        );
        let d2 = p.add(&d, &d);
        let x = p.enter_bytes(&unhex(
            "216936d3cd6e53fec0a4e231fdd6dc5c692cc7609525a7b2c9562d608f25d51a",
        ));
        let y = p.enter_bytes(&unhex(
            "6666666666666666666666666666666666666666666666666666666666666658",
        ));
        let t = p.mul(&x, &y);
        let one = p.enter(&p.one);
        Self {
            base: (x, y, one, t),
            p,
            l,
            d2,
        }
    }

    // add-2008-hwcd-3, unified, so it also doubles.
    fn add(&self, a: &Extended, b: &Extended) -> Extended {
        let p = &self.p;
        let (x1, y1, z1, t1) = a;
        let (x2, y2, z2, t2) = b;
        let a = p.mul(&p.sub(y1, x1), &p.sub(y2, x2));
        let b = p.mul(&p.add(y1, x1), &p.add(y2, x2));
        let c = p.mul(&p.mul(t1, &self.d2), t2);
        let d = p.double(&p.mul(z1, z2));
        let (e, f, g, h) = (p.sub(&b, &a), p.sub(&d, &c), p.add(&d, &c), p.add(&b, &a));
        (p.mul(&e, &f), p.mul(&g, &h), p.mul(&f, &g), p.mul(&e, &h))
    }

    fn base_multiply_encoded(&self, k: &[u8]) -> [u8; 32] {
        let one = self.p.enter(&self.p.one);
        let mut out: Extended = (vec![0; 8], one.clone(), one, vec![0; 8]);
        for byte in k {
            for bit in (0..8).rev() {
                out = self.add(&out, &out);
                if byte >> bit & 1 == 1 {
                    out = self.add(&out, &self.base);
                }
            }
        }
        let p = &self.p;
        let zi = p.invert(&out.2);
        let x = p.leave_bytes(&p.mul(&out.0, &zi));
        let mut y = p.leave_bytes(&p.mul(&out.1, &zi));
        y.reverse();
        y[31] |= (x[31] & 1) << 7;
        y.try_into().unwrap()
    }

    fn reversed(bytes: &[u8]) -> Vec<u8> {
        bytes.iter().rev().copied().collect()
    }

    fn public(&self, seed: &[u8]) -> [u8; 32] {
        self.base_multiply_encoded(&Self::reversed(&Self::scalar(seed)))
    }

    /// The clamped secret scalar, little-endian.
    fn scalar(seed: &[u8]) -> Vec<u8> {
        let mut a = Hash::Sha512.digest(&[seed])[..32].to_vec();
        a[0] &= 248;
        a[31] &= 127;
        a[31] |= 64;
        a
    }

    fn sign(&self, seed: &[u8], message: &[u8]) -> Vec<u8> {
        let l = &self.l;
        let h = Hash::Sha512.digest(&[seed]);
        let a = Self::reversed(&Self::scalar(seed));
        let public = self.base_multiply_encoded(&a);
        let r = l.reduce(&Self::reversed(&Hash::Sha512.digest(&[&h[32..], message])));
        let big_r = self.base_multiply_encoded(&to_be(&r, 32));
        let k = l.reduce(&Self::reversed(
            &Hash::Sha512.digest(&[&big_r, &public, message]),
        ));
        let s = l.add(&r, &l.leave(&l.mul(&l.enter(&k), &l.enter_bytes(&a))));
        [big_r.to_vec(), Self::reversed(&to_be(&s, 32))].concat()
    }
}

// ---- RSA (RFC 8017) ----

struct Rsa {
    n: Vec<u8>,
    e: Vec<u8>,
    modulus: Modulus,
    p: Modulus,
    q: Modulus,
    dp: Vec<u8>,
    dq: Vec<u8>,
    q_inverse: Limbs,
    q_plain: Limbs,
}

/// The full product of two equal-width values.
fn product(a: &[u32], b: &[u32]) -> Limbs {
    let mut out = vec![0u32; a.len() + b.len()];
    for i in 0..a.len() {
        let mut carry = 0u64;
        for j in 0..b.len() {
            let value = u64::from(out[i + j]) + u64::from(a[i]) * u64::from(b[j]) + carry;
            out[i + j] = value as u32;
            carry = value >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    out
}

impl Rsa {
    fn new(fields: &[&str]) -> Self {
        let [n, e, _d, p, q, dp, dq, q_inverse] = fields else {
            panic!("malformed RSA row");
        };
        let p_bytes = unhex(p);
        let q_bytes = unhex(q);
        let p = Modulus::new(&p_bytes);
        let p_plain = from_be(&p_bytes, p.m.len());
        assert_eq!(
            product(&p_plain, &from_be(&q_bytes, p.m.len())),
            from_be(&unhex(n), 2 * p.m.len()),
            "n = p q"
        );
        Self {
            n: unhex(n),
            e: unhex(e),
            modulus: Modulus::new(&unhex(n)),
            q_inverse: from_be(&unhex(q_inverse), p.m.len()),
            q_plain: from_be(&q_bytes, p.m.len()),
            q: Modulus::new(&q_bytes),
            p,
            dp: unhex(dp),
            dq: unhex(dq),
        }
    }

    /// The RSASP1 primitive by CRT, checked against RSAVP1.
    fn private(&self, message: &[u8]) -> Vec<u8> {
        let (p, q) = (&self.p, &self.q);
        let m1 = p.leave(&p.pow(&p.enter_bytes(message), &self.dp));
        let m2 = q.leave(&q.pow(&q.enter_bytes(message), &self.dq));
        // h = q^-1 (m1 - m2) mod p; m2 < q < p.
        let h = p.mul(&p.enter(&p.sub(&m1, &m2)), &self.q_inverse);
        let mut s = product(&h, &self.q_plain);
        let mut m2 = m2;
        m2.resize(s.len(), 0);
        add(&mut s, &m2);
        let signature = to_be(&s, self.n.len());
        let n = &self.modulus;
        let check = n.leave_bytes(&n.pow(&n.enter_bytes(&signature), &self.e));
        assert_eq!(check, message, "RSA signing self-check");
        signature
    }

    fn pkcs1(&self, hash: Hash, message: &[u8]) -> Vec<u8> {
        let info = [hash.digest_info(), &hash.digest(&[message])].concat();
        let mut encoded = vec![0, 1];
        encoded.resize(self.n.len() - info.len() - 1, 0xff);
        encoded.push(0);
        encoded.extend_from_slice(&info);
        self.private(&encoded)
    }

    /// EMSA-PSS with MGF1 over the same hash and a `salt_length`-byte
    /// salt; TLS requires one as long as the hash. The modulus is exactly
    /// 2048 bits.
    fn pss(&self, hash: Hash, salt_length: usize, message: &[u8]) -> Vec<u8> {
        let length = self.n.len();
        let digest = hash.digest(&[message]);
        let salt = fresh(b"pss salt", &[message])[..salt_length].to_vec();
        let h = hash.digest(&[&[0; 8], &digest, &salt]);
        let db_length = length - hash.len() - 1;
        let mut db = vec![0u8; db_length - salt_length - 1];
        db.push(1);
        db.extend_from_slice(&salt);
        let mut mask = Vec::new();
        for counter in 0u32.. {
            if mask.len() >= db_length {
                break;
            }
            mask.extend(hash.digest(&[&h, &counter.to_be_bytes()]));
        }
        for (byte, mask) in db.iter_mut().zip(mask) {
            *byte ^= mask;
        }
        db[0] &= 0x7f;
        db.extend_from_slice(&h);
        db.push(0xbc);
        self.private(&db)
    }
}

// ---- Keys ----

#[derive(Clone, Copy, Debug, PartialEq)]
enum Key {
    Rsa,
    P256,
    P384,
    P521,
    Ed25519,
}

struct EcKey {
    curve: Curve,
    secret: Vec<u8>,
    x: Vec<u8>,
    y: Vec<u8>,
}

struct Keys {
    rsa: Rsa,
    ec: [EcKey; 3],
    edwards: Edwards,
    ed_seed: Vec<u8>,
    ed_public: [u8; 32],
}

const P256: [&str; 4] = [
    "ffffffff00000001000000000000000000000000ffffffffffffffffffffffff",
    "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551",
    "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296",
    "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5",
];
const P384: [&str; 4] = [
    "fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffeffffffff0000000000000000ffffffff",
    "ffffffffffffffffffffffffffffffffffffffffffffffffc7634d81f4372ddf581a0db248b0a77aecec196accc52973",
    "aa87ca22be8b05378eb1c71ef320ad746e1d3b628ba79b9859f741e082542a385502f25dbf55296c3a545e3872760ab7",
    "3617de4a96262c6f5d9e98bf9292dc29f8f41dbd289a147ce9da3113b5f0b8c00a60b1ce1d7e819d7a431d7c90ea0e5f",
];
const P521: [&str; 4] = [
    "01ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
    "01fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffa51868783bf2f966b7fcc0148f709a5d03bb5c9b8899c47aebb6fb71e91386409",
    "00c6858e06b70404e9cd9e3ecb662395b4429c648139053fb521f828af606b4d3dbaa14b5e77efe75928fe1dc127a2ffa8de3348b3c1856a429bf97e7e31c2e5bd66",
    "011839296a789a3bc0045c8a5fb42c7d1bd998f54449579b446817afbd17273e662c97ee72995ef42640c550b9013fad0761353c7086a272c24088be94769fd16650",
];

/// The fixture keys, each public half recomputed here and required to
/// equal Python's.
fn keys() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let rows: Vec<Vec<&str>> = include_str!("fixtures/handshake_keys.txt")
            .lines()
            .map(|line| line.split(' ').collect())
            .collect();
        let row = |name: &str| -> &[&str] { &rows.iter().find(|row| row[0] == name).unwrap()[1..] };
        let ec = |name: &str, [p, n, gx, gy]: [&str; 4], oid: &'static [u8]| {
            let curve = Curve::new(p, n, gx, gy, oid);
            let fields = row(name);
            let secret = unhex(fields[0]);
            let (x, y) = curve.base_multiply(&secret).unwrap();
            assert_eq!(
                (x.clone(), y.clone()),
                (unhex(fields[1]), unhex(fields[2])),
                "{name}"
            );
            EcKey {
                curve,
                secret,
                x,
                y,
            }
        };
        let edwards = Edwards::new();
        let ed = row("ED25519");
        let ed_seed = unhex(ed[0]);
        let ed_public = edwards.public(&ed_seed);
        assert_eq!(ed_public.to_vec(), unhex(ed[1]));
        let rsa = Rsa::new(row("RSA"));
        assert_eq!(rsa.n.len(), 256);
        assert!(rsa.n[0] & 0x80 != 0);
        Keys {
            rsa,
            ec: [
                ec("P256", P256, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
                ec("P384", P384, &[0x2b, 0x81, 0x04, 0x00, 0x22]),
                ec("P521", P521, &[0x2b, 0x81, 0x04, 0x00, 0x23]),
            ],
            edwards,
            ed_seed,
            ed_public,
        }
    })
}

impl Key {
    fn ec(self) -> &'static EcKey {
        let keys = keys();
        match self {
            Key::P256 => &keys.ec[0],
            Key::P384 => &keys.ec[1],
            Key::P521 => &keys.ec[2],
            _ => panic!("{self:?} is not an EC key"),
        }
    }

    fn algorithm(self) -> SignatureAlgorithm {
        match self {
            Key::Rsa => SignatureAlgorithm::RSA,
            Key::Ed25519 => SignatureAlgorithm::ED25519,
            _ => SignatureAlgorithm::ECDSA,
        }
    }

    fn spki(self) -> Vec<u8> {
        let keys = keys();
        let (algorithm, public) = match self {
            Key::Rsa => (
                f::seq(&[
                    f::oid(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 1, 1, 1]),
                    f::der(5, &[]),
                ]),
                f::seq(&[integer(&keys.rsa.n), integer(&keys.rsa.e)]),
            ),
            Key::Ed25519 => (
                f::seq(&[f::oid(&[0x2b, 0x65, 0x70])]),
                keys.ed_public.to_vec(),
            ),
            _ => {
                let key = self.ec();
                (
                    f::seq(&[
                        f::oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
                        f::oid(key.curve.oid),
                    ]),
                    [&[4u8][..], &key.x, &key.y].concat(),
                )
            }
        };
        f::seq(&[algorithm, f::der(3, &[&[0u8][..], &public].concat())])
    }
}

// ---- The peer's signer ----

#[derive(Clone, Copy, Debug, PartialEq)]
enum Method {
    Pkcs1(Hash),
    Pss(Hash),
    /// PSS with a salt of this many bytes.
    PssSalt(Hash, usize),
    Ecdsa(Hash),
    Ed25519,
}

/// Which signature bit to flip: none, in r (the RSA value's last byte,
/// Ed25519's R), or in s (the RSA value's first byte, Ed25519's S).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Corrupt {
    No,
    First,
    Second,
}

#[derive(Clone, Copy, Debug)]
struct Plan {
    key: Key,
    claim: S,
    method: Method,
    algorithm: SignatureAlgorithm,
    corrupt: Corrupt,
}

impl Plan {
    /// The key signing as `scheme` says.
    fn honest(key: Key, scheme: S) -> Self {
        let method = match scheme {
            S::RSA_PKCS1_SHA256 => Method::Pkcs1(Hash::Sha256),
            S::RSA_PKCS1_SHA384 => Method::Pkcs1(Hash::Sha384),
            S::RSA_PKCS1_SHA512 => Method::Pkcs1(Hash::Sha512),
            S::RSA_PSS_SHA256 => Method::Pss(Hash::Sha256),
            S::RSA_PSS_SHA384 => Method::Pss(Hash::Sha384),
            S::RSA_PSS_SHA512 => Method::Pss(Hash::Sha512),
            S::ECDSA_NISTP256_SHA256 => Method::Ecdsa(Hash::Sha256),
            S::ECDSA_NISTP384_SHA384 => Method::Ecdsa(Hash::Sha384),
            S::ECDSA_NISTP521_SHA512 => Method::Ecdsa(Hash::Sha512),
            S::ED25519 => Method::Ed25519,
            _ => panic!("no honest method for {scheme:?}"),
        };
        Self {
            key,
            claim: scheme,
            method,
            algorithm: key.algorithm(),
            corrupt: Corrupt::No,
        }
    }

    /// The key signing by `method` while the handshake says `claim`.
    fn claiming(key: Key, claim: S, method: Method) -> Self {
        Self {
            key,
            claim,
            method,
            algorithm: key.algorithm(),
            corrupt: Corrupt::No,
        }
    }

    fn corrupt(mut self, corrupt: Corrupt) -> Self {
        self.corrupt = corrupt;
        self
    }

    /// Advertise another algorithm, so rustls's own suite choice follows it.
    fn advertise(mut self, algorithm: SignatureAlgorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        let keys = keys();
        let flip = |value: &mut [u8], which: Corrupt| {
            if self.corrupt == which {
                *value.last_mut().unwrap() ^= 1;
            }
        };
        let rsa = |mut signature: Vec<u8>| {
            assert_eq!(self.key, Key::Rsa);
            flip(&mut signature, Corrupt::First);
            if self.corrupt == Corrupt::Second {
                signature[0] ^= 1;
            }
            signature
        };
        match self.method {
            Method::Pkcs1(hash) => rsa(keys.rsa.pkcs1(hash, message)),
            Method::Pss(hash) => rsa(keys.rsa.pss(hash, hash.len(), message)),
            Method::PssSalt(hash, salt) => rsa(keys.rsa.pss(hash, salt, message)),
            Method::Ecdsa(hash) => {
                let key = self.key.ec();
                let (mut r, mut s) = key.curve.sign(&key.secret, &hash.digest(&[message]));
                flip(&mut r, Corrupt::First);
                flip(&mut s, Corrupt::Second);
                ecdsa_der(&r, &s)
            }
            Method::Ed25519 => {
                assert_eq!(self.key, Key::Ed25519);
                let mut signature = keys.edwards.sign(&keys.ed_seed, message);
                let (r, s) = signature.split_at_mut(32);
                flip(r, Corrupt::First);
                // S is little-endian: flip its least significant bit.
                if self.corrupt == Corrupt::Second {
                    s[0] ^= 1;
                }
                signature
            }
        }
    }
}

#[derive(Debug, Default)]
struct Log {
    offered: Vec<Vec<S>>,
    signatures: usize,
}

#[derive(Debug)]
struct PeerKey {
    plan: Plan,
    log: Arc<Mutex<Log>>,
}

#[derive(Debug)]
struct PeerSigner {
    plan: Plan,
    log: Arc<Mutex<Log>>,
}

impl rustls::sign::SigningKey for PeerKey {
    // Signs whatever is offered, so a test controls the claimed scheme.
    fn choose_scheme(&self, offered: &[S]) -> Option<Box<dyn rustls::sign::Signer>> {
        self.log.lock().unwrap().offered.push(offered.to_vec());
        Some(Box::new(PeerSigner {
            plan: self.plan,
            log: self.log.clone(),
        }))
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        self.plan.algorithm
    }
}

impl rustls::sign::Signer for PeerSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        self.log.lock().unwrap().signatures += 1;
        Ok(self.plan.sign(message))
    }

    fn scheme(&self) -> S {
        self.plan.claim
    }
}

// ---- Certificates, configurations and transport ----

struct Clock;
impl UtcClock for Clock {
    fn now(&self) -> Option<u64> {
        Some(NOW)
    }
}

#[derive(Debug)]
struct Time;
impl rustls::time_provider::TimeProvider for Time {
    fn current_time(&self) -> Option<rustls::pki_types::UnixTime> {
        Some(rustls::pki_types::UnixTime::since_unix_epoch(
            std::time::Duration::from_secs(NOW),
        ))
    }
}

#[derive(Debug)]
struct ServerResolves(Arc<rustls::sign::CertifiedKey>);
impl rustls::server::ResolvesServerCert for ServerResolves {
    fn resolve(
        &self,
        _hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }
}

#[derive(Debug)]
struct ClientResolves(Arc<rustls::sign::CertifiedKey>);
impl rustls::client::ResolvesClientCert for ClientResolves {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[S]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// A P-256 root (td-primitives signs the certificates; only the handshake
/// signatures are under test) and its trust store.
struct Authority {
    key: TestKey,
    root: Vec<u8>,
    trust: TrustStore,
}

fn authority() -> Authority {
    let key = TestKey::generate();
    let root = f::make(&key, &key, &f::Parameters::new(true)).unwrap();
    let trust = TrustStore::from_pem(&pem("CERTIFICATE", &root)).unwrap();
    Authority { key, root, trust }
}

/// A `localhost` leaf for `key`, for server or client authentication.
fn leaf(authority: &Authority, key: Key, client: bool) -> Vec<u8> {
    let mut parameters = f::Parameters::new(false);
    if client {
        parameters.extensions[3] =
            f::extension(0x25, false, f::seq(&[f::oid(&[0x2b, 6, 1, 5, 5, 7, 3, 2])]));
    }
    f::build(key.spki(), f::signature_algorithm(), &parameters, |body| {
        authority.key.sign(body)
    })
    .unwrap()
}

fn certified(
    certificate: Vec<u8>,
    plan: Plan,
    log: &Arc<Mutex<Log>>,
) -> Arc<rustls::sign::CertifiedKey> {
    Arc::new(rustls::sign::CertifiedKey::new(
        vec![certificate.into()],
        Arc::new(PeerKey {
            plan,
            log: log.clone(),
        }),
    ))
}

/// td-crypto's policy provider, its suites cut to `suite` when given: the
/// peer's key exchange and AEAD.
fn peer_provider(suite: Option<C>) -> Arc<rustls::crypto::CryptoProvider> {
    let mut provider = crate::tls_policy::provider().unwrap();
    if let Some(suite) = suite {
        provider
            .cipher_suites
            .retain(|candidate| candidate.suite() == suite);
        assert_eq!(provider.cipher_suites.len(), 1);
    }
    Arc::new(provider)
}

#[derive(Debug, PartialEq)]
enum Failure {
    Local(TlsError),
    Peer(rustls::Error),
}

fn drain(session: &mut TlsSession, peer: &mut rustls::Connection) -> Result<(), Failure> {
    let mut buffer = [0u8; 4096];
    while session.status().ciphertext_pending != 0 {
        let count = session
            .drain_ciphertext(&mut buffer)
            .map_err(Failure::Local)?;
        let mut input = Cursor::new(&buffer[..count]);
        while (input.position() as usize) < count {
            assert!(peer.read_tls(&mut input).unwrap() > 0);
            peer.process_new_packets().map_err(Failure::Peer)?;
        }
    }
    Ok(())
}

fn pump(session: &mut TlsSession, peer: &mut rustls::Connection) -> Result<(), Failure> {
    drain(session, peer)?;
    let mut wire = Vec::new();
    while peer.wants_write() {
        peer.write_tls(&mut wire).unwrap();
    }
    let mut offset = 0;
    while offset < wire.len() {
        let end =
            offset + 5 + usize::from(u16::from_be_bytes([wire[offset + 3], wire[offset + 4]]));
        loop {
            match session
                .receive_record(&wire[offset..end], false)
                .map_err(Failure::Local)?
            {
                TlsProgress::Bytes(count) => {
                    assert_eq!(count, end - offset);
                    break;
                }
                TlsProgress::Blocked(crate::BlockedOn::DrainCiphertext) => drain(session, peer)?,
                other => panic!("unexpected record progress {other:?}"),
            }
        }
        offset = end;
        drain(session, peer)?;
    }
    Ok(())
}

fn handshake(session: &mut TlsSession, peer: &mut rustls::Connection) -> Result<(), Failure> {
    for _ in 0..32 {
        pump(session, peer)?;
        if session.status().handshake.is_some()
            && !peer.is_handshaking()
            && !peer.wants_write()
            && session.status().ciphertext_pending == 0
        {
            return Ok(());
        }
    }
    panic!("handshake turn ceiling")
}

/// Application data both ways over an established pair.
fn exchange(session: &mut TlsSession, peer: &mut rustls::Connection) {
    let outbound = b"from td-crypto";
    assert_eq!(
        session.queue_plaintext(outbound),
        Ok(TlsProgress::Bytes(outbound.len()))
    );
    drain(session, peer).unwrap();
    let mut received = [0u8; 14];
    peer.reader().read_exact(&mut received).unwrap();
    assert_eq!(&received, outbound);
    let inbound = b"from the peer";
    peer.writer().write_all(inbound).unwrap();
    pump(session, peer).unwrap();
    let mut received = [0u8; 13];
    assert_eq!(
        session.read_plaintext(&mut received),
        Ok(TlsProgress::Bytes(inbound.len()))
    );
    assert_eq!(&received, inbound);
}

#[derive(Debug)]
struct Negotiated {
    version: TlsVersion,
    suite: C,
    group: rustls::NamedGroup,
    offered: Vec<S>,
    signatures: usize,
    peer: PeerEvidence,
}

fn version_of(version: &rustls::SupportedProtocolVersion) -> TlsVersion {
    if version.version == rustls::ProtocolVersion::TLSv1_2 {
        TlsVersion::V12
    } else {
        TlsVersion::V13
    }
}

fn negotiated(
    session: &mut TlsSession,
    peer: &mut rustls::Connection,
    log: &Arc<Mutex<Log>>,
) -> Result<Negotiated, Failure> {
    handshake(session, peer)?;
    exchange(session, peer);
    let info = session.status().handshake.unwrap();
    let log = log.lock().unwrap();
    assert_eq!(log.offered.len(), 1);
    Ok(Negotiated {
        version: info.version,
        suite: peer.negotiated_cipher_suite().unwrap().suite(),
        group: peer.negotiated_key_exchange_group().unwrap().name(),
        offered: log.offered[0].clone(),
        signatures: log.signatures,
        peer: info.peer,
    })
}

/// td-crypto's outbound session against a peer server signing by `plan`.
fn as_client(
    version: &'static rustls::SupportedProtocolVersion,
    suite: Option<C>,
    plan: Plan,
    certificate_key: Key,
) -> Result<Negotiated, Failure> {
    let authority = authority();
    let log = Arc::new(Mutex::new(Log::default()));
    let key = certified(leaf(&authority, certificate_key, false), plan, &log);
    let server = rustls::ServerConfig::builder_with_details(peer_provider(suite), Arc::new(Time))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(ServerResolves(key)));
    let mut peer =
        rustls::Connection::Server(rustls::ServerConnection::new(Arc::new(server)).unwrap());
    let config = ClientConfig::new(
        &authority.trust,
        Arc::new(ClockHandle::new(Clock)),
        TlsProtocol::Smtp,
    )
    .unwrap();
    let mut session = TlsSession::client(Arc::new(config), "localhost").unwrap();
    let result = negotiated(&mut session, &mut peer, &log);
    if result.is_err() {
        assert_eq!(session.status().handshake, None);
    }
    result
}

/// td-crypto's inbound session, requiring a client certificate under the
/// authority, against a peer client proving its key by `plan`.
fn as_server(
    version: &'static rustls::SupportedProtocolVersion,
    plan: Plan,
    certificate_key: Key,
) -> Result<(Negotiated, Vec<u8>), Failure> {
    let authority = authority();
    let identity_key = crate::certificate_fixtures::TestKey::generate_pkcs8().unwrap();
    let identity_leaf = f::make(
        &TestKey::from_pkcs8(&identity_key).unwrap(),
        &authority.key,
        &f::Parameters::new(false),
    )
    .unwrap();
    let identity = ServerIdentity::from_pem(
        &[
            pem("CERTIFICATE", &identity_leaf),
            pem("CERTIFICATE", &authority.root),
        ]
        .concat(),
        &pem("PRIVATE KEY", &identity_key),
        &["localhost"],
        Some(NOW),
    )
    .unwrap();
    let config = ServerConfig::new(
        &[Arc::new(identity)],
        TlsProtocol::Smtp,
        IdentitySelection::MatchPresentName,
        Some(&authority.trust),
        Arc::new(ClockHandle::new(Clock)),
    )
    .unwrap();
    let mut session = TlsSession::server(Arc::new(config)).unwrap();
    let log = Arc::new(Mutex::new(Log::default()));
    let client_leaf = leaf(&authority, certificate_key, true);
    let key = certified(client_leaf.clone(), plan, &log);
    let client = rustls::ClientConfig::builder_with_details(peer_provider(None), Arc::new(Time))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_root_certificates(authority.trust.roots.clone())
        .with_client_cert_resolver(Arc::new(ClientResolves(key)));
    let mut peer = rustls::Connection::Client(
        rustls::ClientConnection::new(Arc::new(client), "localhost".try_into().unwrap()).unwrap(),
    );
    let result = negotiated(&mut session, &mut peer, &log);
    if result.is_err() {
        assert_eq!(session.status().handshake, None);
    }
    result.map(|negotiated| (negotiated, client_leaf))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Side {
    Client,
    Server,
}

/// The rustls error behind a refusal: the same handshake between plain
/// rustls connections, the verifying side holding td-crypto's policy
/// provider and webpki verifier as its configurations do, and the other
/// side signing by `plan`. td-crypto maps this error to its own.
fn native_refusal(
    verifier: Side,
    version: &'static rustls::SupportedProtocolVersion,
    suite: Option<C>,
    plan: Plan,
    certificate_key: Key,
) -> rustls::Error {
    let authority = authority();
    let log = Arc::new(Mutex::new(Log::default()));
    let policy = Arc::new(crate::tls_policy::provider().unwrap());
    let roots = Arc::new(authority.trust.roots.clone());
    let (client, server) = match verifier {
        Side::Client => {
            let key = certified(leaf(&authority, certificate_key, false), plan, &log);
            let server =
                rustls::ServerConfig::builder_with_details(peer_provider(suite), Arc::new(Time))
                    .with_protocol_versions(&[version])
                    .unwrap()
                    .with_no_client_auth()
                    .with_cert_resolver(Arc::new(ServerResolves(key)));
            let client = rustls::ClientConfig::builder_with_details(policy, Arc::new(Time))
                .with_protocol_versions(&[version])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            (client, server)
        }
        Side::Server => {
            let identity = TestKey::generate_pkcs8().unwrap();
            let identity_leaf = f::make(
                &TestKey::from_pkcs8(&identity).unwrap(),
                &authority.key,
                &f::Parameters::new(false),
            )
            .unwrap();
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                roots.clone(),
                policy.clone(),
            )
            .build()
            .unwrap();
            let server = rustls::ServerConfig::builder_with_details(policy, Arc::new(Time))
                .with_protocol_versions(&[version])
                .unwrap()
                .with_client_cert_verifier(verifier)
                .with_single_cert(
                    vec![identity_leaf.into()],
                    rustls::pki_types::PrivatePkcs8KeyDer::from(identity).into(),
                )
                .unwrap();
            let key = certified(leaf(&authority, certificate_key, true), plan, &log);
            let client =
                rustls::ClientConfig::builder_with_details(peer_provider(suite), Arc::new(Time))
                    .with_protocol_versions(&[version])
                    .unwrap()
                    .with_root_certificates(roots)
                    .with_client_cert_resolver(Arc::new(ClientResolves(key)));
            (client, server)
        }
    };
    let mut client =
        rustls::ClientConnection::new(Arc::new(client), "localhost".try_into().unwrap()).unwrap();
    let mut server = rustls::ServerConnection::new(Arc::new(server)).unwrap();
    for _ in 0..32 {
        let mut wire = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut wire).unwrap();
        }
        let mut input = Cursor::new(&wire);
        while (input.position() as usize) < wire.len() {
            server.read_tls(&mut input).unwrap();
            if let Err(error) = server.process_new_packets() {
                assert_eq!(verifier, Side::Server, "{error:?}");
                return error;
            }
        }
        let mut wire = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut wire).unwrap();
        }
        let mut input = Cursor::new(&wire);
        while (input.position() as usize) < wire.len() {
            client.read_tls(&mut input).unwrap();
            if let Err(error) = client.process_new_packets() {
                assert_eq!(verifier, Side::Client, "{error:?}");
                return error;
            }
        }
    }
    panic!("{plan:?} was not refused")
}

const TLS12: &rustls::SupportedProtocolVersion = &rustls::version::TLS12;
const TLS13: &rustls::SupportedProtocolVersion = &rustls::version::TLS13;

fn assert_client_accepts(
    version: &'static rustls::SupportedProtocolVersion,
    suite: C,
    plan: Plan,
) -> Negotiated {
    let outcome = as_client(version, Some(suite), plan, plan.key)
        .unwrap_or_else(|failure| panic!("{plan:?} {suite:?}: {failure:?}"));
    assert_eq!(outcome.version, version_of(version), "{plan:?}");
    assert_eq!(outcome.suite, suite, "{plan:?}");
    assert_eq!(outcome.group, rustls::NamedGroup::X25519, "{plan:?}");
    assert_eq!(outcome.signatures, 1, "{plan:?}");
    assert_eq!(outcome.peer, PeerEvidence::VerifiedServerName);
    outcome
}

// ---- Tests ----

#[test]
fn independent_signers_reproduce_known_answers() {
    assert_eq!(
        Hash::Sha384.digest(&[b"abc"]),
        unhex("cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7")
    );
    assert_eq!(
        Hash::Sha512.digest(&[b"ab", b"c"]),
        unhex("ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f")
    );
    // RFC 8032 7.1, test 1.
    let edwards = &keys().edwards;
    let seed = unhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
    assert_eq!(
        edwards.public(&seed).to_vec(),
        unhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
    );
    assert_eq!(
        edwards.sign(&seed, b""),
        unhex("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b")
    );
    // Fresh nonces: one key signing one message twice gives two signatures.
    let key = Key::P384.ec();
    let digest = Hash::Sha384.digest(&[b"twice"]);
    assert_ne!(
        key.curve.sign(&key.secret, &digest),
        key.curve.sign(&key.secret, &digest)
    );
    assert_eq!(key.curve.order_bytes(), 48);
}

#[test]
fn td_client_verifies_independent_rsa_handshakes_in_tls12() {
    for suite in [
        C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        C::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ] {
        for scheme in [
            S::RSA_PKCS1_SHA256,
            S::RSA_PKCS1_SHA384,
            S::RSA_PKCS1_SHA512,
            S::RSA_PSS_SHA256,
            S::RSA_PSS_SHA384,
            S::RSA_PSS_SHA512,
        ] {
            let outcome = assert_client_accepts(TLS12, suite, Plan::honest(Key::Rsa, scheme));
            assert!(outcome.offered.contains(&scheme), "{scheme:?}");
        }
    }
}

#[test]
fn td_client_verifies_independent_ecdsa_and_ed25519_handshakes_in_tls12() {
    for (key, scheme, suite) in [
        (
            Key::P256,
            S::ECDSA_NISTP256_SHA256,
            C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        ),
        (
            Key::P384,
            S::ECDSA_NISTP384_SHA384,
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ),
        (
            Key::P521,
            S::ECDSA_NISTP521_SHA512,
            C::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        ),
        (
            Key::Ed25519,
            S::ED25519,
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ),
    ] {
        let outcome = assert_client_accepts(TLS12, suite, Plan::honest(key, scheme));
        assert!(outcome.offered.contains(&scheme), "{scheme:?}");
    }
}

#[test]
fn td_client_verifies_every_admitted_tls13_scheme() {
    for (index, (key, scheme)) in [
        (Key::P256, S::ECDSA_NISTP256_SHA256),
        (Key::P384, S::ECDSA_NISTP384_SHA384),
        (Key::P521, S::ECDSA_NISTP521_SHA512),
        (Key::Ed25519, S::ED25519),
        (Key::Rsa, S::RSA_PSS_SHA256),
        (Key::Rsa, S::RSA_PSS_SHA384),
        (Key::Rsa, S::RSA_PSS_SHA512),
    ]
    .into_iter()
    .enumerate()
    {
        let suite = [
            C::TLS13_AES_256_GCM_SHA384,
            C::TLS13_AES_128_GCM_SHA256,
            C::TLS13_CHACHA20_POLY1305_SHA256,
        ][index % 3];
        let outcome = assert_client_accepts(TLS13, suite, Plan::honest(key, scheme));
        assert!(outcome.offered.contains(&scheme), "{scheme:?}");
    }
}

/// Why a handshake signature is refused: rustls's own error, and td-crypto's
/// fixed mapping of it.
#[derive(Clone, Copy, Debug)]
enum Reason {
    /// webpki's signature check failed.
    BadSignature,
    /// The scheme's first verifier (TLS 1.3) or every verifier (TLS 1.2)
    /// names another key algorithm than the certificate's.
    KeyAlgorithm,
    /// A scheme the verifier did not advertise, or not one TLS 1.3 signs
    /// handshakes with.
    Unadvertised,
    /// A TLS 1.2 key exchange signed by another algorithm than the suite's.
    SuiteAlgorithm,
}

impl Reason {
    fn local(self) -> TlsError {
        match self {
            Reason::BadSignature => TlsError::Verification(V::Signature),
            Reason::KeyAlgorithm => TlsError::Verification(V::Other),
            Reason::Unadvertised | Reason::SuiteAlgorithm => TlsError::Protocol,
        }
    }

    fn native(self, error: &rustls::Error) -> bool {
        use rustls::{CertificateError as E, PeerMisbehaved as P};
        match self {
            Reason::BadSignature => {
                matches!(error, rustls::Error::InvalidCertificate(E::BadSignature))
            }
            Reason::KeyAlgorithm => matches!(
                error,
                rustls::Error::InvalidCertificate(
                    E::UnsupportedSignatureAlgorithmForPublicKeyContext { .. }
                )
            ),
            Reason::Unadvertised => matches!(
                error,
                rustls::Error::PeerMisbehaved(P::SignedHandshakeWithUnadvertisedSigScheme)
            ),
            Reason::SuiteAlgorithm => matches!(
                error,
                rustls::Error::PeerMisbehaved(P::SignedKxWithWrongAlgorithm)
            ),
        }
    }
}

/// td-crypto's session on `verifier`'s side refuses the peer's signature
/// with `reason`'s error, and plain rustls with td-crypto's policy refuses
/// the same handshake for that reason.
fn assert_refused(
    verifier: Side,
    version: &'static rustls::SupportedProtocolVersion,
    suite: Option<C>,
    plan: Plan,
    certificate_key: Key,
    reason: Reason,
) {
    let local = match verifier {
        Side::Client => as_client(version, suite, plan, certificate_key).map(|_| ()),
        Side::Server => as_server(version, plan, certificate_key).map(|_| ()),
    };
    assert_eq!(local, Err(Failure::Local(reason.local())), "{plan:?}");
    let native = native_refusal(verifier, version, suite, plan, certificate_key);
    assert!(reason.native(&native), "{plan:?}: {native:?}");
}

fn ecdsa(hash: Hash) -> Method {
    Method::Ecdsa(hash)
}

#[test]
fn td_client_refuses_corrupted_handshake_signatures() {
    for (version, suite, plan) in [
        (
            TLS12,
            C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
            Plan::honest(Key::Rsa, S::RSA_PKCS1_SHA256),
        ),
        (
            TLS12,
            C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            Plan::honest(Key::Rsa, S::RSA_PSS_SHA384),
        ),
        (
            TLS12,
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
            Plan::honest(Key::P384, S::ECDSA_NISTP384_SHA384),
        ),
        (
            TLS12,
            C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            Plan::honest(Key::Ed25519, S::ED25519),
        ),
        (
            TLS13,
            C::TLS13_AES_128_GCM_SHA256,
            Plan::honest(Key::P256, S::ECDSA_NISTP256_SHA256),
        ),
        (
            TLS13,
            C::TLS13_AES_256_GCM_SHA384,
            Plan::honest(Key::P521, S::ECDSA_NISTP521_SHA512),
        ),
        (
            TLS13,
            C::TLS13_CHACHA20_POLY1305_SHA256,
            Plan::honest(Key::Ed25519, S::ED25519),
        ),
        (
            TLS13,
            C::TLS13_AES_256_GCM_SHA384,
            Plan::honest(Key::Rsa, S::RSA_PSS_SHA512),
        ),
    ] {
        for corrupt in [Corrupt::First, Corrupt::Second] {
            let plan = plan.corrupt(corrupt);
            assert_refused(
                Side::Client,
                version,
                Some(suite),
                plan,
                plan.key,
                Reason::BadSignature,
            );
        }
    }
}

#[test]
fn td_client_tls13_binds_curve_hash_and_scheme() {
    use Reason::{BadSignature as Bad, KeyAlgorithm as Key_, Unadvertised as Not};
    for (plan, reason) in [
        // A key on one curve claiming another curve's scheme.
        (
            Plan::claiming(Key::P384, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha256)),
            Key_,
        ),
        (
            Plan::claiming(Key::P256, S::ECDSA_NISTP384_SHA384, ecdsa(Hash::Sha384)),
            Key_,
        ),
        (
            Plan::claiming(Key::P521, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha256)),
            Key_,
        ),
        (
            Plan::claiming(Key::P521, S::ECDSA_NISTP384_SHA384, ecdsa(Hash::Sha384)),
            Key_,
        ),
        (
            Plan::claiming(Key::P256, S::ECDSA_NISTP521_SHA512, ecdsa(Hash::Sha512)),
            Key_,
        ),
        // The right curve and scheme over another hash.
        (
            Plan::claiming(Key::P256, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha384)),
            Bad,
        ),
        (
            Plan::claiming(Key::Rsa, S::RSA_PSS_SHA256, Method::Pss(Hash::Sha384)),
            Bad,
        ),
        // PSS claimed, PKCS#1 v1.5 padding sent.
        (
            Plan::claiming(Key::Rsa, S::RSA_PSS_SHA256, Method::Pkcs1(Hash::Sha256)),
            Bad,
        ),
        // PSS with a salt other than the hash's length.
        (
            Plan::claiming(
                Key::Rsa,
                S::RSA_PSS_SHA256,
                Method::PssSalt(Hash::Sha256, 0),
            ),
            Bad,
        ),
        (
            Plan::claiming(
                Key::Rsa,
                S::RSA_PSS_SHA384,
                Method::PssSalt(Hash::Sha384, 49),
            ),
            Bad,
        ),
        // PKCS#1 v1.5, which TLS 1.3 does not sign handshakes with.
        (Plan::honest(Key::Rsa, S::RSA_PKCS1_SHA256), Not),
        // Another key type's scheme.
        (
            Plan::claiming(Key::Ed25519, S::ECDSA_NISTP256_SHA256, Method::Ed25519),
            Key_,
        ),
        (
            Plan::claiming(Key::P256, S::ED25519, ecdsa(Hash::Sha512)),
            Key_,
        ),
        (
            Plan::claiming(Key::P256, S::RSA_PSS_SHA256, ecdsa(Hash::Sha256)),
            Key_,
        ),
        // Schemes the client never offered.
        (
            Plan::claiming(Key::P256, S::ECDSA_SHA1_Legacy, ecdsa(Hash::Sha256)),
            Not,
        ),
        (
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA1, Method::Pkcs1(Hash::Sha256)),
            Not,
        ),
        (
            Plan::claiming(Key::P256, S::ED448, ecdsa(Hash::Sha256)),
            Not,
        ),
    ] {
        let suite = Some(C::TLS13_AES_128_GCM_SHA256);
        assert_refused(Side::Client, TLS13, suite, plan, plan.key, reason);
    }
}

/// TLS 1.2 ECDSA schemes name a hash, not a curve: td-crypto's mapping
/// (TLS.md, ADMITTED_MAPPINGS) lets an ECDSA key on any admitted curve sign
/// under any admitted ECDSA scheme. This pins that contract, and the
/// refusals TLS 1.2 does make.
#[test]
fn td_client_tls12_binds_hash_not_curve() {
    let ecdsa_suite = C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256;
    let rsa_suite = C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256;
    for plan in [
        Plan::claiming(Key::P384, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha256)),
        Plan::claiming(Key::P256, S::ECDSA_NISTP384_SHA384, ecdsa(Hash::Sha384)),
        Plan::claiming(Key::P521, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha256)),
        Plan::claiming(Key::P384, S::ECDSA_NISTP521_SHA512, ecdsa(Hash::Sha512)),
    ] {
        assert_client_accepts(TLS12, ecdsa_suite, plan);
    }
    for (suite, plan, certificate, reason) in [
        // The scheme's hash still binds.
        (
            ecdsa_suite,
            Plan::claiming(Key::P256, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha384)),
            Key::P256,
            Reason::BadSignature,
        ),
        // RSA: the scheme's hash and padding bind, and PSS's salt length.
        (
            rsa_suite,
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA256, Method::Pkcs1(Hash::Sha384)),
            Key::Rsa,
            Reason::BadSignature,
        ),
        (
            rsa_suite,
            Plan::claiming(Key::Rsa, S::RSA_PSS_SHA256, Method::Pkcs1(Hash::Sha256)),
            Key::Rsa,
            Reason::BadSignature,
        ),
        (
            rsa_suite,
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA256, Method::Pss(Hash::Sha256)),
            Key::Rsa,
            Reason::BadSignature,
        ),
        (
            rsa_suite,
            Plan::claiming(
                Key::Rsa,
                S::RSA_PSS_SHA512,
                Method::PssSalt(Hash::Sha512, 0),
            ),
            Key::Rsa,
            Reason::BadSignature,
        ),
        (
            rsa_suite,
            Plan::claiming(
                Key::Rsa,
                S::RSA_PSS_SHA256,
                Method::PssSalt(Hash::Sha256, 33),
            ),
            Key::Rsa,
            Reason::BadSignature,
        ),
        // An RSA signature in an ECDHE-ECDSA suite, and the reverse.
        (
            ecdsa_suite,
            Plan::honest(Key::Rsa, S::RSA_PKCS1_SHA256).advertise(SignatureAlgorithm::ECDSA),
            Key::Rsa,
            Reason::SuiteAlgorithm,
        ),
        (
            rsa_suite,
            Plan::honest(Key::P256, S::ECDSA_NISTP256_SHA256).advertise(SignatureAlgorithm::RSA),
            Key::P256,
            Reason::SuiteAlgorithm,
        ),
        // A scheme of the suite's kind that does not fit the certificate key.
        (
            ecdsa_suite,
            Plan::claiming(Key::P256, S::ED25519, ecdsa(Hash::Sha512)),
            Key::P256,
            Reason::KeyAlgorithm,
        ),
        // Schemes the client never offered.
        (
            ecdsa_suite,
            Plan::claiming(Key::P256, S::ECDSA_SHA1_Legacy, ecdsa(Hash::Sha256)),
            Key::P256,
            Reason::Unadvertised,
        ),
        (
            rsa_suite,
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA1, Method::Pkcs1(Hash::Sha256)),
            Key::Rsa,
            Reason::Unadvertised,
        ),
    ] {
        assert_refused(Side::Client, TLS12, Some(suite), plan, certificate, reason);
    }
}

#[test]
fn td_server_verifies_independent_client_proofs() {
    let mut cases = Vec::new();
    for scheme in [
        S::RSA_PKCS1_SHA256,
        S::RSA_PKCS1_SHA384,
        S::RSA_PKCS1_SHA512,
        S::RSA_PSS_SHA256,
        S::RSA_PSS_SHA384,
        S::RSA_PSS_SHA512,
    ] {
        cases.push((TLS12, Plan::honest(Key::Rsa, scheme)));
    }
    for scheme in [S::RSA_PSS_SHA256, S::RSA_PSS_SHA384, S::RSA_PSS_SHA512] {
        cases.push((TLS13, Plan::honest(Key::Rsa, scheme)));
    }
    for version in [TLS12, TLS13] {
        for (key, scheme) in [
            (Key::P256, S::ECDSA_NISTP256_SHA256),
            (Key::P384, S::ECDSA_NISTP384_SHA384),
            (Key::P521, S::ECDSA_NISTP521_SHA512),
            (Key::Ed25519, S::ED25519),
        ] {
            cases.push((version, Plan::honest(key, scheme)));
        }
    }
    // TLS 1.2 binds the hash, not the curve, here too.
    cases.push((
        TLS12,
        Plan::claiming(Key::P384, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha256)),
    ));
    for (version, plan) in cases {
        let (outcome, leaf) = as_server(version, plan, plan.key)
            .unwrap_or_else(|failure| panic!("{plan:?}: {failure:?}"));
        assert_eq!(outcome.version, version_of(version), "{plan:?}");
        // The server's own preference picks, for its P-256 identity.
        let suite = if version == TLS12 {
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
        } else {
            C::TLS13_AES_256_GCM_SHA384
        };
        assert_eq!(outcome.suite, suite, "{plan:?}");
        assert_eq!(outcome.group, rustls::NamedGroup::X25519, "{plan:?}");
        assert_eq!(outcome.signatures, 1, "{plan:?}");
        assert!(outcome.offered.contains(&plan.claim), "{plan:?}");
        assert_eq!(
            outcome.peer,
            PeerEvidence::VerifiedClientLeaf(Hash::Sha256.digest(&[&leaf]).try_into().unwrap())
        );
    }
}

#[test]
fn td_server_refuses_corrupted_and_mismatched_client_proofs() {
    use Reason::{BadSignature as Bad, KeyAlgorithm as Key_, Unadvertised as Not};
    for (version, plan, reason) in [
        (
            TLS12,
            Plan::honest(Key::Rsa, S::RSA_PKCS1_SHA512).corrupt(Corrupt::First),
            Bad,
        ),
        (
            TLS12,
            Plan::honest(Key::P521, S::ECDSA_NISTP521_SHA512).corrupt(Corrupt::Second),
            Bad,
        ),
        (
            TLS12,
            Plan::honest(Key::Ed25519, S::ED25519).corrupt(Corrupt::Second),
            Bad,
        ),
        (
            TLS13,
            Plan::honest(Key::Rsa, S::RSA_PSS_SHA384).corrupt(Corrupt::First),
            Bad,
        ),
        (
            TLS13,
            Plan::honest(Key::P384, S::ECDSA_NISTP384_SHA384).corrupt(Corrupt::First),
            Bad,
        ),
        (
            TLS13,
            Plan::honest(Key::Ed25519, S::ED25519).corrupt(Corrupt::First),
            Bad,
        ),
        (
            TLS13,
            Plan::claiming(Key::P384, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha256)),
            Key_,
        ),
        (
            TLS13,
            Plan::claiming(Key::P256, S::ECDSA_NISTP521_SHA512, ecdsa(Hash::Sha512)),
            Key_,
        ),
        (
            TLS13,
            Plan::claiming(Key::P256, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha384)),
            Bad,
        ),
        (
            TLS13,
            Plan::claiming(Key::Ed25519, S::ECDSA_NISTP256_SHA256, Method::Ed25519),
            Key_,
        ),
        (
            TLS13,
            Plan::claiming(Key::Rsa, S::RSA_PSS_SHA256, Method::Pkcs1(Hash::Sha256)),
            Bad,
        ),
        (TLS13, Plan::honest(Key::Rsa, S::RSA_PKCS1_SHA256), Not),
        (
            TLS13,
            Plan::claiming(Key::P256, S::ECDSA_SHA1_Legacy, ecdsa(Hash::Sha256)),
            Not,
        ),
        (
            TLS12,
            Plan::claiming(Key::P256, S::ECDSA_NISTP256_SHA256, ecdsa(Hash::Sha512)),
            Bad,
        ),
        (
            TLS12,
            Plan::claiming(Key::P256, S::ED25519, ecdsa(Hash::Sha512)),
            Key_,
        ),
        (
            TLS12,
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA1, Method::Pkcs1(Hash::Sha256)),
            Not,
        ),
        (
            TLS12,
            Plan::claiming(Key::P256, S::ED448, ecdsa(Hash::Sha256)),
            Not,
        ),
        (
            TLS13,
            Plan::claiming(Key::P256, S::ED448, ecdsa(Hash::Sha256)),
            Not,
        ),
        (
            TLS12,
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA256, Method::Pkcs1(Hash::Sha384)),
            Bad,
        ),
        (
            TLS12,
            Plan::claiming(Key::Rsa, S::RSA_PSS_SHA384, Method::Pkcs1(Hash::Sha384)),
            Bad,
        ),
        (
            TLS12,
            Plan::claiming(Key::Rsa, S::RSA_PKCS1_SHA384, Method::Pss(Hash::Sha384)),
            Bad,
        ),
        (
            TLS12,
            Plan::claiming(
                Key::Rsa,
                S::RSA_PSS_SHA256,
                Method::PssSalt(Hash::Sha256, 0),
            ),
            Bad,
        ),
        (
            TLS13,
            Plan::claiming(
                Key::Rsa,
                S::RSA_PSS_SHA512,
                Method::PssSalt(Hash::Sha512, 65),
            ),
            Bad,
        ),
        (
            TLS12,
            Plan::honest(Key::Rsa, S::RSA_PSS_SHA256).corrupt(Corrupt::Second),
            Bad,
        ),
    ] {
        assert_refused(Side::Server, version, None, plan, plan.key, reason);
    }
}
