//! Private P-256 primitives. Protocol authentication and entropy are callers' work.

type Words = [u64; 4];
const ZERO: Words = [0; 4];
const ONE: Words = [1, 0, 0, 0];
const P: Words = [
    0xffffffffffffffff,
    0x00000000ffffffff,
    0x0000000000000000,
    0xffffffff00000001,
];
const N: Words = [
    0xf3b9cac2fc632551,
    0xbce6faada7179e84,
    0xffffffffffffffff,
    0xffffffff00000000,
];
const P_R2: Words = [
    0x0000000000000003,
    0xfffffffbffffffff,
    0xfffffffffffffffe,
    0x00000004fffffffd,
];
const N_R2: Words = [
    0x83244c95be79eea2,
    0x4699799c49bd6fa6,
    0x2845b2392b6bec59,
    0x66e12d94f3d95620,
];
const B: Words = [
    0x3bce3c3e27d2604b,
    0x651d06b0cc53b0f6,
    0xb3ebbd55769886bc,
    0x5ac635d8aa3a93e7,
];
const GX: Words = [
    0xf4a13945d898c296,
    0x77037d812deb33a0,
    0xf8bce6e563a440f2,
    0x6b17d1f2e12c4247,
];
const GY: Words = [
    0xcbb6406837bf51f5,
    0x2bce33576b315ece,
    0x8ee7eb4a7c0f9e16,
    0x4fe342e2fe1a7f9b,
];

/// Takes ownership even on refusal, so the candidate has the same drop policy.
pub(super) struct SecretScalar(Box<[u8; 32]>);

impl SecretScalar {
    pub(super) fn from_bytes(bytes: Box<[u8; 32]>) -> Result<Self, &'static str> {
        let owner = Self(bytes);
        let mut words = decode(&owner.0);
        let valid = (zero_mask(&words) == 0) & (subtract(&words, &N).1 == 1);
        words.fill(0);
        std::hint::black_box(&mut words);
        if !valid {
            return Err("P-256 private scalar must be in 1..n");
        }
        Ok(owner)
    }

    pub(super) fn public_key(&self) -> Result<PublicKey, &'static str> {
        Point::generator().multiply(&self.0).affine()
    }

    pub(super) fn agree(&self, peer: &PublicKey) -> Result<SharedSecret, &'static str> {
        let mut point = peer.point().multiply(&self.0);
        let result = point.affine().map(|mut affine| {
            let mut secret = SharedSecret(Box::new([0; 32]));
            affine.x.write_bytes(&mut secret.0);
            affine.x.clear();
            affine.y.clear();
            secret
        });
        point.clear();
        result
    }
}

impl Drop for SecretScalar {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(self.0.as_mut());
    }
}

pub(super) struct SharedSecret(Box<[u8; 32]>);

impl SharedSecret {
    pub(super) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for SharedSecret {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(self.0.as_mut());
    }
}

/// A finite, canonical affine point on the fixed cofactor-one curve.
pub(super) struct PublicKey {
    x: Field,
    y: Field,
}

impl PublicKey {
    pub(super) fn from_coordinates(x: &[u8; 32], y: &[u8; 32]) -> Result<Self, &'static str> {
        let x = Field::from_bytes(x)?;
        let y = Field::from_bytes(y)?;
        let rhs = x
            .square()
            .mul(x)
            .sub(x.double().add(x))
            .add(Field::from_raw(B));
        if y.square().sub(rhs).is_zero() == 0 {
            return Err("P-256 public key is not on the curve");
        }
        Ok(Self { x, y })
    }

    pub(super) fn coordinates(&self) -> ([u8; 32], [u8; 32]) {
        (self.x.bytes(), self.y.bytes())
    }

    /// Verifies an already hashed SHA-256 message and fixed-width r/s values.
    pub(super) fn verify(
        &self,
        digest: &[u8; 32],
        r: &[u8; 32],
        s: &[u8; 32],
    ) -> Result<(), &'static str> {
        let r = Scalar::from_bytes(r)?;
        let s = Scalar::from_bytes(s)?;
        if r.is_zero() != 0 || s.is_zero() != 0 {
            return Err("ES256 signature scalar is zero");
        }
        let inverse = s.inverse();
        let hash = Scalar::from_raw(reduce(&decode(digest), 0, &N));
        let u1 = hash.mul(inverse).bytes();
        let u2 = r.mul(inverse).bytes();
        let point = Point::generator()
            .multiply(&u1)
            .add(self.point().multiply(&u2));
        let affine = point.affine()?;
        let x = Scalar::from_raw(reduce(&decode(&affine.x.bytes()), 0, &N));
        if x.sub(r).is_zero() == 0 {
            return Err("ES256 signature mismatch");
        }
        Ok(())
    }

    fn point(&self) -> Point {
        Point {
            x: self.x,
            y: self.y,
            z: Field::one(),
        }
    }
}

// Montgomery residues, always canonical. The type separates p from n.
#[derive(Clone, Copy)]
struct Residue<const ORDER: bool>(Words);
type Field = Residue<false>;
type Scalar = Residue<true>;

impl<const ORDER: bool> Residue<ORDER> {
    const MODULUS: Words = if ORDER { N } else { P };
    const R2: Words = if ORDER { N_R2 } else { P_R2 };
    const FACTOR: u64 = if ORDER { 0xccd1c8aaee00bc4f } else { 1 };

    fn from_bytes(bytes: &[u8; 32]) -> Result<Self, &'static str> {
        let raw = decode(bytes);
        if subtract(&raw, &Self::MODULUS).1 == 0 {
            return Err("noncanonical P-256 integer");
        }
        Ok(Self::from_raw(raw))
    }

    fn from_raw(raw: Words) -> Self {
        Self(montgomery(&raw, &Self::R2, &Self::MODULUS, Self::FACTOR))
    }

    fn one() -> Self {
        Self::from_raw(ONE)
    }

    fn write_bytes(self, bytes: &mut [u8; 32]) {
        let mut raw = montgomery(&self.0, &ONE, &Self::MODULUS, Self::FACTOR);
        for (out, word) in bytes.as_chunks_mut::<8>().0.iter_mut().rev().zip(raw) {
            *out = word.to_be_bytes();
        }
        raw.fill(0);
        std::hint::black_box(&mut raw);
    }

    fn bytes(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        self.write_bytes(&mut bytes);
        bytes
    }

    fn add(self, rhs: Self) -> Self {
        let (sum, carry) = add(&self.0, &rhs.0);
        Self(reduce(&sum, carry, &Self::MODULUS))
    }

    fn sub(self, rhs: Self) -> Self {
        let (difference, borrow) = subtract(&self.0, &rhs.0);
        let corrected = add(&difference, &Self::MODULUS).0;
        Self(select(&difference, &corrected, 0u64.wrapping_sub(borrow)))
    }

    fn mul(self, rhs: Self) -> Self {
        Self(montgomery(&self.0, &rhs.0, &Self::MODULUS, Self::FACTOR))
    }

    fn square(self) -> Self {
        self.mul(self)
    }
    fn double(self) -> Self {
        self.add(self)
    }
    fn is_zero(self) -> u64 {
        zero_mask(&self.0)
    }

    // Fermat inversion with a fixed 256-bit public exponent. Zero maps to zero.
    fn inverse(self) -> Self {
        let exponent = subtract(&Self::MODULUS, &[2, 0, 0, 0]).0;
        let mut result = Self::one();
        for word in exponent.iter().rev() {
            for bit in (0..64).rev() {
                let square = result.square();
                let product = square.mul(self);
                result = Self(select(
                    &square.0,
                    &product.0,
                    0u64.wrapping_sub((word >> bit) & 1),
                ));
            }
        }
        result
    }

    fn clear(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
    }
}

fn decode(bytes: &[u8; 32]) -> Words {
    let mut words = ZERO;
    for (word, bytes) in words.iter_mut().zip(bytes.as_chunks::<8>().0.iter().rev()) {
        *word = u64::from_be_bytes(*bytes);
    }
    words
}

fn zero_mask(words: &Words) -> u64 {
    let combined = words.iter().fold(0, |all, word| all | word);
    ((combined | combined.wrapping_neg()) >> 63).wrapping_sub(1)
}

// mask is exactly zero or all ones. A barrier discourages branch substitution;
// only inspection of the actual compiler output can establish its lowering.
fn select(a: &Words, b: &Words, mask: u64) -> Words {
    let mask = std::hint::black_box(mask);
    let mut out = ZERO;
    for ((out, a), b) in out.iter_mut().zip(a).zip(b) {
        *out = (a & !mask) | (b & mask);
    }
    out
}

fn add(a: &Words, b: &Words) -> (Words, u64) {
    let mut out = ZERO;
    let mut carry = 0u128;
    for ((out, a), b) in out.iter_mut().zip(a).zip(b) {
        let sum = u128::from(*a) + u128::from(*b) + carry;
        *out = sum as u64;
        carry = sum >> 64;
    }
    (out, carry as u64)
}

fn subtract(a: &Words, b: &Words) -> (Words, u64) {
    let mut out = ZERO;
    let mut borrow = 0;
    for ((out, a), b) in out.iter_mut().zip(a).zip(b) {
        let (value, first) = a.overflowing_sub(*b);
        let (value, second) = value.overflowing_sub(borrow);
        *out = value;
        borrow = u64::from(first | second);
    }
    (out, borrow)
}

// Input is below 2*modulus, with a one-bit carry above the four words.
fn reduce(value: &Words, carry: u64, modulus: &Words) -> Words {
    let (difference, borrow) = subtract(value, modulus);
    select(value, &difference, 0u64.wrapping_sub(carry | (borrow ^ 1)))
}

// Coarsely integrated operand scanning, radix 2^64, R=2^256.
// Requires b < modulus; a may be any four-word integer. Operand order matters.
fn montgomery(a: &Words, b: &Words, modulus: &Words, factor: u64) -> Words {
    let mut accumulator = [0u64; 6];
    for word in a {
        add_product(&mut accumulator, *word, b);
        let [low, ..] = accumulator;
        add_product(&mut accumulator, low.wrapping_mul(factor), modulus);
        let [_, a, b, c, d, e] = accumulator;
        accumulator = [a, b, c, d, e, 0];
    }
    let [a, b, c, d, carry, _] = accumulator;
    let result = reduce(&[a, b, c, d], carry, modulus);
    accumulator.fill(0);
    std::hint::black_box(&mut accumulator);
    result
}

fn add_product(accumulator: &mut [u64; 6], word: u64, value: &Words) {
    let [a, b, c, d, high, extra] = accumulator;
    let mut carry = 0u128;
    for (out, value) in [a, b, c, d].into_iter().zip(value) {
        let sum = u128::from(word) * u128::from(*value) + u128::from(*out) + carry;
        *out = sum as u64;
        carry = sum >> 64;
    }
    let sum = u128::from(*high) + carry;
    *high = sum as u64;
    // CIOS keeps the six-word sum below 2*2^64*modulus: extra is at most one.
    *extra += (sum >> 64) as u64;
}

#[derive(Clone, Copy)]
struct Point {
    x: Field,
    y: Field,
    z: Field,
}

impl Point {
    fn infinity() -> Self {
        Self {
            x: Field::one(),
            y: Field::one(),
            z: Residue(ZERO),
        }
    }
    fn generator() -> Self {
        Self {
            x: Field::from_raw(GX),
            y: Field::from_raw(GY),
            z: Field::one(),
        }
    }

    fn choose(a: Self, b: Self, mask: u64) -> Self {
        Self {
            x: Residue(select(&a.x.0, &b.x.0, mask)),
            y: Residue(select(&a.y.0, &b.y.0, mask)),
            z: Residue(select(&a.z.0, &b.z.0, mask)),
        }
    }

    // EFD dbl-2001-b, a=-3. Canonicalize infinity without a branch.
    fn double(self) -> Self {
        let delta = self.z.square();
        let gamma = self.y.square();
        let beta = self.x.mul(gamma);
        let alpha = self.x.sub(delta).mul(self.x.add(delta));
        let alpha = alpha.double().add(alpha);
        let x = alpha.square().sub(beta.double().double().double());
        let y = alpha
            .mul(beta.double().double().sub(x))
            .sub(gamma.square().double().double().double());
        let z = self.y.add(self.z).square().sub(gamma).sub(delta);
        Self::choose(Self { x, y, z }, Self::infinity(), z.is_zero())
    }

    // EFD add-2007-bl, with unconditional masked exceptional-case handling.
    fn add(self, rhs: Self) -> Self {
        let z1z1 = self.z.square();
        let z2z2 = rhs.z.square();
        let u1 = self.x.mul(z2z2);
        let u2 = rhs.x.mul(z1z1);
        let s1 = self.y.mul(rhs.z).mul(z2z2);
        let s2 = rhs.y.mul(self.z).mul(z1z1);
        let h = u2.sub(u1);
        let i = h.double().square();
        let j = h.mul(i);
        let r = s2.sub(s1).double();
        let v = u1.mul(i);
        let x = r.square().sub(j).sub(v.double());
        let y = r.mul(v.sub(x)).sub(s1.mul(j).double());
        let z = self.z.add(rhs.z).square().sub(z1z1).sub(z2z2).mul(h);
        let exceptional = Self::choose(Self::infinity(), self.double(), r.is_zero());
        let result = Self::choose(Self { x, y, z }, exceptional, h.is_zero());
        let result = Self::choose(result, rhs, self.z.is_zero());
        Self::choose(result, self, rhs.z.is_zero())
    }

    fn multiply(self, scalar: &[u8; 32]) -> Self {
        let mut result = Self::infinity();
        for byte in scalar {
            for bit in (0..8).rev() {
                let mut doubled = result.double();
                let mut added = doubled.add(self);
                result = Self::choose(
                    doubled,
                    added,
                    0u64.wrapping_sub(u64::from((byte >> bit) & 1)),
                );
                doubled.clear();
                added.clear();
            }
        }
        result
    }

    fn affine(self) -> Result<PublicKey, &'static str> {
        if self.z.is_zero() != 0 {
            return Err("P-256 result is infinity");
        }
        let inverse = self.z.inverse();
        let square = inverse.square();
        Ok(PublicKey {
            x: self.x.mul(square),
            y: self.y.mul(square).mul(inverse),
        })
    }

    fn clear(&mut self) {
        self.x.clear();
        self.y.clear();
        self.z.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> [u8; 32] {
        assert_eq!(text.len(), 64);
        let mut bytes = [0; 32];
        for (out, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            *out = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        bytes
    }

    fn raw_bytes(words: Words) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (out, word) in bytes.as_chunks_mut::<8>().0.iter_mut().rev().zip(words) {
            *out = word.to_be_bytes();
        }
        bytes
    }

    fn vectors(kind: &str) -> Vec<Vec<&str>> {
        include_str!("../tests/p256_vectors.txt")
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
            .filter(|fields| fields.first() == Some(&kind))
            .collect()
    }

    fn arithmetic<const ORDER: bool>(name: &str) {
        let mut count = 0;
        for row in vectors("field") {
            let [_, domain, a, b, sum, difference, product, inverse] = row.as_slice() else {
                panic!("invalid field fixture");
            };
            if *domain != name {
                continue;
            }
            let a = Residue::<ORDER>::from_bytes(&hex(a)).unwrap();
            let b = Residue::<ORDER>::from_bytes(&hex(b)).unwrap();
            for (value, expected) in [
                (a.add(b), sum),
                (a.sub(b), difference),
                (a.mul(b), product),
                (a.inverse(), inverse),
            ] {
                assert_eq!(value.bytes(), hex(expected), "field {name}, case {count}");
                assert_eq!(subtract(&value.0, &Residue::<ORDER>::MODULUS).1, 1);
            }
            count += 1;
        }
        assert_eq!(count, 34);
        assert_eq!(Residue::<ORDER>::one().bytes(), raw_bytes(ONE));
        assert_eq!(Residue::<ORDER>::from_raw(ZERO).inverse().bytes(), [0; 32]);
    }

    #[test]
    fn independent_modular_arithmetic_covers_carries_and_borrows() {
        arithmetic::<false>("p");
        arithmetic::<true>("n");
    }

    #[test]
    fn nist_ecdh_and_openssl_scalar_boundaries_match() {
        for (kind, count) in [("ecdh", 25), ("edge", 16), ("leading", 1)] {
            let rows = vectors(kind);
            assert_eq!(rows.len(), count);
            for (index, row) in rows.iter().enumerate() {
                let [_, private, x, y, peer_x, peer_y, shared] = row.as_slice() else {
                    panic!("invalid ECDH fixture");
                };
                let private = SecretScalar::from_bytes(Box::new(hex(private))).unwrap();
                let public = private.public_key().unwrap();
                assert_eq!(public.coordinates(), (hex(x), hex(y)), "{kind} {index}");
                PublicKey::from_coordinates(&hex(x), &hex(y)).unwrap();
                let peer = PublicKey::from_coordinates(&hex(peer_x), &hex(peer_y)).unwrap();
                let actual = private.agree(&peer).unwrap();
                assert_eq!(*actual.bytes(), hex(shared), "{kind} {index}");
                if kind == "leading" {
                    assert_eq!(actual.bytes().first(), Some(&0));
                }
            }
        }
    }

    #[test]
    fn nist_signature_acceptance_and_refusal_and_rare_x_reduction() {
        let mut passed = 0;
        let mut refused = 0;
        for (kind, count) in [("verify", 15), ("rare", 1), ("hash", 3)] {
            let rows = vectors(kind);
            assert_eq!(rows.len(), count);
            for row in rows {
                let [_, expected, digest, x, y, r, s] = row.as_slice() else {
                    panic!("invalid signature fixture");
                };
                let (digest, r, s) = (hex(digest), hex(r), hex(s));
                let key = PublicKey::from_coordinates(&hex(x), &hex(y));
                let result = key
                    .as_ref()
                    .map_err(|error| *error)
                    .and_then(|key| key.verify(&digest, &r, &s));
                assert_eq!(result.is_ok(), *expected == "P");
                if *expected == "P" {
                    passed += 1;
                    let key = key.unwrap();
                    let opposite_s = raw_bytes(subtract(&N, &decode(&s)).0);
                    key.verify(&digest, &r, &opposite_s).unwrap();
                    let mut changed = digest;
                    changed[0] ^= 1;
                    assert!(key.verify(&changed, &r, &s).is_err());
                } else {
                    assert_eq!(*expected, "F");
                    refused += 1;
                }
            }
        }
        assert_eq!((passed, refused), (7, 12));
    }

    #[test]
    fn nist_public_key_validation_preserves_overwide_rejections() {
        let rows = vectors("public");
        assert_eq!(rows.len(), 12);
        let mut passed = 0;
        let mut refused = 0;
        for row in rows {
            let [_, expected, x, y] = row.as_slice() else {
                panic!("invalid public fixture");
            };
            let accepted = x.len() == 64
                && y.len() == 64
                && PublicKey::from_coordinates(&hex(x), &hex(y)).is_ok();
            assert_eq!(accepted, *expected == "P");
            if accepted {
                passed += 1;
            } else {
                refused += 1;
            }
        }
        assert_eq!((passed, refused), (4, 8));
    }

    #[test]
    fn scalar_and_coordinate_admission_never_reduce_untrusted_values() {
        for words in [ZERO, N, add(&N, &ONE).0, [u64::MAX; 4]] {
            assert!(SecretScalar::from_bytes(Box::new(raw_bytes(words))).is_err());
        }
        for words in [ONE, subtract(&N, &ONE).0] {
            assert!(SecretScalar::from_bytes(Box::new(raw_bytes(words))).is_ok());
        }
        let (x, y) = (raw_bytes(GX), raw_bytes(GY));
        for invalid in [P, add(&P, &ONE).0, [u64::MAX; 4]] {
            assert!(PublicKey::from_coordinates(&raw_bytes(invalid), &y).is_err());
            assert!(PublicKey::from_coordinates(&x, &raw_bytes(invalid)).is_err());
        }
        assert!(PublicKey::from_coordinates(&[0; 32], &[0; 32]).is_err());
        let mut off_curve = y;
        off_curve[31] ^= 1;
        assert!(PublicKey::from_coordinates(&x, &off_curve).is_err());
    }

    #[test]
    fn exceptional_point_cases_and_projective_representations() {
        let g = Point::generator();
        let infinity = Point::infinity();
        let negative = Point {
            y: Field::from_raw(ZERO).sub(g.y),
            ..g
        };
        let coordinates = g.affine().unwrap().coordinates();
        assert_eq!(g.add(infinity).affine().unwrap().coordinates(), coordinates);
        assert_eq!(infinity.add(g).affine().unwrap().coordinates(), coordinates);
        assert!(infinity.add(infinity).affine().is_err());
        assert!(infinity.double().affine().is_err());
        assert!(g.add(negative).affine().is_err());
        assert!(negative.add(g).affine().is_err());
        assert_eq!(
            g.add(g).affine().unwrap().coordinates(),
            g.double().affine().unwrap().coordinates()
        );
        let scale = Field::from_raw([7, 0, 0, 0]);
        let scaled = Point {
            x: g.x.mul(scale.square()),
            y: g.y.mul(scale.square()).mul(scale),
            z: scale,
        };
        assert_eq!(scaled.affine().unwrap().coordinates(), coordinates);
        assert_eq!(
            scaled.add(g).affine().unwrap().coordinates(),
            g.double().affine().unwrap().coordinates()
        );
        assert!(scaled.add(negative).affine().is_err());
        assert!(g.multiply(&[0; 32]).affine().is_err());
        assert!(g.multiply(&raw_bytes(N)).affine().is_err());
        assert_eq!(
            g.multiply(&raw_bytes(subtract(&N, &ONE).0))
                .affine()
                .unwrap()
                .coordinates(),
            negative.affine().unwrap().coordinates()
        );
    }

    #[test]
    fn signature_scalar_ranges_and_infinity_result_are_refused() {
        let key = Point::generator().affine().unwrap();
        let one = raw_bytes(ONE);
        for invalid in [ZERO, N, add(&N, &ONE).0, [u64::MAX; 4]] {
            assert!(key.verify(&one, &raw_bytes(invalid), &one).is_err());
            assert!(key.verify(&one, &one, &raw_bytes(invalid)).is_err());
        }
        // u1=u2=1, Q=-G: verification must refuse the resulting infinity.
        let negative = PublicKey {
            x: key.x,
            y: Field::from_raw(ZERO).sub(key.y),
        };
        assert!(negative.verify(&one, &one, &one).is_err());
    }
}
