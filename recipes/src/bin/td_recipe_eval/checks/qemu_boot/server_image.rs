//! `server-image`: a server deployment on a disk image that UEFI firmware
//! boots unchanged (td-svc/DESIGN.md §10, "Profiles"). The kernel carries
//! virtio, NVMe and e1000 drivers, not Hyper-V's or ENA's, so of the clouds
//! only a virtio machine boots it yet.
//!
//! The image is the installed layout of td-install/DESIGN.md §4: a GPT
//! whose ESP holds the deployment's EFI-stub kernel and a selector bound to
//! this run's key and the volume, and a Btrfs volume carrying the signed
//! deployment and a `@var` provisioned with the primary account's name and
//! SSH keys. The key is made for this run and discarded: a server image is
//! immutable, and a newer image replaces it. `--vhd` appends a fixed-VHD
//! footer, the disk format Azure takes and AWS imports.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

use super::build_iso::{live_medium, publish, refuse_stale, staged_path, Admitted, LiveMedium};
use super::{
    build_btrfs_tools, build_system_stem, create_persistent_volume_layout, create_scratch_dir,
    media, provision_selector, RunVolume, Scratch, VerifiedSelector, VolumeLayout, VolumePurpose,
};
use crate::check_runner::RecipeCheckRunner;
use crate::checks::provisioned_account::{check_keys, ProvisionedAccount, KEY_TYPE};
use td_engine::{fat, gpt};
use td_recipe::td_boot_protocol as protocol;

pub(crate) const STEM: &str = super::server::STEM;
pub(crate) const DEFAULT_RAW_OUT: &str = "dist/td-server-x86-64.img";
pub(crate) const DEFAULT_VHD_OUT: &str = "dist/td-server-x86-64.vhd";
pub(crate) const DEFAULT_ISO_OUT: &str = "dist/td-server-x86-64.iso";
const GIB: u64 = 1024 * 1024 * 1024;
pub(crate) const DEFAULT_DISK_GIB: u64 = 8;
const MAX_DISK_GIB: u64 = 2048;
/// The largest disk a fixed VHD describes to Hyper-V and qemu.
const MAX_VHD_GIB: u64 = 2040;
/// Firmware and the clouds present a 512-byte logical sector to a disk
/// built from an image file.
const SECTOR_BYTES: u64 = 512;
/// A public key file larger than this is not one OpenSSH wrote.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024;
const COPY_CHUNK: usize = 1024 * 1024;
/// mkfs.btrfs sizes a volume in whole sectors of this many bytes; the
/// partition's tail past the last one stays unused.
const BTRFS_SECTOR_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerImageOptions {
    pub(crate) out: PathBuf,
    /// The primary account's name; `$USER` when not given.
    pub(crate) user: Option<String>,
    /// Public key files; the Ed25519 `~/.ssh/*.pub` when none are given.
    pub(crate) keys: Vec<PathBuf>,
    /// The disk's size; an ISO has none of its own.
    pub(crate) disk_gib: u64,
    pub(crate) format: Format,
}

pub(crate) fn usage() -> String {
    format!(
        "usage: server-image [--out FILE] [--user NAME] [--authorized-keys FILE]... \
         [--size GIB] [--vhd | --iso]\n       \
         --out FILE              where to write the image (default {DEFAULT_RAW_OUT},\n       \
         \x20                       {DEFAULT_VHD_OUT} with --vhd or {DEFAULT_ISO_OUT}\n       \
         \x20                       with --iso); an existing file is refused\n       \
         --user NAME             the account SSH admits (default $USER)\n       \
         --authorized-keys FILE  OpenSSH {KEY_TYPE} public keys it admits; repeatable\n       \
         \x20                       (default every ~/.ssh/*.pub holding only {KEY_TYPE}\n       \
         \x20                       keys)\n       \
         --size GIB              the disk's size (default {DEFAULT_DISK_GIB})\n       \
         --vhd                   append a fixed-VHD footer, the disk format Azure\n       \
         \x20                       takes and AWS imports (at most {MAX_VHD_GIB} GiB)\n       \
         --iso                   write a live medium instead, which boots the server\n       \
         \x20                       from optical or USB media with its state in RAM"
    )
}

pub(crate) fn parse_args(args: &[String]) -> Result<ServerImageOptions, String> {
    let mut out = None;
    let mut user = None;
    let mut keys = Vec::new();
    let mut disk_gib = None;
    let mut format = None;
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        let mut value = |what: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{argument} needs {what}\n{}", usage()))
        };
        let twice = || format!("{argument} given twice\n{}", usage());
        match argument.as_str() {
            "--vhd" | "--iso" => {
                let chosen = if argument == "--vhd" {
                    Format::Vhd
                } else {
                    Format::Iso
                };
                if format.replace(chosen).is_some() {
                    return Err(format!(
                        "--vhd and --iso are given once, alone\n{}",
                        usage()
                    ));
                }
            }
            "--out" => {
                if out.replace(PathBuf::from(value("a file")?)).is_some() {
                    return Err(twice());
                }
            }
            "--user" => {
                if user.replace(value("a name")?).is_some() {
                    return Err(twice());
                }
            }
            "--authorized-keys" => keys.push(PathBuf::from(value("a file")?)),
            "--size" => {
                let text = value("a size in GiB")?;
                let gib = text
                    .parse::<u64>()
                    .ok()
                    .filter(|gib| (1..=MAX_DISK_GIB).contains(gib))
                    .ok_or_else(|| {
                        format!("--size takes whole GiB from 1 to {MAX_DISK_GIB}, not {text:?}")
                    })?;
                if disk_gib.replace(gib).is_some() {
                    return Err(twice());
                }
            }
            other => return Err(format!("unknown argument '{other}'\n{}", usage())),
        }
    }
    let format = format.unwrap_or(Format::Raw);
    if format == Format::Iso && disk_gib.is_some() {
        return Err("--size sizes a disk; an ISO is sized by what it carries".into());
    }
    let disk_gib = disk_gib.unwrap_or(DEFAULT_DISK_GIB);
    if format == Format::Vhd && disk_gib > MAX_VHD_GIB {
        return Err(format!(
            "a fixed VHD describes at most {MAX_VHD_GIB} GiB, not {disk_gib}"
        ));
    }
    let default_out = match format {
        Format::Raw => DEFAULT_RAW_OUT,
        Format::Vhd => DEFAULT_VHD_OUT,
        Format::Iso => DEFAULT_ISO_OUT,
    };
    Ok(ServerImageOptions {
        out: out.unwrap_or_else(|| PathBuf::from(default_out)),
        user,
        keys,
        disk_gib,
        format,
    })
}

/// The account the options name, settled before any build: `$USER` and
/// the Ed25519 keys in `~/.ssh` unless told otherwise.
pub(crate) fn account(options: &ServerImageOptions) -> Result<ProvisionedAccount, String> {
    let name = match &options.user {
        Some(name) => name.clone(),
        None => std::env::var("USER")
            .ok()
            .filter(|name| !name.is_empty())
            .ok_or("$USER is unset; name the account with --user")?,
    };
    let keys = if options.keys.is_empty() {
        let home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .ok_or("$HOME is unset; name the keys with --authorized-keys")?;
        let ssh = Path::new(&home).join(".ssh");
        let found = default_keys(&ssh)?;
        for (skipped, reason) in &found.skipped {
            println!("   [server-image] skipping {}: {reason}", skipped.display());
        }
        if found.used.is_empty() {
            return Err(format!(
                "no {KEY_TYPE} public key in {}/*.pub; name one with --authorized-keys",
                ssh.display()
            ));
        }
        for used in &found.used {
            println!("   [server-image] admitting {}", used.display());
        }
        found.keys
    } else {
        let mut keys = String::new();
        for path in &options.keys {
            let text = read_key_file(path)?;
            check_keys(&text).map_err(|error| format!("{}: {error}", path.display()))?;
            keys.push_str(&text);
            keys.push('\n');
        }
        keys
    };
    ProvisionedAccount::new(STEM, &name, &keys)
}

struct DefaultKeys {
    keys: String,
    used: Vec<PathBuf>,
    /// Each file left out, and why.
    skipped: Vec<(PathBuf, String)>,
}

/// Every `*.pub` in `ssh` that holds only keys the account admits, in name
/// order. Any other is skipped rather than refused: a default should not
/// fail on an RSA key the operator never meant to name.
fn default_keys(ssh: &Path) -> Result<DefaultKeys, String> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(ssh).map_err(|error| format!("read {}: {error}", ssh.display()))? {
        let path = entry
            .map_err(|error| format!("read {}: {error}", ssh.display()))?
            .path();
        if path.extension().is_some_and(|extension| extension == "pub") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut found = DefaultKeys {
        keys: String::new(),
        used: Vec::new(),
        skipped: Vec::new(),
    };
    for path in paths {
        let admitted = read_key_file(&path).and_then(|text| {
            check_keys(&text)?;
            Ok(text)
        });
        match admitted {
            Ok(text) => {
                found.keys.push_str(&text);
                found.keys.push('\n');
                found.used.push(path);
            }
            Err(reason) => found.skipped.push((path, reason)),
        }
    }
    Ok(found)
}

/// A key file's text, holding at least one key line. Only a regular file
/// is opened, so a FIFO named `*.pub` cannot hang the run.
fn read_key_file(path: &Path) -> Result<String, String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut text = String::new();
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if text.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(format!(
            "{} is larger than a public key file ({MAX_KEY_FILE_BYTES} bytes)",
            path.display()
        ));
    }
    if text
        .lines()
        .map(str::trim)
        .all(|line| line.is_empty() || line.starts_with('#'))
    {
        return Err(format!("{} holds no key", path.display()));
    }
    Ok(text)
}

/// Settle `out` before the build: nothing may be there, and its directory
/// is created.
pub(crate) fn check_out(out: &Path) -> Result<(), String> {
    let raw = out.as_os_str().as_encoded_bytes();
    if raw.ends_with(b"/") || raw.ends_with(b"/.") {
        return Err(format!("{} names a directory, not a file", out.display()));
    }
    refuse_stale(&staged_path(out)?)?;
    match fs::symlink_metadata(out) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {error}", out.display())),
        Ok(_) => {
            return Err(format!(
                "{} already exists; remove it or name another --out",
                out.display()
            ))
        }
    }
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    Ok(())
}

/// `lock` is the ladder lock, held while the image is composed from the
/// ladder store.
pub(crate) fn run(
    runner: &RecipeCheckRunner,
    lock: File,
    options: &ServerImageOptions,
    account: &ProvisionedAccount,
) -> Result<(), String> {
    println!(
        "   [server-image] building the td server ({STEM}). An unchanged tree is\n            \
         reused whole; a cold tree builds every recipe.\n"
    );
    let inputs = ImageInputs::build(runner)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    let identity = root_owned_identity(runner)?;
    let staged = staged_path(&options.out)?;
    refuse_stale(&staged)?;
    let disk_bytes = options
        .disk_gib
        .checked_mul(GIB)
        .ok_or("the disk size overflows")?;
    let id = match options.format {
        Format::Iso => compose_iso(&inputs, account, &scratch.dir, &staged)?,
        format => compose(
            &inputs,
            &identity,
            account,
            disk_bytes,
            format,
            &scratch.dir,
            &staged,
        )?,
    };
    drop(lock);
    publish(&staged, &options.out, Admitted::Absent)?;
    let (what, boot) = match options.format {
        Format::Iso => (
            "a live medium".to_string(),
            "Boot it under UEFI firmware from optical or USB media, with 4 GiB of RAM;\n            \
             its state is lost at power-off.",
        ),
        _ => (
            format!("{} GiB", options.disk_gib),
            "Boot it under UEFI firmware with virtio or NVMe storage.",
        ),
    };
    println!(
        "   [server-image] wrote {} (deployment {id}): {what}, account {:?} with {} SSH key(s).\n            \
         {boot}",
        options.out.display(),
        account.name(),
        account.key_count()
    );
    Ok(())
}

/// The build outputs an image is composed from.
pub(crate) struct ImageInputs {
    pub(crate) kernel: PathBuf,
    pub(crate) selector: VerifiedSelector,
    pub(crate) deployment: PathBuf,
    pub(crate) mkfs: PathBuf,
    pub(crate) btrfs: PathBuf,
}

impl ImageInputs {
    pub(crate) fn build(runner: &RecipeCheckRunner) -> Result<Self, String> {
        let (kernel, selector, deployment) = build_system_stem(runner, STEM)?;
        let (mkfs, btrfs) = build_btrfs_tools(runner)?;
        Ok(Self {
            kernel,
            selector,
            deployment,
            mkfs,
            btrfs,
        })
    }
}

/// A volume identity whose files are guest root's, as an image's must be.
pub(crate) fn root_owned_identity(runner: &RecipeCheckRunner) -> Result<RunVolume, String> {
    RunVolume::generate_root_owned(PathBuf::from(runner.builder_command().get_program()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Raw,
    Vhd,
    /// A live medium (td-install/MEDIA.md "Live boot"), not a disk.
    Iso,
}

/// Write the live medium to `out`, which must not exist, returning the
/// deployment id: build-iso's medium of the server deployment, carrying the
/// account as a provision, which td-boot's live seed stages into the RAM
/// volume's `@var`.
pub(crate) fn compose_iso(
    inputs: &ImageInputs,
    account: &ProvisionedAccount,
    scratch: &Path,
    out: &Path,
) -> Result<String, String> {
    let LiveMedium {
        id,
        selector,
        mut payloads,
    } = live_medium(&inputs.selector, &inputs.deployment, scratch)?;
    let provision = scratch.join("provision");
    fs::write(&provision, account.provision())
        .map_err(|error| format!("write {}: {error}", provision.display()))?;
    payloads.push((protocol::MEDIA_PROVISION_NAME, provision));
    // The writer refuses an existing name; settled here, a failure after
    // this can only be one the writer reached by creating `out` itself.
    match fs::symlink_metadata(out) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("inspect {}: {error}", out.display())),
        Ok(_) => return Err(format!("{} already exists", out.display())),
    }
    if let Err(error) = media::write_image_with_payloads(out, &inputs.kernel, &selector, &payloads)
    {
        // Nothing was there, so this removes only the writer's own
        // publication, which its final sync can follow.
        let _ = fs::remove_file(out);
        return Err(error);
    }
    Ok(id)
}

/// Write the image to `out`, which must not exist, returning the
/// deployment id. `scratch` holds the volume until it is copied in.
pub(crate) fn compose(
    inputs: &ImageInputs,
    identity: &RunVolume,
    account: &ProvisionedAccount,
    disk_bytes: u64,
    format: Format,
    scratch: &Path,
    out: &Path,
) -> Result<String, String> {
    if format == Format::Iso {
        return Err("a live medium is composed by compose_iso, not as a disk".into());
    }
    let plan = DiskPlan::new(disk_bytes)?;
    let volume = scratch.join("server-image-volume.btrfs");
    let partition_bytes = plan.volume_bytes()?;
    let volume_bytes = partition_bytes - partition_bytes % BTRFS_SECTOR_BYTES;
    create_persistent_volume_layout(
        &inputs.deployment,
        &inputs.mkfs,
        &inputs.btrfs,
        &volume,
        VolumeLayout::Basic,
        false,
        identity,
        None,
        VolumePurpose::Image {
            bytes: volume_bytes,
        },
        None,
        Some(account),
    )?;
    let selector = provision_selector(&inputs.selector, scratch, identity)?;
    let id = crate::sha256::sha256_file(&inputs.deployment.join(protocol::MANIFEST_NAME))
        .map_err(|error| format!("hash the deployment manifest: {error}"))?;

    let mut random = [0u8; 52];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| format!("read /dev/urandom for the disk identity: {error}"))?;
    let (guids, serial) = random
        .split_first_chunk::<48>()
        .ok_or("short disk identity")?;
    let guid = |index: usize| -> Result<gpt::Guid, String> {
        let bytes: [u8; 16] = guids
            .get(index * 16..(index + 1) * 16)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or("short disk identity")?;
        Ok(v4_guid(bytes))
    };
    let serial: [u8; 4] = serial.try_into().map_err(|_| "short ESP serial")?;
    let disk_guid = guid(0)?;
    let layout = plan.layout(disk_guid, guid(1)?, guid(2)?);
    let table = gpt::build(&layout)?;

    let image = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(out)
        .map_err(|error| format!("create {}: {error}", out.display()))?;
    // This call created `out`, so a failure removes only its own image.
    let written = (|| {
        image
            .set_len(disk_bytes)
            .map_err(|error| format!("size {}: {error}", out.display()))?;
        let write = |offset: u64, bytes: &[u8]| {
            image
                .write_all_at(bytes, offset)
                .map_err(|error| format!("write {} at {offset}: {error}", out.display()))
        };
        write(table.primary_offset, &table.primary)?;
        write(table.backup_offset, &table.backup)?;
        write_esp(
            &image,
            out,
            &plan,
            u32::from_le_bytes(serial),
            inputs,
            &selector,
        )?;
        copy_volume(&volume, volume_bytes, &image, out, plan.volume_offset()?)?;
        fs::remove_file(&volume)
            .map_err(|error| format!("remove {}: {error}", volume.display()))?;
        if format == Format::Vhd {
            write_vhd_footer(&image, out, disk_bytes, disk_guid.0)?;
        }
        image
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", out.display()))
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(out);
        return Err(error);
    }
    Ok(id)
}

/// Append the footer that makes the `disk_bytes` raw disk in `image` a
/// fixed VHD.
fn write_vhd_footer(
    image: &File,
    out: &Path,
    disk_bytes: u64,
    unique_id: [u8; 16],
) -> Result<(), String> {
    let len = image
        .metadata()
        .map_err(|error| format!("stat {}: {error}", out.display()))?
        .len();
    if len != disk_bytes {
        return Err(format!(
            "{} is {len} bytes, not the {disk_bytes}-byte disk its footer describes",
            out.display()
        ));
    }
    image
        .write_all_at(&vhd_footer(disk_bytes, unique_id), disk_bytes)
        .map_err(|error| format!("write the VHD footer of {}: {error}", out.display()))
}

/// td-install's partition plan (td-install/DESIGN.md §4) at a 512-byte
/// sector: the ESP at 1 MiB and the volume from the next MiB to the last
/// usable LBA.
#[derive(Debug, PartialEq, Eq)]
struct DiskPlan {
    disk_sectors: u64,
    esp_start: u64,
    esp_end: u64,
    volume_start: u64,
    volume_end: u64,
}

impl DiskPlan {
    fn new(disk_bytes: u64) -> Result<Self, String> {
        if !disk_bytes.is_multiple_of(protocol::PARTITION_ALIGN_BYTES) {
            return Err(format!(
                "a {disk_bytes}-byte disk is not whole {}-byte units",
                protocol::PARTITION_ALIGN_BYTES
            ));
        }
        let align = protocol::PARTITION_ALIGN_BYTES / SECTOR_BYTES;
        let disk_sectors = disk_bytes / SECTOR_BYTES;
        let last_usable = gpt::last_usable_lba(SECTOR_BYTES, disk_sectors)?;
        let first_usable = gpt::first_usable_lba(SECTOR_BYTES)?;
        let align_up = |lba: u64| lba.div_ceil(align).checked_mul(align);
        let esp_start = align_up(first_usable).ok_or("the ESP start overflows")?;
        let esp_end = esp_start + protocol::ESP_BYTES / SECTOR_BYTES - 1;
        let volume_start = align_up(esp_end + 1).ok_or("the volume start overflows")?;
        let plan = Self {
            disk_sectors,
            esp_start,
            esp_end,
            volume_start,
            volume_end: last_usable,
        };
        let volume_bytes = plan.volume_bytes()?;
        if volume_bytes < protocol::MIN_VOLUME_BYTES {
            return Err(format!(
                "a {disk_bytes}-byte disk leaves a {volume_bytes}-byte td volume, and it needs \
                 at least {}",
                protocol::MIN_VOLUME_BYTES
            ));
        }
        Ok(plan)
    }

    fn volume_bytes(&self) -> Result<u64, String> {
        self.volume_end
            .checked_sub(self.volume_start)
            .and_then(|sectors| sectors.checked_add(1))
            .and_then(|sectors| sectors.checked_mul(SECTOR_BYTES))
            .ok_or_else(|| "the disk holds no volume after its ESP".to_string())
    }

    fn volume_offset(&self) -> Result<u64, String> {
        self.volume_start
            .checked_mul(SECTOR_BYTES)
            .ok_or_else(|| "the volume offset overflows".to_string())
    }

    fn layout(&self, disk: gpt::Guid, esp: gpt::Guid, volume: gpt::Guid) -> gpt::Layout {
        gpt::Layout {
            sector_size: SECTOR_BYTES,
            disk_sectors: self.disk_sectors,
            disk_guid: disk,
            align_sectors: protocol::PARTITION_ALIGN_BYTES / SECTOR_BYTES,
            partitions: vec![
                gpt::Partition {
                    type_guid: gpt::TYPE_ESP,
                    unique_guid: esp,
                    start_lba: self.esp_start,
                    end_lba: self.esp_end,
                    attributes: 0,
                    name: protocol::ESP_PARTITION_NAME.to_string(),
                },
                gpt::Partition {
                    type_guid: gpt::TYPE_LINUX_FS,
                    unique_guid: volume,
                    start_lba: self.volume_start,
                    end_lba: self.volume_end,
                    attributes: 0,
                    name: protocol::VOLUME_PARTITION_NAME.to_string(),
                },
            ],
        }
    }
}

/// Version 4 and the RFC 4122 variant, in GPT's mixed-endian layout.
fn v4_guid(mut bytes: [u8; 16]) -> gpt::Guid {
    if let Some(byte) = bytes.get_mut(7) {
        *byte = (*byte & 0x0f) | 0x40;
    }
    if let Some(byte) = bytes.get_mut(8) {
        *byte = (*byte & 0x3f) | 0x80;
    }
    gpt::Guid(bytes)
}

/// Format the ESP over the image's zeroed space and stream the kernel and
/// selector into the places the formatter gave them.
fn write_esp(
    image: &File,
    out: &Path,
    plan: &DiskPlan,
    serial: u32,
    inputs: &ImageInputs,
    selector: &Path,
) -> Result<(), String> {
    let kernel_len = file_len(&inputs.kernel)?;
    let selector_len = file_len(selector)?;
    let initrd_name = protocol::EFI_INITRD_PATH
        .rsplit('\\')
        .next()
        .ok_or("the EFI initrd path names no file")?;
    let volume = fat::Volume {
        bytes_per_sector: SECTOR_BYTES as u32,
        total_sectors: plan.esp_end - plan.esp_start + 1,
        hidden_sectors: u32::try_from(plan.esp_start).map_err(|_| "the ESP starts too far")?,
        volume_id: serial,
        label: protocol::ESP_VOLUME_LABEL.to_string(),
        sectors_per_cluster: None,
        root: vec![(
            "EFI".into(),
            fat::Node::Dir(vec![(
                "BOOT".into(),
                fat::Node::Dir(vec![
                    (
                        protocol::EFI_BOOT_FILE.into(),
                        fat::Node::Stream(kernel_len),
                    ),
                    (initrd_name.into(), fat::Node::Stream(selector_len)),
                ]),
            )]),
        )],
    };
    let esp = fat::build(&volume)?;
    let esp_offset = plan
        .esp_start
        .checked_mul(SECTOR_BYTES)
        .ok_or("the ESP offset overflows")?;
    for extent in &esp.extents {
        image
            .write_all_at(&extent.bytes, esp_offset + extent.offset)
            .map_err(|error| format!("write the ESP into {}: {error}", out.display()))?;
    }
    let kernel_path = format!("\\EFI\\BOOT\\{}", protocol::EFI_BOOT_FILE);
    let payloads = [
        (kernel_path.as_str(), inputs.kernel.as_path(), kernel_len),
        (protocol::EFI_INITRD_PATH, selector, selector_len),
    ];
    if esp.placements.len() != payloads.len() {
        return Err("the ESP placed files the image does not carry".into());
    }
    for (name, source, len) in payloads {
        let placement = esp
            .placements
            .iter()
            .find(|placement| placement.path == name)
            .filter(|placement| placement.len == len)
            .ok_or_else(|| format!("the ESP has no {len}-byte placement for {name}"))?;
        copy_at(source, len, image, out, esp_offset + placement.offset)?;
    }
    Ok(())
}

fn file_len(path: &Path) -> Result<u64, String> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("stat {}: {error}", path.display()))
}

/// Copy exactly `len` bytes of `source` to `offset` in the image.
fn copy_at(source: &Path, len: u64, image: &File, out: &Path, offset: u64) -> Result<(), String> {
    let mut input =
        File::open(source).map_err(|error| format!("open {}: {error}", source.display()))?;
    let mut target = image
        .try_clone()
        .map_err(|error| format!("reopen {}: {error}", out.display()))?;
    target
        .seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {}: {error}", out.display()))?;
    let copied = std::io::copy(&mut (&mut input).take(len), &mut target)
        .map_err(|error| format!("copy {} into {}: {error}", source.display(), out.display()))?;
    let mut extra = [0u8; 1];
    if copied != len || input.read(&mut extra).map_err(|error| error.to_string())? != 0 {
        return Err(format!(
            "{} changed size while it was copied",
            source.display()
        ));
    }
    target
        .flush()
        .map_err(|error| format!("write {}: {error}", out.display()))
}

/// Copy the volume into its partition, leaving its zero runs as holes so the
/// image stays as sparse as the volume.
fn copy_volume(
    volume: &Path,
    volume_bytes: u64,
    image: &File,
    out: &Path,
    offset: u64,
) -> Result<(), String> {
    let len = file_len(volume)?;
    if len != volume_bytes {
        return Err(format!(
            "mkfs.btrfs made a {len}-byte volume where {volume_bytes} bytes were asked"
        ));
    }
    let mut input =
        File::open(volume).map_err(|error| format!("open {}: {error}", volume.display()))?;
    let mut chunk = vec![0u8; COPY_CHUNK];
    let mut at = 0u64;
    while at < len {
        let want = usize::try_from((len - at).min(COPY_CHUNK as u64))
            .map_err(|_| "chunk size overflows")?;
        let buffer = chunk.get_mut(..want).ok_or("chunk out of range")?;
        input
            .read_exact(buffer)
            .map_err(|error| format!("read {}: {error}", volume.display()))?;
        if buffer.iter().any(|byte| *byte != 0) {
            image
                .write_all_at(buffer, offset + at)
                .map_err(|error| format!("write the volume into {}: {error}", out.display()))?;
        }
        at += want as u64;
    }
    Ok(())
}

/// Virtual PC's fixed-disk footer (Microsoft's VHD format specification
/// 1.0): the raw disk followed by these 512 bytes is a fixed VHD whose
/// current size is the disk. Big-endian throughout.
fn vhd_footer(disk_bytes: u64, unique_id: [u8; 16]) -> Vec<u8> {
    const FEATURES_RESERVED: u32 = 2;
    const VERSION: u32 = 0x0001_0000;
    const NO_DATA_OFFSET: u64 = u64::MAX;
    const FIXED_DISK: u32 = 2;
    const CHECKSUM_AT: usize = 64;
    // The largest CHS geometry, as `qemu-img -o force_size` writes it:
    // every reader, Virtual PC's CHS-sized table included, then takes the
    // size field as the disk's size. A real geometry would size the disk to
    // whole cylinders, short of the backup GPT.
    const MAX_GEOMETRY: [u8; 4] = [0xff, 0xff, 0x10, 0xff];
    let mut footer = Vec::with_capacity(512);
    footer.extend_from_slice(b"conectix");
    footer.extend_from_slice(&FEATURES_RESERVED.to_be_bytes());
    footer.extend_from_slice(&VERSION.to_be_bytes());
    footer.extend_from_slice(&NO_DATA_OFFSET.to_be_bytes());
    // Seconds since 2000: zero, so the footer depends only on the disk.
    footer.extend_from_slice(&0u32.to_be_bytes());
    footer.extend_from_slice(b"td  ");
    footer.extend_from_slice(&VERSION.to_be_bytes());
    footer.extend_from_slice(b"Wi2k");
    footer.extend_from_slice(&disk_bytes.to_be_bytes());
    footer.extend_from_slice(&disk_bytes.to_be_bytes());
    footer.extend_from_slice(&MAX_GEOMETRY);
    footer.extend_from_slice(&FIXED_DISK.to_be_bytes());
    footer.extend_from_slice(&0u32.to_be_bytes());
    footer.extend_from_slice(&unique_id);
    footer.resize(512, 0);
    let checksum = vhd_checksum(&footer);
    if let Some(field) = footer.get_mut(CHECKSUM_AT..CHECKSUM_AT + 4) {
        field.copy_from_slice(&checksum.to_be_bytes());
    }
    footer
}

/// The one's complement of the byte sum, the checksum field counted as zero.
fn vhd_checksum(footer: &[u8]) -> u32 {
    let sum = footer
        .iter()
        .enumerate()
        .filter(|(index, _)| !(64..68).contains(index))
        .fold(0u32, |sum, (_, byte)| sum.wrapping_add(u32::from(*byte)));
    !sum
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_default_and_refuse_ambiguity() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            ServerImageOptions {
                out: PathBuf::from(DEFAULT_RAW_OUT),
                user: None,
                keys: Vec::new(),
                disk_gib: DEFAULT_DISK_GIB,
                format: Format::Raw,
            }
        );
        assert_eq!(
            parse_args(&args(&[
                "--vhd",
                "--user",
                "ada",
                "--authorized-keys",
                "a.pub",
                "--authorized-keys",
                "b.pub",
                "--size",
                "16",
            ]))
            .unwrap(),
            ServerImageOptions {
                out: PathBuf::from(DEFAULT_VHD_OUT),
                user: Some("ada".into()),
                keys: vec![PathBuf::from("a.pub"), PathBuf::from("b.pub")],
                disk_gib: 16,
                format: Format::Vhd,
            }
        );
        assert_eq!(
            parse_args(&args(&["--iso"])).unwrap().out,
            PathBuf::from(DEFAULT_ISO_OUT)
        );
        for bad in [
            vec!["--out"],
            vec!["--out", "a", "--out", "b"],
            vec!["--user", "a", "--user", "b"],
            vec!["--size", "0"],
            vec!["--size", "4097"],
            vec!["--size", "8", "--size", "9"],
            vec!["--size", "1.5"],
            vec!["--size", "2041", "--vhd"],
            vec!["--vhd", "--iso"],
            vec!["--iso", "--iso"],
            vec!["--iso", "--size", "8"],
            vec!["x.img"],
        ] {
            assert!(parse_args(&args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_plan_is_td_installs_layout() {
        let plan = DiskPlan::new(DEFAULT_DISK_GIB * GIB).unwrap();
        assert_eq!(plan.esp_start, 2048);
        assert_eq!(
            (plan.esp_end - plan.esp_start + 1) * SECTOR_BYTES,
            protocol::ESP_BYTES
        );
        assert_eq!(plan.volume_start, 2048 + protocol::ESP_BYTES / SECTOR_BYTES);
        assert_eq!(
            plan.volume_end,
            gpt::last_usable_lba(SECTOR_BYTES, plan.disk_sectors).unwrap()
        );
        let table =
            gpt::build(&plan.layout(v4_guid([1; 16]), v4_guid([2; 16]), v4_guid([3; 16]))).unwrap();
        let parsed = gpt::parse(&table.primary, &table.backup, SECTOR_BYTES).unwrap();
        let names: Vec<&str> = parsed.partitions.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            [
                protocol::ESP_PARTITION_NAME,
                protocol::VOLUME_PARTITION_NAME
            ]
        );
        assert_eq!(
            table.backup_offset + table.backup.len() as u64,
            DEFAULT_DISK_GIB * GIB
        );
        // A disk whose volume cannot hold two deployments is refused.
        assert!(DiskPlan::new(5 * GIB).is_err());
        assert!(DiskPlan::new(DEFAULT_DISK_GIB * GIB + 512).is_err());
    }

    #[test]
    fn the_vhd_footer_matches_the_specification() {
        // A fixed footer qemu-img wrote for an 8 GiB disk, up to its zero tail.
        let qemu: [u8; 85] = [
            0x63, 0x6f, 0x6e, 0x65, 0x63, 0x74, 0x69, 0x78, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01,
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x32, 0x71, 0x78, 0x86,
            0x71, 0x65, 0x6d, 0x32, 0x00, 0x05, 0x00, 0x03, 0x57, 0x69, 0x32, 0x6b, 0x00, 0x00,
            0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00,
            0xff, 0xff, 0x10, 0xff, 0x00, 0x00, 0x00, 0x02, 0xff, 0xff, 0xe6, 0xce, 0x5e, 0xb0,
            0x41, 0x21, 0xd1, 0x6c, 0x4b, 0x26, 0x83, 0x6b, 0x03, 0x77, 0x28, 0x7c, 0x2f, 0xf2,
            0x00,
        ];
        let mut reference = qemu.to_vec();
        reference.resize(512, 0);
        assert_eq!(vhd_checksum(&reference), 0xffff_e6ce);

        let footer = vhd_footer(8 * GIB, [7; 16]);
        assert_eq!(footer.len(), 512);
        assert_eq!(&footer[..16], &reference[..16]);
        assert_eq!(&footer[40..56], &reference[40..56]);
        assert_eq!(&footer[56..64], &reference[56..64]);
        assert_eq!(
            u32::from_be_bytes(footer[64..68].try_into().unwrap()),
            vhd_checksum(&footer)
        );
        assert_eq!(&footer[68..84], &[7; 16]);
        assert!(footer[84..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn the_vhd_footer_follows_exactly_the_disk() {
        let dir = crate::test_temp::TempDir::new("td-server-image-vhd").unwrap();
        let path = dir.path().join("disk.vhd");
        let image = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        let disk = 2 * 1024 * 1024;
        image.set_len(disk).unwrap();
        write_vhd_footer(&image, &path, disk, [9; 16]).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, disk + 512);
        let footer = &bytes[disk as usize..];
        assert_eq!(&footer[..8], b"conectix");
        assert_eq!(u64::from_be_bytes(footer[48..56].try_into().unwrap()), disk);
        assert_eq!(footer, vhd_footer(disk, [9; 16]).as_slice());
        // A second footer would describe a disk that already has one.
        assert!(write_vhd_footer(&image, &path, disk, [9; 16]).is_err());
    }

    #[test]
    fn default_keys_take_only_ed25519_files() {
        let dir = crate::test_temp::TempDir::new("td-server-image-keys").unwrap();
        let ed25519 =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl a";
        fs::write(dir.path().join("b.pub"), format!("{ed25519}\n")).unwrap();
        fs::write(dir.path().join("a.pub"), "ssh-rsa AAAA a\n").unwrap();
        fs::write(
            dir.path().join("c.pub"),
            format!("{ed25519}\nssh-rsa AAAA b\n"),
        )
        .unwrap();
        fs::write(dir.path().join("d.pub"), "# nothing\n").unwrap();
        fs::create_dir(dir.path().join("e.pub")).unwrap();
        fs::write(dir.path().join("id_ed25519"), "private").unwrap();
        let found = default_keys(dir.path()).unwrap();
        assert_eq!(found.used, [dir.path().join("b.pub")]);
        let skipped: Vec<_> = found.skipped.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            skipped,
            ["a.pub", "c.pub", "d.pub", "e.pub"].map(|name| dir.path().join(name))
        );
        assert!(
            found.skipped[1].1.contains("line 2"),
            "{:?}",
            found.skipped[1]
        );
        assert_eq!(found.keys, format!("{ed25519}\n\n"));
    }
}
