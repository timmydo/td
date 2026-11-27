use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use td_fs::TempDir;
use td_vm::disk::{create_blank, create_overlay, lock_idle};

#[test]
fn exclusive_image_excludes_a_second_manager_and_template_reader() -> io::Result<()> {
    let dir = TempDir::new("vm-disk-lock")?;
    let base = dir.join("base");
    let overlay = dir.join("overlay");
    create_blank(&base, 5 << 20)?;
    let first = lock_idle(&base)?;
    assert!(lock_idle(&base).is_err());
    assert!(create_overlay(&overlay, &base).is_err());
    assert!(!overlay.exists());
    drop(first);
    create_overlay(&overlay, &base)?;
    assert!(create_blank(&base, 512).is_err());
    Ok(())
}

#[test]
fn large_geometry_counts_all_initial_metadata_clusters() -> io::Result<()> {
    let dir = TempDir::new("vm-disk-tables")?;
    let path = dir.join("base");
    let cluster = 65536u64;
    let capacity = (4u64 << 40) + 512;
    create_blank(&path, capacity)?;
    let file = File::open(path)?;
    let mut word = [0; 4];
    file.read_exact_at(&mut word, 36)?;
    assert_eq!(u32::from_be_bytes(word), 8193);
    assert_eq!(file.metadata()?.len(), 5 * cluster);
    let mut counts = [0; 12];
    file.read_exact_at(&mut counts, 2 * cluster)?;
    assert_eq!(counts, [0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 0]);
    Ok(())
}

#[test]
fn an_invalid_refcount_order_cannot_publish_an_overlay() -> io::Result<()> {
    let dir = TempDir::new("vm-disk-header")?;
    let base = dir.join("base");
    let overlay = dir.join("overlay");
    create_blank(&base, 512)?;
    File::options()
        .write(true)
        .open(&base)?
        .write_all_at(&7u32.to_be_bytes(), 96)?;
    assert!(create_overlay(&overlay, &base).is_err());
    assert!(!overlay.exists());
    Ok(())
}

#[test]
fn only_the_private_lock_function_calls_fcntl() {
    let raw = include_str!("../src/disk_lock.rs");
    let disk = include_str!("../src/disk.rs");
    assert_eq!(
        raw.trim()
            .lines()
            .map(str::trim)
            .collect::<String>()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b))
                .wrapping_mul(0x100000001b3)),
        LOCK_ABI
    );
    assert_eq!(raw.matches("unsafe").count(), 3);
    assert!(raw.contains("fcntl(file.as_raw_fd(), cmd, &mut lock as *mut Flock)"));
    assert!(disk.contains("disk_lock::template(&file)?"));
    assert!(disk.contains("disk_lock::idle(&file)?"));
    assert!(!disk.contains("unsafe"));
}

const LOCK_ABI: u64 = 0x9b3446e83f693965;
