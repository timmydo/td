//! td's LUKS2 tokens over td-luks's bounded header reader (DESIGN.md
//! "LUKS2 tokens"). td-luks selects the copy cryptsetup would use and
//! parses it as generic LUKS2; this module reads the used copy's tokens
//! under td's token rules. It runs before any C parser has seen the header.

use crate::protected::{self, REQUEST_TYPE, UPGRADE_TYPE};
use crate::token::{slot_number, Role, Token, TOKEN_TYPE};
use std::io::{Read, Seek};
use td_json::Json;
use td_luks::header::{self as luks, numbered, UsedCopy};
pub use td_luks::header::{
    binary_claim, identity, segment_digest, BinaryClaim, Cost, Costs, HeaderCopy, Identity, Volume,
    BINARY_HEADER_LEN, FORMATTED_HEADER_SIZE, HEADER_SIZES, JSON_AREA_LEN, MAX_DIGEST_ITERATIONS,
    MAX_KEYSLOTS_SIZE, MAX_KEYSLOT_ITERATIONS, MAX_METADATA_JSON,
};

/// The most td tokens a header may carry, orphans included: a first-boot
/// and a device-bound protector, one superseded and one an interrupted
/// transition left.
pub const MAX_TD_TOKENS: usize = 4;
/// The protected tier's bounds per token class (DESIGN.md "Protected
/// roles"): tpm-pin, fido2, `recovery-key`, `lockout`, `td-upgrade` and
/// `td-request` tokens, orphans and pending tokens included. Beside a
/// `td-upgrade` token at most one first-boot or device-bound token
/// remains.
pub const MAX_TPM_PIN_TOKENS: usize = 2;
pub const MAX_FIDO2_TOKENS: usize = 4;
pub const MAX_UPGRADE_DEVICE_BOUND_TOKENS: usize = 1;

/// What a header's td tokens make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// First-boot or device-bound tokens, or no td token but keyslot-less
    /// fido2 ones: a pending fido2 token without a `td-upgrade` token is
    /// an orphan.
    DeviceBound,
    /// tpm-pin, fido2 tokens naming a keyslot, the `recovery-key` marker,
    /// the `lockout` or a `td-request` token, and no device-bound one.
    Protected,
    /// A `td-upgrade` token, beside either kind.
    Upgrading,
}

/// The kind the protected-tier tokens of a header give it.
fn kind_of(protected: &[(u8, protected::Token)]) -> Kind {
    if protected
        .iter()
        .any(|(_, token)| matches!(token, protected::Token::Upgrade { .. }))
    {
        Kind::Upgrading
    } else if protected
        .iter()
        .any(|(_, token)| !matches!(token, protected::Token::Fido2(fido) if fido.keyslot.is_none()))
    {
        Kind::Protected
    } else {
        Kind::DeviceBound
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
    /// Each token of another type with its token number and the keyslots
    /// it names, ascending: the transition leaves them alone.
    pub foreign: Vec<(u8, Vec<u8>)>,
    /// Each protected-tier token of td's types with its token number,
    /// ascending: the device-bound transition leaves them alone, as it
    /// leaves foreign tokens.
    pub protected: Vec<(u8, protected::Token)>,
}

impl Header {
    /// The keyslots tokens of other types and protected-tier tokens name,
    /// ascending: every keyslot the device-bound transition leaves alone.
    pub fn foreign_keyslots(&self) -> Vec<u8> {
        let mut slots: Vec<u8> = self
            .foreign
            .iter()
            .flat_map(|(_, slots)| slots.iter().copied())
            .chain(
                self.protected
                    .iter()
                    .filter_map(|(_, token)| token.keyslot()),
            )
            .collect();
        slots.sort_unstable();
        slots.dedup();
        slots
    }

    /// Every token number in use, of any type, ascending.
    pub fn token_numbers(&self) -> Vec<u8> {
        let mut numbers: Vec<u8> = self
            .tokens
            .iter()
            .map(|(number, _)| *number)
            .chain(self.orphans.iter().map(|(number, _)| *number))
            .chain(self.foreign.iter().map(|(number, _)| *number))
            .chain(self.protected.iter().map(|(number, _)| *number))
            .collect();
        numbers.sort_unstable();
        numbers
    }

    /// Whether the header is device-bound, protected or upgrading.
    pub fn kind(&self) -> Kind {
        kind_of(&self.protected)
    }

    /// The keyslots and tokens of every type, to compare with cryptsetup's
    /// own metadata (`metadata`).
    pub fn metadata(&self) -> Metadata {
        Metadata {
            keyslots: self.keyslots.clone(),
            tokens: self.tokens.clone(),
            orphans: self.orphans.clone(),
            foreign: self.foreign.clone(),
            protected: self.protected.clone(),
        }
    }
}

/// A header's keyslots and tokens of every type, as `Header` carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub keyslots: Vec<u8>,
    pub tokens: Vec<(u8, Token)>,
    pub orphans: Vec<(u8, Role)>,
    pub foreign: Vec<(u8, Vec<u8>)>,
    pub protected: Vec<(u8, protected::Token)>,
}

/// The used copy's td view, from its parsed JSON area.
fn header_from(copy: &UsedCopy, json: &Json) -> Result<Header, String> {
    let Metadata {
        keyslots,
        tokens,
        orphans,
        foreign,
        protected,
    } = contents(json)?;
    let identity = copy.identity.clone();
    Ok(Header {
        copy: identity.copy,
        seqid: identity.seqid,
        hdr_size: identity.hdr_size,
        uuid: identity.uuid,
        label: identity.label,
        keyslots,
        tokens,
        orphans,
        foreign,
        protected,
    })
}

/// The keyslots and tokens of a parsed JSON header, under td's token
/// rules.
fn contents(json: &Json) -> Result<Metadata, String> {
    let keyslots: Vec<u8> = numbered(json, "keyslots")?
        .into_iter()
        .map(|(number, _)| number)
        .collect();
    let mut tokens = Vec::new();
    let mut orphans = Vec::new();
    let mut foreign = Vec::new();
    let mut protected = Vec::new();
    for (number, value) in numbered(json, "tokens")? {
        let kind = value
            .get("type")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("LUKS2 token {number} has no type"))?;
        let named = value
            .get("keyslots")
            .and_then(Json::as_arr)
            .ok_or_else(|| format!("LUKS2 token {number} has no keyslots array"))?;
        let mut slots = Vec::with_capacity(named.len());
        for keyslot in named {
            let slot = keyslot
                .as_str()
                .and_then(slot_number)
                .filter(|slot| keyslots.contains(slot))
                .ok_or_else(|| {
                    format!("LUKS2 token {number} names a keyslot the header does not have")
                })?;
            slots.push(slot);
        }
        let protected_role = value
            .get("role")
            .and_then(Json::as_str)
            .is_some_and(protected::is_protected_role);
        if kind == UPGRADE_TYPE || kind == REQUEST_TYPE || (kind == TOKEN_TYPE && protected_role) {
            let token = protected::Token::from_json(value)
                .map_err(|e| format!("LUKS2 token {number}: {e}"))?;
            protected.push((number, token));
            continue;
        }
        if kind != TOKEN_TYPE {
            slots.sort_unstable();
            foreign.push((number, slots));
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
    protected_bounds(tokens.len() + orphans.len(), &protected)?;
    Ok(Metadata {
        keyslots,
        tokens,
        orphans,
        foreign,
        protected,
    })
}

/// The protected tier's per-class bounds and the kinds a header may mix:
/// device-bound and protected td tokens only beside a `td-upgrade` token,
/// and then at most one device-bound one.
fn protected_bounds(
    device_bound: usize,
    protected: &[(u8, protected::Token)],
) -> Result<(), String> {
    use protected::Token as T;
    let count = |class: &dyn Fn(&T) -> bool| protected.iter().filter(|(_, t)| class(t)).count();
    let bounds = [
        (
            "tpm-pin",
            count(&|t| matches!(t, T::TpmPin(_))),
            MAX_TPM_PIN_TOKENS,
        ),
        (
            "fido2",
            count(&|t| matches!(t, T::Fido2(_))),
            MAX_FIDO2_TOKENS,
        ),
        (
            "recovery-key",
            count(&|t| matches!(t, T::RecoveryKey { .. })),
            1,
        ),
        ("lockout", count(&|t| matches!(t, T::Lockout(_))), 1),
        (
            "update-retry",
            count(&|t| matches!(t, T::UpdateRetry(_))),
            1,
        ),
        ("td-upgrade", count(&|t| matches!(t, T::Upgrade { .. })), 1),
        ("td-request", count(&|t| matches!(t, T::Request(_))), 1),
    ];
    for (class, count, most) in bounds {
        if count > most {
            return Err(format!(
                "LUKS2 header carries more than {most} {class} tokens"
            ));
        }
    }
    match kind_of(protected) {
        Kind::Protected if device_bound > 0 => Err(
            "LUKS2 header mixes device-bound and protected td tokens without a td-upgrade token"
                .into(),
        ),
        Kind::Upgrading if device_bound > MAX_UPGRADE_DEVICE_BOUND_TOKENS => Err(format!(
            "LUKS2 header carries more than {MAX_UPGRADE_DEVICE_BOUND_TOKENS} device-bound td token beside a td-upgrade token"
        )),
        _ => Ok(()),
    }
}

/// The keyslots and tokens of the header cryptsetup itself uses, from
/// `luksDump --dump-json-metadata`'s output: at most `MAX_METADATA_JSON`
/// bytes of one JSON object, read under the same token rules as `read`.
pub fn metadata(text: &[u8]) -> Result<Metadata, String> {
    contents(&luks::metadata_json(text)?)
}

/// Read the LUKS2 header at the start of `device` as cryptsetup would
/// choose its copy, and return the used copy's td tokens. A used copy
/// that fails td's own checks refuses the header rather than falling back.
pub fn read<R: Read + Seek>(device: &mut R) -> Result<Header, String> {
    let copy = luks::read_copy(device)?;
    let (json, _) = copy.json()?;
    header_from(&copy, &json)
}

/// `read`, and the upgrade's view of the same copy with the length of its
/// JSON text, which the upgrade's planner grows only within the area.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeView {
    pub header: Header,
    pub volume: Volume,
    pub json_len: usize,
}

pub fn read_upgrade<R: Read + Seek>(device: &mut R) -> Result<UpgradeView, String> {
    let copy = luks::read_copy(device)?;
    let (json, json_len) = copy.json()?;
    let header = header_from(&copy, &json)?;
    Ok(UpgradeView {
        header,
        volume: luks::volume(&json)?,
        json_len,
    })
}

/// `metadata`, and the upgrade's view of the header cryptsetup uses, so
/// that an upgrade plan's confirmation compares both.
pub fn upgrade_metadata(text: &[u8]) -> Result<(Metadata, Volume), String> {
    let json = luks::metadata_json(text)?;
    let metadata = contents(&json)?;
    Ok((metadata, luks::volume(&json)?))
}

/// What a keyslot test reads of the used copy beyond `read`: the bytes a
/// detached copy of the header must hold, both copies and the keyslots
/// area, and each keyslot's cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestArea {
    pub header: Header,
    /// Two copies of `hdr_size` and `config.keyslots_size`.
    pub length: u64,
    costs: Costs,
}

impl TestArea {
    /// The cost of testing `slot`, or why td does not bound it: a keyslot
    /// or digest that is not PBKDF2-SHA256, such as Argon2, whose memory
    /// and time td does not run, or one over its ceiling.
    pub fn cost(&self, slot: u8) -> Result<Cost, String> {
        match self.costs.iter().find(|(number, _)| *number == slot) {
            Some((_, cost)) => cost.clone(),
            None => Err(format!("LUKS2 header has no keyslot {slot}")),
        }
    }

    /// Whether every keyslot has a cost, or why the first that has none
    /// does not: a volume-key dump needs it, since cryptsetup 2.8.8's
    /// `luksDump --dump-volume-key` tries every keyslot (`CRYPT_ANY_SLOT`),
    /// not the tested one. A header without keyslots holds, and its proof
    /// has no keyslot to test.
    pub fn all_costed(&self) -> Result<(), String> {
        self.costs
            .iter()
            .try_for_each(|(_, cost)| cost.as_ref().map(|_| ()).map_err(Clone::clone))
    }
}

/// `read`, and what a keyslot test against a detached copy of the header
/// needs: the area's length, `config.keyslots_size` bounded by
/// `MAX_KEYSLOTS_SIZE`, and each keyslot's cost.
pub fn test_area<R: Read + Seek>(device: &mut R) -> Result<TestArea, String> {
    let copy = luks::read_copy(device)?;
    let (json, _) = copy.json()?;
    let header = header_from(&copy, &json)?;
    let length = luks::test_length(&copy, &json)?;
    let costs = luks::costs(&json)?;
    Ok(TestArea {
        header,
        length,
        costs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Role;
    use std::io::{Cursor, SeekFrom};
    use td_luks::header::{
        CHECKSUM_AT, CHECKSUM_FIELD_LEN, PRIMARY_MAGIC, SECONDARY_MAGIC, SHA256_LEN,
    };
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
        // The systemd token is another type: only its number and keyslots
        // are reported.
        assert_eq!(header.tokens.len(), 1);
        assert_eq!(header.foreign, [(1, vec![0])]);
        assert_eq!(header.foreign_keyslots(), [0]);
        assert_eq!(header.token_numbers(), [0, 1]);
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

    /// A header JSON with keyslots 0 to 5 and these numbered tokens.
    fn six_slots(tokens: &[(u8, String)]) -> String {
        let tokens: Vec<String> = tokens
            .iter()
            .map(|(number, token)| format!(r#""{number}":{token}"#))
            .collect();
        let slots: Vec<String> = (0..6).map(|n| format!(r#""{n}":{{}}"#)).collect();
        format!(
            r#"{{"keyslots":{{{}}},"tokens":{{{}}},"segments":{{}},"digests":{{}},"config":{{}}}}"#,
            slots.join(","),
            tokens.join(",")
        )
    }

    fn read_json(json: &str) -> Result<Header, String> {
        read_bytes(device(7, json, 7, json))
    }

    fn encoded(token: &protected::Token) -> String {
        token.encode().unwrap()
    }

    #[test]
    fn protected_headers_are_read_and_classified() {
        use crate::protected::fixtures::*;
        use crate::protected::FidoRole::{Primary, Recovery};
        let systemd = r#"{"type":"systemd-tpm2","keyslots":["5"]}"#.to_owned();
        let tokens = [
            (0, encoded(&tpm_pin(Some(2)))),
            (1, encoded(&fido2(Some(3), Primary))),
            (2, encoded(&fido2(Some(4), Recovery))),
            (
                3,
                encoded(&protected::Token::RecoveryKey { keyslot: Some(0) }),
            ),
            (4, encoded(&lockout())),
            (5, encoded(&request())),
            (6, systemd),
            // Two tpm-pin tokens on one keyslot: an update's pre-seal.
            (7, encoded(&tpm_pin(Some(2)))),
        ];
        let json = six_slots(&tokens);
        let header = read_json(&json).unwrap();
        assert_eq!(header.kind(), Kind::Protected);
        assert!(header.tokens.is_empty() && header.orphans.is_empty());
        assert_eq!(header.foreign, [(6, vec![5])]);
        let numbers: Vec<u8> = header.protected.iter().map(|(n, _)| *n).collect();
        assert_eq!(numbers, [0, 1, 2, 3, 4, 5, 7]);
        assert_eq!(header.protected[1].1, fido2(Some(3), Primary));
        assert_eq!(header.protected[4].1, lockout());
        assert_eq!(header.foreign_keyslots(), [0, 2, 3, 4, 5]);
        assert_eq!(header.token_numbers(), [0, 1, 2, 3, 4, 5, 6, 7]);
        // cryptsetup's metadata dump is read under the same rules.
        assert_eq!(metadata(json.as_bytes()).unwrap(), header.metadata());
        // Pending fido2 tokens and the upgrade's token beside one
        // device-bound protector: U0.
        let mut u0: Vec<(u8, String)> = (0..4)
            .map(|n| (n, encoded(&fido2(None, Recovery))))
            .collect();
        u0.push((4, td_token(1, Role::DeviceBound)));
        u0.push((9, encoded(&upgrade())));
        let header = read_json(&six_slots(&u0)).unwrap();
        assert_eq!(header.kind(), Kind::Upgrading);
        assert_eq!((header.tokens.len(), header.protected.len()), (1, 5));
        // Every protected kind beside the upgrade's token: U4 and U5.
        let mut u5 = tokens.to_vec();
        u5.retain(|(n, _)| *n != 5);
        u5.push((9, encoded(&upgrade())));
        assert_eq!(read_json(&six_slots(&u5)).unwrap().kind(), Kind::Upgrading);
        // Pending fido2 tokens without the upgrade's token are orphans of
        // a device-bound header: an interrupted phase 1.
        u0.retain(|(n, _)| *n != 9);
        let header = read_json(&six_slots(&u0)).unwrap();
        assert_eq!(header.kind(), Kind::DeviceBound);
        assert_eq!(header.protected.len(), 4);
        // No td token at all is device-bound, as before.
        assert_eq!(
            read_json(&six_slots(&[])).unwrap().kind(),
            Kind::DeviceBound
        );
    }

    #[test]
    fn protected_headers_are_bounded() {
        use crate::protected::fixtures::*;
        use crate::protected::FidoRole::Primary;
        let refused = |tokens: &[(u8, String)]| read_json(&six_slots(tokens)).unwrap_err();
        let many = |token: &protected::Token, count: u8| -> Vec<(u8, String)> {
            (0..count).map(|n| (n, encoded(token))).collect()
        };
        let marker = protected::Token::RecoveryKey { keyslot: Some(0) };
        let cases: &[(Vec<(u8, String)>, &str)] = &[
            (many(&tpm_pin(Some(2)), 3), "more than 2 tpm-pin tokens"),
            (many(&fido2(None, Primary), 5), "more than 4 fido2 tokens"),
            (many(&marker, 2), "more than 1 recovery-key tokens"),
            (many(&lockout(), 2), "more than 1 lockout tokens"),
            (many(&upgrade(), 2), "more than 1 td-upgrade tokens"),
            (many(&request(), 2), "more than 1 td-request tokens"),
            (many(&update_retry(), 2), "more than 1 update-retry tokens"),
        ];
        for (tokens, expected) in cases {
            let error = refused(tokens);
            assert!(error.contains(expected), "{error}");
        }
        // At the bounds every class is admitted together.
        let mut most = many(&fido2(Some(3), Primary), 4);
        most.extend([
            (4, encoded(&tpm_pin(Some(2)))),
            (5, encoded(&tpm_pin(Some(1)))),
            (6, encoded(&marker)),
            (7, encoded(&lockout())),
            (8, encoded(&upgrade())),
            (9, encoded(&request())),
            (10, td_token(5, Role::DeviceBound)),
            (11, encoded(&update_retry())),
        ]);
        assert_eq!(read_json(&six_slots(&most)).unwrap().protected.len(), 11);
        // Device-bound and protected tokens mix only beside the upgrade's.
        let mixture = "mixes device-bound and protected td tokens";
        for token in [
            tpm_pin(Some(2)),
            fido2(Some(3), Primary),
            marker,
            lockout(),
            request(),
            update_retry(),
        ] {
            let tokens = [(0, td_token(1, Role::DeviceBound)), (1, encoded(&token))];
            assert!(refused(&tokens).contains(mixture), "{token:?}");
        }
        let orphan = td_token(1, Role::FirstBoot).replacen(r#"["1"]"#, "[]", 1);
        assert!(refused(&[(0, orphan.clone()), (1, encoded(&lockout()))]).contains(mixture));
        // Beside it, one device-bound token at most, an orphan included.
        let two = [
            (0, td_token(1, Role::DeviceBound)),
            (1, orphan),
            (2, encoded(&upgrade())),
        ];
        assert!(refused(&two).contains("more than 1 device-bound td token beside a td-upgrade"));
        // A protected token that is not its format refuses the header, as
        // does a role td does not know yet (8i's).
        let bad = encoded(&lockout()).replacen(r#""keyslots":[]"#, r#""keyslots":["1"]"#, 1);
        assert_eq!(
            refused(&[(3, bad)]),
            "LUKS2 token 3: td token of this kind names no keyslot"
        );
        let bad = encoded(&upgrade()).replacen("tpm", "both", 1);
        assert!(refused(&[(3, bad)]).starts_with("LUKS2 token 3: td token shape"));
        let config = r#"{"type":"td-protector","keyslots":[],"role":"config","tags":[]}"#;
        assert!(refused(&[(3, config.into())]).starts_with("LUKS2 token 3: td token"));
        let later = r#"{"type":"td-protector","keyslots":[],"role":"secure-boot","public":"00","private":"00"}"#;
        assert_eq!(
            refused(&[(3, later.into())]),
            "LUKS2 token 3: td token role is unknown"
        );
        let bad = encoded(&update_retry()).replacen(r#""pair""#, r#""public""#, 1);
        assert_eq!(
            refused(&[(3, bad)]),
            "LUKS2 token 3: td token has an unknown key \"public\""
        );
        let absent = encoded(&tpm_pin(Some(9)));
        assert!(refused(&[(3, absent)]).contains("names a keyslot the header does not have"));
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

    #[test]
    fn identity_chooses_the_copy_as_read_does_and_parses_no_json() {
        let json = standard();
        let identity_of = |bytes: Vec<u8>| identity(&mut Cursor::new(bytes));
        let found = identity_of(device(7, &json, 8, &json)).unwrap();
        assert_eq!(
            found,
            Identity {
                copy: HeaderCopy::Secondary,
                seqid: 8,
                hdr_size: 0x4000,
                uuid: "0f0e0d0c-0b0a-4908-8706-050403020100".into(),
                label: "td-system".into(),
            }
        );
        // JSON and tokens `read` refuses are not identity's concern.
        for area in [
            "{}".to_string(),
            header_json(&format!(
                r#""0":{},"1":{},"2":{},"3":{},"4":{}"#,
                td_token(1, Role::FirstBoot),
                td_token(1, Role::DeviceBound),
                td_token(1, Role::DeviceBound),
                td_token(1, Role::DeviceBound),
                td_token(1, Role::DeviceBound)
            )),
        ] {
            let bytes = device(7, &area, 7, &area);
            assert!(read_bytes(bytes.clone()).is_err());
            assert_eq!(identity_of(bytes).unwrap().copy, HeaderCopy::Primary);
        }
        // A size td does not format is a valid copy for identity alone.
        let mut large = copy(false, 0, 0x8000, 7, json.as_bytes());
        large.extend(copy(true, 0x8000, 0x8000, 7, json.as_bytes()));
        assert!(read_bytes(large.clone()).is_err());
        assert_eq!(identity_of(large).unwrap().hdr_size, 0x8000);
        // What choosing a copy refuses, identity refuses.
        let mut corrupt = device(7, &json, 7, &json);
        corrupt[1000] ^= 1;
        corrupt[0x4000 + 1000] ^= 1;
        assert_eq!(
            identity_of(corrupt).unwrap_err(),
            "no valid LUKS2 header copy"
        );
        let other = header_json("");
        assert!(identity_of(device(7, &json, 7, &other))
            .unwrap_err()
            .contains("disagree"));
    }

    fn tested_json(keyslot_kdf: &str, digest: &str, keyslots_size: &str) -> String {
        format!(
            r#"{{"keyslots":{{"0":{{"type":"luks2","kdf":{keyslot_kdf}}},"1":{{"type":"luks2","kdf":{{"type":"pbkdf2","hash":"sha256","iterations":1000}}}}}},"tokens":{{}},"segments":{{"0":{{"type":"crypt"}}}},"digests":{{"0":{digest}}},"config":{{"json_size":"12288","keyslots_size":"{keyslots_size}"}}}}"#
        )
    }

    const TD_KDF: &str = r#"{"type":"pbkdf2","hash":"sha256","iterations":1000,"salt":"AA=="}"#;
    const DIGEST: &str = r#"{"type":"pbkdf2","keyslots":["0","1"],"segments":["0"],"hash":"sha256","iterations":250000}"#;

    fn area(json: &str) -> Result<TestArea, String> {
        test_area(&mut Cursor::new(device(5, json, 5, json)))
    }

    #[test]
    fn a_test_area_is_both_copies_and_the_keyslots_area_with_bounded_costs() {
        let json = tested_json(TD_KDF, DIGEST, "16744448");
        let area = area(&json).unwrap();
        assert_eq!(area.header, read_bytes(device(5, &json, 5, &json)).unwrap());
        assert_eq!(area.length, 2 * 0x4000 + 16744448);
        let td = Cost {
            keyslot: 1000,
            digest: 250000,
        };
        assert_eq!(area.cost(0), Ok(td));
        assert_eq!(area.cost(1), Ok(td));
        assert!(area.cost(2).is_err());
        assert_eq!(area.all_costed(), Ok(()));
        // cryptsetup's largest keyslots area, and one byte past it.
        let largest = MAX_KEYSLOTS_SIZE.to_string();
        assert!(super::tests::area(&tested_json(TD_KDF, DIGEST, &largest)).is_ok());
        let past = (MAX_KEYSLOTS_SIZE + 1).to_string();
        for size in [past.as_str(), "-1", "", "0x1000"] {
            assert!(super::tests::area(&tested_json(TD_KDF, DIGEST, size)).is_err());
        }
    }

    #[test]
    fn a_keyslot_or_digest_td_does_not_bound_has_no_cost() {
        for (kdf, digest) in [
            // Argon2, whose memory and time td never runs.
            (
                r#"{"type":"argon2id","time":4,"memory":1048576,"cpus":4}"#,
                DIGEST,
            ),
            (
                r#"{"type":"pbkdf2","hash":"sha512","iterations":1000}"#,
                DIGEST,
            ),
            (
                r#"{"type":"pbkdf2","hash":"sha256","iterations":100001}"#,
                DIGEST,
            ),
            (
                r#"{"type":"pbkdf2","hash":"sha256","iterations":0}"#,
                DIGEST,
            ),
            (r#"{"type":"pbkdf2","hash":"sha256"}"#, DIGEST),
            (
                TD_KDF,
                r#"{"type":"pbkdf2","keyslots":["0","1"],"hash":"sha256","iterations":4000001}"#,
            ),
            // No digest covers the keyslot, or two do.
            (
                TD_KDF,
                r#"{"type":"pbkdf2","keyslots":["1"],"hash":"sha256","iterations":1000}"#,
            ),
        ] {
            let area = area(&tested_json(kdf, digest, "16744448")).unwrap();
            assert!(area.cost(0).is_err(), "{kdf} {digest}");
            // Keyslot 1 alone is testable, but a dump tries keyslot 0 too.
            if digest == DIGEST {
                assert!(area.cost(1).is_ok(), "{kdf} {digest}");
            }
            assert_eq!(area.all_costed(), area.cost(0).map(|_| ()));
        }
        let two = format!(
            r#"{{"keyslots":{{"0":{{"type":"luks2","kdf":{TD_KDF}}}}},"tokens":{{}},"segments":{{}},"digests":{{"0":{DIGEST},"1":{DIGEST}}},"config":{{"keyslots_size":"4096"}}}}"#
        );
        assert!(area(&two).unwrap().cost(0).is_err());
        // Keyslot 0 costed and keyslot 1 not: a dump still tries keyslot 1.
        let later = tested_json(TD_KDF, DIGEST, "4096").replacen(
            r#""iterations":1000}}}"#,
            r#""iterations":100001}}}"#,
            1,
        );
        let later = area(&later).unwrap();
        assert!(later.cost(0).is_ok() && later.cost(1).is_err());
        assert_eq!(later.all_costed(), later.cost(1).map(|_| ()));
        // At the ceilings exactly, the cost is admitted.
        let ceiling = tested_json(
            r#"{"type":"pbkdf2","hash":"sha256","iterations":100000}"#,
            r#"{"type":"pbkdf2","keyslots":["0","1"],"hash":"sha256","iterations":4000000}"#,
            "4096",
        );
        assert_eq!(
            area(&ceiling).unwrap().cost(0),
            Ok(Cost {
                keyslot: MAX_KEYSLOT_ITERATIONS,
                digest: MAX_DIGEST_ITERATIONS
            })
        );
    }

    /// cryptsetup 2.8.8's own metadata, compacted: a U1 header (keyslot 0
    /// and the `td-upgrade` token), the same header after `reencrypt
    /// --init-only` with journal resilience (U2), and a header after a
    /// whole reencryption (U3), which rewrapped keyslot 0's key into
    /// keyslot 1.
    const U1_DUMP: &str = r#"{"keyslots":{"0":{"type":"luks2","key_size":64,"af":{"type":"luks1","stripes":4000,"hash":"sha256"},"area":{"type":"raw","offset":"32768","size":"258048","encryption":"aes-xts-plain64","key_size":64},"kdf":{"type":"pbkdf2","hash":"sha256","iterations":1000,"salt":"JryVNwtodNCKirHhnn23zMszf5s/gwfh34C720cNwq8="}}},"tokens":{"3":{"type":"td-upgrade","keyslots":[],"shape":"tpm","from":"7777777777777777777777777777777777777777777777777777777777777777"}},"segments":{"0":{"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},"digests":{"0":{"type":"pbkdf2","keyslots":["0"],"segments":["0"],"hash":"sha256","iterations":1000,"salt":"71shKqe2GoUjdvWJ5LwpIUK9u0s5ag1V7LuGEnVRK6s=","digest":"Qufgdz095/Yz8tjTATvRwkixYFtr6GVq8GZfx14emAY="}},"config":{"json_size":"12288","keyslots_size":"16744448"}}"#;
    const U2_DUMP: &str = r#"{"keyslots":{"0":{"type":"luks2","key_size":64,"af":{"type":"luks1","stripes":4000,"hash":"sha256"},"area":{"type":"raw","offset":"32768","size":"258048","encryption":"aes-xts-plain64","key_size":64},"kdf":{"type":"pbkdf2","hash":"sha256","iterations":1000,"salt":"JryVNwtodNCKirHhnn23zMszf5s/gwfh34C720cNwq8="}},"1":{"type":"luks2","key_size":64,"af":{"type":"luks1","stripes":4000,"hash":"sha256"},"area":{"type":"raw","offset":"290816","size":"258048","encryption":"aes-xts-plain64","key_size":64},"kdf":{"type":"pbkdf2","hash":"sha256","iterations":1000,"salt":"wQvAYaQc0EZdrdz3uMJNtLDbntRyU6i6SGzRbYmL8fM="}},"2":{"area":{"offset":"548864","size":"16228352","type":"journal"},"type":"reencrypt","key_size":1,"mode":"reencrypt","direction":"forward","priority":0}},"tokens":{"3":{"type":"td-upgrade","keyslots":[],"shape":"tpm","from":"7777777777777777777777777777777777777777777777777777777777777777"}},"segments":{"0":{"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096},"1":{"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096,"flags":["backup-previous"]},"2":{"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096,"flags":["backup-final"]}},"digests":{"0":{"type":"pbkdf2","keyslots":["0"],"segments":["0","1"],"hash":"sha256","iterations":1000,"salt":"71shKqe2GoUjdvWJ5LwpIUK9u0s5ag1V7LuGEnVRK6s=","digest":"Qufgdz095/Yz8tjTATvRwkixYFtr6GVq8GZfx14emAY="},"1":{"type":"pbkdf2","keyslots":["1"],"segments":["2"],"hash":"sha256","iterations":1000,"salt":"6EgPXWrS1HYZOBrpOMR6s/XA4xaeX7V1Y4oelsRI+p4=","digest":"ZgWMTUHdvQtCZ8BOff5hFv2FRu5Xe0YCsmzeFZfeo7o="},"2":{"type":"pbkdf2","keyslots":["2"],"segments":[],"hash":"sha256","iterations":1000,"salt":"nNbPV3EqWQ9QWrNtRsv9tHbQ7jvMLtBGIKBDwkrYV+c=","digest":"etahzNZdEpir4XuOCGXZhGvs4VrAYHyHXRxkM1shB1g="}},"config":{"json_size":"12288","keyslots_size":"16744448","requirements":{"mandatory":["online-reencrypt-v2"]}}}"#;
    const U3_DUMP: &str = r#"{"keyslots":{"1":{"type":"luks2","key_size":64,"af":{"type":"luks1","stripes":4000,"hash":"sha256"},"area":{"type":"raw","offset":"290816","size":"258048","encryption":"aes-xts-plain64","key_size":64},"kdf":{"type":"pbkdf2","hash":"sha256","iterations":1000,"salt":"HSQ8JL2FgkQxfE/Qa/M7W4uGURZAVdud9IpkLBFIyaE="}}},"tokens":{},"segments":{"0":{"type":"crypt","offset":"16777216","size":"dynamic","iv_tweak":"0","encryption":"aes-xts-plain64","sector_size":4096}},"digests":{"1":{"type":"pbkdf2","keyslots":["1"],"segments":["0"],"hash":"sha256","iterations":1000,"salt":"PJmI4euY91knIROPEZ4N1KsMBiwordGaxCaGXam2iq0=","digest":"+FrKhxWcXi0iWLhQaZ54MV2G4iLQXUJA9XhoIyyyWoQ="}},"config":{"json_size":"12288","keyslots_size":"16744448"}}"#;
    /// `segment_digest` of U1's data segment and of U3's, by Python's
    /// hashlib over the digest text, a zero byte and the salt text.
    const U1_SEGMENT: &str = "8184a7dd05012478e30f74d54fde60529df8e06382a5fbd62e6ced18b31a3ac3";
    const U3_SEGMENT: &str = "14054f22dd709921bac58513122a92a2d2f3381558a70875a4e17cc3c899e4f1";

    fn segment(hex: &str) -> Option<[u8; SHA256_LEN]> {
        crate::token::unhex("segment", hex, SHA256_LEN)
            .ok()?
            .try_into()
            .ok()
    }

    /// The upgrade's view of cryptsetup's reencryption states: the data
    /// segment's identity unchanged by an initialized reencryption and
    /// changed by a finished one, the requirement, and the `luks2`
    /// keyslots beside the reencryption's own.
    #[test]
    fn the_upgrade_view_reads_cryptsetup_reencryption_states() {
        assert_eq!(
            Some(segment_digest(
                "Qufgdz095/Yz8tjTATvRwkixYFtr6GVq8GZfx14emAY=",
                "71shKqe2GoUjdvWJ5LwpIUK9u0s5ag1V7LuGEnVRK6s="
            )),
            segment(U1_SEGMENT)
        );
        let (u1_metadata, u1) = upgrade_metadata(U1_DUMP.as_bytes()).unwrap();
        let (u2_metadata, u2) = upgrade_metadata(U2_DUMP.as_bytes()).unwrap();
        let (u3_metadata, u3) = upgrade_metadata(U3_DUMP.as_bytes()).unwrap();
        assert_eq!(
            u1,
            Volume {
                segment: segment(U1_SEGMENT),
                reencryption: false,
                luks2: vec![0],
                bound: vec![0],
                reencrypt_modes: vec![],
            }
        );
        assert_eq!(
            u2,
            Volume {
                segment: segment(U1_SEGMENT),
                reencryption: true,
                luks2: vec![0, 1],
                bound: vec![0],
                reencrypt_modes: vec!["reencrypt".into()],
            }
        );
        assert_eq!(
            u3,
            Volume {
                segment: segment(U3_SEGMENT),
                reencryption: false,
                luks2: vec![1],
                bound: vec![1],
                reencrypt_modes: vec![],
            }
        );
        assert_eq!(u1_metadata.keyslots, [0]);
        assert_eq!(u2_metadata.keyslots, [0, 1, 2]);
        assert_eq!(u3_metadata.keyslots, [1]);
        assert_eq!(u2_metadata.protected.len(), 1);
        // The header area's own reading agrees, with its text's length.
        for (dump, metadata, volume) in [(U1_DUMP, u1_metadata, u1), (U2_DUMP, u2_metadata, u2)] {
            let view = read_upgrade(&mut Cursor::new(device(5, dump, 5, dump))).unwrap();
            assert_eq!(view.header.kind(), Kind::Upgrading);
            assert_eq!(view.header.metadata(), metadata);
            assert_eq!(view.volume, volume);
            assert_eq!(view.json_len, dump.len());
        }
    }

    #[test]
    fn the_upgrade_view_names_one_data_segment_digest_and_reads_requirements() {
        let volume = |digests: &str, config: &str| {
            let json = format!(
                r#"{{"keyslots":{{"0":{{"type":"luks2"}},"2":{{"type":"reencrypt"}}}},"tokens":{{}},"segments":{{}},"digests":{{{digests}}},"config":{{{config}}}}}"#
            );
            upgrade_metadata(json.as_bytes()).map(|(_, volume)| volume)
        };
        let one = r#""0":{"segments":["0"],"digest":"d","salt":"s"}"#;
        let found = volume(one, "").unwrap();
        assert_eq!(found.segment, Some(segment_digest("d", "s")));
        assert_ne!(found.segment, Some(segment_digest("ds", "")));
        assert_eq!(found.luks2, [0]);
        assert_eq!(found.reencrypt_modes, [""]);
        assert!(!found.reencryption);
        for digests in [
            "",
            r#""0":{"segments":["1"],"digest":"d","salt":"s"}"#,
            r#""0":{"segments":["0"],"digest":"d"}"#,
            r#""0":{"segments":["0"],"salt":"s"}"#,
            r#""0":{"segments":["0"],"digest":"d","salt":"s"},"1":{"segments":["0"],"digest":"e","salt":"s"}"#,
        ] {
            assert_eq!(volume(digests, "").unwrap().segment, None, "{digests}");
        }
        for (mandatory, reencryption) in [
            ("[]", false),
            (r#"["foo"]"#, false),
            (r#"["online-reencrypt"]"#, true),
            (r#"["foo","online-reencrypt-v3"]"#, true),
        ] {
            let config = format!(r#""requirements":{{"mandatory":{mandatory}}}"#);
            assert_eq!(
                volume(one, &config).unwrap().reencryption,
                reencryption,
                "{mandatory}"
            );
        }
        for mandatory in [r#""online-reencrypt-v2""#, "[2]"] {
            let config = format!(r#""requirements":{{"mandatory":{mandatory}}}"#);
            assert!(volume(one, &config).is_err(), "{mandatory}");
        }
    }
}
