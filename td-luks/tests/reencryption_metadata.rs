#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
use std::io;
use td_luks::write::Medium;
use td_luks::{header, keyslot, reencrypt, write};
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";
struct Random(u8);
impl keyslot::Random for Random {
    fn fill(&mut self, out: &mut [u8]) -> io::Result<()> {
        for byte in out {
            self.0 = self.0.wrapping_add(17);
            *byte = self.0;
        }
        Ok(())
    }
}
#[derive(Clone)]
struct Memory {
    bytes: Vec<u8>,
    mutations: usize,
    fail_at: usize,
}
impl Medium for Memory {
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> io::Result<usize> {
        let bytes = self
            .bytes
            .get(offset as usize..)
            .ok_or_else(|| io::Error::other("offset"))?;
        let len = bytes.len().min(out.len());
        out[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        self.step()?;
        self.bytes
            .get_mut(offset as usize..offset as usize + bytes.len())
            .ok_or_else(|| io::Error::other("write bounds"))?
            .copy_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.step()
    }
    fn size(&mut self) -> io::Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}
impl Memory {
    fn new() -> Self {
        let mut bytes = include_bytes!("../fixtures/fresh.bin").to_vec();
        bytes.resize(write::DATA_OFFSET as usize + 2 * 1024 * 1024, 0);
        Self {
            bytes,
            mutations: 0,
            fail_at: usize::MAX,
        }
    }
    fn step(&mut self) -> io::Result<()> {
        if self.mutations == self.fail_at {
            return Err(io::Error::other("cut"));
        }
        self.mutations += 1;
        Ok(())
    }
}
#[test]
fn atomic_start_preserves_an_unlockable_header_at_every_mutation() {
    for mode in [
        reencrypt::Resilience::Checksum,
        reencrypt::Resilience::Journal,
    ] {
        let mut medium = Memory::new();
        let (plan, keys) =
            reencrypt::plan_start(&mut medium, &mut Random(7), 0, KEY, mode).unwrap();
        assert_eq!(medium.mutations, 0);
        assert_eq!(keys.old.bytes(), &std::array::from_fn(|i| i as u8));
        let mut success = medium.clone();
        reencrypt::commit(&mut success, &mut Random(19), &plan).unwrap();
        for cut in 0..=success.mutations {
            let mut image = medium.clone();
            image.fail_at = cut;
            let result = reencrypt::commit(&mut image, &mut Random(19), &plan);
            assert_eq!(result.is_ok(), cut == success.mutations);
            if let Ok((header, layout)) = reencrypt::load(&mut image) {
                let (again, opened) = reencrypt::unlock(&mut image, &header, KEY).unwrap();
                assert_eq!(layout, again);
                assert_eq!(opened.old.bytes(), keys.old.bytes());
                assert_eq!(opened.new.bytes(), keys.new.bytes());
            } else {
                write::load(&mut image).unwrap();
                write::activation(
                    &mut image,
                    write::ActivationKey::Passphrase {
                        slot: Some(0),
                        bytes: KEY,
                    },
                )
                .unwrap();
            }
        }
        let (header, _) = reencrypt::load(&mut success).unwrap();
        assert!(matches!(
            reencrypt::unlock(&mut success, &header, b"wrong"),
            Err(write::Error::WrongKey)
        ));
        let mut changed = header.clone();
        let digest = changed
            .json
            .get_mut("digests")
            .unwrap()
            .get_mut("2")
            .unwrap();
        let _ = digest.insert(
            "digest",
            td_json::Json::from("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
        );
        assert!(matches!(
            reencrypt::unlock(&mut success, &changed, KEY),
            Err(write::Error::Refused(_))
        ));
    }
}
#[test]
fn final_header_commit_keeps_last_dirty_or_finished_metadata() {
    for mode in [
        reencrypt::Resilience::Checksum,
        reencrypt::Resilience::Journal,
    ] {
        let mut medium = Memory::new();
        let (mut plan, _) =
            reencrypt::plan_start(&mut medium, &mut Random(7), 0, KEY, mode).unwrap();
        let size = 2 * 1024 * 1024;
        plan.json = reencrypt::mark_hotzone(&plan.json, size, 1024 * 1024).unwrap();
        plan.json = reencrypt::finish_hotzone(&plan.json, size).unwrap();
        plan.json = reencrypt::mark_hotzone(&plan.json, size, 1024 * 1024).unwrap();
        reencrypt::commit(&mut medium, &mut Random(19), &plan).unwrap();
        plan.json = reencrypt::finish_hotzone(&plan.json, size).unwrap();
        plan.seqid += 1;
        plan.areas.clear();
        medium.mutations = 0;
        for cut in 0..=8 {
            let mut image = medium.clone();
            image.fail_at = cut;
            let result = reencrypt::commit(&mut image, &mut Random(19), &plan);
            assert_eq!(result.is_ok(), cut == 8);
            let json = header::read_copy(&mut std::io::Cursor::new(&image.bytes))
                .unwrap()
                .json()
                .unwrap()
                .0;
            if json.get_path(&["config", "requirements"]).is_some() {
                assert!(reencrypt::profile(&json, size).unwrap().hotzone.is_some());
            } else {
                write::profile(&json).unwrap();
            }
        }
    }
}

#[test]
fn start_refusals_write_nothing_and_tokens_follow_the_replacement_slot() {
    use td_json::{Json, ObjectBuilder};
    let mut medium = Memory::new();
    assert!(matches!(
        reencrypt::plan_start(
            &mut medium,
            &mut Random(7),
            0,
            b"wrong",
            reencrypt::Resilience::Checksum
        ),
        Err(write::Error::WrongKey)
    ));
    assert_eq!(medium.mutations, 0);
    let original = write::load(&mut medium).unwrap();
    let mut plan = write::Plan {
        uuid: original.uuid,
        label: original.label,
        seqid: original.seqid + 1,
        json: original.json,
        used: original.used,
        areas: vec![],
    };
    let _ = plan
        .json
        .get_mut("keyslots")
        .unwrap()
        .get_mut("0")
        .unwrap()
        .insert("priority", Json::Num("0".into()));
    write::commit(&mut medium, &mut Random(19), &plan).unwrap();
    medium.mutations = 0;
    assert!(matches!(
        reencrypt::plan_start(
            &mut medium,
            &mut Random(7),
            0,
            KEY,
            reencrypt::Resilience::Checksum
        ),
        Err(write::Error::Refused(_))
    ));
    assert_eq!(medium.mutations, 0);
    let _ = plan
        .json
        .get_mut("keyslots")
        .unwrap()
        .get_mut("0")
        .unwrap()
        .remove("priority");
    let _ = plan.json.get_mut("tokens").unwrap().insert(
        "4",
        ObjectBuilder::new()
            .set("type", "foreign-token")
            .set("keyslots", Json::Arr(vec![Json::from("0")]))
            .set("opaque", "preserved")
            .build(),
    );
    plan.seqid += 1;
    write::commit(&mut medium, &mut Random(19), &plan).unwrap();
    let (start, _) = reencrypt::plan_start(
        &mut medium,
        &mut Random(7),
        0,
        KEY,
        reencrypt::Resilience::Checksum,
    )
    .unwrap();
    assert_eq!(
        start.json.get_path(&["tokens", "4", "keyslots"]),
        Some(&Json::Arr(vec![Json::from("1")]))
    );
    assert_eq!(
        start.json.get_path(&["tokens", "4", "opaque"]),
        Some(&Json::from("preserved"))
    );
}
