//! Host/PID-1 reencryption oracle; see REENCRYPTION.md. Not shipped.
#![deny(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code)]
mod cpio;
#[path = "../bin/dm_sys.rs"]
#[allow(dead_code)]
mod dm_sys;
#[path = "../bin/lock.rs"]
#[allow(dead_code)]
mod lock;
#[allow(dead_code)]
mod replay;
#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";
const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";
const DATA: usize = 32 << 20;
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
fn td(args: &[&str]) -> Result<()> {
    eprintln!("td-luks {args:?}");
    let status = Command::new("/td-luks")
        .args(args)
        .stdin(File::open("/key")?)
        .status()?;
    if !status.success() {
        return Err(format!("td-luks: {status}").into());
    }
    Ok(())
}
fn metadata() -> Result<td_json::Json> {
    Ok(td_luks::header::read_copy(&mut File::open("/dev/vda")?)?
        .json()?
        .0)
}
fn required() -> Result<bool> {
    Ok(metadata()?.get_path(&["config", "requirements"]).is_some())
}
fn format() -> Result<Vec<u8>> {
    fs::write("/volume-key", (0u8..64).collect::<Vec<_>>())?;
    crypt(&[
        "luksFormat",
        "--batch-mode",
        "--type",
        "luks2",
        "--cipher",
        "aes-xts-plain64",
        "--key-size",
        "512",
        "--sector-size",
        "4096",
        "--hash",
        "sha256",
        "--pbkdf",
        "pbkdf2",
        "--pbkdf-force-iterations",
        "1000",
        "--use-random",
        "--luks2-metadata-size",
        "16384",
        "--luks2-keyslots-size",
        "16744448",
        "--offset",
        "32768",
        "--uuid",
        UUID,
        "--label",
        "td-system",
        "--volume-key-file",
        "/volume-key",
        "--key-file",
        "/key",
        "/dev/vda",
    ])?;
    let mut bytes = vec![0; 16 << 20];
    File::open("/dev/vda")?.read_exact_at(&mut bytes, 0)?;
    Ok(bytes)
}
fn seed(base: &[u8]) -> Result<()> {
    let disk = File::options().write(true).open("/dev/vda")?;
    disk.write_all_at(base, 0)?;
    disk.sync_all()?;
    drop(disk);
    td(&["open", "/dev/vda", "live"])?;
    let mut plain = File::options().write(true).open("/dev/mapper/live")?;
    plain.write_all(&vec![0x5a; DATA])?;
    plain.sync_all()?;
    Ok(())
}
fn no_temporary_maps() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut found = false;
        for entry in fs::read_dir("/sys/block")? {
            let uuid = fs::read_to_string(entry?.path().join("dm/uuid")).unwrap_or_default();
            found |= uuid.starts_with("CRYPT-TEMP-") && uuid.contains("-td-luks-");
        }
        if !found {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("td-luks temporary mapping remains".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn orphan() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let raw = dm_sys::device_number(File::open("/dev/vda")?.metadata()?.rdev());
    let uuid = format!("CRYPT-TEMP-{}-td-luks-999-0", UUID.replace('-', ""));
    dm_sys::Control::open()?.create_crypt(
        "temporary-cryptsetup-999-0",
        &uuid,
        DATA as u64 / 512,
        raw,
        &std::array::from_fn(|i| i as u8),
    )?;
    Ok(())
}
fn verify(expected: &[u8]) -> Result<()> {
    no_temporary_maps()?;
    let control = dm_sys::Control::open()?;
    for name in ["live-overlay", "live-hotzone-forward"] {
        if control.status(name)?.is_some() {
            return Err(format!("helper {name} remains").into());
        }
    }
    let mut actual = vec![0; expected.len()];
    File::open("/dev/mapper/live")?.read_exact(&mut actual)?;
    if actual != expected {
        return Err("td plaintext differs".into());
    }
    crypt(&["close", "live"])?;
    crypt(&["luksDump", "/dev/vda"])?;
    crypt(&[
        "open",
        "--disable-keyring",
        "--key-file",
        "/key",
        "/dev/vda",
        "stock",
    ])?;
    File::open("/dev/mapper/stock")?.read_exact(&mut actual)?;
    if actual != expected {
        return Err("stock plaintext differs".into());
    }
    td(&["close", "stock"])?;
    Ok(())
}
fn refused_under_lock(program: &str, args: &[&str], expected: i32) -> Result<()> {
    let mut child = GuestProcess(
        Command::new(program)
            .args(args)
            .stdin(File::open("/key")?)
            .spawn()?,
    );
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait()? {
            if status.code() != Some(expected) {
                return Err(format!("lock refusal {program}: {status}").into());
            }
            return Ok(());
        }
        if Instant::now() >= end {
            return Err("lock refusal waited".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn stack_edges() -> Result<()> {
    use std::os::unix::fs::{symlink, MetadataExt};
    let control = dm_sys::Control::open()?;
    let raw = dm_sys::device_number(File::open("/dev/vda")?.metadata()?.rdev());
    let compact = UUID.replace('-', "");
    let key = std::array::from_fn(|i| i as u8);
    for detached in [false, true] {
        let hot = control.create_table(
            "live-hotzone-forward",
            &format!("CRYPT-SUBDEV-{compact}-live-hotzone-forward"),
            &[dm_sys::Target {
                start: 0,
                length: DATA as u64 / 512,
                device: raw,
                offset: 32768,
                kind: dm_sys::Kind::Linear,
            }],
        )?;
        let overlay = control.create_crypt(
            "live-overlay",
            &format!("CRYPT-SUBDEV-{compact}-live-overlay"),
            DATA as u64 / 512,
            raw,
            &key,
        )?;
        for helper in [&hot, &overlay] {
            symlink(
                format!("../dm-{}", dm_sys::device_number(helper.device).1),
                format!("/dev/mapper/{}", helper.name),
            )?;
        }
        let top = dm_sys::Target {
            start: 0,
            length: DATA as u64 / 512,
            device: dm_sys::device_number(overlay.device),
            offset: 0,
            kind: dm_sys::Kind::Linear,
        };
        control.load_table("live", &[top])?;
        control.set_suspended("live", true)?;
        control.set_suspended("live", false)?;
        if detached {
            let zone = dm_sys::Target {
                start: 0,
                length: DATA as u64 / 512,
                device: dm_sys::device_number(hot.device),
                offset: 0,
                kind: dm_sys::Kind::Crypt { key: &key, iv: 0 },
            };
            control.load_table("live-overlay", &[zone])?;
            control.set_suspended("live-overlay", true)?;
            control.set_suspended("live-overlay", false)?;
            let final_top = dm_sys::Target {
                start: 0,
                length: DATA as u64 / 512,
                device: raw,
                offset: 32768,
                kind: dm_sys::Kind::Crypt { key: &key, iv: 0 },
            };
            control.load_table("live", &[final_top])?;
            control.set_suspended("live", true)?;
            control.set_suspended("live", false)?;
        }
        td(&["close", "live"])?;
        for name in ["live-overlay", "live-hotzone-forward"] {
            if control.status(name)?.is_some()
                || fs::symlink_metadata(format!("/dev/mapper/{name}")).is_ok()
            {
                return Err(
                    format!("unreferenced helper leaked: {name}, detached={detached}").into(),
                );
            }
        }
        crypt(&[
            "open",
            "--disable-keyring",
            "--key-file",
            "/key",
            "/dev/vda",
            "live",
        ])?;
        crypt(&["close", "live"])?;
        td(&["open", "/dev/vda", "live"])?;
    }
    // An unrelated UUID at the helper name must survive close.
    control.create_table(
        "live-hotzone-forward",
        "foreign-helper",
        &[dm_sys::Target {
            start: 0,
            length: DATA as u64 / 512,
            device: raw,
            offset: 32768,
            kind: dm_sys::Kind::Linear,
        }],
    )?;
    td(&["close", "live"])?;
    if control.status("live-hotzone-forward")?.is_none() {
        return Err("close removed foreign helper".into());
    }
    control.remove("live-hotzone-forward")?;
    td(&["open", "/dev/vda", "live"])?;
    Ok(())
}
fn locks(mode: &str) -> Result<()> {
    let held = lock::reencryption(UUID)?;
    let header = lock::open(Path::new("/dev/vda"), lock::Mode::Write, false)?;
    refused_under_lock("/td-luks", &["reencrypt", "0", mode, "/dev/vda", "live"], 1)?;
    refused_under_lock("/td-luks", &["reencrypt-resume", "/dev/vda", "live"], 1)?;
    refused_under_lock("/td-luks", &["open", "/dev/vda", "other"], 1)?;
    drop(header);
    refused_under_lock("/td-luks", &["reencrypt", "0", mode, "/dev/vda", "live"], 1)?;
    // Stock init-only takes the metadata lock, not the UUID lock.
    crypt(&[
        "reencrypt",
        "--init-only",
        "--batch-mode",
        "--key-file",
        "/key",
        "--pbkdf",
        "pbkdf2",
        "--pbkdf-force-iterations",
        "1000",
        "--resilience",
        mode,
        "--active-name",
        "live",
        "/dev/vda",
    ])?;
    refused_under_lock(
        "/cryptsetup",
        &[
            "reencrypt",
            "--resume-only",
            "--batch-mode",
            "--key-file",
            "/key",
            "--active-name",
            "live",
            "/dev/vda",
        ],
        5,
    )?;
    drop(held);
    td(&["reencrypt-resume", "/dev/vda", "live"])
}
fn concurrent(mode: &str) -> Result<Vec<u8>> {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let stopped = Arc::new(AtomicBool::new(false));
    let done = stopped.clone();
    let writer = std::thread::spawn(move || -> Result<u64> {
        let file = File::options()
            .read(true)
            .write(true)
            .open("/dev/mapper/live")?;
        let block = [0x6a; 4096];
        let mut read = [0; 4096];
        let mut count = 0;
        loop {
            for at in (0..DATA).step_by(DATA / 256) {
                file.write_all_at(&block, at as u64)?;
                file.sync_data()?;
                file.read_exact_at(&mut read, at as u64)?;
                if read != block {
                    return Err(format!("concurrent readback differs at {at}").into());
                }
                count += 1;
            }
            if done.load(Ordering::Acquire) {
                break;
            }
        }
        Ok(count)
    });
    let result = td(&["reencrypt", "0", mode, "/dev/vda", "live"]);
    stopped.store(true, Ordering::Release);
    let count = writer.join().map_err(|_| "writer panicked")??;
    result?;
    eprintln!("{mode}: {count} concurrent verified sector writes");
    let mut expected = vec![0x5a; DATA];
    for at in (0..DATA).step_by(DATA / 256) {
        expected
            .get_mut(at..at + 4096)
            .ok_or("expected bounds")?
            .fill(0x6a);
    }
    Ok(expected)
}
fn interrupt(mode: &str, finisher: &str) -> Result<()> {
    let mut child = GuestProcess(
        Command::new("/td-luks")
            .args(["reencrypt", "0", mode, "/dev/vda", "live"])
            .stdin(File::open("/key")?)
            .spawn()?,
    );
    let control = dm_sys::Control::open()?;
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        if child.0.try_wait()?.is_some() {
            return Err("finished before interruption".into());
        }
        if Instant::now() >= end {
            return Err("no dirty hotzone observed".into());
        }
        let dirty = metadata()
            .ok()
            .and_then(|j| td_luks::reencrypt::profile(&j, DATA as u64).ok())
            .is_some_and(|l| l.hotzone.is_some());
        if dirty
            && control
                .status("live-hotzone-forward")?
                .is_some_and(|s| s.suspended)
        {
            child.0.kill()?;
            child.0.wait()?;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    eprintln!("interrupted {mode}, finisher {finisher}");
    no_temporary_maps()?;
    td(&["close", "live"])?;
    if finisher == "td" {
        td(&["open", "/dev/vda", "live"])?;
        if required()? {
            td(&["reencrypt-resume", "/dev/vda", "live"])?;
        }
    } else {
        crypt(&["repair", "--batch-mode", "--key-file", "/key", "/dev/vda"])?;
        if required()? {
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
        crypt(&[
            "open",
            "--disable-keyring",
            "--key-file",
            "/key",
            "/dev/vda",
            "live",
        ])?;
    }
    Ok(())
}
fn prefixes() -> Result<()> {
    let mut checked = 0;
    let mut refused = 0;
    for (mode, base, trace) in [
        (
            "checksum",
            include_bytes!("reencrypt/checksum.base") as &[u8],
            include_bytes!("reencrypt/checksum.trace") as &[u8],
        ),
        (
            "journal",
            include_bytes!("reencrypt/journal.base") as &[u8],
            include_bytes!("reencrypt/journal.trace") as &[u8],
        ),
    ] {
        let mut image = replay::initial(base)?;
        let writes = replay::decode(trace)?;
        for cut in 0..=writes.len() {
            if let Some(write) = cut.checked_sub(1).and_then(|i| writes.get(i)) {
                replay::apply(&mut image, write)?;
            }
            let json = td_luks::header::read_copy(&mut std::io::Cursor::new(&image))?
                .json()?
                .0;
            if td_luks::reencrypt::profile(&json, 2 << 20).is_err() {
                refused += 1;
                continue;
            }
            let disk = File::options().write(true).open("/dev/vda")?;
            disk.write_all_at(&image, 0)?;
            disk.sync_all()?;
            drop(disk);
            td(&["open", "/dev/vda", "live"])?;
            let expected = vec![0x5a; 2 << 20];
            let mut plain = vec![0; 2 << 20];
            File::open("/dev/mapper/live")?.read_exact(&mut plain)?;
            if plain != expected {
                return Err(format!("{mode}/{cut}: recovered plaintext differs").into());
            }
            if required()? {
                td(&["reencrypt-resume", "/dev/vda", "live"])?;
            }
            verify(&expected)?;
            checked += 1;
            eprintln!("accepted {mode} prefix {cut}");
        }
    }
    if checked != 92 || refused != 14 {
        return Err(format!("wrong prefix counts {checked}/{refused}").into());
    }
    Ok(())
}
fn guest() -> Result<()> {
    for dir in ["/dev", "/proc", "/sys", "/run", "/tmp"] {
        fs::create_dir_all(dir)?;
    }
    for (kind, target) in [("devtmpfs", "/dev"), ("proc", "/proc"), ("sysfs", "/sys")] {
        run("/td-init", &["mount", "-t", kind, kind, target])?;
    }
    crypt(&["--version"])?;
    fs::write("/key", KEY)?;
    let cmdline = fs::read_to_string("/proc/cmdline")?;
    let mode = cmdline
        .split_whitespace()
        .find_map(|s| s.strip_prefix("td_luks_oracle="))
        .ok_or("missing mode")?;
    if mode == "prefixes" {
        prefixes()?;
    } else {
        let base = format()?;
        for resilience in ["checksum", "journal"] {
            let finishers: &[&str] = if mode == "interrupt" {
                &["td", "stock"]
            } else {
                &["td"]
            };
            for finisher in finishers {
                seed(&base)?;
                let expected = match mode {
                    "smoke" => {
                        stack_edges()?;
                        orphan()?;
                        td(&["reencrypt", "0", resilience, "/dev/vda", "live"])?;
                        vec![0x5a; DATA]
                    }
                    "locks" => {
                        locks(resilience)?;
                        vec![0x5a; DATA]
                    }
                    "interrupt" => {
                        interrupt(resilience, finisher)?;
                        vec![0x5a; DATA]
                    }
                    "concurrent" => concurrent(resilience)?,
                    _ => return Err("unknown mode".into()),
                };
                verify(&expected)?;
                eprintln!("passed {mode}/{resilience}/{finisher}");
            }
        }
    }
    println!("TD-LUKS-FIXTURES-PASS");
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
    let [mode, kernel, init, cryptsetup, td_luks, destination] = args else {
        return Err(
            "usage: reencryption-oracle MODE KERNEL TD-INIT CRYPTSETUP TD-LUKS NEW-OUTPUT-DIR"
                .into(),
        );
    };
    if !matches!(
        mode.as_str(),
        "smoke" | "prefixes" | "concurrent" | "interrupt" | "locks"
    ) {
        return Err("unknown oracle mode".into());
    }
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
    let td_luks = fs::read(td_luks)?;
    let archive = cpio::build(&[
        cpio::Entry {
            name: "td-luks",
            mode: 0o755,
            kind: cpio::Kind::File(&td_luks),
        },
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
    File::create(&disk)?.set_len(if mode == "prefixes" {
        18 << 20
    } else {
        48 << 20
    })?;
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
            .arg("-append")
            .arg(format!("console=ttyS0 panic=-1 td_luks_oracle={mode}"))
            .arg("-drive")
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
    if !String::from_utf8_lossy(&log_bytes).contains("TD-LUKS-FIXTURES-PASS") {
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
            eprintln!("TD-LUKS-FIXTURES-FAIL: {error}");
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
