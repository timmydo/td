#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_install::installation_plan::{
    Basis, Candidates, Destination, DestinationObservation, Plan, Settings, Storage, Zones,
    MAX_BYTES, MAX_CANDIDATES, MAX_CANDIDATE_BYTES, MAX_ZONES, MAX_ZONE_BYTES,
};

fn observed_destination(
    name: &str,
    number: (u32, u32),
    sequence: u64,
    geometry: (u64, u32),
    removable: bool,
    labels: (Option<&str>, Option<&str>, Option<&str>),
) -> Result<Destination, String> {
    Destination::new(DestinationObservation {
        name,
        major: number.0,
        minor: number.1,
        sequence,
        capacity: geometry.0,
        sector: geometry.1,
        removable,
        model: labels.0,
        serial: labels.1,
        wwid: labels.2,
    })
}

fn destination() -> Destination {
    observed_destination(
        "nvme0n1",
        (259, 3),
        27,
        (6 << 30, 4096),
        false,
        (Some("Model A"), Some("serial-42"), None),
    )
    .unwrap()
}
fn settings() -> Settings {
    Settings::new("alice", "td-laptop", "us", "Etc/UTC").unwrap()
}
fn uuid() -> [u8; 16] {
    [
        0x19, 2, 3, 4, 5, 6, 0x47, 8, 0x89, 10, 11, 12, 13, 14, 15, 16,
    ]
}
fn plan() -> Plan {
    Plan::new(
        [1; 32],
        destination(),
        [2; 32],
        uuid(),
        Basis::default(),
        settings(),
    )
    .unwrap()
}

fn distinct_candidates(count: usize, maximal: bool) -> Vec<Destination> {
    let label = "X".repeat(256);
    let mut disks = Vec::new();
    for index in 0..count {
        let stem = format!("vd{index:02}");
        let name = if maximal {
            format!("{stem:0<64}")
        } else {
            stem
        };
        let labels = if maximal {
            (
                Some(label.as_str()),
                Some(label.as_str()),
                Some(label.as_str()),
            )
        } else {
            (None, None, None)
        };
        disks.push(
            observed_destination(
                &name,
                (253, index as u32),
                index as u64 + 1,
                (6 << 30, 4096),
                false,
                labels,
            )
            .unwrap(),
        );
    }
    disks
}

#[test]
fn candidate_wire_is_canonical_bounded_and_authority_free() {
    let disk = destination();
    let candidates = Candidates::new(vec![disk.clone()]).unwrap();
    let mut bytes = b"TDCAND01\x01".to_vec();
    bytes.extend([0, 0, 1, 3, 0, 0, 0, 3]);
    bytes.extend([0, 0, 0, 0, 0, 0, 0, 27]);
    bytes.extend([0, 0, 0, 1, 128, 0, 0, 0]);
    bytes.extend([0, 0, 16, 0, 0]);
    bytes.extend(b"\0\x07nvme0n1\x01\0\x07Model A\x01\0\x09serial-42\0");
    assert_eq!(candidates.encode(), bytes);
    assert_eq!(Candidates::decode(&bytes).unwrap().as_slice(), &[disk]);
    let mut duplicate = b"TDCAND01\x02".to_vec();
    duplicate.extend_from_slice(&bytes[9..]);
    duplicate.extend_from_slice(&bytes[9..]);
    assert!(Candidates::decode(&duplicate).is_err());
    for prefix in 0..bytes.len() {
        assert!(Candidates::decode(&bytes[..prefix]).is_err());
    }
    assert_eq!(Candidates::decode(b"TDCAND01\0").unwrap().as_slice(), &[]);
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Candidates::decode(&trailing).is_err());
    let mut malformed = bytes.clone();
    *malformed.get_mut(37).unwrap() = 2;
    assert_eq!(
        Candidates::decode(&malformed),
        Err("invalid candidates flag".into())
    );
    let mut too_many = b"TDCAND01".to_vec();
    too_many.push((MAX_CANDIDATES + 1) as u8);
    assert_eq!(
        Candidates::decode(&too_many),
        Err("too many installation candidates".into())
    );
}

#[test]
fn candidate_identity_is_unique_and_count_is_bounded() {
    let first = destination();
    let same_number = observed_destination(
        "vda",
        (259, 3),
        28,
        (6 << 30, 4096),
        false,
        (None, None, None),
    )
    .unwrap();
    let same_name = observed_destination(
        "nvme0n1",
        (8, 0),
        28,
        (6 << 30, 4096),
        false,
        (None, None, None),
    )
    .unwrap();
    let same_sequence = observed_destination(
        "vda",
        (8, 0),
        27,
        (6 << 30, 4096),
        false,
        (None, None, None),
    )
    .unwrap();
    assert!(Candidates::new(vec![first.clone(), first.clone()]).is_err());
    assert!(Candidates::new(vec![first.clone(), same_number]).is_err());
    assert!(Candidates::new(vec![first.clone(), same_name]).is_err());
    assert!(Candidates::new(vec![first, same_sequence]).is_err());
    assert!(Candidates::new(distinct_candidates(MAX_CANDIDATES + 1, false)).is_err());

    let full = Candidates::new(distinct_candidates(MAX_CANDIDATES, true)).unwrap();
    let bytes = full.encode();
    assert_eq!(bytes.len(), MAX_CANDIDATE_BYTES);
    assert_eq!(Candidates::decode(&bytes).unwrap(), full);
    let mut oversized = bytes;
    oversized.push(0);
    assert_eq!(
        Candidates::decode(&oversized),
        Err("installation candidates exceed wire bound".into())
    );
}

#[test]
fn time_zones_are_ascending_tokens_and_their_wire_is_canonical() {
    let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
    let zones = Zones::new(ids(&["America/Los_Angeles", "Etc/UTC"])).unwrap();
    assert_eq!(zones.as_slice(), ["America/Los_Angeles", "Etc/UTC"]);
    let bytes = zones.encode();
    assert_eq!(
        bytes,
        b"TDZONE01\0\x02\0\x13America/Los_Angeles\0\x07Etc/UTC"
    );
    assert_eq!(Zones::decode(&bytes).unwrap(), zones);
    for end in 0..bytes.len() {
        assert!(Zones::decode(&bytes[..end]).is_err(), "{end}");
    }
    let mut extended = bytes.clone();
    extended.push(0);
    assert!(Zones::decode(&extended).is_err());
    // Out of order, repeated, empty, spaced, too long or too many refuse,
    // built or decoded.
    for bad in [
        ids(&["Etc/UTC", "America/Los_Angeles"]),
        ids(&["Etc/UTC", "Etc/UTC"]),
        ids(&[]),
        ids(&["Etc/U TC"]),
        ids(&[""]),
        vec!["Z".repeat(65)],
        (0..=MAX_ZONES).map(|i| format!("Zone/{i:04}")).collect(),
    ] {
        assert!(Zones::new(bad.clone()).is_err(), "{bad:?}");
        let mut wire = b"TDZONE01".to_vec();
        wire.extend((bad.len() as u16).to_be_bytes());
        for id in &bad {
            wire.extend((id.len() as u16).to_be_bytes());
            wire.extend(id.as_bytes());
        }
        assert!(Zones::decode(&wire).is_err(), "{bad:?}");
    }
    let widest = (0..MAX_ZONES)
        .map(|i| format!("Zone/{i:059}"))
        .collect::<Vec<_>>();
    let bytes = Zones::new(widest).unwrap().encode();
    assert_eq!(bytes.len(), MAX_ZONE_BYTES);
    let mut oversized = bytes;
    oversized.push(0);
    assert_eq!(
        Zones::decode(&oversized),
        Err("installation time zones exceed wire bound".into())
    );
}

#[test]
fn deployment_id_requires_exact_lowercase_manifest_digest() {
    let p = Plan::new(
        [1; 32],
        destination(),
        [0xab; 32],
        uuid(),
        Basis::default(),
        settings(),
    )
    .unwrap();
    let exact = "ab".repeat(32);
    assert!(p.matches_deployment_id(&exact));
    for changed in [
        exact.to_uppercase(),
        exact[..62].to_owned(),
        format!("{exact}ab"),
        format!("{exact}\n"),
        format!("ac{}", &exact[2..]),
        "zz".repeat(32),
    ] {
        assert!(!p.matches_deployment_id(&changed));
    }
}

#[test]
fn wire_order_is_independently_specified_and_lossless() {
    let p = plan();
    let mut bytes = b"TDPLAN03".to_vec();
    bytes.extend([1; 32]);
    bytes.extend([2; 32]);
    bytes.extend(uuid());
    bytes.extend([0, 0, 1, 3, 0, 0, 0, 3]);
    bytes.extend([0, 0, 0, 0, 0, 0, 0, 27]);
    bytes.extend([0, 0, 0, 1, 128, 0, 0, 0]);
    // Sector size, removable, then the storage and basis bytes.
    bytes.extend([0, 0, 16, 0, 0]);
    bytes.extend([0, 0]);
    bytes.extend(b"\0\x07nvme0n1\x01\0\x07Model A\x01\0\x09serial-42\0");
    bytes.extend(b"\0\x05alice\0\x09td-laptop\0\x02us\0\x07Etc/UTC");
    assert_eq!(p.encode(), bytes);
    assert_eq!(Plan::decode(&bytes).unwrap(), p);
    assert_eq!(p.storage(), Storage::Unencrypted);
    assert_eq!(p.basis(), Basis::default());
    assert!(!p.basis().tpm() && !p.basis().keyboard_console());
    assert_eq!(p.destination().number(), (259, 3));
    assert_eq!(p.destination().sequence(), 27);
    assert_eq!(p.destination().capacity(), 6 << 30);
    assert_eq!(p.destination().sector(), 4096);
    assert_eq!(p.destination().name(), "nvme0n1");
    assert!(!p.destination().removable());
    assert_eq!(p.destination().model(), Some("Model A"));
    assert_eq!(p.destination().serial(), Some("serial-42"));
    assert_eq!(p.destination().wwid(), None);
    assert_eq!(p.nonce(), &[1; 32]);
    assert_eq!(p.deployment(), &[2; 32]);
    assert_eq!(p.volume_uuid(), &uuid());
    assert_eq!(p.settings().username(), "alice");
    assert_eq!(p.settings().hostname(), "td-laptop");
    assert_eq!(p.settings().keyboard(), "us");
    assert_eq!(p.settings().timezone(), "Etc/UTC");
}

#[test]
fn every_truncation_and_extension_refuses() {
    let bytes = plan().encode();
    for end in 0..bytes.len() {
        assert!(Plan::decode(&bytes[..end]).is_err(), "{end}");
    }
    for extra in [vec![0], vec![b'\n'], bytes.clone()] {
        let mut extended = bytes.clone();
        extended.extend(extra);
        assert!(Plan::decode(&extended).is_err());
    }
    let mut oversized = bytes.clone();
    oversized.resize(MAX_BYTES + 1, 0);
    assert_eq!(
        Plan::decode(&oversized).unwrap_err(),
        "installation plan exceeds wire bound"
    );
    for version in [b'2', b'4'] {
        let mut wrong_version = bytes.clone();
        wrong_version[7] = version;
        assert_eq!(
            Plan::decode(&wrong_version).unwrap_err(),
            "unsupported installation plan version"
        );
    }
}

/// Storage follows the basis: device-bound exactly when both probes
/// passed (ENCRYPTION.md "Activation"), and nothing else names it.
#[test]
fn storage_follows_the_basis() {
    for (tpm, console, storage) in [
        (false, false, Storage::Unencrypted),
        (true, false, Storage::Unencrypted),
        (false, true, Storage::Unencrypted),
        (true, true, Storage::DeviceBound),
    ] {
        let basis = Basis::new(tpm, console);
        assert_eq!(basis.storage(), storage, "{tpm} {console}");
        let p = Plan::new([1; 32], destination(), [2; 32], uuid(), basis, settings()).unwrap();
        assert_eq!(p.storage(), storage);
    }
    assert_eq!(Storage::ALL, [Storage::Unencrypted, Storage::DeviceBound]);
}

/// The storage byte follows the removable flag, the storage the basis
/// names; a storage byte the basis does not name, or no storage at all,
/// is refused.
#[test]
fn storage_is_one_byte_after_the_removable_flag() {
    let unencrypted = plan();
    let bound = Plan::new(
        [1; 32],
        destination(),
        [2; 32],
        uuid(),
        Basis::new(true, true),
        settings(),
    )
    .unwrap();
    assert_eq!(bound.storage(), Storage::DeviceBound);
    assert_ne!(bound, unencrypted);
    // Magic, nonce, digest, UUID, then the fixed destination fields.
    let at = 8 + 32 + 32 + 16 + 4 + 4 + 8 + 8 + 4 + 1;
    let bytes = bound.encode();
    assert_eq!(bytes[at], 1);
    assert_eq!(unencrypted.encode()[at], 0);
    let mut differs = unencrypted.encode();
    differs[at] = 1;
    differs[at + 1] = 3;
    assert_eq!(differs, bytes);
    assert_eq!(Plan::decode(&bytes).unwrap(), bound);
    // Each storage byte with a basis that names the other is refused.
    let mut unfollowed = bytes.clone();
    unfollowed[at] = 0;
    assert_eq!(
        Plan::decode(&unfollowed).unwrap_err(),
        "plan storage does not follow its basis"
    );
    let mut unfollowed = unencrypted.encode();
    unfollowed[at] = 1;
    assert_eq!(
        Plan::decode(&unfollowed).unwrap_err(),
        "plan storage does not follow its basis"
    );
    for code in 2..=255 {
        let mut other = bytes.clone();
        other[at] = code;
        assert_eq!(
            Plan::decode(&other).unwrap_err(),
            "invalid plan storage",
            "{code}"
        );
    }
}

/// The basis is one byte after the storage byte: bit 0 the TPM probe, bit
/// 1 the keyboard-console probe, every other bit refused.
#[test]
fn the_basis_is_one_byte_after_the_storage_byte() {
    let at = 8 + 32 + 32 + 16 + 4 + 4 + 8 + 8 + 4 + 1 + 1;
    for (tpm, console, code) in [
        (false, false, 0),
        (true, false, 1),
        (false, true, 2),
        (true, true, 3),
    ] {
        let basis = Basis::new(tpm, console);
        assert_eq!((basis.tpm(), basis.keyboard_console()), (tpm, console));
        let p = Plan::new([1; 32], destination(), [2; 32], uuid(), basis, settings()).unwrap();
        let bytes = p.encode();
        assert_eq!(bytes[at], code);
        assert_eq!(bytes[at - 1], u8::from(code == 3));
        let decoded = Plan::decode(&bytes).unwrap();
        assert_eq!(decoded, p);
        assert_eq!(decoded.basis(), basis);
        assert_eq!(decoded.storage(), basis.storage());
        assert_eq!(p == plan(), code == 0);
        for other in 4..=255 {
            let mut other_bytes = bytes.clone();
            other_bytes[at] = other;
            assert_eq!(
                Plan::decode(&other_bytes).unwrap_err(),
                "invalid plan storage basis",
                "{other}"
            );
        }
    }
}

#[test]
fn the_records_before_the_basis_are_refused() {
    let removable = 8 + 32 + 32 + 16 + 4 + 4 + 8 + 8 + 4;
    // A TDPLAN02 record: the same fields with no basis byte.
    let mut old = plan().encode();
    old.remove(removable + 2);
    old[7] = b'2';
    assert_eq!(
        Plan::decode(&old).unwrap_err(),
        "unsupported installation plan version"
    );
    // Under the current magic its missing byte misreads the name length.
    old[7] = b'3';
    assert!(Plan::decode(&old).is_err());
    // A TDPLAN01 record: no storage byte either.
    let mut older = plan().encode();
    older.drain(removable + 1..removable + 3);
    older[7] = b'1';
    assert_eq!(
        Plan::decode(&older).unwrap_err(),
        "unsupported installation plan version"
    );
    older[7] = b'3';
    assert!(Plan::decode(&older).is_err());
}

#[test]
fn record_is_canonical_under_every_single_byte_change() {
    let bytes = plan().encode();
    for offset in 0..bytes.len() {
        for replacement in 0..=255 {
            let mut changed = bytes.clone();
            changed[offset] = replacement;
            if let Ok(decoded) = Plan::decode(&changed) {
                assert_eq!(decoded.encode(), changed, "offset {offset}");
                assert_eq!(decoded == plan(), changed == bytes, "offset {offset}");
            }
        }
    }
}

#[test]
fn retained_plan_owns_settings_and_labels() {
    let mut label = String::from("original");
    let mut username = String::from("alice");
    let d = observed_destination(
        "sda",
        (8, 0),
        1,
        (6 << 30, 512),
        true,
        (None, Some(&label), Some("wwid")),
    )
    .unwrap();
    let s = Settings::new(&username, "host", "us", "Etc/UTC").unwrap();
    let p = Plan::new([1; 32], d, [2; 32], uuid(), Basis::default(), s).unwrap();
    let original = p.encode();
    label.clear();
    username.clear();
    assert_eq!(p.encode(), original);
    assert_eq!(p.destination().serial(), Some("original"));
    assert_eq!(p.settings().username(), "alice");
    assert_eq!(Plan::decode(&original).unwrap(), p);
}

#[test]
fn boundaries_and_missing_labels_remain_distinct() {
    let long = "x".repeat(256);
    let d = observed_destination(
        &"s".repeat(64),
        (u32::MAX, u32::MAX),
        u64::MAX,
        (u64::MAX - 511, 512),
        true,
        (Some(&long), Some(&long), Some(&long)),
    )
    .unwrap();
    let s = Settings::new(
        &"a".repeat(32),
        &"h".repeat(63),
        &"k".repeat(64),
        &"z".repeat(64),
    )
    .unwrap();
    let p = Plan::new([255; 32], d, [0; 32], uuid(), Basis::new(true, true), s).unwrap();
    assert_eq!(p.encode().len(), 1193);
    assert!(p.encode().len() <= MAX_BYTES);
    assert_eq!(Plan::decode(&p.encode()).unwrap(), p);
    for model in [
        None,
        Some(""),
        Some(" "),
        Some("model"),
        Some("é"),
        Some("a\0b"),
        Some("\u{202e}"),
    ] {
        let d = observed_destination("vda", (254, 0), 1, (512, 512), false, (model, None, None))
            .unwrap();
        let p = Plan::new([1; 32], d, [2; 32], uuid(), Basis::default(), settings()).unwrap();
        assert_eq!(
            Plan::decode(&p.encode()).unwrap().destination().model(),
            model
        );
    }
}

#[test]
fn malformed_observations_and_choices_refuse() {
    for (name, major, sequence, capacity, sector) in [
        ("", 8, 1, 512, 512),
        ("../sda", 8, 1, 512, 512),
        ("sda", 0, 1, 512, 512),
        ("sda", 8, 0, 512, 512),
        ("sda", 8, 1, 0, 512),
        ("sda", 8, 1, 513, 512),
        ("sda", 8, 1, 512, 0),
        ("sda", 8, 1, 512, 1024),
    ] {
        assert!(observed_destination(
            name,
            (major, 0),
            sequence,
            (capacity, sector),
            false,
            (None, None, None)
        )
        .is_err());
    }
    for value in [
        String::new(),
        "a b".into(),
        "a\nb".into(),
        "é".into(),
        "x".repeat(65),
    ] {
        assert!(Settings::new(&value, "host", "us", "Etc/UTC").is_err());
        assert!(Settings::new("alice", &value, "us", "Etc/UTC").is_err());
        assert!(Settings::new("alice", "host", &value, "Etc/UTC").is_err());
        assert!(Settings::new("alice", "host", "us", &value).is_err());
    }
    assert!(Plan::new(
        [0; 32],
        destination(),
        [2; 32],
        uuid(),
        Basis::default(),
        settings()
    )
    .is_err());
    for (offset, byte) in [(6, 0x37), (8, 0x49)] {
        let mut bad = uuid();
        bad[offset] = byte;
        assert!(Plan::new(
            [1; 32],
            destination(),
            [2; 32],
            bad,
            Basis::default(),
            settings()
        )
        .is_err());
    }
}

#[test]
fn one_byte_over_each_field_bound_refuses() {
    assert!(observed_destination(
        &"s".repeat(65),
        (8, 0),
        1,
        (512, 512),
        false,
        (None, None, None)
    )
    .is_err());
    let long = "x".repeat(257);
    for labels in [
        (Some(long.as_str()), None, None),
        (None, Some(long.as_str()), None),
        (None, None, Some(long.as_str())),
    ] {
        assert!(observed_destination("sda", (8, 0), 1, (512, 512), false, labels).is_err());
    }
    assert!(Settings::new(&"a".repeat(33), "host", "us", "Etc/UTC").is_err());
    assert!(Settings::new("alice", &"h".repeat(64), "us", "Etc/UTC").is_err());
    assert!(Settings::new("alice", "host", &"k".repeat(65), "Etc/UTC").is_err());
    assert!(Settings::new("alice", "host", "us", &"z".repeat(65)).is_err());
}

#[test]
fn generated_uuid_text_maps_to_the_plan_network_byte_order() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_td-install"))
        .arg("new-volume-uuid")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let text = std::str::from_utf8(&output.stdout).unwrap();
    assert_eq!(text.len(), 37);
    let compact = text.strip_suffix('\n').unwrap().replace('-', "");
    assert_eq!(compact.len(), 32);
    let mut uuid = [0; 16];
    for (slot, pair) in uuid.iter_mut().zip(compact.as_bytes().chunks_exact(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    let p = Plan::new(
        [1; 32],
        destination(),
        [2; 32],
        uuid,
        Basis::default(),
        settings(),
    )
    .unwrap();
    assert_eq!(p.volume_uuid(), &uuid);
    assert_eq!(Plan::decode(&p.encode()).unwrap(), p);
}
