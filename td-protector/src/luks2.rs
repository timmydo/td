//! td's bounded LUKS2 header reader (DESIGN.md "LUKS2 tokens"). It selects
//! the header copy cryptsetup would use, by cryptsetup's own validity and
//! sequence rule, refuses copies that disagree, and returns the td tokens
//! of the copy it used. It runs before any C parser has seen the header.

use crate::token::{slot_number, Role, Token, MAX_SLOT, TOKEN_TYPE};
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
/// The most td tokens a header may carry, orphans included: a first-boot
/// and a device-bound protector, one superseded and one an interrupted
/// transition left.
pub const MAX_TD_TOKENS: usize = 4;

const PRIMARY_MAGIC: &[u8] = b"LUKS\xba\xbe";
const SECONDARY_MAGIC: &[u8] = b"SKUL\xba\xbe";
const CHECKSUM_ALG: &[u8] = b"sha256";
const MIN_HEADER_SIZE: u64 = 0x4000;
const MAX_HEADER_SIZE: u64 = 0x40_0000;
// Field offsets in the binary header (cryptsetup's `struct luks2_hdr_disk`).
const VERSION_AT: usize = 6;
const HDR_SIZE_AT: usize = 8;
const SEQID_AT: usize = 16;
const LABEL_AT: usize = 24;
const LABEL_LEN: usize = 48;
const CHECKSUM_ALG_AT: usize = 72;
const CHECKSUM_ALG_LEN: usize = 32;
const UUID_AT: usize = 168;
const UUID_LEN: usize = 40;
const SUBSYSTEM_AT: usize = 208;
const SUBSYSTEM_LEN: usize = 48;
const HDR_OFFSET_AT: usize = 256;
const CHECKSUM_AT: usize = 448;
const CHECKSUM_FIELD_LEN: usize = 64;
const SHA256_LEN: usize = 32;

/// Which copy the reader used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderCopy {
    Primary,
    Secondary,
}
impl HeaderCopy {
    fn name(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
        }
    }
}

/// What td reads from the used copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub copy: HeaderCopy,
    pub seqid: u64,
    pub hdr_size: u64,
    /// The binary header's UUID and label, up to their first NUL.
    pub uuid: String,
    pub label: String,
    /// The keyslot numbers present, ascending.
    pub keyslots: Vec<u8>,
    /// Each td token with its token number, ascending.
    pub tokens: Vec<(u8, Token)>,
    /// Each orphaned td token, naming no keyslot, with its token number,
    /// ascending: never released, left for the transition to remove.
    pub orphans: Vec<(u8, Role)>,
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
fn read_copy<R: Read + Seek>(
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
    if td_tpm::digest(&area) != stored {
        return Ok(None);
    }
    let json_area = area.get(BINARY_HEADER_LEN..).unwrap_or_default();
    let json_digest = td_tpm::digest(json_area);
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
/// only NUL bytes. Nothing longer reaches td-json.
fn json_area(json: &[u8]) -> Result<Json, String> {
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
    td_json::parse_slice(text).map_err(|e| format!("LUKS2 JSON area: {e}"))
}

/// An object keyed by slot numbers 0 to 31, as `(number, value)`.
fn numbered<'a>(header: &'a Json, section: &str) -> Result<Vec<(u8, &'a Json)>, String> {
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

fn header_from(copy: &ValidCopy, which: HeaderCopy) -> Result<Header, String> {
    // Refused before any JSON is parsed: a larger copy's area is not
    // handed to td-json.
    let json = match &copy.json {
        Some(json) if copy.hdr_size == FORMATTED_HEADER_SIZE => json,
        _ => {
            return Err(format!(
                "LUKS2 {} header copy has hdr_size {}, not the {FORMATTED_HEADER_SIZE} td formats",
                which.name(),
                copy.hdr_size
            ))
        }
    };
    let json = json_area(json)?;
    let keyslots: Vec<u8> = numbered(&json, "keyslots")?
        .into_iter()
        .map(|(number, _)| number)
        .collect();
    let mut tokens = Vec::new();
    let mut orphans = Vec::new();
    for (number, value) in numbered(&json, "tokens")? {
        let kind = value
            .get("type")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("LUKS2 token {number} has no type"))?;
        let named = value
            .get("keyslots")
            .and_then(Json::as_arr)
            .ok_or_else(|| format!("LUKS2 token {number} has no keyslots array"))?;
        for keyslot in named {
            let exists = keyslot
                .as_str()
                .and_then(slot_number)
                .is_some_and(|slot| keyslots.contains(&slot));
            if !exists {
                return Err(format!(
                    "LUKS2 token {number} names a keyslot the header does not have"
                ));
            }
        }
        if kind != TOKEN_TYPE {
            continue;
        }
        if tokens.len() + orphans.len() == MAX_TD_TOKENS {
            return Err(format!(
                "LUKS2 header carries more than {MAX_TD_TOKENS} td tokens"
            ));
        }
        if named.is_empty() {
            let role =
                Token::orphan_from_json(value).map_err(|e| format!("LUKS2 token {number}: {e}"))?;
            orphans.push((number, role));
            continue;
        }
        let token = Token::from_json(value).map_err(|e| format!("LUKS2 token {number}: {e}"))?;
        tokens.push((number, token));
    }
    let text = |at, len| -> Result<String, String> {
        Ok(String::from_utf8_lossy(text_at(&copy.binary, at, len)?).into_owned())
    };
    Ok(Header {
        copy: which,
        seqid: copy.seqid,
        hdr_size: copy.hdr_size,
        uuid: text(UUID_AT, UUID_LEN)?,
        label: text(LABEL_AT, LABEL_LEN)?,
        keyslots,
        tokens,
        orphans,
    })
}

/// Read the LUKS2 header at the start of `device` as cryptsetup would
/// choose its copy, and return the used copy's td tokens. A used copy
/// that fails td's own checks refuses the header rather than falling back.
pub fn read<R: Read + Seek>(device: &mut R) -> Result<Header, String> {
    let primary = read_copy(device, 0, false)?;
    let secondary = match &primary {
        Some(primary) => read_copy(device, primary.hdr_size, true)?,
        None => {
            let mut found = None;
            for offset in HEADER_SIZES {
                found = read_copy(device, *offset, true)?;
                if found.is_some() {
                    break;
                }
            }
            found
        }
    };
    let (copy, which) = match (&primary, &secondary) {
        (Some(primary), Some(secondary)) => {
            if primary.seqid > secondary.seqid {
                (primary, HeaderCopy::Primary)
            } else if primary.seqid < secondary.seqid {
                (secondary, HeaderCopy::Secondary)
            } else if primary.hdr_size != secondary.hdr_size
                || shared(primary)? != shared(secondary)?
            {
                return Err(format!(
                    "LUKS2 header copies disagree at the same sequence number {}",
                    primary.seqid
                ));
            } else {
                (primary, HeaderCopy::Primary)
            }
        }
        (Some(primary), None) => (primary, HeaderCopy::Primary),
        (None, Some(secondary)) => (secondary, HeaderCopy::Secondary),
        (None, None) => return Err("no valid LUKS2 header copy".into()),
    };
    header_from(copy, which)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Role;
    use std::io::Cursor;
    use td_tpm::SealedObject;

    fn td_token(keyslot: u8, role: Role) -> String {
        Token::new(
            keyslot,
            role,
            SealedObject {
                public: vec![0x00, 0x08, keyslot],
                private: vec![0x01, keyslot],
            },
        )
        .unwrap()
        .encode()
    }

    fn header_json(tokens: &str) -> String {
        format!(
            r#"{{"keyslots":{{"0":{{"type":"luks2"}},"1":{{"type":"luks2"}}}},"tokens":{{{tokens}}},"segments":{{"0":{{"type":"crypt"}}}},"digests":{{}},"config":{{"json_size":"12288","keyslots_size":"16744448"}}}}"#
        )
    }

    fn standard() -> String {
        header_json(&format!(
            r#""0":{},"1":{{"type":"systemd-tpm2","keyslots":["0"],"tpm2-pcrs":[7]}}"#,
            td_token(1, Role::FirstBoot)
        ))
    }

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
        seal(&mut area);
        area
    }

    fn seal(area: &mut [u8]) {
        area[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_FIELD_LEN].fill(0);
        let digest = td_tpm::digest(area);
        area[CHECKSUM_AT..CHECKSUM_AT + SHA256_LEN].copy_from_slice(&digest);
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

    fn read_bytes(bytes: Vec<u8>) -> Result<Header, String> {
        read(&mut Cursor::new(bytes))
    }

    #[test]
    fn agreeing_copies_yield_the_primary_and_its_td_tokens() {
        let json = standard();
        let header = read_bytes(device(7, &json, 7, &json)).unwrap();
        assert_eq!(header.copy, HeaderCopy::Primary);
        assert_eq!(header.seqid, 7);
        assert_eq!(header.hdr_size, 0x4000);
        assert_eq!(header.uuid, "0f0e0d0c-0b0a-4908-8706-050403020100");
        assert_eq!(header.label, "td-system");
        assert_eq!(header.keyslots, [0, 1]);
        // The systemd token is another type and is ignored.
        assert_eq!(header.tokens.len(), 1);
        let (number, token) = &header.tokens[0];
        assert_eq!(*number, 0);
        assert_eq!(token.keyslot(), 1);
        assert_eq!(token.role(), Role::FirstBoot);
        assert_eq!(token.sealed().public, [0x00, 0x08, 1]);
    }

    #[test]
    fn the_higher_sequence_number_wins_as_cryptsetup_chooses() {
        let older = standard();
        let newer = header_json(&format!(
            r#""0":{},"2":{}"#,
            td_token(1, Role::FirstBoot),
            td_token(0, Role::DeviceBound)
        ));
        let header = read_bytes(device(7, &older, 8, &newer)).unwrap();
        assert_eq!(header.copy, HeaderCopy::Secondary);
        assert_eq!(header.seqid, 8);
        assert_eq!(header.tokens.len(), 2);
        assert_eq!(header.tokens[1].0, 2);
        assert_eq!(header.tokens[1].1.role(), Role::DeviceBound);
        let header = read_bytes(device(9, &older, 8, &newer)).unwrap();
        assert_eq!(header.copy, HeaderCopy::Primary);
        assert_eq!(header.tokens.len(), 1);
    }

    #[test]
    fn copies_that_disagree_at_one_sequence_number_are_refused() {
        let other = header_json(&format!(r#""0":{}"#, td_token(0, Role::FirstBoot)));
        let refused = read_bytes(device(7, &standard(), 7, &other)).unwrap_err();
        assert_eq!(
            refused,
            "LUKS2 header copies disagree at the same sequence number 7"
        );
        // A differing label is a disagreement too, but the salt is not.
        let json = standard();
        let mut bytes = device(7, &json, 7, &json);
        bytes[0x4000 + 24] = b'T';
        seal(&mut bytes[0x4000..0x8000]);
        assert!(read_bytes(bytes).unwrap_err().contains("disagree"));
    }

    #[test]
    fn an_invalid_primary_falls_back_to_the_secondary() {
        let json = standard();
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
            let mut bytes = device(9, &json, 7, &json);
            bytes[*at..*at + value.len()].copy_from_slice(value);
            let header = read_bytes(bytes).unwrap();
            assert_eq!(header.copy, HeaderCopy::Secondary, "{what}");
            assert_eq!(header.seqid, 7);
        }
        // The secondary is then found by scanning cryptsetup's offsets: at
        // 64 KiB here, after a primary of that size whose checksum fails.
        // It is the copy cryptsetup uses, and td refuses its size.
        let mut bytes = copy(false, 0, 0x1_0000, 9, json.as_bytes());
        bytes[BINARY_HEADER_LEN] = b' ';
        bytes.extend(copy(true, 0x1_0000, 0x1_0000, 7, json.as_bytes()));
        assert_eq!(
            read_bytes(bytes).unwrap_err(),
            "LUKS2 secondary header copy has hdr_size 65536, not the 16384 td formats"
        );
        // A valid secondary must sit at its own size.
        let mut bytes = copy(false, 0, 0x4000, 9, json.as_bytes());
        bytes[BINARY_HEADER_LEN] = b' ';
        bytes.extend(copy(true, 0x4000, 0x8000, 7, json.as_bytes()));
        assert_eq!(read_bytes(bytes).unwrap_err(), "no valid LUKS2 header copy");
    }

    #[test]
    fn an_invalid_secondary_leaves_the_primary() {
        let json = standard();
        let mut bytes = device(7, &json, 9, &json);
        bytes[0x4000] = b'L';
        let header = read_bytes(bytes).unwrap();
        assert_eq!((header.copy, header.seqid), (HeaderCopy::Primary, 7));
        // A medium that ends inside the secondary is the same.
        let mut bytes = device(7, &json, 9, &json);
        bytes.truncate(0x4000 + 100);
        assert_eq!(read_bytes(bytes).unwrap().copy, HeaderCopy::Primary);
    }

    #[test]
    fn no_valid_copy_and_unknown_algorithms_refuse() {
        assert_eq!(
            read_bytes(vec![0; 0x10000]).unwrap_err(),
            "no valid LUKS2 header copy"
        );
        assert_eq!(
            read_bytes(vec![0; 100]).unwrap_err(),
            "no valid LUKS2 header copy"
        );
        let json = standard();
        let mut bytes = device(7, &json, 7, &json);
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
        let json = standard();
        for size in [0x5000u64, 0x8000] {
            let mut bytes = copy(false, 0, size, 7, json.as_bytes());
            bytes.extend(copy(true, size, size, 7, json.as_bytes()));
            assert_eq!(
                read_bytes(bytes).unwrap_err(),
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
        let bytes = copy(false, 0, size as u64, 9, &json);
        let started = std::time::Instant::now();
        assert_eq!(
            read_bytes(bytes).unwrap_err(),
            "LUKS2 primary header copy has hdr_size 4194304, not the 16384 td formats"
        );
        eprintln!("4 MiB primary refused in {:?}", started.elapsed());
    }

    #[test]
    fn malformed_json_areas_are_refused() {
        let duplicate = standard().replacen(r#""digests""#, r#""tokens":{},"digests""#, 1);
        let cases: &[(&[u8], &str)] = &[
            (b"{\"keyslots\":}", "LUKS2 JSON area: "),
            (b"{\"keyslots\":", "not one object"),
            (b" {}", "not one object"),
            (b"{} ", "not one object"),
            (duplicate.as_bytes(), "duplicate object key `tokens'"),
            (b"{}", "no keyslots object"),
            (b"{\"keyslots\":{}}", "no tokens object"),
            (
                b"{\"keyslots\":{\"01\":{}},\"tokens\":{}}",
                "keyslots key \"01\" is not a number",
            ),
            (
                b"{\"keyslots\":{},\"tokens\":{\"32\":{}}}",
                "tokens key \"32\" is not a number",
            ),
            (
                b"{\"keyslots\":{},\"tokens\":{\"0\":{\"keyslots\":[]}}}",
                "token 0 has no type",
            ),
            (
                b"{\"keyslots\":{},\"tokens\":{\"0\":{\"type\":\"x\"}}}",
                "token 0 has no keyslots array",
            ),
            (
                b"{\"keyslots\":{\"0\":{}},\"tokens\":{\"0\":{\"type\":\"x\",\"keyslots\":[\"1\"]}}}",
                "token 0 names a keyslot the header does not have",
            ),
        ];
        for (json, expected) in cases {
            let mut bytes = copy(false, 0, 0x4000, 7, json);
            bytes.extend(copy(true, 0x4000, 0x4000, 7, json));
            let refused = read_bytes(bytes).unwrap_err();
            assert!(refused.contains(expected), "{refused}");
        }
        // A td token that is not exactly the format refuses the header.
        let token = td_token(1, Role::FirstBoot).replacen(r#""role""#, r#""x":1,"role""#, 1);
        let bad = header_json(&format!(r#""3":{token}"#));
        let refused = read_bytes(device(7, &bad, 7, &bad)).unwrap_err();
        assert_eq!(refused, "LUKS2 token 3: td token has an unknown key \"x\"");
        // An area the JSON fills has no NUL; bytes after the NUL refuse.
        let full = format!(
            "{{\"a\":\"{}\"}}",
            "b".repeat(0x4000 - BINARY_HEADER_LEN - 8)
        );
        let refused = read_bytes(device(7, &full, 7, &full)).unwrap_err();
        assert_eq!(refused, "LUKS2 JSON area has no NUL padding");
        let json = standard();
        let mut bytes = device(7, &json, 7, &json);
        bytes[0x4000 - 1] = b' ';
        seal(&mut bytes[..0x4000]);
        bytes[0x8000 - 1] = b' ';
        seal(&mut bytes[0x4000..0x8000]);
        assert_eq!(
            read_bytes(bytes).unwrap_err(),
            "LUKS2 JSON area has bytes after its NUL padding"
        );
    }

    #[test]
    fn td_tokens_are_bounded() {
        let four: Vec<String> = (0..4)
            .map(|number| format!(r#""{number}":{}"#, td_token(1, Role::DeviceBound)))
            .collect();
        let json4 = header_json(&four.join(","));
        assert_eq!(
            read_bytes(device(7, &json4, 7, &json4))
                .unwrap()
                .tokens
                .len(),
            4
        );
        let mut five = four;
        five.push(format!(r#""9":{}"#, td_token(0, Role::FirstBoot)));
        let json5 = header_json(&five.join(","));
        assert_eq!(
            read_bytes(device(7, &json5, 7, &json5)).unwrap_err(),
            "LUKS2 header carries more than 4 td tokens"
        );
    }

    /// A td token whose keyslot cryptsetup destroyed: it names none.
    fn orphan(role: Role) -> String {
        td_token(1, role).replacen(r#"["1"]"#, "[]", 1)
    }

    #[test]
    fn orphaned_td_tokens_are_reported_not_released() {
        let json = header_json(&format!(
            r#""0":{},"2":{}"#,
            orphan(Role::FirstBoot),
            td_token(0, Role::DeviceBound)
        ));
        let header = read_bytes(device(7, &json, 7, &json)).unwrap();
        assert_eq!(header.orphans, [(0, Role::FirstBoot)]);
        assert_eq!(header.tokens.len(), 1);
        assert_eq!(header.tokens[0].0, 2);
        assert_eq!(header.tokens[0].1.keyslot(), 0);
        // Orphans count toward the bound.
        let mut five: Vec<String> = (0..4)
            .map(|number| format!(r#""{number}":{}"#, orphan(Role::DeviceBound)))
            .collect();
        let json4 = header_json(&five.join(","));
        let header = read_bytes(device(7, &json4, 7, &json4)).unwrap();
        assert_eq!((header.tokens.len(), header.orphans.len()), (0, 4));
        five.push(format!(r#""9":{}"#, td_token(1, Role::FirstBoot)));
        let json5 = header_json(&five.join(","));
        assert_eq!(
            read_bytes(device(7, &json5, 7, &json5)).unwrap_err(),
            "LUKS2 header carries more than 4 td tokens"
        );
        // An orphan is otherwise the format.
        let bad = orphan(Role::FirstBoot).replacen(r#""role""#, r#""x":1,"role""#, 1);
        let json = header_json(&format!(r#""5":{bad}"#));
        assert_eq!(
            read_bytes(device(7, &json, 7, &json)).unwrap_err(),
            "LUKS2 token 5: td token has an unknown key \"x\""
        );
        // A td token naming two keyslots, which cryptsetup's validation
        // admits, is not td's format and refuses the header.
        let two = td_token(1, Role::FirstBoot).replacen(r#"["1"]"#, r#"["0","1"]"#, 1);
        let json = header_json(&format!(r#""0":{two}"#));
        assert_eq!(
            read_bytes(device(7, &json, 7, &json)).unwrap_err(),
            "LUKS2 token 0: td token does not name exactly one keyslot"
        );
    }

    #[test]
    fn read_errors_other_than_the_end_refuse() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("medium error"))
            }
        }
        impl Seek for Failing {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                Ok(0)
            }
        }
        assert_eq!(
            read(&mut Failing).unwrap_err(),
            "read LUKS2 header copy at 0: medium error"
        );
    }
}
