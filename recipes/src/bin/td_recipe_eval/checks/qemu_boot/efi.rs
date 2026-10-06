//! Host firmware oracle; its disposable disk never names an operator device.
use super::*;
const DISK_BYTES: u64 = 6 * 1024 * 1024 * 1024;
const NEGATIVE_SECONDS: u64 = 20;
const MAX_INPUT: u64 = 256 * 1024 * 1024;

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (code, vars_template) = firmware(&qemu)?;
    let (kernel, initramfs) = build_kernel(runner)?;
    runner.prepare_recipe_target("td-install")?;
    let build_out = runner.build_plan("td-install")?;
    let installer = runner
        .ladder_out_from(&build_out, "td-install")?
        .join("bin/td-install");
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    println!("   [qemu-boot-uefi] kernel: {}\n      initramfs: {}\n      firmware: {}\n      vars template: {}",
        kernel.display(), initramfs.display(), code.display(), vars_template.display());
    for (phase, present) in [("boot", true), ("reboot", true), ("missing", false)] {
        let disk = scratch
            .dir
            .join(if present { "boot.img" } else { "missing.img" });
        if phase != "reboot" {
            write_disk(&installer, &disk, &kernel, &initramfs, present)?;
        }
        let vars = scratch.dir.join(format!("{phase}-vars.fd"));
        copy_input(&vars_template, &vars)?;
        println!("   [qemu-boot-uefi] {phase}: cold firmware boot, EFI entry present: {present}");
        let result = boot_source(
            &qemu,
            BootSource::Firmware {
                code: &code,
                vars: &vars,
                attachment: FirmwareAttachment::Virtio,
                installation_target: None,
            },
            BootPlan {
                disk: Some(BootDisk::new(&disk, phase != "boot")),
                mem: "512",
                target_marker: MARKER,
                kill_on_marker: true,
                extra_append: "",
                user_net: false,
                audio: false,
                physical_input: false,
                capture_firefox_audio: false,
                tpm_socket: None,
                side_channel: None,
                answers: None,
                cut: false,
                keep_console: None,
                screen: None,
                shell: None,
            },
            &scratch.dir,
            if present {
                boot_timeout()
            } else {
                Duration::from_secs(NEGATIVE_SECONDS)
            },
        )?;
        if result.evidence.target != present {
            return Err(format!(
                "UEFI entry present={present}, userspace marker={} — {}\n{}",
                result.evidence.target,
                result.reason,
                tail(&result.console, 60),
            ));
        }
        // A missing executable should leave firmware running, not crash QEMU.
        if !present
            && (result.exited_clean
                || result.elapsed < Duration::from_secs(NEGATIVE_SECONDS)
                || !missing_entry_observed(&result.console))
        {
            return Err(format!("missing-entry firmware did not complete the expected refusal (elapsed {:?}) — {}\n{}",
                result.elapsed, result.reason, tail(&result.console, 60)));
        }
    }
    println!("PASS: x86-64 UEFI boots td-install's GPT/FAT BOOTX64.EFI and INITRD twice to {MARKER}; missing-entry control has no userspace marker");
    Ok(())
}

fn missing_entry_observed(console: &str) -> bool {
    console.lines().any(|line| {
        line.contains("BdsDxe: failed to load Boot")
            && line.contains("\"UEFI Misc Device\"")
            && line.ends_with(": Not Found")
    })
}

pub(crate) fn firmware(qemu: &str) -> Result<(PathBuf, PathBuf), String> {
    match (
        env::var_os("TD_QEMU_EFI_CODE"),
        env::var_os("TD_QEMU_EFI_VARS"),
    ) {
        (Some(code), Some(vars)) => checked_firmware(code.into(), vars.into()),
        (None, None) => {
            let executable = fs::canonicalize(qemu).map_err(|e| format!("resolve {qemu}: {e}"))?;
            let prefix = executable
                .parent()
                .and_then(Path::parent)
                .ok_or("qemu has no installation prefix")?;
            let share = prefix.join("share/qemu");
            let candidates = [
                (
                    share.join("edk2-x86_64-code.fd"),
                    share.join("edk2-i386-vars.fd"),
                ),
                (
                    PathBuf::from("/usr/share/OVMF/OVMF_CODE_4M.fd"),
                    PathBuf::from("/usr/share/OVMF/OVMF_VARS_4M.fd"),
                ),
                (
                    PathBuf::from("/usr/share/OVMF/OVMF_CODE.fd"),
                    PathBuf::from("/usr/share/OVMF/OVMF_VARS.fd"),
                ),
            ];
            for (code, vars) in candidates {
                if code.is_file() && vars.is_file() {
                    return checked_firmware(code, vars);
                }
            }
            Err("UEFI firmware missing; set TD_QEMU_EFI_CODE and TD_QEMU_EFI_VARS to a matching non-Secure-Boot x86-64 firmware pair".into())
        }
        _ => Err("set both TD_QEMU_EFI_CODE and TD_QEMU_EFI_VARS, or neither".into()),
    }
}

fn checked_firmware(code: PathBuf, vars: PathBuf) -> Result<(PathBuf, PathBuf), String> {
    input(&code)?;
    input(&vars)?;
    Ok((code, vars))
}

pub(super) fn input(path: &Path) -> Result<(File, u64), String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_INPUT {
        return Err(format!(
            "{} must be a nonempty regular file of at most 256 MiB",
            path.display()
        ));
    }
    Ok((file, meta.len()))
}

pub(super) fn copy_input(source: &Path, destination: &Path) -> Result<(), String> {
    let (file, len) = input(source)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|e| format!("create {}: {e}", destination.display()))?;
    let count = std::io::copy(&mut file.take(len), &mut out).map_err(|e| e.to_string())?;
    if count != len {
        return Err("firmware template shortened during copy".into());
    }
    out.sync_all().map_err(|e| e.to_string())
}

fn write_disk(
    installer: &Path,
    path: &Path,
    kernel: &Path,
    initramfs: &Path,
    present: bool,
) -> Result<(), String> {
    // The source-built installer receives only an exclusively created scratch
    // file. Firmware subsequently reads exactly those bytes, with no media.
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    out.set_len(DISK_BYTES)
        .map_err(|e| format!("size {}: {e}", path.display()))?;
    drop(out);
    let mut command = Command::new(installer);
    command
        .arg("layout")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if present {
        command.arg(kernel).arg(initramfs);
    }
    let status = command
        .status()
        .map_err(|e| format!("run {}: {e}", installer.display()))?;
    if !status.success() {
        return Err(format!("td-install EFI layout failed: {status}"));
    }
    Ok(())
}

/// The removable-media loader. Named alone in a boot entry, firmware loads
/// it from the first filesystem holding it, removable media first.
const REMOVABLE_LOADER: &str = "\\EFI\\BOOT\\BOOTX64.EFI";
/// The firmware volume that holds edk2's variables (PI's
/// EFI_FIRMWARE_VOLUME_HEADER), checked as OVMF checks it, since OVMF
/// formats a volume it finds invalid afresh and an entry in it is lost.
const FV_FILE_SYSTEM_AT: usize = 0x10;
const FV_LENGTH_AT: usize = 0x20;
const FV_SIGNATURE: &[u8] = b"_FVH";
const FV_SIGNATURE_AT: usize = 0x28;
const FV_HEADER_LENGTH_AT: usize = 0x30;
const FV_REVISION_AT: usize = 0x37;
const FV_REVISION: u8 = 2;
/// edk2's variable store (MdeModulePkg VariableFormat.h) follows the volume
/// header: a store header, then variables whose headers start 4-byte
/// aligned, each name followed by its data with no padding on x86.
const STORE_HEADER: usize = 28;
const STORE_FORMATTED: u8 = 0x5a;
const STORE_HEALTHY: u8 = 0xfe;
const VARIABLE_START: [u8; 2] = [0xaa, 0x55];
const VARIABLE_ADDED: u8 = 0x3f;
/// Added and caught mid-deletion: edk2 still reads it while no added copy
/// replaces it.
const VARIABLE_DELETING: u8 = 0x3e;
/// A header edk2 has not finished writing: its sizes count as zero there.
const ERASED_STATE: u8 = 0xff;
const NV_DATA_VOLUME: [u8; 16] = guid(
    0xfff1_2b8d,
    0x7696,
    0x4c8b,
    [0xa9, 0x85, 0x27, 0x47, 0x07, 0x5b, 0x4f, 0x50],
);
const AUTHENTICATED_STORE: [u8; 16] = guid(
    0xaaf3_2c78,
    0x947b,
    0x439a,
    [0xa1, 0x80, 0x2e, 0x14, 0x4e, 0xc3, 0x77, 0x92],
);
const PLAIN_STORE: [u8; 16] = guid(
    0xddcf_3616,
    0x3275,
    0x4164,
    [0x98, 0xb6, 0xfe, 0x85, 0x70, 0x7f, 0xfe, 0x7d],
);
const GLOBAL_VARIABLE: [u8; 16] = guid(
    0x8be4_df61,
    0x93ca,
    0x11d2,
    [0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c],
);
/// Non-volatile, boot-service and runtime access, as every boot variable.
const BOOT_VARIABLE: u32 = 7;
const LOAD_OPTION_ACTIVE: u32 = 1;

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> [u8; 16] {
    let a = data1.to_le_bytes();
    let b = data2.to_le_bytes();
    let c = data3.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], data4[0], data4[1], data4[2], data4[3],
        data4[4], data4[5], data4[6], data4[7],
    ]
}

fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn bytes(image: &[u8], at: usize, count: usize) -> Result<&[u8], String> {
    at.checked_add(count)
        .and_then(|stop| image.get(at..stop))
        .ok_or_else(|| format!("firmware variables end inside the field at {at:#x}"))
}

fn read_u32(image: &[u8], at: usize) -> Result<u32, String> {
    bytes(image, at, 4)?
        .try_into()
        .map(u32::from_le_bytes)
        .map_err(|_| "firmware variable field truncated".to_string())
}

fn align4(offset: usize) -> Option<usize> {
    offset.checked_add(3).map(|end| end & !3)
}

/// Whether a global variable's NUL-terminated UTF-16 name is boot state a
/// boot entry of ours would compete with.
fn boot_state(name: &[u8]) -> Option<String> {
    let units: Vec<u16> = name
        .chunks_exact(2)
        .map(|unit| {
            u16::from_le_bytes([
                unit.first().copied().unwrap_or(0),
                unit.get(1).copied().unwrap_or(0),
            ])
        })
        .collect();
    let text = String::from_utf16(units.strip_suffix(&[0])?).ok()?;
    let option = text
        .strip_prefix("Boot")
        .is_some_and(|number| number.len() == 4 && number.bytes().all(|b| b.is_ascii_hexdigit()));
    (option || text == "BootOrder" || text == "BootNext").then_some(text)
}

/// The variable store in a firmware volume: where its first variable
/// header goes, where the store ends, and how long its headers are.
fn variable_store(image: &[u8]) -> Result<(usize, usize, usize), String> {
    if bytes(image, FV_SIGNATURE_AT, FV_SIGNATURE.len())? != FV_SIGNATURE
        || bytes(image, FV_FILE_SYSTEM_AT, 16)? != NV_DATA_VOLUME
    {
        return Err("not a firmware volume of variables".into());
    }
    let length = bytes(image, FV_LENGTH_AT, 8)?
        .try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| "firmware volume length truncated")?;
    if usize::try_from(length).ok() != Some(image.len()) {
        return Err(format!(
            "the firmware volume declares {length} bytes, not its file's {}",
            image.len()
        ));
    }
    if bytes(image, FV_REVISION_AT, 1)? != [FV_REVISION] {
        return Err("the firmware volume is not revision 2".into());
    }
    let store = bytes(image, FV_HEADER_LENGTH_AT, 2)?
        .try_into()
        .map(u16::from_le_bytes)
        .map(usize::from)
        .map_err(|_| "firmware volume header length truncated")?;
    // The store's variables start 4-byte aligned only after a header that is.
    if store % 4 != 0 {
        return Err(format!(
            "the firmware volume header length {store} is not 4-byte aligned"
        ));
    }
    let sum = bytes(image, 0, store)?
        .chunks_exact(2)
        .map(|word| {
            u16::from_le_bytes([
                word.first().copied().unwrap_or(0),
                word.get(1).copied().unwrap_or(0),
            ])
        })
        .fold(0u16, u16::wrapping_add);
    if sum != 0 {
        return Err("the firmware volume header checksum is wrong".into());
    }
    let header = match bytes(image, store, 16)? {
        signature if signature == AUTHENTICATED_STORE => 60,
        signature if signature == PLAIN_STORE => 32,
        _ => return Err("the firmware volume holds no variable store".into()),
    };
    if bytes(image, store + 20, 2)? != [STORE_FORMATTED, STORE_HEALTHY] {
        return Err("the variable store is not formatted and healthy".into());
    }
    let size = usize::try_from(read_u32(image, store + 16)?).map_err(|e| e.to_string())?;
    let end = store
        .checked_add(size)
        .filter(|end| *end <= image.len() && *end >= store + STORE_HEADER)
        .ok_or("the variable store does not fit its volume")?;
    Ok((store + STORE_HEADER, end, header))
}

/// Where the store's variables end: past every header edk2 would read,
/// refusing one it has not finished writing, and refusing boot state an
/// entry of ours would compete with.
fn variables_end(store: &[u8], start: usize, header: usize) -> Result<usize, String> {
    let mut offset = start;
    while store.get(offset..offset.saturating_add(2)) == Some(&VARIABLE_START[..]) {
        let state = bytes(store, offset + 2, 1)?;
        let attributes = read_u32(store, offset + 4)?;
        let name_size = read_u32(store, offset + header - 24)?;
        let data_size = read_u32(store, offset + header - 20)?;
        if state == [ERASED_STATE] || [attributes, name_size, data_size].contains(&u32::MAX) {
            return Err(format!(
                "the firmware variable at {offset:#x} is half written"
            ));
        }
        let name_size = usize::try_from(name_size).map_err(|e| e.to_string())?;
        let data_size = usize::try_from(data_size).map_err(|e| e.to_string())?;
        let vendor = bytes(store, offset + header - 16, 16)?;
        let name = bytes(store, offset + header, name_size)?;
        if (state == [VARIABLE_ADDED] || state == [VARIABLE_DELETING]) && vendor == GLOBAL_VARIABLE
        {
            if let Some(held) = boot_state(name) {
                return Err(format!("the firmware variables already hold {held}"));
            }
        }
        offset = (offset + header)
            .checked_add(name_size)
            .and_then(|next| next.checked_add(data_size))
            .and_then(align4)
            .filter(|next| *next <= store.len())
            .ok_or_else(|| format!("the firmware variable at {offset:#x} overruns its store"))?;
    }
    Ok(offset)
}

/// An `EFI_LOAD_OPTION` for the removable loader with `options` as the
/// image's load options.
fn load_option(description: &str, options: &str) -> Result<Vec<u8>, String> {
    let path = utf16z(REMOVABLE_LOADER);
    let node = u16::try_from(path.len() + 4).map_err(|_| "loader path too long")?;
    let mut file_path = vec![4, 4];
    file_path.extend(node.to_le_bytes());
    file_path.extend(path);
    file_path.extend([0x7f, 0xff, 4, 0]);
    let list = u16::try_from(file_path.len()).map_err(|_| "device path too long")?;
    let mut option = LOAD_OPTION_ACTIVE.to_le_bytes().to_vec();
    option.extend(list.to_le_bytes());
    option.extend(utf16z(description));
    option.extend(file_path);
    option.extend(utf16z(options));
    Ok(option)
}

/// One global variable as `header` bytes of header, its name and its data,
/// padded with erased bytes to the next header.
fn variable(header: usize, name: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    let name = utf16z(name);
    let mut bytes = VARIABLE_START.to_vec();
    bytes.extend([VARIABLE_ADDED, 0]);
    bytes.extend(BOOT_VARIABLE.to_le_bytes());
    // An authenticated store's monotonic count, time stamp and key index
    // stay zero for a variable no key signs.
    bytes.resize(header - 24, 0);
    bytes.extend(
        u32::try_from(name.len())
            .map_err(|_| "variable name too long")?
            .to_le_bytes(),
    );
    bytes.extend(
        u32::try_from(data.len())
            .map_err(|_| "variable data too long")?
            .to_le_bytes(),
    );
    bytes.extend(GLOBAL_VARIABLE);
    bytes.extend(name);
    bytes.extend(data);
    let padded = align4(bytes.len()).ok_or("variable length overflow")?;
    bytes.resize(padded, 0xff);
    Ok(bytes)
}

/// Gives `vars`, a private copy of the firmware's variables holding no
/// boot state, one boot entry, `Boot0000` first in `BootOrder`: the
/// removable loader, started with `options`, which the kernel's EFI stub
/// appends to its built-in command line.
pub(super) fn boot_entry(vars: &Path, description: &str, options: &str) -> Result<(), String> {
    let (file, len) = input(vars)?;
    let mut image = Vec::new();
    file.take(len)
        .read_to_end(&mut image)
        .map_err(|e| format!("read {}: {e}", vars.display()))?;
    let (start, end, header) =
        variable_store(&image).map_err(|e| format!("{}: {e}", vars.display()))?;
    let store = image.get(..end).ok_or("variable store truncated")?;
    let offset =
        variables_end(store, start, header).map_err(|e| format!("{}: {e}", vars.display()))?;
    let mut entry = variable(header, "Boot0000", &load_option(description, options)?)?;
    entry.extend(variable(header, "BootOrder", &0u16.to_le_bytes())?);
    let free = offset
        .checked_add(entry.len())
        .filter(|stop| *stop <= end)
        .and_then(|stop| image.get_mut(offset..stop))
        .ok_or("the variable store has no room for a boot entry")?;
    if free.iter().any(|byte| *byte != 0xff) {
        return Err("the variable store's free space is not erased".into());
    }
    free.copy_from_slice(&entry);
    fs::write(vars, &image).map_err(|e| format!("write {}: {e}", vars.display()))
}

pub(super) fn pflash_arg(path: &Path, unit: u8, read_only: bool) -> OsString {
    let mut arg = OsString::from(format!(
        "if=pflash,format=raw,unit={unit},readonly={},file=",
        if read_only { "on" } else { "off" }
    ));
    let mut escaped = Vec::new();
    for &byte in path.as_os_str().as_bytes() {
        if byte == b',' {
            escaped.push(b',');
        }
        escaped.push(byte);
    }
    arg.push(OsString::from_vec(escaped));
    arg
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn negative_requires_the_firmware_disk_lookup_refusal() {
        assert!(missing_entry_observed("BdsDxe: failed to load Boot0001 \"UEFI Misc Device\" from PciRoot(0x0)/Pci(0x2,0x0): Not Found\r\n"));
        assert!(!missing_entry_observed(""));
        assert!(!missing_entry_observed("UEFI Interactive Shell v2.2"));
        assert!(!missing_entry_observed("BdsDxe: failed to load Boot0001 \"UEFI Misc Device\" from PciRoot(0x0): Security Violation"));
    }

    #[test]
    fn flash_paths_preserve_literal_commas_and_units() {
        assert_eq!(
            pflash_arg(Path::new("/scratch/a,b/code.fd"), 0, true),
            "if=pflash,format=raw,unit=0,readonly=on,file=/scratch/a,,b/code.fd"
        );
        assert_eq!(
            pflash_arg(Path::new("/scratch/vars.fd"), 1, false),
            "if=pflash,format=raw,unit=1,readonly=off,file=/scratch/vars.fd"
        );
    }

    /// Seals a volume header so its 16-bit words sum to zero.
    fn seal(image: &mut [u8]) {
        image[0x32..0x34].copy_from_slice(&[0, 0]);
        let sum = image[..0x48].chunks_exact(2).fold(0u16, |sum, word| {
            sum.wrapping_add(u16::from_le_bytes([word[0], word[1]]))
        });
        image[0x32..0x34].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
    }

    fn volume(store: [u8; 16], size: u32) -> Vec<u8> {
        let total = 0x48 + size as usize;
        let mut image = vec![0u8; 0x48];
        image[FV_FILE_SYSTEM_AT..FV_FILE_SYSTEM_AT + 16].copy_from_slice(&NV_DATA_VOLUME);
        image[FV_LENGTH_AT..FV_LENGTH_AT + 8].copy_from_slice(&(total as u64).to_le_bytes());
        image[FV_SIGNATURE_AT..FV_SIGNATURE_AT + 4].copy_from_slice(FV_SIGNATURE);
        image[FV_HEADER_LENGTH_AT..FV_HEADER_LENGTH_AT + 2].copy_from_slice(&0x48u16.to_le_bytes());
        image[FV_REVISION_AT] = FV_REVISION;
        seal(&mut image);
        image.extend(store);
        image.extend(size.to_le_bytes());
        image.extend([STORE_FORMATTED, STORE_HEALTHY, 0, 0, 0, 0, 0, 0]);
        image.resize(total, 0xff);
        image
    }

    const FIRST: usize = 0x48 + STORE_HEADER;

    /// The layout edk2 reads on x86: each name runs straight into its data,
    /// only headers are 4-byte aligned, and the entry names the removable
    /// loader with the options after it, past what the store already holds.
    #[test]
    fn a_boot_entry_is_written_as_edk2_reads_it() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        for (store, header) in [(AUTHENTICATED_STORE, 60), (PLAIN_STORE, 32)] {
            let vars = scratch.dir.join(format!("vars-{header}.fd"));
            let mut image = volume(store, 0x1000);
            // A variable firmware left, its name and data lengths not
            // multiples of four, and a deleted entry of the same number.
            let lang = variable(header, "Lang", b"eng").unwrap();
            let mut deleted = variable(header, "Boot0000", b"old").unwrap();
            deleted[2] = 0x3c;
            let held = [lang.clone(), deleted.clone()].concat();
            image[FIRST..FIRST + held.len()].copy_from_slice(&held);
            assert_eq!(lang.len(), align4(header + 10 + 3).unwrap());
            fs::write(&vars, &image).unwrap();
            boot_entry(&vars, "td", "audit=0 td.autotest=1").unwrap();
            let image = fs::read(&vars).unwrap();
            assert_eq!(image[FIRST..FIRST + held.len()], held);
            let first = FIRST + held.len();
            assert_eq!(image[first..first + 4], [0xaa, 0x55, VARIABLE_ADDED, 0]);
            assert_eq!(read_u32(&image, first + 4).unwrap(), BOOT_VARIABLE);
            assert_eq!(read_u32(&image, first + header - 24).unwrap(), 18);
            let data_size = read_u32(&image, first + header - 20).unwrap() as usize;
            assert_eq!(image[first + header - 16..first + header], GLOBAL_VARIABLE);
            assert_eq!(
                image[first + header..first + header + 18],
                utf16z("Boot0000")
            );
            let data = &image[first + header + 18..first + header + 18 + data_size];
            let mut expected = vec![1, 0, 0, 0, 52, 0];
            expected.extend(utf16z("td"));
            expected.extend([4, 4, 48, 0]);
            expected.extend(utf16z("\\EFI\\BOOT\\BOOTX64.EFI"));
            expected.extend([0x7f, 0xff, 4, 0]);
            expected.extend(utf16z("audit=0 td.autotest=1"));
            assert_eq!(data, expected);
            let order = align4(first + header + 18 + data_size).unwrap();
            assert_eq!(image[order..order + 2], VARIABLE_START);
            assert_eq!(read_u32(&image, order + header - 24).unwrap(), 20);
            assert_eq!(read_u32(&image, order + header - 20).unwrap(), 2);
            assert_eq!(
                image[order + header..order + header + 20],
                utf16z("BootOrder")
            );
            assert_eq!(image[order + header + 20..order + header + 22], [0, 0]);
            let free = align4(order + header + 22).unwrap();
            assert!(image[free..].iter().all(|byte| *byte == 0xff));
            // A store that already holds an entry is not given a second.
            let error = boot_entry(&vars, "td", "x").unwrap_err();
            assert!(error.contains("already hold Boot0000"), "{error}");
            assert_eq!(fs::read(&vars).unwrap(), image);
        }
    }

    /// What firmware would read differently, or boot before the entry,
    /// is refused and the file left as it was.
    #[test]
    fn a_store_firmware_would_read_otherwise_is_refused() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let header = 60;
        let at_first = |held: Vec<u8>| {
            let mut image = volume(AUTHENTICATED_STORE, 0x1000);
            image[FIRST..FIRST + held.len()].copy_from_slice(&held);
            image
        };
        let torn = |field: usize| {
            let mut held = variable(header, "Lang", b"eng").unwrap();
            held[field..field + 4].copy_from_slice(&[0xff; 4]);
            at_first(held)
        };
        let mut deleting = variable(header, "BootNext", &[1, 0]).unwrap();
        deleting[2] = VARIABLE_DELETING;
        let mut unwritten = variable(header, "Lang", b"eng").unwrap();
        unwritten[2] = ERASED_STATE;
        let mut overrun = variable(header, "Lang", b"eng").unwrap();
        overrun[header - 20..header - 16].copy_from_slice(&0x10_0000u32.to_le_bytes());
        let mut checksum = volume(AUTHENTICATED_STORE, 0x1000);
        checksum[0x32] ^= 1;
        let mut unaligned = volume(AUTHENTICATED_STORE, 0x1000);
        unaligned[FV_HEADER_LENGTH_AT] = 0x4a;
        seal(&mut unaligned);
        let mut length = volume(AUTHENTICATED_STORE, 0x1000);
        length[FV_LENGTH_AT] ^= 0x10;
        seal(&mut length);
        let mut revision = volume(AUTHENTICATED_STORE, 0x1000);
        revision[FV_REVISION_AT] = 1;
        seal(&mut revision);
        let mut unhealthy = volume(AUTHENTICATED_STORE, 0x1000);
        unhealthy[0x48 + 21] = 0xff;
        let mut dirty = volume(AUTHENTICATED_STORE, 0x1000);
        dirty[FIRST + 200] = 0;
        for (name, image, refusal) in [
            (
                "next",
                at_first(variable(header, "BootNext", &[1, 0]).unwrap()),
                "already hold BootNext",
            ),
            (
                "order",
                at_first(variable(header, "BootOrder", &[1, 0]).unwrap()),
                "already hold BootOrder",
            ),
            (
                "option",
                at_first(variable(header, "Boot00A1", b"x").unwrap()),
                "already hold Boot00A1",
            ),
            ("deleting", at_first(deleting), "already hold BootNext"),
            ("unwritten", at_first(unwritten), "half written"),
            ("attributes", torn(4), "half written"),
            ("name", torn(header - 24), "half written"),
            ("data", torn(header - 20), "half written"),
            ("overrun", at_first(overrun), "overruns"),
            ("checksum", checksum, "checksum"),
            ("unaligned", unaligned, "not 4-byte aligned"),
            ("length", length, "declares"),
            ("revision", revision, "revision"),
            ("unhealthy", unhealthy, "healthy"),
            ("dirty", dirty, "not erased"),
            ("full", volume(AUTHENTICATED_STORE, 0x80), "no room"),
            ("other", vec![0xff; 0x1000], "not a firmware volume"),
        ] {
            let vars = scratch.dir.join(format!("{name}.fd"));
            fs::write(&vars, &image).unwrap();
            let error = boot_entry(&vars, "td", "x").unwrap_err();
            assert!(error.contains(refusal), "{name}: {error}");
            assert_eq!(fs::read(&vars).unwrap(), image, "{name}");
        }
    }

    #[test]
    fn disk_creation_refuses_an_existing_destination() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let scratch = Scratch {
            dir: create_scratch_dir(&env::temp_dir(), &SEQ).unwrap(),
        };
        let source = scratch.dir.join("source");
        let disk = scratch.dir.join("disk");
        fs::write(&source, b"input").unwrap();
        fs::write(&disk, b"preserve").unwrap();
        assert!(write_disk(
            Path::new("/does-not-exist/td-install"),
            &disk,
            &source,
            &source,
            true
        )
        .is_err());
        assert_eq!(fs::read(&disk).unwrap(), b"preserve");
    }
}
