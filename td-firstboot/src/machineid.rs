//! The 128-bit per-machine id, in the one representation every reader of
//! `/etc/machine-id` expects: 32 lowercase hex digits and a trailing newline.
//!
//! Format handling is separated from provisioning because the two failure
//! modes are opposite. Writing is trivial; READING a machine-id that does not
//! look right must never be answered by generating a fresh one, because that
//! turns a corrupt file into a new machine identity on every boot. So parsing is
//! strict and returns a diagnostic the operator can act on.

/// The `/etc/machine-id` conventions this implements: exactly 32 lowercase hex
/// digits, and all-zeroes reserved to mean "not provisioned" (so it is never a
/// valid id to have persisted).
const HEX_DIGITS: usize = 32;

/// Render 16 bytes of kernel entropy as the on-disk machine-id line.
pub(crate) fn encode(bytes: &[u8; 16]) -> String {
    let mut id = String::with_capacity(HEX_DIGITS + 1);
    for byte in bytes {
        // One tiny allocation per nibble pair, once per machine LIFETIME — the
        // panicking-index-free alternatives cost more clarity than this costs
        // anything.
        id.push_str(&format!("{byte:02x}"));
    }
    id.push('\n');
    id
}

/// The id in a file we previously wrote, or why it cannot be trusted. Never
/// answered by regenerating: see the module comment.
pub(crate) fn validate(text: &str) -> Result<&str, String> {
    // Exactly ONE optional trailing newline: `trim_end_matches` would accept a
    // file that had grown extra ones, and the shape of a file we wrote ourselves
    // changing is itself the signal something else has been editing it.
    let id = text.strip_suffix('\n').unwrap_or(text);
    if id.len() != HEX_DIGITS {
        return Err(format!(
            "machine-id is {} characters, expected exactly {HEX_DIGITS} lowercase hex digits",
            id.len()
        ));
    }
    for character in id.chars() {
        if !matches!(character, '0'..='9' | 'a'..='f') {
            return Err(format!(
                "machine-id contains {character:?}, which is not a lowercase hex digit"
            ));
        }
    }
    if id.bytes().all(|byte| byte == b'0') {
        return Err(
            "machine-id is all zeroes, which is reserved to mean 'not provisioned'".to_string(),
        );
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sixteen_bytes_encode_to_thirty_two_hex_digits_and_a_newline() {
        let id = encode(&[
            0x00, 0x01, 0x0f, 0x10, 0x7f, 0x80, 0xfe, 0xff, 0xa5, 0x5a, 0x12, 0x34, 0x56, 0x78,
            0x9a, 0xbc,
        ]);
        assert_eq!(id, "00010f107f80feffa55a123456789abc\n");
        assert_eq!(id.len(), 33);
    }

    #[test]
    fn what_encode_writes_is_what_validate_accepts() {
        let id = encode(&[0x42; 16]);
        assert_eq!(validate(&id).unwrap(), id.trim_end());
        // …with or without the trailing newline a reader may have stripped.
        assert!(validate(id.trim_end()).is_ok());
    }

    #[test]
    fn a_malformed_id_is_a_diagnostic_not_a_regeneration() {
        for (text, want) in [
            ("", "0 characters"),
            ("deadbeef\n", "8 characters"),
            ("00000000000000000000000000000000\n", "all zeroes"),
            ("DEADBEEFDEADBEEFDEADBEEFDEADBEEF\n", "not a lowercase hex"),
            ("deadbeefdeadbeefdeadbeefdeadbee \n", "not a lowercase hex"),
            ("deadbeef-dead-beef-dead-beefdead\n", "not a lowercase hex"),
        ] {
            let error = validate(text).unwrap_err();
            assert!(
                error.contains(want),
                "validate({text:?}) said {error:?}, expected it to mention {want:?}"
            );
        }
    }

    /// A second trailing newline is still 33 characters of id, so it is rejected
    /// rather than quietly trimmed — the file is one we wrote, and a changed shape
    /// means something else has been editing it.
    #[test]
    fn only_the_exact_shape_is_accepted() {
        assert!(validate("deadbeefdeadbeefdeadbeefdeadbeef\n\n").is_err());
        assert!(validate(" deadbeefdeadbeefdeadbeefdeadbeef\n").is_err());
        assert!(validate("deadbeefdeadbeefdeadbeefdeadbeefa\n").is_err());
    }
}
