//! The bounded LUKS2 header reader at the format level (DESIGN.md "Crates").
//! It selects the header copy cryptsetup would use, by cryptsetup's own
//! validity and sequence rule, refuses copies that disagree, and parses
//! the used copy's JSON area as generic LUKS2 data. It runs before any C
//! parser has seen the header. td's token semantics are td-protector's.

use std::io::{ErrorKind, Read, Seek, SeekFrom};
use td_json::Json;

/// The binary header that starts every copy.
pub const BINARY_HEADER_LEN: usize = 4096;
/// cryptsetup's secondary-copy scan offsets, in its order. A copy's own
/// `hdr_size` need only lie in the 16 KiB to 4 MiB range for validity;
/// td then refuses a selected copy that is not `FORMATTED_HEADER_SIZE`.
pub const HEADER_SIZES: &[u64] = &[
    0x4000, 0x8000, 0x1_0000, 0x2_0000, 0x4_0000, 0x8_0000, 0x10_0000, 0x20_0000, 0x40_0000,
];
/// The only `hdr_size` td formats (td-install/ENCRYPTION.md "Device-bound
/// formatting"), and so the only one whose JSON td parses.
pub const FORMATTED_HEADER_SIZE: u64 = 0x4000;
/// The JSON area of a copy of `FORMATTED_HEADER_SIZE`: the most JSON text
/// td parses.
pub const JSON_AREA_LEN: usize = 0x4000 - BINARY_HEADER_LEN;
/// The highest LUKS2 keyslot or token number (LUKS2 has 32 of each).
pub const MAX_SLOT: u8 = 31;
/// The most text `metadata_json` parses: cryptsetup's indented rendering
/// of a JSON area of at most 12 KiB, whose shortest element grows by a
/// line break and its indentation.
pub const MAX_METADATA_JSON: usize = 128 * 1024;
/// The largest keyslots area cryptsetup itself formats (its
/// `LUKS2_MAX_KEYSLOTS_SIZE`): a test area is at most two 16 KiB copies
/// and this.
pub const MAX_KEYSLOTS_SIZE: u64 = 128 << 20;
/// The most PBKDF2 iterations a tested keyslot may ask: td forces 1000
/// on every keyslot it adds, so this leaves margin and no more.
pub const MAX_KEYSLOT_ITERATIONS: u64 = 100_000;
/// The most PBKDF2 iterations a tested keyslot's digest may ask. td forces
/// 1000, but a header stock cryptsetup wrote carries a benchmarked count;
/// this is td-fido's PBKDF2 bound, so every digest admitted here can be
/// verified (DESIGN.md "The profile").
pub const MAX_DIGEST_ITERATIONS: u64 = td_fido::PBKDF2_MAX_ITERATIONS as u64;

pub const PRIMARY_MAGIC: &[u8] = b"LUKS\xba\xbe";
pub const SECONDARY_MAGIC: &[u8] = b"SKUL\xba\xbe";
const CHECKSUM_ALG: &[u8] = b"sha256";
const MIN_HEADER_SIZE: u64 = 0x4000;
const MAX_HEADER_SIZE: u64 = 0x40_0000;
// Field offsets in the binary header (cryptsetup's `struct luks2_hdr_disk`).
pub const VERSION_AT: usize = 6;
pub const HDR_SIZE_AT: usize = 8;
pub const SEQID_AT: usize = 16;
pub const LABEL_AT: usize = 24;
pub const LABEL_LEN: usize = 48;
pub const CHECKSUM_ALG_AT: usize = 72;
pub const CHECKSUM_ALG_LEN: usize = 32;
pub const UUID_AT: usize = 168;
pub const UUID_LEN: usize = 40;
pub const SUBSYSTEM_AT: usize = 208;
pub const SUBSYSTEM_LEN: usize = 48;
pub const HDR_OFFSET_AT: usize = 256;
pub const CHECKSUM_AT: usize = 448;
pub const CHECKSUM_FIELD_LEN: usize = 64;
pub const SHA256_LEN: usize = 32;

/// Which copy the reader used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderCopy {
    Primary,
    Secondary,
}
impl HeaderCopy {
    pub fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
        }
    }
}

/// The identity of the copy cryptsetup would use, chosen as `read_copy`
/// chooses it, with no JSON parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub copy: HeaderCopy,
    pub seqid: u64,
    pub hdr_size: u64,
    /// The binary header's UUID and label, up to their first NUL.
    pub uuid: String,
    pub label: String,
}

/// The copy cryptsetup would use: its identity, and its JSON area when it
/// is of td's formatted size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsedCopy {
    pub identity: Identity,
    json: Option<Vec<u8>>,
}

impl UsedCopy {
    /// The copy's JSON object and the length of its text. Refused before
    /// any JSON is parsed unless the copy is `FORMATTED_HEADER_SIZE`: a
    /// larger copy's area is not handed to td-json.
    pub fn json(&self) -> Result<(Json, usize), String> {
        match &self.json {
            Some(json) if self.identity.hdr_size == FORMATTED_HEADER_SIZE => json_area(json),
            _ => Err(format!(
                "LUKS2 {} header copy has hdr_size {}, not the {FORMATTED_HEADER_SIZE} td formats",
                self.identity.copy.name(),
                self.identity.hdr_size
            )),
        }
    }
}

/// What td keeps of one checksum-valid copy once its area is verified:
/// the binary header, the JSON area only at td's formatted size, and the
/// JSON area's SHA-256 to compare copies of any size.
struct ValidCopy {
    binary: Vec<u8>,
    json: Option<Vec<u8>>,
    json_digest: [u8; SHA256_LEN],
    hdr_size: u64,
    seqid: u64,
}

fn bytes_at(area: &[u8], at: usize, len: usize) -> Result<&[u8], String> {
    at.checked_add(len)
        .and_then(|end| area.get(at..end))
        .ok_or_else(|| "LUKS2 binary header is truncated".to_string())
}

fn u64_at(area: &[u8], at: usize) -> Result<u64, String> {
    let bytes: [u8; 8] = bytes_at(area, at, 8)?
        .try_into()
        .map_err(|_| "LUKS2 binary header is truncated".to_string())?;
    Ok(u64::from_be_bytes(bytes))
}

/// A NUL-terminated field, up to its first NUL.
fn text_at(area: &[u8], at: usize, len: usize) -> Result<&[u8], String> {
    let field = bytes_at(area, at, len)?;
    Ok(field.split(|byte| *byte == 0).next().unwrap_or_default())
}

/// Read exactly `out` at `offset`. A medium that ends inside it is `None`;
/// any other error refuses the header.
fn read_at<R: Read + Seek>(device: &mut R, offset: u64, out: &mut [u8]) -> Result<bool, String> {
    device
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek to LUKS2 header copy at {offset}: {e}"))?;
    match device.read_exact(out) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(format!("read LUKS2 header copy at {offset}: {e}")),
    }
}

/// The copy at `offset`, if cryptsetup would find it valid: its magic,
/// version, own offset, size and checksum. A checksum algorithm other
/// than SHA-256 refuses the header, so td never judges a copy cryptsetup
/// would judge differently. The whole area, up to 4 MiB, is read and
/// hashed, then released before the next copy is read.
fn valid_copy<R: Read + Seek>(
    device: &mut R,
    offset: u64,
    secondary: bool,
) -> Result<Option<ValidCopy>, String> {
    let mut binary = vec![0u8; BINARY_HEADER_LEN];
    if !read_at(device, offset, &mut binary)? {
        return Ok(None);
    }
    let magic = if secondary {
        SECONDARY_MAGIC
    } else {
        PRIMARY_MAGIC
    };
    let hdr_size = u64_at(&binary, HDR_SIZE_AT)?;
    if bytes_at(&binary, 0, magic.len())? != magic
        || bytes_at(&binary, VERSION_AT, 2)? != [0, 2]
        || u64_at(&binary, HDR_OFFSET_AT)? != offset
        || !(MIN_HEADER_SIZE..=MAX_HEADER_SIZE).contains(&hdr_size)
        || (secondary && offset != hdr_size)
    {
        return Ok(None);
    }
    if text_at(&binary, CHECKSUM_ALG_AT, CHECKSUM_ALG_LEN)? != CHECKSUM_ALG {
        return Err(format!(
            "LUKS2 header copy at {offset} uses a checksum algorithm other than sha256"
        ));
    }
    let size = usize::try_from(hdr_size).map_err(|_| "LUKS2 header size overflows")?;
    let mut area = binary;
    area.resize(size, 0);
    let json_offset = offset.saturating_add(BINARY_HEADER_LEN as u64);
    if !read_at(
        device,
        json_offset,
        area.get_mut(BINARY_HEADER_LEN..).unwrap_or_default(),
    )? {
        return Ok(None);
    }
    let stored: [u8; SHA256_LEN] = bytes_at(&area, CHECKSUM_AT, SHA256_LEN)?
        .try_into()
        .map_err(|_| "LUKS2 binary header is truncated")?;
    // The checksum covers the area with its own field zeroed.
    area.get_mut(CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_FIELD_LEN)
        .ok_or("LUKS2 binary header is truncated")?
        .fill(0);
    if td_fido::digest(&area) != stored {
        return Ok(None);
    }
    let json_area = area.get(BINARY_HEADER_LEN..).unwrap_or_default();
    let json_digest = td_fido::digest(json_area);
    let json = (hdr_size == FORMATTED_HEADER_SIZE).then(|| json_area.to_vec());
    let binary = bytes_at(&area, 0, BINARY_HEADER_LEN)?.to_vec();
    drop(area);
    Ok(Some(ValidCopy {
        seqid: u64_at(&binary, SEQID_AT)?,
        binary,
        json,
        json_digest,
        hdr_size,
    }))
}

/// The fields both copies carry alike, and the JSON area's digest. Salts,
/// magic, own offset and checksum legitimately differ between copies.
fn shared(copy: &ValidCopy) -> Result<[&[u8]; 5], String> {
    Ok([
        bytes_at(&copy.binary, LABEL_AT, LABEL_LEN)?,
        bytes_at(&copy.binary, CHECKSUM_ALG_AT, CHECKSUM_ALG_LEN)?,
        bytes_at(&copy.binary, UUID_AT, UUID_LEN)?,
        bytes_at(&copy.binary, SUBSYSTEM_AT, SUBSYSTEM_LEN)?,
        &copy.json_digest,
    ])
}

/// The JSON area of a 16 KiB copy: one object from its first byte, then
/// only NUL bytes, with the object's length. Nothing longer reaches
/// td-json.
fn json_area(json: &[u8]) -> Result<(Json, usize), String> {
    if json.len() != JSON_AREA_LEN {
        return Err(format!(
            "LUKS2 JSON area is {} bytes, not {JSON_AREA_LEN}",
            json.len()
        ));
    }
    let end = json
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("LUKS2 JSON area has no NUL padding")?;
    let (text, padding) = json.split_at_checked(end).unwrap_or_default();
    if padding.iter().any(|byte| *byte != 0) {
        return Err("LUKS2 JSON area has bytes after its NUL padding".into());
    }
    if text.first() != Some(&b'{') || text.last() != Some(&b'}') {
        return Err("LUKS2 JSON area is not one object followed by NUL bytes".into());
    }
    let json = td_json::parse_slice(text).map_err(|e| format!("LUKS2 JSON area: {e}"))?;
    Ok((json, end))
}

/// A LUKS2 keyslot or token number as LUKS2 writes it: decimal, 0 to 31,
/// with no sign, space or leading zero.
pub fn slot_number(text: &str) -> Option<u8> {
    if text.is_empty() || text.len() > 2 || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u8>().ok().filter(|slot| *slot <= MAX_SLOT)
}

/// An object keyed by slot numbers 0 to 31, as `(number, value)`.
pub fn numbered<'a>(header: &'a Json, section: &str) -> Result<Vec<(u8, &'a Json)>, String> {
    let pairs = header
        .get(section)
        .and_then(Json::as_obj)
        .ok_or_else(|| format!("LUKS2 JSON has no {section} object"))?;
    let mut out = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        let number = slot_number(key).ok_or_else(|| {
            format!("LUKS2 {section} key {key:?} is not a number from 0 to {MAX_SLOT}")
        })?;
        out.push((number, value));
    }
    out.sort_unstable_by_key(|(number, _)| *number);
    Ok(out)
}

/// What one binary header claims, unverified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinaryClaim<'a> {
    /// The label and UUID, each up to its first NUL.
    pub label: &'a [u8],
    pub uuid: &'a [u8],
}

/// The label and UUID of a binary header that carries its position's
/// magic and version 2; `None` for any other bytes. A cheap pre-check
/// for volume discovery that verifies no checksum: only `identity` and
/// `read_copy` admit a header.
pub fn binary_claim(binary: &[u8], secondary: bool) -> Option<BinaryClaim<'_>> {
    let magic = if secondary {
        SECONDARY_MAGIC
    } else {
        PRIMARY_MAGIC
    };
    if binary.get(..magic.len())? != magic || binary.get(VERSION_AT..HDR_SIZE_AT)? != [0, 2] {
        return None;
    }
    Some(BinaryClaim {
        label: text_at(binary, LABEL_AT, LABEL_LEN).ok()?,
        uuid: text_at(binary, UUID_AT, UUID_LEN).ok()?,
    })
}

/// `luksDump --dump-json-metadata`'s output as one JSON object: at most
/// `MAX_METADATA_JSON` bytes.
pub fn metadata_json(text: &[u8]) -> Result<Json, String> {
    if text.len() > MAX_METADATA_JSON {
        return Err(format!(
            "cryptsetup's LUKS2 metadata is over {MAX_METADATA_JSON} bytes"
        ));
    }
    let json =
        td_json::parse_slice(text).map_err(|e| format!("cryptsetup's LUKS2 metadata: {e}"))?;
    if json.as_obj().is_none() {
        return Err("cryptsetup's LUKS2 metadata is not one object".into());
    }
    Ok(json)
}

/// Read the LUKS2 header at the start of `device` and return the copy
/// cryptsetup would use. It refuses a header with no valid copy, with
/// copies disagreeing at one sequence number, or with a checksum algorithm
/// other than SHA-256, and admits a copy of any valid size:
/// `UsedCopy::json` refuses the rest.
pub fn read_copy<R: Read + Seek>(device: &mut R) -> Result<UsedCopy, String> {
    let (copy, which) = used_copy(device)?;
    let text = |at, len| -> Result<String, String> {
        Ok(String::from_utf8_lossy(text_at(&copy.binary, at, len)?).into_owned())
    };
    let identity = Identity {
        copy: which,
        seqid: copy.seqid,
        hdr_size: copy.hdr_size,
        uuid: text(UUID_AT, UUID_LEN)?,
        label: text(LABEL_AT, LABEL_LEN)?,
    };
    Ok(UsedCopy {
        identity,
        json: copy.json,
    })
}

/// The used copy's identity: its magic, version, offsets and checksum
/// verified as `read_copy` verifies them, and nothing of its JSON area.
/// Volume discovery uses it, and td-protector's `read` refuses the rest on
/// the volume chosen.
pub fn identity<R: Read + Seek>(device: &mut R) -> Result<Identity, String> {
    read_copy(device).map(|copy| copy.identity)
}

/// What the re-encrypting upgrade reads of a header beyond its keyslots
/// and tokens (td-protector/DESIGN.md "Protected roles").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The data segment's identity (`segment_digest`) for the one digest
    /// naming segment 0; `None` when no single digest names it with a
    /// `digest` and a `salt`.
    pub segment: Option<[u8; SHA256_LEN]>,
    /// A mandatory requirement cryptsetup's online reencryption sets
    /// (`online-reencrypt`, `-v2` or `-v3`): a reencryption is in progress.
    pub reencryption: bool,
    /// The keyslots of type `luks2`, ascending: a reencryption's own keyslot
    /// is another type, and unlocks nothing.
    pub luks2: Vec<u8>,
    /// The `mode` of each keyslot of type `reencrypt`, in keyslot order:
    /// `reencrypt`, `encrypt` or `decrypt` as cryptsetup writes it.
    pub reencrypt_modes: Vec<String>,
}

/// The data segment's identity: the SHA-256 over its digest's `digest`
/// text, a zero byte and its `salt` text, as cryptsetup writes them in
/// base64. A reencryption draws both anew, so a changed identity says the
/// volume key changed.
pub fn segment_digest(digest: &str, salt: &str) -> [u8; SHA256_LEN] {
    td_fido::digest(&[digest.as_bytes(), &[0], salt.as_bytes()].concat())
}

/// The volume a parsed JSON header describes.
pub fn volume(json: &Json) -> Result<Volume, String> {
    let keyslots = numbered(json, "keyslots")?;
    let typed = |kind: &'static str| {
        keyslots
            .iter()
            .filter(move |(_, keyslot)| keyslot.get("type").and_then(Json::as_str) == Some(kind))
    };
    let luks2 = typed("luks2").map(|(number, _)| *number).collect();
    let reencrypt_modes = typed("reencrypt")
        .map(|(_, keyslot)| {
            keyslot
                .get("mode")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let digests = numbered(json, "digests")?;
    let mut naming = digests.iter().filter(|(_, digest)| {
        digest
            .get("segments")
            .and_then(Json::as_arr)
            .is_some_and(|segments| segments.iter().any(|s| s.as_str() == Some("0")))
    });
    let segment = match (naming.next(), naming.next()) {
        (Some((_, digest)), None) => match (
            digest.get("digest").and_then(Json::as_str),
            digest.get("salt").and_then(Json::as_str),
        ) {
            (Some(digest), Some(salt)) => Some(segment_digest(digest, salt)),
            _ => None,
        },
        _ => None,
    };
    let reencryption = match json.get_path(&["config", "requirements", "mandatory"]) {
        None => false,
        Some(mandatory) => {
            let mut reencryption = false;
            for requirement in mandatory
                .as_arr()
                .ok_or("LUKS2 mandatory requirements are not an array")?
            {
                let requirement = requirement
                    .as_str()
                    .ok_or("LUKS2 mandatory requirement is not a string")?;
                reencryption |= requirement.starts_with("online-reencrypt");
            }
            reencryption
        }
    };
    Ok(Volume {
        segment,
        reencryption,
        luks2,
        reencrypt_modes,
    })
}

/// The bytes a detached copy of the header must hold: two copies of the
/// used copy's `hdr_size` and `config.keyslots_size`, bounded by
/// `MAX_KEYSLOTS_SIZE`.
pub fn test_length(copy: &UsedCopy, json: &Json) -> Result<u64, String> {
    let keyslots_size = json
        .get_path(&["config", "keyslots_size"])
        .and_then(Json::as_str)
        .and_then(|size| size.parse::<u64>().ok())
        .filter(|size| *size <= MAX_KEYSLOTS_SIZE)
        .ok_or("LUKS2 keyslots area size is missing or over its bound")?;
    copy.identity
        .hdr_size
        .checked_mul(2)
        .and_then(|copies| copies.checked_add(keyslots_size))
        .ok_or_else(|| "LUKS2 header area overflows".to_string())
}

/// A keyslot's PBKDF2-SHA256 iterations and its digest's, each within
/// its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub keyslot: u64,
    pub digest: u64,
}

/// Each keyslot number with its cost, or why it has none.
pub type Costs = Vec<(u8, Result<Cost, String>)>;

/// Each keyslot's cost of a test, or why td does not bound it: a keyslot
/// or digest that is not PBKDF2-SHA256, such as Argon2, whose memory and
/// time td does not run, or one over its ceiling.
pub fn costs(json: &Json) -> Result<Costs, String> {
    let digests = numbered(json, "digests")?;
    Ok(numbered(json, "keyslots")?
        .into_iter()
        .map(|(number, keyslot)| (number, cost(number, keyslot, &digests)))
        .collect())
}

/// PBKDF2-SHA256 iterations of `value`, at most `ceiling`.
fn pbkdf2(value: Option<&Json>, ceiling: u64, what: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("{what} names no PBKDF"))?;
    if value.get("type").and_then(Json::as_str) != Some("pbkdf2")
        || value.get("hash").and_then(Json::as_str) != Some("sha256")
    {
        return Err(format!("{what} is not PBKDF2-SHA256"));
    }
    value
        .get("iterations")
        .and_then(Json::as_u64)
        .filter(|iterations| (1..=ceiling).contains(iterations))
        .ok_or_else(|| format!("{what} asks more than {ceiling} PBKDF2 iterations"))
}

fn cost(number: u8, keyslot: &Json, digests: &[(u8, &Json)]) -> Result<Cost, String> {
    let keyslot = pbkdf2(
        keyslot.get("kdf"),
        MAX_KEYSLOT_ITERATIONS,
        &format!("LUKS2 keyslot {number}"),
    )?;
    let name = number.to_string();
    let mut covering = digests.iter().filter(|(_, digest)| {
        digest
            .get("keyslots")
            .and_then(Json::as_arr)
            .is_some_and(|slots| slots.iter().any(|slot| slot.as_str() == Some(&name)))
    });
    let (Some((_, digest)), None) = (covering.next(), covering.next()) else {
        return Err(format!("LUKS2 keyslot {number} has no one digest"));
    };
    let digest = pbkdf2(
        Some(digest),
        MAX_DIGEST_ITERATIONS,
        &format!("LUKS2 keyslot {number}'s digest"),
    )?;
    Ok(Cost { keyslot, digest })
}

fn used_copy<R: Read + Seek>(device: &mut R) -> Result<(ValidCopy, HeaderCopy), String> {
    let primary = valid_copy(device, 0, false)?;
    let secondary = match &primary {
        Some(primary) => valid_copy(device, primary.hdr_size, true)?,
        None => {
            let mut found = None;
            for offset in HEADER_SIZES {
                found = valid_copy(device, *offset, true)?;
                if found.is_some() {
                    break;
                }
            }
            found
        }
    };
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => {
            if primary.seqid > secondary.seqid {
                Ok((primary, HeaderCopy::Primary))
            } else if primary.seqid < secondary.seqid {
                Ok((secondary, HeaderCopy::Secondary))
            } else if primary.hdr_size != secondary.hdr_size
                || shared(&primary)? != shared(&secondary)?
            {
                Err(format!(
                    "LUKS2 header copies disagree at the same sequence number {}",
                    primary.seqid
                ))
            } else {
                Ok((primary, HeaderCopy::Primary))
            }
        }
        (Some(primary), None) => Ok((primary, HeaderCopy::Primary)),
        (None, Some(secondary)) => Ok((secondary, HeaderCopy::Secondary)),
        (None, None) => Err("no valid LUKS2 header copy".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A generic header: two keyslots, a foreign token, a digest.
    const JSON: &str = r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}},"tokens":{"0":{"type":"systemd-tpm2","keyslots":["0"]}},"segments":{"0":{"type":"crypt"}},"digests":{"0":{"type":"pbkdf2","keyslots":["0","1"],"segments":["0"],"hash":"sha256","iterations":1000,"salt":"c2FsdA==","digest":"ZGlnZXN0"}},"config":{"json_size":"12288","keyslots_size":"16744448"}}"#;

    /// One copy as cryptsetup writes it, its checksum computed here.
    fn copy(secondary: bool, offset: u64, hdr_size: u64, seqid: u64, json: &[u8]) -> Vec<u8> {
        let mut area = vec![0u8; hdr_size as usize];
        area[..6].copy_from_slice(if secondary {
            SECONDARY_MAGIC
        } else {
            PRIMARY_MAGIC
        });
        area[6..8].copy_from_slice(&2u16.to_be_bytes());
        area[8..16].copy_from_slice(&hdr_size.to_be_bytes());
        area[16..24].copy_from_slice(&seqid.to_be_bytes());
        area[24..33].copy_from_slice(b"td-system");
        area[72..78].copy_from_slice(b"sha256");
        // Each copy has its own salt.
        area[104..168].fill(if secondary { 0x52 } else { 0x51 });
        area[168..204].copy_from_slice(b"0f0e0d0c-0b0a-4908-8706-050403020100");
        area[256..264].copy_from_slice(&offset.to_be_bytes());
        area[BINARY_HEADER_LEN..BINARY_HEADER_LEN + json.len()].copy_from_slice(json);
        area[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_FIELD_LEN].fill(0);
        let digest = td_fido::digest(&area);
        area[CHECKSUM_AT..CHECKSUM_AT + SHA256_LEN].copy_from_slice(&digest);
        area
    }

    /// Both copies at the 16 KiB layout, then some data.
    fn device(primary_seqid: u64, primary: &str, secondary_seqid: u64, secondary: &str) -> Vec<u8> {
        let mut out = copy(false, 0, 0x4000, primary_seqid, primary.as_bytes());
        out.extend(copy(
            true,
            0x4000,
            0x4000,
            secondary_seqid,
            secondary.as_bytes(),
        ));
        out.extend(vec![0xaa; 4096]);
        out
    }

    fn read_bytes(bytes: Vec<u8>) -> Result<UsedCopy, String> {
        read_copy(&mut Cursor::new(bytes))
    }

    #[test]
    fn the_used_copy_is_the_one_cryptsetup_chooses() {
        let other = JSON.replace(r#""1":{"type":"luks2"}"#, r#""2":{"type":"luks2"}"#);
        let used = read_bytes(device(7, JSON, 8, &other)).unwrap();
        assert_eq!(used.identity.copy, HeaderCopy::Secondary);
        assert_eq!(used.identity.seqid, 8);
        let (json, len) = used.json().unwrap();
        assert_eq!(len, other.len());
        let slots: Vec<u8> = numbered(&json, "keyslots")
            .unwrap()
            .into_iter()
            .map(|(number, _)| number)
            .collect();
        assert_eq!(slots, [0, 2]);
        let used = read_bytes(device(9, JSON, 8, &other)).unwrap();
        assert_eq!(used.identity.copy, HeaderCopy::Primary);
        assert_eq!(
            used.identity,
            identity(&mut Cursor::new(device(9, JSON, 8, &other))).unwrap()
        );
        assert_eq!(
            read_bytes(device(7, JSON, 7, &other)).unwrap_err(),
            "LUKS2 header copies disagree at the same sequence number 7"
        );
        let mut corrupt = device(7, JSON, 7, JSON);
        corrupt[1000] ^= 1;
        let used = read_bytes(corrupt.clone()).unwrap();
        assert_eq!(used.identity.copy, HeaderCopy::Secondary);
        corrupt[0x4000 + 1000] ^= 1;
        assert_eq!(
            read_bytes(corrupt).unwrap_err(),
            "no valid LUKS2 header copy"
        );
    }

    #[test]
    fn only_the_formatted_size_has_its_json_parsed() {
        let mut large = copy(false, 0, 0x8000, 7, JSON.as_bytes());
        large.extend(copy(true, 0x8000, 0x8000, 7, JSON.as_bytes()));
        let used = read_bytes(large).unwrap();
        assert_eq!(used.identity.hdr_size, 0x8000);
        assert_eq!(
            used.json().unwrap_err(),
            "LUKS2 primary header copy has hdr_size 32768, not the 16384 td formats"
        );
        for (area, refusal) in [
            ("[1]", "not one object"),
            ("{\"keyslots\":", "not one object"),
        ] {
            let error = read_bytes(device(7, area, 7, area))
                .unwrap()
                .json()
                .unwrap_err();
            assert!(error.contains(refusal), "{area}: {error}");
        }
    }

    #[test]
    fn numbered_sections_take_canonical_slot_numbers() {
        for slot in 0..=31u8 {
            assert_eq!(slot_number(&slot.to_string()), Some(slot));
        }
        for text in ["", "32", "00", "01", "-0", "a", "1.0", "255", "٣"] {
            assert_eq!(slot_number(text), None, "{text:?}");
        }
        let json = td_json::parse_slice(br#"{"keyslots":{"01":{}}}"#).unwrap();
        assert_eq!(
            numbered(&json, "keyslots").unwrap_err(),
            "LUKS2 keyslots key \"01\" is not a number from 0 to 31"
        );
        assert_eq!(
            numbered(&json, "tokens").unwrap_err(),
            "LUKS2 JSON has no tokens object"
        );
    }

    #[test]
    fn the_volume_names_its_segment_and_costs_bound_each_keyslot() {
        let (json, _) = read_bytes(device(5, JSON, 5, JSON))
            .unwrap()
            .json()
            .unwrap();
        let volume = volume(&json).unwrap();
        assert_eq!(volume.segment, Some(segment_digest("ZGlnZXN0", "c2FsdA==")));
        assert!(!volume.reencryption);
        assert_eq!(volume.luks2, [0, 1]);
        // Neither keyslot names a PBKDF, so neither has a cost.
        let costs = costs(&json).unwrap();
        assert_eq!(costs.len(), 2);
        assert!(costs
            .iter()
            .all(|(_, cost)| cost.as_ref().is_err_and(|e| e.ends_with("names no PBKDF"))));
        let used = read_bytes(device(5, JSON, 5, JSON)).unwrap();
        assert_eq!(test_length(&used, &json).unwrap(), 2 * 0x4000 + 16744448);
        assert_eq!(MAX_DIGEST_ITERATIONS, 4_000_000);
    }

    fn seal(area: &mut [u8]) {
        area[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_FIELD_LEN].fill(0);
        let digest = td_fido::digest(area);
        area[CHECKSUM_AT..CHECKSUM_AT + SHA256_LEN].copy_from_slice(&digest);
    }

    #[test]
    fn an_invalid_primary_falls_back_to_the_secondary() {
        // Each damages one check, the checksum left as written.
        let corrupt: &[(&str, usize, &[u8])] = &[
            ("magic", 0, b"LUKS\xba\xbf"),
            ("version", VERSION_AT, &[0, 1]),
            (
                "hdr_size below 16 KiB",
                HDR_SIZE_AT,
                &0x2000u64.to_be_bytes(),
            ),
            (
                "hdr_size above 4 MiB",
                HDR_SIZE_AT,
                &0x80_0000u64.to_be_bytes(),
            ),
            ("hdr_offset", HDR_OFFSET_AT, &1u64.to_be_bytes()),
            ("JSON area", BINARY_HEADER_LEN, b"["),
            ("stored checksum", CHECKSUM_AT, &[0xff]),
        ];
        for (what, at, value) in corrupt {
            let mut bytes = device(9, JSON, 7, JSON);
            bytes[*at..*at + value.len()].copy_from_slice(value);
            let used = read_bytes(bytes).unwrap();
            assert_eq!(used.identity.copy, HeaderCopy::Secondary, "{what}");
            assert_eq!(used.identity.seqid, 7);
        }
        // The secondary is then found by scanning cryptsetup's offsets: at
        // 64 KiB here, after a primary of that size whose checksum fails.
        // It is the copy cryptsetup uses, and td refuses its size.
        let mut bytes = copy(false, 0, 0x1_0000, 9, JSON.as_bytes());
        bytes[BINARY_HEADER_LEN] = b' ';
        bytes.extend(copy(true, 0x1_0000, 0x1_0000, 7, JSON.as_bytes()));
        assert_eq!(
            read_bytes(bytes).unwrap().json().unwrap_err(),
            "LUKS2 secondary header copy has hdr_size 65536, not the 16384 td formats"
        );
        // A valid secondary must sit at its own size.
        let mut bytes = copy(false, 0, 0x4000, 9, JSON.as_bytes());
        bytes[BINARY_HEADER_LEN] = b' ';
        bytes.extend(copy(true, 0x4000, 0x8000, 7, JSON.as_bytes()));
        assert_eq!(read_bytes(bytes).unwrap_err(), "no valid LUKS2 header copy");
    }

    #[test]
    fn an_invalid_secondary_leaves_the_primary() {
        let mut bytes = device(7, JSON, 9, JSON);
        bytes[0x4000] = b'L';
        let used = read_bytes(bytes).unwrap();
        assert_eq!(
            (used.identity.copy, used.identity.seqid),
            (HeaderCopy::Primary, 7)
        );
        // A medium that ends inside the secondary is the same.
        let mut bytes = device(7, JSON, 9, JSON);
        bytes.truncate(0x4000 + 100);
        assert_eq!(
            read_bytes(bytes).unwrap().identity.copy,
            HeaderCopy::Primary
        );
    }

    #[test]
    fn no_valid_copy_and_unknown_algorithms_refuse() {
        for len in [0x10000, 100] {
            assert_eq!(
                read_bytes(vec![0; len]).unwrap_err(),
                "no valid LUKS2 header copy"
            );
        }
        let mut bytes = device(7, JSON, 7, JSON);
        bytes[CHECKSUM_ALG_AT..CHECKSUM_ALG_AT + 6].copy_from_slice(b"sha1\0\0");
        seal(&mut bytes[..0x4000]);
        assert_eq!(
            read_bytes(bytes).unwrap_err(),
            "LUKS2 header copy at 0 uses a checksum algorithm other than sha256"
        );
    }

    #[test]
    fn a_size_td_does_not_format_is_refused() {
        // 20 KiB passes the copy checks, as it does cryptsetup's, but is no
        // size td formats; nor is 32 KiB.
        for size in [0x5000u64, 0x8000] {
            let mut bytes = copy(false, 0, size, 7, JSON.as_bytes());
            bytes.extend(copy(true, size, size, 7, JSON.as_bytes()));
            assert_eq!(
                read_bytes(bytes).unwrap().json().unwrap_err(),
                format!("LUKS2 primary header copy has hdr_size {size}, not the 16384 td formats")
            );
        }
    }

    #[test]
    fn a_large_copy_is_refused_before_its_json_is_parsed() {
        // A checksum-valid 4 MiB primary whose JSON area is one string
        // filling it, with nothing after it: cryptsetup would use it, and
        // td refuses its size without handing the JSON to td-json.
        let size = 0x40_0000usize;
        let mut json = vec![b'b'; size - BINARY_HEADER_LEN - 1];
        json[..6].copy_from_slice(b"{\"a\":\"");
        let end = json.len();
        json[end - 2..].copy_from_slice(b"\"}");
        let used = read_bytes(copy(false, 0, size as u64, 9, &json)).unwrap();
        assert!(used.json.is_none(), "the large area is not even kept");
        assert_eq!(
            used.json().unwrap_err(),
            "LUKS2 primary header copy has hdr_size 4194304, not the 16384 td formats"
        );
    }

    #[test]
    fn the_json_area_is_one_object_then_nul_bytes() {
        let full = format!(
            "{{\"a\":\"{}\"}}",
            "b".repeat(0x4000 - BINARY_HEADER_LEN - 8)
        );
        assert_eq!(
            read_bytes(device(7, &full, 7, &full))
                .unwrap()
                .json()
                .unwrap_err(),
            "LUKS2 JSON area has no NUL padding"
        );
        let mut bytes = device(7, JSON, 7, JSON);
        bytes[0x4000 - 1] = b' ';
        seal(&mut bytes[..0x4000]);
        bytes[0x8000 - 1] = b' ';
        seal(&mut bytes[0x4000..0x8000]);
        assert_eq!(
            read_bytes(bytes).unwrap().json().unwrap_err(),
            "LUKS2 JSON area has bytes after its NUL padding"
        );
        for area in [" {}", "{} ", "{\"keyslots\":}"] {
            assert!(read_bytes(device(7, area, 7, area))
                .unwrap()
                .json()
                .is_err());
        }
    }

    #[test]
    fn metadata_is_one_bounded_object() {
        assert!(metadata_json(JSON.as_bytes()).is_ok());
        assert_eq!(
            metadata_json(b"[1]").unwrap_err(),
            "cryptsetup's LUKS2 metadata is not one object"
        );
        let over = format!("{{\"a\":\"{}\"}}", "b".repeat(MAX_METADATA_JSON));
        assert_eq!(
            metadata_json(over.as_bytes()).unwrap_err(),
            format!("cryptsetup's LUKS2 metadata is over {MAX_METADATA_JSON} bytes")
        );
    }

    #[test]
    fn costs_hold_each_pbkdf2_to_its_ceiling() {
        let header = |keyslot: u64, digest: u64, kind: &str| {
            td_json::parse_slice(format!(
                r#"{{"keyslots":{{"0":{{"type":"luks2","kdf":{{"type":"{kind}","hash":"sha256","iterations":{keyslot}}}}}}},"digests":{{"0":{{"type":"pbkdf2","hash":"sha256","iterations":{digest},"keyslots":["0"],"segments":["0"]}}}}}}"#
            ).as_bytes())
            .unwrap()
        };
        let cost = |json: &Json| costs(json).unwrap().remove(0).1;
        assert_eq!(
            cost(&header(1000, 1000, "pbkdf2")),
            Ok(Cost {
                keyslot: 1000,
                digest: 1000
            })
        );
        assert_eq!(
            cost(&header(
                MAX_KEYSLOT_ITERATIONS,
                MAX_DIGEST_ITERATIONS,
                "pbkdf2"
            )),
            Ok(Cost {
                keyslot: MAX_KEYSLOT_ITERATIONS,
                digest: MAX_DIGEST_ITERATIONS
            })
        );
        for (keyslot, digest, kind, refusal) in [
            (MAX_KEYSLOT_ITERATIONS + 1, 1000, "pbkdf2", "asks more than"),
            (1000, MAX_DIGEST_ITERATIONS + 1, "pbkdf2", "asks more than"),
            (0, 1000, "pbkdf2", "asks more than"),
            (1000, 1000, "argon2id", "is not PBKDF2-SHA256"),
        ] {
            let error = cost(&header(keyslot, digest, kind)).unwrap_err();
            assert!(error.contains(refusal), "{error}");
        }
    }

    #[test]
    fn the_test_length_bounds_the_keyslots_area_and_requirements_are_read() {
        let used = read_bytes(device(5, JSON, 5, JSON)).unwrap();
        let with = |config: &str| {
            td_json::parse_slice(
                format!(r#"{{"keyslots":{{}},"digests":{{}},"config":{config}}}"#).as_bytes(),
            )
            .unwrap()
        };
        let bound = format!(r#"{{"keyslots_size":"{MAX_KEYSLOTS_SIZE}"}}"#);
        assert_eq!(
            test_length(&used, &with(&bound)).unwrap(),
            2 * 0x4000 + MAX_KEYSLOTS_SIZE
        );
        for config in [
            format!(r#"{{"keyslots_size":"{}"}}"#, MAX_KEYSLOTS_SIZE + 1),
            r#"{"keyslots_size":16744448}"#.into(),
            "{}".into(),
        ] {
            assert!(test_length(&used, &with(&config)).is_err(), "{config}");
        }
        let reencrypting = with(r#"{"requirements":{"mandatory":["online-reencrypt-v2"]}}"#);
        assert!(volume(&reencrypting).unwrap().reencryption);
        assert!(
            !volume(&with(r#"{"requirements":{"mandatory":[]}}"#))
                .unwrap()
                .reencryption
        );
        assert!(volume(&with(r#"{"requirements":{"mandatory":"x"}}"#)).is_err());
        assert!(volume(&with(r#"{"requirements":{"mandatory":[7]}}"#)).is_err());
    }

    #[test]
    fn the_binary_claim_needs_its_position_magic_and_version_two() {
        let primary = copy(false, 0, 0x4000, 7, JSON.as_bytes());
        let secondary = copy(true, 0x4000, 0x4000, 7, JSON.as_bytes());
        let claim = Some(BinaryClaim {
            label: b"td-system",
            uuid: b"0f0e0d0c-0b0a-4908-8706-050403020100",
        });
        assert_eq!(binary_claim(&primary[..BINARY_HEADER_LEN], false), claim);
        assert_eq!(binary_claim(&secondary[..BINARY_HEADER_LEN], true), claim);
        // No checksum is verified: a corrupt area still reports its claim.
        let mut corrupt = primary.clone();
        corrupt[CHECKSUM_AT] ^= 1;
        assert_eq!(binary_claim(&corrupt, false), claim);
        // The other position's magic, another version, other bytes.
        assert_eq!(binary_claim(&primary, true), None);
        assert_eq!(binary_claim(&secondary, false), None);
        for version in [0u16, 1, 3, 0x0200] {
            let mut other = primary.clone();
            other[6..8].copy_from_slice(&version.to_be_bytes());
            assert_eq!(binary_claim(&other, false), None, "version {version}");
        }
        assert_eq!(binary_claim(&[0; BINARY_HEADER_LEN], false), None);
        assert_eq!(binary_claim(&primary[..7], false), None);
        // Fields are read up to their first NUL; a short header has none.
        let mut relabelled = primary.clone();
        relabelled[24..72].fill(0);
        relabelled[24..29].copy_from_slice(b"other");
        assert_eq!(
            binary_claim(&relabelled, false).map(|claim| claim.label),
            Some(b"other".as_slice())
        );
        assert_eq!(binary_claim(&primary[..30], false), None);
        assert_eq!(binary_claim(&primary[..200], false), None);
    }
}
