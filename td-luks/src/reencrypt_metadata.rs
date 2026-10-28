//! Authenticated reencryption header I/O and initialization.
use super::{digest, profile, Area, Layout, Resilience};
use crate::encoding::base64;
use crate::header::{numbered, MAX_DIGEST_ITERATIONS, MAX_SLOT};
use crate::keyslot::{self, Key, Random, ITERATIONS, SALT_LEN};
use crate::write::{self, Error, Header, Medium, Plan, DATA_OFFSET, KEYSLOTS_OFFSET, SECTOR_SIZE};
use td_json::{Json, ObjectBuilder};

pub struct Keys {
    pub old: Key,
    pub new: Key,
}
fn data_size<M: Medium + ?Sized>(medium: &mut M) -> Result<u64, Error> {
    medium
        .size()
        .map_err(|e| Error::Refused(format!("device size: {e}")))?
        .checked_sub(DATA_OFFSET)
        .ok_or_else(|| Error::Refused("device has no data segment".into()))
}
/// Load the in-progress profile from the held device. No recovery or write.
pub fn load<M: Medium + ?Sized>(medium: &mut M) -> Result<(Header, Layout), Error> {
    let header = write::load_raw(medium)?;
    let layout = profile(&header.json, data_size(medium)?)?;
    Ok((header, layout))
}
fn by_id<'a>(json: &'a Json, section: &str, id: u8) -> Result<&'a Json, Error> {
    json.get(section)
        .and_then(|s| s.get(&id.to_string()))
        .ok_or_else(|| Error::Refused(format!("missing {section} {id}")))
}
fn digest_key<M: Medium + ?Sized>(
    medium: &mut M,
    json: &Json,
    id: u8,
    passphrase: &[u8],
) -> Result<Key, Error> {
    let named = write::slots(by_id(json, "digests", id)?, "keyslots", "digest")?;
    let mut preferred = Vec::new();
    let mut normal = Vec::new();
    for slot in named {
        match by_id(json, "keyslots", slot)?
            .get("priority")
            .and_then(Json::as_u64)
        {
            Some(0) => {}
            Some(2) => preferred.push(slot),
            _ => normal.push(slot),
        }
    }
    preferred.sort_unstable();
    normal.sort_unstable();
    preferred.extend(normal);
    write::unlock_any(medium, json, passphrase, &preferred).map(|(_, key)| key)
}
/// Open both volume keys and authenticate the v2 verification digest.
/// The header must be held stable under its device/reencryption locks.
pub fn unlock<M: Medium + ?Sized>(
    medium: &mut M,
    header: &Header,
    passphrase: &[u8],
) -> Result<(Layout, Keys), Error> {
    let layout = profile(&header.json, data_size(medium)?)?;
    let old = digest_key(medium, &header.json, layout.old_digest, passphrase)?;
    let new = digest_key(medium, &header.json, layout.new_digest, passphrase)?;
    let stored = by_id(&header.json, "digests", layout.reencrypt_digest)?;
    let salt: [u8; SALT_LEN] = write::bytes_of(stored, "salt", SALT_LEN, "reencryption digest")?
        .try_into()
        .map_err(|_| "reencryption salt length")?;
    let count = write::iterations(stored, MAX_DIGEST_ITERATIONS, "reencryption digest")?;
    let answer = write::bytes_of(stored, "digest", keyslot::DIGEST_LEN, "reencryption digest")?;
    if !td_fido::same(&digest(&old, &new, layout.area, &salt, count)?, &answer) {
        return Err(Error::Refused(
            "reencryption verification digest does not match".into(),
        ));
    }
    Ok((layout, Keys { old, new }))
}
/// Commit a validated in-progress or finalized header using ordinary copy ordering.
/// The caller owns locks and must sync resilience/data before its transition.
pub fn commit<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    plan: &Plan,
) -> Result<(), Error> {
    if plan.json.get_path(&["config", "requirements"]).is_some() {
        profile(&plan.json, data_size(medium)?)?;
    } else {
        write::profile(&plan.json)?;
    }
    write::write_plan(medium, random, plan)
}
fn available(json: &Json, section: &str) -> Result<u8, Error> {
    for id in 0..=MAX_SLOT {
        if !write::slot_in_use(json, section, id)? {
            return Ok(id);
        }
    }
    Err(Error::Refused(format!("no free {section} number")))
}
fn digest_json(keyslots: Vec<Json>, salt: &[u8], value: &[u8]) -> Json {
    ObjectBuilder::new()
        .set("type", "pbkdf2")
        .set("hash", "sha256")
        .set("iterations", u64::from(ITERATIONS))
        .set("salt", base64(salt))
        .set("digest", base64(value))
        .set("keyslots", Json::Arr(keyslots))
        .set("segments", Json::Arr(vec![]))
        .build()
}
fn largest_area(json: &Json, resilience: Resilience) -> Result<Area, Error> {
    let mut spans = Vec::new();
    for (_, slot) in numbered(json, "keyslots")? {
        let a = slot.get("area").ok_or("keyslot has no area")?;
        let start = write::decimal(a, "offset", "area")?;
        let end = start
            .checked_add(write::decimal(a, "size", "area")?)
            .ok_or("area overflow")?;
        spans.push((start, end));
    }
    spans.sort_unstable();
    spans.push((DATA_OFFSET, DATA_OFFSET));
    let mut best = Area {
        resilience,
        offset: 0,
        size: 0,
    };
    let mut cursor = KEYSLOTS_OFFSET;
    for (start, end) in spans {
        let length = start.checked_sub(cursor).ok_or("keyslot areas overlap")?;
        if length > best.size {
            best.offset = cursor;
            best.size = length;
        }
        cursor = end;
    }
    best.validate()?;
    Ok(best)
}
/// Prepare one atomic start: preserve old slots, add a new volume key
/// wrapped under SLOT's passphrase, and create the requirement together.
/// No bytes are written until commit. The supplied RNG owns new key material.
pub fn plan_start<M: Medium + ?Sized>(
    medium: &mut M,
    random: &mut dyn Random,
    slot: u8,
    passphrase: &[u8],
    resilience: Resilience,
) -> Result<(Plan, Keys), Error> {
    let header = write::load(medium)?;
    let size = data_size(medium)?;
    if by_id(&header.json, "keyslots", slot)?
        .get("priority")
        .and_then(Json::as_u64)
        == Some(0)
    {
        return Err(Error::Refused(
            "reencryption needs an authorizing slot enabled for automatic resume".into(),
        ));
    }
    let old = write::unlock_bound(medium, &header.json, slot, passphrase)?;
    let old_digest = write::digest_of(&header.json, slot)?
        .ok_or("keyslot has no digest")?
        .0;
    let new = Key::random(random).map_err(|e| Error::Refused(format!("volume key: {e}")))?;
    if td_fido::same(old.bytes(), new.bytes()) {
        return Err(Error::Refused("new volume key equals old key".into()));
    }
    let mut plan = write::next(&header)?;
    let new_slot = available(&plan.json, "keyslots")?;
    let offset = write::free_area(&plan.json)?;
    let (slot_json, sealed) = write::new_keyslot(random, &new, passphrase, offset)?;
    let _ = write::section_mut(&mut plan.json, "keyslots")?.insert(new_slot.to_string(), slot_json);
    plan.areas.push((offset, sealed));
    let new_digest = available(&plan.json, "digests")?;
    let salt = write::salt(random)?;
    let value = keyslot::digest(&new, &salt, ITERATIONS)?;
    let _ = write::section_mut(&mut plan.json, "digests")?.insert(
        new_digest.to_string(),
        digest_json(vec![Json::from(new_slot.to_string())], &salt, &value),
    );
    let area = largest_area(&plan.json, resilience)?;
    let reencrypt_slot = available(&plan.json, "keyslots")?;
    let mut area_json = ObjectBuilder::new()
        .set("offset", area.offset.to_string())
        .set("size", area.size.to_string())
        .set(
            "type",
            if resilience == Resilience::Checksum {
                "checksum"
            } else {
                "journal"
            },
        )
        .build();
    if resilience == Resilience::Checksum {
        let _ = area_json.insert("hash", Json::from("sha256"));
        let _ = area_json.insert("sector_size", Json::Num(SECTOR_SIZE.to_string()));
    }
    let reencrypt = ObjectBuilder::new()
        .set("type", "reencrypt")
        .set("key_size", 1u64)
        .set("priority", 0u64)
        .set("mode", "reencrypt")
        .set("direction", "forward")
        .set("area", area_json)
        .build();
    let _ = write::section_mut(&mut plan.json, "keyslots")?
        .insert(reencrypt_slot.to_string(), reencrypt);
    let reencrypt_digest = available(&plan.json, "digests")?;
    let salt = write::salt(random)?;
    let value = digest(&old, &new, area, &salt, ITERATIONS)?;
    let _ = write::section_mut(&mut plan.json, "digests")?.insert(
        reencrypt_digest.to_string(),
        digest_json(vec![Json::from(reencrypt_slot.to_string())], &salt, &value),
    );
    let active = by_id(&plan.json, "segments", 0)?.clone();
    for (id, flag) in [(1, "backup-previous"), (2, "backup-final")] {
        let mut backup = active.clone();
        let _ = backup.insert("flags", Json::Arr(vec![Json::from(flag)]));
        let _ = write::section_mut(&mut plan.json, "segments")?.insert(id.to_string(), backup);
    }
    for (id, segments) in [
        (old_digest, vec![Json::from("0"), Json::from("1")]),
        (new_digest, vec![Json::from("2")]),
    ] {
        let value = write::section_mut(&mut plan.json, "digests")?
            .get_mut(&id.to_string())
            .ok_or("missing digest")?;
        let _ = value.insert("segments", Json::Arr(segments));
    }
    let requirements = ObjectBuilder::new()
        .set(
            "mandatory",
            Json::Arr(vec![Json::from("online-reencrypt-v2")]),
        )
        .build();
    let _ = write::section_mut(&mut plan.json, "config")?.insert("requirements", requirements);
    // Tokens on the selected recovery slot follow its replacement; other
    // old slots remain usable only until the eventual final transition.
    let Json::Obj(tokens) = write::section_mut(&mut plan.json, "tokens")? else {
        return Err("tokens not an object".into());
    };
    for (_, token) in tokens {
        let Some(Json::Arr(named)) = token.get_mut("keyslots") else {
            return Err("token keyslots not an array".into());
        };
        for name in named {
            if name.as_str() == Some(&slot.to_string()) {
                *name = Json::from(new_slot.to_string());
            }
        }
    }
    profile(&plan.json, size)?;
    Ok((plan, Keys { old, new }))
}
