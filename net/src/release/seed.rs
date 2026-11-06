//! Local bytes are only a source of objects named by authenticated metadata.
use super::*;
use std::collections::BTreeMap;
use td_engine::image_chunks::{Chunker, MAX};

fn retain(chunk: &[u8], missing: &mut BTreeMap<String, u64>, cache: &Path) -> Result<u64> {
    let digest = hash(chunk);
    if missing.get(&digest) != Some(&(chunk.len() as u64)) {
        return Ok(0);
    }
    replace(&cache.join(&digest), chunk)?;
    missing.remove(&digest);
    Ok(chunk.len() as u64)
}

pub(super) fn cache(
    release: &Release,
    cache: &Path,
    deployment: &Path,
    deadline: Instant,
) -> Result<u64> {
    let mut missing = BTreeMap::new();
    for payload in &release.payloads {
        for object in &payload.objects {
            if Instant::now() >= deadline {
                return Err("release download exceeded one hour while seeding".into());
            }
            let valid = read(&cache.join(&object.digest), object.size).is_ok_and(|bytes| {
                bytes.len() as u64 == object.size && hash(&bytes) == object.digest
            });
            if !valid {
                missing.insert(object.digest.clone(), object.size);
            }
        }
    }
    let mut seeded = 0u64;
    let mut block = vec![0u8; MAX];
    let mut chunk = Vec::with_capacity(MAX);
    for (name, maximum) in format::FILES {
        if missing.is_empty() {
            break;
        }
        let (mut file, meta) = io(td_fs::open_real_file(
            &deployment.join(name),
            "seed payload",
        ))?;
        if meta.len() == 0 || meta.len() > maximum {
            return Err("seed payload size exceeds bound".into());
        }
        let mut chunker = Chunker::default();
        let mut size = 0u64;
        chunk.clear();
        loop {
            if Instant::now() >= deadline {
                return Err("release download exceeded one hour while seeding".into());
            }
            let count = io(file.read(&mut block))?;
            if count == 0 {
                break;
            }
            size += count as u64;
            if size > meta.len() {
                return Err("seed payload grew".into());
            }
            for byte in block.get(..count).ok_or("seed read length")? {
                chunk.push(*byte);
                if chunker.push(*byte) {
                    seeded += retain(&chunk, &mut missing, cache)?;
                    chunk.clear();
                }
            }
            if missing.is_empty() {
                return Ok(seeded);
            }
        }
        if size != meta.len() {
            return Err("seed payload shrank".into());
        }
        if !chunk.is_empty() {
            seeded += retain(&chunk, &mut missing, cache)?;
        }
    }
    Ok(seeded)
}
