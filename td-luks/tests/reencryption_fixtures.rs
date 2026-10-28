//! Stock reencryption prefixes: format/crypto evidence without AF_ALG or QEMU.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use std::io::Cursor;
use td_json::Json;
use td_luks::{header, keyslot};
#[path = "../../td-encoding/src/lib.rs"]
#[allow(dead_code, reason = "fixture keyslot decoding uses base64 alone")]
mod encoding;
#[path = "../fixtures/replay.rs"]
#[allow(dead_code, reason = "log capture is used by the standalone generator")]
mod replay;
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";
const CASES: &[(&str, &[u8], &[u8])] = &[
    (
        "checksum",
        include_bytes!("../fixtures/reencrypt/checksum.base"),
        include_bytes!("../fixtures/reencrypt/checksum.trace"),
    ),
    (
        "journal",
        include_bytes!("../fixtures/reencrypt/journal.base"),
        include_bytes!("../fixtures/reencrypt/journal.trace"),
    ),
];
fn number(value: &Json, field: &str) -> usize {
    value.get(field).unwrap().as_str().unwrap().parse().unwrap()
}
fn has_flag(value: &Json, flag: &str) -> bool {
    value
        .get("flags")
        .and_then(Json::as_array)
        .is_some_and(|flags| flags.iter().any(|f| f.as_str() == Some(flag)))
}
fn b64(value: &Json, field: &str) -> Vec<u8> {
    encoding::from_base64(value.get(field).unwrap().as_str().unwrap()).unwrap()
}
fn keys(image: &[u8], json: &Json) {
    for (slot, start) in [("0", 0), ("1", 64)] {
        let expected: [u8; 64] = std::array::from_fn(|i| (i + start) as u8);
        let s = json.get_path(&["keyslots", slot]).unwrap();
        let kdf = s.get("kdf").unwrap();
        let iterations = kdf.get("iterations").unwrap().as_u64().unwrap() as u32;
        let key = keyslot::area_key(KEY, &b64(kdf, "salt"), iterations).unwrap();
        let offset = number(s.get("area").unwrap(), "offset");
        let mut split = image[offset..offset + keyslot::SPLIT_LEN].to_vec();
        keyslot::open(&key, &mut split).unwrap();
        let volume = keyslot::merge(&split).unwrap();
        assert_eq!(volume.bytes(), &expected);
        let digest = json
            .get("digests")
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, d)| {
                d.get("keyslots")
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v.as_str() == Some(slot))
            })
            .unwrap()
            .1
            .clone();
        let count = digest.get("iterations").unwrap().as_u64().unwrap() as u32;
        assert_eq!(
            keyslot::digest(&volume, &b64(&digest, "salt"), count)
                .unwrap()
                .as_slice(),
            b64(&digest, "digest")
        );
    }
}
fn ciphertext(start: u8) -> Vec<u8> {
    let key: [u8; 64] = std::array::from_fn(|i| start + i as u8);
    let mut bytes = vec![0x5a; replay::DATA_LEN];
    // The profile omits iv_large_sectors: plain64 numbers 512-byte
    // sectors, even though one encryption data unit contains 4096 bytes.
    for (i, unit) in bytes.as_chunks_mut::<4096>().0.iter_mut().enumerate() {
        td_fido::fido_aes::xts_encrypt(&key, (i * 8) as u64, 4096, unit).unwrap();
    }
    bytes
}
#[test]
fn every_stock_write_prefix_has_consistent_headers_keys_and_data() {
    let old = ciphertext(0);
    let new = ciphertext(64);
    for (mode, base, trace) in CASES {
        let mut image = replay::initial(base).unwrap();
        let initial = header::read_copy(&mut Cursor::new(&image))
            .unwrap()
            .json()
            .unwrap()
            .0;
        keys(&image, &initial);
        let writes = replay::decode(trace).unwrap();
        let mut dirty_count = 0;
        let mut mixed_count = 0;
        let mut unsafe_end_count = 0;
        for cut in 0..=writes.len() {
            if let Some(write) = cut.checked_sub(1).and_then(|i| writes.get(i)) {
                replay::apply(&mut image, write).unwrap();
            }
            let copy = header::read_copy(&mut Cursor::new(&image)).unwrap();
            assert_eq!(copy.identity.uuid, "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a");
            let json = copy.json().unwrap().0;
            assert_eq!(
                json.get_path(&["config", "requirements", "mandatory"]),
                Some(&Json::Arr(vec![Json::from("online-reencrypt-v2")]))
            );
            let area = json.get_path(&["keyslots", "2", "area"]).unwrap();
            assert_eq!(area.get("type").unwrap().as_str(), Some(*mode));
            assert_eq!(number(area, "offset"), replay::PREFIX_LEN);
            let segments = json
                .get("segments")
                .unwrap()
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, s)| !has_flag(s, "backup-previous") && !has_flag(s, "backup-final"))
                .collect::<Vec<_>>();
            let mut coverage = 0;
            for (id, s) in &segments {
                let offset = number(s, "offset") - replay::DATA_OFFSET;
                let len = if s.get("size").unwrap().as_str() == Some("dynamic") {
                    replay::DATA_LEN - offset
                } else {
                    number(s, "size")
                };
                assert_eq!(offset, coverage, "{mode} cut {cut}");
                coverage += len;
                let owns = |digest| {
                    json.get_path(&["digests", digest, "segments"])
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v.as_str() == Some(id))
                };
                let old_owns = owns("0");
                let new_owns = owns("1");
                let dirty = has_flag(s, "in-reencryption");
                assert_eq!(dirty, old_owns && new_owns);
                assert!(old_owns || new_owns);
                let mut old_blocks = 0;
                let mut new_blocks = 0;
                for at in (offset..offset + len).step_by(4096) {
                    let actual = &image[replay::DATA_OFFSET + at..replay::DATA_OFFSET + at + 4096];
                    let is_old = actual == &old[at..at + 4096];
                    let is_new = actual == &new[at..at + 4096];
                    assert!(is_old ^ is_new, "{mode} cut {cut} data unit {at}");
                    assert!(
                        (old_owns && is_old) || (new_owns && is_new),
                        "{mode} cut {cut} wrong segment key"
                    );
                    old_blocks += usize::from(is_old);
                    new_blocks += usize::from(is_new);
                }
                if dirty {
                    dirty_count += 1;
                    mixed_count += usize::from(old_blocks > 0 && new_blocks > 0);
                    let resilience = number(area, "offset");
                    if *mode == "checksum" {
                        assert_eq!(area.get("hash").unwrap().as_str(), Some("sha256"));
                        for (i, block) in old[offset..offset + len]
                            .as_chunks::<4096>()
                            .0
                            .iter()
                            .enumerate()
                        {
                            assert_eq!(
                                &image[resilience + i * 32..resilience + (i + 1) * 32],
                                td_fido::digest(block)
                            );
                        }
                    } else {
                        assert_eq!(
                            &image[resilience..resilience + len],
                            &old[offset..offset + len]
                        );
                    }
                }
                if segments.len() == 1 && new_owns && !dirty {
                    unsafe_end_count += 1;
                }
            }
            assert_eq!(coverage, replay::DATA_LEN);
        }
        assert!(
            dirty_count > 0 && mixed_count > 0 && unsafe_end_count > 0,
            "{mode}: checkpoint classes missing"
        );
    }
}
#[test]
fn truncated_or_out_of_range_trace_records_are_refused() {
    let valid = CASES[0].2;
    for len in [0, 7, 9, 15, valid.len() - 1] {
        assert!(replay::decode(&valid[..len]).is_err());
    }
    let mut bad = valid.to_vec();
    bad[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(replay::decode(&bad).is_err());
    let mut bad = valid.to_vec();
    bad[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(replay::decode(&bad).is_err());
}

#[test]
fn reencryption_digest_matches_every_stock_prefix() {
    use td_luks::reencrypt::{self, Area, Resilience};
    let old = keyslot::Key::from_bytes(&std::array::from_fn(|i| i as u8));
    let new = keyslot::Key::from_bytes(&std::array::from_fn(|i| i as u8 + 64));
    let wrong = keyslot::Key::from_bytes(&[0xff; 64]);
    for (mode, base, trace) in CASES {
        let mut image = replay::initial(base).unwrap();
        let writes = replay::decode(trace).unwrap();
        let mut checked = 0;
        for cut in 0..=writes.len() {
            if let Some(write) = cut.checked_sub(1).and_then(|i| writes.get(i)) {
                replay::apply(&mut image, write).unwrap();
            }
            let json = header::read_copy(&mut Cursor::new(&image))
                .unwrap()
                .json()
                .unwrap()
                .0;
            let admitted = reencrypt::profile(&json, replay::DATA_LEN as u64);
            let clean_all_new = json
                .get("segments")
                .unwrap()
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, s)| !has_flag(s, "backup-previous") && !has_flag(s, "backup-final"))
                .count()
                == 1
                && json
                    .get_path(&["digests", "1", "segments"])
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s.as_str() == Some("0"))
                && !has_flag(
                    json.get_path(&["segments", "0"]).unwrap(),
                    "in-reencryption",
                );
            if clean_all_new {
                assert!(
                    admitted.unwrap_err().contains("all-new"),
                    "{mode} cut {cut}"
                );
            } else {
                let layout = admitted.unwrap_or_else(|e| panic!("{mode} cut {cut}: {e}"));
                assert_eq!(layout.data_size, replay::DATA_LEN as u64);
                assert_eq!(layout.old_digest, 0);
                assert_eq!(layout.new_digest, 1);
                assert_eq!(layout.reencrypt_slot, 2);
                assert!(layout.progress < layout.data_size);
            }
            // The typed serializer represents only these fixed backups.
            for flag in ["backup-previous", "backup-final"] {
                let (_, segment) = json
                    .get("segments")
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .iter()
                    .find(|(_, s)| has_flag(s, flag))
                    .unwrap();
                assert_eq!(segment.get("type").unwrap().as_str(), Some("crypt"));
                assert_eq!(number(segment, "offset"), 16 << 20);
                assert_eq!(segment.get("size").unwrap().as_str(), Some("dynamic"));
                assert_eq!(number(segment, "iv_tweak"), 0);
                assert_eq!(
                    segment.get("encryption").unwrap().as_str(),
                    Some("aes-xts-plain64")
                );
                assert_eq!(segment.get("sector_size").unwrap().as_u64(), Some(4096));
            }
            let slot = json.get_path(&["keyslots", "2"]).unwrap();
            assert_eq!(slot.get("mode").unwrap().as_str(), Some("reencrypt"));
            assert_eq!(slot.get("direction").unwrap().as_str(), Some("forward"));
            let area_json = slot.get("area").unwrap();
            let area = Area {
                resilience: if *mode == "checksum" {
                    Resilience::Checksum
                } else {
                    Resilience::Journal
                },
                offset: number(area_json, "offset") as u64,
                size: number(area_json, "size") as u64,
            };
            let digest = json
                .get("digests")
                .unwrap()
                .as_object()
                .unwrap()
                .iter()
                .map(|(_, d)| d)
                .find(|d| {
                    d.get("keyslots")
                        .unwrap()
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|s| s.as_str() == Some("2"))
                })
                .unwrap();
            let salt: [u8; 32] = b64(digest, "salt").try_into().unwrap();
            let count = digest.get("iterations").unwrap().as_u64().unwrap() as u32;
            let expected = b64(digest, "digest");
            assert_eq!(
                reencrypt::digest(&old, &new, area, &salt, count)
                    .unwrap()
                    .as_slice(),
                expected,
                "{mode} cut {cut}"
            );
            if cut == 0 {
                for (a, b) in [(&new, &old), (&wrong, &new), (&old, &wrong)] {
                    assert_ne!(
                        reencrypt::digest(a, b, area, &salt, count)
                            .unwrap()
                            .as_slice(),
                        expected
                    );
                }
                let changed = Area {
                    size: area.size - 4096,
                    ..area
                };
                assert_ne!(
                    reencrypt::digest(&old, &new, changed, &salt, count)
                        .unwrap()
                        .as_slice(),
                    expected
                );
                // D1 admits no new writer or activation profile yet.
                assert!(td_luks::write::profile(&json).is_err());
            }
            checked += 1;
        }
        assert_eq!(checked, if *mode == "checksum" { 58 } else { 48 });
    }
}

#[test]
fn reencryption_profile_refuses_unsupported_or_inconsistent_layouts() {
    use td_luks::reencrypt;
    fn replace(json: &mut Json, path: &[&str], value: Json) {
        let (last, parents) = path.split_last().unwrap();
        let mut at = json;
        for name in parents {
            at = at.get_mut(name).unwrap();
        }
        let _ = at.insert(*last, value);
    }
    for (_, base, trace) in CASES {
        let mut image = replay::initial(base).unwrap();
        let initial = header::read_copy(&mut Cursor::new(&image))
            .unwrap()
            .json()
            .unwrap()
            .0;
        for size in [0, 1, replay::DATA_LEN as u64 + 1, u64::MAX - 4095] {
            assert!(reencrypt::profile(&initial, size).is_err());
        }
        for (path, value) in [
            (
                &["config", "requirements", "mandatory"][..],
                Json::Arr(vec![Json::from("online-reencrypt-v3")]),
            ),
            (&["config", "flags"][..], Json::Arr(vec![])),
            (&["keyslots", "2", "mode"][..], Json::from("encrypt")),
            (&["keyslots", "2", "direction"][..], Json::from("backward")),
            (&["keyslots", "2", "priority"][..], Json::Num("1".into())),
            (&["keyslots", "2", "key_size"][..], Json::Num("64".into())),
            (&["keyslots", "2", "extra"][..], Json::from("ignored")),
            (
                &["keyslots", "2", "area", "offset"][..],
                Json::from("32768"),
            ),
            (&["keyslots", "2", "area", "size"][..], Json::from("4095")),
            (&["keyslots", "2", "area", "type"][..], Json::from("none")),
            (&["digests", "0", "hash"][..], Json::from("sha512")),
            (&["digests", "1", "keyslots"][..], Json::Arr(vec![])),
            (
                &["digests", "0", "segments"][..],
                Json::Arr(vec![Json::from("0"), Json::from("1"), Json::from("2")]),
            ),
            (&["segments", "0", "iv_tweak"][..], Json::from("8")),
            (&["segments", "0", "offset"][..], Json::from("16781312")),
            (
                &["segments", "2", "encryption"][..],
                Json::from("aes-cbc-plain64"),
            ),
            (
                &["segments", "2", "flags"][..],
                Json::Arr(vec![Json::from("backup-moved-segment")]),
            ),
        ] {
            let mut bad = initial.clone();
            replace(&mut bad, path, value);
            assert!(
                reencrypt::profile(&bad, replay::DATA_LEN as u64).is_err(),
                "{path:?}"
            );
        }
        let mut dirty = None;
        for write in replay::decode(trace).unwrap() {
            replay::apply(&mut image, &write).unwrap();
            let json = header::read_copy(&mut Cursor::new(&image))
                .unwrap()
                .json()
                .unwrap()
                .0;
            if reencrypt::profile(&json, replay::DATA_LEN as u64).is_ok_and(|p| p.hotzone.is_some())
            {
                dirty = Some(json);
                break;
            }
        }
        let dirty = dirty.unwrap();
        for (path, value) in [
            (&["segments", "0", "flags"][..], Json::Arr(vec![])),
            (&["segments", "0", "size"][..], Json::from("dynamic")),
            (&["segments", "1", "offset"][..], Json::from("16777216")),
            (&["segments", "1", "iv_tweak"][..], Json::from("0")),
            (
                &["digests", "1", "segments"][..],
                Json::Arr(vec![Json::from("3")]),
            ),
        ] {
            let mut bad = dirty.clone();
            replace(&mut bad, path, value);
            assert!(
                reencrypt::profile(&bad, replay::DATA_LEN as u64).is_err(),
                "dirty {path:?}"
            );
        }
    }
}
