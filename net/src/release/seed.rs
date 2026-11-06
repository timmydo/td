//! Local bytes are only a source of objects named by authenticated metadata.
use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use td_engine::image_chunks::{Chunker, MAX};

#[derive(Default)]
pub(super) struct Seed {
    files: Vec<File>,
    objects: BTreeMap<String, (usize, u64, u64)>,
}

impl Seed {
    fn retain(
        &mut self,
        chunk: &[u8],
        file: usize,
        offset: u64,
        missing: &mut BTreeMap<String, u64>,
    ) {
        let digest = hash(chunk);
        if missing.get(&digest) == Some(&(chunk.len() as u64)) {
            missing.remove(&digest);
            self.objects
                .insert(digest, (file, offset, chunk.len() as u64));
        }
    }

    pub(super) fn get(&self, digest: &str, size: u64) -> Result<Option<Vec<u8>>> {
        let Some(&(file, offset, length)) = self.objects.get(digest) else {
            return Ok(None);
        };
        if size != length || size > MAX as u64 {
            return Err("seed object length mismatch".into());
        }
        let mut bytes = vec![0u8; size as usize];
        io(self
            .files
            .get(file)
            .ok_or("seed descriptor missing")?
            .read_exact_at(&mut bytes, offset))?;
        if hash(&bytes) != digest {
            return Err("local seed changed after scanning".into());
        }
        Ok(Some(bytes))
    }
}

pub(super) fn scan(
    release: &Release,
    cache: &mut cache::Cache,
    deployment: &Path,
    deadline: Instant,
) -> Result<Seed> {
    let mut missing = BTreeMap::new();
    let mut cold = false;
    for payload in &release.payloads {
        for object in &payload.objects {
            if Instant::now() >= deadline {
                return Err("release download exceeded one hour while seeding".into());
            }
            cold |= cache.get(&object.digest, object.size)?.is_none();
            missing.insert(object.digest.clone(), object.size);
        }
    }
    let mut seed = Seed::default();
    if !cold {
        return Ok(seed);
    }
    let mut block = vec![0u8; MAX];
    let mut chunk = Vec::with_capacity(MAX);
    for (name, maximum) in format::FILES {
        if missing.is_empty() {
            break;
        }
        let (file, meta) = io(td_fs::open_real_file(
            &deployment.join(name),
            "seed payload",
        ))?;
        if meta.len() == 0 || meta.len() > maximum {
            return Err("seed payload size exceeds bound".into());
        }
        let index = seed.files.len();
        seed.files.push(file);
        let mut chunker = Chunker::default();
        let mut size = 0u64;
        let mut start = 0u64;
        chunk.clear();
        loop {
            if Instant::now() >= deadline {
                return Err("release download exceeded one hour while seeding".into());
            }
            let count = io(seed
                .files
                .get_mut(index)
                .ok_or("seed descriptor missing")?
                .read(&mut block))?;
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
                    seed.retain(&chunk, index, start, &mut missing);
                    start += chunk.len() as u64;
                    chunk.clear();
                }
            }
            if missing.is_empty() {
                return Ok(seed);
            }
        }
        if size != meta.len() {
            return Err("seed payload shrank".into());
        }
        if !chunk.is_empty() {
            seed.retain(&chunk, index, start, &mut missing);
        }
    }
    Ok(seed)
}
