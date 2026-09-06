//! RFC 8439 AEAD and RFC 5869 SHA-256 HKDF. No application imports this module.

#[path = "../../engine/src/sha256.rs"]
#[allow(
    dead_code,
    reason = "the shared hash also supports build artifact files"
)]
mod sha256;

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut normalized = [0u8; 64];
    if key.len() > 64 {
        let mut hash = sha256::Sha256::new();
        hash.update(key);
        for (out, byte) in normalized.iter_mut().zip(hash.finalize()) {
            *out = byte;
        }
    } else {
        for (out, byte) in normalized.iter_mut().zip(key) {
            *out = *byte;
        }
    }
    let mut inner = sha256::Sha256::new();
    inner.update(&normalized.map(|byte| byte ^ 0x36));
    inner.update(data);
    let mut outer = sha256::Sha256::new();
    outer.update(&normalized.map(|byte| byte ^ 0x5c));
    outer.update(&inner.finalize());
    outer.finalize()
}

#[allow(
    dead_code,
    reason = "the console target recipe runs this shared selftest"
)]
pub fn selftest() -> Result<(), String> {
    let expected = [
        0x3c, 0xb2, 0x5f, 0x25, 0xfa, 0xac, 0xd5, 0x7a, 0x90, 0x43, 0x4f, 0x64, 0xd0, 0x36, 0x2f,
        0x2a, 0x2d, 0x2d, 0x0a, 0x90, 0xcf, 0x1a, 0x5a, 0x4c, 0x5d, 0xb0, 0x2d, 0x56, 0xec, 0xc4,
        0xc5, 0xbf,
    ];
    if hkdf(
        &[0x0b; 22],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        &[0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9],
    ) != expected
    {
        return Err("HKDF known-answer selftest failed".into());
    }
    let key = [7u8; 32];
    let nonce = [3u8; 12];
    let sealed = seal(&key, &nonce, b"identity", b"credential");
    if open(&key, &nonce, b"identity", &sealed)? != b"credential"
        || open(&key, &nonce, b"other identity", &sealed).is_ok()
    {
        return Err("credential AEAD selftest failed".into());
    }
    Ok(())
}

pub fn derive(master: &[u8; 32], app: &str) -> [u8; 32] {
    hkdf(master, b"td-secret/store/v1", app.as_bytes())
}

fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hmac(salt, ikm);
    let mut input = info.to_vec();
    input.push(1);
    hmac(&prk, &input)
}

fn quarter([mut a, mut b, mut c, mut d]: [u32; 4]) -> [u32; 4] {
    a = a.wrapping_add(b);
    d = (d ^ a).rotate_left(16);
    c = c.wrapping_add(d);
    b = (b ^ c).rotate_left(12);
    a = a.wrapping_add(b);
    d = (d ^ a).rotate_left(8);
    c = c.wrapping_add(d);
    b = (b ^ c).rotate_left(7);
    [a, b, c, d]
}

fn block(key: &[u8; 32], nonce: &[u8; 12], counter: u32) -> [u8; 64] {
    let mut state = [0u32; 16];
    let words = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]
        .into_iter()
        .chain(
            key.as_chunks::<4>()
                .0
                .iter()
                .map(|word| u32::from_le_bytes(*word)),
        )
        .chain([counter])
        .chain(
            nonce
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| u32::from_le_bytes(*word)),
        );
    for (out, word) in state.iter_mut().zip(words) {
        *out = word;
    }
    let initial = state;
    for _ in 0..10 {
        for indices in [
            [0, 4, 8, 12],
            [1, 5, 9, 13],
            [2, 6, 10, 14],
            [3, 7, 11, 15],
            [0, 5, 10, 15],
            [1, 6, 11, 12],
            [2, 7, 8, 13],
            [3, 4, 9, 14],
        ] {
            let input = indices.map(|index| state.get(index).copied().unwrap_or(0));
            for (index, value) in indices.into_iter().zip(quarter(input)) {
                if let Some(out) = state.get_mut(index) {
                    *out = value;
                }
            }
        }
    }
    let mut output = [0u8; 64];
    for (out, (value, start)) in output
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(state.into_iter().zip(initial))
    {
        *out = value.wrapping_add(start).to_le_bytes();
    }
    output
}

fn crypt(key: &[u8; 32], nonce: &[u8; 12], data: &mut [u8]) {
    for (index, chunk) in data.chunks_mut(64).enumerate() {
        // Store records are bounded to 4096 bytes before entering this module.
        let stream = block(key, nonce, (index as u32).wrapping_add(1));
        for (byte, pad) in chunk.iter_mut().zip(stream) {
            *byte ^= pad;
        }
    }
}

fn limbs(bytes: &[u8; 16]) -> [u64; 5] {
    let n = u128::from_le_bytes(*bytes);
    [
        n as u64 & 0x3ff_ffff,
        (n >> 26) as u64 & 0x3ff_ffff,
        (n >> 52) as u64 & 0x3ff_ffff,
        (n >> 78) as u64 & 0x3ff_ffff,
        (n >> 104) as u64,
    ]
}

fn poly1305(key: &[u8; 32], message: &[u8]) -> [u8; 16] {
    let (r_bytes, s_bytes) = key.split_at(16);
    let mut r = [0u8; 16];
    for (out, byte) in r.iter_mut().zip(r_bytes) {
        *out = *byte;
    }
    let clamp = 0x0fff_fffc_0fff_fffc_0fff_fffc_0fff_ffffu128;
    let [r0, r1, r2, r3, r4] = limbs(&(u128::from_le_bytes(r) & clamp).to_le_bytes());
    let mut h = [0u64; 5];
    for chunk in message.chunks(16) {
        let mut padded = [0u8; 16];
        for (out, byte) in padded.iter_mut().zip(chunk) {
            *out = *byte;
        }
        if let Some(out) = padded.get_mut(chunk.len()) {
            *out = 1;
        }
        let mut n = limbs(&padded);
        if chunk.len() == 16 {
            if let Some(top) = n.last_mut() {
                *top |= 1 << 24;
            }
        }
        for (out, limb) in h.iter_mut().zip(n) {
            *out += limb;
        }
        let [h0, h1, h2, h3, h4] = h;
        let d = [
            h0 * r0 + 5 * (h1 * r4 + h2 * r3 + h3 * r2 + h4 * r1),
            h0 * r1 + h1 * r0 + 5 * (h2 * r4 + h3 * r3 + h4 * r2),
            h0 * r2 + h1 * r1 + h2 * r0 + 5 * (h3 * r4 + h4 * r3),
            h0 * r3 + h1 * r2 + h2 * r1 + h3 * r0 + 5 * h4 * r4,
            h0 * r4 + h1 * r3 + h2 * r2 + h3 * r1 + h4 * r0,
        ];
        let mut carry = 0;
        for (out, limb) in h.iter_mut().zip(d) {
            let value = limb + carry;
            *out = value & 0x3ff_ffff;
            carry = value >> 26;
        }
        if let Some(low) = h.first_mut() {
            *low += carry * 5;
        }
    }
    // Two fixed carry passes normalize the 130-bit accumulator.
    for _ in 0..2 {
        let mut carry = 0;
        for limb in &mut h {
            *limb += carry;
            carry = *limb >> 26;
            *limb &= 0x3ff_ffff;
        }
        if let Some(low) = h.first_mut() {
            *low += carry * 5;
        }
    }
    let mut reduced = h;
    let mut carry = 5;
    for limb in &mut reduced {
        *limb += carry;
        carry = *limb >> 26;
        *limb &= 0x3ff_ffff;
    }
    let mask = 0u64.wrapping_sub(carry);
    for (out, reduced) in h.iter_mut().zip(reduced) {
        *out = (*out & !mask) | (reduced & mask);
    }
    let [h0, h1, h2, h3, h4] = h;
    let n = h0 as u128
        | (h1 as u128) << 26
        | (h2 as u128) << 52
        | (h3 as u128) << 78
        | (h4 as u128) << 104;
    let mut s = [0u8; 16];
    for (out, byte) in s.iter_mut().zip(s_bytes) {
        *out = *byte;
    }
    n.wrapping_add(u128::from_le_bytes(s)).to_le_bytes()
}

fn tag(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], ciphertext: &[u8]) -> [u8; 16] {
    let mut poly_key = [0u8; 32];
    for (out, byte) in poly_key.iter_mut().zip(block(key, nonce, 0)) {
        *out = byte;
    }
    let mut mac = Vec::with_capacity(aad.len() + ciphertext.len() + 48);
    mac.extend_from_slice(aad);
    mac.resize(mac.len().next_multiple_of(16), 0);
    mac.extend_from_slice(ciphertext);
    mac.resize(mac.len().next_multiple_of(16), 0);
    mac.extend_from_slice(&(aad.len() as u64).to_le_bytes());
    mac.extend_from_slice(&(ciphertext.len() as u64).to_le_bytes());
    poly1305(&poly_key, &mac)
}

pub fn seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut ciphertext = plaintext.to_vec();
    crypt(key, nonce, &mut ciphertext);
    let tag = tag(key, nonce, aad, &ciphertext);
    ciphertext.extend_from_slice(&tag);
    ciphertext
}

pub fn open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, String> {
    let length = sealed
        .len()
        .checked_sub(16)
        .ok_or("truncated credential tag")?;
    let (ciphertext, supplied) = sealed.split_at(length);
    let expected = tag(key, nonce, aad, ciphertext);
    let different = supplied
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (*a ^ b));
    if different != 0 {
        return Err("credential authentication failed".into());
    }
    let mut plaintext = ciphertext.to_vec();
    crypt(key, nonce, &mut plaintext);
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bytes(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .chunks(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    #[test]
    fn rfc5869_case_one() {
        assert_eq!(
            hkdf(
                &[0x0b; 22],
                &bytes("000102030405060708090a0b0c"),
                &bytes("f0f1f2f3f4f5f6f7f8f9")
            ),
            bytes("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf").as_slice()
        );
    }
    #[test]
    fn rfc8439_poly1305() {
        let key = bytes("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b")
            .try_into()
            .unwrap();
        assert_eq!(
            poly1305(&key, b"Cryptographic Forum Research Group"),
            bytes("a8061dc1305136c6c22b8baf0c0127a9").as_slice()
        );
    }
    #[test]
    fn rfc8439_aead_and_tampering() {
        let key = bytes("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
            .try_into()
            .unwrap();
        let nonce = bytes("070000004041424344454647").try_into().unwrap();
        let aad = bytes("50515253c0c1c2c3c4c5c6c7");
        let plaintext = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let expected = bytes(concat!(
            "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6",
            "3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36",
            "92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc",
            "3ff4def08e4b7a9de576d26586cec64b6116",
            "1ae10b594f09e26a7e902ecbd0600691"
        ));
        assert_eq!(seal(&key, &nonce, &aad, plaintext), expected);
        assert_eq!(open(&key, &nonce, &aad, &expected).unwrap(), plaintext);
        for index in 0..expected.len() {
            let mut corrupt = expected.clone();
            corrupt[index] ^= 1;
            assert!(open(&key, &nonce, &aad, &corrupt).is_err());
        }
        assert!(open(&key, &nonce, b"other identity", &expected).is_err());
    }
}
