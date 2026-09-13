//! Retained hybrid ISO composition from stable, already-built inputs.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use td_engine::{fat, gpt, iso9660};

use td_recipe::td_boot_realfile as realfile;

const MIB: u64 = 1024 * 1024;
const ESP_BYTES: u64 = 64 * MIB;
const MAX_BOOT_BYTES: u64 = 256 * MIB;
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024 * MIB;
const MAX_PAYLOADS: usize = 64;
const USAGE: &str = "usage: compose-iso OUTPUT KERNEL INITRAMFS [ISO-NAME=FILE ...]";

pub(crate) fn cli(args: &[String]) -> Result<(), String> {
    let [output, kernel, initramfs, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    if rest.len() > MAX_PAYLOADS {
        return Err(format!("ISO accepts at most {MAX_PAYLOADS} payloads"));
    }
    let mut payloads = Vec::new();
    for arg in rest {
        let (name, path) = arg.split_once('=').ok_or(USAGE)?;
        if name.is_empty() || path.is_empty() {
            return Err(USAGE.into());
        }
        payloads.push((name, PathBuf::from(path)));
    }
    write_image_with_payloads(
        Path::new(output),
        Path::new(kernel),
        Path::new(initramfs),
        &payloads,
    )?;
    println!("ISO written: {output}");
    Ok(())
}

struct Input {
    file: File,
    path: PathBuf,
    len: u64,
}

impl Input {
    fn open(path: &Path, limit: u64) -> Result<Self, String> {
        let (file, meta) =
            realfile::open_real_file(path, "ISO input").map_err(|error| error.to_string())?;
        if meta.len() == 0 || meta.len() > limit {
            return Err(format!(
                "ISO input {} must contain 1..={limit} bytes",
                path.display()
            ));
        }
        Ok(Self {
            file,
            path: path.into(),
            len: meta.len(),
        })
    }

    fn copy_to(&mut self, out: &mut File) -> Result<(), String> {
        copy_exact(&mut self.file, out, self.len)
            .map_err(|error| format!("copy ISO input {}: {error}", self.path.display()))?;
        if self
            .file
            .metadata()
            .map_err(|error| format!("stat {}: {error}", self.path.display()))?
            .len()
            != self.len
        {
            return Err(format!("ISO input changed length: {}", self.path.display()));
        }
        Ok(())
    }
}

pub(crate) fn write_image(path: &Path, kernel: &Path, initramfs: &Path) -> Result<(), String> {
    write_image_with_payloads(path, kernel, initramfs, &[])
}

pub(crate) fn write_image_with_payloads(
    path: &Path,
    kernel: &Path,
    initramfs: &Path,
    payloads: &[(&str, PathBuf)],
) -> Result<(), String> {
    if payloads.len() > MAX_PAYLOADS {
        return Err(format!("ISO accepts at most {MAX_PAYLOADS} payloads"));
    }
    let mut kernel = Input::open(kernel, MAX_BOOT_BYTES)?;
    let mut initramfs = Input::open(initramfs, MAX_BOOT_BYTES)?;
    let mut files = Vec::new();
    let mut inputs = Vec::new();
    for (name, source) in payloads {
        let input = Input::open(source, MAX_PAYLOAD_BYTES)?;
        files.push(iso9660::FileSpec {
            name: (*name).into(),
            len: input.len,
        });
        inputs.push((*name, input));
    }
    let (image, esp) = layout(kernel.len, initramfs.len, files)?;
    // The destination appears only after all declared bytes have been synced.
    let publication = Publication::new(path)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&publication.temporary)
        .map_err(|error| format!("create {}: {error}", publication.temporary.display()))?;
    publication.probe_hard_links()?;
    let result = (|| {
        out.set_len(image.total_bytes)
            .map_err(|error| error.to_string())?;
        for extent in &image.extents {
            write_at(&mut out, extent.offset, &extent.bytes)?;
        }
        for extent in &esp.extents {
            write_at(&mut out, image.esp_offset + extent.offset, &extent.bytes)?;
        }
        let boot_path = format!(r"\EFI\BOOT\{}", td_recipe::ladder::EFI_BOOT_FILE);
        for placement in &esp.placements {
            let source = match placement.path.as_str() {
                td_recipe::ladder::EFI_INITRD_PATH => &mut initramfs,
                name if name == boot_path => &mut kernel,
                _ => return Err(format!("unexpected ESP file {}", placement.path)),
            };
            out.seek(SeekFrom::Start(image.esp_offset + placement.offset))
                .map_err(|e| e.to_string())?;
            source.copy_to(&mut out)?;
        }
        for placement in &image.placements {
            let (_, source) = inputs
                .iter_mut()
                .find(|(name, _)| *name == placement.name)
                .ok_or_else(|| format!("missing ISO source {}", placement.name))?;
            out.seek(SeekFrom::Start(placement.offset))
                .map_err(|e| e.to_string())?;
            source.copy_to(&mut out)?;
        }
        out.sync_all().map_err(|e| e.to_string())
    })();
    result.map_err(|error| format!("write ISO {}: {error}", path.display()))?;
    publication.publish()
}

fn layout(
    kernel_len: u64,
    initramfs_len: u64,
    files: Vec<iso9660::FileSpec>,
) -> Result<(iso9660::Image, fat::Image<'static>), String> {
    // Grow only the firmware partition. Deployment payloads remain ISO files.
    for esp_bytes in [ESP_BYTES, 128 * MIB, 256 * MIB, 512 * MIB] {
        let image = iso9660::build(&iso9660::Volume {
            disk_guid: gpt::Guid([0x41; 16]),
            esp_guid: gpt::Guid([0x42; 16]),
            esp_bytes,
            files: files.clone(),
        })?;
        let esp = fat::build(&fat::Volume {
            bytes_per_sector: 512,
            total_sectors: esp_bytes / 512,
            hidden_sectors: u32::try_from(image.esp_offset / 512)
                .map_err(|_| "ESP offset exceeds FAT field")?,
            volume_id: 0x54444953,
            label: "TD INSTALL".into(),
            sectors_per_cluster: None,
            root: vec![(
                "EFI".into(),
                fat::Node::Dir(vec![(
                    "BOOT".into(),
                    fat::Node::Dir(vec![
                        (
                            td_recipe::ladder::EFI_BOOT_FILE.into(),
                            fat::Node::Stream(kernel_len),
                        ),
                        ("INITRD".into(), fat::Node::Stream(initramfs_len)),
                    ]),
                )]),
            )],
        });
        match esp {
            Ok(esp) => return Ok((image, esp)),
            Err(error) if esp_bytes == 512 * MIB => return Err(format!("media ESP: {error}")),
            Err(_) => {}
        }
    }
    Err("media boot files exceed the ESP capacity".into())
}

fn write_at(out: &mut File, offset: u64, bytes: &[u8]) -> Result<(), String> {
    out.seek(SeekFrom::Start(offset))
        .and_then(|_| out.write_all(bytes))
        .map_err(|e| e.to_string())
}

fn copy_exact(source: &mut impl Read, out: &mut impl Write, len: u64) -> Result<(), String> {
    let count = std::io::copy(&mut source.take(len), out).map_err(|e| e.to_string())?;
    let mut extra = [0];
    if count != len || source.read(&mut extra).map_err(|e| e.to_string())? != 0 {
        return Err("ISO input changed length during copy".into());
    }
    Ok(())
}

/// Caller controls the output parent and keeps it stable during composition.
struct Publication {
    destination: PathBuf,
    directory: PathBuf,
    temporary: PathBuf,
    parent: File,
}

impl Publication {
    fn new(destination: &Path) -> Result<Self, String> {
        if destination.as_os_str().is_empty() {
            return Err("ISO destination path cannot be empty".into());
        }
        match fs::symlink_metadata(destination) {
            Ok(_) => {
                return Err(format!(
                    "ISO destination already exists: {}",
                    destination.display()
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("stat {}: {error}", destination.display())),
        }
        let parent_path = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = File::open(parent_path)
            .map_err(|error| format!("open ISO parent {}: {error}", parent_path.display()))?;
        if !parent
            .metadata()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            return Err(format!(
                "ISO parent is not a directory: {}",
                parent_path.display()
            ));
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
            let directory = parent_path.join(format!(".td-iso-{}-{sequence}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        destination: destination.into(),
                        temporary: directory.join("image"),
                        directory,
                        parent,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "create ISO staging {}: {error}",
                        directory.display()
                    ))
                }
            }
        }
        Err("cannot allocate a private ISO staging directory".into())
    }

    fn probe_hard_links(&self) -> Result<(), String> {
        let probe = self.directory.join("link-probe");
        fs::hard_link(&self.temporary, &probe)
            .and_then(|()| fs::remove_file(&probe))
            .map_err(|error| {
                format!(
                    "ISO hard-link probe in {} failed: {error}",
                    self.directory.display()
                )
            })
    }

    fn publish(&self) -> Result<(), String> {
        let destination = &self.destination;
        // Same filesystem, atomic and no replacement, including dangling symlinks.
        fs::hard_link(&self.temporary, destination)
            .map_err(|error| format!("publish ISO {}: {error}", destination.display()))?;
        self.parent.sync_all().map_err(|error| {
            format!(
                "sync ISO parent for {} (complete file may exist): {error}",
                destination.display()
            )
        })
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join("link-probe"));
        let _ = fs::remove_file(&self.temporary);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    struct Scratch {
        dir: PathBuf,
    }
    impl Scratch {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "td-iso-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self { dir }
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
    #[test]
    fn iso_payloads_stream_to_their_named_extents_with_zero_padding() {
        let scratch = Scratch::new();
        let source = scratch.dir.join("boot");
        fs::write(&source, b"boot bytes").unwrap();
        let mut payloads = Vec::new();
        for (name, bytes) in [
            ("Z.IMG", vec![0x5a; 4097]),
            ("A.IMG", vec![0x41; 1300]),
            ("M.IMG", vec![0x4d; 2048]),
        ] {
            let path = scratch.dir.join(name);
            fs::write(&path, bytes).unwrap();
            payloads.push((name, path));
        }
        let disk = scratch.dir.join("payload.iso");
        write_image_with_payloads(&disk, &source, &source, &payloads).unwrap();
        let metadata = iso9660::build(&iso9660::Volume {
            disk_guid: gpt::Guid([0x41; 16]),
            esp_guid: gpt::Guid([0x42; 16]),
            esp_bytes: ESP_BYTES,
            files: vec![
                iso9660::FileSpec {
                    name: "A.IMG".into(),
                    len: 1300,
                },
                iso9660::FileSpec {
                    name: "Z.IMG".into(),
                    len: 4097,
                },
                iso9660::FileSpec {
                    name: "M.IMG".into(),
                    len: 2048,
                },
            ],
        })
        .unwrap();
        let mut file = File::open(&disk).unwrap();
        assert_eq!(file.metadata().unwrap().len(), metadata.total_bytes);
        for placement in metadata.placements {
            let expected = fs::read(scratch.dir.join(&placement.name)).unwrap();
            file.seek(SeekFrom::Start(placement.offset)).unwrap();
            let mut got = vec![0; expected.len()];
            file.read_exact(&mut got).unwrap();
            assert_eq!(got, expected);
            let padding_len = (iso9660::BLOCK - placement.len % iso9660::BLOCK) % iso9660::BLOCK;
            let mut padding = vec![1; padding_len as usize];
            file.read_exact(&mut padding).unwrap();
            assert!(padding.iter().all(|byte| *byte == 0));
        }
    }

    #[test]
    fn invalid_or_missing_payload_refuses_before_output_creation() {
        let scratch = Scratch::new();
        let source = scratch.dir.join("source");
        fs::write(&source, b"input").unwrap();
        let disk = scratch.dir.join("never-created.iso");
        for payloads in [
            vec![("ABSENT", scratch.dir.join("absent"))],
            vec![("EFI.IMG", source.clone())],
            vec![("DUP", source.clone()), ("DUP.", source.clone())],
        ] {
            assert!(write_image_with_payloads(&disk, &source, &source, &payloads).is_err());
            assert!(!disk.exists());
        }
    }

    #[test]
    fn streams_refuse_short_and_grown_inputs() {
        let mut out = Vec::new();
        assert!(copy_exact(&mut b"short".as_slice(), &mut out, 6).is_err());
        out.clear();
        assert!(copy_exact(&mut b"payload suffix".as_slice(), &mut out, 7).is_err());
        assert_eq!(out, b"payload");
    }

    #[test]
    fn image_creation_preserves_existing_destination() {
        let scratch = Scratch::new();
        let source = scratch.dir.join("source");
        let disk = scratch.dir.join("disk");
        fs::write(&source, b"input").unwrap();
        fs::write(&disk, b"preserve").unwrap();
        let error = write_image(&disk, &source, &source).unwrap_err();
        assert!(error.contains("destination already exists"));
        assert_eq!(fs::read(&disk).unwrap(), b"preserve");
    }
    #[test]
    fn payload_larger_than_four_gib_uses_multiple_iso_sections_without_loading_it() {
        let scratch = Scratch::new();
        let path = scratch.dir.join("large");
        let file = File::create(&path).unwrap();
        let len = 5 * 1024 * MIB + 3;
        file.set_len(len).unwrap();
        let input = Input::open(&path, MAX_PAYLOAD_BYTES).unwrap();
        assert_eq!(input.len, len);
        let (image, _) = layout(
            10,
            10,
            vec![iso9660::FileSpec {
                name: "ROOT.EROFS".into(),
                len,
            }],
        )
        .unwrap();
        assert_eq!(image.placements[0].len, len);
        assert!(image.total_bytes > len);
        assert!(Input::open(&path, MAX_BOOT_BYTES).is_err());
        file.set_len(MAX_PAYLOAD_BYTES + 1).unwrap();
        assert!(Input::open(&path, MAX_PAYLOAD_BYTES).is_err());
    }

    #[test]
    fn esp_grows_for_live_boot_files_and_refuses_overflow() {
        let (_, esp) = layout(40 * MIB, 40 * MIB, vec![]).unwrap();
        assert_eq!(esp.total_bytes, 128 * MIB);
        assert!(layout(MAX_BOOT_BYTES, MAX_BOOT_BYTES, vec![]).is_err());
        let files = (0..MAX_PAYLOADS)
            .map(|index| iso9660::FileSpec {
                name: format!("PAYLOAD{index:02}"),
                len: MAX_PAYLOAD_BYTES,
            })
            .collect();
        let (image, esp) = layout(128 * MIB, 128 * MIB, files).unwrap();
        assert_eq!(esp.total_bytes, 512 * MIB);
        assert_eq!(image.placements.len(), MAX_PAYLOADS);
        assert!(image
            .placements
            .iter()
            .all(|placement| placement.len == MAX_PAYLOAD_BYTES));
        assert!(image.total_bytes > MAX_PAYLOAD_BYTES * MAX_PAYLOADS as u64);
    }

    #[test]
    fn enlarged_esp_places_both_boot_files_before_the_iso_payload() {
        let scratch = Scratch::new();
        let kernel = scratch.dir.join("kernel");
        let initramfs = scratch.dir.join("initramfs");
        let length = 40 * MIB;
        for (path, marker) in [(&kernel, b"KERNEL"), (&initramfs, b"INITRD")] {
            let mut file = File::create(path).unwrap();
            file.write_all(marker).unwrap();
            file.set_len(length).unwrap();
            file.seek(SeekFrom::Start(length - 6)).unwrap();
            file.write_all(marker).unwrap();
        }
        let payload = scratch.dir.join("payload");
        fs::write(&payload, b"after ESP").unwrap();
        let output = scratch.dir.join("grown.iso");
        write_image_with_payloads(&output, &kernel, &initramfs, &[("ROOT.EROFS", payload)])
            .unwrap();
        let (image, esp) = layout(
            length,
            length,
            vec![iso9660::FileSpec {
                name: "ROOT.EROFS".into(),
                len: 9,
            }],
        )
        .unwrap();
        assert_eq!(esp.total_bytes, 128 * MIB);
        let mut output = File::open(output).unwrap();
        for placement in &esp.placements {
            let source = if placement.path.ends_with("BOOTX64.EFI") {
                &kernel
            } else {
                &initramfs
            };
            let expected = fs::read(source).unwrap();
            let mut actual = vec![0; expected.len()];
            output
                .seek(SeekFrom::Start(image.esp_offset + placement.offset))
                .unwrap();
            output.read_exact(&mut actual).unwrap();
            assert_eq!(actual, expected);
        }
        let placement = &image.placements[0];
        assert!(placement.offset >= image.esp_offset + esp.total_bytes);
        output.seek(SeekFrom::Start(placement.offset)).unwrap();
        let mut actual = [0; 9];
        output.read_exact(&mut actual).unwrap();
        assert_eq!(&actual, b"after ESP");
    }

    #[test]
    fn hard_link_probe_is_private_and_cleanup_handles_a_retained_probe() {
        let scratch = Scratch::new();
        let destination = scratch.dir.join("out.iso");
        let publication = Publication::new(&destination).unwrap();
        fs::write(&publication.temporary, b"").unwrap();
        publication.probe_hard_links().unwrap();
        assert!(!destination.exists());
        assert!(!publication.directory.join("link-probe").exists());
        // A colliding probe exercises the same failure and cleanup path.
        fs::write(publication.directory.join("link-probe"), b"collision").unwrap();
        assert!(publication.probe_hard_links().is_err());
        let directory = publication.directory.clone();
        drop(publication);
        assert!(!directory.exists());
        assert!(!destination.exists());
    }

    #[test]
    fn publication_is_absent_until_commit_and_never_replaces_a_racing_destination() {
        assert!(Publication::new(Path::new("")).is_err());
        let scratch = Scratch::new();
        let dest = scratch.dir.join("out.iso");
        let publication = Publication::new(&dest).unwrap();
        fs::write(&publication.temporary, b"complete").unwrap();
        assert!(!dest.exists());
        fs::write(&dest, b"preserve").unwrap();
        assert!(publication.publish().is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"preserve");
        let directory = publication.directory.clone();
        drop(publication);
        assert!(!directory.exists());
    }

    #[test]
    fn ordinary_failure_removes_staging_and_publishes_nothing() {
        let scratch = Scratch::new();
        let dest = scratch.dir.join("out.iso");
        let publication = Publication::new(&dest).unwrap();
        fs::write(&publication.temporary, b"partial").unwrap();
        drop(publication);
        assert!(!dest.exists());
        assert_eq!(fs::read_dir(&scratch.dir).unwrap().count(), 0);
    }

    #[test]
    fn sources_and_destinations_refuse_symlinks_and_special_files() {
        let scratch = Scratch::new();
        let source = scratch.dir.join("source");
        fs::write(&source, b"input").unwrap();
        let alias = scratch.dir.join("alias");
        symlink(&source, &alias).unwrap();
        let out = scratch.dir.join("out");
        for path in [&alias, Path::new("/dev/null"), &scratch.dir] {
            assert!(write_image(&out, path, &source).is_err());
            assert!(!out.exists());
        }
        symlink(scratch.dir.join("absent"), &out).unwrap();
        assert!(write_image(&out, &source, &source).is_err());
        assert!(out.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(write_image(&source, &source, &source).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"input");
    }

    #[test]
    fn held_input_refuses_growth_and_shrinkage() {
        let scratch = Scratch::new();
        let source = scratch.dir.join("source");
        let mut out = File::create(scratch.dir.join("out")).unwrap();
        for changed in [b"s".as_slice(), b"longer".as_slice()] {
            fs::write(&source, b"input").unwrap();
            let mut input = Input::open(&source, MAX_PAYLOAD_BYTES).unwrap();
            fs::write(&source, changed).unwrap();
            assert!(input.copy_to(&mut out).is_err());
        }
    }

    #[test]
    fn cli_composes_repeatably_with_equals_in_payload_path() {
        let scratch = Scratch::new();
        let source = scratch.dir.join("input=bytes");
        fs::write(&source, b"payload").unwrap();
        let first = scratch.dir.join("first.iso");
        let second = scratch.dir.join("second.iso");
        for dest in [&first, &second] {
            cli(&[
                dest.display().to_string(),
                source.display().to_string(),
                source.display().to_string(),
                format!("ROOT.EROFS={}", source.display()),
            ])
            .unwrap();
            assert_eq!(
                dest.metadata().unwrap().permissions().mode() & 0o777 & !0o600,
                0
            );
        }
        assert_eq!(
            td_engine::sha256::sha256_file(&first).unwrap(),
            td_engine::sha256::sha256_file(&second).unwrap()
        );
        assert!(!fs::read_dir(&scratch.dir).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".td-iso-")));
    }

    #[test]
    fn cli_refuses_missing_arguments_and_bad_payload_syntax() {
        for args in [
            vec![],
            vec!["out", "kernel"],
            vec!["out", "kernel", "initrd", "NAME"],
            vec!["out", "kernel", "initrd", "NAME="],
            vec!["out", "kernel", "initrd", "=file"],
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
            assert_eq!(cli(&args).unwrap_err(), USAGE);
        }
        let args = vec!["x".to_owned(); MAX_PAYLOADS + 4];
        assert!(cli(&args).unwrap_err().contains("at most 64"));
    }
}
