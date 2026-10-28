//! Host/guest activation oracle. See ACTIVATION.md; not a shipped program.
#![deny(unsafe_code)]
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
#[path = "../bin/dm_sys.rs"]
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
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";
const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";
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
fn td(args: &[&str], key: Option<&str>, expected: i32) -> Result<()> {
    eprintln!("/td-luks {args:?} => {expected}");
    let mut command = Command::new("/td-luks");
    command.args(args);
    if let Some(key) = key {
        command.stdin(File::open(key)?);
    } else {
        command.stdin(Stdio::null());
    }
    let status = command.status()?;
    if status.code() != Some(expected) {
        return Err(format!("td-luks unexpected {status}").into());
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
    run("/cryptsetup", &["--version"])?;
    fs::write("/key", KEY)?;
    fs::write("/second", SECOND)?;
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
        "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a",
        "--label",
        "td-system",
        "--volume-key-file",
        "/volume-key",
        "--key-file",
        "/key",
        "/dev/vda",
    ])?;
    let control = dm_sys::Control::open()?;
    let device = dm_sys::device_number(fs::metadata("/dev/vda")?.rdev());
    let key: [u8; 64] = std::array::from_fn(|i| i as u8);
    let uuid = "CRYPT-LUKS2-5a5a5a5a5a5a405a805a5a5a5a5a5a5a-ours";
    let length = 4096u64;
    td(&["status", "missing"], None, 4)?;
    td(&["open", "/dev/vda", "ours"], Some("/second"), 2)?;
    td(&["open", "/dev/vda", "ours"], Some("/key"), 0)?;
    if fs::read_link("/dev/mapper/ours")? != Path::new("../dm-0") {
        return Err("wrong mapper symlink".into());
    }
    if !control.matches_crypt("ours", length, device, &key)? {
        return Err("td table differs".into());
    }
    let status = control.status("ours")?.ok_or("no td mapping")?;
    if fs::metadata("/dev/mapper/ours")?.rdev() != status.device {
        return Err("mapper node identity differs".into());
    }
    if status.uuid != uuid {
        return Err("td UUID differs".into());
    }
    td(&["open", "/dev/vda", "ours"], Some("/key"), 1)?;
    if !control.matches_crypt("ours", length, device, &key)? {
        return Err("duplicate create changed table".into());
    }
    crypt(&["status", "ours"])?;
    td(&["status", "ours"], None, 0)?;
    control.set_suspended("ours", true)?;
    if !control
        .status("ours")?
        .ok_or("missing suspended device")?
        .suspended
    {
        return Err("not suspended".into());
    }
    control.set_suspended("ours", false)?;
    let plain = vec![0x5a; 8192];
    let mut data = File::options().write(true).open("/dev/mapper/ours")?;
    data.write_all(&plain)?;
    data.sync_all()?;
    drop(data);
    crypt(&["close", "ours"])?;
    if control.status("ours")?.is_some() {
        return Err("stock close left mapping".into());
    }
    crypt(&[
        "open",
        "--disable-keyring",
        "--key-file",
        "/key",
        "/dev/vda",
        "stock",
    ])?;
    if !control.matches_crypt("stock", length, device, &key)? {
        return Err("stock table differs".into());
    }
    let mut actual = vec![0; 8192];
    File::open("/dev/mapper/stock")?.read_exact(&mut actual)?;
    if actual != plain {
        return Err("stock plaintext differs".into());
    }
    if control
        .create_crypt("failed", uuid, length, (4095, 1048575), &key)
        .is_ok()
    {
        return Err("bad backing device accepted".into());
    }
    if control.status("failed")?.is_some() {
        return Err("failed load leaked mapping".into());
    }
    td(&["close", "stock"], None, 0)?;
    if fs::symlink_metadata("/dev/mapper/stock").is_ok() {
        return Err("close left mapper entry".into());
    }
    td(&["status", "stock"], None, 4)?;
    fs::write("/wrong-volume-key", [0u8; 64])?;
    td(
        &["open-volume-key", "/dev/vda", "raw", "/wrong-volume-key"],
        None,
        2,
    )?;
    td(
        &["open-volume-key", "/dev/vda", "raw", "/volume-key"],
        None,
        0,
    )?;
    if !control.matches_crypt("raw", length, device, &key)? {
        return Err("raw key table differs".into());
    }
    td(&["close", "raw"], None, 0)?;
    std::os::unix::fs::symlink("../dm-99", "/dev/mapper/stale")?;
    td(&["open", "0", "/dev/vda", "stale"], Some("/key"), 0)?;
    td(&["close", "stale"], None, 0)?;
    fs::write("/dev/mapper/occupied", b"keep")?;
    td(&["open", "/dev/vda", "occupied"], Some("/key"), 1)?;
    if fs::read("/dev/mapper/occupied")? != b"keep" || control.status("occupied")?.is_some() {
        return Err("unsafe mapper entry replacement".into());
    }
    td(
        &[
            "format",
            "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a",
            "td-system",
            "/dev/vda",
        ],
        Some("/key"),
        0,
    )?;
    crypt(&[
        "open",
        "--disable-keyring",
        "--key-file",
        "/key",
        "/dev/vda",
        "from-td",
    ])?;
    td(&["close", "from-td"], None, 0)?;
    // Use the current volume's key, since td-luks format changed it.
    td(&["volume-key", "/dev/vda", "/race-key"], Some("/key"), 0)?;
    for _ in 0..3 {
        let mut openers = Vec::new();
        for _ in 0..24 {
            openers.push(GuestProcess(
                Command::new("/td-luks")
                    .args(["open-volume-key", "/dev/vda", "race", "/race-key"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            ));
        }
        let mut winners = 0;
        for mut opener in openers {
            match opener.0.wait()?.code() {
                Some(0) => winners += 1,
                Some(1) => {}
                _ => return Err("unexpected concurrent open failure".into()),
            }
        }
        let race = control
            .status("race")?
            .ok_or("concurrent winner has no mapping")?;
        if winners != 1 || fs::metadata("/dev/mapper/race")?.rdev() != race.device {
            return Err("concurrent open lost the winner's published entry".into());
        }
        td(&["close", "race"], None, 0)?;
    }
    fs::remove_file("/race-key")?;
    let unbound = include_bytes!("unbound.bin");
    let mut volume = File::options().write(true).open("/dev/vda")?;
    volume.write_all(unbound)?;
    volume.sync_all()?;
    drop(volume);
    td(&["test", "1", "/dev/vda"], Some("/second"), 0)?;
    td(&["open", "1", "/dev/vda", "unbound"], Some("/second"), 1)?;
    td(&["open", "/dev/vda", "unbound"], Some("/second"), 2)?;
    fs::write("/ordinary", unbound)?;
    td(&["open", "/ordinary", "file"], Some("/key"), 1)?;
    if control.status("unbound")?.is_some() || control.status("file")?.is_some() {
        return Err("refused activation created a mapping".into());
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
    let [kernel, init, cryptsetup, td_luks, destination] = args else {
        return Err(
            "usage: activation-oracle KERNEL TD-INIT CRYPTSETUP TD-LUKS NEW-OUTPUT-DIR".into(),
        );
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
    let deadline = Instant::now() + Duration::from_secs(180);
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
