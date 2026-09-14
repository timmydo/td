//! Bounded AES-256-CBC primitive for CTAP. No padding or authentication here.

const MAX_BYTES: usize = 128;

pub(super) fn encrypt(key: &[u8; 32], iv: &[u8; 16], bytes: &mut [u8]) -> Result<(), &'static str> {
    validate_length(bytes.len())?;
    let aes = Aes256::new(key);
    let mut previous = *iv;
    for block in bytes.as_chunks_mut::<16>().0 {
        xor(block, &previous);
        aes.encrypt_block(block);
        previous = *block;
    }
    Ok(())
}

pub(super) fn decrypt(key: &[u8; 32], iv: &[u8; 16], bytes: &mut [u8]) -> Result<(), &'static str> {
    validate_length(bytes.len())?;
    let aes = Aes256::new(key);
    let mut previous = *iv;
    for block in bytes.as_chunks_mut::<16>().0 {
        let ciphertext = *block;
        aes.decrypt_block(block);
        xor(block, &previous);
        previous = ciphertext;
    }
    Ok(())
}

fn validate_length(size: usize) -> Result<(), &'static str> {
    if size == 0 || size > MAX_BYTES || !size.is_multiple_of(16) {
        return Err("CTAP AES input must contain one through eight complete blocks");
    }
    Ok(())
}

// Heap ownership keeps schedule moves from copying secret-bearing arrays.
struct Aes256 {
    rounds: Box<[[u8; 16]; 15]>,
}

impl Drop for Aes256 {
    fn drop(&mut self) {
        for key in self.rounds.iter_mut() {
            key.fill(0);
        }
        std::hint::black_box(self.rounds.as_mut());
    }
}

impl Aes256 {
    fn new(key: &[u8; 32]) -> Self {
        let mut aes = Self {
            rounds: Box::new([[0; 16]; 15]),
        };
        let mut words = [0u32; 8];
        for (word, bytes) in words.iter_mut().zip(key.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        let mut rcon = 1u8;
        for (round, output) in aes.rounds.iter_mut().enumerate() {
            if round >= 2 {
                let [a, b, c, d, e, f, g, h] = words;
                let t = if round.is_multiple_of(2) {
                    let t = sub_word(h.rotate_left(8)) ^ (u32::from(rcon) << 24);
                    rcon = xtime(rcon);
                    t
                } else {
                    sub_word(h)
                };
                words = [
                    e,
                    f,
                    g,
                    h,
                    a ^ t,
                    b ^ a ^ t,
                    c ^ b ^ a ^ t,
                    d ^ c ^ b ^ a ^ t,
                ];
            }
            let [a, b, c, d, e, f, g, h] = words;
            let selected = if round == 0 {
                [a, b, c, d]
            } else {
                [e, f, g, h]
            };
            for (bytes, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(selected) {
                *bytes = word.to_be_bytes();
            }
        }
        words.fill(0);
        std::hint::black_box(&mut words);
        aes
    }

    fn encrypt_block(&self, state: &mut [u8; 16]) {
        let [first, middle @ .., last] = &*self.rounds;
        xor(state, first);
        for key in middle {
            substitute(state, sbox);
            shift_rows(state);
            mix_columns(state);
            xor(state, key);
        }
        substitute(state, sbox);
        shift_rows(state);
        xor(state, last);
    }

    fn decrypt_block(&self, state: &mut [u8; 16]) {
        let [first, middle @ .., last] = &*self.rounds;
        xor(state, last);
        for key in middle.iter().rev() {
            inverse_shift_rows(state);
            substitute(state, inverse_sbox);
            xor(state, key);
            inverse_mix_columns(state);
        }
        inverse_shift_rows(state);
        substitute(state, inverse_sbox);
        xor(state, first);
    }
}

fn xor(state: &mut [u8; 16], key: &[u8; 16]) {
    for (byte, key_byte) in state.iter_mut().zip(key) {
        *byte ^= key_byte;
    }
}

fn xtime(x: u8) -> u8 {
    (x << 1) ^ (0x1b & 0u8.wrapping_sub(x >> 7))
}

fn multiply(mut a: u8, mut b: u8) -> u8 {
    let mut out = 0;
    for _ in 0..8 {
        out ^= a & 0u8.wrapping_sub(b & 1);
        a = xtime(a);
        b >>= 1;
    }
    out
}

fn inverse(x: u8) -> u8 {
    // x^254 in GF(2^8), also mapping zero to zero as AES requires.
    let x2 = multiply(x, x);
    let x4 = multiply(x2, x2);
    let x8 = multiply(x4, x4);
    let x16 = multiply(x8, x8);
    let x32 = multiply(x16, x16);
    let x64 = multiply(x32, x32);
    let x128 = multiply(x64, x64);
    let x6 = multiply(x2, x4);
    let x14 = multiply(x6, x8);
    let x30 = multiply(x14, x16);
    let x62 = multiply(x30, x32);
    let x126 = multiply(x62, x64);
    multiply(x126, x128)
}

fn sbox(x: u8) -> u8 {
    let x = inverse(x);
    x ^ x.rotate_left(1) ^ x.rotate_left(2) ^ x.rotate_left(3) ^ x.rotate_left(4) ^ 0x63
}

fn inverse_sbox(x: u8) -> u8 {
    inverse(x.rotate_left(1) ^ x.rotate_left(3) ^ x.rotate_left(6) ^ 0x05)
}

fn sub_word(word: u32) -> u32 {
    u32::from_be_bytes(word.to_be_bytes().map(sbox))
}

fn substitute(state: &mut [u8; 16], operation: impl Fn(u8) -> u8) {
    for byte in state {
        *byte = operation(*byte);
    }
}

fn shift_rows(state: &mut [u8; 16]) {
    let [a0, a1, a2, a3, b0, b1, b2, b3, c0, c1, c2, c3, d0, d1, d2, d3] = *state;
    *state = [
        a0, b1, c2, d3, b0, c1, d2, a3, c0, d1, a2, b3, d0, a1, b2, c3,
    ];
}

fn inverse_shift_rows(state: &mut [u8; 16]) {
    let [a0, a1, a2, a3, b0, b1, b2, b3, c0, c1, c2, c3, d0, d1, d2, d3] = *state;
    *state = [
        a0, d1, c2, b3, b0, a1, d2, c3, c0, b1, a2, d3, d0, c1, b2, a3,
    ];
}

fn mix_columns(state: &mut [u8; 16]) {
    for column in state.as_chunks_mut::<4>().0 {
        let [a, b, c, d] = *column;
        let total = a ^ b ^ c ^ d;
        *column = [
            a ^ total ^ xtime(a ^ b),
            b ^ total ^ xtime(b ^ c),
            c ^ total ^ xtime(c ^ d),
            d ^ total ^ xtime(d ^ a),
        ];
    }
}

fn inverse_mix_columns(state: &mut [u8; 16]) {
    for column in state.as_chunks_mut::<4>().0 {
        let [a, b, c, d] = *column;
        let u = xtime(xtime(a ^ c));
        let v = xtime(xtime(b ^ d));
        *column = [a ^ u, b ^ v, c ^ u, d ^ v];
    }
    mix_columns(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex<const N: usize>(text: &str) -> [u8; N] {
        assert_eq!(text.len(), N * 2);
        let mut result = [0; N];
        for (out, pair) in result.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
            *out = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        result
    }

    #[test]
    fn fips197_aes256_block_and_inverse() {
        let key = hex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f");
        let plain = hex("00112233445566778899aabbccddeeff");
        let sealed = hex("8ea2b7ca516745bfeafc49904b496089");
        let aes = Aes256::new(&key);
        let mut block = plain;
        aes.encrypt_block(&mut block);
        assert_eq!(block, sealed);
        let mut block = sealed;
        aes.decrypt_block(&mut block);
        assert_eq!(block, plain);
    }

    #[test]
    fn nist_sp800_38a_cbc256_both_directions() {
        let key = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let plain: [u8; 64] = hex(concat!(
            "6bc1bee22e409f96e93d7e117393172a",
            "ae2d8a571e03ac9c9eb76fac45af8e51",
            "30c81c46a35ce411e5fbc1191a0a52ef",
            "f69f2445df4f9b17ad2b417be66c3710",
        ));
        let sealed: [u8; 64] = hex(concat!(
            "f58c4c04d6e5f1ba779eabfb5f7bfbd69cfc4e967edb808d679f777bc6702c7d",
            "39f23369a9d9bacfa530e26304231461b2eb05e2c39be9fcda6c19078c6a9d1b",
        ));
        let mut encrypted = plain;
        encrypt(&key, &iv, &mut encrypted).unwrap();
        assert_eq!(encrypted, sealed);
        let mut decrypted = sealed;
        decrypt(&key, &iv, &mut decrypted).unwrap();
        assert_eq!(decrypted, plain);
    }

    #[test]
    fn all_invalid_lengths_refuse_without_changing_input() {
        for size in 0..=MAX_BYTES + 32 {
            if size != 0 && size <= MAX_BYTES && size.is_multiple_of(16) {
                continue;
            }
            let original = vec![0xa5; size];
            let mut bytes = original.clone();
            assert!(encrypt(&[0x11; 32], &[0x22; 16], &mut bytes).is_err());
            assert_eq!(bytes, original);
            assert!(decrypt(&[0x11; 32], &[0x22; 16], &mut bytes).is_err());
            assert_eq!(bytes, original);
        }
    }

    #[test]
    fn sboxes_match_every_fips197_value() {
        let table: [u8; 256] = hex(concat!(
            "637c777bf26b6fc53001672bfed7ab76ca82c97dfa5947f0add4a2af9ca472c0",
            "b7fd9326363ff7cc34a5e5f171d8311504c723c31896059a071280e2eb27b275",
            "09832c1a1b6e5aa0523bd6b329e32f8453d100ed20fcb15b6acbbe394a4c58cf",
            "d0efaafb434d338545f9027f503c9fa851a3408f929d38f5bcb6da2110fff3d2",
            "cd0c13ec5f974417c4a77e3d645d197360814fdc222a908846eeb814de5e0bdb",
            "e0323a0a4906245cc2d3ac629195e479e7c8376d8dd54ea96c56f4ea657aae08",
            "ba78252e1ca6b4c6e8dd741f4bbd8b8a703eb5664803f60e613557b986c11d9e",
            "e1f8981169d98e949b1e87e9ce5528df8ca1890dbfe6426841992d0fb054bb16",
        ));
        for (x, expected) in (0..=255u8).zip(table) {
            assert_eq!(sbox(x), expected);
            assert_eq!(inverse_sbox(expected), x);
        }
    }

    #[test]
    fn independent_cbc_vectors_cover_each_admitted_length_and_zero_iv() {
        let vectors = include_str!("../tests/aes_vectors.txt");
        let mut lengths = std::collections::BTreeSet::new();
        let mut zero_iv = false;
        let mut nonzero_iv = false;
        let mut uniform_inputs = std::collections::BTreeSet::new();
        let mut count = 0;
        for line in vectors.lines().filter(|line| !line.starts_with('#')) {
            let fields: Vec<_> = line.split_whitespace().collect();
            let [key, iv, plain, sealed] = fields.as_slice() else {
                panic!("invalid public AES fixture");
            };
            let key = hex(key);
            let iv = hex(iv);
            let decode = |text: &str| -> Vec<u8> {
                assert!(text.len().is_multiple_of(2));
                text.as_bytes()
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                    .collect()
            };
            let plain = decode(plain);
            let sealed = decode(sealed);
            assert_eq!(plain.len(), sealed.len());
            zero_iv |= iv == [0; 16];
            nonzero_iv |= iv != [0; 16];
            for byte in [0, 255] {
                if key == [byte; 32] && iv == [byte; 16] && plain == vec![byte; MAX_BYTES] {
                    uniform_inputs.insert(byte);
                }
            }
            lengths.insert(plain.len());
            let mut actual = plain.clone();
            encrypt(&key, &iv, &mut actual).unwrap();
            assert_eq!(actual, sealed);
            let mut actual = sealed;
            decrypt(&key, &iv, &mut actual).unwrap();
            assert_eq!(actual, plain);
            count += 1;
        }
        assert_eq!(count, 10);
        assert!(zero_iv && nonzero_iv);
        assert_eq!(uniform_inputs, [0, 255].into_iter().collect());
        assert_eq!(
            lengths.into_iter().collect::<Vec<_>>(),
            (1..=8).map(|n| n * 16).collect::<Vec<_>>()
        );
    }
}
