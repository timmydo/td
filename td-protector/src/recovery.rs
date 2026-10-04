//! The device-bound tier's recovery key (DESIGN.md "Recovery key"): 16 bytes
//! from `/dev/random`, written as eight groups of a five-digit 16-bit value
//! and its Damm check digit. Its 48 digits are the keyslot passphrase.

use std::fmt;
use std::path::Path;

/// The key's random bytes: 128 bits.
pub const KEY_BYTES: usize = 16;
/// Groups in the written key, one per big-endian 16-bit value.
pub const GROUPS: usize = 8;
/// Digits in a group: five for the value, one check digit.
pub const GROUP_DIGITS: usize = 6;
/// The keyslot passphrase: every group's digits, no separator.
pub const PASSPHRASE_LEN: usize = GROUPS * GROUP_DIGITS;
/// The display form: the groups joined with hyphens.
pub const DISPLAY_LEN: usize = PASSPHRASE_LEN + GROUPS - 1;
/// The longest entry `RecoveryKey::parse` scans.
pub const MAX_ENTRY_LEN: usize = 256;

/// The standard Damm quasigroup of order 10. Its shape is the invariant.
const DAMM: [[u8; 10]; 10] = [
    [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
    [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
    [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
    [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
    [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
    [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
    [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
    [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
    [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
    [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
];

/// The Damm interim digit over `digits` (values 0 to 9), from zero. Over a
/// value's digits it is the check digit; over a whole group it is zero
/// exactly when the check digit agrees. `None` for a non-digit value.
fn damm(digits: &[u8]) -> Option<u8> {
    digits.iter().try_fold(0u8, |interim, digit| {
        DAMM.get(usize::from(interim))?
            .get(usize::from(*digit))
            .copied()
    })
}

/// A group's six digit values for one 16-bit value.
fn group(value: u16) -> Option<[u8; GROUP_DIGITS]> {
    let mut digits = [0u8; GROUP_DIGITS];
    let mut rest = value;
    for slot in digits.iter_mut().take(GROUP_DIGITS - 1).rev() {
        *slot = u8::try_from(rest % 10).ok()?;
        rest /= 10;
    }
    let check = damm(digits.get(..GROUP_DIGITS - 1)?)?;
    *digits.last_mut()? = check;
    Some(digits)
}

/// ASCII text derived from a recovery key: its passphrase or display form.
/// Allocated once at its exact final length and zeroed on drop;
/// deliberately neither `Debug`, `Display` nor `Clone`.
pub struct RecoveryText(Vec<u8>);
impl Drop for RecoveryText {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}
impl RecoveryText {
    /// The bytes, for a keyslot operation or a display that must not copy
    /// them into argv, the environment, a log or a persistent file.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// The same bytes as text; they are always ASCII. The borrow is for
    /// drawing them: UI code must not copy them into an ordinary `String`
    /// or any other buffer that is not zeroed when dropped.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

/// Why an entered recovery key was refused. Groups and positions count
/// from one; no variant carries the entered digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryError {
    /// More than `MAX_ENTRY_LEN` bytes.
    TooLong,
    /// A byte other than a digit, space or hyphen, at this byte offset
    /// counting from one. A tab or line terminator is such a byte.
    Character { position: usize },
    /// A separator ends a run of this many digits, starting at this group,
    /// that is not a whole number of groups.
    GroupLength { group: usize, digits: usize },
    /// This many digits rather than 48.
    Length { digits: usize },
    /// This group's five digits exceed 65535.
    Value { group: usize },
    /// This group's check digit does not agree.
    Check { group: usize },
}
impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(
                f,
                "recovery key entry is longer than {MAX_ENTRY_LEN} bytes"
            ),
            Self::Character { position } => write!(
                f,
                "recovery key entry has a character other than a digit, space or hyphen at byte {position}"
            ),
            Self::GroupLength { group, digits } => write!(
                f,
                "recovery key group {group} runs {digits} digits to a separator; groups have {GROUP_DIGITS} digits"
            ),
            Self::Length { digits } => write!(
                f,
                "recovery key entry has {digits} digits, not {PASSPHRASE_LEN}"
            ),
            Self::Value { group } => {
                write!(f, "recovery key group {group} is above 65535")
            }
            Self::Check { group } => {
                write!(f, "recovery key group {group} has a wrong check digit")
            }
        }
    }
}
impl std::error::Error for EntryError {}

/// A recovery key, held as its 48 ASCII passphrase digits, which encode
/// its 16 bytes one to one. It lives in one heap allocation that is zeroed
/// on drop, and it is deliberately neither `Debug`, `Display` nor `Clone`.
pub struct RecoveryKey(Box<[u8; PASSPHRASE_LEN]>);
impl Drop for RecoveryKey {
    fn drop(&mut self) {
        td_tpm::zero(self.0.as_mut_slice());
    }
}
impl RecoveryKey {
    /// 16 bytes from `/dev/random`, as `Secret::generate` reads.
    pub fn generate() -> Result<Self, String> {
        Self::read_from(Path::new(crate::SECRET_SOURCE))
    }

    fn read_from(source: &Path) -> Result<Self, String> {
        let mut bytes = Box::new([0u8; KEY_BYTES]);
        let key = crate::read_random(source, bytes.as_mut_slice(), "recovery key").and_then(|()| {
            Self::from_bytes(&bytes).ok_or_else(|| "encode recovery key".to_string())
        });
        td_tpm::zero(bytes.as_mut_slice());
        key
    }

    /// Encode 16 bytes. Every 16-bit value has a group, so this is `None`
    /// only if the encoder itself is wrong, and no digits escape then.
    fn from_bytes(bytes: &[u8; KEY_BYTES]) -> Option<Self> {
        let mut key = Self(Box::new([0; PASSPHRASE_LEN]));
        for (pair, out) in bytes
            .as_chunks::<2>()
            .0
            .iter()
            .zip(key.0.as_chunks_mut::<GROUP_DIGITS>().0.iter_mut())
        {
            let mut digits = group(u16::from_be_bytes(*pair))?;
            for (slot, digit) in out.iter_mut().zip(digits.iter()) {
                *slot = b'0' + digit;
            }
            td_tpm::zero(&mut digits);
        }
        Some(key)
    }

    /// Admit a typed key. Spaces and hyphens may appear only between
    /// groups and around the whole. Any other byte, a tab or a line
    /// terminator included, is refused, so a caller strips a line's
    /// terminator first. Everything else is DESIGN.md's.
    pub fn parse(entry: &[u8]) -> Result<Self, EntryError> {
        if entry.len() > MAX_ENTRY_LEN {
            return Err(EntryError::TooLong);
        }
        let mut digits = [0u8; PASSPHRASE_LEN];
        let result = Self::parse_into(entry, &mut digits);
        td_tpm::zero(&mut digits);
        result
    }

    fn parse_into(entry: &[u8], digits: &mut [u8; PASSPHRASE_LEN]) -> Result<Self, EntryError> {
        let mut count = 0usize;
        // The digits before the current run: always whole groups.
        let mut run_start = 0usize;
        for (index, byte) in entry.iter().enumerate() {
            match byte {
                b'0'..=b'9' => {
                    if let Some(slot) = digits.get_mut(count) {
                        *slot = byte - b'0';
                    }
                    count = count.saturating_add(1);
                }
                b' ' | b'-' => {
                    let run = count.saturating_sub(run_start);
                    if !run.is_multiple_of(GROUP_DIGITS) {
                        return Err(EntryError::GroupLength {
                            group: run_start / GROUP_DIGITS + 1,
                            digits: run,
                        });
                    }
                    run_start = count;
                }
                _ => {
                    return Err(EntryError::Character {
                        position: index + 1,
                    })
                }
            }
        }
        if count != PASSPHRASE_LEN {
            return Err(EntryError::Length { digits: count });
        }
        let mut key = Self(Box::new([0; PASSPHRASE_LEN]));
        for (index, (written, out)) in digits
            .as_chunks::<GROUP_DIGITS>()
            .0
            .iter()
            .zip(key.0.as_chunks_mut::<GROUP_DIGITS>().0.iter_mut())
            .enumerate()
        {
            let group = index + 1;
            let value = written
                .iter()
                .take(GROUP_DIGITS - 1)
                .fold(0u32, |value, digit| value * 10 + u32::from(*digit));
            if value > u32::from(u16::MAX) {
                return Err(EntryError::Value { group });
            }
            if damm(written) != Some(0) {
                return Err(EntryError::Check { group });
            }
            for (slot, digit) in out.iter_mut().zip(written.iter()) {
                *slot = b'0' + digit;
            }
        }
        Ok(key)
    }

    /// The keyslot passphrase: the 48 ASCII digits with no separator,
    /// which is the form stock cryptsetup must be given.
    pub fn passphrase(&self) -> RecoveryText {
        RecoveryText(self.0.to_vec())
    }

    /// The display form: the eight groups joined with hyphens. UI code
    /// draws it from the borrow and keeps no other copy.
    pub fn display(&self) -> RecoveryText {
        let mut out = RecoveryText(Vec::with_capacity(DISPLAY_LEN));
        for (index, group) in self.0.as_chunks::<GROUP_DIGITS>().0.iter().enumerate() {
            if index > 0 {
                out.0.push(b'-');
            }
            out.0.extend_from_slice(group);
        }
        out
    }

    /// Whether `other` is the same key, comparing every digit.
    pub fn matches(&self, other: &Self) -> bool {
        let difference = self
            .0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b));
        std::hint::black_box(difference) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits(text: &str) -> Vec<u8> {
        text.bytes().map(|byte| byte - b'0').collect()
    }

    fn key(bytes: [u8; KEY_BYTES]) -> RecoveryKey {
        match RecoveryKey::from_bytes(&bytes) {
            Some(key) => key,
            None => panic!("{bytes:?} did not encode"),
        }
    }

    /// `RecoveryKey` is not `Debug`, so no `unwrap`.
    fn admitted(entry: &str) -> RecoveryKey {
        match RecoveryKey::parse(entry.as_bytes()) {
            Ok(key) => key,
            Err(error) => panic!("{entry:?} refused: {error}"),
        }
    }

    fn refused(entry: &str) -> EntryError {
        match RecoveryKey::parse(entry.as_bytes()) {
            Ok(_) => panic!("{entry:?} admitted"),
            Err(error) => error,
        }
    }

    #[test]
    fn damm_matches_its_published_table_and_example() {
        // Damm's table as published, and its worked example: 572 checks to
        // 4, and 5724 validates to zero.
        assert_eq!(
            DAMM,
            [
                [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
                [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
                [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
                [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
                [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
                [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
                [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
                [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
                [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
                [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
            ]
        );
        assert!((0..10).all(|row| DAMM[row][row] == 0));
        for row in DAMM {
            let mut seen = row.to_vec();
            seen.sort_unstable();
            assert_eq!(seen, (0..10).collect::<Vec<u8>>());
        }
        assert_eq!(damm(&digits("572")), Some(4));
        assert_eq!(damm(&digits("5724")), Some(0));
        assert_eq!(damm(&[10]), None);
    }

    #[test]
    fn keys_encode_to_pinned_groups() {
        // An independent implementation computed these.
        let counting = key(std::array::from_fn(|index| index as u8));
        assert_eq!(
            counting.display().as_str(),
            "000013-005150-010290-015439-020571-025716-030859-035998"
        );
        assert_eq!(
            counting.passphrase().expose(),
            b"000013005150010290015439020571025716030859035998"
        );
        assert_eq!(
            key([0; KEY_BYTES]).passphrase().as_str(),
            "000000".repeat(GROUPS)
        );
        assert_eq!(
            key([0xff; KEY_BYTES]).display().as_str(),
            ["655354"; GROUPS].join("-")
        );
        // Each text is one allocation at exactly its length.
        let display = counting.display();
        assert_eq!(
            (display.0.len(), display.0.capacity()),
            (DISPLAY_LEN, DISPLAY_LEN)
        );
        let passphrase = counting.passphrase();
        assert_eq!(
            (passphrase.0.len(), passphrase.0.capacity()),
            (PASSPHRASE_LEN, PASSPHRASE_LEN)
        );
    }

    #[test]
    fn every_group_detects_single_substitutions_and_adjacent_transpositions() {
        for value in 0..=u16::MAX {
            let written = group(value).unwrap();
            assert_eq!(damm(&written), Some(0), "{value}");
            for position in 0..GROUP_DIGITS {
                for digit in 0..10 {
                    if digit == written[position] {
                        continue;
                    }
                    let mut wrong = written;
                    wrong[position] = digit;
                    assert_ne!(damm(&wrong), Some(0), "{value} at {position}");
                }
            }
            for position in 0..GROUP_DIGITS - 1 {
                if written[position] == written[position + 1] {
                    continue;
                }
                let mut swapped = written;
                swapped.swap(position, position + 1);
                assert_ne!(damm(&swapped), Some(0), "{value} at {position}");
            }
        }
    }

    #[test]
    fn keys_round_trip_through_their_passphrase_and_display_form() {
        for bytes in [
            [0; KEY_BYTES],
            [0xff; KEY_BYTES],
            std::array::from_fn(|index| (index as u8).wrapping_mul(37)),
        ] {
            let original = key(bytes);
            let passphrase = original.passphrase();
            let display = original.display();
            for entry in [passphrase.as_str(), display.as_str()] {
                let parsed = admitted(entry);
                assert!(parsed.matches(&original));
                assert_eq!(parsed.passphrase().expose(), passphrase.expose());
            }
        }
        assert!(!key([0; KEY_BYTES]).matches(&key([0xff; KEY_BYTES])));
    }

    #[test]
    fn entry_tolerates_separators_only_between_groups() {
        let canonical = "000013-005150-010290-015439-020571-025716-030859-035998";
        let original = admitted(canonical);
        for entry in [
            "000013005150010290015439020571025716030859035998",
            "000013 005150 010290 015439 020571 025716 030859 035998",
            "  000013 - 005150--010290 015439 020571 025716 030859 035998 ",
            "-000013-005150-010290-015439-020571-025716-030859-035998-",
        ] {
            assert!(admitted(entry).matches(&original), "{entry}");
        }
        assert_eq!(
            refused("000013-00515 0-010290-015439-020571-025716-030859-035998"),
            EntryError::GroupLength {
                group: 2,
                digits: 5
            }
        );
        assert_eq!(
            refused("000013-005150-010290-015439-020571-025716-030859-03599 8"),
            EntryError::GroupLength {
                group: 8,
                digits: 5
            }
        );
        // An extra digit names the group it was typed in, not the next.
        assert_eq!(
            refused("000013-0051500-010290-015439-020571-025716-030859-035998"),
            EntryError::GroupLength {
                group: 2,
                digits: 7
            }
        );
        // A run may hold several groups, from the one after a separator.
        assert_eq!(
            refused("000013-0051500102900 015439-020571-025716-030859-035998"),
            EntryError::GroupLength {
                group: 2,
                digits: 13
            }
        );
        assert_eq!(
            EntryError::GroupLength {
                group: 2,
                digits: 7
            }
            .to_string(),
            "recovery key group 2 runs 7 digits to a separator; groups have 6 digits"
        );
        assert_eq!(
            refused("000013_005150-010290-015439-020571-025716-030859-035998"),
            EntryError::Character { position: 7 }
        );
        // The first refused byte follows only ASCII, so its byte offset is
        // also its character position.
        assert_eq!(
            refused("000013-005150-010290-015439-020571-025716-030859-03599\u{0663}"),
            EntryError::Character { position: 55 }
        );
        // A tab or line terminator is refused; callers strip a line's end.
        for end in ["\n", "\r\n", "\t"] {
            assert_eq!(
                refused(&format!("{canonical}{end}")),
                EntryError::Character { position: 56 },
                "{end:?}"
            );
        }
    }

    #[test]
    fn entry_refuses_lengths_values_and_check_digits_by_group() {
        assert_eq!(
            refused("000013-005150-010290-015439-020571-025716-030859-03599"),
            EntryError::Length { digits: 47 }
        );
        assert_eq!(
            refused("000013-005150-010290-015439-020571-025716-030859-0359980"),
            EntryError::Length { digits: 49 }
        );
        assert_eq!(refused(""), EntryError::Length { digits: 0 });
        assert_eq!(refused(&"0".repeat(MAX_ENTRY_LEN + 1)), EntryError::TooLong);
        assert_eq!(
            refused(&"0".repeat(MAX_ENTRY_LEN)),
            EntryError::Length {
                digits: MAX_ENTRY_LEN
            }
        );
        // 65535 is the largest value; 65536 with its own check digit is
        // refused as a value, not a check digit.
        let mut value = digits("65536");
        value.push(damm(&value).unwrap());
        let value: String = value.iter().map(|digit| char::from(b'0' + digit)).collect();
        let entry = format!("000000-000000-{value}-000000-000000-000000-000000-000000");
        assert_eq!(refused(&entry), EntryError::Value { group: 3 });
        assert_eq!(
            refused("000000-000000-000000-000000-000000-000000-000000-999999"),
            EntryError::Value { group: 8 }
        );
        // A wrong check digit in group 4, then a transposition in group 6.
        assert_eq!(
            refused("000013-005150-010290-015438-020571-025716-030859-035998"),
            EntryError::Check { group: 4 }
        );
        assert_eq!(
            refused("000013-005150-010290-015439-020571-052716-030859-035998"),
            EntryError::Check { group: 6 }
        );
        assert_eq!(
            EntryError::Check { group: 4 }.to_string(),
            "recovery key group 4 has a wrong check digit"
        );
        assert_eq!(
            EntryError::Length { digits: 47 }.to_string(),
            "recovery key entry has 47 digits, not 48"
        );
    }

    #[test]
    fn keys_come_from_dev_random_in_one_exact_read() {
        assert_eq!(crate::SECRET_SOURCE, "/dev/random");
        let dir = std::env::temp_dir().join(format!(
            "td-protector-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("random");
        let bytes: Vec<u8> = (0..20).collect();
        std::fs::write(&source, &bytes).unwrap();
        let read = match RecoveryKey::read_from(&source) {
            Ok(key) => key,
            Err(error) => panic!("{error}"),
        };
        // The first 16 bytes, 0 to 15, as the pinned counting key.
        assert_eq!(
            read.display().as_str(),
            "000013-005150-010290-015439-020571-025716-030859-035998"
        );
        std::fs::write(&source, &bytes[..KEY_BYTES - 1]).unwrap();
        let short = match RecoveryKey::read_from(&source) {
            Ok(_) => panic!("a short source was accepted"),
            Err(error) => error,
        };
        assert!(
            short.starts_with(&format!("read recovery key from {}: ", source.display())),
            "{short}"
        );
        std::fs::remove_dir_all(&dir).unwrap();

        let first = RecoveryKey::generate().unwrap_or_else(|error| panic!("{error}"));
        let second = RecoveryKey::generate().unwrap_or_else(|error| panic!("{error}"));
        assert!(!first.matches(&second));
    }
}
