//! Header transitions; the caller orders resilience and data syncs.
use super::{profile, Layout};
use crate::header::numbered;
use crate::write::{self, DATA_OFFSET, SECTOR_SIZE};
use td_json::{Json, ObjectBuilder};

/// td's bounded working hotzone, independent of stock's larger limit.
pub const HOTZONE_SIZE: u64 = 1024 * 1024;

fn section<'a>(json: &'a mut Json, name: &str) -> Result<&'a mut Json, String> {
    json.get_mut(name).ok_or_else(|| format!("missing {name}"))
}
fn segment(offset: u64, size: Option<u64>, dirty: bool) -> Json {
    let mut value = ObjectBuilder::new()
        .set("type", "crypt")
        .set("offset", (DATA_OFFSET + offset).to_string())
        .set(
            "size",
            size.map_or_else(|| "dynamic".into(), |n| n.to_string()),
        )
        .set("iv_tweak", (offset / 512).to_string())
        .set("encryption", write::CIPHER)
        .set("sector_size", SECTOR_SIZE)
        .build();
    if dirty {
        let _ = value.insert("flags", Json::Arr(vec![Json::from("in-reencryption")]));
    }
    value
}
fn layout(json: &Json, before: &Layout, progress: u64, dirty: Option<u64>) -> Result<Json, String> {
    let mut result = json.clone();
    let mut segments = Vec::new();
    let mut old = Vec::new();
    let mut new = Vec::new();
    let final_state = progress == before.data_size;
    let mut add = |value: Json, is_old: bool, is_new: bool| {
        let id = segments.len().to_string();
        if is_old {
            old.push(Json::from(id.clone()));
        }
        if is_new {
            new.push(Json::from(id.clone()));
        }
        segments.push((id, value));
    };
    if progress != 0 {
        add(
            segment(0, if final_state { None } else { Some(progress) }, false),
            false,
            true,
        );
    }
    let end = if let Some(size) = dirty {
        add(segment(progress, Some(size), true), true, true);
        progress.checked_add(size).ok_or("hotzone overflow")?
    } else {
        progress
    };
    if end < before.data_size {
        add(segment(end, None, false), true, false);
    }
    if final_state {
        let _ = section(&mut result, "config")?.remove("requirements");
        let _ = section(&mut result, "keyslots")?.remove(&before.reencrypt_slot.to_string());
        let _ = section(&mut result, "digests")?.remove(&before.reencrypt_digest.to_string());
    } else {
        for (flag, is_old, is_new) in [
            ("backup-previous", true, false),
            ("backup-final", false, true),
        ] {
            let mut backup = segment(0, None, false);
            let _ = backup.insert("flags", Json::Arr(vec![Json::from(flag)]));
            add(backup, is_old, is_new);
        }
    }
    let _ = result.insert("segments", Json::Obj(segments));
    for (id, _) in numbered(json, "digests")? {
        if final_state && id == before.reencrypt_digest {
            continue;
        }
        let value = if id == before.old_digest {
            old.clone()
        } else if id == before.new_digest {
            new.clone()
        } else {
            Vec::new()
        };
        let digest = section(&mut result, "digests")?
            .get_mut(&id.to_string())
            .ok_or("missing digest")?;
        let _ = digest.insert("segments", Json::Arr(value));
    }
    if final_state {
        write::profile(&result)?;
    } else {
        profile(&result, before.data_size)?;
    }
    Ok(result)
}

/// Mark the next hotzone dirty. Persist its resilience bytes before
/// committing this JSON; no data rewrite may precede that commit.
pub fn mark_hotzone(json: &Json, data_size: u64, size: u64) -> Result<Json, String> {
    let before = profile(json, data_size)?;
    if before.hotzone.is_some() {
        return Err("recover the existing hotzone first".into());
    }
    if size == 0
        || !size.is_multiple_of(SECTOR_SIZE)
        || size > HOTZONE_SIZE
        || before
            .progress
            .checked_add(size)
            .is_none_or(|end| end > data_size)
    {
        return Err("invalid next hotzone size".into());
    }
    layout(json, &before, before.progress, Some(size))
}

/// After rewriting and syncing the dirty hotzone, commit its completion.
/// The last zone goes straight to ordinary metadata, never to the unsafe
/// all-new layout that still carries the reencryption requirement.
/// This does not erase journal ciphertext: after committing the final
/// header, callers must wipe/sync free keyslot-area spans before claiming
/// old-key revocation (DESIGN.md "Start and end").
pub fn finish_hotzone(json: &Json, data_size: u64) -> Result<Json, String> {
    let before = profile(json, data_size)?;
    let zone = before.hotzone.ok_or("there is no dirty hotzone")?;
    let end = zone
        .offset
        .checked_add(zone.size)
        .ok_or("hotzone overflow")?;
    layout(json, &before, end, None)
}
