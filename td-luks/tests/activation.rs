//! Activation authorization differs from testing an unbound keyslot.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use std::fs::{self, File};
use td_fs::TempDir;
use td_luks::write::{self, ActivationKey, Error};

const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";

#[test]
fn activation_requires_a_bound_slot_or_a_verified_volume_key() {
    let dir = TempDir::new("td-luks-activation").unwrap();
    let path = dir.path().join("device");
    fs::write(&path, include_bytes!("../fixtures/unbound.bin")).unwrap();
    let mut file = File::open(path).unwrap();
    let expected: [u8; 64] = std::array::from_fn(|i| i as u8);
    for credential in [
        ActivationKey::Passphrase {
            slot: None,
            bytes: RECOVERY,
        },
        ActivationKey::Passphrase {
            slot: Some(0),
            bytes: RECOVERY,
        },
        ActivationKey::Volume(&expected),
    ] {
        let (header, key) = write::activation(&mut file, credential).unwrap();
        assert_eq!(header.uuid, "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a");
        assert_eq!(key.bytes(), &expected);
    }
    assert!(matches!(
        write::activation(
            &mut file,
            ActivationKey::Passphrase {
                slot: Some(1),
                bytes: SECOND
            }
        ),
        Err(Error::Refused(_))
    ));
    write::test(&mut file, 1, SECOND).unwrap();
    for credential in [
        ActivationKey::Passphrase {
            slot: None,
            bytes: SECOND,
        },
        ActivationKey::Passphrase {
            slot: Some(0),
            bytes: SECOND,
        },
        ActivationKey::Volume(&[0; 64]),
    ] {
        assert!(matches!(
            write::activation(&mut file, credential),
            Err(Error::WrongKey)
        ));
    }
    assert!(matches!(
        write::activation(&mut file, ActivationKey::Volume(&[0; 63])),
        Err(Error::Refused(_))
    ));
}
