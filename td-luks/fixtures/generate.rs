//! Host/guest fixture capture. See README.md; this is not a shipped program.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code, reason = "standalone archive producer uses build alone")]
mod cpio;
#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const SIZE: usize = 1 << 20;
const JSON_SIZE: usize = 16384;
const NAMES: &[&str] = &["fresh", "added", "token", "killed", "unbound"];
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";
const SECOND: &[u8] = b"ABCDEFGHABCDEFGHABCDEFGHABCDEFGH";
const PBKDF: &[&str] = &[
    "--pbkdf",
    "pbkdf2",
    "--pbkdf-force-iterations",
    "1000",
    "--hash",
    "sha256",
];

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
fn capture(output: &mut File) -> Result<()> {
    let mut bytes = vec![0; SIZE];
    File::open("/dev/vda")?.read_exact(&mut bytes)?;
    let dump = Command::new("/cryptsetup")
        .args(["luksDump", "--dump-json-metadata", "/dev/vda"])
        .output()?;
    if !dump.status.success() || dump.stdout.len() >= JSON_SIZE {
        return Err("metadata dump failed or too large".into());
    }
    crypt(&[
        "luksDump",
        "--dump-volume-key",
        "--batch-mode",
        "--key-file",
        "/key",
        "--volume-key-file",
        "/dumped-key",
        "/dev/vda",
    ])?;
    if fs::read("/dumped-key")? != fs::read("/volume-key")? {
        return Err("stock volume key differs".into());
    }
    fs::remove_file("/dumped-key")?;
    let mut after = vec![0; SIZE];
    File::open("/dev/vda")?.read_exact(&mut after)?;
    if after != bytes {
        return Err("stock read repaired header".into());
    }
    output.write_all(&bytes)?;
    let mut json = dump.stdout;
    json.resize(JSON_SIZE, 0);
    output.write_all(&json)?;
    output.sync_all()?;
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
    let mut output = fs::OpenOptions::new().write(true).open("/dev/vdb")?;
    // A completion marker is written only after every capture succeeds.
    output.seek(SeekFrom::Start(4096))?;
    capture(&mut output)?;
    let mut add = vec![
        "luksAddKey",
        "--batch-mode",
        "--key-file",
        "/key",
        "--new-key-slot",
        "1",
    ];
    add.extend(PBKDF);
    add.extend(["/dev/vda", "/second"]);
    crypt(&add)?;
    capture(&mut output)?;
    fs::write(
        "/token",
        br#"{"type":"td-protector","keyslots":["1"],"role":"first-boot"}"#,
    )?;
    crypt(&[
        "token",
        "import",
        "--token-id",
        "0",
        "--json-file",
        "/token",
        "/dev/vda",
    ])?;
    capture(&mut output)?;
    crypt(&[
        "luksKillSlot",
        "--batch-mode",
        "--key-file",
        "/key",
        "/dev/vda",
        "1",
    ])?;
    capture(&mut output)?;
    let mut unbound = vec![
        "luksAddKey",
        "--batch-mode",
        "--unbound",
        "--key-size",
        "512",
        "--volume-key-file",
        "/volume-key",
    ];
    unbound.extend(PBKDF);
    unbound.extend(["/dev/vda", "/second"]);
    crypt(&unbound)?;
    capture(&mut output)?;
    output.seek(SeekFrom::Start(0))?;
    output.write_all(b"td-luks-fixtures-v1\n")?;
    output.sync_all()?;
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
    let [kernel, init, cryptsetup, destination] = args else {
        return Err("usage: generate KERNEL TD-INIT CRYPTSETUP NEW-OUTPUT-DIR".into());
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
    let output = scratch.path().join("output");
    File::create(&disk)?.set_len(32 << 20)?;
    File::create(&output)?.set_len((4096 + NAMES.len() * (SIZE + JSON_SIZE)) as u64)?;
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
            .arg("-drive")
            .arg(format!("file={},format=raw,if=virtio", output.display()))
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
    let mut output = File::open(output)?;
    let mut marker = [0; b"td-luks-fixtures-v1\n".len()];
    output.read_exact(&mut marker)?;
    if &marker != b"td-luks-fixtures-v1\n" {
        return Err("guest did not finish".into());
    }
    output.seek(SeekFrom::Start(4096))?;
    fs::create_dir(destination)?;
    for name in NAMES {
        let mut bytes = vec![0; SIZE];
        output.read_exact(&mut bytes)?;
        fs::write(Path::new(destination).join(format!("{name}.bin")), bytes)?;
        let mut json = vec![0; JSON_SIZE];
        output.read_exact(&mut json)?;
        let used = json
            .iter()
            .position(|b| *b == 0)
            .ok_or("unterminated metadata")?;
        json.truncate(used);
        fs::write(Path::new(destination).join(format!("{name}.json")), json)?;
    }
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
