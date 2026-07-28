#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, FileType, Metadata, Permissions};
use std::os::unix::fs::{self as unix_fs, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;

const FRAMEBUFFER: &str = "/dev/fb0";
const INPUT_DIR: &str = "/dev/input";
const RUNTIME_BASE: &str = "/run/user";
const READY_MARKER: &str = "TD-SEAT-READY";

fn usage() -> String {
    "usage: td-seatd assign --uid UID --gid GID | \
     td-seatd probe --uid UID --gid GID | td-seatd selftest"
        .into()
}

#[derive(Clone, Copy)]
struct Account {
    uid: u32,
    gid: u32,
}

fn parse_account(args: &[String]) -> Result<Account, String> {
    let mut uid = None;
    let mut gid = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args
            .get(index)
            .ok_or_else(|| "missing account flag".to_string())?;
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--uid" if uid.is_none() => {
                uid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid uid '{value}'"))?,
                );
            }
            "--gid" if gid.is_none() => {
                gid = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("invalid gid '{value}'"))?,
                );
            }
            "--uid" | "--gid" => return Err(format!("duplicate flag '{flag}'")),
            _ => return Err(format!("unrecognised argument '{flag}'")),
        }
        index += 2;
    }
    let uid = uid.ok_or_else(|| "--uid is required".to_string())?;
    let gid = gid.ok_or_else(|| "--gid is required".to_string())?;
    if uid == 0 || gid == 0 {
        return Err("the graphical seat must not be assigned to uid or gid 0".into());
    }
    Ok(Account { uid, gid })
}

fn event_name(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

fn input_paths(input_dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(input_dir)
        .map_err(|e| format!("read input directory {}: {e}", input_dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("read input directory entry: {e}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if event_name(name) {
            paths.push(entry.path());
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err(format!(
            "{} contains no evdev event nodes",
            input_dir.display()
        ));
    }
    Ok(paths)
}

fn checked_metadata(path: &Path, require_char: bool) -> Result<Metadata, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let kind: FileType = metadata.file_type();
    if kind.is_symlink() {
        return Err(format!("refusing symlink {}", path.display()));
    }
    if require_char && !kind.is_char_device() {
        return Err(format!("{} is not a character device", path.display()));
    }
    if !require_char && !kind.is_file() {
        return Err(format!("{} is not a regular test file", path.display()));
    }
    Ok(metadata)
}

fn assign_path(path: &Path, account: Account, require_char: bool) -> Result<(), String> {
    checked_metadata(path, require_char)?;
    unix_fs::lchown(path, Some(account.uid), Some(account.gid))
        .map_err(|e| format!("lchown {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    verify_owner_mode(path, account, 0o600)
}

fn verify_owner_mode(path: &Path, account: Account, mode: u32) -> Result<(), String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let actual_mode = metadata.permissions().mode() & 0o7777;
    if metadata.uid() != account.uid || metadata.gid() != account.gid || actual_mode != mode {
        return Err(format!(
            "{} is {:04o} {}:{}, expected {:04o} {}:{}",
            path.display(),
            actual_mode,
            metadata.uid(),
            metadata.gid(),
            mode,
            account.uid,
            account.gid
        ));
    }
    Ok(())
}

fn prepare_runtime_base(path: &Path, require_root: bool) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(format!("{} exists but is not a directory", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|create| format!("mkdir {}: {create}", path.display()))?;
        }
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    }
    fs::set_permissions(path, Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let mode = metadata.permissions().mode() & 0o7777;
    if mode != 0o755 {
        return Err(format!("{} is {mode:04o}, expected 0755", path.display()));
    }
    if require_root && (metadata.uid() != 0 || metadata.gid() != 0) {
        return Err(format!(
            "{} is owned by {}:{}, expected root:root",
            path.display(),
            metadata.uid(),
            metadata.gid()
        ));
    }
    Ok(())
}

fn prepare_runtime(path: &Path, account: Account, require_root_base: bool) -> Result<(), String> {
    let base = path
        .parent()
        .ok_or_else(|| format!("runtime path {} has no parent", path.display()))?;
    prepare_runtime_base(base, require_root_base)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(format!("{} exists but is not a directory", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|create| format!("mkdir {}: {create}", path.display()))?;
        }
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    }
    unix_fs::lchown(path, Some(account.uid), Some(account.gid))
        .map_err(|e| format!("lchown {}: {e}", path.display()))?;
    fs::set_permissions(path, Permissions::from_mode(0o700))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    verify_owner_mode(path, account, 0o700)
}

fn assign(
    framebuffer: &Path,
    input_dir: &Path,
    runtime: &Path,
    account: Account,
    require_char: bool,
) -> Result<usize, String> {
    prepare_runtime(runtime, account, require_char)?;
    assign_path(framebuffer, account, require_char)?;
    let inputs = input_paths(input_dir)?;
    for path in &inputs {
        assign_path(path, account, require_char)?;
    }
    Ok(inputs.len())
}

fn probe(
    framebuffer: &Path,
    input_dir: &Path,
    runtime: &Path,
    account: Account,
    require_char: bool,
) -> Result<usize, String> {
    verify_owner_mode(runtime, account, 0o700)?;
    checked_metadata(framebuffer, require_char)?;
    verify_owner_mode(framebuffer, account, 0o600)?;
    let inputs = input_paths(input_dir)?;
    for path in &inputs {
        checked_metadata(path, require_char)?;
        verify_owner_mode(path, account, 0o600)?;
    }
    Ok(inputs.len())
}

fn run(args: &[String]) -> Result<(), String> {
    let command = args.first().ok_or_else(usage)?;
    if command == "selftest" {
        if args.get(1).is_some() {
            return Err(usage());
        }
        if !event_name("event0")
            || event_name("event")
            || parse_account(&["--uid".into(), "1000".into(), "--gid".into(), "1000".into()])
                .is_err()
            || parse_account(&["--uid".into(), "0".into(), "--gid".into(), "1000".into()]).is_ok()
        {
            return Err("seat parser selftest failed".into());
        }
        println!("TD-SEAT-SELFTEST-OK");
        return Ok(());
    }
    let account = parse_account(args.get(1..).ok_or_else(usage)?)?;
    let runtime = PathBuf::from(RUNTIME_BASE).join(account.uid.to_string());
    let inputs = match command.as_str() {
        "assign" => assign(
            Path::new(FRAMEBUFFER),
            Path::new(INPUT_DIR),
            &runtime,
            account,
            true,
        )?,
        "probe" => probe(
            Path::new(FRAMEBUFFER),
            Path::new(INPUT_DIR),
            &runtime,
            account,
            true,
        )?,
        _ => return Err(usage()),
    };
    println!(
        "{READY_MARKER} uid={} gid={} framebuffer={} inputs={inputs} runtime={}",
        account.uid,
        account.gid,
        FRAMEBUFFER,
        runtime.display()
    );
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("td-seatd: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "td-seatd-test-{}-{}",
                process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Scratch { path }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn event_names_are_narrow() {
        assert!(event_name("event0"));
        assert!(event_name("event123"));
        for bad in ["event", "event-1", "mouse0", "event0.bak"] {
            assert!(!event_name(bad), "{bad}");
        }
    }

    #[test]
    fn account_parser_rejects_root_and_duplicates() {
        assert!(
            parse_account(&["--uid".into(), "1000".into(), "--gid".into(), "1000".into()]).is_ok()
        );
        assert!(
            parse_account(&["--uid".into(), "0".into(), "--gid".into(), "1000".into()]).is_err()
        );
        assert!(parse_account(&[
            "--uid".into(),
            "1000".into(),
            "--uid".into(),
            "1001".into(),
            "--gid".into(),
            "1000".into()
        ])
        .is_err());
    }

    #[test]
    fn assignment_is_bounded_to_framebuffer_events_and_runtime() {
        let scratch = Scratch::new();
        let dev = scratch.path.join("dev");
        let input = dev.join("input");
        let run = scratch.path.join("run");
        let runtime = run.join("user/1000");
        fs::create_dir_all(&input).unwrap();
        fs::create_dir(&run).unwrap();
        fs::write(dev.join("fb0"), b"").unwrap();
        fs::write(input.join("event0"), b"").unwrap();
        fs::write(input.join("event12"), b"").unwrap();
        fs::write(input.join("mouse0"), b"untouched").unwrap();
        let metadata = fs::metadata(&scratch.path).unwrap();
        let account = Account {
            uid: metadata.uid(),
            gid: metadata.gid(),
        };

        let count = assign(&dev.join("fb0"), &input, &runtime, account, false).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            fs::read(input.join("mouse0")).unwrap(),
            b"untouched".to_vec()
        );
        assert_eq!(
            probe(&dev.join("fb0"), &input, &runtime, account, false).unwrap(),
            2
        );
        let base = fs::symlink_metadata(run.join("user")).unwrap();
        assert!(base.file_type().is_dir());
        assert_eq!(base.permissions().mode() & 0o7777, 0o755);
    }
}
