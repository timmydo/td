//! HMAC (RFC 2104) over any `Digest`. HKDF and TLS 1.2's PRF are not here:
//! rustls derives both from this through its own `HkdfUsingHmac` and
//! `PrfUsingHmac`. SHA-1 is no `Digest`, and `hmac_sha1` is the only HMAC over it.
//! `pbkdf2_sha256` is PBKDF2 over HMAC-SHA-256, SCRAM-SHA-256's `Hi`.

use crate::ct;
use crate::sha::{sha1, sha1_of_parts, Digest, Sha256};
use crate::Refused;

#[derive(Clone)]
pub struct Hmac<D: Digest> {
    inner: D,
    outer: D,
}

impl<D: Digest> Hmac<D> {
    pub fn new(key: &[u8]) -> Self {
        let mut pad = vec![0u8; D::BLOCK_LEN];
        if key.len() > D::BLOCK_LEN {
            let mut hashed = D::digest(key);
            for (out, byte) in pad.iter_mut().zip(hashed.as_ref()) {
                *out = *byte;
            }
            zero_output(&mut hashed);
        } else {
            for (out, byte) in pad.iter_mut().zip(key) {
                *out = *byte;
            }
        }
        for byte in &mut pad {
            *byte ^= 0x36;
        }
        let mut inner = D::new();
        inner.update(&pad);
        for byte in &mut pad {
            *byte ^= 0x36 ^ 0x5c;
        }
        let mut outer = D::new();
        outer.update(&pad);
        ct::zero(&mut pad);
        Self { inner, outer }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    pub fn finalize(mut self) -> D::Output {
        let inner = std::mem::replace(&mut self.inner, D::new());
        let mut hashed = inner.finalize();
        self.outer.update(hashed.as_ref());
        zero_output(&mut hashed);
        std::mem::replace(&mut self.outer, D::new()).finalize()
    }
}

/// Both hash states are key-equivalent, so dropping overwrites each with a
/// fresh one, best effort: copies a move or `finalize` left are not reached.
impl<D: Digest> Drop for Hmac<D> {
    fn drop(&mut self) {
        self.inner = D::new();
        self.outer = D::new();
        std::hint::black_box(&self.inner);
        std::hint::black_box(&self.outer);
    }
}

fn zero_output<T: AsMut<[u8]>>(value: &mut T) {
    ct::zero(value.as_mut());
}

/// HMAC of the concatenation of `parts`. The output is a plain array, so
/// compare one against a secret with `ct::eq`, never `==`.
pub fn mac<D: Digest>(key: &[u8], parts: &[&[u8]]) -> D::Output {
    let mut hmac = Hmac::<D>::new(key);
    for part in parts {
        hmac.update(part);
    }
    hmac.finalize()
}

/// PBKDF2's longest output here, 32 SHA-256 blocks.
pub const PBKDF2_MAX_OUTPUT: usize = 1024;

/// PBKDF2-HMAC-SHA-256 (RFC 8018 5.2) of `password` and `salt`, filling `out`.
/// It refuses zero iterations and an output empty or past `PBKDF2_MAX_OUTPUT`,
/// writing nothing. Iterations are otherwise not capped: a peer that chooses
/// them is the caller's to bound. The password is the HMAC key, so its length
/// counts as public, as every HMAC key's does.
pub fn pbkdf2_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out: &mut [u8],
) -> Result<(), Refused> {
    if iterations == 0 || out.is_empty() || out.len() > PBKDF2_MAX_OUTPUT {
        return Err(Refused);
    }
    // The password's keyed midstates, copied for every link of every chain.
    // Hmac<Sha256> is two fixed-size states: the copy allocates nothing.
    let keyed = Hmac::<Sha256>::new(password);
    let fresh = || keyed.clone();
    for (block, chunk) in (1u32..).zip(out.chunks_mut(32)) {
        let mut link = fresh();
        link.update(salt);
        link.update(&block.to_be_bytes());
        let mut previous = link.finalize();
        let mut sum = previous;
        for _ in 1..iterations {
            let mut link = fresh();
            link.update(&previous);
            previous = link.finalize();
            for (total, byte) in sum.iter_mut().zip(&previous) {
                *total ^= byte;
            }
        }
        for (target, byte) in chunk.iter_mut().zip(&sum) {
            *target = *byte;
        }
        ct::zero(&mut previous);
        ct::zero(&mut sum);
    }
    Ok(())
}

/// HMAC-SHA-1 (RFC 2104), for what its format defines by it: a hashed `known_hosts`
/// entry. Nothing else should take a dependency on this. Compare with `ct::eq`.
pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut pad = [0u8; 64];
    if key.len() > pad.len() {
        let mut hashed = sha1(key);
        for (out, byte) in pad.iter_mut().zip(hashed) {
            *out = byte;
        }
        ct::zero(&mut hashed);
    } else {
        for (out, byte) in pad.iter_mut().zip(key) {
            *out = *byte;
        }
    }
    for byte in &mut pad {
        *byte ^= 0x36;
    }
    let mut inner = sha1_of_parts(&[&pad, data]);
    for byte in &mut pad {
        *byte ^= 0x36 ^ 0x5c;
    }
    let out = sha1_of_parts(&[&pad, &inner]);
    ct::zero(&mut pad);
    ct::zero(&mut inner);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]
    use super::mac;
    use crate::sha::{Sha256, Sha384, Sha512};

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 4231 4.2, 4.3, 4.7: short, ASCII and longer-than-block keys.
    #[test]
    fn rfc_4231() {
        let key = [0x0b; 20];
        let data: &[u8] = b"Hi There";
        assert_eq!(
            hex(&mac::<Sha256>(&key, &[data])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&mac::<Sha384>(&key, &[data])),
            "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59c\
             faea9ea9076ede7f4af152e8b2fa9cb6"
        );
        assert_eq!(
            hex(&mac::<Sha512>(&key, &[data])),
            "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cde\
             daa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
        );
        assert_eq!(
            hex(&mac::<Sha256>(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        let long_key = [0xaa; 131];
        assert_eq!(
            hex(&mac::<Sha256>(
                &long_key,
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32, length: usize) -> String {
        let mut out = vec![0u8; length];
        assert_eq!(
            super::pbkdf2_sha256(password, salt, iterations, &mut out),
            Ok(())
        );
        hex(&out)
    }

    // RFC 7914 11's two PBKDF2-HMAC-SHA256 vectors, two blocks each.
    #[test]
    fn rfc_7914() {
        assert_eq!(
            pbkdf2(b"passwd", b"salt", 1, 64),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
        assert_eq!(
            pbkdf2(b"Password", b"NaCl", 80000, 64),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56\
             a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
    }

    // Python's `hashlib.pbkdf2_hmac("sha256", ...)`: SCRAM-SHA-256's salted
    // password for RFC 7677's exchange, then outputs ending inside, at and
    // past a block, NUL bytes in both inputs, a password longer than a
    // block and a salt longer than one.
    #[test]
    fn hashlib_lengths_and_inputs() {
        let salt = [
            0x5b, 0x6d, 0x99, 0x68, 0x9d, 0x12, 0x35, 0x8e, 0xec, 0xa0, 0x4b, 0x14, 0x12, 0x36,
            0xfa, 0x81,
        ];
        assert_eq!(
            pbkdf2(b"pencil", &salt, 4096, 32),
            "c4a49510323ab4f952cac1fa99441939e78ea74d6be81ddf7096e87513dc615d"
        );
        assert_eq!(
            pbkdf2(b"", b"salt", 2, 32),
            "62384466264daadc4144018c6bd864648272b34da8980d31521ffcce92ae003b"
        );
        assert_eq!(
            pbkdf2(b"password", b"", 2, 32),
            "8bc2f9167a81cdcfad1235cd9047f1136271c1f978fcfcb35e22dbeafa4634f6"
        );
        assert_eq!(pbkdf2(b"password", b"salt", 2, 1), "ae");
        assert_eq!(
            pbkdf2(b"password", b"salt", 2, 31),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c"
        );
        assert_eq!(
            pbkdf2(b"password", b"salt", 3, 33),
            "ad35240ac683febfaf3cd49d845473fbbbaa2437f5f82d5a415ae00ac76c6bfccf"
        );
        assert_eq!(
            pbkdf2(b"pass\0word", b"sa\0lt", 4096, 65),
            "89b69d0516f829893c696226650a86878c029ac13ee276509d5ae58b6466a724\
             22433a5c04e21eb0279e22f185520546d5e4cf9de5dd0bdb6de357648085700f\
             95"
        );
        assert_eq!(
            pbkdf2(&[b'k'; 100], &b"salt".repeat(20), 7, 96),
            "8c50825995f444f597c6dbd2eadc4fb57c8dc2685fe08c49bf158c7caa6e292c\
             81c7a031ebe97efc724aac490cff3da9dd3c56bebb32bb25048bc390ac79d274\
             bde71df2061cde0f8beed6acafbaaca8a4632f06cc1b73c78eea0da5e9f41282"
        );
    }

    #[test]
    fn pbkdf2_refuses_without_writing() {
        for (iterations, length) in [(0, 32), (1, 0), (1, super::PBKDF2_MAX_OUTPUT + 1)] {
            let mut out = vec![7u8; length];
            assert_eq!(
                super::pbkdf2_sha256(b"password", b"salt", iterations, &mut out),
                Err(crate::Refused)
            );
            assert!(out.iter().all(|&byte| byte == 7));
        }
        let mut longest = vec![0u8; super::PBKDF2_MAX_OUTPUT];
        assert_eq!(
            super::pbkdf2_sha256(b"password", b"salt", 1, &mut longest),
            Ok(())
        );
        // The 32nd block, from Python's hashlib.
        assert_eq!(
            hex(&longest[992..]),
            "f37cbd455c23dca0b7da1cacd2eca89f1b9a26e4da57476fffa4eca2a95a7270"
        );
    }
}
