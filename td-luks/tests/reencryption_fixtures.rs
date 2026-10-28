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
