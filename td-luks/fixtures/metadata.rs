//! Host/guest metadata transition oracle. See METADATA.md; not shipped.
#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
#[path = "../bin/dm_sys.rs"]
#[allow(dead_code, reason = "metadata oracle uses temporary crypt mappings")]
mod dm_sys;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code, reason = "standalone archive producer uses build alone")]
mod cpio;
#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

use std::os::unix::fs::FileExt;
use td_luks::{header, keyslot, reencrypt, write};
use write::Medium;
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";
const DATA: usize = 2 * 1024 * 1024;
fn run(program: &str, args: &[&str]) -> Result<()> {
    eprintln!("{program} {args:?}");
    let status = Command::new(program).args(args).status()?;
    if !status.success() {
        return Err(format!("{program}: {status}").into());
    }
    Ok(())
}
fn crypt(args: &[&str]) -> Result<()> {
    run("/cryptsetup", args)
}
struct Memory(Vec<u8>);
impl Medium for Memory {
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> std::io::Result<usize> {
        let bytes = self
            .0
            .get(offset as usize..)
            .ok_or_else(|| std::io::Error::other("offset"))?;
        let len = bytes.len().min(out.len());
        out.get_mut(..len)
            .ok_or_else(|| std::io::Error::other("out"))?
            .copy_from_slice(
                bytes
                    .get(..len)
                    .ok_or_else(|| std::io::Error::other("bytes"))?,
            );
        Ok(len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.0
            .get_mut(offset as usize..offset as usize + bytes.len())
            .ok_or_else(|| std::io::Error::other("bounds"))?
            .copy_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn size(&mut self) -> std::io::Result<u64> {
        Ok(self.0.len() as u64)
    }
}
fn metadata() -> Result<td_json::Json> {
    Ok(header::read_copy(&mut File::open("/dev/vda")?)?.json()?.0)
}
fn stock(image: &[u8], label: &str) -> Result<()> {
    eprintln!("checking {label}");
    let disk = File::options().read(true).write(true).open("/dev/vda")?;
    disk.write_all_at(image, 0)?;
    disk.sync_all()?;
    drop(disk);
    crypt(&["luksDump", "/dev/vda"])?;
    if metadata()?.get_path(&["config", "requirements"]).is_some() {
        crypt(&["repair", "--batch-mode", "--key-file", "/key", "/dev/vda"])?;
        if metadata()?.get_path(&["config", "requirements"]).is_some() {
            crypt(&[
                "reencrypt",
                "--resume-only",
                "--batch-mode",
                "--key-file",
                "/key",
                "--hotzone-size",
                "1M",
                "/dev/vda",
            ])?;
        }
    }
    crypt(&[
        "open",
        "--disable-keyring",
        "--key-file",
        "/key",
        "/dev/vda",
        "verify",
    ])?;
    let mut plain = vec![0; DATA];
    File::open("/dev/mapper/verify")?.read_exact(&mut plain)?;
    crypt(&["close", "verify"])?;
    if plain.iter().any(|b| *b != 0x5a) {
        return Err(format!("{label}: plaintext differs").into());
    }
    eprintln!("stock accepted and recovered {label}");
    Ok(())
}
fn guest() -> Result<()> {
    for dir in ["/dev", "/proc", "/sys", "/run", "/tmp"] {
        fs::create_dir_all(dir)?;
    }
    for (kind, target) in [("devtmpfs", "/dev"), ("proc", "/proc"), ("sysfs", "/sys")] {
        run("/td-init", &["mount", "-t", kind, kind, target])?;
    }
    fs::write("/key", KEY)?;
    let initial = include_bytes!("../fixtures/fresh.bin");
    let device = dm_sys::device_number(fs::metadata("/dev/vda")?.rdev());
    let control = dm_sys::Control::open()?;
    for mode in [
        reencrypt::Resilience::Checksum,
        reencrypt::Resilience::Journal,
    ] {
        let mut mem = Memory(initial.to_vec());
        mem.0.resize(write::DATA_OFFSET as usize + DATA, 0);
        let (plan, keys) = reencrypt::plan_start(&mut mem, &mut keyslot::DevRandom, 0, KEY, mode)?;
        let mut ciphertext = Vec::new();
        for key in [&keys.old, &keys.new] {
            let status = control.create_crypt(
                "encode",
                "CRYPT-TEMP-td-luks-oracle",
                DATA as u64 / 512,
                device,
                key.bytes(),
                false,
            )?;
            let (_, minor) = dm_sys::device_number(status.device);
            let mut output = File::options()
                .write(true)
                .open(format!("/dev/dm-{minor}"))?;
            output.write_all(&vec![0x5a; DATA])?;
            output.sync_all()?;
            drop(output);
            control.remove("encode")?;
            let mut bytes = vec![0; DATA];
            File::open("/dev/vda")?.read_exact_at(&mut bytes, write::DATA_OFFSET)?;
            ciphertext.push(bytes);
        }
        let old = ciphertext.first().ok_or("no old data")?;
        let new = ciphertext.get(1).ok_or("no new data")?;
        mem.write_at(write::DATA_OFFSET, old)?;
        reencrypt::commit(&mut mem, &mut keyslot::DevRandom, &plan)?;
        stock(&mem.0, &format!("{mode:?} start"))?;
        for offset in [0usize, 1024 * 1024] {
            let (header, layout) = reencrypt::load(&mut mem)?;
            let (_, opened) = reencrypt::unlock(&mut mem, &header, KEY)?;
            if opened.new.bytes() != keys.new.bytes() {
                return Err("new key differs".into());
            }
            let hotzone = old.get(offset..offset + 1024 * 1024).ok_or("old hotzone")?;
            let resilience = match mode {
                reencrypt::Resilience::Journal => hotzone.to_vec(),
                reencrypt::Resilience::Checksum => hotzone
                    .as_chunks::<4096>()
                    .0
                    .iter()
                    .flat_map(|block| td_fido::digest(block))
                    .collect(),
            };
            mem.write_at(layout.area.offset, &resilience)?;
            let mut plan = write::Plan {
                uuid: header.uuid,
                label: header.label,
                seqid: header.seqid + 1,
                used: header.used,
                areas: vec![],
                json: reencrypt::mark_hotzone(&header.json, DATA as u64, 1024 * 1024)?,
            };
            reencrypt::commit(&mut mem, &mut keyslot::DevRandom, &plan)?;
            for bytes in [0usize, 4096, 512 * 1024, 1024 * 1024] {
                mem.write_at(
                    write::DATA_OFFSET + offset as u64,
                    new.get(offset..offset + bytes).ok_or("new hotzone")?,
                )?;
                stock(
                    &mem.0,
                    &format!("{mode:?} dirty offset={offset} rewritten={bytes}"),
                )?;
            }
            let (header, _) = reencrypt::load(&mut mem)?;
            plan.seqid = header.seqid + 1;
            plan.used = header.used;
            plan.json = reencrypt::finish_hotzone(&header.json, DATA as u64)?;
            reencrypt::commit(&mut mem, &mut keyslot::DevRandom, &plan)?;
            stock(&mem.0, &format!("{mode:?} complete offset={offset}"))?;
        }
    }
    println!("TD-LUKS-METADATA-PASS");
    Ok(())
}
struct GuestProcess(Child);
impl Drop for GuestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn host(args: &[String]) -> Result<()> {
    let [kernel, init, cryptsetup, destination] = args else {
        return Err("usage: metadata-oracle KERNEL TD-INIT CRYPTSETUP NEW-OUTPUT-DIR".into());
    };
    let version = Command::new(cryptsetup).arg("--version").output()?;
    if !version.status.success()
        || version.stdout != b"cryptsetup 2.8.8 flags: BLKID KERNEL_CAPI \n"
    {
        return Err("need pinned cryptsetup 2.8.8 KERNEL_CAPI build".into());
    }
    if Path::new(destination).exists() {
        return Err("output directory already exists".into());
    }
    let scratch = temp_dir::TempDir::new("td-luks-capture")?;
    let executable = fs::read(std::env::current_exe()?)?;
    let init = fs::read(init)?;
    let cryptsetup = fs::read(cryptsetup)?;
    let archive = cpio::build(&[
        cpio::Entry {
            name: "dev",
            mode: 0o755,
            kind: cpio::Kind::Directory,
        },
        cpio::Entry {
            name: "init",
            mode: 0o755,
            kind: cpio::Kind::File(&executable),
        },
        cpio::Entry {
            name: "td-init",
            mode: 0o755,
            kind: cpio::Kind::File(&init),
        },
        cpio::Entry {
            name: "cryptsetup",
            mode: 0o755,
            kind: cpio::Kind::File(&cryptsetup),
        },
    ])?;
    let archive_path = scratch.path().join("initramfs");
    fs::write(&archive_path, archive)?;
    let disk = scratch.path().join("volume");
    if disk.to_str().is_none_or(|name| name.contains(',')) {
        return Err("QEMU disk path must be UTF-8 without commas".into());
    }
    File::create(&disk)?.set_len(18 << 20)?;
    let log = scratch.path().join("guest.log");
    let mut child = GuestProcess(
        Command::new("qemu-system-x86_64")
            .args([
                "-enable-kvm",
                "-m",
                "512",
                "-smp",
                "2",
                "-nographic",
                "-no-reboot",
                "-nic",
                "none",
                "-kernel",
            ])
            .arg(kernel)
            .arg("-initrd")
            .arg(archive_path)
            .args(["-append", "console=ttyS0 panic=-1", "-drive"])
            .arg(format!("file={},format=raw,if=virtio", disk.display()))
            .stdin(Stdio::null())
            .stdout(File::create(&log)?)
            .stderr(File::options().append(true).open(&log)?)
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(600);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.0.kill()?;
            child.0.wait()?;
            eprintln!("{}", fs::read_to_string(&log)?);
            return Err("QEMU timed out".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let log_bytes = fs::read(&log)?;
    eprintln!("{}", String::from_utf8_lossy(&log_bytes));
    if !status.success() {
        return Err(format!("QEMU: {status}").into());
    }
    if !String::from_utf8_lossy(&log_bytes).contains("TD-LUKS-METADATA-PASS") {
        return Err("guest failed".into());
    }
    fs::create_dir(destination)?;
    fs::copy(&disk, Path::new(destination).join("volume.img"))?;
    fs::write(Path::new(destination).join("guest.log"), log_bytes)?;
    scratch.cleanup()?;
    Ok(())
}
fn main() -> ExitCode {
    if std::process::id() == 1 {
        if let Err(error) = guest() {
            eprintln!("TD-LUKS-METADATA-FAIL: {error}");
        }
        let _ = run("/td-init", &["poweroff", "-f"]);
        return ExitCode::FAILURE;
    }
    match host(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
