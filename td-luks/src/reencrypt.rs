//! cryptsetup's v2 reencryption digest for td's in-place forward profile.
//! The digest binds the two keys, resilience area and backup segments.
//! It does not bind active segments or progress; their validation is separate.
use crate::header::MAX_DIGEST_ITERATIONS;
use crate::keyslot::{self, Key, DIGEST_LEN, SALT_LEN};
use crate::write::{CIPHER, DATA_OFFSET, KEYSLOTS_OFFSET, SECTOR_SIZE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resilience {
    Checksum,
    Journal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub resilience: Resilience,
    pub offset: u64,
    pub size: u64,
}
impl Area {
    fn validate(self) -> Result<(), String> {
        if self.offset < KEYSLOTS_OFFSET
            || !self.offset.is_multiple_of(SECTOR_SIZE)
            || self.size == 0
            || !self.size.is_multiple_of(SECTOR_SIZE)
            || self
                .offset
                .checked_add(self.size)
                .is_none_or(|end| end > DATA_OFFSET)
        {
            return Err("reencryption resilience area is outside the aligned keyslots area".into());
        }
        Ok(())
    }
}

struct VerificationData {
    bytes: Box<[u8; 274]>,
    used: usize,
}
impl VerificationData {
    fn append(&mut self, bytes: &[u8]) -> Result<(), String> {
        let end = self
            .used
            .checked_add(bytes.len())
            .ok_or("reencryption digest input overflow")?;
        self.bytes
            .get_mut(self.used..end)
            .ok_or("reencryption digest input exceeds bound")?
            .copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }
    fn bytes(&self) -> Result<&[u8], String> {
        self.bytes
            .get(..self.used)
            .ok_or_else(|| "reencryption digest input bound".into())
    }
}
impl Drop for VerificationData {
    fn drop(&mut self) {
        keyslot::clear(self.bytes.as_mut_slice());
    }
}

/// PBKDF2 of the v2 verification data, with td's fixed backup-previous
/// and backup-final segments (both full-volume, dynamic, aes-xts-plain64).
/// The caller must separately establish that those are the on-disk
/// segments, that the old/new digest identities differ, and that the area
/// overlaps no normal keyslot. This function neither admits a header nor
/// decrypts data, and accepts no other reencryption mode or version.
pub fn digest(
    old: &Key,
    new: &Key,
    area: Area,
    salt: &[u8; SALT_LEN],
    iterations: u32,
) -> Result<[u8; DIGEST_LEN], String> {
    area.validate()?;
    if !(1..=MAX_DIGEST_ITERATIONS).contains(&u64::from(iterations)) {
        return Err("reencryption digest iteration count is outside the reader bound".into());
    }
    // All fields below have fixed lengths: 274 bytes for checksum and
    // 263 for journal. No key-bearing buffer grows or reallocates.
    let mut data = VerificationData {
        bytes: Box::new([0; 274]),
        used: 0,
    };
    data.append(b"v2")?;
    data.append(old.bytes())?;
    data.append(new.bytes())?;
    data.append(b"reencryptforward")?;
    data.append(match area.resilience {
        Resilience::Checksum => b"checksum",
        Resilience::Journal => b"journal",
    })?;
    data.append(&area.offset.to_be_bytes())?;
    data.append(&area.size.to_be_bytes())?;
    if area.resilience == Resilience::Checksum {
        data.append(b"sha256")?;
        data.append(&(SECTOR_SIZE as u32).to_be_bytes())?;
    }
    for _ in 0..2 {
        data.append(b"crypt")?;
        data.append(&DATA_OFFSET.to_be_bytes())?;
        data.append(b"dynamic")?;
        data.append(&0u64.to_be_bytes())?;
        data.append(CIPHER.as_bytes())?;
        data.append(&(SECTOR_SIZE as u32).to_be_bytes())?;
    }
    let mut out = [0; DIGEST_LEN];
    td_fido::pbkdf2_sha256(data.bytes()?, salt, iterations, &mut out)
        .map_err(|error| format!("reencryption digest PBKDF2: {error}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_area_and_iteration_bounds_are_refused_before_hashing() {
        let old = Key::zeroed();
        let new = Key::from_bytes(&[1; 64]);
        let area = Area {
            resilience: Resilience::Checksum,
            offset: KEYSLOTS_OFFSET,
            size: SECTOR_SIZE,
        };
        for bad in [
            Area { offset: 0, ..area },
            Area {
                offset: KEYSLOTS_OFFSET + 1,
                ..area
            },
            Area { size: 0, ..area },
            Area {
                size: SECTOR_SIZE - 1,
                ..area
            },
            Area {
                size: DATA_OFFSET,
                ..area
            },
            Area {
                offset: u64::MAX - 4095,
                size: 4096,
                ..area
            },
        ] {
            assert!(digest(&old, &new, bad, &[0; SALT_LEN], 1000).is_err());
        }
        for count in [0, MAX_DIGEST_ITERATIONS as u32 + 1] {
            assert!(digest(&old, &new, area, &[0; SALT_LEN], count).is_err());
        }
    }
}

#[path = "reencrypt_profile.rs"]
mod profile;
pub use profile::{profile, Hotzone, Layout};

#[path = "reencrypt_transition.rs"]
mod transition;
pub use transition::{finish_hotzone, mark_hotzone, HOTZONE_SIZE};

#[path = "reencrypt_metadata.rs"]
mod metadata;
pub use metadata::{commit, load, plan_start, unlock, Keys};
