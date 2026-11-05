//! Offline byte-reuse experiment, never an update admission mechanism.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

mod erofs;
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
mod transfer;
#[path = "../../builder/src/zstd.rs"]
#[allow(dead_code, clippy::same_item_push)]
#[cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::indexing_slicing,
        clippy::manual_is_multiple_of
    )
)]
mod zstd;

#[cfg(test)]
#[path = "../../builder/src/erofs.rs"]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::type_complexity
)]
mod packer;
#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const MAX_PAYLOAD: u64 = 512 * 1024 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_PAYLOAD {
        return Err(invalid("input must be a regular file of at most 512 MiB").into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_PAYLOAD + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PAYLOAD {
        return Err(invalid("input grew past 512 MiB").into());
    }
    Ok(bytes)
}

fn output(path: &Path) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(
        OpenOptions::new().write(true).create_new(true).open(path)?,
    ))
}

fn files_report(old: &erofs::Image, new: &erofs::Image, a: &[u8], b: &[u8]) -> Result<()> {
    let start = Instant::now();
    let before = old.files(a)?;
    let after = new.files(b)?;
    let old_keys: BTreeSet<_> = before.keys().collect();
    let missing: BTreeMap<_, _> = after
        .iter()
        .filter(|(key, _)| !old_keys.contains(key))
        .collect();
    println!("files\troot.erofs\tunique_files={}\tmissing_files={}\tunique_logical_bytes={}\tmissing_logical_bytes={}\tindex_ms={}",
        after.len(), missing.len(), after.values().sum::<u64>(), missing.values().map(|n| **n).sum::<u64>(), start.elapsed().as_millis());
    Ok(())
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [old, new, out] = args.as_slice() else {
        return Err(
            "usage: td-image-transfer OLD-DEPLOYMENT NEW-DEPLOYMENT NEW-OUTPUT-DIRECTORY".into(),
        );
    };
    let old = Path::new(old);
    let new = Path::new(new);
    let out = Path::new(out);
    fs::create_dir(out)?;
    println!("td-image-transfer-v1\tindex_record_model=48\tindex_header_model=48\tcache=old-payload-only\tcompression=none-added");
    for name in ["root.erofs", "bzImage", "initramfs.cpio"] {
        let a = read(&old.join(name))?;
        let b = read(&new.join(name))?;
        println!(
            "payload\t{name}\told_bytes={}\tnew_bytes={}\told_sha256={}\tnew_sha256={}",
            a.len(),
            b.len(),
            sha256::hex_digest(&a),
            sha256::hex_digest(&b)
        );
        for method in [transfer::Method::Fixed, transfer::Method::Cdc] {
            let start = Instant::now();
            let before = transfer::index(&a, method)?;
            let after = transfer::index(&b, method)?;
            let indexing = start.elapsed();
            let mut writer = output(&out.join(format!("{name}.{}", method.name())))?;
            let start = Instant::now();
            let stats = transfer::reconstruct(&a, &b, &before, &after, &mut writer)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            stats.print(name, method.name(), indexing, start.elapsed());
        }
        if name == "root.erofs" {
            let start = Instant::now();
            let before = erofs::Image::parse(&a)?;
            let after = erofs::Image::parse(&b)?;
            let old_index = transfer::ranges(&a, &before.partition(a.len())?)?;
            let new_index = transfer::ranges(&b, &after.partition(b.len())?)?;
            let indexing = start.elapsed();
            let mut writer = output(&out.join("root.erofs.extents"))?;
            let start = Instant::now();
            let stats = transfer::reconstruct(&a, &b, &old_index, &new_index, &mut writer)?;
            writer.flush()?;
            writer.get_ref().sync_all()?;
            stats.print(name, "extents", indexing, start.elapsed());
            files_report(&before, &after, &a, &b)?;
        }
    }
    println!("complete\tall_reconstructions_sha256_verified=true");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("td-image-transfer: {error}");
            ExitCode::FAILURE
        }
    }
}
