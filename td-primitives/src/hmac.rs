//! HMAC (RFC 2104) over any `Digest`. HKDF and TLS 1.2's PRF are not here:
//! rustls derives both from this through its own `HkdfUsingHmac` and
//! `PrfUsingHmac`. SHA-1 is no `Digest`, and `hmac_sha1` is the only HMAC over it.

use crate::ct;
use crate::sha::{sha1, sha1_of_parts, Digest};

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
}
