//! Strict admission of the in-place, forward, two-key reencryption layout.
use super::{Area, Resilience};
use crate::header::numbered;
use crate::write::{
    self, decimal, exactly, number_is, slots, text_is, CIPHER, DATA_OFFSET, SECTOR_SIZE,
};
use td_json::Json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotzone {
    pub offset: u64,
    pub size: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub area: Area,
    pub old_digest: u8,
    pub new_digest: u8,
    pub reencrypt_slot: u8,
    pub reencrypt_digest: u8,
    pub data_size: u64,
    pub progress: u64,
    pub hotzone: Option<Hotzone>,
}
fn field<'a>(value: &'a Json, key: &str) -> Result<&'a Json, String> {
    value
        .get(key)
        .ok_or_else(|| format!("reencryption metadata has no {key}"))
}
fn flag(segment: &Json) -> Result<Option<&str>, String> {
    let Some(value) = segment.get("flags") else {
        return Ok(None);
    };
    let Some([flag]) = value.as_arr() else {
        return Err("reencryption segment must have one flag".into());
    };
    flag.as_str()
        .map(Some)
        .ok_or_else(|| "reencryption segment flag is not text".into())
}
fn segment_fields(segment: &Json) -> Result<(), String> {
    let fields: &[&str] = if segment.get("flags").is_some() {
        &[
            "type",
            "offset",
            "size",
            "iv_tweak",
            "encryption",
            "sector_size",
            "flags",
        ]
    } else {
        &[
            "type",
            "offset",
            "size",
            "iv_tweak",
            "encryption",
            "sector_size",
        ]
    };
    exactly(segment, fields, "reencryption segment")?;
    text_is(segment, "type", "crypt", "reencryption segment")?;
    text_is(segment, "encryption", CIPHER, "reencryption segment")?;
    number_is(segment, "sector_size", SECTOR_SIZE, "reencryption segment")
}
fn backup(segment: &Json, want: &str) -> Result<(), String> {
    segment_fields(segment)?;
    if flag(segment)? != Some(want)
        || decimal(segment, "offset", "backup")? != DATA_OFFSET
        || decimal(segment, "iv_tweak", "backup")? != 0
    {
        return Err("reencryption backup is not the fixed full-volume segment".into());
    }
    text_is(segment, "size", "dynamic", "reencryption backup")
}
fn bindings(json: &Json, segment: u8) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for (id, digest) in numbered(json, "digests")? {
        if slots(digest, "segments", "digest")?.contains(&segment) {
            out.push(id);
        }
    }
    Ok(out)
}
fn one_binding(json: &Json, segment: u8) -> Result<u8, String> {
    match bindings(json, segment)?.as_slice() {
        [id] => Ok(*id),
        _ => Err("reencryption backup must name exactly one digest".into()),
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Old,
    New,
    Dirty,
}

/// Validate the reencryption JSON against the opened device's data length.
/// This performs no I/O or key verification, and admits no clean all-new
/// layout still carrying the requirement: stock 2.8.8 corrupts that state
/// on resume. Ordinary writer/activation admission remains separate.
pub fn profile(json: &Json, data_size: u64) -> Result<Layout, String> {
    if data_size == 0
        || !data_size.is_multiple_of(SECTOR_SIZE)
        || DATA_OFFSET.checked_add(data_size).is_none()
    {
        return Err("reencryption data length is not positive whole sectors".into());
    }
    let config = field(json, "config")?;
    let requirements = field(config, "requirements")?;
    exactly(requirements, &["mandatory"], "reencryption requirements")?;
    if field(requirements, "mandatory")? != &Json::Arr(vec![Json::from("online-reencrypt-v2")]) {
        return Err("reencryption requires exactly online-reencrypt-v2".into());
    }
    let keyslots = numbered(json, "keyslots")?;
    let candidates: Vec<_> = keyslots
        .iter()
        .filter(|(_, k)| k.get("type").and_then(Json::as_str) == Some("reencrypt"))
        .collect();
    let [(reencrypt_slot, slot)] = candidates.as_slice() else {
        return Err("reencryption needs exactly one reencryption keyslot".into());
    };
    let reencrypt_slot = *reencrypt_slot;
    exactly(
        slot,
        &["area", "type", "key_size", "mode", "direction", "priority"],
        "reencryption keyslot",
    )?;
    number_is(slot, "key_size", 1, "reencryption keyslot")?;
    number_is(slot, "priority", 0, "reencryption keyslot")?;
    text_is(slot, "mode", "reencrypt", "reencryption keyslot")?;
    text_is(slot, "direction", "forward", "reencryption keyslot")?;
    let area_json = field(slot, "area")?;
    let resilience = match field(area_json, "type")?.as_str() {
        Some("checksum") => {
            exactly(
                area_json,
                &["offset", "size", "type", "hash", "sector_size"],
                "checksum area",
            )?;
            text_is(area_json, "hash", "sha256", "checksum area")?;
            number_is(area_json, "sector_size", SECTOR_SIZE, "checksum area")?;
            Resilience::Checksum
        }
        Some("journal") => {
            exactly(area_json, &["offset", "size", "type"], "journal area")?;
            Resilience::Journal
        }
        _ => return Err("unsupported reencryption resilience".into()),
    };
    let area = Area {
        resilience,
        offset: decimal(area_json, "offset", "resilience area")?,
        size: decimal(area_json, "size", "resilience area")?,
    };
    area.validate()?;
    let area_end = area
        .offset
        .checked_add(area.size)
        .ok_or("resilience area overflow")?;
    for (id, keyslot) in &keyslots {
        if *id == reencrypt_slot {
            continue;
        }
        let a = field(keyslot, "area")?;
        let start = decimal(a, "offset", "keyslot area")?;
        let end = start
            .checked_add(decimal(a, "size", "keyslot area")?)
            .ok_or("keyslot area overflow")?;
        if start < area_end && area.offset < end {
            return Err("resilience area overlaps a normal keyslot".into());
        }
    }
    let segments = numbered(json, "segments")?;
    let count = segments
        .len()
        .checked_sub(2)
        .filter(|n| (1..=3).contains(n))
        .ok_or("reencryption needs one to three active segments and two backups")?;
    for (i, (number, _)) in segments.iter().enumerate() {
        if usize::from(*number) != i {
            return Err("reencryption segments are not consecutively numbered".into());
        }
    }
    let (old_backup, old_segment) = segments.get(count).ok_or("missing previous backup")?;
    let (new_backup, new_segment) = segments.get(count + 1).ok_or("missing final backup")?;
    backup(old_segment, "backup-previous")?;
    backup(new_segment, "backup-final")?;
    let old_digest = one_binding(json, *old_backup)?;
    let new_digest = one_binding(json, *new_backup)?;
    if old_digest == new_digest {
        return Err("reencryption old and new digest identities must differ".into());
    }
    let mut reencrypt_digest = None;
    for (id, digest) in numbered(json, "digests")? {
        let named = slots(digest, "keyslots", "digest")?;
        let named_segments = slots(digest, "segments", "digest")?;
        if named_segments
            .iter()
            .any(|n| !segments.iter().any(|(id, _)| id == n))
        {
            return Err("digest names an absent reencryption segment".into());
        }
        if id != old_digest && id != new_digest && !named_segments.is_empty() {
            return Err("another digest names reencryption segments".into());
        }
        if (id == old_digest || id == new_digest) && named.is_empty() {
            return Err("reencryption key has no keyslot".into());
        }
        if named.contains(&reencrypt_slot)
            && (named != [reencrypt_slot]
                || !named_segments.is_empty()
                || reencrypt_digest.replace(id).is_some())
        {
            return Err("reencryption keyslot must have one private digest".into());
        }
    }
    let reencrypt_digest = reencrypt_digest.ok_or("reencryption keyslot has no digest")?;

    // Reuse ordinary keyslot/digest/token admission after checking every
    // field whose reencryption meaning differs. Only a private clone is
    // normalized; no caller or disk metadata is changed.
    let mut normal = json.clone();
    let _ = normal
        .get_mut("config")
        .ok_or("missing config")?
        .remove("requirements");
    let _ = normal
        .get_mut("keyslots")
        .ok_or("missing keyslots")?
        .remove(&reencrypt_slot.to_string());
    let mut segment = (*old_segment).clone();
    let _ = segment.remove("flags");
    let _ = normal.insert("segments", Json::Obj(vec![("0".into(), segment)]));
    for (id, _) in numbered(json, "digests")? {
        let d = normal
            .get_mut("digests")
            .and_then(|d| d.get_mut(&id.to_string()))
            .ok_or("missing digest")?;
        let _ = d.insert(
            "segments",
            Json::Arr(if id == old_digest {
                vec![Json::from("0")]
            } else {
                vec![]
            }),
        );
        if id == reencrypt_digest {
            let _ = d.insert("keyslots", Json::Arr(vec![]));
        }
    }
    write::profile(&normal)?;

    let mut roles = Vec::with_capacity(count);
    let mut cursor = 0u64;
    let mut progress = 0;
    let mut hotzone = None;
    for (id, segment) in segments.iter().take(count) {
        segment_fields(segment)?;
        if decimal(segment, "offset", "active segment")?
            != DATA_OFFSET
                .checked_add(cursor)
                .ok_or("segment offset overflow")?
            || decimal(segment, "iv_tweak", "active segment")? != cursor / 512
        {
            return Err("reencryption active offsets or IVs are not contiguous".into());
        }
        let dynamic = field(segment, "size")?.as_str() == Some("dynamic");
        let size = if dynamic {
            if usize::from(*id) + 1 != count {
                return Err("only the final active segment may be dynamic".into());
            }
            data_size
                .checked_sub(cursor)
                .ok_or("active segment exceeds data length")?
        } else {
            decimal(segment, "size", "active segment")?
        };
        let end = cursor
            .checked_add(size)
            .filter(|end| *end <= data_size)
            .ok_or("active segment exceeds data length")?;
        if size == 0 || !size.is_multiple_of(SECTOR_SIZE) {
            return Err("active segment is not positive whole sectors".into());
        }
        let bound = bindings(json, *id)?;
        let role = if bound.len() == 2 && bound.contains(&old_digest) && bound.contains(&new_digest)
        {
            Role::Dirty
        } else if bound == [old_digest] {
            Role::Old
        } else if bound == [new_digest] {
            Role::New
        } else {
            return Err("active segment has unexpected digest bindings".into());
        };
        match role {
            Role::Dirty => {
                if flag(segment)? != Some("in-reencryption") || dynamic || hotzone.is_some() {
                    return Err("invalid dirty hotzone".into());
                }
                let needed = match area.resilience {
                    Resilience::Journal => size,
                    Resilience::Checksum => (size / SECTOR_SIZE)
                        .checked_mul(32)
                        .ok_or("checksum area size overflow")?,
                };
                if needed > area.size {
                    return Err("dirty hotzone exceeds resilience area".into());
                }
                hotzone = Some(Hotzone {
                    offset: cursor,
                    size,
                });
            }
            _ if flag(segment)?.is_some() => {
                return Err("clean segment has a reencryption flag".into())
            }
            Role::New => progress = end,
            Role::Old if !dynamic => return Err("old suffix must have dynamic size".into()),
            Role::Old => {}
        }
        roles.push(role);
        cursor = end;
    }
    if cursor != data_size {
        return Err("reencryption active segments do not cover the volume".into());
    }
    use Role::{Dirty, New, Old};
    match roles.as_slice() {
        [Old] | [New, Old] | [Dirty] | [Dirty, Old] | [New, Dirty] | [New, Dirty, Old] => {}
        [New] => {
            return Err("clean all-new layout still carries the reencryption requirement".into())
        }
        _ => return Err("reencryption segment order is not forward in-place".into()),
    }
    Ok(Layout {
        area,
        old_digest,
        new_digest,
        reencrypt_slot,
        reencrypt_digest,
        data_size,
        progress,
        hotzone,
    })
}
