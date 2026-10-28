//! LUKS2 header writes and the keyslot and token operations of td's
//! profile (DESIGN.md "The profile", "Commands", "Header writes"). Every
//! operation reads and checks the header first, builds the whole change
//! as a `Plan`, and commits it in one ordered sequence of writes; a
//! header outside the profile is refused before anything is written.

use crate::encoding::{base64, from_base64};
use crate::header::{
    self, numbered, slot_number, HeaderCopy, BINARY_HEADER_LEN, CHECKSUM_ALG_AT, CHECKSUM_AT,
    CHECKSUM_FIELD_LEN, FORMATTED_HEADER_SIZE, HDR_OFFSET_AT, HDR_SIZE_AT, JSON_AREA_LEN, LABEL_AT,
    LABEL_LEN, MAX_DIGEST_ITERATIONS, MAX_KEYSLOT_ITERATIONS, MAX_SLOT, PRIMARY_MAGIC,
    SECONDARY_MAGIC, SEQID_AT, SHA256_LEN, SUBSYSTEM_AT, SUBSYSTEM_LEN, UUID_AT, UUID_LEN,
    VERSION_AT,
};
use crate::keyslot::{self, Key, Random, AREA_LEN, ITERATIONS, KEY_LEN, SALT_LEN, SPLIT_LEN};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use td_json::{Json, ObjectBuilder};

/// Where the keyslots area starts: after both 16 KiB header copies.
pub const KEYSLOTS_OFFSET: u64 = 2 * FORMATTED_HEADER_SIZE;
/// The keyslots area td formats.
pub const KEYSLOTS_SIZE: u64 = 16_744_448;
/// Where the data segment starts: 16 MiB.
pub const DATA_OFFSET: u64 = KEYSLOTS_OFFSET + KEYSLOTS_SIZE;
/// The data segment's sector, and the alignment of keyslot areas.
pub const SECTOR_SIZE: u64 = 4096;
/// The volume's and every keyslot area's cipher.
pub const CIPHER: &str = "aes-xts-plain64";
/// The most token JSON td-protector's runner hands over: one pipe page.
pub const MAX_TOKEN_JSON: usize = 4096;
/// The longest passphrase or key file td-luks reads: one pipe page.
pub const MAX_KEY: usize = 4096;
/// The shortest key `kill_slot` takes: td's shortest secret, a
/// protector's 32 bytes. cryptsetup kills unasked on empty input.
pub const MIN_KILL_KEY: usize = 32;
/// The binary header's salt, drawn anew for each copy at each commit.
const SALT_AT: usize = 104;
const COPY_SALT_LEN: usize = 64;
const JSON_SIZE: u64 = FORMATTED_HEADER_SIZE - BINARY_HEADER_LEN as u64;
const SECTIONS: &[&str] = &["keyslots", "tokens", "segments", "digests", "config"];

/// Why an operation did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The key opens no candidate keyslot: cryptsetup's wrong-passphrase
    /// status.
    WrongKey,
    /// Anything else, with its reason.
    Refused(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKey => f.write_str("no keyslot opens with that key"),
            Self::Refused(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(reason: String) -> Self {
        Self::Refused(reason)
    }
}

impl From<&str> for Error {
    fn from(reason: &str) -> Self {
        Self::Refused(reason.to_owned())
    }
}

fn refused<T>(reason: impl Into<String>) -> Result<T, Error> {
    Err(Error::Refused(reason.into()))
}

/// What a header lives on: a device or a regular file. Positioned I/O
/// only, so a test can fail any write.
pub trait Medium {
    /// Read at `offset`; 0 at the end.
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize>;
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
    fn size(&mut self) -> io::Result<u64>;
}

impl Medium for File {
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
        FileExt::read_at(self, out, offset)
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.write_all_at(bytes, offset)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.sync_data()
    }

    // A block device's metadata length is 0; its end is its size.
    fn size(&mut self) -> io::Result<u64> {
        self.seek(SeekFrom::End(0))
    }
}

/// A `Medium` as the reader's `Read + Seek`.
struct Reading<'m, M: Medium + ?Sized> {
    medium: &'m mut M,
    at: u64,
}

impl<M: Medium + ?Sized> Read for Reading<'_, M> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let read = self.medium.read_at(self.at, out)?;
        self.at = self.at.saturating_add(read as u64);
        Ok(read)
    }
}

impl<M: Medium + ?Sized> Seek for Reading<'_, M> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match position {
            SeekFrom::Start(at) => {
                self.at = at;
                Ok(at)
            }
            _ => Err(io::Error::other("the LUKS2 reader seeks from the start")),
        }
    }
}

fn read_exact_at<M: Medium + ?Sized>(
    medium: &mut M,
    offset: u64,
    out: &mut [u8],
) -> io::Result<()> {
    Reading { medium, at: offset }.read_exact(out)
}

/// A checked header of td's profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub seqid: u64,
    pub uuid: String,
    pub label: String,
    pub json: Json,
    /// The copy the reader used, which the next commit writes last.
    pub used: HeaderCopy,
}

fn copy_offset(copy: HeaderCopy) -> u64 {
    match copy {
        HeaderCopy::Primary => 0,
        HeaderCopy::Secondary => FORMATTED_HEADER_SIZE,
    }
}

/// Refuse a binary header whose label, UUID or subsystem a commit would
/// not write back byte for byte: each field is UTF-8 up to a NUL, then
/// only NUL bytes, and the subsystem is empty, as td formats it.
fn round_trips(binary: &[u8]) -> Result<(), String> {
    for (what, at, len) in [
        ("label", LABEL_AT, LABEL_LEN),
        ("UUID", UUID_AT, UUID_LEN),
        ("subsystem", SUBSYSTEM_AT, SUBSYSTEM_LEN),
    ] {
        let field = at
            .checked_add(len)
            .and_then(|end| binary.get(at..end))
            .ok_or("LUKS2 binary header is truncated")?;
        let end = field.iter().position(|byte| *byte == 0);
        let (text, padding) = field
            .split_at_checked(end.unwrap_or(len))
            .unwrap_or_default();
        if end.is_none()
            || padding.iter().any(|byte| *byte != 0)
            || std::str::from_utf8(text).is_err()
            || (what == "subsystem" && !text.is_empty())
        {
            return Err(format!(
                "LUKS2 header's {what} field is not one td writes back unchanged"
            ));
        }
    }
    Ok(())
}

/// Read the header the reader chooses and refuse it unless it is td's
/// profile.
pub fn load<M: Medium + ?Sized>(medium: &mut M) -> Result<Header, Error> {
    let used = header::read_copy(&mut Reading {
        medium: &mut *medium,
        at: 0,
    })?;
    let (json, _) = used.json()?;
    profile(&json)?;
    let mut binary = vec![0u8; BINARY_HEADER_LEN];
    read_exact_at(medium, copy_offset(used.identity.copy), &mut binary)
        .map_err(io("LUKS2 binary header read"))?;
    round_trips(&binary)?;
    Ok(Header {
        seqid: used.identity.seqid,
        uuid: used.identity.uuid,
        label: used.identity.label,
        json,
        used: used.identity.copy,
    })
}

fn object<'a>(json: &'a Json, what: &str) -> Result<&'a [(String, Json)], String> {
    json.as_obj()
        .ok_or_else(|| format!("LUKS2 {what} is not an object"))
}

/// `value`'s keys are exactly `keys`, in any order.
fn exactly(value: &Json, keys: &[&str], what: &str) -> Result<(), String> {
    let pairs = object(value, what)?;
    for (key, _) in pairs {
        if !keys.contains(&key.as_str()) {
            return Err(format!(
                "LUKS2 {what} carries {key:?}, outside td's profile"
            ));
        }
    }
    for key in keys {
        if value.get(key).is_none() {
            return Err(format!("LUKS2 {what} has no {key:?}"));
        }
    }
    Ok(())
}

fn text_is(value: &Json, key: &str, want: &str, what: &str) -> Result<(), String> {
    if value.get(key).and_then(Json::as_str) == Some(want) {
        Ok(())
    } else {
        Err(format!("LUKS2 {what}'s {key} is not {want:?}"))
    }
}

fn number_is(value: &Json, key: &str, want: u64, what: &str) -> Result<(), String> {
    if value.get(key).and_then(Json::as_u64) == Some(want) {
        Ok(())
    } else {
        Err(format!("LUKS2 {what}'s {key} is not {want}"))
    }
}

/// A decimal string offset or size, as LUKS2 writes them.
fn decimal(value: &Json, key: &str, what: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(Json::as_str)
        .filter(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| format!("LUKS2 {what}'s {key} is not a decimal string"))
}

/// A base64 field of exactly `len` bytes.
fn bytes_of(value: &Json, key: &str, len: usize, what: &str) -> Result<Vec<u8>, String> {
    value
        .get(key)
        .and_then(Json::as_str)
        .and_then(from_base64)
        .filter(|bytes| bytes.len() == len)
        .ok_or_else(|| format!("LUKS2 {what}'s {key} is not {len} bytes of base64"))
}

fn iterations(value: &Json, ceiling: u64, what: &str) -> Result<u32, String> {
    value
        .get("iterations")
        .and_then(Json::as_u64)
        .filter(|count| (1..=ceiling).contains(count))
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(|| format!("LUKS2 {what}'s iterations are not 1 to {ceiling}"))
}

/// The slot numbers an array of slot-number strings names.
fn slots(value: &Json, key: &str, what: &str) -> Result<Vec<u8>, String> {
    let array = value
        .get(key)
        .and_then(Json::as_arr)
        .ok_or_else(|| format!("LUKS2 {what} has no {key} array"))?;
    let mut out = Vec::with_capacity(array.len());
    for item in array {
        let number = item
            .as_str()
            .and_then(slot_number)
            .ok_or_else(|| format!("LUKS2 {what}'s {key} names no slot number"))?;
        if out.contains(&number) {
            return Err(format!("LUKS2 {what}'s {key} repeats {number}"));
        }
        out.push(number);
    }
    Ok(out)
}

/// One keyslot of the profile: its area's offset.
fn keyslot_profile(number: u8, keyslot: &Json) -> Result<u64, String> {
    let what = format!("keyslot {number}");
    let keys: &[&str] = if keyslot.get("priority").is_some() {
        &["type", "key_size", "af", "area", "kdf", "priority"]
    } else {
        &["type", "key_size", "af", "area", "kdf"]
    };
    exactly(keyslot, keys, &what)?;
    text_is(keyslot, "type", "luks2", &what)?;
    number_is(keyslot, "key_size", KEY_LEN as u64, &what)?;
    if let Some(priority) = keyslot.get("priority") {
        if !matches!(priority.as_u64(), Some(0..=2)) {
            return Err(format!("LUKS2 {what}'s priority is not 0 to 2"));
        }
    }
    let af = keyslot.get("af").unwrap_or(&Json::Null);
    let af_what = format!("{what} AF");
    exactly(af, &["type", "stripes", "hash"], &af_what)?;
    text_is(af, "type", "luks1", &af_what)?;
    number_is(af, "stripes", keyslot::STRIPES as u64, &af_what)?;
    text_is(af, "hash", "sha256", &af_what)?;
    let area = keyslot.get("area").unwrap_or(&Json::Null);
    let area_what = format!("{what} area");
    exactly(
        area,
        &["type", "offset", "size", "encryption", "key_size"],
        &area_what,
    )?;
    text_is(area, "type", "raw", &area_what)?;
    text_is(area, "encryption", CIPHER, &area_what)?;
    number_is(area, "key_size", KEY_LEN as u64, &area_what)?;
    if decimal(area, "size", &area_what)? != AREA_LEN {
        return Err(format!("LUKS2 {area_what}'s size is not {AREA_LEN}"));
    }
    let offset = decimal(area, "offset", &area_what)?;
    if offset < KEYSLOTS_OFFSET
        || !offset.is_multiple_of(SECTOR_SIZE)
        || offset.saturating_add(AREA_LEN) > DATA_OFFSET
    {
        return Err(format!(
            "LUKS2 {area_what} at {offset} is outside the keyslots area"
        ));
    }
    let kdf = keyslot.get("kdf").unwrap_or(&Json::Null);
    let kdf_what = format!("{what} KDF");
    exactly(kdf, &["type", "hash", "iterations", "salt"], &kdf_what)?;
    text_is(kdf, "type", "pbkdf2", &kdf_what)?;
    text_is(kdf, "hash", "sha256", &kdf_what)?;
    iterations(kdf, MAX_KEYSLOT_ITERATIONS, &kdf_what)?;
    bytes_of(kdf, "salt", SALT_LEN, &kdf_what)?;
    Ok(offset)
}

/// Refuse any header outside td's profile (DESIGN.md "The profile").
pub fn profile(json: &Json) -> Result<(), String> {
    exactly(json, SECTIONS, "header")?;
    let config = json.get("config").unwrap_or(&Json::Null);
    if let Some(mandatory) = config
        .get_path(&["requirements", "mandatory"])
        .and_then(Json::as_arr)
    {
        if let Some(first) = mandatory.first() {
            return Err(format!(
                "LUKS2 header carries the mandatory requirement {first}, outside td's profile"
            ));
        }
    }
    exactly(config, &["json_size", "keyslots_size"], "config")?;
    if decimal(config, "json_size", "config")? != JSON_SIZE
        || decimal(config, "keyslots_size", "config")? != KEYSLOTS_SIZE
    {
        return Err("LUKS2 config's sizes are not td's".into());
    }
    let segments = numbered(json, "segments")?;
    let [(0, segment)] = segments.as_slice() else {
        return Err("LUKS2 header has segments other than one segment 0".into());
    };
    exactly(
        segment,
        &[
            "type",
            "offset",
            "size",
            "iv_tweak",
            "encryption",
            "sector_size",
        ],
        "segment 0",
    )?;
    text_is(segment, "type", "crypt", "segment 0")?;
    text_is(segment, "offset", &DATA_OFFSET.to_string(), "segment 0")?;
    text_is(segment, "size", "dynamic", "segment 0")?;
    text_is(segment, "iv_tweak", "0", "segment 0")?;
    text_is(segment, "encryption", CIPHER, "segment 0")?;
    number_is(segment, "sector_size", SECTOR_SIZE, "segment 0")?;
    let keyslots = numbered(json, "keyslots")?;
    let mut areas: Vec<u64> = Vec::with_capacity(keyslots.len());
    for (number, keyslot) in &keyslots {
        areas.push(keyslot_profile(*number, keyslot)?);
    }
    areas.sort_unstable();
    if areas
        .windows(2)
        .any(|pair| matches!(pair, [a, b] if a.saturating_add(AREA_LEN) > *b))
    {
        return Err("LUKS2 keyslot areas overlap".into());
    }
    let exists = |slot: &u8| keyslots.iter().any(|(number, _)| number == slot);
    let mut digested: Vec<u8> = Vec::new();
    let mut segment_digest = None;
    for (number, digest) in numbered(json, "digests")? {
        let what = format!("digest {number}");
        exactly(
            digest,
            &[
                "type",
                "keyslots",
                "segments",
                "hash",
                "iterations",
                "salt",
                "digest",
            ],
            &what,
        )?;
        text_is(digest, "type", "pbkdf2", &what)?;
        text_is(digest, "hash", "sha256", &what)?;
        iterations(digest, MAX_DIGEST_ITERATIONS, &what)?;
        bytes_of(digest, "salt", SALT_LEN, &what)?;
        bytes_of(digest, "digest", keyslot::DIGEST_LEN, &what)?;
        for slot in slots(digest, "keyslots", &what)? {
            if !exists(&slot) || digested.contains(&slot) {
                return Err(format!(
                    "LUKS2 {what} names keyslot {slot}, which is absent or digested twice"
                ));
            }
            digested.push(slot);
        }
        let segments = slots(digest, "segments", &what)?;
        if segments.iter().any(|s| *s != 0) {
            return Err(format!("LUKS2 {what} names a segment other than 0"));
        }
        // One volume key: a second digest of segment 0 is a reencryption's.
        if !segments.is_empty() {
            if let Some(first) = segment_digest.replace(number) {
                return Err(format!(
                    "LUKS2 digests {first} and {number} both name segment 0"
                ));
            }
        }
    }
    for (number, _) in &keyslots {
        if !digested.contains(number) {
            return Err(format!("LUKS2 keyslot {number} has no digest"));
        }
    }
    for (number, token) in numbered(json, "tokens")? {
        let what = format!("token {number}");
        token
            .get("type")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("LUKS2 {what} has no type"))?;
        for slot in slots(token, "keyslots", &what)? {
            if !exists(&slot) {
                return Err(format!("LUKS2 {what} names absent keyslot {slot}"));
            }
        }
    }
    Ok(())
}

/// A change to commit: the new JSON and the keyslots-area writes that
/// come before it.
#[derive(Debug, Clone)]
pub struct Plan {
    pub uuid: String,
    pub label: String,
    pub seqid: u64,
    pub json: Json,
    /// Positioned writes made and synced before either header copy.
    pub areas: Vec<(u64, Vec<u8>)>,
    /// The copy the reader used, written last: the other is written
    /// first, so the copy the reader relies on stays valid until a new
    /// one is (DESIGN.md "Commits").
    pub used: HeaderCopy,
}

/// One whole copy: the binary header and the JSON area, its checksum
/// computed with the checksum field zeroed.
fn copy_area(plan: &Plan, secondary: bool, salt: &[u8], json: &[u8]) -> Result<Vec<u8>, String> {
    let mut area = vec![0u8; FORMATTED_HEADER_SIZE as usize];
    let mut put = |at: usize, bytes: &[u8]| -> Result<(), String> {
        area.get_mut(at..at.saturating_add(bytes.len()))
            .ok_or_else(|| "LUKS2 header field overflows its copy".to_string())?
            .copy_from_slice(bytes);
        Ok(())
    };
    put(
        0,
        if secondary {
            SECONDARY_MAGIC
        } else {
            PRIMARY_MAGIC
        },
    )?;
    put(VERSION_AT, &2u16.to_be_bytes())?;
    put(HDR_SIZE_AT, &FORMATTED_HEADER_SIZE.to_be_bytes())?;
    put(SEQID_AT, &plan.seqid.to_be_bytes())?;
    put(LABEL_AT, plan.label.as_bytes())?;
    put(CHECKSUM_ALG_AT, b"sha256")?;
    put(SALT_AT, salt)?;
    put(UUID_AT, plan.uuid.as_bytes())?;
    let offset = if secondary { FORMATTED_HEADER_SIZE } else { 0 };
    put(HDR_OFFSET_AT, &offset.to_be_bytes())?;
    put(BINARY_HEADER_LEN, json)?;
    let checksum = td_fido::digest(&area);
    area.get_mut(CHECKSUM_AT..CHECKSUM_AT + SHA256_LEN)
        .ok_or("LUKS2 checksum field overflows its copy")?
        .copy_from_slice(&checksum);
    Ok(area)
}

fn io(what: &str) -> impl Fn(io::Error) -> Error + '_ {
    move |e| Error::Refused(format!("{what}: {e}"))
}

/// Commit `plan`: the keyslots-area writes, synced, then the copy the
/// reader did not use and then the one it did, each as its binary header
/// without its checksum, its JSON area, then the binary header with it,
/// and a sync (cryptsetup's `hdr_write_disk`). A crash leaves one valid
/// copy with the old JSON or the new.
pub fn commit<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    plan: &Plan,
) -> Result<(), Error> {
    profile(&plan.json)?;
    write_plan(medium, random, plan)
}

/// `commit` without the profile check.
fn write_plan<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    plan: &Plan,
) -> Result<(), Error> {
    if plan.label.len() >= LABEL_LEN || plan.uuid.len() >= UUID_LEN {
        return refused("LUKS2 label or UUID is too long");
    }
    let text = plan.json.to_string();
    if text.len() >= JSON_AREA_LEN {
        return refused(format!(
            "LUKS2 JSON is {} bytes; its area holds {}",
            text.len(),
            JSON_AREA_LEN - 1
        ));
    }
    let mut json = vec![0u8; JSON_AREA_LEN];
    json.get_mut(..text.len())
        .ok_or("LUKS2 JSON overflows its area")?
        .copy_from_slice(text.as_bytes());
    let order = match plan.used {
        HeaderCopy::Primary => [HeaderCopy::Secondary, HeaderCopy::Primary],
        HeaderCopy::Secondary => [HeaderCopy::Primary, HeaderCopy::Secondary],
    };
    let mut copies = Vec::with_capacity(2);
    for copy in order {
        let mut salt = [0u8; COPY_SALT_LEN];
        random.fill(&mut salt).map_err(io("header salt"))?;
        let secondary = copy == HeaderCopy::Secondary;
        copies.push((copy, copy_area(plan, secondary, &salt, &json)?));
    }
    for (offset, bytes) in &plan.areas {
        medium
            .write_at(*offset, bytes)
            .map_err(io("keyslots area write"))?;
    }
    if !plan.areas.is_empty() {
        medium.sync().map_err(io("keyslots area sync"))?;
    }
    for (copy, area) in &copies {
        let offset = copy_offset(*copy);
        let binary = area
            .get(..BINARY_HEADER_LEN)
            .ok_or("LUKS2 copy is shorter than its binary header")?;
        let mut unsummed = binary.to_vec();
        unsummed
            .get_mut(CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_FIELD_LEN)
            .ok_or("LUKS2 checksum field overflows its copy")?
            .fill(0);
        medium
            .write_at(offset, &unsummed)
            .map_err(io("header write"))?;
        medium
            .write_at(offset + BINARY_HEADER_LEN as u64, &json)
            .map_err(io("header JSON write"))?;
        medium
            .write_at(offset, binary)
            .map_err(io("header write"))?;
        medium.sync().map_err(io("header sync"))?;
    }
    Ok(())
}

fn salt(random: &mut dyn Random) -> Result<[u8; SALT_LEN], Error> {
    let mut salt = [0u8; SALT_LEN];
    random.fill(&mut salt).map_err(io("salt"))?;
    Ok(salt)
}

fn keyslot_json(offset: u64, salt: &[u8]) -> Json {
    ObjectBuilder::new()
        .set("type", "luks2")
        .set("key_size", KEY_LEN as u64)
        .set(
            "af",
            ObjectBuilder::new()
                .set("type", "luks1")
                .set("stripes", keyslot::STRIPES as u64)
                .set("hash", "sha256")
                .build(),
        )
        .set(
            "area",
            ObjectBuilder::new()
                .set("type", "raw")
                .set("offset", offset.to_string())
                .set("size", AREA_LEN.to_string())
                .set("encryption", CIPHER)
                .set("key_size", KEY_LEN as u64)
                .build(),
        )
        .set(
            "kdf",
            ObjectBuilder::new()
                .set("type", "pbkdf2")
                .set("hash", "sha256")
                .set("iterations", u64::from(ITERATIONS))
                .set("salt", base64(salt))
                .build(),
        )
        .build()
}

/// A new keyslot for `volume_key` under `passphrase`: its JSON and its
/// sealed split key.
fn new_keyslot(
    random: &mut dyn Random,
    volume_key: &Key,
    passphrase: &[u8],
    offset: u64,
) -> Result<(Json, Vec<u8>), Error> {
    let salt = salt(random)?;
    let area_key = keyslot::area_key(passphrase, &salt, ITERATIONS)?;
    let mut split = keyslot::split(volume_key, random).map_err(io("keyslot split"))?;
    if let Err(e) = keyslot::seal(&area_key, &mut split) {
        keyslot::clear(&mut split);
        return Err(e.into());
    }
    Ok((keyslot_json(offset, &salt), split))
}

fn section_mut<'a>(json: &'a mut Json, section: &str) -> Result<&'a mut Json, Error> {
    json.get_mut(section)
        .ok_or_else(|| Error::Refused(format!("LUKS2 JSON has no {section} object")))
}

/// The digest naming `slot`, if any.
fn digest_of(json: &Json, slot: u8) -> Result<Option<(u8, Json)>, Error> {
    for (number, digest) in numbered(json, "digests")? {
        if slots(digest, "keyslots", "digest")?.contains(&slot) {
            return Ok(Some((number, digest.clone())));
        }
    }
    Ok(None)
}

/// A keyslot whose digest names a segment: one that opens the volume.
fn bound(json: &Json, slot: u8) -> Result<bool, Error> {
    Ok(match digest_of(json, slot)? {
        Some((_, digest)) => !slots(&digest, "segments", "digest")?.is_empty(),
        None => return refused(format!("LUKS2 keyslot {slot} has no digest")),
    })
}

fn check_key(key: &[u8]) -> Result<(), Error> {
    if key.is_empty() || key.len() > MAX_KEY {
        return refused(format!("a key is 1 to {MAX_KEY} bytes"));
    }
    Ok(())
}

/// The volume key `slot` gives for `passphrase`, checked against its
/// digest.
fn unlock<M: Medium + ?Sized>(
    medium: &mut M,
    json: &Json,
    slot: u8,
    passphrase: &[u8],
) -> Result<Key, Error> {
    check_key(passphrase)?;
    let keyslot = numbered(json, "keyslots")?
        .into_iter()
        .find(|(number, _)| *number == slot)
        .map(|(_, keyslot)| keyslot.clone())
        .ok_or_else(|| Error::Refused(format!("LUKS2 keyslot {slot} is not in use")))?;
    let Some((_, digest)) = digest_of(json, slot)? else {
        return refused(format!("LUKS2 keyslot {slot} has no digest"));
    };
    let kdf = keyslot.get("kdf").unwrap_or(&Json::Null);
    let salt = bytes_of(kdf, "salt", SALT_LEN, "keyslot KDF")?;
    let count = iterations(kdf, MAX_KEYSLOT_ITERATIONS, "keyslot KDF")?;
    let area = keyslot.get("area").unwrap_or(&Json::Null);
    let offset = decimal(area, "offset", "keyslot area")?;
    let area_key = keyslot::area_key(passphrase, &salt, count)?;
    let mut split = vec![0u8; SPLIT_LEN];
    read_exact_at(medium, offset, &mut split).map_err(io("keyslot area read"))?;
    // Whatever the outcome, the decrypted split key is cleared.
    let volume_key = keyslot::open(&area_key, &mut split).and_then(|()| keyslot::merge(&split));
    keyslot::clear(&mut split);
    let volume_key = volume_key?;
    let digest_salt = bytes_of(&digest, "salt", SALT_LEN, "digest")?;
    let stored = bytes_of(&digest, "digest", keyslot::DIGEST_LEN, "digest")?;
    let count = iterations(&digest, MAX_DIGEST_ITERATIONS, "digest")?;
    let computed = keyslot::digest(&volume_key, &digest_salt, count)?;
    if td_fido::same(&computed, &stored) {
        Ok(volume_key)
    } else {
        Err(Error::WrongKey)
    }
}

/// `unlock` of a keyslot bound to the volume's segment, as cryptsetup's
/// `luksAddKey` opens its authorizing keyslot (`LUKS2_keyslot_open` for
/// `CRYPT_DEFAULT_SEGMENT`). An unbound one is refused, as cryptsetup's
/// -ENOENT is a failure (exit 1, "No usable keyslot is available"), not
/// a wrong key.
fn unlock_bound<M: Medium + ?Sized>(
    medium: &mut M,
    json: &Json,
    slot: u8,
    passphrase: &[u8],
) -> Result<Key, Error> {
    if slot_in_use(json, "keyslots", slot)? && !bound(json, slot)? {
        return refused(format!(
            "LUKS2 keyslot {slot} is not bound to the volume's segment"
        ));
    }
    unlock(medium, json, slot, passphrase)
}

/// The bound keyslots an open with no slot tries: priority 2 first, then
/// priority 1, never priority 0, each in keyslot-number order. cryptsetup
/// goes in JSON order within a priority, so only which keyslot opens can
/// differ, never whether one does.
fn candidates(json: &Json) -> Result<Vec<u8>, Error> {
    let mut preferred = Vec::new();
    let mut normal = Vec::new();
    for (number, keyslot) in numbered(json, "keyslots")? {
        if !bound(json, number)? {
            continue;
        }
        match keyslot.get("priority").and_then(Json::as_u64) {
            Some(0) => {}
            Some(2) => preferred.push(number),
            _ => normal.push(number),
        }
    }
    preferred.extend(normal);
    Ok(preferred)
}

/// Every bound keyslot but `except`, whatever its priority: those a kill
/// tries by number, as cryptsetup's `verify_keyslot` does.
fn bound_slots(json: &Json, except: Option<u8>) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    for (number, _) in numbered(json, "keyslots")? {
        if Some(number) != except && bound(json, number)? {
            out.push(number);
        }
    }
    Ok(out)
}

/// The first of `slots` that `passphrase` opens.
fn unlock_any<M: Medium + ?Sized>(
    medium: &mut M,
    json: &Json,
    passphrase: &[u8],
    slots: &[u8],
) -> Result<(u8, Key), Error> {
    check_key(passphrase)?;
    for &slot in slots {
        match unlock(medium, json, slot, passphrase) {
            Ok(key) => return Ok((slot, key)),
            Err(Error::WrongKey) => {}
            Err(other) => return Err(other),
        }
    }
    Err(Error::WrongKey)
}

/// The lowest 4096-aligned span of the keyslots area that holds a new
/// area: cryptsetup's first fit.
fn free_area(json: &Json) -> Result<u64, Error> {
    let mut used = Vec::new();
    for (_, keyslot) in numbered(json, "keyslots")? {
        let area = keyslot.get("area").unwrap_or(&Json::Null);
        used.push(decimal(area, "offset", "keyslot area")?);
    }
    used.sort_unstable();
    let mut candidate = KEYSLOTS_OFFSET;
    for start in used {
        if candidate.saturating_add(AREA_LEN) <= start {
            break;
        }
        candidate = candidate.max(start.saturating_add(AREA_LEN));
    }
    if candidate.saturating_add(AREA_LEN) > DATA_OFFSET {
        return refused("the LUKS2 keyslots area has no room for another keyslot");
    }
    Ok(candidate)
}

fn slot_in_use(json: &Json, section: &str, slot: u8) -> Result<bool, Error> {
    Ok(numbered(json, section)?
        .iter()
        .any(|(number, _)| *number == slot))
}

/// Remove `slot` from every digest's and token's `keyslots`.
fn strip(json: &mut Json, slot: u8) -> Result<(), Error> {
    let name = slot.to_string();
    for section in ["digests", "tokens"] {
        if let Some(Json::Obj(entries)) = json.get_mut(section) {
            for (_, entry) in entries.iter_mut() {
                if let Some(Json::Arr(list)) = entry.get_mut("keyslots") {
                    list.retain(|item| item.as_str() != Some(name.as_str()));
                }
            }
        } else {
            return refused(format!("LUKS2 JSON has no {section} object"));
        }
    }
    Ok(())
}

/// A keyslot area overwritten with random bytes before its keyslot goes.
fn wipe(random: &mut dyn Random, json: &Json, slot: u8) -> Result<(u64, Vec<u8>), Error> {
    let keyslot = numbered(json, "keyslots")?
        .into_iter()
        .find(|(number, _)| *number == slot)
        .map(|(_, keyslot)| keyslot)
        .ok_or_else(|| Error::Refused(format!("LUKS2 keyslot {slot} is not in use")))?;
    let area = keyslot.get("area").unwrap_or(&Json::Null);
    let offset = decimal(area, "offset", "keyslot area")?;
    let mut bytes = vec![0u8; AREA_LEN as usize];
    random.fill(&mut bytes).map_err(io("wipe bytes"))?;
    Ok((offset, bytes))
}

fn next(header: &Header) -> Result<Plan, Error> {
    Ok(Plan {
        uuid: header.uuid.clone(),
        label: header.label.clone(),
        seqid: header
            .seqid
            .checked_add(1)
            .ok_or("LUKS2 sequence number overflows")?,
        json: header.json.clone(),
        areas: Vec::new(),
        used: header.used,
    })
}

/// A UUID as cryptsetup writes it: lowercase 8-4-4-4-12 hex.
fn uuid_ok(uuid: &str) -> bool {
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, byte)| match i {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

/// `format`'s plan: zeroed header areas, a random keyslots area, then a
/// header with keyslot 0 under `passphrase` (DESIGN.md "Format and
/// kill").
pub fn plan_format<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    uuid: &str,
    label: &str,
    passphrase: &[u8],
) -> Result<Plan, Error> {
    check_key(passphrase)?;
    if !uuid_ok(uuid) {
        return refused(format!("{uuid:?} is not a lowercase LUKS2 UUID"));
    }
    if label.len() >= LABEL_LEN || label.contains('\0') {
        return refused("a LUKS2 label is at most 47 bytes with no NUL");
    }
    let size = medium.size().map_err(io("device size"))?;
    if size <= DATA_OFFSET || !(size - DATA_OFFSET).is_multiple_of(SECTOR_SIZE) {
        return refused(format!(
            "a device of {size} bytes holds no whole 4096-byte sector past {DATA_OFFSET}"
        ));
    }
    let volume_key = Key::random(random).map_err(io("volume key"))?;
    let (keyslot, sealed) = new_keyslot(random, &volume_key, passphrase, KEYSLOTS_OFFSET)?;
    let digest_salt = salt(random)?;
    let digest = keyslot::digest(&volume_key, &digest_salt, ITERATIONS)?;
    let json = ObjectBuilder::new()
        .set("keyslots", ObjectBuilder::new().set("0", keyslot).build())
        .set("tokens", ObjectBuilder::new().build())
        .set(
            "segments",
            ObjectBuilder::new()
                .set(
                    "0",
                    ObjectBuilder::new()
                        .set("type", "crypt")
                        .set("offset", DATA_OFFSET.to_string())
                        .set("size", "dynamic")
                        .set("iv_tweak", "0")
                        .set("encryption", CIPHER)
                        .set("sector_size", SECTOR_SIZE)
                        .build(),
                )
                .build(),
        )
        .set(
            "digests",
            ObjectBuilder::new()
                .set(
                    "0",
                    ObjectBuilder::new()
                        .set("type", "pbkdf2")
                        .set("keyslots", Json::Arr(vec![Json::from("0")]))
                        .set("segments", Json::Arr(vec![Json::from("0")]))
                        .set("hash", "sha256")
                        .set("iterations", u64::from(ITERATIONS))
                        .set("salt", base64(&digest_salt))
                        .set("digest", base64(&digest))
                        .build(),
                )
                .build(),
        )
        .set(
            "config",
            ObjectBuilder::new()
                .set("json_size", JSON_SIZE.to_string())
                .set("keyslots_size", KEYSLOTS_SIZE.to_string())
                .build(),
        )
        .build();
    let mut keyslots_area = vec![0u8; KEYSLOTS_SIZE as usize];
    random
        .fill(&mut keyslots_area)
        .map_err(io("keyslots area fill"))?;
    Ok(Plan {
        uuid: uuid.to_owned(),
        label: label.to_owned(),
        seqid: 1,
        json,
        // Nothing is valid once the header areas are zeroed.
        used: HeaderCopy::Primary,
        areas: vec![
            (0, vec![0u8; KEYSLOTS_OFFSET as usize]),
            (KEYSLOTS_OFFSET, keyslots_area),
            (KEYSLOTS_OFFSET, sealed),
        ],
    })
}

/// Format `medium` as td's profile, keyslot 0 under `passphrase`.
pub fn format<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    uuid: &str,
    label: &str,
    passphrase: &[u8],
) -> Result<(), Error> {
    let plan = plan_format(medium, random, uuid, label, passphrase)?;
    commit(medium, random, &plan)
}

/// `add_key`'s plan: keyslot `new` under `new_key`, authorized by
/// keyslot `by` opening with `key`.
pub fn plan_add_key<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    by: u8,
    new: u8,
    key: &[u8],
    new_key: &[u8],
) -> Result<Plan, Error> {
    check_key(new_key)?;
    let header = load(medium)?;
    if new > MAX_SLOT || slot_in_use(&header.json, "keyslots", new)? {
        return refused(format!("LUKS2 keyslot {new} is in use or out of range"));
    }
    let volume_key = unlock_bound(medium, &header.json, by, key)?;
    let Some((digest, _)) = digest_of(&header.json, by)? else {
        return refused(format!("LUKS2 keyslot {by} has no digest"));
    };
    let offset = free_area(&header.json)?;
    let (keyslot, sealed) = new_keyslot(random, &volume_key, new_key, offset)?;
    let mut plan = next(&header)?;
    section_mut(&mut plan.json, "keyslots")?.insert(new.to_string(), keyslot);
    if let Some(Json::Arr(list)) = section_mut(&mut plan.json, "digests")?
        .get_mut(&digest.to_string())
        .and_then(|digest| digest.get_mut("keyslots"))
    {
        list.push(Json::from(new.to_string()));
    } else {
        return refused(format!("LUKS2 digest {digest} has no keyslots array"));
    }
    plan.areas.push((offset, sealed));
    Ok(plan)
}

pub fn add_key<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    by: u8,
    new: u8,
    key: &[u8],
    new_key: &[u8],
) -> Result<(), Error> {
    let plan = plan_add_key(medium, random, by, new, key, new_key)?;
    commit(medium, random, &plan)
}

/// `kill_slot`'s plan: `key` must open an active keyslot other than
/// `slot`, or `slot` itself when it is the last; its area is wiped
/// before the commit that removes it from keyslots, digests and tokens.
pub fn plan_kill_slot<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    slot: u8,
    key: &[u8],
) -> Result<Plan, Error> {
    if key.len() < MIN_KILL_KEY {
        return refused(format!(
            "a kill takes a key of at least {MIN_KILL_KEY} bytes"
        ));
    }
    let header = load(medium)?;
    if !slot_in_use(&header.json, "keyslots", slot)? {
        return refused(format!("LUKS2 keyslot {slot} is not in use"));
    }
    let others = bound_slots(&header.json, Some(slot))?;
    if others.is_empty() {
        unlock(medium, &header.json, slot, key)?;
    } else {
        unlock_any(medium, &header.json, key, &others)?;
    }
    let mut plan = next(&header)?;
    plan.areas.push(wipe(random, &header.json, slot)?);
    section_mut(&mut plan.json, "keyslots")?.remove(&slot.to_string());
    strip(&mut plan.json, slot)?;
    Ok(plan)
}

pub fn kill_slot<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    slot: u8,
    key: &[u8],
) -> Result<(), Error> {
    let plan = plan_kill_slot(medium, random, slot, key)?;
    commit(medium, random, &plan)
}

/// Keyslots whose digest names no segment. A missing digest is refused.
pub fn unbound(json: &Json) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    for (number, _) in numbered(json, "keyslots")? {
        if !bound(json, number)? {
            out.push(number);
        }
    }
    Ok(out)
}

/// `kill_unbound`'s plan, or `None` when no keyslot is unbound: `key`
/// must open a bound keyslot; each unbound area is wiped before the
/// commit that removes them all.
pub fn plan_kill_unbound<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    key: &[u8],
) -> Result<Option<Plan>, Error> {
    let header = load(medium)?;
    unlock_any(medium, &header.json, key, &bound_slots(&header.json, None)?)?;
    let targets = unbound(&header.json)?;
    if targets.is_empty() {
        return Ok(None);
    }
    let mut plan = next(&header)?;
    for slot in targets {
        plan.areas.push(wipe(random, &header.json, slot)?);
        section_mut(&mut plan.json, "keyslots")?.remove(&slot.to_string());
        strip(&mut plan.json, slot)?;
    }
    Ok(Some(plan))
}

/// Remove every unbound keyslot; the number removed.
pub fn kill_unbound<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    key: &[u8],
) -> Result<usize, Error> {
    match plan_kill_unbound(medium, random, key)? {
        None => Ok(0),
        Some(plan) => {
            let removed = plan.areas.len();
            commit(medium, random, &plan)?;
            Ok(removed)
        }
    }
}

/// A token as a LUKS2 header carries it: one object with a type and its
/// keyslots, within `MAX_TOKEN_JSON`.
fn token(text: &[u8]) -> Result<Json, Error> {
    if text.len() > MAX_TOKEN_JSON {
        return refused(format!("token JSON is over {MAX_TOKEN_JSON} bytes"));
    }
    let token = td_json::parse_slice(text).map_err(|e| format!("token JSON: {e}"))?;
    if token.as_obj().is_none() {
        return refused("token JSON is not one object");
    }
    token
        .get("type")
        .and_then(Json::as_str)
        .ok_or("token JSON has no type")?;
    slots(&token, "keyslots", "token")?;
    Ok(token)
}

/// What a token operation does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenChange {
    /// Import at the number given, which must be free, or the lowest free.
    Import(Option<u8>),
    /// Set the number given, replacing any token there.
    Replace(u8),
    /// Remove the number given, which must be in use.
    Remove(u8),
}

/// A token change's plan, and the token number it touched.
pub fn plan_token<M: Medium + ?Sized>(
    medium: &mut M,
    change: TokenChange,
    text: &[u8],
) -> Result<(Plan, u8), Error> {
    let header = load(medium)?;
    let mut plan = next(&header)?;
    let used = |slot| slot_in_use(&header.json, "tokens", slot);
    let number = match change {
        TokenChange::Import(Some(id)) | TokenChange::Replace(id) | TokenChange::Remove(id)
            if id > MAX_SLOT =>
        {
            return refused(format!("token {id} is out of range"));
        }
        TokenChange::Import(Some(id)) => {
            if used(id)? {
                return refused(format!("LUKS2 token {id} is in use"));
            }
            id
        }
        TokenChange::Import(None) => {
            let mut free = None;
            for id in 0..=MAX_SLOT {
                if !used(id)? {
                    free = Some(id);
                    break;
                }
            }
            free.ok_or("every LUKS2 token number is in use")?
        }
        TokenChange::Replace(id) => id,
        TokenChange::Remove(id) => {
            if !used(id)? {
                return refused(format!("LUKS2 token {id} is not in use"));
            }
            id
        }
    };
    let tokens = section_mut(&mut plan.json, "tokens")?;
    if let TokenChange::Remove(_) = change {
        tokens.remove(&number.to_string());
    } else {
        tokens.insert(number.to_string(), token(text)?);
    }
    Ok((plan, number))
}

pub fn token_change<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    change: TokenChange,
    text: &[u8],
) -> Result<u8, Error> {
    let (plan, number) = plan_token(medium, change, text)?;
    commit(medium, random, &plan)?;
    Ok(number)
}

/// The header's JSON as one compact object.
pub fn metadata<M: Medium + ?Sized>(medium: &mut M) -> Result<String, Error> {
    Ok(load(medium)?.json.to_string())
}

/// The volume key `key` gives through `slot`, or through any candidate
/// keyslot.
pub fn volume_key<M: Medium + ?Sized>(
    medium: &mut M,
    slot: Option<u8>,
    key: &[u8],
) -> Result<Key, Error> {
    let header = load(medium)?;
    match slot {
        Some(slot) => unlock(medium, &header.json, slot, key),
        None => {
            unlock_any(medium, &header.json, key, &candidates(&header.json)?).map(|(_, key)| key)
        }
    }
}

/// Whether `key` opens `slot`. The medium is only read, so a detached
/// header file is tested as it is. An unbound keyslot is tested against
/// its own digest, as cryptsetup 2.8.8's `open --test-passphrase
/// --key-slot` tests it (0 for its key, 2 for another).
pub fn test<M: Medium + ?Sized>(medium: &mut M, slot: u8, key: &[u8]) -> Result<(), Error> {
    volume_key(medium, Some(slot), key).map(drop)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::keyslot::tests::Counter;

    /// A medium in memory that fails every write after its budget, as a
    /// crash would leave it.
    #[derive(Clone)]
    pub(crate) struct Memory {
        pub bytes: Vec<u8>,
        pub budget: Option<usize>,
        pub writes: usize,
    }

    impl Memory {
        pub fn new(size: usize) -> Self {
            Self {
                bytes: vec![0; size],
                budget: None,
                writes: 0,
            }
        }

        fn spend(&mut self) -> io::Result<()> {
            if self.budget.is_some_and(|budget| self.writes >= budget) {
                return Err(io::Error::other("crashed"));
            }
            self.writes += 1;
            Ok(())
        }
    }

    impl Medium for Memory {
        fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
            let start = (offset as usize).min(self.bytes.len());
            let end = start.saturating_add(out.len()).min(self.bytes.len());
            out[..end - start].copy_from_slice(&self.bytes[start..end]);
            Ok(end - start)
        }

        fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
            self.spend()?;
            let start = offset as usize;
            self.bytes[start..start + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            self.spend()
        }

        fn size(&mut self) -> io::Result<u64> {
            Ok(self.bytes.len() as u64)
        }
    }

    pub(crate) const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
    pub(crate) const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
    pub(crate) const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";
    pub(crate) const THIRD: &[u8] = b"zyxwvutsrqponmlkjihgfedcbaZYXWVU";
    const SIZE: usize = DATA_OFFSET as usize + 2 * 4096;

    /// The next commit of `header` with `json` in place of its own.
    fn rewrite(header: &Header, json: Json) -> Plan {
        let mut plan = next(header).unwrap();
        plan.json = json;
        plan
    }

    pub(crate) fn formatted(random: &mut Counter) -> Memory {
        let mut medium = Memory::new(SIZE);
        format(&mut medium, random, UUID, "td-system", RECOVERY).unwrap();
        medium
    }

    /// Every write of `plan` failed in turn: what the reader then chooses
    /// is the old header or the new one, never neither, once a header
    /// existed before.
    fn crash_everywhere(before: &Memory, plan: &Plan, old: Option<&Json>) -> usize {
        let mut finished = before.clone();
        commit(&mut finished, &mut Counter(900), plan).unwrap();
        let steps = finished.writes - before.writes;
        for budget in 0..steps {
            let mut crashed = before.clone();
            crashed.budget = Some(before.writes + budget);
            assert!(commit(&mut crashed, &mut Counter(900), plan).is_err());
            let loaded = load(&mut crashed);
            // A new header is never chosen before every area it names.
            if loaded.as_ref().is_ok_and(|header| header.json == plan.json) {
                let mut expected = before.clone();
                for (offset, bytes) in &plan.areas {
                    expected.write_at(*offset, bytes).unwrap();
                }
                let end = KEYSLOTS_OFFSET as usize..DATA_OFFSET as usize;
                assert!(
                    crashed.bytes[end.clone()] == expected.bytes[end],
                    "{budget}"
                );
            }
            match (loaded, old) {
                (Ok(header), Some(old)) => {
                    assert!(header.json == *old || header.json == plan.json, "{budget}")
                }
                (Ok(header), None) => assert_eq!(header.json, plan.json, "{budget}"),
                (Err(e), None) => assert!(
                    e.to_string().contains("no valid LUKS2 header copy"),
                    "{budget}: {e}"
                ),
                (Err(e), Some(_)) => panic!("after {budget} writes: {e}"),
            }
        }
        let header = load(&mut finished).unwrap();
        assert_eq!(header.json, plan.json);
        assert_eq!(header.seqid, plan.seqid);
        steps
    }

    #[test]
    fn a_format_opens_with_its_passphrase_and_survives_every_crash() {
        let mut random = Counter(1);
        let mut medium = Memory::new(SIZE);
        let plan = plan_format(&mut medium, &mut random, UUID, "td-system", RECOVERY).unwrap();
        // The zeroing, the fill and the keyslot, one sync, two copies.
        assert_eq!(crash_everywhere(&medium, &plan, None), 3 + 1 + 2 * 4);
        commit(&mut medium, &mut random, &plan).unwrap();
        let header = load(&mut medium).unwrap();
        assert_eq!(
            (header.uuid.as_str(), header.label.as_str()),
            (UUID, "td-system")
        );
        assert_eq!(header.seqid, 1);
        test(&mut medium, 0, RECOVERY).unwrap();
        assert_eq!(test(&mut medium, 0, SECOND), Err(Error::WrongKey));
        assert_eq!(
            test(&mut medium, 1, RECOVERY),
            Err(Error::Refused("LUKS2 keyslot 1 is not in use".into()))
        );
        // The header area past both copies' JSON is zero; the keyslots
        // area past keyslot 0's split key is the random fill.
        assert!(medium.bytes[0x4000 + BINARY_HEADER_LEN + 2000..0x8000]
            .iter()
            .all(|b| *b == 0));
        let tail = &medium.bytes[KEYSLOTS_OFFSET as usize + SPLIT_LEN..DATA_OFFSET as usize];
        assert!(tail.iter().any(|b| *b != 0));
    }

    #[test]
    fn format_refuses_what_is_not_td_s() {
        let mut random = Counter(1);
        let mut medium = Memory::new(SIZE);
        for (uuid, label, size) in [
            ("5A5A5A5A-5A5A-405A-805A-5A5A5A5A5A5A", "td-system", SIZE),
            (UUID, &"x".repeat(48) as &str, SIZE),
            (UUID, "td-system", DATA_OFFSET as usize),
            (UUID, "td-system", SIZE + 512),
        ] {
            medium.bytes.resize(size, 0);
            assert!(format(&mut medium, &mut random, uuid, label, RECOVERY).is_err());
        }
        assert_eq!(medium.writes, 0, "nothing written");
    }

    #[test]
    fn keys_are_added_and_killed_as_cryptsetup_authorizes_them() {
        let mut random = Counter(7);
        let mut medium = formatted(&mut random);
        let before = load(&mut medium).unwrap();
        // A wrong authorizing key adds nothing.
        assert_eq!(
            add_key(&mut medium, &mut random, 0, 1, SECOND, SECOND),
            Err(Error::WrongKey)
        );
        let plan = plan_add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
        assert_eq!(plan.areas.first().map(|(at, _)| *at), Some(290_816));
        crash_everywhere(&medium, &plan, Some(&before.json));
        commit(&mut medium, &mut random, &plan).unwrap();
        test(&mut medium, 1, SECOND).unwrap();
        assert_eq!(
            add_key(&mut medium, &mut random, 0, 1, RECOVERY, THIRD),
            Err(Error::Refused(
                "LUKS2 keyslot 1 is in use or out of range".into()
            ))
        );
        // A key shorter than a protector's is refused before anything.
        assert!(kill_slot(&mut medium, &mut random, 1, b"short").is_err());
        // Keyslot 1's own key cannot kill it while keyslot 0 remains.
        assert_eq!(
            kill_slot(&mut medium, &mut random, 1, SECOND),
            Err(Error::WrongKey)
        );
        let with_two = load(&mut medium).unwrap();
        let plan = plan_kill_slot(&mut medium, &mut random, 1, RECOVERY).unwrap();
        crash_everywhere(&medium, &plan, Some(&with_two.json));
        commit(&mut medium, &mut random, &plan).unwrap();
        let after = load(&mut medium).unwrap();
        assert_eq!(after.json, before.json);
        // The killed area is wiped, so the old header no longer opens it.
        let mut stale = medium.clone();
        let mut replan = rewrite(&with_two, with_two.json.clone());
        replan.seqid += 10;
        commit(&mut stale, &mut random, &replan).unwrap();
        assert_eq!(test(&mut stale, 1, SECOND), Err(Error::WrongKey));
        // The last keyslot dies by its own key.
        kill_slot(&mut medium, &mut random, 0, RECOVERY).unwrap();
        assert!(numbered(&load(&mut medium).unwrap().json, "keyslots")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn tokens_are_imported_replaced_and_removed_and_follow_killed_keyslots() {
        let mut random = Counter(11);
        let mut medium = formatted(&mut random);
        add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
        let token = br#"{"type":"td-protector","keyslots":["1"],"role":"first-boot"}"#;
        let before = load(&mut medium).unwrap();
        let (plan, number) = plan_token(&mut medium, TokenChange::Import(None), token).unwrap();
        assert_eq!(number, 0);
        crash_everywhere(&medium, &plan, Some(&before.json));
        commit(&mut medium, &mut random, &plan).unwrap();
        assert!(token_change(
            &mut medium,
            &mut random,
            TokenChange::Import(Some(0)),
            token
        )
        .is_err());
        assert_eq!(
            token_change(&mut medium, &mut random, TokenChange::Import(None), token),
            Ok(1)
        );
        let replaced = br#"{"type":"td-protector","keyslots":["0"],"role":"device-bound"}"#;
        token_change(&mut medium, &mut random, TokenChange::Replace(1), replaced).unwrap();
        for bad in [
            &br#"{"type":"x","keyslots":["7"]}"#[..],
            br#"{"keyslots":[]}"#,
            br#"[1]"#,
            &[b' '; MAX_TOKEN_JSON + 1],
        ] {
            assert!(
                token_change(&mut medium, &mut random, TokenChange::Import(None), bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        kill_slot(&mut medium, &mut random, 1, RECOVERY).unwrap();
        let json = load(&mut medium).unwrap().json;
        assert_eq!(
            json.get_path(&["tokens", "0", "keyslots"]),
            Some(&Json::Arr(vec![]))
        );
        assert_eq!(
            json.get_path(&["tokens", "1", "role"])
                .and_then(Json::as_str),
            Some("device-bound")
        );
        token_change(&mut medium, &mut random, TokenChange::Remove(0), b"").unwrap();
        assert!(token_change(&mut medium, &mut random, TokenChange::Remove(0), b"").is_err());
        assert!(load(&mut medium)
            .unwrap()
            .json
            .get_path(&["tokens", "0"])
            .is_none());
    }

    #[test]
    fn unbound_keyslots_go_by_a_bound_key() {
        let mut random = Counter(13);
        let mut medium = formatted(&mut random);
        add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
        assert_eq!(kill_unbound(&mut medium, &mut random, RECOVERY), Ok(0));
        // Retain the old digest with no segment during end cleanup.
        let header = load(&mut medium).unwrap();
        let mut json = header.json.clone();
        strip(&mut json, 1).unwrap();
        let mut digest = header.json.get_path(&["digests", "0"]).unwrap().clone();
        digest.insert("keyslots", parse(r#"["1"]"#));
        digest.insert("segments", parse("[]"));
        at(&mut json, &["digests"]).insert("1", digest);
        let plan = rewrite(&header, json);
        commit(&mut medium, &mut random, &plan).unwrap();
        assert_eq!(unbound(&load(&mut medium).unwrap().json).unwrap(), [1]);
        // Its own key opens nothing bound.
        assert_eq!(
            kill_unbound(&mut medium, &mut random, SECOND),
            Err(Error::WrongKey)
        );
        let plan = plan_kill_unbound(&mut medium, &mut random, RECOVERY)
            .unwrap()
            .unwrap();
        let old = load(&mut medium).unwrap().json;
        crash_everywhere(&medium, &plan, Some(&old));
        assert_eq!(kill_unbound(&mut medium, &mut random, RECOVERY), Ok(1));
        assert!(unbound(&load(&mut medium).unwrap().json)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_open_with_no_slot_skips_priority_zero_and_prefers_two() {
        let mut random = Counter(17);
        let mut medium = formatted(&mut random);
        add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
        add_key(&mut medium, &mut random, 0, 2, RECOVERY, THIRD).unwrap();
        let mut header = load(&mut medium).unwrap();
        for (slot, priority) in [("1", 0u64), ("2", 2)] {
            header
                .json
                .get_mut("keyslots")
                .and_then(|k| k.get_mut(slot))
                .unwrap()
                .insert("priority", priority);
        }
        assert_eq!(candidates(&header.json).unwrap(), [2, 0]);
        let plan = rewrite(&header, header.json.clone());
        commit(&mut medium, &mut random, &plan).unwrap();
        assert_eq!(
            volume_key(&mut medium, None, SECOND).unwrap_err(),
            Error::WrongKey
        );
        test(&mut medium, 1, SECOND).unwrap();
        let by_any = volume_key(&mut medium, None, THIRD).unwrap();
        let by_zero = volume_key(&mut medium, Some(0), RECOVERY).unwrap();
        assert_eq!(by_any.bytes(), by_zero.bytes());
        // A kill tries every other bound keyslot by number, as
        // cryptsetup's verify_keyslot does, priority 0 included.
        assert_eq!(kill_unbound(&mut medium, &mut random, SECOND), Ok(0));
        kill_slot(&mut medium, &mut random, 2, SECOND).unwrap();
        assert!(load(&mut medium)
            .unwrap()
            .json
            .get_path(&["keyslots", "2"])
            .is_none());
    }

    fn parse(text: &str) -> Json {
        td_json::parse_slice(text.as_bytes()).unwrap()
    }

    fn at<'a>(json: &'a mut Json, path: &[&str]) -> &'a mut Json {
        path.iter()
            .fold(json, |json, key| json.get_mut(key).unwrap())
    }

    /// Recompute the checksum of the copy at `offset`, as a writer that
    /// meant its bytes would.
    fn reseal(medium: &mut Memory, offset: usize) {
        let area = &mut medium.bytes[offset..offset + FORMATTED_HEADER_SIZE as usize];
        area[CHECKSUM_AT..CHECKSUM_AT + CHECKSUM_FIELD_LEN].fill(0);
        let checksum = td_fido::digest(area);
        area[CHECKSUM_AT..CHECKSUM_AT + SHA256_LEN].copy_from_slice(&checksum);
    }

    #[test]
    fn a_commit_from_one_valid_copy_keeps_one_through_every_crash() {
        // A copy torn by an earlier crash, or damaged: the commit writes
        // the other copy first, so the one the reader uses stays valid.
        for (damaged, used) in [
            (FORMATTED_HEADER_SIZE, HeaderCopy::Primary),
            (0, HeaderCopy::Secondary),
        ] {
            let mut random = Counter(19);
            let mut medium = formatted(&mut random);
            medium.bytes[(damaged + 8000) as usize] ^= 1;
            let before = load(&mut medium).unwrap();
            assert_eq!(before.used, used);
            let plan = plan_add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
            assert_eq!(plan.used, used);
            crash_everywhere(&medium, &plan, Some(&before.json));
            let token = br#"{"type":"td-protector","keyslots":["0"]}"#;
            let (plan, _) = plan_token(&mut medium, TokenChange::Import(None), token).unwrap();
            crash_everywhere(&medium, &plan, Some(&before.json));
            let plan = plan_kill_slot(&mut medium, &mut random, 0, RECOVERY).unwrap();
            crash_everywhere(&medium, &plan, Some(&before.json));
            // A finished commit leaves both copies valid again.
            commit(&mut medium, &mut random, &plan).unwrap();
            for offset in [0, FORMATTED_HEADER_SIZE] {
                let mut other = medium.clone();
                other.bytes[(offset + 8000) as usize] ^= 1;
                assert_eq!(load(&mut other).unwrap().json, plan.json);
            }
        }
    }

    #[test]
    fn an_unbound_keyslot_tests_but_authorizes_no_add() {
        let mut random = Counter(23);
        let mut medium = formatted(&mut random);
        add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
        // Keyslot 1 moved to a digest of its own that names no segment,
        // as cryptsetup's `luksAddKey --unbound` leaves one.
        let header = load(&mut medium).unwrap();
        let mut json = header.json.clone();
        strip(&mut json, 1).unwrap();
        let mut digest = json.get_path(&["digests", "0"]).unwrap().clone();
        digest.insert("keyslots", parse(r#"["1"]"#));
        digest.insert("segments", parse("[]"));
        at(&mut json, &["digests"]).insert("1", digest);
        commit(&mut medium, &mut random, &rewrite(&header, json)).unwrap();
        assert_eq!(unbound(&load(&mut medium).unwrap().json).unwrap(), [1]);
        // cryptsetup 2.8.8's `open --test-passphrase --key-slot 1`: 0 for
        // its key, 2 for another.
        test(&mut medium, 1, SECOND).unwrap();
        assert_eq!(test(&mut medium, 1, RECOVERY), Err(Error::WrongKey));
        // Its `luksAddKey --key-slot 1`: "No usable keyslot", exit 1.
        let writes = medium.writes;
        assert_eq!(
            add_key(&mut medium, &mut random, 1, 2, SECOND, THIRD),
            Err(Error::Refused(
                "LUKS2 keyslot 1 is not bound to the volume's segment".into()
            ))
        );
        assert_eq!(medium.writes, writes);
        // No open with no slot tries it, and its key is the same volume
        // key here only because this test reused it.
        assert_eq!(
            volume_key(&mut medium, None, SECOND).unwrap_err(),
            Error::WrongKey
        );
        let unbound_key = volume_key(&mut medium, Some(1), SECOND).unwrap();
        let bound_key = volume_key(&mut medium, Some(0), RECOVERY).unwrap();
        assert_eq!(unbound_key.bytes(), bound_key.bytes());
        assert_eq!(
            kill_unbound(&mut medium, &mut random, SECOND),
            Err(Error::WrongKey)
        );
        assert_eq!(kill_unbound(&mut medium, &mut random, RECOVERY), Ok(1));
        let json = load(&mut medium).unwrap().json;
        assert!(json.get_path(&["keyslots", "1"]).is_none());
        // The emptied digest stays, as cryptsetup leaves it.
        assert_eq!(
            json.get_path(&["digests", "1", "keyslots"]),
            Some(&parse("[]"))
        );
    }

    /// Each case takes a header td wrote outside td's profile, installed
    /// past the profile check, and every operation refuses it before
    /// writing anything, as `commit` refuses to write it.
    #[test]
    fn every_operation_refuses_a_header_outside_the_profile() {
        let mut random = Counter(29);
        let mut base = formatted(&mut random);
        add_key(&mut base, &mut random, 0, 1, RECOVERY, SECOND).unwrap();
        let token = br#"{"type":"td-protector","keyslots":["1"]}"#;
        token_change(&mut base, &mut random, TokenChange::Import(None), token).unwrap();
        let header = load(&mut base).unwrap();
        type Mutation = fn(&mut Json);
        let cases: &[(&str, Mutation)] = &[
            (
                "the mandatory requirement \"online-reencrypt-v2\"",
                |json| {
                    at(json, &["config"]).insert(
                        "requirements",
                        parse(r#"{"mandatory":["online-reencrypt-v2"]}"#),
                    );
                },
            ),
            ("config carries \"flags\"", |json| {
                at(json, &["config"]).insert("flags", parse(r#"["allow-discards"]"#));
            }),
            ("keyslot 0 KDF carries \"time\"", |json| {
                let salt = json
                    .get_path(&["keyslots", "0", "kdf", "salt"])
                    .unwrap()
                    .clone();
                let mut kdf = parse(r#"{"type":"argon2id","time":4,"memory":1048576,"cpus":4}"#);
                kdf.insert("salt", salt);
                at(json, &["keyslots", "0"]).insert("kdf", kdf);
            }),
            ("keyslot 0 KDF's iterations are not 1 to 100000", |json| {
                at(json, &["keyslots", "0", "kdf"]).insert("iterations", 100_001u64);
            }),
            ("keyslot 0 area's encryption is not", |json| {
                at(json, &["keyslots", "0", "area"]).insert("encryption", "aes-cbc-essiv:sha256");
            }),
            ("keyslot 0's key_size is not 64", |json| {
                at(json, &["keyslots", "0"]).insert("key_size", 32u64);
            }),
            ("keyslot 0's priority is not 0 to 2", |json| {
                at(json, &["keyslots", "0"]).insert("priority", 3u64);
            }),
            (
                "keyslot 1 area at 290817 is outside the keyslots area",
                |json| {
                    at(json, &["keyslots", "1", "area"]).insert("offset", "290817");
                },
            ),
            (
                "keyslot 1 area at 16773120 is outside the keyslots area",
                |json| {
                    at(json, &["keyslots", "1", "area"]).insert("offset", "16773120");
                },
            ),
            ("keyslot areas overlap", |json| {
                at(json, &["keyslots", "1", "area"]).insert("offset", "36864");
            }),
            ("keyslot 1 has no digest", |json| {
                at(json, &["digests", "0"]).insert("keyslots", parse(r#"["0"]"#));
            }),
            ("digest 0 names a segment other than 0", |json| {
                at(json, &["digests", "0"]).insert("segments", parse(r#"["1"]"#));
            }),
            ("digest 0 names keyslot 5, which is absent", |json| {
                at(json, &["digests", "0"]).insert("keyslots", parse(r#"["0","1","5"]"#));
            }),
            (
                "digest 1 names keyslot 1, which is absent or digested twice",
                |json| {
                    let mut digest = json.get_path(&["digests", "0"]).unwrap().clone();
                    digest.insert("keyslots", parse(r#"["1"]"#));
                    digest.insert("segments", parse("[]"));
                    at(json, &["digests"]).insert("1", digest);
                },
            ),
            ("digests 0 and 1 both name segment 0", |json| {
                let mut digest = json.get_path(&["digests", "0"]).unwrap().clone();
                digest.insert("keyslots", parse("[]"));
                at(json, &["digests"]).insert("1", digest);
            }),
            ("token 0 names absent keyslot 7", |json| {
                at(json, &["tokens", "0"]).insert("keyslots", parse(r#"["7"]"#));
            }),
            ("segment 0's offset is not \"16777216\"", |json| {
                at(json, &["segments", "0"]).insert("offset", "32768");
            }),
            ("segment 0's sector_size is not 4096", |json| {
                at(json, &["segments", "0"]).insert("sector_size", 512u64);
            }),
            ("segment 0's encryption is not", |json| {
                at(json, &["segments", "0"]).insert("encryption", "aes-cbc-essiv:sha256");
            }),
        ];
        for (expected, mutate) in cases {
            let mut json = header.json.clone();
            mutate(&mut json);
            let plan = rewrite(&header, json);
            let mut medium = base.clone();
            let refused = |result: Result<(), Error>| match result {
                Err(Error::Refused(reason)) => {
                    assert!(reason.contains(expected), "{expected}: {reason}")
                }
                other => panic!("{expected}: {other:?}"),
            };
            refused(commit(&mut medium, &mut random, &plan));
            assert_eq!(medium.writes, base.writes, "{expected}");
            write_plan(&mut medium, &mut random, &plan).unwrap();
            let writes = medium.writes;
            refused(load(&mut medium).map(drop));
            refused(add_key(&mut medium, &mut random, 0, 2, RECOVERY, THIRD));
            refused(kill_slot(&mut medium, &mut random, 1, RECOVERY));
            refused(kill_unbound(&mut medium, &mut random, RECOVERY).map(drop));
            refused(
                token_change(&mut medium, &mut random, TokenChange::Import(None), token).map(drop),
            );
            refused(metadata(&mut medium).map(drop));
            refused(volume_key(&mut medium, None, RECOVERY).map(drop));
            refused(test(&mut medium, 0, RECOVERY));
            assert_eq!(medium.writes, writes, "{expected}: nothing written");
        }
    }

    #[test]
    fn a_binary_field_a_commit_would_change_is_refused() {
        let mut random = Counter(31);
        let base = formatted(&mut random);
        for (expected, at, bytes) in [
            ("label", LABEL_AT, &b"td-\xffsystem"[..]),
            ("label", LABEL_AT + 20, b"x"),
            ("label", LABEL_AT, &[b'x'; LABEL_LEN]),
            ("UUID", UUID_AT + 37, b"x"),
            ("subsystem", SUBSYSTEM_AT, b"td"),
        ] {
            let mut medium = base.clone();
            for offset in [0, FORMATTED_HEADER_SIZE as usize] {
                medium.bytes[offset + at..offset + at + bytes.len()].copy_from_slice(bytes);
                reseal(&mut medium, offset);
            }
            let writes = medium.writes;
            assert_eq!(
                load(&mut medium).unwrap_err(),
                Error::Refused(format!(
                    "LUKS2 header's {expected} field is not one td writes back unchanged"
                ))
            );
            assert!(add_key(&mut medium, &mut random, 0, 1, RECOVERY, SECOND).is_err());
            assert_eq!(medium.writes, writes);
        }
    }
}
