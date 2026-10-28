//! Keyslot cryptography for td's LUKS2 profile (DESIGN.md "The profile",
//! "Cryptography"): PBKDF2-HMAC-SHA256 keys and digests, LUKS1-style AF
//! splitting with SHA-256 diffusion, and AES-256-XTS keyslot areas in
//! 512-byte sectors whose IV counts from 0 at the area's offset.

use std::fs::File;
use std::io::{self, Read};
use td_fido::fido_aes::{xts_decrypt, xts_encrypt};

/// The volume key, and so each keyslot area key and AF block: 512 bits.
pub const KEY_LEN: usize = 64;
/// LUKS1-style AF stripes.
pub const STRIPES: usize = 4000;
/// A keyslot's split key, encrypted into its area: 500 sectors.
pub const SPLIT_LEN: usize = KEY_LEN * STRIPES;
/// A keyslot's area: the split key rounded up to 4096 bytes.
pub const AREA_LEN: u64 = 258_048;
/// The keyslot area's encryption sector, which the IV counts.
pub const AREA_SECTOR: usize = 512;
/// PBKDF2 iterations td forces on every keyslot and digest.
pub const ITERATIONS: u32 = 1000;
/// Keyslot and digest salts.
pub const SALT_LEN: usize = 32;
/// A digest's PBKDF2 output.
pub const DIGEST_LEN: usize = 32;
/// SHA-256's output, the AF diffusion's block.
const HASH_LEN: usize = 32;

/// A source of key material and salts: `/dev/random` outside tests.
pub trait Random {
    fn fill(&mut self, out: &mut [u8]) -> io::Result<()>;
}

/// `/dev/random`, which blocks only until the kernel's generator is
/// initialized.
pub struct DevRandom;

impl Random for DevRandom {
    fn fill(&mut self, out: &mut [u8]) -> io::Result<()> {
        File::open("/dev/random")?.read_exact(out)
    }
}

/// A 512-bit key, cleared when dropped.
pub struct Key(Box<[u8; KEY_LEN]>);

impl Key {
    pub fn zeroed() -> Self {
        Self(Box::new([0; KEY_LEN]))
    }

    pub fn random(random: &mut dyn Random) -> io::Result<Self> {
        let mut key = Self::zeroed();
        random.fill(key.0.as_mut_slice())?;
        Ok(key)
    }

    pub fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        clear(self.0.as_mut_slice());
    }
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// Zero key material where the compiler cannot drop the stores.
pub(crate) fn clear(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
}

/// cryptsetup's `hash_buf`: SHA-256 of the block index big-endian and
/// `block`, truncated to its length.
fn diffuse(block: &mut [u8; KEY_LEN]) {
    let mut input = [0u8; 4 + HASH_LEN];
    for (index, chunk) in block.chunks_mut(HASH_LEN).enumerate() {
        let (prefix, rest) = input.split_at_mut(4);
        prefix.copy_from_slice(&u32::try_from(index).unwrap_or(u32::MAX).to_be_bytes());
        let len = chunk.len();
        let Some(rest) = rest.get_mut(..len) else {
            continue;
        };
        rest.copy_from_slice(chunk);
        let mut hash = td_fido::digest(input.get(..4 + len).unwrap_or_default());
        for (byte, hashed) in chunk.iter_mut().zip(&hash) {
            *byte = *hashed;
        }
        clear(&mut hash);
    }
    clear(&mut input);
}

fn xor_into(target: &mut [u8; KEY_LEN], source: &[u8]) {
    for (byte, other) in target.iter_mut().zip(source) {
        *byte ^= other;
    }
}

/// The AF diffusion over every stripe but the last.
fn diffused(stripes: &[u8]) -> [u8; KEY_LEN] {
    let mut buffer = [0u8; KEY_LEN];
    for stripe in stripes.as_chunks::<KEY_LEN>().0 {
        xor_into(&mut buffer, stripe);
        diffuse(&mut buffer);
    }
    buffer
}

/// LUKS1's AF split of `key` into `STRIPES` stripes: random stripes but
/// the last, which makes the merge give `key`.
pub fn split(key: &Key, random: &mut dyn Random) -> io::Result<Vec<u8>> {
    let mut out = vec![0u8; SPLIT_LEN];
    let (body, last) = out.split_at_mut(SPLIT_LEN - KEY_LEN);
    random.fill(body)?;
    let mut buffer = diffused(body);
    for (target, (key, mixed)) in last.iter_mut().zip(key.bytes().iter().zip(&buffer)) {
        *target = key ^ mixed;
    }
    clear(&mut buffer);
    Ok(out)
}

/// LUKS1's AF merge of a split key.
pub fn merge(split: &[u8]) -> Result<Key, String> {
    if split.len() != SPLIT_LEN {
        return Err(format!("an AF split key is {SPLIT_LEN} bytes"));
    }
    let (body, last) = split.split_at(SPLIT_LEN - KEY_LEN);
    let mut buffer = diffused(body);
    let mut key = Key::zeroed();
    for (target, (stripe, mixed)) in key.0.iter_mut().zip(last.iter().zip(&buffer)) {
        *target = stripe ^ mixed;
    }
    clear(&mut buffer);
    Ok(key)
}

/// A keyslot area's key: PBKDF2-HMAC-SHA256 of the passphrase.
pub fn area_key(passphrase: &[u8], salt: &[u8], iterations: u32) -> Result<Key, String> {
    let mut key = Key::zeroed();
    td_fido::pbkdf2_sha256(passphrase, salt, iterations, key.0.as_mut_slice())
        .map_err(|e| format!("keyslot PBKDF2: {e}"))?;
    Ok(key)
}

/// A digest of `key`: PBKDF2-HMAC-SHA256 under `salt`.
pub fn digest(key: &Key, salt: &[u8], iterations: u32) -> Result<[u8; DIGEST_LEN], String> {
    let mut out = [0u8; DIGEST_LEN];
    td_fido::pbkdf2_sha256(key.bytes(), salt, iterations, &mut out)
        .map_err(|e| format!("digest PBKDF2: {e}"))?;
    Ok(out)
}

/// The split key encrypted in place for its area.
pub fn seal(area_key: &Key, split: &mut [u8]) -> Result<(), String> {
    xts_encrypt(area_key.bytes(), 0, AREA_SECTOR, split)
        .map_err(|e| format!("keyslot area encryption: {e}"))
}

/// The area's split key decrypted in place.
pub fn open(area_key: &Key, split: &mut [u8]) -> Result<(), String> {
    xts_decrypt(area_key.bytes(), 0, AREA_SECTOR, split)
        .map_err(|e| format!("keyslot area decryption: {e}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A deterministic source for tests: SHA-256 in counter mode.
    pub(crate) struct Counter(pub u64);

    impl Random for Counter {
        fn fill(&mut self, out: &mut [u8]) -> io::Result<()> {
            for chunk in out.chunks_mut(HASH_LEN) {
                self.0 += 1;
                let block = td_fido::digest(&self.0.to_be_bytes());
                chunk.copy_from_slice(&block[..chunk.len()]);
            }
            Ok(())
        }
    }

    fn key(byte: u8) -> Key {
        let mut key = Key::zeroed();
        key.0.fill(byte);
        key
    }

    #[test]
    fn a_split_key_merges_back_and_every_stripe_counts() {
        let original = key(0x42);
        let split_key = split(&original, &mut Counter(1)).unwrap();
        assert_eq!(merge(&split_key).unwrap().bytes(), original.bytes());
        for at in [0, KEY_LEN * 1234 + 5, SPLIT_LEN - 1] {
            let mut damaged = split_key.clone();
            damaged[at] ^= 1;
            assert_ne!(merge(&damaged).unwrap().bytes(), original.bytes(), "{at}");
        }
        assert!(merge(&split_key[1..]).is_err());
    }

    /// The diffusion pinned to one computed with Python's hashlib:
    /// `b = sha256(be32(0) + a[:32])[:32] + sha256(be32(1) + a[32:])[:32]`
    /// for `a` = bytes 0..64.
    #[test]
    fn the_diffusion_is_cryptsetups_hash_buf() {
        let mut block = [0u8; KEY_LEN];
        for (i, byte) in block.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let mut first = vec![0, 0, 0, 0];
        first.extend(0..32u8);
        let mut second = vec![0, 0, 0, 1];
        second.extend(32..64u8);
        let mut expected = td_fido::digest(&first).to_vec();
        expected.extend(td_fido::digest(&second));
        diffuse(&mut block);
        assert_eq!(block.to_vec(), expected);
    }

    #[test]
    fn each_area_sector_is_xts_under_its_number_from_zero() {
        let area = area_key(b"passphrase", &[7; SALT_LEN], ITERATIONS).unwrap();
        let plain: Vec<u8> = (0..3 * AREA_SECTOR).map(|i| (i * 7) as u8).collect();
        let mut sealed = plain.clone();
        seal(&area, &mut sealed).unwrap();
        for (i, (sector, plain)) in sealed
            .chunks(AREA_SECTOR)
            .zip(plain.chunks(AREA_SECTOR))
            .enumerate()
        {
            let mut expected = plain.to_vec();
            xts_encrypt(area.bytes(), i as u64, AREA_SECTOR, &mut expected).unwrap();
            assert_eq!(sector, expected, "sector {i}");
        }
    }

    /// Keyslot 0 and digest 0 of a header the pinned cryptsetup 2.8.8
    /// wrote with td-protector's `luksFormat` arguments under the
    /// 48-zero recovery key, captured by tests/cryptsetup_interop.rs's
    /// `capture_the_keyslot_known_answer`: its KDF salt, the first
    /// 512-byte sector of its area and that sector's plaintext (the
    /// first eight AF stripes, vouched for by the whole area merging to
    /// cryptsetup's dumped volume key), the digest's salt and value and
    /// the volume key.
    mod known_answer {
        pub const SALT: &str = "21eaf10425619e8610fb97923d43768e20d2d01f50f497fa67f44869a64b8b4f";
        pub const SECTOR: &str = "e27bb7c8c7a2fea7f100b179a97d1bb2b09672ef7734897b278a364df2dc21e5d4fbeab8e610c8c48500389b8c5e436ce99f52c38c0fd7942ebf3b900e2c31f089fd62fdef719b780ee33c5421815352418f3d62aa60a964479aea746c820496f36c63ac6e5905cc622b8579caaec998aca036e5dd4aa2fd84a04616c0d4acf4428edebc55a0140a769772e1b9b7ce37b8a0e3fae8bde43d049bdc253057a56f590ae6d1a71e59507afd84ed7e5dcc429c4f3936beccba8e5a0f9cf2f5b03f680f4f92304769bff2919e45ff70dba0c6303974e660c51cf8e2f74d43813476f26622e03ea3b2c944febf15eb1676d50424c33d8999b9a15c5948375a6d3100d31be1de31d0742eb80710e0edb018dae4054cf1da18158d2c6fdf21a60b6c8252e34fbd887cb93d682c7f852cc3a3da94fdc3f2e2e589e0cc5c0694a9e3a04699a14226028745b231d12b37d58b07c6499a48b7b9edad6f342bca64e95c220f650d87809def2158739e2b9e395a3de6363dbe177d518ae69173be9a55ceeb27a05aa889a97c8bba02c73687b091caff55ca4573375d29e9ae0f52cf7837662a96b4be9340086d24564cfc12fbbe6d5ed33b9b80425410c39d2c0bbd1762f4cd0690a9887845b624f26c9fd3d7160a8235a7ff78fcb4786c4b6b9798e0f409e135e6f4eaaa7fb779ed463bb19a6dd0957dd722647b8ebacf473245cac2819d4203";
        pub const PLAIN: &str = "a9565087aac55ac71bed2723da7b73ff94fd668f97939f6fcca96e4b714acd1447e89ef9ad4c77de8673b15e50a359b1fad4f011c4bc011891c46bd966221187b34fa501983f5919b51428a86fc5372e52d888bd4f1b302050c14e8eb5d8295bc10dbbe0b5c3b86545a26f4d190c796a4d64b44fea4c21948c3737d582fe8edb3d2a42a7d2de20a387e35dbfbc110a113c35460598227a4d36aad69ec44d7e68f337790802639464c47a792c5fb14e367b5fb3a07baa3fa0747743947ccf77658756685ddc9a58b2b827c75e4bfae772f93449081ade149d97b14aa2793b2c01d7ff75e74ccb823af6c96d620dace297e200a1e62ed7ea4b69e2e534876d1efc8bb0e0c37eefa8958c89563b109a30864717a7e7c84f24354e51c818587b35884266c5c2e3e85013bbef5b0067b37438c6c6813da7cb6c9f4dcb75c2d99c172f82ade8f229799035556da520f087bcf20912b53f9057305456f868c3b957220cc1e80a0c79232129f0a050589186e04e6d4002f94359ed3a4c693b401485f94e659f77a59a2217bac610eb238fdd71d74912a64cb5aead87fea13938c0a85509241be1f0ac6496312c5ea034867416b7c0b05bd0b9c8e54b6a7b78a454f61b3bd59dfd97cb329280b6e04bc536f325879753af2b8187ee77ee98d01a8dce180542bde9a35c4d6242d60fd81c161a53a65d54aa01cf12581cd3cbde90a04ef23d";
        pub const DIGEST_SALT: &str =
            "5141b36df54593a17d48ac547f660c215b12878c37a565462ccf85957c4735df";
        pub const DIGEST: &str = "651e96577393f7acacda340c20dd7df2f6d01d079866b725642e6dccbae5afcb";
        pub const VOLUME_KEY: &str = "328defc9355474493a158a9698e1702c9941c2f9394976a01f89dbfa1b4ea97498f1c54dd2b0c2d9ac0f40c8332123178c756bdfd645edf3093e83e4a225f24a";
    }

    #[test]
    fn cryptsetup_s_keyslot_sector_and_digest_are_reproduced() {
        use crate::encoding::from_hex;
        let hex = |text| from_hex(text).unwrap();
        let recovery = [b'0'; 48];
        let area = area_key(&recovery, &hex(known_answer::SALT), ITERATIONS).unwrap();
        let mut sector = hex(known_answer::SECTOR);
        open(&area, &mut sector).unwrap();
        assert_eq!(sector, hex(known_answer::PLAIN));
        seal(&area, &mut sector).unwrap();
        assert_eq!(sector, hex(known_answer::SECTOR));
        let mut volume_key = Key::zeroed();
        volume_key.0.copy_from_slice(&hex(known_answer::VOLUME_KEY));
        assert_eq!(
            digest(&volume_key, &hex(known_answer::DIGEST_SALT), ITERATIONS)
                .unwrap()
                .to_vec(),
            hex(known_answer::DIGEST)
        );
    }

    #[test]
    fn a_sealed_area_opens_only_under_its_key() {
        let area = area_key(b"passphrase", &[7; SALT_LEN], ITERATIONS).unwrap();
        let original = split(&key(9), &mut Counter(3)).unwrap();
        let mut sealed = original.clone();
        seal(&area, &mut sealed).unwrap();
        assert_ne!(sealed, original);
        let mut opened = sealed.clone();
        open(&area, &mut opened).unwrap();
        assert_eq!(opened, original);
        let other = area_key(b"passphrasf", &[7; SALT_LEN], ITERATIONS).unwrap();
        let mut wrong = sealed;
        open(&other, &mut wrong).unwrap();
        assert_ne!(merge(&wrong).unwrap().bytes(), key(9).bytes());
    }
}
