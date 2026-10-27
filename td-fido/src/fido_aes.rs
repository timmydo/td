//! Bounded AES-256-CBC primitive for CTAP, AES-128-CFB for td-tpm's
//! session parameter encryption, AES-128 key unwrap for td-netd's
//! WPA2 group keys, and AES-256-XTS for td-luks's LUKS2 keyslot areas. No
//! padding or other authentication here.

const MAX_BYTES: usize = 128;
/// td-tpm's MAX_PACKET: no TPM parameter is longer.
const MAX_CFB_BYTES: usize = 4096;
/// Key unwrap's longest plaintext: an EAPOL-Key frame's whole key data
/// field fits a 16-bit length, and no WPA2 key data comes near this.
const MAX_UNWRAP_BYTES: usize = 1024;
/// RFC 3394's default initial value, the unwrap's integrity check.
const WRAP_IV: [u8; 8] = [0xa6; 8];

pub fn encrypt(key: &[u8; 32], iv: &[u8; 16], bytes: &mut [u8]) -> Result<(), &'static str> {
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

pub fn decrypt(key: &[u8; 32], iv: &[u8; 16], bytes: &mut [u8]) -> Result<(), &'static str> {
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

/// AES-128 in CFB mode with 128-bit feedback, as TPM 2.0 parameter
/// encryption uses it (Part 1, "CFB Mode Parameter Encryption"): a short
/// last block takes the leading bytes of its keystream. An empty input is
/// left as it is.
pub fn cfb_encrypt(key: &[u8; 16], iv: &[u8; 16], bytes: &mut [u8]) -> Result<(), &'static str> {
    cfb(key, iv, bytes, true)
}

pub fn cfb_decrypt(key: &[u8; 16], iv: &[u8; 16], bytes: &mut [u8]) -> Result<(), &'static str> {
    cfb(key, iv, bytes, false)
}

fn cfb(key: &[u8; 16], iv: &[u8; 16], bytes: &mut [u8], encrypt: bool) -> Result<(), &'static str> {
    if bytes.len() > MAX_CFB_BYTES {
        return Err("AES-CFB input exceeds 4096 bytes");
    }
    let aes = Aes128::new(key);
    let mut feedback = *iv;
    for chunk in bytes.chunks_mut(16) {
        let mut pad = feedback;
        aes.encrypt_block(&mut pad);
        for (byte, (pad, next)) in chunk.iter_mut().zip(pad.iter().zip(feedback.iter_mut())) {
            let ciphertext = if encrypt { *byte ^ pad } else { *byte };
            *byte ^= pad;
            *next = ciphertext;
        }
        pad.fill(0);
        std::hint::black_box(&mut pad);
    }
    Ok(())
}

/// RFC 3394 AES key unwrap under a 128-bit key-encryption key, as WPA2's
/// EAPOL-Key key data uses it (IEEE 802.11-2020 12.7.2): `wrapped` is
/// `out.len() + 8` bytes, `out` at least two and at most 128 whole 64-bit
/// blocks. An integrity failure zeroes `out` and is refused.
pub fn key_unwrap_128(kek: &[u8; 16], wrapped: &[u8], out: &mut [u8]) -> Result<(), &'static str> {
    if out.len() < 16
        || out.len() > MAX_UNWRAP_BYTES
        || !out.len().is_multiple_of(8)
        || wrapped.len() != out.len() + 8
    {
        return Err("AES key unwrap needs 2 to 128 whole blocks and one more of check");
    }
    let (initial, rest) = wrapped.split_at(8);
    out.copy_from_slice(rest);
    let mut check = [0u8; 8];
    check.copy_from_slice(initial);
    let aes = Aes128::new(kek);
    let blocks = out.len() / 8;
    let mut state = [0u8; 16];
    for round in (0..6u64).rev() {
        for (index, block) in out.as_chunks_mut::<8>().0.iter_mut().enumerate().rev() {
            // t = n*j + i, RFC 3394 2.2.2 with i counting from one.
            let t = (blocks as u64) * round + index as u64 + 1;
            for (byte, step) in check.iter_mut().zip(t.to_be_bytes()) {
                *byte ^= step;
            }
            let (high, low) = state.split_at_mut(8);
            high.copy_from_slice(&check);
            low.copy_from_slice(block);
            aes.decrypt_block(&mut state);
            let (high, low) = state.split_at(8);
            check.copy_from_slice(high);
            block.copy_from_slice(low);
        }
    }
    state.fill(0);
    std::hint::black_box(&mut state);
    let difference = check
        .iter()
        .zip(WRAP_IV)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    if std::hint::black_box(difference) != 0 {
        out.fill(0);
        std::hint::black_box(&mut *out);
        return Err("AES key unwrap integrity check failed");
    }
    Ok(())
}

/// The most bytes one XTS call takes: the whole keyslots area of the LUKS2
/// header td formats (td-luks/DESIGN.md "The profile") fits.
pub const MAX_XTS_BYTES: usize = 16 * 1024 * 1024;
/// The largest XTS data unit, dm-crypt's largest sector.
const MAX_XTS_UNIT: usize = 4096;

/// AES-256-XTS (IEEE 1619, SP 800-38E) encryption of `bytes` as
/// consecutive data units of `unit` bytes, the first numbered `sector`,
/// each unit's tweak its number little-endian, as dm-crypt's plain64 IV
/// gives it. `key` is the data key, then the tweak key.
///
/// Refused, with `bytes` untouched: equal key halves, a unit that is not
/// 1 to 256 whole blocks, an empty input, a length that is not whole
/// units or exceeds `MAX_XTS_BYTES`, and unit numbers past `u64::MAX`.
pub fn xts_encrypt(
    key: &[u8; 64],
    sector: u64,
    unit: usize,
    bytes: &mut [u8],
) -> Result<(), &'static str> {
    xts_256(key, sector, unit, bytes, true)
}

/// The inverse of `xts_encrypt`, under the same refusals.
pub fn xts_decrypt(
    key: &[u8; 64],
    sector: u64,
    unit: usize,
    bytes: &mut [u8],
) -> Result<(), &'static str> {
    xts_256(key, sector, unit, bytes, false)
}

fn xts_256(
    key: &[u8; 64],
    sector: u64,
    unit: usize,
    bytes: &mut [u8],
    encrypt: bool,
) -> Result<(), &'static str> {
    let ([data_key, tweak_key], []) = key.as_chunks::<32>() else {
        return Err("AES-256-XTS takes two 32-byte keys");
    };
    // SP 800-38E: the two halves must differ. Compared whatever differs.
    let difference = data_key
        .iter()
        .zip(tweak_key)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    if std::hint::black_box(difference) == 0 {
        return Err("AES-XTS refuses a tweak key equal to its data key");
    }
    validate_xts(sector, unit, bytes.len())?;
    let data = Aes256::new(data_key);
    let tweak = Aes256::new(tweak_key);
    xts(&data, &tweak, sector, unit, bytes, encrypt);
    Ok(())
}

fn validate_xts(sector: u64, unit: usize, size: usize) -> Result<(), &'static str> {
    if unit == 0 || unit > MAX_XTS_UNIT || !unit.is_multiple_of(16) {
        return Err("an AES-XTS data unit is 1 to 256 whole blocks");
    }
    if size == 0 || size > MAX_XTS_BYTES || !size.is_multiple_of(unit) {
        return Err("AES-XTS input must be 1 to 16 MiB of whole data units");
    }
    let last = u64::try_from(size / unit - 1).map_err(|_| "too many AES-XTS data units")?;
    sector
        .checked_add(last)
        .ok_or("AES-XTS data unit numbers pass the largest sector")?;
    Ok(())
}

/// The block cipher XTS runs over, so tests drive the mode with AES-128's
/// vectors as well.
trait BlockCipher {
    fn encrypt(&self, state: &mut [u8; 16]);
    fn decrypt(&self, state: &mut [u8; 16]);
}

impl BlockCipher for Aes256 {
    fn encrypt(&self, state: &mut [u8; 16]) {
        self.encrypt_block(state);
    }
    fn decrypt(&self, state: &mut [u8; 16]) {
        self.decrypt_block(state);
    }
}

#[cfg(test)]
impl BlockCipher for Aes128 {
    fn encrypt(&self, state: &mut [u8; 16]) {
        self.encrypt_block(state);
    }
    fn decrypt(&self, state: &mut [u8; 16]) {
        self.decrypt_block(state);
    }
}

/// XTS over validated input: each unit's tweak is its number encrypted
/// under the tweak key, multiplied by the primitive element per block.
fn xts<C: BlockCipher>(
    data: &C,
    tweak: &C,
    sector: u64,
    unit: usize,
    bytes: &mut [u8],
    encrypt: bool,
) {
    let mut number = sector;
    let mut mask = [0u8; 16];
    for data_unit in bytes.chunks_exact_mut(unit) {
        mask = [0; 16];
        for (byte, value) in mask.iter_mut().zip(number.to_le_bytes()) {
            *byte = value;
        }
        tweak.encrypt(&mut mask);
        for block in data_unit.as_chunks_mut::<16>().0 {
            xor(block, &mask);
            if encrypt {
                data.encrypt(block);
            } else {
                data.decrypt(block);
            }
            xor(block, &mask);
            double(&mut mask);
        }
        // The last unit's number was checked; only its successor may wrap.
        number = number.wrapping_add(1);
    }
    mask.fill(0);
    std::hint::black_box(&mut mask);
}

/// Multiplication by x in GF(2^128) as XTS orders the bits, little-endian,
/// reducing by x^128 + x^7 + x^2 + x + 1 without a branch on the tweak.
fn double(mask: &mut [u8; 16]) {
    let mut carry = 0u8;
    for byte in mask.iter_mut() {
        let next = *byte >> 7;
        *byte = (*byte << 1) | carry;
        carry = next;
    }
    if let Some(first) = mask.first_mut() {
        *first ^= 0x87 & 0u8.wrapping_sub(carry);
    }
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
        clear(self.rounds.as_mut());
    }
}

struct Aes128 {
    rounds: Box<[[u8; 16]; 11]>,
}

impl Drop for Aes128 {
    fn drop(&mut self) {
        clear(self.rounds.as_mut());
    }
}

fn clear(rounds: &mut [[u8; 16]]) {
    for key in rounds.iter_mut() {
        key.fill(0);
    }
    std::hint::black_box(rounds);
}

impl Aes128 {
    fn new(key: &[u8; 16]) -> Self {
        let mut aes = Self {
            rounds: Box::new([[0; 16]; 11]),
        };
        let mut words = [0u32; 4];
        for (word, bytes) in words.iter_mut().zip(key.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        let mut rcon = 1u8;
        for (round, output) in aes.rounds.iter_mut().enumerate() {
            if round > 0 {
                let [a, b, c, d] = words;
                let t = sub_word(d.rotate_left(8)) ^ (u32::from(rcon) << 24);
                rcon = xtime(rcon);
                words = [a ^ t, b ^ a ^ t, c ^ b ^ a ^ t, d ^ c ^ b ^ a ^ t];
            }
            for (bytes, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(words) {
                *bytes = word.to_be_bytes();
            }
        }
        words.fill(0);
        std::hint::black_box(&mut words);
        aes
    }

    fn encrypt_block(&self, state: &mut [u8; 16]) {
        let [first, middle @ .., last] = &*self.rounds;
        encrypt_rounds(first, middle, last, state);
    }

    fn decrypt_block(&self, state: &mut [u8; 16]) {
        let [first, middle @ .., last] = &*self.rounds;
        decrypt_rounds(first, middle, last, state);
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
        encrypt_rounds(first, middle, last, state);
    }

    fn decrypt_block(&self, state: &mut [u8; 16]) {
        let [first, middle @ .., last] = &*self.rounds;
        decrypt_rounds(first, middle, last, state);
    }
}

fn decrypt_rounds(first: &[u8; 16], middle: &[[u8; 16]], last: &[u8; 16], state: &mut [u8; 16]) {
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

fn encrypt_rounds(first: &[u8; 16], middle: &[[u8; 16]], last: &[u8; 16], state: &mut [u8; 16]) {
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
    fn fips197_aes128_block_and_schedule() {
        let aes = Aes128::new(&hex("000102030405060708090a0b0c0d0e0f"));
        let mut block = hex("00112233445566778899aabbccddeeff");
        aes.encrypt_block(&mut block);
        assert_eq!(block, hex("69c4e0d86a7b0430d8cdb78070b4c55a"));
        aes.decrypt_block(&mut block);
        assert_eq!(block, hex("00112233445566778899aabbccddeeff"));
        // FIPS 197 A.1's last round key.
        assert_eq!(
            Aes128::new(&hex("2b7e151628aed2a6abf7158809cf4f3c")).rounds[10],
            hex::<16>("d014f9a8c9ee2589e13f0cc8b6630ca6")
        );
    }

    /// SP 800-38A F.3.13 and F.3.14, whole and cut short: a partial last
    /// block is the leading bytes of the whole one's.
    #[test]
    fn nist_sp800_38a_cfb128_aes128_both_directions() {
        let key = hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex("000102030405060708090a0b0c0d0e0f");
        let plain: [u8; 64] = hex(concat!(
            "6bc1bee22e409f96e93d7e117393172a",
            "ae2d8a571e03ac9c9eb76fac45af8e51",
            "30c81c46a35ce411e5fbc1191a0a52ef",
            "f69f2445df4f9b17ad2b417be66c3710",
        ));
        let sealed: [u8; 64] = hex(concat!(
            "3b3fd92eb72dad20333449f8e83cfb4ac8a64537a0b3a93fcde3cdad9f1ce58b",
            "26751f67a3cbb140b1808cf187a4f4dfc04b05357c5d1c0eeac4c66f9ff7f2e6",
        ));
        for size in 0..=64 {
            let mut encrypted = plain[..size].to_vec();
            cfb_encrypt(&key, &iv, &mut encrypted).unwrap();
            assert_eq!(encrypted, sealed[..size]);
            cfb_decrypt(&key, &iv, &mut encrypted).unwrap();
            assert_eq!(encrypted, plain[..size]);
        }
        let mut long = vec![0xa5; MAX_CFB_BYTES + 1];
        assert!(cfb_encrypt(&key, &iv, &mut long).is_err());
        assert!(cfb_decrypt(&key, &iv, &mut long).is_err());
        assert!(long.iter().all(|byte| *byte == 0xa5));
        long.pop();
        cfb_encrypt(&key, &iv, &mut long).unwrap();
        assert!(long.iter().any(|byte| *byte != 0xa5));
        cfb_decrypt(&key, &iv, &mut long).unwrap();
        assert!(long.iter().all(|byte| *byte == 0xa5));
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

    /// RFC 3394 2.2.1's wrap, the reference the unwrap must invert.
    fn wrap(kek: &[u8; 16], plain: &[u8]) -> Vec<u8> {
        let aes = Aes128::new(kek);
        let mut check = WRAP_IV;
        let mut blocks: Vec<[u8; 8]> = plain.as_chunks::<8>().0.iter().copied().collect();
        let n = blocks.len() as u64;
        for round in 0..6u64 {
            for (index, block) in blocks.iter_mut().enumerate() {
                let mut state = [0u8; 16];
                state[..8].copy_from_slice(&check);
                state[8..].copy_from_slice(block);
                aes.encrypt_block(&mut state);
                let t = n * round + index as u64 + 1;
                for (out, (byte, step)) in
                    check.iter_mut().zip(state[..8].iter().zip(t.to_be_bytes()))
                {
                    *out = byte ^ step;
                }
                block.copy_from_slice(&state[8..]);
            }
        }
        let mut out = check.to_vec();
        for block in blocks {
            out.extend_from_slice(&block);
        }
        out
    }

    #[test]
    fn rfc3394_unwraps_its_128_bit_vector() {
        let kek = hex("000102030405060708090a0b0c0d0e0f");
        let wrapped = hex::<24>("1fa68b0a8112b447aef34bd8fb5a7b829d3e862371d2cfe5");
        let mut out = [0u8; 16];
        key_unwrap_128(&kek, &wrapped, &mut out).unwrap();
        assert_eq!(out, hex("00112233445566778899aabbccddeeff"));
        assert_eq!(wrap(&kek, &out), wrapped.to_vec());
    }

    /// A WPA2 group key's key data, a KDE with padding, round trips at
    /// every admitted length; any flipped bit is refused with `out`
    /// zeroed.
    #[test]
    fn unwrap_inverts_wrap_and_refuses_any_change() {
        let kek = hex("2b7e151628aed2a6abf7158809cf4f3c");
        for blocks in 2..=128usize {
            let plain: Vec<u8> = (0..blocks * 8).map(|i| (i * 7 + blocks) as u8).collect();
            let wrapped = wrap(&kek, &plain);
            let mut out = vec![0u8; plain.len()];
            key_unwrap_128(&kek, &wrapped, &mut out).unwrap();
            assert_eq!(out, plain, "{blocks}");
        }
        let plain: Vec<u8> = (0..40).collect();
        let wrapped = wrap(&kek, &plain);
        // Python `cryptography`'s aes_key_wrap over the same key and bytes.
        assert_eq!(
            wrapped,
            hex::<48>("278305838c7c6e299d87212e3a681c4c1a6590d9f6fdf70dc5945d8245a7c1b4bc3ecdd85f892a1ea7c0468d09de001e").to_vec()
        );
        for bit in 0..wrapped.len() * 8 {
            let mut changed = wrapped.clone();
            changed[bit / 8] ^= 1 << (bit % 8);
            let mut out = vec![0x55u8; plain.len()];
            assert!(key_unwrap_128(&kek, &changed, &mut out).is_err(), "{bit}");
            assert!(out.iter().all(|byte| *byte == 0), "{bit}");
        }
        let mut out = vec![0u8; plain.len()];
        assert!(
            key_unwrap_128(&hex("000102030405060708090a0b0c0d0e0f"), &wrapped, &mut out).is_err()
        );
    }

    #[test]
    fn unwrap_refuses_lengths_outside_its_bounds() {
        let kek = [0u8; 16];
        for (wrapped, out) in [
            (16, 8),
            (24, 15),
            (23, 16),
            (25, 16),
            (32, 16),
            (1040, 1032),
            (8, 0),
        ] {
            let mut buffer = vec![0x33u8; out];
            assert!(key_unwrap_128(&kek, &vec![0; wrapped], &mut buffer).is_err());
            assert!(buffer.iter().all(|byte| *byte == 0x33));
        }
    }

    /// IEEE 1619-2007 Annex B XTS-AES 10 (AES-256, data unit 0xff of 512
    /// bytes), as Linux's crypto/testmgr.h and Go's x/crypto/xts carry it,
    /// the two sources byte for byte the same.
    const XTS_AES_10_KEY: &str = concat!(
        "2718281828459045235360287471352662497757247093699959574966967627",
        "3141592653589793238462643383279502884197169399375105820974944592",
    );
    const XTS_AES_10_SEALED: &str = concat!(
        "1c3b3a102f770386e4836c99e370cf9bea00803f5e482357a4ae12d414a3e63b",
        "5d31e276f8fe4a8d66b317f9ac683f44680a86ac35adfc3345befecb4bb188fd",
        "5776926c49a3095eb108fd1098baec70aaa66999a72a82f27d848b21d4a741b0",
        "c5cd4d5fff9dac89aeba122961d03a757123e9870f8acf1000020887891429ca",
        "2a3e7a7d7df7b10355165c8b9a6d0a7de8b062c4500dc4cd120c0f7418dae3d0",
        "b5781c34803fa75421c790dfe1de1834f280d7667b327f6c8cd7557e12ac3a0f",
        "93ec05c52e0493ef31a12d3d9260f79a289d6a379bc70c50841473d1a8cc81ec",
        "583e9645e07b8d9670655ba5bbcfecc6dc3966380ad8fecb17b6ba02469a020a",
        "84e18e8f84252070c13e9f1f289be54fbc481457778f616015e1327a02b140f1",
        "505eb309326d68378f8374595c849d84f4c333ec4423885143cb47bd71c5edae",
        "9be69a2ffeceb1bec9de244fbe15992b11b77c040f12bd8f6a975a44a0f90c29",
        "a9abc3d4d893927284c58754cce294529f8614dcd2aba991925fedc4ae74ffac",
        "6e333b93eb4aff0479da9a410e4450e0dd7ae4c6e2910900575da401fc07059f",
        "645e8b7e9bfdef33943054ff84011493c27b3429eaedb4ed5376441a77ed4385",
        "1ad77f16f541dfd269d50d6a5f14fb0aab1cbb4c1550be97f7ab4066193c4caa",
        "773dad38014bd2092fa755c824bb5e54c4f36ffda9fcea70b9c6e693e148c151",
    );
    /// XTS-AES 4's sealed unit 0, which XTS-AES 5 seals again as unit 1.
    const XTS_AES_4_SEALED: &str = concat!(
        "27a7479befa1d476489f308cd4cfa6e2a96e4bbe3208ff25287dd3819616e89c",
        "c78cf7f5e543445f8333d8fa7f56000005279fa5d8b5e4ad40e736ddb4d35412",
        "328063fd2aab53e5ea1e0a9f332500a5df9487d07a5c92cc512c8866c7e860ce",
        "93fdf166a24912b422976146ae20ce846bb7dc9ba94a767aaef20c0d61ad0265",
        "5ea92dc4c4e41a8952c651d33174be51a10c421110e6d81588ede82103a252d8",
        "a750e8768defffed9122810aaeb99f9172af82b604dc4b8e51bcb08235a6f434",
        "1332e4ca60482a4ba1a03b3e65008fc5da76b70bf1690db4eae29c5f1badd03c",
        "5ccf2a55d705ddcd86d449511ceb7ec30bf12b1fa35b913f9f747a8afd1b130e",
        "94bff94effd01a91735ca1726acd0b197c4e5b03393697e126826fb6bbde8ecc",
        "1e08298516e2c9ed03ff3c1b7860f6de76d4cecd94c8119855ef5297ca67e9f3",
        "e7ff72b1e99785ca0a7e7720c5b36dc6d72cac9574c8cbbc2f801e23e56fd344",
        "b07f22154beba0f08ce8891e643ed995c94d9a69c9f1b5f499027a78572aeebd",
        "74d20cc39881c213ee770b1010e4bea718846977ae119f7a023ab58cca0ad752",
        "afe656bb3c17256a9f6e9bf19fdd5a38fc82bbe872c5539edb609ef4f79c203e",
        "bb140f2e583cb2ad15b4aa5b655016a8449277dbd477ef2c8d6c017db738b18d",
        "eb4a427d1923ce3ff262735779a418f20a282df920147beabe421ee5319d0568",
    );
    const XTS_AES_5_SEALED: &str = concat!(
        "264d3ca8512194fec312c8c9891f279fefdd608d0c027b60483a3fa811d65ee5",
        "9d52d9e40ec5672d81532b38b6b089ce951f0f9c35590b8b978d175213f329bb",
        "1c2fd30f2f7f30492a61a532a79f51d36f5e31a7c9a12c286082ff7d2394d18f",
        "783e1a8e72c722caaaa52d8f065657d2631fd25bfd8e5baad6e527d763517501",
        "c68c5edc3cdd55435c532d7125c8614deed9adaa3acade5888b87bef641c4c99",
        "4c8091b5bcd387f3963fb5bc37aa922fbfe3df4e5b915e6eb514717bdd2a7407",
        "9a5073f5c4bfd46adf7d282e7a393a52579d11a028da4d9cd9c77124f9648ee3",
        "83b1ac763930e7162a8d37f350b2f74b8472cf09902063c6b32e8c2d9290cefb",
        "d7346d1c779a0df50edcde4531da07b099c638e83a755944df2aef1aa31752fd",
        "323dcb710fb4bfbb9d22b925bc3577e1b8949e729a90bbafeacf7f7879e7b114",
        "7e28ba0bae940db795a61b15ecf4df8db07b824bb062802cc98a9545bb2aaeed",
        "77cb3fc6db15dcd7d80d7d5bc406c4970a3478ada8899b329198eb61c193fb62",
        "75aa8ca340344a75a862aebe92eee1ce032fd950b47d7704a3876923b4ad6284",
        "4bf4a09c4dbe8b4397184b7471360c9564880aedddb9baa4af2e75394b08cd32",
        "ff479c57a07d3eab5d54de5f9738b8d27f27a9f0ab11799d7b7ffefb2704c95c",
        "6ad12c39f1e867a4b7b1d7818a4b753dfd2a89ccb45e001a03a867b187f225dd",
    );

    fn counting() -> [u8; 512] {
        std::array::from_fn(|index| index as u8)
    }

    /// XTS-AES 1 to 5 (AES-128): unit numbers 0, 1 and 0x3333333333, the
    /// last placing five little-endian bytes in the tweak, and 512-byte
    /// units chaining 32 tweak doublings.
    #[test]
    fn ieee1619_xts_aes128_vectors_drive_the_mode() {
        let run = |key: &[u8; 32], sector: u64, bytes: &mut [u8], encrypt: bool| {
            let [data, tweak] = key.as_chunks::<16>().0 else {
                panic!("two halves")
            };
            let (data, tweak) = (Aes128::new(data), Aes128::new(tweak));
            xts(&data, &tweak, sector, bytes.len(), bytes, encrypt);
        };
        let short: &[(&str, u64, &str, &str)] = &[
            (
                "0000000000000000000000000000000000000000000000000000000000000000",
                0,
                "0000000000000000000000000000000000000000000000000000000000000000",
                "917cf69ebd68b2ec9b9fe9a3eadda692cd43d2f59598ed858c02c2652fbf922e",
            ),
            (
                "1111111111111111111111111111111122222222222222222222222222222222",
                0x3333333333,
                "4444444444444444444444444444444444444444444444444444444444444444",
                "c454185e6a16936e39334038acef838bfb186fff7480adc4289382ecd6d394f0",
            ),
            (
                "fffefdfcfbfaf9f8f7f6f5f4f3f2f1f022222222222222222222222222222222",
                0x3333333333,
                "4444444444444444444444444444444444444444444444444444444444444444",
                "af85336b597afc1a900b2eb21ec949d292df4c047e0b21532186a5971a227a89",
            ),
        ];
        for (key, sector, plain, sealed) in short {
            let key = hex::<32>(key);
            let mut bytes = hex::<32>(plain);
            run(&key, *sector, &mut bytes, true);
            assert_eq!(bytes, hex::<32>(sealed));
            run(&key, *sector, &mut bytes, false);
            assert_eq!(bytes, hex::<32>(plain));
        }
        let key = hex::<32>("2718281828459045235360287471352631415926535897932384626433832795");
        let mut bytes = counting();
        run(&key, 0, &mut bytes, true);
        assert_eq!(bytes, hex::<512>(XTS_AES_4_SEALED));
        run(&key, 1, &mut bytes, true);
        assert_eq!(bytes, hex::<512>(XTS_AES_5_SEALED));
        run(&key, 1, &mut bytes, false);
        run(&key, 0, &mut bytes, false);
        assert_eq!(bytes, counting());
    }

    #[test]
    fn ieee1619_xts_aes256_vector_both_directions() {
        let key = hex::<64>(XTS_AES_10_KEY);
        let mut bytes = counting();
        xts_encrypt(&key, 0xff, 512, &mut bytes).unwrap();
        assert_eq!(bytes, hex::<512>(XTS_AES_10_SEALED));
        xts_decrypt(&key, 0xff, 512, &mut bytes).unwrap();
        assert_eq!(bytes, counting());
    }

    /// One call over consecutive units numbers them from `sector`: XTS-AES
    /// 10's unit sealed second of two from 0xfe, and each unit alone.
    #[test]
    fn xts_numbers_consecutive_units() {
        let key = hex::<64>(XTS_AES_10_KEY);
        let mut both = [[0x5a; 512], counting()].concat();
        xts_encrypt(&key, 0xfe, 512, &mut both).unwrap();
        assert_eq!(both[512..], hex::<512>(XTS_AES_10_SEALED));
        let mut first = [0x5a; 512];
        xts_encrypt(&key, 0xfe, 512, &mut first).unwrap();
        assert_eq!(both[..512], first);
        // Smaller units renumber: the same bytes seal differently.
        let mut halves = counting();
        xts_encrypt(&key, 0xff, 256, &mut halves).unwrap();
        assert_ne!(halves, hex::<512>(XTS_AES_10_SEALED));
        xts_decrypt(&key, 0xff, 256, &mut halves).unwrap();
        assert_eq!(halves, counting());
    }

    #[test]
    fn xts_refuses_and_leaves_input_untouched() {
        let key = hex::<64>(XTS_AES_10_KEY);
        let mut same = [7u8; 64];
        let mut bytes = vec![0x33; 1024];
        assert!(xts_encrypt(&same, 0, 512, &mut bytes).is_err());
        assert!(xts_decrypt(&same, 0, 512, &mut bytes).is_err());
        same[63] = 8;
        xts_encrypt(&same, 0, 512, &mut bytes).unwrap();
        xts_decrypt(&same, 0, 512, &mut bytes).unwrap();
        for unit in [0, 8, 15, 17, 4096 + 16, 8192] {
            assert!(xts_encrypt(&key, 0, unit, &mut bytes).is_err(), "{unit}");
            assert!(xts_decrypt(&key, 0, unit, &mut bytes).is_err(), "{unit}");
        }
        assert!(xts_encrypt(&key, 0, 512, &mut []).is_err());
        assert!(xts_encrypt(&key, 0, 512, &mut bytes[..1000]).is_err());
        assert!(xts_encrypt(&key, u64::MAX, 512, &mut bytes).is_err());
        assert!(bytes.iter().all(|byte| *byte == 0x33));
        xts_encrypt(&key, u64::MAX - 1, 512, &mut bytes).unwrap();
        xts_decrypt(&key, u64::MAX - 1, 512, &mut bytes).unwrap();
        assert!(bytes.iter().all(|byte| *byte == 0x33));
        let mut long = vec![0x33; MAX_XTS_BYTES + 4096];
        assert!(xts_encrypt(&key, 0, 4096, &mut long).is_err());
        assert!(long.iter().all(|byte| *byte == 0x33));
        // The bound itself, without sealing 16 MiB in a test build.
        assert!(validate_xts(0, 4096, MAX_XTS_BYTES).is_ok());
        assert!(validate_xts(u64::MAX - 4095, 4096, MAX_XTS_BYTES).is_ok());
        assert!(validate_xts(u64::MAX - 4094, 4096, MAX_XTS_BYTES).is_err());
    }

    /// The tweak doubling carries across every byte and reduces by 0x87.
    #[test]
    fn xts_doubling_carries_and_reduces() {
        let mut mask = [0u8; 16];
        mask[0] = 0x80;
        double(&mut mask);
        assert_eq!(mask, hex::<16>("00010000000000000000000000000000"));
        let mut mask = [0u8; 16];
        mask[15] = 0x80;
        double(&mut mask);
        assert_eq!(mask, hex::<16>("87000000000000000000000000000000"));
        let mut mask = [0xff; 16];
        double(&mut mask);
        assert_eq!(mask, hex::<16>("79ffffffffffffffffffffffffffffff"));
    }
}
