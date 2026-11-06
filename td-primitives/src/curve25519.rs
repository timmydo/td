//! X25519 (RFC 7748) and Ed25519 (RFC 8032) over one field in five 51-bit
//! limbs. Scalar multiplication always doubles and adds and selects with masks,
//! whether or not its scalar is secret: one routine is less to audit than a
//! fast public path beside a careful secret one.

use crate::bignum::{less_mask, Modulus};
use crate::sha::{Digest, Sha512};
use crate::{ct, random, Refused};
use std::hint::black_box;

const MASK_51: u64 = (1 << 51) - 1;

/// -121665/121666.
const D: [u8; 32] = [
    0xa3, 0x78, 0x59, 0x13, 0xca, 0x4d, 0xeb, 0x75, 0xab, 0xd8, 0x41, 0x41, 0x4d, 0x0a, 0x70, 0x00,
    0x98, 0xe8, 0x79, 0x77, 0x79, 0x40, 0xc7, 0x8c, 0x73, 0xfe, 0x6f, 0x2b, 0xee, 0x6c, 0x03, 0x52,
];
const SQRT_M1: [u8; 32] = [
    0xb0, 0xa0, 0x0e, 0x4a, 0x27, 0x1b, 0xee, 0xc4, 0x78, 0xe4, 0x2f, 0xad, 0x06, 0x18, 0x43, 0x2f,
    0xa7, 0xd7, 0xfb, 0x3d, 0x99, 0x00, 0x4d, 0x2b, 0x0b, 0xdf, 0xc1, 0x4f, 0x80, 0x24, 0x83, 0x2b,
];
/// The base point's encoding: y = 4/5, x even.
const BASE: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];
/// The group order, in little-endian limbs.
const L: [u64; 4] = [
    0x5812631a5cf5d3ed,
    0x14def9dea2f79cd6,
    0x0000000000000000,
    0x1000000000000000,
];

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// The 255 low bits; bit 255 is ignored, as RFC 7748 requires.
    fn from_bytes(bytes: &[u8; 32]) -> Fe {
        let [a, b, c, d] = words(bytes);
        Fe([
            a & MASK_51,
            ((a >> 51) | (b << 13)) & MASK_51,
            ((b >> 38) | (c << 26)) & MASK_51,
            ((c >> 25) | (d << 39)) & MASK_51,
            (d >> 12) & MASK_51,
        ])
    }

    /// The canonical encoding, fully reduced.
    fn to_bytes(self) -> [u8; 32] {
        let [mut l0, mut l1, mut l2, mut l3, mut l4] = self.carry().0;
        // q is 1 exactly when the value is at least p.
        let mut q = (l0 + 19) >> 51;
        q = (l1 + q) >> 51;
        q = (l2 + q) >> 51;
        q = (l3 + q) >> 51;
        q = (l4 + q) >> 51;
        l0 += 19 * q;
        l1 += l0 >> 51;
        l0 &= MASK_51;
        l2 += l1 >> 51;
        l1 &= MASK_51;
        l3 += l2 >> 51;
        l2 &= MASK_51;
        l4 += l3 >> 51;
        l3 &= MASK_51;
        l4 &= MASK_51;
        let mut out = [0u8; 32];
        for (bytes, word) in out.as_chunks_mut::<8>().0.iter_mut().zip([
            l0 | (l1 << 51),
            (l1 >> 13) | (l2 << 38),
            (l2 >> 26) | (l3 << 25),
            (l3 >> 39) | (l4 << 12),
        ]) {
            *bytes = word.to_le_bytes();
        }
        out
    }

    /// One carry pass, leaving every limb below 2^52.
    fn carry(self) -> Fe {
        let [l0, l1, l2, l3, l4] = self.0;
        Fe([
            (l0 & MASK_51) + (l4 >> 51) * 19,
            (l1 & MASK_51) + (l0 >> 51),
            (l2 & MASK_51) + (l1 >> 51),
            (l3 & MASK_51) + (l2 >> 51),
            (l4 & MASK_51) + (l3 >> 51),
        ])
    }

    fn add(self, other: Fe) -> Fe {
        let [a0, a1, a2, a3, a4] = self.0;
        let [b0, b1, b2, b3, b4] = other.0;
        Fe([a0 + b0, a1 + b1, a2 + b2, a3 + b3, a4 + b4]).carry()
    }

    /// Adding 16p first keeps every limb positive for inputs below 2^52.
    fn sub(self, other: Fe) -> Fe {
        const LOW: u64 = 16 * ((1 << 51) - 19);
        const HIGH: u64 = 16 * ((1 << 51) - 1);
        let [a0, a1, a2, a3, a4] = self.0;
        let [b0, b1, b2, b3, b4] = other.0;
        Fe([
            a0 + LOW - b0,
            a1 + HIGH - b1,
            a2 + HIGH - b2,
            a3 + HIGH - b3,
            a4 + HIGH - b4,
        ])
        .carry()
    }

    fn neg(self) -> Fe {
        Fe::ZERO.sub(self)
    }

    fn mul(self, other: Fe) -> Fe {
        let wide = |x: u64, y: u64| u128::from(x) * u128::from(y);
        let [a0, a1, a2, a3, a4] = self.0;
        let [b0, b1, b2, b3, b4] = other.0;
        let (b1_19, b2_19, b3_19, b4_19) = (b1 * 19, b2 * 19, b3 * 19, b4 * 19);
        let c0 =
            wide(a0, b0) + wide(a4, b1_19) + wide(a3, b2_19) + wide(a2, b3_19) + wide(a1, b4_19);
        let mut c1 =
            wide(a1, b0) + wide(a0, b1) + wide(a4, b2_19) + wide(a3, b3_19) + wide(a2, b4_19);
        let mut c2 = wide(a2, b0) + wide(a1, b1) + wide(a0, b2) + wide(a4, b3_19) + wide(a3, b4_19);
        let mut c3 = wide(a3, b0) + wide(a2, b1) + wide(a1, b2) + wide(a0, b3) + wide(a4, b4_19);
        let mut c4 = wide(a4, b0) + wide(a3, b1) + wide(a2, b2) + wide(a1, b3) + wide(a0, b4);
        c1 += c0 >> 51;
        c2 += c1 >> 51;
        c3 += c2 >> 51;
        c4 += c3 >> 51;
        let mut l0 = (c0 as u64) & MASK_51;
        let mut l1 = (c1 as u64) & MASK_51;
        let l2 = (c2 as u64) & MASK_51;
        let l3 = (c3 as u64) & MASK_51;
        let l4 = (c4 as u64) & MASK_51;
        l0 += ((c4 >> 51) as u64) * 19;
        l1 += l0 >> 51;
        l0 &= MASK_51;
        Fe([l0, l1, l2, l3, l4])
    }

    fn square(self) -> Fe {
        self.mul(self)
    }

    fn squares(self, count: u32) -> Fe {
        (0..count).fold(self, |value, _| value.square())
    }

    /// z^(2^250 - 1) and z^11, from which inversion and square roots follow.
    fn chain(self) -> (Fe, Fe) {
        let z2 = self.square();
        let z9 = z2.squares(2).mul(self);
        let z11 = z9.mul(z2);
        let z_5_0 = z11.square().mul(z9);
        let z_10_0 = z_5_0.squares(5).mul(z_5_0);
        let z_20_0 = z_10_0.squares(10).mul(z_10_0);
        let z_40_0 = z_20_0.squares(20).mul(z_20_0);
        let z_50_0 = z_40_0.squares(10).mul(z_10_0);
        let z_100_0 = z_50_0.squares(50).mul(z_50_0);
        let z_200_0 = z_100_0.squares(100).mul(z_100_0);
        let z_250_0 = z_200_0.squares(50).mul(z_50_0);
        (z_250_0, z11)
    }

    /// z^(p-2); zero stays zero.
    fn invert(self) -> Fe {
        let (z_250_0, z11) = self.chain();
        z_250_0.squares(5).mul(z11)
    }

    /// z^((p-5)/8).
    fn pow_p58(self) -> Fe {
        let (z_250_0, _) = self.chain();
        z_250_0.squares(2).mul(self)
    }

    fn is_negative(self) -> bool {
        self.to_bytes().first().is_some_and(|low| low & 1 == 1)
    }

    fn is_equal(self, other: Fe) -> bool {
        ct::eq(&self.to_bytes(), &other.to_bytes())
    }

    /// Swap when `mask` is all ones.
    fn swap(a: &mut Fe, b: &mut Fe, mask: u64) {
        let mask = black_box(mask);
        for (x, y) in a.0.iter_mut().zip(b.0.iter_mut()) {
            let difference = (*x ^ *y) & mask;
            *x ^= difference;
            *y ^= difference;
        }
    }
}

fn words(bytes: &[u8; 32]) -> [u64; 4] {
    let mut out = [0u64; 4];
    for (word, chunk) in out.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *word = u64::from_le_bytes(*chunk);
    }
    out
}

pub mod x25519 {
    use super::{ct, random, Fe, Refused};

    pub const KEY_LEN: usize = 32;
    const BASE_U: [u8; 32] = {
        let mut u = [0; 32];
        u[0] = 9;
        u
    };

    pub fn generate_secret() -> Result<[u8; KEY_LEN], Refused> {
        let mut secret = [0u8; KEY_LEN];
        random::fill(&mut secret)?;
        Ok(secret)
    }

    pub fn public_key(secret: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
        ladder(secret, &BASE_U)
    }

    /// The shared secret. An all-zero result means a small-order peer key, and
    /// RFC 8446 7.4.2 requires refusing it.
    pub fn agree(secret: &[u8; KEY_LEN], peer: &[u8; KEY_LEN]) -> Result<[u8; KEY_LEN], Refused> {
        let shared = ladder(secret, peer);
        if ct::eq(&shared, &[0; KEY_LEN]) {
            return Err(Refused);
        }
        Ok(shared)
    }

    /// RFC 7748 5.
    fn ladder(secret: &[u8; KEY_LEN], u: &[u8; KEY_LEN]) -> [u8; KEY_LEN] {
        let mut scalar = *secret;
        if let Some(low) = scalar.first_mut() {
            *low &= 248;
        }
        if let Some(high) = scalar.last_mut() {
            *high = (*high & 127) | 64;
        }
        let a24 = Fe([121665, 0, 0, 0, 0]);
        let x1 = Fe::from_bytes(u);
        let (mut x2, mut z2, mut x3, mut z3) = (Fe::ONE, Fe::ZERO, x1, Fe::ONE);
        let mut swap = 0u64;
        for position in (0..255usize).rev() {
            let byte = scalar.get(position / 8).copied().unwrap_or_default();
            let bit = u64::from((byte >> (position % 8)) & 1);
            swap ^= bit;
            Fe::swap(&mut x2, &mut x3, swap.wrapping_neg());
            Fe::swap(&mut z2, &mut z3, swap.wrapping_neg());
            swap = bit;

            let a = x2.add(z2);
            let aa = a.square();
            let b = x2.sub(z2);
            let bb = b.square();
            let e = aa.sub(bb);
            let c = x3.add(z3);
            let d = x3.sub(z3);
            let da = d.mul(a);
            let cb = c.mul(b);
            x3 = da.add(cb).square();
            z3 = x1.mul(da.sub(cb).square());
            x2 = aa.mul(bb);
            z2 = e.mul(aa.add(a24.mul(e)));
        }
        Fe::swap(&mut x2, &mut x3, swap.wrapping_neg());
        Fe::swap(&mut z2, &mut z3, swap.wrapping_neg());
        ct::zero(&mut scalar);
        x2.mul(z2.invert()).to_bytes()
    }
}

/// A point in extended coordinates on -x^2 + y^2 = 1 + d x^2 y^2.
#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

impl Point {
    const IDENTITY: Point = Point {
        x: Fe::ZERO,
        y: Fe::ONE,
        z: Fe::ONE,
        t: Fe::ZERO,
    };

    /// RFC 8032 5.1.3. Refuses a non-canonical y and an x of 0 with its sign
    /// bit set; a public input, so it may branch.
    fn decode(bytes: &[u8; 32]) -> Result<Point, Refused> {
        let sign = bytes.last().is_some_and(|high| high & 0x80 != 0);
        let mut canonical = *bytes;
        if let Some(high) = canonical.last_mut() {
            *high &= 0x7f;
        }
        let y = Fe::from_bytes(&canonical);
        if y.to_bytes() != canonical {
            return Err(Refused);
        }
        let d = Fe::from_bytes(&D);
        let yy = y.square();
        let u = yy.sub(Fe::ONE);
        let v = yy.mul(d).add(Fe::ONE);
        let v3 = v.square().mul(v);
        let mut x = u.mul(v3).mul(u.mul(v3.square().mul(v)).pow_p58());
        let check = v.mul(x.square());
        if !check.is_equal(u) {
            if !check.is_equal(u.neg()) {
                return Err(Refused);
            }
            x = x.mul(Fe::from_bytes(&SQRT_M1));
        }
        if x.is_negative() != sign {
            if x.is_equal(Fe::ZERO) {
                return Err(Refused);
            }
            x = x.neg();
        }
        Ok(Point {
            x,
            y,
            z: Fe::ONE,
            t: x.mul(y),
        })
    }

    fn encode(self) -> [u8; 32] {
        let inverse = self.z.invert();
        let mut out = self.y.mul(inverse).to_bytes();
        if let Some(high) = out.last_mut() {
            *high |= u8::from(self.x.mul(inverse).is_negative()) << 7;
        }
        out
    }

    /// add-2008-hwcd-3, complete for a = -1 and non-square d, so it also
    /// doubles and needs no case for the identity.
    fn add(self, other: Point) -> Point {
        let two_d = Fe::from_bytes(&D).mul(Fe([2, 0, 0, 0, 0]));
        let a = self.y.sub(self.x).mul(other.y.sub(other.x));
        let b = self.y.add(self.x).mul(other.y.add(other.x));
        let c = self.t.mul(two_d).mul(other.t);
        let zz = self.z.mul(other.z);
        let d = zz.add(zz);
        let e = b.sub(a);
        let f = d.sub(c);
        let g = d.add(c);
        let h = b.add(a);
        Point {
            x: e.mul(f),
            y: g.mul(h),
            z: f.mul(g),
            t: e.mul(h),
        }
    }

    fn negate(self) -> Point {
        Point {
            x: self.x.neg(),
            y: self.y,
            z: self.z,
            t: self.t.neg(),
        }
    }

    /// [scalar]self for little-endian bytes: fixed work, selection by mask.
    fn multiply(self, scalar: &[u8; 32]) -> Point {
        let mut result = Point::IDENTITY;
        for byte in scalar.iter().rev() {
            for shift in (0..8).rev() {
                result = result.add(result);
                let mut added = result.add(self);
                let mask = u64::from((byte >> shift) & 1).wrapping_neg();
                Fe::swap(&mut result.x, &mut added.x, mask);
                Fe::swap(&mut result.y, &mut added.y, mask);
                Fe::swap(&mut result.z, &mut added.z, mask);
                Fe::swap(&mut result.t, &mut added.t, mask);
            }
        }
        result
    }
}

fn scalar_field() -> Modulus<4> {
    Modulus::new(L)
}

/// A hash output as a little-endian integer, reduced mod L. It is
/// high * 2^256 + low, and multiplying `high` by R^2 in Montgomery form gives
/// high * 2^256 mod L directly.
fn reduce_wide(bytes: &[u8; 64]) -> [u8; 32] {
    let field = scalar_field();
    let (mut low, mut high) = ([0u8; 32], [0u8; 32]);
    let (head, tail) = bytes.split_at(32);
    low.copy_from_slice(head);
    high.copy_from_slice(tail);
    let high_part = field.enter(&words(&high));
    let low_part = field.leave(&field.enter(&words(&low)));
    limbs_to_le(&field.add(&high_part, &low_part))
}

fn limbs_to_le(limbs: &[u64; 4]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (bytes, word) in out.as_chunks_mut::<8>().0.iter_mut().zip(limbs) {
        *bytes = word.to_le_bytes();
    }
    out
}

/// S = r + k * a mod L, over little-endian inputs.
fn multiply_add(k: &[u8; 32], a: &[u8; 32], r: &[u8; 32]) -> [u8; 32] {
    let field = scalar_field();
    let k_entered = field.enter(&words(k));
    let a_entered = field.enter(&words(a));
    let product = field.leave(&field.mul(&k_entered, &a_entered));
    limbs_to_le(&field.add(&product, &field.leave(&field.enter(&words(r)))))
}

fn is_canonical_scalar(bytes: &[u8; 32]) -> bool {
    let limbs = words(bytes);
    less_mask(&limbs, &L) != 0
}

fn challenge(r: &[u8; 32], public: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut hash = Sha512::new();
    hash.update(r);
    hash.update(public);
    hash.update(message);
    reduce_wide(&hash.finalize())
}

pub mod ed25519 {
    use super::{
        challenge, ct, is_canonical_scalar, multiply_add, random, reduce_wide, Digest, Point,
        Refused, Sha512, BASE,
    };

    pub const SEED_LEN: usize = 32;
    pub const PUBLIC_LEN: usize = 32;
    pub const SIGNATURE_LEN: usize = 64;

    /// The y of every point of order 1, 2, 4 or 8, each way it can be spelled: 0, 1,
    /// the two y of order 8, p - 1, and p and p + 1 as the only y at or past p that
    /// reduce to one of those. The sign bit is left out, so seven stand for fourteen.
    const SMALL_ORDER_Y: &[[u8; 32]] = &[
        [0; 32],
        [
            1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0,
        ],
        [
            0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef,
            0x98, 0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88,
            0x6d, 0x53, 0xfc, 0x05,
        ],
        [
            0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10,
            0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77,
            0x92, 0xac, 0x03, 0x7a,
        ],
        [
            0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
        [
            0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
        [
            0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
    ];

    /// Whether `public` encodes a point of small order, in any spelling, those `verify`
    /// refuses anyway included. `verify` itself takes the canonical ones, being handed
    /// only trusted keys, so a caller holding untrusted key material asks here. A key of
    /// mixed order is not small and is not caught. The input is public: this may branch.
    pub fn is_small_order(public: &[u8; PUBLIC_LEN]) -> bool {
        let mut y = *public;
        if let Some(high) = y.last_mut() {
            *high &= 0x7f;
        }
        SMALL_ORDER_Y.contains(&y)
    }

    pub struct SigningKey {
        scalar: [u8; 32],
        prefix: [u8; 32],
        public: [u8; PUBLIC_LEN],
    }

    impl SigningKey {
        pub fn from_seed(seed: &[u8; SEED_LEN]) -> Result<Self, Refused> {
            let mut expanded = Sha512::digest(seed);
            let (head, tail) = expanded.split_at_mut(32);
            let mut scalar = [0u8; 32];
            scalar.copy_from_slice(head);
            let mut prefix = [0u8; 32];
            prefix.copy_from_slice(tail);
            ct::zero(&mut expanded);
            if let Some(low) = scalar.first_mut() {
                *low &= 248;
            }
            if let Some(high) = scalar.last_mut() {
                *high = (*high & 127) | 64;
            }
            let public = Point::decode(&BASE)?.multiply(&scalar).encode();
            Ok(Self {
                scalar,
                prefix,
                public,
            })
        }

        pub fn generate_seed() -> Result<[u8; SEED_LEN], Refused> {
            let mut seed = [0u8; SEED_LEN];
            random::fill(&mut seed)?;
            Ok(seed)
        }

        pub fn public_key(&self) -> [u8; PUBLIC_LEN] {
            self.public
        }

        /// RFC 8032 5.1.6.
        pub fn sign(&self, message: &[u8]) -> Result<[u8; SIGNATURE_LEN], Refused> {
            let mut hash = Sha512::new();
            hash.update(&self.prefix);
            hash.update(message);
            let mut nonce_hash = hash.finalize();
            let mut nonce = reduce_wide(&nonce_hash);
            ct::zero(&mut nonce_hash);
            let r = Point::decode(&BASE)?.multiply(&nonce).encode();
            let k = challenge(&r, &self.public, message);
            let s = multiply_add(&k, &self.scalar, &nonce);
            ct::zero(&mut nonce);
            let mut signature = [0u8; SIGNATURE_LEN];
            let (left, right) = signature.split_at_mut(32);
            left.copy_from_slice(&r);
            right.copy_from_slice(&s);
            Ok(signature)
        }
    }

    impl Drop for SigningKey {
        fn drop(&mut self) {
            ct::zero(&mut self.scalar);
            ct::zero(&mut self.prefix);
        }
    }

    /// RFC 8032 5.1.7, cofactorless: S must be below L, and the recomputed R
    /// must match byte for byte, which also refuses a non-canonical R.
    pub fn verify(
        public: &[u8; PUBLIC_LEN],
        message: &[u8],
        signature: &[u8; SIGNATURE_LEN],
    ) -> Result<(), Refused> {
        let (r, s) = signature.split_at(32);
        let r: &[u8; 32] = r.try_into().map_err(|_| Refused)?;
        let s: &[u8; 32] = s.try_into().map_err(|_| Refused)?;
        if !is_canonical_scalar(s) {
            return Err(Refused);
        }
        let key = Point::decode(public)?;
        let k = challenge(r, public, message);
        let recomputed = Point::decode(&BASE)?
            .multiply(s)
            .add(key.negate().multiply(&k));
        if recomputed.encode() == *r {
            Ok(())
        } else {
            Err(Refused)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::{ed25519, reduce_wide, x25519, Fe, Point, BASE, D, L, SQRT_M1};

    // From Python big integers: y = 2 gives an x^2 that is not a square.
    #[test]
    fn a_y_with_no_x_is_not_a_point_with_either_sign() {
        let mut encoded = [0u8; 32];
        encoded[0] = 2;
        assert!(Point::decode(&encoded).is_err());
        encoded[31] = 0x80;
        assert!(Point::decode(&encoded).is_err());
    }

    fn hex32(text: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (at, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * at..2 * at + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn the_constants_are_what_they_claim() {
        let d = Fe::from_bytes(&D);
        assert!(d
            .mul(Fe([121666, 0, 0, 0, 0]))
            .is_equal(Fe([121665, 0, 0, 0, 0]).neg()));
        let root = Fe::from_bytes(&SQRT_M1);
        assert!(root.square().is_equal(Fe::ONE.neg()));
        assert!(Fe([5, 0, 0, 0, 0])
            .mul(Fe::from_bytes(&BASE))
            .is_equal(Fe([4, 0, 0, 0, 0])));
    }

    #[test]
    fn the_field_inverts_and_encodes_canonically() {
        let two_inverse = Fe([2, 0, 0, 0, 0]).invert();
        assert!(two_inverse.mul(Fe([2, 0, 0, 0, 0])).is_equal(Fe::ONE));
        // p itself, and p + 1, encode as 0 and 1.
        let p = Fe([
            (1 << 51) - 19,
            (1 << 51) - 1,
            (1 << 51) - 1,
            (1 << 51) - 1,
            (1 << 51) - 1,
        ]);
        assert_eq!(p.to_bytes(), [0; 32]);
        let mut one = [0u8; 32];
        one[0] = 1;
        assert_eq!(p.add(Fe::ONE).to_bytes(), one);
    }

    #[test]
    fn the_base_point_has_order_l() {
        let base = Point::decode(&BASE).unwrap();
        let mut order = [0u8; 32];
        for (bytes, word) in order.chunks_mut(8).zip(L) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        let product = base.multiply(&order);
        assert_eq!(product.encode(), Point::IDENTITY.encode());
    }

    #[test]
    fn reduction_matches_python_big_integers() {
        // python3: (int.from_bytes(bytes(range(64)), 'little') % L)
        let mut wide = [0u8; 64];
        for (at, byte) in wide.iter_mut().enumerate() {
            *byte = at as u8;
        }
        assert_eq!(
            reduce_wide(&wide),
            hex32("7a3c6282f02d37a05023b60d5428e6cc5961d4c31221937adae0b574e4d07205")
        );
        let mut order = [0u8; 64];
        for (bytes, word) in order.chunks_mut(8).zip(L) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        assert_eq!(reduce_wide(&order), [0; 32]);
    }

    // RFC 7748 5.2, whose second case's u has bit 255 set, and the 1000-round
    // iteration of 5.2's closing test.
    #[test]
    fn rfc_7748() {
        let scalar = hex32("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
        let u = hex32("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
        assert_eq!(
            x25519::agree(&scalar, &u).unwrap(),
            hex32("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552")
        );
        let scalar = hex32("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d");
        let u = hex32("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493");
        assert_eq!(
            x25519::agree(&scalar, &u).unwrap(),
            hex32("95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957")
        );
        let (mut k, mut u) = ([0u8; 32], [0u8; 32]);
        k[0] = 9;
        u[0] = 9;
        for _ in 0..1000 {
            let next = x25519::agree(&k, &u).unwrap();
            u = k;
            k = next;
        }
        assert_eq!(
            k,
            hex32("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51")
        );
    }

    // The torsion subgroup taken from the group law rather than from the list: the
    // multiples of an order-8 point, eight distinct encodings, each of which [8]P
    // sends to the identity.
    #[test]
    fn the_small_order_list_is_exactly_the_torsion_subgroup() {
        let generator = hex32("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05");
        let generator = Point::decode(&generator).unwrap();
        let mut scalar = [0u8; 32];
        let mut encodings = std::collections::BTreeSet::new();
        for multiple in 0..8 {
            scalar[0] = multiple;
            let point = generator.multiply(&scalar);
            let encoded = point.encode();
            assert!(ed25519::is_small_order(&encoded), "{multiple}");
            scalar[0] = 8;
            assert_eq!(point.multiply(&scalar).encode(), Point::IDENTITY.encode());
            encodings.insert(encoded);
        }
        assert_eq!(encodings.len(), 8);

        // Those eight, and the six that decode refuses: the y at or past p, and x = 0
        // with its sign bit set. Those are the only other ways of spelling a member.
        let mut non_canonical = Vec::new();
        for text in [
            "0100000000000000000000000000000000000000000000000000000000000080",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        ] {
            let encoded = hex32(text);
            assert!(ed25519::is_small_order(&encoded), "{text}");
            assert!(Point::decode(&encoded).is_err(), "{text}");
            non_canonical.push(encoded);
        }

        // One bit from any of the fourteen is not small, but for the sign bit.
        for encoded in encodings.iter().chain(&non_canonical) {
            for bit in 0..255 {
                let mut changed = *encoded;
                changed[bit / 8] ^= 1 << (bit % 8);
                let is_listed = encodings.contains(&changed) || non_canonical.contains(&changed);
                assert_eq!(ed25519::is_small_order(&changed), is_listed, "{bit}");
            }
            let mut other_sign = *encoded;
            other_sign[31] ^= 0x80;
            assert!(ed25519::is_small_order(&other_sign));
        }

        // Nor is the base point, an ordinary key or a key of mixed order.
        assert!(!ed25519::is_small_order(&BASE));
        let key = ed25519::SigningKey::from_seed(&[7; 32])
            .unwrap()
            .public_key();
        assert!(!ed25519::is_small_order(&key));
        let mixed = Point::decode(&key).unwrap().add(generator).encode();
        assert!(Point::decode(&mixed).is_ok());
        assert!(!ed25519::is_small_order(&mixed));
    }

    #[test]
    fn a_small_order_peer_key_is_refused() {
        assert!(x25519::agree(&[7; 32], &[0; 32]).is_err());
        let mut one = [0u8; 32];
        one[0] = 1;
        assert!(x25519::agree(&[7; 32], &one).is_err());
    }

    // RFC 8032 7.1 tests 1 and 3.
    #[test]
    fn rfc_8032() {
        let cases: [(&str, &str, &[u8], &str); 2] = [
            (
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                b"",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
                 5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            ),
            (
                "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
                "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
                &[0xaf, 0x82],
                "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac\
                 18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
            ),
        ];
        for (seed, public, message, signature) in cases {
            let key = ed25519::SigningKey::from_seed(&hex32(seed)).unwrap();
            assert_eq!(key.public_key(), hex32(public));
            let signature_text: String = signature.split_whitespace().collect();
            let mut want = [0u8; 64];
            for (at, byte) in want.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&signature_text[2 * at..2 * at + 2], 16).unwrap();
            }
            let made = key.sign(message).unwrap();
            assert_eq!(made, want);
            ed25519::verify(&key.public_key(), message, &made).unwrap();
            let mut changed = made;
            changed[63] ^= 1;
            assert!(ed25519::verify(&key.public_key(), message, &changed).is_err());
            assert!(ed25519::verify(&key.public_key(), b"other", &made).is_err());
        }
    }
}
