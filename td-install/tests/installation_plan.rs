#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_install::installation_plan::{
    Destination, DestinationObservation, Plan, Settings, MAX_BYTES,
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
    Plan::new([1; 32], destination(), [2; 32], uuid(), settings()).unwrap()
}

#[test]
fn deployment_id_requires_exact_lowercase_manifest_digest() {
    let p = Plan::new([1; 32], destination(), [0xab; 32], uuid(), settings()).unwrap();
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
    let mut bytes = b"TDPLAN01".to_vec();
    bytes.extend([1; 32]);
    bytes.extend([2; 32]);
    bytes.extend(uuid());
    bytes.extend([0, 0, 1, 3, 0, 0, 0, 3]);
    bytes.extend([0, 0, 0, 0, 0, 0, 0, 27]);
    bytes.extend([0, 0, 0, 1, 128, 0, 0, 0]);
    bytes.extend([0, 0, 16, 0, 0]);
    bytes.extend(b"\0\x07nvme0n1\x01\0\x07Model A\x01\0\x09serial-42\0");
    bytes.extend(b"\0\x05alice\0\x09td-laptop\0\x02us\0\x07Etc/UTC");
    assert_eq!(p.encode(), bytes);
    assert_eq!(Plan::decode(&bytes).unwrap(), p);
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
    let mut wrong_version = bytes;
    wrong_version[7] = b'2';
    assert!(Plan::decode(&wrong_version).is_err());
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
    let p = Plan::new([1; 32], d, [2; 32], uuid(), s).unwrap();
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
    let p = Plan::new([255; 32], d, [0; 32], uuid(), s).unwrap();
    assert_eq!(p.encode().len(), 1191);
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
        let p = Plan::new([1; 32], d, [2; 32], uuid(), settings()).unwrap();
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
    assert!(Plan::new([0; 32], destination(), [2; 32], uuid(), settings()).is_err());
    for (offset, byte) in [(6, 0x37), (8, 0x49)] {
        let mut bad = uuid();
        bad[offset] = byte;
        assert!(Plan::new([1; 32], destination(), [2; 32], bad, settings()).is_err());
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
    let p = Plan::new([1; 32], destination(), [2; 32], uuid, settings()).unwrap();
    assert_eq!(p.volume_uuid(), &uuid);
    assert_eq!(Plan::decode(&p.encode()).unwrap(), p);
}
