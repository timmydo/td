//! Capture and validate stock reencryption write prefixes in disposable guests.
#![forbid(unsafe_code)]
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use td_json::Json;
#[path = "../../engine/src/cpio.rs"]
#[allow(dead_code, reason = "only the archive producer is needed")]
mod cpio;
mod replay;
#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;
use replay::{DATA_LEN, DATA_OFFSET, DISK_LEN, PREFIX_LEN};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const KEY: &[u8] = b"000000000000000000000000000000000000000000000000";

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
fn metadata() -> Result<Json> {
    let output = Command::new("/cryptsetup")
        .args(["luksDump", "--dump-json-metadata", "/dev/vda"])
        .output()?;
    if !output.status.success() {
        return Err("stock metadata dump failed".into());
    }
    Ok(td_json::parse_slice(&output.stdout)?)
}
fn required(json: &Json) -> bool {
    json.get_path(&["config", "requirements", "mandatory"])
        .and_then(Json::as_array)
        .is_some_and(|values| {
            values
                .iter()
                .any(|v| v.as_str() == Some("online-reencrypt-v2"))
        })
}
/// Stock's clean all-new layout is indistinguishable from offset zero to
/// its forward-resume offset calculation. Preserve it as negative evidence.
fn unsafe_end(json: &Json) -> Result<bool> {
    let segments = json
        .get("segments")
        .and_then(Json::as_object)
        .ok_or("segments missing")?;
    let active = segments
        .iter()
        .filter(|(_, segment)| {
            !segment
                .get("flags")
                .and_then(Json::as_array)
                .is_some_and(|flags| {
                    flags
                        .iter()
                        .any(|f| f.as_str().is_some_and(|f| f.starts_with("backup-")))
                })
        })
        .collect::<Vec<_>>();
    let [(id, segment)] = active.as_slice() else {
        return Ok(false);
    };
    if segment
        .get("flags")
        .and_then(Json::as_array)
        .is_some_and(|flags| flags.iter().any(|f| f.as_str() == Some("in-reencryption")))
    {
        return Ok(false);
    }
    let digests = json
        .get("digests")
        .and_then(Json::as_object)
        .ok_or("digests missing")?;
    Ok(required(json)
        && digests.iter().any(|(_, d)| {
            d.get("keyslots")
                .and_then(Json::as_array)
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("1")))
                && d.get("segments")
                    .and_then(Json::as_array)
                    .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(id)))
        }))
}
fn check_plain(negative: bool) -> Result<()> {
    crypt(&[
        "open",
        "--disable-keyring",
        "--key-file",
        "/key",
        "/dev/vda",
        "fixture",
    ])?;
    let mut bytes = vec![0; DATA_LEN];
    File::open("/dev/mapper/fixture")?.read_exact(&mut bytes)?;
    crypt(&["close", "fixture"])?;
    let different = bytes.iter().filter(|b| **b != 0x5a).count();
    if negative != (different != 0) {
        return Err(format!(
            "plaintext mismatch bytes {different}, expected corruption {negative}"
        )
        .into());
    }
    if required(&metadata()?) {
        return Err("reencryption requirement remains".into());
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
    if fs::read("/dumped-key")? != (64u8..128).collect::<Vec<_>>() {
        return Err("final volume key differs".into());
    }
    fs::remove_file("/dumped-key")?;
    Ok(())
}
fn resume() -> Result<()> {
    crypt(&["repair", "--batch-mode", "--key-file", "/key", "/dev/vda"])?;
    if required(&metadata()?) {
        crypt(&[
            "reencrypt",
            "--resume-only",
            "--batch-mode",
            "--key-file",
            "/key",
            "--key-slot",
            "0",
            "--hotzone-size",
            "1M",
            "/dev/vda",
        ])?;
    }
    Ok(())
}
fn prepare(mode: &str) -> Result<()> {
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
        "/old-key",
        "--key-file",
        "/key",
        "/dev/vda",
    ])?;
    crypt(&[
        "open",
        "--disable-keyring",
        "--key-file",
        "/key",
        "/dev/vda",
        "fixture",
    ])?;
    let mut mapped = fs::OpenOptions::new()
        .write(true)
        .open("/dev/mapper/fixture")?;
    mapped.write_all(&vec![0x5a; DATA_LEN])?;
    mapped.sync_all()?;
    drop(mapped);
    crypt(&["close", "fixture"])?;
    let mut args = vec![
        "reencrypt",
        "--init-only",
        "--batch-mode",
        "--key-file",
        "/key",
        "--new-volume-key-file",
        "/new-key",
        "--new-key-size",
        "512",
        "--key-slot",
        "0",
        "--pbkdf",
        "pbkdf2",
        "--pbkdf-force-iterations",
        "1000",
        "--resilience",
        mode,
        "--hotzone-size",
        "1M",
    ];
    if mode == "checksum" {
        args.extend(["--resilience-hash", "sha256"]);
    }
    args.push("/dev/vda");
    crypt(&args)?;
    crypt(&["luksDump", "--dump-json-metadata", "/dev/vda"])
}
fn verify() -> Result<()> {
    let mut image = replay::initial(&fs::read("/base")?)?;
    let writes = replay::decode(&fs::read("/trace")?)?;
    let mut good = 0;
    let mut bad = 0;
    for cut in 0..=writes.len() {
        if let Some(write) = cut.checked_sub(1).and_then(|i| writes.get(i)) {
            replay::apply(&mut image, write)?;
        }
        let mut disk = fs::OpenOptions::new().write(true).open("/dev/vda")?;
        disk.write_all(&image)?;
        disk.sync_all()?;
        drop(disk);
        let negative = unsafe_end(&metadata()?)?;
        eprintln!("CUT {cut} BEGIN negative={negative}");
        resume()?;
        check_plain(negative)?;
        if negative {
            bad += 1;
        } else {
            good += 1;
        }
        eprintln!("CUT {cut} PASS negative={negative}");
    }
    if good == 0 || bad == 0 {
        return Err("missing positive or negative checkpoints".into());
    }
    eprintln!("VERIFIED {good} resumable checkpoints and {bad} stock end-state failures");
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
    fs::write("/old-key", (0u8..64).collect::<Vec<_>>())?;
    fs::write("/new-key", (64u8..128).collect::<Vec<_>>())?;
    match fs::read_to_string("/action")?.as_str() {
        "prepare-checksum" => prepare("checksum")?,
        "prepare-journal" => prepare("journal")?,
        "run" => {
            resume()?;
            check_plain(false)?;
        }
        "verify" => verify()?,
        _ => return Err("unknown guest action".into()),
    }
    println!("\nTD-LUKS-REENCRYPT-PASS");
    Ok(())
}
struct GuestProcess(Child);
impl Drop for GuestProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Vm {
    kernel: PathBuf,
    init: Vec<u8>,
    cryptsetup: Vec<u8>,
    executable: Vec<u8>,
}
impl Vm {
    fn boot(
        &self,
        directory: &Path,
        disk: &Path,
        action: &str,
        base: &[u8],
        trace: &[u8],
        writes: Option<&Path>,
    ) -> Result<()> {
        for path in std::iter::once(disk).chain(writes) {
            let text = path.to_str().ok_or("QEMU disk path is not UTF-8")?;
            if text.contains(',') {
                return Err("QEMU disk paths must not contain commas".into());
            }
        }
        fs::create_dir(directory)?;
        let archive = cpio::build(&[
            cpio::Entry {
                name: "dev",
                mode: 0o755,
                kind: cpio::Kind::Directory,
            },
            cpio::Entry {
                name: "init",
                mode: 0o755,
                kind: cpio::Kind::File(&self.executable),
            },
            cpio::Entry {
                name: "td-init",
                mode: 0o755,
                kind: cpio::Kind::File(&self.init),
            },
            cpio::Entry {
                name: "cryptsetup",
                mode: 0o755,
                kind: cpio::Kind::File(&self.cryptsetup),
            },
            cpio::Entry {
                name: "action",
                mode: 0o400,
                kind: cpio::Kind::File(action.as_bytes()),
            },
            cpio::Entry {
                name: "base",
                mode: 0o400,
                kind: cpio::Kind::File(base),
            },
            cpio::Entry {
                name: "trace",
                mode: 0o400,
                kind: cpio::Kind::File(trace),
            },
        ])?;
        let initramfs = directory.join("initramfs");
        fs::write(&initramfs, archive)?;
        let log = directory.join("guest.log");
        let mut command = Command::new("qemu-system-x86_64");
        command
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
            .arg(&self.kernel)
            .arg("-initrd")
            .arg(initramfs)
            .args(["-append", "console=ttyS0 panic=-1"]);
        if let Some(writes) = writes {
            File::create(writes)?;
            command.arg("-blockdev").arg(format!("driver=blklogwrites,node-name=volume,file.driver=file,file.filename={},log.driver=file,log.filename={}", disk.display(), writes.display())).args(["-device", "virtio-blk-pci,drive=volume"]);
        } else {
            command.arg("-drive").arg(format!(
                "file={},format=raw,if=virtio,cache=none",
                disk.display()
            ));
        }
        command
            .stdin(Stdio::null())
            .stdout(File::create(&log)?)
            .stderr(File::options().append(true).open(&log)?);
        let mut child = GuestProcess(command.spawn()?);
        let deadline =
            Instant::now() + Duration::from_secs(if action == "verify" { 900 } else { 180 });
        loop {
            if let Some(status) = child.0.try_wait()? {
                let bytes = fs::read_to_string(&log)?;
                if !status.success() || !bytes.lines().any(|l| l.trim() == "TD-LUKS-REENCRYPT-PASS")
                {
                    eprintln!("{bytes}");
                    return Err(format!("guest {action} failed: {status}").into());
                }
                break;
            }
            if Instant::now() >= deadline {
                child.0.kill()?;
                child.0.wait()?;
                eprintln!("{}", fs::read_to_string(&log)?);
                return Err(format!("guest {action} timed out").into());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("guest {action} passed");
        Ok(())
    }
}
fn host(args: &[String]) -> Result<()> {
    let [kernel, init, cryptsetup, destination] = args else {
        return Err("usage: generate-reencrypt KERNEL TD-INIT CRYPTSETUP NEW-OUTPUT-DIR".into());
    };
    if Path::new(destination).exists() {
        return Err("output exists".into());
    }
    let version = Command::new(cryptsetup).arg("--version").output()?;
    if !version.status.success()
        || version.stdout != b"cryptsetup 2.8.8 flags: BLKID KERNEL_CAPI \n"
    {
        return Err("need pinned cryptsetup 2.8.8 KERNEL_CAPI".into());
    }
    let vm = Vm {
        kernel: kernel.into(),
        init: fs::read(init)?,
        cryptsetup: fs::read(cryptsetup)?,
        executable: fs::read(std::env::current_exe()?)?,
    };
    let scratch = temp_dir::TempDir::new("td-luks-reencrypt")?;
    let mut outputs = Vec::new();
    for mode in ["checksum", "journal"] {
        let disk = scratch.path().join(format!("{mode}.img"));
        File::create(&disk)?.set_len(DISK_LEN as u64)?;
        let prep = scratch.path().join(format!("prepare-{mode}"));
        vm.boot(&prep, &disk, &format!("prepare-{mode}"), &[], &[], None)?;
        let bytes = fs::read(&disk)?;
        let mut base = bytes.get(..PREFIX_LEN).ok_or("short prefix")?.to_vec();
        base.extend_from_slice(bytes.get(DATA_OFFSET..).ok_or("short data")?);
        let writes = scratch.path().join(format!("{mode}.writes"));
        let run = scratch.path().join(format!("run-{mode}"));
        vm.boot(&run, &disk, "run", &[], &[], Some(&writes))?;
        let trace = replay::capture(&mut File::open(&writes)?)?;
        let verify = scratch.path().join(format!("verify-{mode}"));
        vm.boot(&verify, &disk, "verify", &base, &trace, None)?;
        outputs.push((format!("{mode}.base"), base));
        outputs.push((format!("{mode}.trace"), trace));
        for (name, path) in [("prepare", prep), ("run", run), ("verify", verify)] {
            outputs.push((
                format!("{mode}-{name}.log"),
                fs::read(path.join("guest.log"))?,
            ));
        }
    }
    fs::create_dir(destination)?;
    for (name, bytes) in outputs {
        fs::write(Path::new(destination).join(name), bytes)?;
    }
    scratch.cleanup()?;
    Ok(())
}
fn main() -> ExitCode {
    if std::process::id() == 1 {
        if let Err(error) = guest() {
            eprintln!("TD-LUKS-REENCRYPT-FAIL: {error}");
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
