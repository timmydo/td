//! Flathub as td trusts it: one repository, one collection, one signing key.
//!
//! td reads no Flathub keyring, summary or remote configuration. The signing
//! subkey below is the exact public-subkey packet from
//! `https://dl.flathub.org/repo/flathub.gpg`, bound to primary key
//! 6E5C05D979C76DAF93C081354184DD4D907A7CAE; that binding was checked at
//! review, and replacing the subkey is a reviewed change to this file.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

use td_engine::ostree::{self, Checksum, Commit};

use crate::openpgp::{self, RsaKey};

pub(crate) const REPOSITORY: &str = "https://dl.flathub.org/repo";
pub(crate) const COLLECTION: &str = "org.flathub.Stable";
pub(crate) const SIGNING_SUBKEY_FINGERPRINT: &str = "54A6CDDD8919FB204200D8AC562702E9E3ED7EE8";

/// The v4 RSA-4096 public-subkey packet body, created 2017-06-16.
const SIGNING_SUBKEY: &str = concat!(
    "045943daec011000b4ff0a64b267c2d695161064a630e35ee9ed9da1c6a77a4e",
    "3c24c0cdc84358abba69167136416486c182b48e83a09c5c3da85b27424075a2",
    "f6a6b186e8449766c92f92ec92605c33155d41e0f42cb1ddf105276e79c86f2f",
    "2872f58dd5a94f7ef2458c2513ae60f6d94d7270ecd1c369487b892240e35b39",
    "9b5d5ab5489904129dd8c5962b22b5b9a5587bc79b694e398cbcf86241ed87df",
    "555d7dde19a705225f7c2cb86ef92e2f1b08fa7d4384ecdcecc3d5df3e871a4e",
    "07884705d32cadf6d098869b311c5741a2ada1efbd477c0712d2cf7c11502a60",
    "c1677fd7a6fb8727629748ea68487d2d454222a5e83040600f2d4f782095ad34",
    "0eeacfa342705ffccdf8f68ee61ffa3bd68267057b1baadb92032ea8c7ab8176",
    "9ff283f7f20af4cfe32a5a791f33adc1a1dabaf063f897885dd360681416c37f",
    "7f1332e190075e3eb7024c75f6c0c68d8c6e096ebd081fc077d9d9c6ba832118",
    "fbdb3f603d60ba02e357e4ba081e755a414d7b5cf8b403e025937151c2826de6",
    "8735bb6197c1e4b3fa58f51084b1e5ff11d7f3175f1ea5b4a324c3e1e033d05d",
    "b5281709b2b9872804e3e6bd91cd97d84aec1ff544ff30d59ad793ca9dba90b4",
    "48d6da7d7ef6ef407b94c315226cd97ad1ac33dca95c9ea5961a771705b4f62b",
    "1e37218918fb2bfddbd33060d7a8e3df53ff52012d4b4936382bd3db46d76b04",
    "4df5f185fc9889390011010001",
);

/// A commit whose bytes, ref bindings and Flathub signature all checked out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SignedCommit {
    pub(crate) commit: Commit,
    pub(crate) signed_at: u32,
}

/// Whether a fetch from `repository` (already normalized) needs Flathub's
/// signature. Any other spelling of a flathub.org host is refused rather than
/// fetched unsigned, so the check cannot be lost to a port or a capital.
pub(crate) fn requires_signature(repository: &str) -> Result<bool, String> {
    if repository == REPOSITORY {
        return Ok(true);
    }
    let authority = repository
        .split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "flathub.org" || host.ends_with(".flathub.org") {
        return Err(format!(
            "Flathub repository {repository:?} must be spelled exactly {REPOSITORY:?}, where its signature is checked"
        ));
    }
    Ok(false)
}

pub(crate) fn signing_key() -> Result<RsaKey, String> {
    let body = crate::sig::from_hex(SIGNING_SUBKEY)?;
    let key = RsaKey::from_packet_body(&body)?;
    if key.fingerprint_hex() != SIGNING_SUBKEY_FINGERPRINT {
        return Err(format!(
            "compiled Flathub signing subkey has fingerprint {}, expected {SIGNING_SUBKEY_FINGERPRINT}",
            key.fingerprint_hex()
        ));
    }
    Ok(key)
}

/// Authenticate one commit for `exact_ref`: its bytes hash to `checksum`, it
/// binds itself to exactly that ref in Flathub's collection, and one of its
/// detached signatures verifies under the compiled key at time `now`.
pub(crate) fn verify_commit(
    exact_ref: &str,
    checksum: Checksum,
    commit_bytes: &[u8],
    commitmeta: &[u8],
    now: u64,
) -> Result<SignedCommit, String> {
    let commit = ostree::parse_commit_verified(checksum, commit_bytes)?;
    commit.metadata.require_exact_ref(exact_ref)?;
    if commit.metadata.collection_binding.as_deref() != Some(COLLECTION) {
        return Err(format!(
            "commit ostree.collection-binding {:?} is not {COLLECTION:?}",
            commit.metadata.collection_binding
        ));
    }
    if commit.metadata.collection_refs_binding != [(COLLECTION.to_string(), exact_ref.to_string())]
    {
        return Err(format!(
            "commit ostree.collection-refs-binding {:?} does not name only ({COLLECTION:?}, {exact_ref:?})",
            commit.metadata.collection_refs_binding
        ));
    }
    let key = signing_key()?;
    let signatures = ostree::parse_commitmeta_signatures(commitmeta)?;
    let verified =
        openpgp::verify_any(&key, &signatures, commit_bytes, now).map_err(|refusals| {
            format!(
                "no Flathub signature on commit {} verifies: {}",
                checksum.to_hex(),
                refusals.join("; ")
            )
        })?;
    Ok(SignedCommit {
        commit,
        signed_at: verified.created,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    pub(crate) const SUBKEY_BODY: &str = SIGNING_SUBKEY;
    const FIREFOX_REF: &str = "app/org.mozilla.firefox/x86_64/stable";
    const RUNTIME_REF: &str = "runtime/org.freedesktop.Platform/x86_64/25.08";
    const NOW: u64 = 1_792_600_000;

    fn hex(text: &str) -> Vec<u8> {
        let digits: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        crate::sig::from_hex(&digits).unwrap()
    }

    fn firefox() -> (Checksum, Vec<u8>, Vec<u8>) {
        (
            Checksum::from_hex("7985364b35b83cfe5aa17a00932df1c2d1dae69e5c592aa637a53ff1d53fdabc")
                .unwrap(),
            hex(include_str!(
                "../../engine/tests/fixtures/flathub-firefox-7985364b.commit.hex"
            )),
            hex(include_str!(
                "../../engine/tests/fixtures/flathub-firefox-7985364b.commitmeta.hex"
            )),
        )
    }

    fn runtime() -> (Checksum, Vec<u8>, Vec<u8>) {
        (
            Checksum::from_hex("bd44a6230581917d04f89812a4c21090c304d390edb73995af1c2f9fd8abf4e8")
                .unwrap(),
            hex(include_str!(
                "../../engine/tests/fixtures/flathub-freedesktop-25.08.commit.hex"
            )),
            hex(include_str!(
                "../../engine/tests/fixtures/flathub-freedesktop-25.08.commitmeta.hex"
            )),
        )
    }

    #[test]
    fn compiled_key_is_the_reviewed_subkey() {
        assert_eq!(
            signing_key().unwrap().fingerprint_hex(),
            SIGNING_SUBKEY_FINGERPRINT
        );
        assert_eq!(requires_signature("https://dl.flathub.org/repo"), Ok(true));
        assert_eq!(requires_signature("https://example.org/repo"), Ok(false));
        assert_eq!(requires_signature("http://127.0.0.1:8080/repo"), Ok(false));
        for elsewhere in [
            "https://dl.flathub.org/beta-repo",
            "https://dl.flathub.org:443/repo",
            "https://DL.Flathub.org/repo",
            "https://dl.flathub.org./repo",
            "https://flathub.org/repo",
            "https://mirror.flathub.org/repo",
        ] {
            assert!(requires_signature(elsewhere)
                .unwrap_err()
                .contains("spelled exactly"));
        }
    }

    #[test]
    fn real_app_and_runtime_commits_verify_under_their_refs() {
        let (checksum, commit, meta) = firefox();
        let signed = verify_commit(FIREFOX_REF, checksum, &commit, &meta, NOW).unwrap();
        assert_eq!(signed.signed_at, 1_792_503_025);
        assert_eq!(signed.commit.timestamp, 1_792_501_610);
        let (checksum, commit, meta) = runtime();
        let signed = verify_commit(RUNTIME_REF, checksum, &commit, &meta, NOW).unwrap();
        assert!(u64::from(signed.signed_at) >= signed.commit.timestamp);
    }

    #[test]
    fn a_signature_or_ref_from_elsewhere_is_refused() {
        let (checksum, commit, meta) = firefox();
        let (runtime_checksum, runtime_commit, runtime_meta) = runtime();
        // A good Flathub signature over a different commit does not transfer.
        assert!(
            verify_commit(FIREFOX_REF, checksum, &commit, &runtime_meta, NOW)
                .unwrap_err()
                .contains("no Flathub signature")
        );
        assert!(
            verify_commit(RUNTIME_REF, runtime_checksum, &runtime_commit, &meta, NOW)
                .unwrap_err()
                .contains("does not verify")
        );
        // A real signed commit presented under another ref is refused before
        // its signature is consulted.
        assert!(verify_commit(
            "app/org.mozilla.firefox/x86_64/beta",
            checksum,
            &commit,
            &meta,
            NOW
        )
        .unwrap_err()
        .contains("xa.ref"));
        assert!(
            verify_commit(FIREFOX_REF, runtime_checksum, &commit, &meta, NOW)
                .unwrap_err()
                .contains("mismatch")
        );
        assert!(verify_commit(FIREFOX_REF, checksum, &commit, &meta, 1_000)
            .unwrap_err()
            .contains("future"));
    }
}
