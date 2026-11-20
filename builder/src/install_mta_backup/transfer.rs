//! Move the newest completed logical dump and its original receipt over a private stream.
use super::{
    absolute, cleanup_interrupted, destination_lock, drill, inventory, private_file, prune, Result,
    Temporary, CHUNK, GIB,
};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::os::{fd::OwnedFd, unix::net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use td_engine::{allocation, sha256};

const MAGIC: &[u8] = b"td-mta-backup-v1\n";
const RECEIPT_LIMIT: u32 = 65536;
const RETAIN: usize = 2;

fn io_error(error: io::Error) -> String {
    allocation::format(format_args!("{error}")).unwrap_or_default()
}

fn entry(dir: &Path, name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        allocation::format(format_args!("{}/{name}", super::text(dir)?))
            .map_err(|_| "backup path allocation")?,
    ))
}

fn export<W: Write>(dir: &Path, mut out: W) -> Result<()> {
    let mut backup = drill::latest_for_copy(dir)?;
    let len = u32::try_from(backup.receipt.len()).map_err(|_| "backup receipt exceeds limit")?;
    out.write_all(MAGIC)
        .and_then(|_| out.write_all(&len.to_be_bytes()))
        .and_then(|_| out.write_all(&backup.receipt))
        .map_err(io_error)?;
    if io::copy(&mut backup.file, &mut out).map_err(io_error)? != backup.metadata.bytes {
        return Err("archive length changed during export".into());
    }
    out.flush().map_err(io_error)
}

fn header(input: &mut impl Read) -> Result<Vec<u8>> {
    let mut magic = [0; MAGIC.len()];
    input.read_exact(&mut magic).map_err(io_error)?;
    if magic != MAGIC {
        return Err("invalid backup stream".into());
    }
    let mut size = [0; 4];
    input.read_exact(&mut size).map_err(io_error)?;
    let len = u32::from_be_bytes(size);
    if !(1..=RECEIPT_LIMIT).contains(&len) {
        return Err("invalid backup receipt size".into());
    }
    let mut data = allocation::filled(len as usize, 0u8).map_err(|_| "receipt allocation")?;
    input.read_exact(&mut data).map_err(io_error)?;
    Ok(data)
}

fn copy_verified(
    mut input: impl Read,
    mut out: impl Write,
    meta: &drill::ScheduledReceipt,
) -> Result<()> {
    let mut remaining = meta.bytes;
    let mut buf = [0; CHUNK];
    let mut hash = sha256::Sha256::new();
    while remaining > 0 {
        let count = remaining.min(CHUNK as u64) as usize;
        let chunk = buf.get_mut(..count).ok_or("invalid backup chunk size")?;
        input.read_exact(chunk).map_err(io_error)?;
        out.write_all(chunk).map_err(io_error)?;
        hash.update(chunk);
        remaining -= count as u64;
    }
    if input.read(&mut [0]).map_err(io_error)? != 0
        || sha256::to_base16(&hash.finalize()) != meta.sha256
    {
        return Err("backup stream differs from source receipt".into());
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir).and_then(|f| f.sync_all()).map_err(io_error)
}

// Only a directory initialized by this receiver can be rotated. Never adopt a
// production backup destination or prune an unrelated collection of archives.
fn can_initialize(dir: &Path) -> Result<bool> {
    if super::receiver_initialized(dir)? {
        return Ok(false);
    }
    for item in fs::read_dir(dir).map_err(io_error)? {
        if item.map_err(io_error)?.file_name() != "lock" {
            return Err("initialize off-host copies in an empty private directory".into());
        }
    }
    Ok(true)
}

fn receive(
    dir: &Path,
    ceiling: u64,
    mut input: impl Read,
    finish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let receipt = header(&mut input)?;
    let meta = drill::parse_receipt(&receipt)?;
    if meta.bytes > ceiling {
        return Err("incoming backup exceeds configured ceiling".into());
    }
    receive_record(dir, ceiling, input, &receipt, &meta, finish)
}

fn receive_record(
    dir: &Path,
    ceiling: u64,
    input: impl Read,
    receipt: &[u8],
    meta: &drill::ScheduledReceipt,
    finish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    can_initialize(dir)?;
    let _lock = destination_lock(dir)?;
    if can_initialize(dir)? {
        private_file(&entry(dir, super::RECEIVER_MARKER)?)?
            .sync_all()
            .map_err(io_error)?;
        sync_dir(dir)?;
    }
    cleanup_interrupted(dir)?;
    // Keep two generations; allow a third during the verified transfer. A bad
    // incoming stream cannot cause even the older retained copy to be removed.
    // A crash after publishing the third complete dump can interrupt rotation.
    // Reclaim that completed generation before calculating room for another.
    prune(dir, 0, u64::MAX, RETAIN)?;
    let (archives, used, entries) = inventory(dir)?;
    // Existing generations were admitted under the ceiling in effect then.
    // Lowering the incoming limit must still leave them intact during staging.
    let retained = archives.iter().try_fold(0u64, |sum, archive| {
        sum.checked_add(archive.bytes)
            .ok_or("archive size overflow")
    })?;
    let max_total = retained.saturating_add(ceiling + 4 * u64::from(RECEIPT_LIMIT));
    let target = entry(dir, &meta.name)?;
    if let Some(last) = archives.last() {
        if meta.sequence < last.stamp {
            return Err("source backup is older than the recovery copy".into());
        }
        if meta.sequence == last.stamp {
            let original = super::super::install_mta_setup::read_private_input(
                &last.path.with_extension("json"),
                u64::from(RECEIPT_LIMIT),
            )?;
            if target != last.path
                || meta.bytes != last.bytes
                || receipt != original
                || sha256::sha256_file(&target).map_err(io_error)? != meta.sha256
            {
                return Err("conflicting existing recovery copy".into());
            }
            copy_verified(input, io::sink(), meta)?;
            finish()?;
            prune(dir, 0, max_total, RETAIN)?;
            return sync_dir(dir);
        }
    }
    if used
        .checked_add(meta.bytes)
        .and_then(|n| n.checked_add(receipt.len() as u64))
        .is_none_or(|n| n > max_total)
        || entries + 3 > super::MAX_ENTRIES
    {
        return Err("off-host recovery directory has insufficient capacity".into());
    }
    let mut staged = Temporary::create(target.with_extension("partial"))?;
    copy_verified(input, &mut staged.file, meta)?;
    finish()?;
    staged.file.sync_all().map_err(io_error)?;
    let mut record = Temporary::create(target.with_extension("json"))?;
    record
        .file
        .write_all(receipt)
        .and_then(|_| record.file.sync_all())
        .map_err(io_error)?;
    // Make the receipt durable before its matching .dump becomes discoverable.
    sync_dir(dir)?;
    fs::hard_link(&staged.path, &target).map_err(io_error)?;
    record.retained = true;
    sync_dir(dir)?;
    drop(staged);
    prune(dir, 0, max_total, RETAIN)?;
    sync_dir(dir)
}

pub(in crate::install_mta) fn valid_host(host: &str) -> bool {
    host.len() <= 128
        && host
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
pub(in crate::install_mta) use super::valid_ceiling;

struct Ssh(Child);
impl Drop for Ssh {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = crate::sys::kill_child_recorded(&mut self.0, "terminate backup SSH export");
            let _ = self.0.wait();
        }
    }
}

struct TimedStream {
    stream: UnixStream,
    until: Instant,
}
impl Read for TimedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = self
            .until
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(io::ErrorKind::TimedOut)?;
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

fn pull(dir: &Path, host: &str, ceiling: u64) -> Result<()> {
    let mut command = Command::new("ssh");
    command.args([
        "-nT",
        "-S",
        "none",
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
        "-oClearAllForwardings=yes",
        "-oForwardAgent=no",
        "-oForwardX11=no",
        "-oConnectTimeout=10",
        "-oServerAliveInterval=15",
        "-oServerAliveCountMax=3",
        "--",
        host,
        "td-mta-backup-export",
    ]);
    pull_from(dir, ceiling, command, Duration::from_secs(30 * 60))
}

fn pull_from(dir: &Path, ceiling: u64, mut command: Command, timeout: Duration) -> Result<()> {
    let (reader, writer) = UnixStream::pair().map_err(io_error)?;
    let until = Instant::now() + timeout;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null());
    crate::sandbox::die_with_parent(&mut command);
    let mut ssh = Ssh(command.spawn().map_err(io_error)?);
    drop(command);
    receive(
        dir,
        ceiling,
        TimedStream {
            stream: reader,
            until,
        },
        || loop {
            if let Some(status) = ssh.0.try_wait().map_err(io_error)? {
                return if status.success() {
                    Ok(())
                } else {
                    Err("SSH backup export failed".into())
                };
            }
            if Instant::now() >= until {
                return Err("SSH backup export timed out".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        },
    )
}

pub(in crate::install_mta) fn run(op: &str, args: &[String]) -> ExitCode {
    if args == ["--help"] {
        match op {
            "--export-latest-backup" => println!("--export-latest-backup --destination PATH\nWrite the newest verified receipt and dump to stdout. Protect this secret-bearing stream."),
            "--pull-backup" => println!("--pull-backup --host SSH_ALIAS --destination PATH [--max-archive-bytes N]\nPull through an authenticated export-only SSH command; retains the latest two archives."),
            _ => println!("--receive-backup --destination PATH [--max-archive-bytes N]\nRead a private backup stream into an owned 0700 directory. Retains the newest two copies. Default per-archive ceiling: 1 GiB."),
        }
        return ExitCode::SUCCESS;
    }
    let result = (|| {
        let mut dir = None;
        let mut limit = None;
        let mut host = None;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args.next().ok_or("missing backup transfer option value")?;
            match flag.as_str() {
                "--destination" if dir.is_none() => dir = Some(absolute(value)?),
                "--host" if op == "--pull-backup" && host.is_none() && valid_host(value) => {
                    host = Some(value.as_str())
                }
                "--max-archive-bytes" if op != "--export-latest-backup" && limit.is_none() => {
                    let n = value
                        .parse::<u64>()
                        .map_err(|_| "invalid archive ceiling")?;
                    if !valid_ceiling(n) {
                        return Err("invalid archive ceiling".into());
                    }
                    limit = Some(n);
                }
                _ => return Err("unknown or duplicate backup transfer option".into()),
            }
        }
        let path = dir.ok_or("missing --destination")?;
        if op == "--export-latest-backup" {
            if io::stdout().is_terminal() {
                return Err("refusing to send a backup to a terminal".into());
            }
            export(&path, io::stdout().lock())
        } else {
            drill::private_directory(&path)?;
            let ceiling = limit.unwrap_or(GIB);
            if op == "--pull-backup" {
                pull(&path, host.ok_or("missing --host")?, ceiling)
            } else {
                receive(&path, ceiling, io::stdin().lock(), || Ok(()))
            }
        }
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("backup transfer: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_temp::TempDir;
    use td_engine::sha256::hex_digest;

    fn stream(stamp: u64, data: &[u8]) -> Vec<u8> {
        let digest = hex_digest(data);
        let receipt = format!(
            concat!(
                r#"{{"schema":1,"created_unix_seconds":{},"archive":"/source/backup-{}-1.dump","#,
                r#""bytes":{},"sha256":"{}","restored":false,"pg_dump":"pg_dump (PostgreSQL) 17.10","#,
                r#""pg_restore":"pg_restore (PostgreSQL) 17.10"}}"#
            ),
            stamp,
            stamp,
            data.len(),
            digest
        );
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(receipt.len() as u32).to_be_bytes());
        bytes.extend_from_slice(receipt.as_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    #[test]
    fn recovery_copy_retries_and_rotates_only_verified_generations() {
        let tmp = TempDir::new("backup-stream").unwrap();
        let dir = tmp.path();
        let first = stream(1, b"one");
        let second = stream(2, b"two");
        let third = stream(3, b"three");
        for good in [&first, &second, &second] {
            receive(dir, 100, good.as_slice(), || Ok(())).unwrap();
        }
        assert!(receive(dir, 100, third.as_slice(), || Err(
            "SSH exited unsuccessfully".into()
        ))
        .is_err());
        assert!(!dir.join("backup-3-1.dump").exists());
        let mut damaged = third.clone();
        *damaged.last_mut().unwrap() = b'?';
        for bad in [
            damaged,
            third[..third.len() - 1].to_vec(),
            [third.as_slice(), b"!"].concat(),
            stream(2, b"wrong"),
            first.clone(),
        ] {
            assert!(receive(dir, 100, bad.as_slice(), || Ok(())).is_err());
            assert_eq!(inventory(dir).unwrap().0.len(), 2);
            assert!(!dir.join("backup-3-1.json").exists());
        }
        assert!(receive(dir, 4, third.as_slice(), || Ok(())).is_err());
        assert_eq!(fs::read(dir.join("backup-1-1.dump")).unwrap(), b"one");
        receive(dir, 100, third.as_slice(), || Ok(())).unwrap();
        assert!(!dir.join("backup-1-1.dump").exists());
        assert_eq!(fs::read(dir.join("backup-3-1.dump")).unwrap(), b"three");
        let original = header(&mut third.as_slice()).unwrap();
        assert_eq!(fs::read(dir.join("backup-3-1.json")).unwrap(), original);
    }

    #[test]
    fn lowering_incoming_limit_preserves_and_retires_larger_generations() {
        let tmp = TempDir::new("backup-smaller-limit").unwrap();
        let dir = tmp.path();
        let old = stream(1, &vec![0; 300_000]);
        receive(dir, 300_000, old.as_slice(), || Ok(())).unwrap();
        receive(dir, 1, stream(2, b"x").as_slice(), || Ok(())).unwrap();
        // A locally truncated retained generation still occupies a rotation
        // slot; the next verified generation must be able to replace it.
        File::options()
            .write(true)
            .truncate(true)
            .open(dir.join("backup-1-1.dump"))
            .unwrap();
        receive(dir, 1, stream(3, b"x").as_slice(), || Ok(())).unwrap();
        assert!(!dir.join("backup-1-1.dump").exists());
        assert_eq!(inventory(dir).unwrap().0.len(), 2);
    }

    #[test]
    fn pull_requires_a_successful_export_and_reaps_a_stalled_child() {
        let tmp = TempDir::new("backup-pull").unwrap();
        let dest = tmp.path().join("copies");
        fs::create_dir(&dest).unwrap();
        let source = tmp.path().join("export");
        fs::write(&source, stream(1, b"first")).unwrap();
        let command = |script: &str| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script, "export"]).arg(&source);
            cmd
        };
        let limit = Duration::from_secs(5);
        pull_from(&dest, 100, command(r#"exec cat "$1""#), limit).unwrap();
        fs::write(&source, stream(2, b"second")).unwrap();
        assert!(pull_from(&dest, 100, command(r#"cat "$1"; exit 7"#), limit).is_err());
        assert!(pull_from(
            &dest,
            100,
            command("exec sleep 10"),
            Duration::from_millis(150)
        )
        .is_err());
        assert!(!dest.join("backup-2-1.dump").exists());
        assert_eq!(fs::read(dest.join("backup-1-1.dump")).unwrap(), b"first");
    }

    #[test]
    fn receiver_refuses_to_adopt_or_rotate_an_existing_backup_destination() {
        let tmp = TempDir::new("backup-no-adopt").unwrap();
        let prior = tmp.path().join("backup-1-3.dump");
        private_file(&prior).unwrap().write_all(b"keep").unwrap();
        assert!(receive(tmp.path(), 100, stream(4, b"new").as_slice(), || Ok(())).is_err());
        assert_eq!(fs::read(&prior).unwrap(), b"keep");
        assert!(!tmp.path().join(super::super::RECEIVER_MARKER).exists());
    }
}
