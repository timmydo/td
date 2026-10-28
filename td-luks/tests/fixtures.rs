//! Persistent cryptsetup 2.8.8 answers. No cryptsetup, AF_ALG or QEMU at test time.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use std::fs::{self, File, OpenOptions};
use td_fs::TempDir;
use td_json::Json;
use td_luks::header::{self, HeaderCopy};
use td_luks::write::{self, Error};

const RECOVERY: &[u8] = b"000000000000000000000000000000000000000000000000";
const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";
const FIXTURES: &[(&str, &[u8], &str)] = &[
    (
        "fresh",
        include_bytes!("../fixtures/fresh.bin"),
        include_str!("../fixtures/fresh.json"),
    ),
    (
        "added",
        include_bytes!("../fixtures/added.bin"),
        include_str!("../fixtures/added.json"),
    ),
    (
        "token",
        include_bytes!("../fixtures/token.bin"),
        include_str!("../fixtures/token.json"),
    ),
    (
        "killed",
        include_bytes!("../fixtures/killed.bin"),
        include_str!("../fixtures/killed.json"),
    ),
    (
        "unbound",
        include_bytes!("../fixtures/unbound.bin"),
        include_str!("../fixtures/unbound.json"),
    ),
];
fn device(dir: &TempDir, bytes: &[u8]) -> File {
    let path = dir.path().join("volume");
    fs::write(&path, bytes).unwrap();
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.set_len(32 << 20).unwrap();
    file
}
#[test]
fn stock_headers_and_af_keyslots_unlock_to_the_stock_volume_key() {
    let dir = TempDir::new("td-luks-fixtures").unwrap();
    let expected: Vec<u8> = (0u8..64).collect();
    for (name, bytes, dump) in FIXTURES {
        let mut file = device(&dir, bytes);
        let header = write::load(&mut file).unwrap();
        assert_eq!(
            header.uuid, "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a",
            "{name}"
        );
        assert_eq!(header.label, "td-system", "{name}");
        assert_eq!(
            header.json,
            header::metadata_json(dump.as_bytes()).unwrap(),
            "{name}"
        );
        assert_eq!(
            write::volume_key(&mut file, Some(0), RECOVERY)
                .unwrap()
                .bytes(),
            expected.as_slice(),
            "{name}"
        );
        assert_eq!(
            write::test(&mut file, 0, b"incorrect"),
            Err(Error::WrongKey),
            "{name}"
        );
        let second = matches!(*name, "added" | "token" | "unbound");
        assert_eq!(header.json.get_path(&["keyslots", "1"]).is_some(), second);
        if second {
            assert_eq!(
                write::volume_key(&mut file, Some(1), SECOND)
                    .unwrap()
                    .bytes(),
                expected.as_slice(),
                "{name}"
            );
            assert_eq!(
                write::test(&mut file, 1, b"incorrect"),
                Err(Error::WrongKey)
            );
        } else {
            assert!(write::test(&mut file, 1, SECOND).is_err());
        }
        assert_eq!(
            write::unbound(&header.json).unwrap(),
            if *name == "unbound" { vec![1] } else { vec![] }
        );
        match *name {
            "token" => assert_eq!(
                header.json.get_path(&["tokens", "0", "keyslots"]),
                Some(&Json::Arr(vec![Json::from("1")]))
            ),
            "killed" | "unbound" => assert_eq!(
                header.json.get_path(&["tokens", "0", "keyslots"]),
                Some(&Json::Arr(vec![]))
            ),
            _ => assert_eq!(header.json.get("tokens"), Some(&Json::Obj(vec![]))),
        }
        // Every live keyslot area is present in the captured prefix.
        for slot in ["0", "1"] {
            if let Some(area) = header.json.get_path(&["keyslots", slot, "area"]) {
                let value = |field| {
                    area.get(field)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                };
                assert!(value("offset") + value("size") <= bytes.len());
            }
        }
        assert_eq!(
            &fs::read(dir.path().join("volume")).unwrap()[..bytes.len()],
            *bytes,
            "read-only operations changed {name}"
        );
    }
}
#[test]
fn either_stock_copy_survives_loss_of_the_other() {
    let dir = TempDir::new("td-luks-fixture-copies").unwrap();
    for (name, bytes, dump) in FIXTURES {
        for (lost, used) in [(0, HeaderCopy::Secondary), (16384, HeaderCopy::Primary)] {
            let mut damaged = bytes.to_vec();
            damaged[lost] ^= 1;
            let mut file = device(&dir, &damaged);
            let header = write::load(&mut file).unwrap();
            assert_eq!(header.used, used, "{name}");
            assert_eq!(header.json, header::metadata_json(dump.as_bytes()).unwrap());
            write::test(&mut file, 0, RECOVERY).unwrap();
        }
    }
}
