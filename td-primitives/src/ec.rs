//! P-256, P-384 and P-521 over one generic implementation: Renes, Costello and
//! Batina's complete addition for a = -3 (eprint 2015/1060, algorithm 4), which
//! has no exceptional case, so it also doubles and there is no second formula.
//! Scalar multiplication takes fixed four-bit windows, reading every table entry
//! and selecting with masks, whether or not its scalars are secret: one routine
//! is less to audit than a fast public path beside a careful secret one. P-256
//! holds long-lived keys and signs; P-384 verifies certificate chains and holds
//! only ephemeral ECDH keys; P-521 only verifies. Each prime is 3 mod 4, so a
//! compressed point's y is one exponentiation away.

use std::sync::OnceLock;

use crate::bignum::{from_be, less_mask, select, to_be, zero_mask, Modulus};
use crate::hmac::Hmac;
use crate::sha::{Digest, Sha256};
use crate::{ct, der, random, Refused};

const P256_P: [u64; 4] = [
    0xffffffffffffffff,
    0x00000000ffffffff,
    0x0000000000000000,
    0xffffffff00000001,
];
const P256_N: [u64; 4] = [
    0xf3b9cac2fc632551,
    0xbce6faada7179e84,
    0xffffffffffffffff,
    0xffffffff00000000,
];
const P256_B: [u64; 4] = [
    0x3bce3c3e27d2604b,
    0x651d06b0cc53b0f6,
    0xb3ebbd55769886bc,
    0x5ac635d8aa3a93e7,
];
const P256_GX: [u64; 4] = [
    0xf4a13945d898c296,
    0x77037d812deb33a0,
    0xf8bce6e563a440f2,
    0x6b17d1f2e12c4247,
];
const P256_GY: [u64; 4] = [
    0xcbb6406837bf51f5,
    0x2bce33576b315ece,
    0x8ee7eb4a7c0f9e16,
    0x4fe342e2fe1a7f9b,
];
const P384_P: [u64; 6] = [
    0x00000000ffffffff,
    0xffffffff00000000,
    0xfffffffffffffffe,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
];
const P384_N: [u64; 6] = [
    0xecec196accc52973,
    0x581a0db248b0a77a,
    0xc7634d81f4372ddf,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
];
const P384_B: [u64; 6] = [
    0x2a85c8edd3ec2aef,
    0xc656398d8a2ed19d,
    0x0314088f5013875a,
    0x181d9c6efe814112,
    0x988e056be3f82d19,
    0xb3312fa7e23ee7e4,
];
const P384_GX: [u64; 6] = [
    0x3a545e3872760ab7,
    0x5502f25dbf55296c,
    0x59f741e082542a38,
    0x6e1d3b628ba79b98,
    0x8eb1c71ef320ad74,
    0xaa87ca22be8b0537,
];
const P384_GY: [u64; 6] = [
    0x7a431d7c90ea0e5f,
    0x0a60b1ce1d7e819d,
    0xe9da3113b5f0b8c0,
    0xf8f41dbd289a147c,
    0x5d9e98bf9292dc29,
    0x3617de4a96262c6f,
];

const P521_P: [u64; 9] = [
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0x00000000000001ff,
];
const P521_N: [u64; 9] = [
    0xbb6fb71e91386409,
    0x3bb5c9b8899c47ae,
    0x7fcc0148f709a5d0,
    0x51868783bf2f966b,
    0xfffffffffffffffa,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0xffffffffffffffff,
    0x00000000000001ff,
];
const P521_B: [u64; 9] = [
    0xef451fd46b503f00,
    0x3573df883d2c34f1,
    0x1652c0bd3bb1bf07,
    0x56193951ec7e937b,
    0xb8b489918ef109e1,
    0xa2da725b99b315f3,
    0x929a21a0b68540ee,
    0x953eb9618e1c9a1f,
    0x0000000000000051,
];
const P521_GX: [u64; 9] = [
    0xf97e7e31c2e5bd66,
    0x3348b3c1856a429b,
    0xfe1dc127a2ffa8de,
    0xa14b5e77efe75928,
    0xf828af606b4d3dba,
    0x9c648139053fb521,
    0x9e3ecb662395b442,
    0x858e06b70404e9cd,
    0x00000000000000c6,
];
const P521_GY: [u64; 9] = [
    0x88be94769fd16650,
    0x353c7086a272c240,
    0xc550b9013fad0761,
    0x97ee72995ef42640,
    0x17afbd17273e662c,
    0x98f54449579b4468,
    0x5c8a5fb42c7d1bd9,
    0x39296a789a3bc004,
    0x0000000000000118,
];

#[derive(Clone, Copy)]
struct Point<const N: usize> {
    x: [u64; N],
    y: [u64; N],
    z: [u64; N],
}

struct Curve<const N: usize> {
    p: Modulus<N>,
    n: Modulus<N>,
    b: [u64; N],
    g: Point<N>,
    /// Bytes in an encoded coordinate: 8 N, but 66 for P-521's 9 limbs.
    width: usize,
    /// Bits in the order, which bounds a digest's contribution.
    order_bits: usize,
    /// (p + 1) / 4, the exponent of a square root mod p.
    root: [u64; N],
}

/// The position of the highest set bit, plus one, of a public value.
fn bit_length(words: &[u64]) -> usize {
    words
        .iter()
        .enumerate()
        .rev()
        .find(|(_, word)| **word != 0)
        .map_or(0, |(at, word)| 64 * at + 64 - word.leading_zeros() as usize)
}

impl<const N: usize> Curve<N> {
    fn new(p: [u64; N], n: [u64; N], b: [u64; N], gx: [u64; N], gy: [u64; N]) -> Self {
        // p = 3 mod 4, so (p + 1) / 4 = (p >> 2) + 1.
        let mut root = [0u64; N];
        let above = p.iter().skip(1).chain(std::iter::once(&0));
        for ((word, low), high) in root.iter_mut().zip(&p).zip(above) {
            *word = (low >> 2) | (high << 62);
        }
        let mut carry = 1u64;
        for word in &mut root {
            let (sum, over) = word.overflowing_add(carry);
            *word = sum;
            carry = u64::from(over);
        }
        let width = bit_length(&p).div_ceil(8);
        let order_bits = bit_length(&n);
        let p = Modulus::new(p);
        let g = Point {
            x: p.enter(&gx),
            y: p.enter(&gy),
            z: p.one,
        };
        Self {
            b: p.enter(&b),
            n: Modulus::new(n),
            p,
            g,
            width,
            order_bits,
            root,
        }
    }

    fn identity(&self) -> Point<N> {
        Point {
            x: [0; N],
            y: self.p.one,
            z: [0; N],
        }
    }

    /// Algorithm 4, whose step numbers are in the comments.
    // Out of line: the portable builds check this body (td-crypto/PORTABLE.md).
    #[inline(never)]
    fn add(&self, a: &Point<N>, c: &Point<N>) -> Point<N> {
        let f = &self.p;
        let (x1, y1, z1) = (&a.x, &a.y, &a.z);
        let (x2, y2, z2) = (&c.x, &c.y, &c.z);
        let mut t0 = f.mul(x1, x2); // 1
        let mut t1 = f.mul(y1, y2); // 2
        let mut t2 = f.mul(z1, z2); // 3
        let mut t3 = f.add(x1, y1); // 4
        let mut t4 = f.add(x2, y2); // 5
        t3 = f.mul(&t3, &t4); // 6
        t4 = f.add(&t0, &t1); // 7
        t3 = f.sub(&t3, &t4); // 8
        t4 = f.add(y1, z1); // 9
        let mut x3 = f.add(y2, z2); // 10
        t4 = f.mul(&t4, &x3); // 11
        x3 = f.add(&t1, &t2); // 12
        t4 = f.sub(&t4, &x3); // 13
        x3 = f.add(x1, z1); // 14
        let mut y3 = f.add(x2, z2); // 15
        x3 = f.mul(&x3, &y3); // 16
        y3 = f.add(&t0, &t2); // 17
        y3 = f.sub(&x3, &y3); // 18
        let mut z3 = f.mul(&self.b, &t2); // 19
        x3 = f.sub(&y3, &z3); // 20
        z3 = f.add(&x3, &x3); // 21
        x3 = f.add(&x3, &z3); // 22
        z3 = f.sub(&t1, &x3); // 23
        x3 = f.add(&t1, &x3); // 24
        y3 = f.mul(&self.b, &y3); // 25
        t1 = f.add(&t2, &t2); // 26
        t2 = f.add(&t1, &t2); // 27
        y3 = f.sub(&y3, &t2); // 28
        y3 = f.sub(&y3, &t0); // 29
        t1 = f.add(&y3, &y3); // 30
        y3 = f.add(&t1, &y3); // 31
        t1 = f.add(&t0, &t0); // 32
        t0 = f.add(&t1, &t0); // 33
        t0 = f.sub(&t0, &t2); // 34
        t1 = f.mul(&t4, &y3); // 35
        t2 = f.mul(&t0, &y3); // 36
        y3 = f.mul(&x3, &z3); // 37
        y3 = f.add(&y3, &t2); // 38
        x3 = f.mul(&t3, &x3); // 39
        x3 = f.sub(&x3, &t1); // 40
        z3 = f.mul(&t4, &z3); // 41
        t1 = f.mul(&t3, &t0); // 42
        z3 = f.add(&z3, &t1); // 43
        Point {
            x: x3,
            y: y3,
            z: z3,
        }
    }

    /// 0 to 15 times `point`, whose multiples are as public as it is.
    fn multiples(&self, point: &Point<N>) -> [Point<N>; WINDOW_ENTRIES] {
        let mut table = [self.identity(); WINDOW_ENTRIES];
        let mut sum = self.identity();
        for entry in table.iter_mut().skip(1) {
            sum = self.add(&sum, point);
            *entry = sum;
        }
        table
    }

    /// The sum of [scalar]point over `terms`, for scalars in plain limbs: one run of
    /// doublings shared between them, fixed work, every entry read, masks only.
    fn multiply_sum<const M: usize>(&self, terms: [(&[u64; N], &Point<N>); M]) -> Point<N> {
        let tables = terms.map(|(_, point)| self.multiples(point));
        let mut result = self.identity();
        for window in (0..64 * N / WINDOW_BITS).rev() {
            for _ in 0..WINDOW_BITS {
                result = self.add(&result, &result);
            }
            let at = window * WINDOW_BITS;
            for ((scalar, _), table) in terms.iter().zip(&tables) {
                let word = scalar.get(at / 64).copied().unwrap_or_default();
                let digit = (word >> (at % 64)) & (WINDOW_ENTRIES as u64 - 1);
                let mut chosen = self.identity();
                for (entry, index) in table.iter().zip(0u64..) {
                    let mask = zero_mask(&[index ^ digit]);
                    select(&mut chosen.x, &entry.x, mask);
                    select(&mut chosen.y, &entry.y, mask);
                    select(&mut chosen.z, &entry.z, mask);
                }
                result = self.add(&result, &chosen);
            }
        }
        result
    }

    fn multiply(&self, scalar: &[u64; N], point: &Point<N>) -> Point<N> {
        self.multiply_sum([(scalar, point)])
    }

    /// Affine x and y in plain limbs. The identity is refused; for a valid
    /// secret scalar and a valid point it cannot occur.
    fn affine(&self, point: &Point<N>) -> Result<([u64; N], [u64; N]), Refused> {
        if zero_mask(&point.z) != 0 {
            return Err(Refused);
        }
        let inverse = self.p.invert(&point.z);
        Ok((
            self.p.leave(&self.p.mul(&point.x, &inverse)),
            self.p.leave(&self.p.mul(&point.y, &inverse)),
        ))
    }

    /// A coordinate's `width` big-endian bytes, which must be below p.
    fn coordinate(&self, bytes: &[u8]) -> Result<[u64; N], Refused> {
        if bytes.len() != self.width {
            return Err(Refused);
        }
        // On the stack: every TLS key exchange and signature check comes here.
        let mut buffer = [0u8; 8 * MAX_LIMBS];
        let padded = buffer.get_mut(..8 * N).ok_or(Refused)?;
        let room = (8 * N).checked_sub(self.width).ok_or(Refused)?;
        padded
            .get_mut(room..)
            .ok_or(Refused)?
            .copy_from_slice(bytes);
        let value: [u64; N] = from_be(padded).ok_or(Refused)?;
        if less_mask(&value, &self.p.m) == 0 {
            return Err(Refused);
        }
        Ok(value)
    }

    /// x^3 - 3x + b, for x in Montgomery form, in Montgomery form.
    fn right_side(&self, x: &[u64; N]) -> [u64; N] {
        let f = &self.p;
        let minus_three = f.sub(&f.sub(&f.sub(&[0; N], &f.one), &f.one), &f.one);
        f.add(&f.mul(&f.add(&f.mul(x, x), &minus_three), x), &self.b)
    }

    /// An uncompressed SEC 1 point: canonical coordinates on the curve. The
    /// cofactor is 1, so that is the whole of validation.
    fn decode_point(&self, encoded: &[u8]) -> Result<Point<N>, Refused> {
        let (&tag, coordinates) = encoded.split_first().ok_or(Refused)?;
        if tag != 4 {
            return Err(Refused);
        }
        let (x_bytes, y_bytes) = coordinates.split_at_checked(self.width).ok_or(Refused)?;
        let x = self.p.enter(&self.coordinate(x_bytes)?);
        let y = self.p.enter(&self.coordinate(y_bytes)?);
        if self.p.mul(&y, &y) != self.right_side(&x) {
            return Err(Refused);
        }
        Ok(Point {
            x,
            y,
            z: self.p.one,
        })
    }

    /// A compressed SEC 1 point: x, with y's parity in the tag. An x whose
    /// right side has no square root is not on the curve. Points are public.
    fn decompress(&self, encoded: &[u8]) -> Result<Point<N>, Refused> {
        let (&tag, x_bytes) = encoded.split_first().ok_or(Refused)?;
        let odd = match tag {
            2 => 0,
            3 => 1,
            _ => return Err(Refused),
        };
        let x = self.p.enter(&self.coordinate(x_bytes)?);
        let right = self.right_side(&x);
        let mut y = self.p.pow(&right, &self.root);
        if self.p.mul(&y, &y) != right {
            return Err(Refused);
        }
        let parity = |value: &[u64; N]| self.p.leave(value).first().copied().unwrap_or(0) & 1;
        if parity(&y) != odd {
            y = self.p.sub(&[0; N], &y);
        }
        // Only y = 0 keeps its parity when negated, and no point of a curve of
        // prime order has it.
        if parity(&y) != odd {
            return Err(Refused);
        }
        Ok(Point {
            x,
            y,
            z: self.p.one,
        })
    }

    /// A point in either SEC 1 form, validated, as 1 + 2 width uncompressed bytes.
    fn uncompressed(&self, encoded: &[u8], out: &mut [u8]) -> Result<(), Refused> {
        let point = match encoded.first() {
            Some(4) => self.decode_point(encoded)?,
            _ => self.decompress(encoded)?,
        };
        self.encode_affine(&self.p.leave(&point.x), &self.p.leave(&point.y), out)
    }

    /// bits2int of a digest, reduced (FIPS 186-5 6.4.1): as many of its leftmost
    /// bits as the order has. Only P-521's order is not whole bytes, and only a
    /// digest of 66 bytes or more reaches past it.
    fn digest_scalar(&self, digest: &[u8]) -> Result<[u64; N], Refused> {
        let mut bytes = vec![0u8; 8 * N];
        let take = digest.len().min(self.width);
        let head = digest.get(..take).ok_or(Refused)?;
        let room = bytes.get_mut(8 * N - take..).ok_or(Refused)?;
        room.copy_from_slice(head);
        let mut limbs: [u64; N] = from_be(&bytes).ok_or(Refused)?;
        // Fewer than 8: `width` rounds p's bits up to a byte, and each order
        // here has its prime's bit length, as a unit test holds.
        let excess = (8 * take).saturating_sub(self.order_bits);
        if excess > 0 {
            let mut carry = 0u64;
            for word in limbs.iter_mut().rev() {
                let low = *word << (64 - excess);
                *word = (*word >> excess) | carry;
                carry = low;
            }
        }
        Ok(self.n.leave(&self.n.enter(&limbs)))
    }

    /// `der_signature` by `public` over `digest`, FIPS 186-5 6.4.2.
    fn verify(&self, public: &[u8], digest: &[u8], der_signature: &[u8]) -> Result<(), Refused> {
        // An empty digest is e = 0, under which anyone can forge for any key, so
        // nothing shorter than the shortest hash here is one.
        if digest.len() < MIN_DIGEST_LEN {
            return Err(Refused);
        }
        let key = self.decode_point(public)?;
        let (r_bytes, s_bytes) = der::two_integers(der_signature)?;
        let r = self.signature_scalar(r_bytes)?;
        let s = self.signature_scalar(s_bytes)?;
        let e = self.n.enter(&self.digest_scalar(digest)?);
        let w = self.n.invert(&self.n.enter(&s));
        let u1 = self.n.leave(&self.n.mul(&e, &w));
        let u2 = self.n.leave(&self.n.mul(&self.n.enter(&r), &w));
        let sum = self.multiply_sum([(&u1, &self.g), (&u2, &key)]);
        let (x, _) = self.affine(&sum)?;
        if self.n.leave(&self.n.enter(&x)) == r {
            Ok(())
        } else {
            Err(Refused)
        }
    }

    /// A DER integer's value, which must lie in 1..n.
    fn signature_scalar(&self, value: &[u8]) -> Result<[u64; N], Refused> {
        if value.len() > 8 * N {
            return Err(Refused);
        }
        let mut bytes = vec![0u8; 8 * N];
        let room = bytes.get_mut(8 * N - value.len()..).ok_or(Refused)?;
        room.copy_from_slice(value);
        let limbs: [u64; N] = from_be(&bytes).ok_or(Refused)?;
        if zero_mask(&limbs) != 0 || less_mask(&limbs, &self.n.m) == 0 {
            return Err(Refused);
        }
        Ok(limbs)
    }

    fn encode_coordinate(value: &[u64; N], out: &mut [u8]) {
        to_be(value, out);
    }

    /// A secret scalar from its big-endian bytes, which must lie in 1..n.
    fn secret_scalar(&self, bytes: &[u8]) -> Result<[u64; N], Refused> {
        let scalar: [u64; N] = from_be(bytes).ok_or(Refused)?;
        if zero_mask(&scalar) != 0 || less_mask(&scalar, &self.n.m) == 0 {
            return Err(Refused);
        }
        Ok(scalar)
    }

    /// A uniform scalar in 1..n, by rejection. Which draws were rejected says
    /// nothing of the one kept.
    fn generate_scalar(&self) -> Result<[u64; N], Refused> {
        let mut bytes = vec![0u8; 8 * N];
        for _ in 0..64 {
            random::fill(&mut bytes)?;
            let scalar = self.secret_scalar(&bytes);
            if scalar.is_ok() {
                ct::zero(&mut bytes);
                return scalar;
            }
        }
        ct::zero(&mut bytes);
        Err(Refused)
    }

    /// The uncompressed public point of `scalar`, into 1 + 2 width bytes.
    fn encode_public(&self, scalar: &[u64; N], out: &mut [u8]) -> Result<(), Refused> {
        let (x, y) = self.affine(&self.multiply(scalar, &self.g))?;
        self.encode_affine(&x, &y, out)
    }

    /// Affine x and y in plain limbs as an uncompressed point of 1 + 2 width bytes.
    fn encode_affine(&self, x: &[u64; N], y: &[u64; N], out: &mut [u8]) -> Result<(), Refused> {
        if out.len() != 1 + 2 * self.width {
            return Err(Refused);
        }
        let (tag, coordinates) = out.split_first_mut().ok_or(Refused)?;
        *tag = 4;
        let (left, right) = coordinates
            .split_at_mut_checked(self.width)
            .ok_or(Refused)?;
        Self::encode_coordinate(x, left);
        Self::encode_coordinate(y, right);
        Ok(())
    }

    /// ECDH: the shared point's x, after validating the peer's point.
    fn agree(&self, scalar: &[u64; N], peer: &[u8], out: &mut [u8]) -> Result<(), Refused> {
        let point = self.decode_point(peer)?;
        let (x, _) = self.affine(&self.multiply(scalar, &point))?;
        Self::encode_coordinate(&x, out);
        Ok(())
    }
}

const MIN_DIGEST_LEN: usize = 32;
/// P-521's, the most limbs of any curve here.
const MAX_LIMBS: usize = 9;
const WINDOW_BITS: usize = 4;
const WINDOW_ENTRIES: usize = 1 << WINDOW_BITS;

/// The constants take hundreds of modular doublings, so each curve makes them once.
fn p256_curve() -> &'static Curve<4> {
    static CURVE: OnceLock<Curve<4>> = OnceLock::new();
    CURVE.get_or_init(|| Curve::new(P256_P, P256_N, P256_B, P256_GX, P256_GY))
}

fn p384_curve() -> &'static Curve<6> {
    static CURVE: OnceLock<Curve<6>> = OnceLock::new();
    CURVE.get_or_init(|| Curve::new(P384_P, P384_N, P384_B, P384_GX, P384_GY))
}

fn p521_curve() -> &'static Curve<9> {
    static CURVE: OnceLock<Curve<9>> = OnceLock::new();
    CURVE.get_or_init(|| Curve::new(P521_P, P521_N, P521_B, P521_GX, P521_GY))
}

pub mod p256 {
    use super::{
        ct, der, from_be, less_mask, p256_curve, zero_mask, Curve, Digest, Hmac, Refused, Sha256,
    };

    pub const SECRET_LEN: usize = 32;
    pub const PUBLIC_LEN: usize = 65;

    /// `der_signature` by an uncompressed `public` over a digest. The hash is
    /// the caller's, and a longer one is truncated as FIPS 186-5 requires.
    pub fn verify(public: &[u8], digest: &[u8], der_signature: &[u8]) -> Result<(), Refused> {
        p256_curve().verify(public, digest, der_signature)
    }

    /// A SEC 1 point, compressed or not, validated and uncompressed: the one
    /// form `verify` and `agree` take, which is all TLS sends, from a key a
    /// certificate may hold in either.
    pub fn uncompressed(encoded: &[u8]) -> Result<[u8; PUBLIC_LEN], Refused> {
        let mut out = [0u8; PUBLIC_LEN];
        p256_curve().uncompressed(encoded, &mut out)?;
        Ok(out)
    }

    pub struct SecretKey {
        scalar: [u64; 4],
    }

    impl SecretKey {
        pub fn generate() -> Result<Self, Refused> {
            Ok(Self {
                scalar: p256_curve().generate_scalar()?,
            })
        }

        pub fn from_bytes(bytes: &[u8; SECRET_LEN]) -> Result<Self, Refused> {
            Ok(Self {
                scalar: p256_curve().secret_scalar(bytes)?,
            })
        }

        pub fn public_key(&self) -> Result<[u8; PUBLIC_LEN], Refused> {
            let mut out = [0u8; PUBLIC_LEN];
            p256_curve().encode_public(&self.scalar, &mut out)?;
            Ok(out)
        }

        /// ECDH: the shared point's x, after validating the peer's point.
        pub fn agree(&self, peer: &[u8]) -> Result<[u8; SECRET_LEN], Refused> {
            let mut out = [0u8; SECRET_LEN];
            p256_curve().agree(&self.scalar, peer, &mut out)?;
            Ok(out)
        }

        /// A DER ECDSA signature over SHA-256 of `message`, with RFC 6979's
        /// deterministic nonce, so a failed RNG cannot repeat one.
        pub fn sign_sha256(&self, message: &[u8]) -> Result<Vec<u8>, Refused> {
            self.sign_sha256_with(message, |_, block| block)
        }

        /// `sign_sha256`, with each candidate passed through `replace`, which a
        /// test uses to force the retries no reachable nonce takes.
        fn sign_sha256_with(
            &self,
            message: &[u8],
            replace: impl Fn(usize, [u8; 32]) -> [u8; 32],
        ) -> Result<Vec<u8>, Refused> {
            let curve = p256_curve();
            let digest = Sha256::digest(message);
            let e = curve.digest_scalar(&digest)?;
            let mut secret_bytes = [0u8; SECRET_LEN];
            Curve::encode_coordinate(&self.scalar, &mut secret_bytes);
            let mut e_bytes = [0u8; SECRET_LEN];
            Curve::encode_coordinate(&e, &mut e_bytes);
            let mut nonces = Rfc6979::new(&secret_bytes, &e_bytes);
            ct::zero(&mut secret_bytes);
            for attempt in 0..64 {
                let mut candidate = replace(attempt, nonces.next_block());
                let Some(mut k) = from_be::<4>(&candidate) else {
                    continue;
                };
                if zero_mask(&k) != 0 || less_mask(&k, &curve.n.m) == 0 {
                    continue;
                }
                let (x, _) = curve.affine(&curve.multiply(&k, &curve.g))?;
                let r = curve.n.leave(&curve.n.enter(&x));
                if zero_mask(&r) != 0 {
                    continue;
                }
                // s = k^-1 (e + r d)
                let entered_r = curve.n.enter(&r);
                let mut entered_d = curve.n.enter(&self.scalar);
                let entered_e = curve.n.enter(&e);
                let mut sum = curve
                    .n
                    .add(&entered_e, &curve.n.mul(&entered_r, &entered_d));
                let mut inverse = curve.n.invert(&curve.n.enter(&k));
                let s = curve.n.leave(&curve.n.mul(&inverse, &sum));
                // k and a signature give the key, so these go as Ed25519's nonce
                // does, best effort.
                ct::zero_words(&mut k);
                ct::zero_words(&mut entered_d);
                ct::zero_words(&mut sum);
                ct::zero_words(&mut inverse);
                ct::zero(&mut candidate);
                if zero_mask(&s) != 0 {
                    continue;
                }
                let (mut r_bytes, mut s_bytes) = ([0u8; 32], [0u8; 32]);
                Curve::encode_coordinate(&r, &mut r_bytes);
                Curve::encode_coordinate(&s, &mut s_bytes);
                return der::encode_two_integers(&r_bytes, &s_bytes);
            }
            Err(Refused)
        }
    }

    impl Drop for SecretKey {
        fn drop(&mut self) {
            self.scalar.fill(0);
            std::hint::black_box(&self.scalar);
        }
    }

    /// RFC 6979 3.2 for SHA-256 and a 256-bit order. bits2int is then the identity
    /// on 32 bytes, and bits2octets is a reduction mod q, so the caller passes `e`
    /// already reduced.
    struct Rfc6979 {
        k: [u8; 32],
        v: [u8; 32],
    }

    impl Rfc6979 {
        fn hmac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
            let mut mac = Hmac::<Sha256>::new(key);
            for part in parts {
                mac.update(part);
            }
            mac.finalize()
        }

        fn new(secret: &[u8; 32], digest: &[u8; 32]) -> Self {
            let mut state = Self {
                k: [0; 32],
                v: [1; 32],
            };
            state.k = Self::hmac(&state.k, &[&state.v, &[0], secret, digest]);
            state.v = Self::hmac(&state.k, &[&state.v]);
            state.k = Self::hmac(&state.k, &[&state.v, &[1], secret, digest]);
            state.v = Self::hmac(&state.k, &[&state.v]);
            state
        }

        fn next_block(&mut self) -> [u8; 32] {
            self.v = Self::hmac(&self.k, &[&self.v]);
            let out = self.v;
            self.k = Self::hmac(&self.k, &[&self.v, &[0]]);
            self.v = Self::hmac(&self.k, &[&self.v]);
            out
        }
    }

    impl Drop for Rfc6979 {
        fn drop(&mut self) {
            ct::zero(&mut self.k);
            ct::zero(&mut self.v);
        }
    }

    /// `sign_sha256` with the first candidates replaced, the DRBG still stepping
    /// past each.
    #[cfg(test)]
    pub(super) fn sign_replacing(
        key: &SecretKey,
        message: &[u8],
        replaced: &[[u8; 32]],
    ) -> Result<Vec<u8>, Refused> {
        key.sign_sha256_with(message, |attempt, block| {
            replaced.get(attempt).copied().unwrap_or(block)
        })
    }

    #[cfg(test)]
    pub(super) fn deterministic_nonce(secret: &[u8; 32], digest: &[u8; 32]) -> [u8; 32] {
        Rfc6979::new(secret, digest).next_block()
    }

    /// The third block, past the retries a rejected candidate takes.
    #[cfg(test)]
    pub(super) fn third_nonce(secret: &[u8; 32], digest: &[u8; 32]) -> [u8; 32] {
        let mut nonces = Rfc6979::new(secret, digest);
        nonces.next_block();
        nonces.next_block();
        nonces.next_block()
    }
}

/// P-384 signs nothing here: certificates are verified, never made. It does
/// agree, because a TLS 1.2 server holding a P-384 certificate refuses a client
/// that does not list the group, whichever group they then negotiate.
pub mod p384 {
    use super::{p384_curve, Refused};

    pub const SECRET_LEN: usize = 48;
    pub const PUBLIC_LEN: usize = 97;

    /// `der_signature` by an uncompressed `public` over a digest.
    pub fn verify(public: &[u8], digest: &[u8], der_signature: &[u8]) -> Result<(), Refused> {
        p384_curve().verify(public, digest, der_signature)
    }

    /// A SEC 1 point, compressed or not, validated and uncompressed, as
    /// `p256::uncompressed`.
    pub fn uncompressed(encoded: &[u8]) -> Result<[u8; PUBLIC_LEN], Refused> {
        let mut out = [0u8; PUBLIC_LEN];
        p384_curve().uncompressed(encoded, &mut out)?;
        Ok(out)
    }

    pub struct SecretKey {
        scalar: [u64; 6],
    }

    impl SecretKey {
        pub fn generate() -> Result<Self, Refused> {
            Ok(Self {
                scalar: p384_curve().generate_scalar()?,
            })
        }

        pub fn from_bytes(bytes: &[u8; SECRET_LEN]) -> Result<Self, Refused> {
            Ok(Self {
                scalar: p384_curve().secret_scalar(bytes)?,
            })
        }

        pub fn public_key(&self) -> Result<[u8; PUBLIC_LEN], Refused> {
            let mut out = [0u8; PUBLIC_LEN];
            p384_curve().encode_public(&self.scalar, &mut out)?;
            Ok(out)
        }

        /// ECDH: the shared point's x, after validating the peer's point.
        pub fn agree(&self, peer: &[u8]) -> Result<[u8; SECRET_LEN], Refused> {
            let mut out = [0u8; SECRET_LEN];
            p384_curve().agree(&self.scalar, peer, &mut out)?;
            Ok(out)
        }
    }

    impl Drop for SecretKey {
        fn drop(&mut self) {
            self.scalar.fill(0);
            std::hint::black_box(&self.scalar);
        }
    }
}

/// P-521 verifies signatures and reads the keys they are checked against:
/// td-crypto's certificate and handshake policy admits them, and td holds no
/// P-521 key and offers no P-521 exchange.
pub mod p521 {
    use super::{p521_curve, Refused};

    pub const PUBLIC_LEN: usize = 133;

    /// `der_signature` by an uncompressed `public` over a digest. A digest of
    /// 66 bytes or more keeps its leftmost 521 bits; none here is that long.
    pub fn verify(public: &[u8], digest: &[u8], der_signature: &[u8]) -> Result<(), Refused> {
        p521_curve().verify(public, digest, der_signature)
    }

    /// A SEC 1 point, compressed or not, validated and uncompressed, as
    /// `p256::uncompressed`.
    pub fn uncompressed(encoded: &[u8]) -> Result<[u8; PUBLIC_LEN], Refused> {
        let mut out = [0u8; PUBLIC_LEN];
        p521_curve().uncompressed(encoded, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
    use super::{p256, p256_curve, p384, p384_curve, p521_curve, Curve, Digest, Sha256};
    use crate::bignum::zero_mask;

    fn hex(text: &str) -> Vec<u8> {
        let text: Vec<u8> = text.bytes().filter(u8::is_ascii_hexdigit).collect();
        text.chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    /// Bit at a time, sharing only `add` with the windowed routine.
    fn by_bits<const N: usize>(
        curve: &Curve<N>,
        scalar: &[u64; N],
        point: &super::Point<N>,
    ) -> super::Point<N> {
        let mut result = curve.identity();
        for word in scalar.iter().rev() {
            for shift in (0..64).rev() {
                result = curve.add(&result, &result);
                if (word >> shift) & 1 == 1 {
                    result = curve.add(&result, point);
                }
            }
        }
        result
    }

    fn windows_agree_with_bits<const N: usize>(curve: &Curve<N>) {
        let same = |a: &super::Point<N>, b: &super::Point<N>| {
            assert_eq!(zero_mask(&a.z) != 0, zero_mask(&b.z) != 0);
            if zero_mask(&a.z) == 0 {
                assert_eq!(curve.affine(a).unwrap(), curve.affine(b).unwrap());
            }
        };
        let doubled = curve.add(&curve.g, &curve.g);
        let mut scalars = vec![[0u64; N], curve.n.m, [u64::MAX; N]];
        // Each digit value at each end of the scalar, and in the middle.
        for digit in 1..16u64 {
            for (word, shift) in [(0, 0), (0, 60), (N - 1, 60), (N / 2, 4)] {
                let mut scalar = [0u64; N];
                scalar[word] = digit << shift;
                scalars.push(scalar);
            }
        }
        let mut state = 0x9e3779b97f4a7c15u64;
        for _ in 0..8 {
            let mut scalar = [0u64; N];
            for word in &mut scalar {
                state = state.wrapping_mul(0x2545f4914f6cdd1d) ^ (state >> 29);
                *word = state;
            }
            scalars.push(scalar);
        }
        for pair in scalars.windows(2) {
            let (one, two) = (&pair[0], &pair[1]);
            same(
                &curve.multiply(one, &doubled),
                &by_bits(curve, one, &doubled),
            );
            same(
                &curve.multiply_sum([(one, &curve.g), (two, &doubled)]),
                &curve.add(
                    &by_bits(curve, one, &curve.g),
                    &by_bits(curve, two, &doubled),
                ),
            );
        }
    }

    #[test]
    fn fixed_windows_agree_with_one_bit_at_a_time() {
        windows_agree_with_bits(p256_curve());
        windows_agree_with_bits(p384_curve());
        windows_agree_with_bits(p521_curve());
    }

    fn order_times_base_is_the_identity<const N: usize>(curve: &Curve<N>) {
        let product = curve.multiply(&curve.n.m, &curve.g);
        assert_ne!(zero_mask(&product.z), 0);
        assert!(curve.affine(&product).is_err());
    }

    #[test]
    fn the_order_annihilates_the_base_point() {
        order_times_base_is_the_identity(p256_curve());
        order_times_base_is_the_identity(p384_curve());
        order_times_base_is_the_identity(p521_curve());
    }

    /// The generator, compressed under either tag, is G or -G, which share x.
    fn generator_decompresses<const N: usize>(curve: &Curve<N>, width: usize, order_bits: usize) {
        assert_eq!((curve.width, curve.order_bits), (width, order_bits));
        assert_eq!(curve.p.m[0] & 3, 3, "p = 3 mod 4");
        // 4 root = p + 1.
        let mut times_four = [0u64; N];
        let mut carry = 0u64;
        for (out, word) in times_four.iter_mut().zip(&curve.root) {
            *out = (word << 2) | carry;
            carry = word >> 62;
        }
        let mut plus_one = curve.p.m;
        let mut carry = 1u64;
        for word in &mut plus_one {
            let (sum, over) = word.overflowing_add(carry);
            *word = sum;
            carry = u64::from(over);
        }
        assert_eq!(times_four, plus_one);

        let mut encoded = vec![0u8; 1 + 2 * width];
        let (x, y) = curve.affine(&curve.g).unwrap();
        curve.encode_affine(&x, &y, &mut encoded).unwrap();
        let odd = encoded[2 * width] & 1;
        let mut out = vec![0u8; encoded.len()];
        for tag in [2u8, 3] {
            let mut compressed = encoded[..=width].to_vec();
            compressed[0] = tag;
            curve.uncompressed(&compressed, &mut out).unwrap();
            assert_eq!(out[..=width], encoded[..=width]);
            let negated = curve.p.leave(&curve.p.sub(&[0; N], &curve.g.y));
            let mut want = vec![0u8; width];
            Curve::<N>::encode_coordinate(if tag & 1 == odd { &y } else { &negated }, &mut want);
            assert_eq!(out[1 + width..], want[..], "tag {tag}");
        }
        curve.uncompressed(&encoded, &mut out).unwrap();
        assert_eq!(out, encoded);
        assert!(
            curve.uncompressed(&encoded[..=width], &mut out).is_err(),
            "tag 4, x only"
        );
        assert!(
            curve.uncompressed(&encoded, &mut out[1..]).is_err(),
            "short output"
        );
        // Infinity, hybrid (6, 7) and unassigned tags, over a real x and over
        // a real x and y, are refused at both lengths.
        for tag in [0u8, 1, 5, 6, 7] {
            let mut whole = encoded.clone();
            whole[0] = tag;
            assert!(curve.uncompressed(&whole, &mut out).is_err(), "tag {tag}");
            assert!(
                curve.uncompressed(&whole[..=width], &mut out).is_err(),
                "tag {tag}, x only"
            );
        }
    }

    #[test]
    fn each_curve_has_its_widths_and_a_square_root_exponent() {
        generator_decompresses(p256_curve(), 32, 256);
        generator_decompresses(p384_curve(), 48, 384);
        generator_decompresses(p521_curve(), 66, 521);
    }

    fn addition_handles_every_case<const N: usize>(curve: &Curve<N>) {
        let g = &curve.g;
        let identity = curve.identity();
        let doubled = curve.add(g, g);
        let same = |a: &super::Point<N>, b: &super::Point<N>| {
            curve.affine(a).unwrap() == curve.affine(b).unwrap()
        };
        assert!(same(&curve.add(g, &identity), g));
        assert!(same(&curve.add(&identity, g), g));
        assert!(same(&curve.add(&doubled, &identity), &doubled));
        // n - 1 + 1 reaches the identity through a point and its negative.
        let mut below = curve.n.m;
        below[0] -= 1;
        let negative = curve.multiply(&below, g);
        assert_ne!(zero_mask(&curve.add(&negative, g).z), 0);
        // 2G + G = 3G by two different routes.
        let mut three = [0u64; N];
        three[0] = 3;
        assert!(same(&curve.add(&doubled, g), &curve.multiply(&three, g)));
    }

    // From Python's hmac module: the retry path a known answer for the first
    // block never reaches.
    #[test]
    fn rfc6979_goes_on_to_later_blocks() {
        let mut secret = [0u8; 32];
        for (byte, value) in secret.iter_mut().zip(1u8..) {
            *byte = value;
        }
        let digest: [u8; 32] =
            hex("70f4b15ed1294bd307aa6f7b0ca2e3da6ce85102eac89d66810189bd5a40ef22")
                .try_into()
                .unwrap();
        assert_eq!(
            p256::deterministic_nonce(&secret, &digest).to_vec(),
            hex("fda378cb7c1d3cacf3b150e9674a941bef0b21f5bd275c9b506fdfc6dab067b6")
        );
        assert_eq!(
            p256::third_nonce(&secret, &digest).to_vec(),
            hex("94936c5c23686cf551e28906d60e9f6cd21861b504581881fe49cf741d6581c0")
        );
    }

    /// bignum's oracle rows are against these curves' moduli, not copies of them.
    #[test]
    fn curve_moduli_are_the_bignum_corpus_moduli() {
        let mut seen = 0;
        for line in include_str!("../tests/bignum_vectors.txt").lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let ["modulus", name, _, want] = fields[..] else {
                continue;
            };
            let limbs: &[u64] = match name {
                "P256-p" => &p256_curve().p.m,
                "P256-n" => &p256_curve().n.m,
                "P384-p" => &p384_curve().p.m,
                "P384-n" => &p384_curve().n.m,
                "P521-p" => &p521_curve().p.m,
                "P521-n" => &p521_curve().n.m,
                _ => panic!("unknown modulus {name}"),
            };
            let mut got = vec![0u8; limbs.len() * 8];
            crate::bignum::to_be(limbs, &mut got);
            assert_eq!(got, hex(want), "{name}");
            seen += 1;
        }
        assert_eq!(seen, 6);
    }

    /// The signing loop's retries, with candidates no reachable nonce gives put in
    /// place of the first: out of range, zero, and one making s zero. Answers from
    /// tests/ecdsa_retry.py, which says how each row is built.
    #[test]
    fn signing_retries_past_a_refused_candidate() {
        let mut rows = 0;
        for line in include_str!("../tests/ecdsa_retry.txt").lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, secret, replaced, want] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let key = p256::SecretKey::from_bytes(&hex(secret).try_into().unwrap()).unwrap();
            let replaced: Vec<[u8; 32]> = replaced
                .split(',')
                .map(|block| hex(block).try_into().unwrap())
                .collect();
            let signature = p256::sign_replacing(&key, b"sample", &replaced).unwrap();
            assert_eq!(signature, hex(want), "{name}");
            let digest = Sha256::digest(b"sample");
            let public = key.public_key().unwrap();
            assert!(p256::verify(&public, &digest, &signature).is_ok(), "{name}");
            rows += 1;
        }
        assert_eq!(rows, 3);
        // A DRBG that never yields a usable candidate ends in a refusal, not a loop.
        let key = p256::SecretKey::from_bytes(&[7; 32]).unwrap();
        assert!(p256::sign_replacing(&key, b"sample", &[[0; 32]; 64]).is_err());
        assert!(p256::sign_replacing(&key, b"sample", &[[0; 32]; 63]).is_ok());
    }

    #[test]
    fn complete_addition_handles_doubling_and_the_identity() {
        addition_handles_every_case(p256_curve());
        addition_handles_every_case(p384_curve());
        addition_handles_every_case(p521_curve());
    }

    // RFC 6979 A.2.5: P-256, SHA-256, message "sample".
    #[test]
    fn rfc_6979_sample() {
        let secret: [u8; 32] =
            hex("c9afa9d845ba75166b5c215767b1d6934e50c3db36e89b127b8a622b120f6721")
                .try_into()
                .unwrap();
        let digest = Sha256::digest(b"sample");
        assert_eq!(
            p256::deterministic_nonce(&secret, &digest).to_vec(),
            hex("a6e3c57dd01abe90086538398355dd4c3b17aa873382b0f24d6129493d8aad60")
        );
        let key = p256::SecretKey::from_bytes(&secret).unwrap();
        let public = key.public_key().unwrap();
        assert_eq!(
            public[1..].to_vec(),
            hex(
                "60fed4ba255a9d31c961eb74c6356d68c049b8923b61fa6ce669622e60f29fb6\
                 7903fe1008b8bc99a41ae9e95628bc64f2f1b20c2d7e9f5177a3c294d4462299"
            )
        );
        let signature = key.sign_sha256(b"sample").unwrap();
        let (r, s) = crate::der::two_integers(&signature).unwrap();
        assert_eq!(
            r.to_vec(),
            hex("efd48b2aacb6a8fd1140dd9cd45e81d69d2c877b56aaf991c34d0ea84eaf3716")
        );
        assert_eq!(
            s.to_vec(),
            hex("f7cb1c942d657c41d436c7a1b6e29f65f3e900dbb9aff4064dc4ab2f843acda8")
        );
        p256::verify(&public, &digest, &signature).unwrap();
        assert!(p256::verify(&public, &Sha256::digest(b"other"), &signature).is_err());
    }

    #[test]
    fn a_generated_key_signs_and_agrees() {
        let a = p256::SecretKey::generate().unwrap();
        let b = p256::SecretKey::generate().unwrap();
        let (a_public, b_public) = (a.public_key().unwrap(), b.public_key().unwrap());
        assert_eq!(a.agree(&b_public).unwrap(), b.agree(&a_public).unwrap());
        let signature = a.sign_sha256(b"message").unwrap();
        p256::verify(&a_public, &Sha256::digest(b"message"), &signature).unwrap();
        assert!(p256::verify(&b_public, &Sha256::digest(b"message"), &signature).is_err());
    }

    #[test]
    fn a_point_off_the_curve_or_out_of_range_is_refused() {
        let key = p256::SecretKey::generate().unwrap();
        let mut public = key.public_key().unwrap();
        assert!(key.agree(&public).is_ok());
        public[64] ^= 1;
        assert!(key.agree(&public).is_err());
        public[64] ^= 1;
        public[0] = 2;
        assert!(key.agree(&public).is_err());
        assert!(key.agree(&[0; p256::PUBLIC_LEN]).is_err());
        assert!(key.agree(&[]).is_err());
        assert!(p384::verify(&[4; p384::PUBLIC_LEN], &[0; 48], &[0x30, 0]).is_err());
    }
}
