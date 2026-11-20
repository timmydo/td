//! Select one published scheduled archive and verify the exact file used by the drill.
use super::{
    absolute, destination_lock, inventory, receipt_number, valid_ceiling, Archive, Json, Result,
};
use crate::install_mta::{install_mta_restore_engine, install_mta_setup::trusted_ancestors};
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use td_engine::allocation;

pub(super) struct ScheduledReceipt {
    pub name: String,
    pub sequence: u64,
    pub created: u64,
    pub bytes: u64,
    pub sha256: String,
    source: PathBuf,
}

impl ScheduledReceipt {
    pub(super) fn matches_archive(&self, archive: &Archive, receiver: bool) -> Result<()> {
        if archive.path.file_name() != Some(self.name.as_ref())
            || archive.stamp != self.sequence
            || archive.bytes != self.bytes
            || (!receiver && archive.path.as_os_str() != self.source.as_os_str())
        {
            return Err("scheduled archive does not match its receipt".into());
        }
        Ok(())
    }
}

pub(super) struct VerifiedArchive {
    pub metadata: ScheduledReceipt,
    pub receipt: Vec<u8>,
    pub file: File,
}

// The writer uses a flat, scalar object. Bound nesting before the general
// JSON reader, whose caller owns its recursion limit.
fn flat_object(bytes: &[u8]) -> bool {
    let mut object = false;
    let mut quoted = false;
    let mut escaped = false;
    for &byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' if !object => object = true,
                b'{' | b'[' => return false,
                _ => (),
            }
        }
    }
    object
}

pub(super) fn parse_receipt(data: &[u8]) -> Result<ScheduledReceipt> {
    if data.is_empty() || data.len() > 65536 || !flat_object(data) {
        return Err("invalid scheduled backup receipt".into());
    }
    let receipt =
        crate::json::parse(std::str::from_utf8(data).map_err(|_| "invalid receipt encoding")?)
            .map_err(|_| "invalid scheduled backup receipt")?;
    let schema = receipt_number(&receipt, "schema")?;
    let bytes = receipt_number(&receipt, "bytes")?;
    let created = receipt_number(&receipt, "created_unix_seconds")?;
    let source = receipt
        .get("archive")
        .and_then(Json::as_str)
        .ok_or("missing receipt archive")?;
    let archive = absolute(source)?;
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|name| source.ends_with(*name))
        .ok_or("invalid receipt archive")?;
    let (sequence, pid) = name
        .strip_prefix("backup-")
        .and_then(|s| s.strip_suffix(".dump"))
        .and_then(|s| s.split_once('-'))
        .ok_or("invalid receipt filename")?;
    let decimal = |s: &str| {
        !s.is_empty()
            && s.bytes().all(|b| b.is_ascii_digit())
            && (s.len() == 1 || !s.starts_with('0'))
    };
    if !decimal(sequence) || !decimal(pid) || pid.parse::<u32>().ok().is_none_or(|v| v == 0) {
        return Err("invalid receipt filename".into());
    }
    let sequence = sequence
        .parse::<u64>()
        .map_err(|_| "invalid backup sequence")?;
    let sha256 = receipt
        .get("sha256")
        .and_then(Json::as_str)
        .ok_or("missing receipt digest")?;
    if schema != 1
        || !valid_ceiling(bytes)
        || created > sequence
        || receipt.get("restored") != Some(&Json::Bool(false))
        || !receipt
            .get("pg_dump")
            .and_then(Json::as_str)
            .is_some_and(|v| v.starts_with("pg_dump (PostgreSQL) 17."))
        || !receipt
            .get("pg_restore")
            .and_then(Json::as_str)
            .is_some_and(|v| v.starts_with("pg_restore (PostgreSQL) 17."))
        || sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid scheduled backup receipt fields".into());
    }
    Ok(ScheduledReceipt {
        name: allocation::string(name).map_err(|_| "backup name allocation")?,
        sha256: allocation::string(sha256).map_err(|_| "backup digest allocation")?,
        sequence,
        created,
        bytes,
        source: archive,
    })
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    trusted_ancestors(path)?;
    let m = fs::symlink_metadata(path).map_err(|_| "cannot inspect backup/drill directory")?;
    if !m.is_dir() || m.uid() != crate::sys::getuid() || m.mode() & 0o777 != 0o700 {
        return Err("backup/drill directory must be owned by this user with mode 0700".into());
    }
    Ok(())
}

fn select_archive(destination: &Path) -> Result<VerifiedArchive> {
    // The writer holds this lock while publishing or pruning. Once we open the
    // completed inode, a later unlink cannot change the selected snapshot.
    let lock = destination_lock(destination)?;
    let (mut archives, _, _) = inventory(destination)?;
    let latest = archives.pop().ok_or("no completed scheduled archive")?;
    if archives.last().is_some_and(|a| a.stamp == latest.stamp) {
        return Err("ambiguous newest scheduled archive".into());
    }
    let receipt = crate::install_mta::install_mta_setup::read_private_input(
        &latest.path.with_extension("json"),
        65536,
    )?;
    let metadata = parse_receipt(&receipt)?;
    metadata.matches_archive(&latest, super::receiver_initialized(destination)?)?;
    // inventory validated the contents of the mode-0700 destination under the
    // lock. No source pathname is used again after the completed file is open.
    let file = File::open(&latest.path).map_err(|_| "cannot open newest scheduled archive")?;
    let opened = file
        .metadata()
        .map_err(|_| "cannot inspect newest scheduled archive")?;
    if !opened.is_file()
        || opened.uid() != crate::sys::getuid()
        || opened.mode() & 0o077 != 0
        || opened.len() != metadata.bytes
    {
        return Err("scheduled archive changed before open".into());
    }
    drop(lock);
    let mut file = file;
    let hash = td_engine::sha256::sha256_reader((&mut file).take(metadata.bytes + 1))
        .map_err(|_| "cannot read scheduled archive")?;
    if hash != metadata.sha256
        || file
            .stream_position()
            .map_err(|_| "cannot measure scheduled archive")?
            != metadata.bytes
    {
        return Err("scheduled archive digest or size mismatch".into());
    }
    file.rewind()
        .map_err(|_| "cannot rewind scheduled archive")?;
    Ok(VerifiedArchive {
        metadata,
        receipt,
        file,
    })
}

pub(super) fn latest_for_copy(destination: &Path) -> Result<VerifiedArchive> {
    private_directory(destination)?;
    select_archive(destination)
}

fn options(args: &[String]) -> Result<(PathBuf, PathBuf)> {
    let mut destination = None;
    let mut scratch = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("missing restore drill option value")?;
        match flag.as_str() {
            "--destination" if destination.is_none() => destination = Some(absolute(value)?),
            "--scratch-directory" if scratch.is_none() => scratch = Some(absolute(value)?),
            _ => return Err("unknown or duplicate restore drill option".into()),
        }
    }
    let destination = destination.ok_or("missing --destination")?;
    let scratch = scratch.ok_or("missing --scratch-directory")?;
    if scratch.starts_with(&destination) || destination.starts_with(&scratch) {
        return Err("backup and restore scratch directories must be disjoint".into());
    }
    Ok((destination, scratch))
}

fn run(args: &[String]) -> Result<()> {
    let (destination, scratch) = options(args)?;
    private_directory(&scratch)?;
    let mut selected = latest_for_copy(&destination)?;
    let checked = install_mta_restore_engine::verify(&mut selected.file, &scratch)?;
    // All fields here are counts, a checked numeric archive name, or lowercase hex.
    writeln!(
        std::io::stdout().lock(),
        concat!(
        "{{\"schema\":1,\"status\":\"verified\",\"archive\":\"{}\",\"created_unix_seconds\":{},",
        "\"sha256\":\"{}\",\"archive_bytes\":{},\"accounts\":{},\"mailboxes\":{},",
        "\"emails\":{},\"bodies\":{},\"body_bytes\":{}}}"),
        selected.metadata.name,
        selected.metadata.created,
        selected.metadata.sha256,
        selected.metadata.bytes,
        checked.accounts,
        checked.mailboxes,
        checked.emails,
        checked.bodies,
        checked.body_bytes
    )
    .map_err(|_| "cannot write restore drill report".into())
}

pub(in crate::install_mta) fn restore_drill(args: &[String]) -> ExitCode {
    if args == ["--help"] {
        println!("Usage: ./install-td-mta --restore-drill --destination PATH --scratch-directory PATH\nVerify and restore the newest scheduled archive into a disposable PostgreSQL 17 cluster.\nBoth directories must already exist, be owned by this user and have mode 0700.\nPrint verified counts and the source digest after a successful drill.");
        return ExitCode::SUCCESS;
    }
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("install-td-mta restore drill: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_mta::install_mta_backup::private_file;
    use crate::test_temp::TempDir;

    fn receipt(name: &str, data: &[u8], created: u64) -> Vec<u8> {
        let digest = td_engine::sha256::hex_digest(data);
        format!(r#"{{"schema":1,"archive":"/source/scheduled/{name}","sha256":"{digest}","bytes":{},"restored":false,"created_unix_seconds":{created},"pg_dump":"pg_dump (PostgreSQL) 17.10","pg_restore":"pg_restore (PostgreSQL) 17.10"}}"#, data.len()).into_bytes()
    }
    fn publish(dir: &Path, name: &str, data: &[u8], created: u64) -> Vec<u8> {
        let path = dir.join(name);
        private_file(&path).unwrap().write_all(data).unwrap();
        let metadata = receipt(name, data, created);
        private_file(&path.with_extension("json"))
            .unwrap()
            .write_all(&metadata)
            .unwrap();
        metadata
    }

    #[test]
    fn relocated_latest_receipt_and_open_file_survive_retention() {
        let temp = TempDir::new("drill-pinned-archive").unwrap();
        let root = temp.path();
        publish(root, "backup-9-7.dump", b"old", 9);
        let latest = "backup-10-2.dump";
        let original = publish(root, latest, b"newest", 8);
        assert!(select_archive(root).is_err());
        private_file(&root.join(super::super::RECEIVER_MARKER)).unwrap();
        private_file(&root.join("backup-11-8.partial")).unwrap();
        let lock = destination_lock(root).unwrap();
        assert!(select_archive(root).is_err());
        drop(lock);
        let mut selection = select_archive(root).unwrap();
        assert_eq!(selection.metadata.name, latest);
        assert_eq!(selection.metadata.created, 8);
        assert_eq!(selection.receipt, original);
        let lock = destination_lock(root).unwrap();
        fs::remove_file(root.join(latest)).unwrap();
        fs::remove_file(root.join(latest).with_extension("json")).unwrap();
        drop(lock);
        let mut contents = Vec::new();
        selection.file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"newest");
    }

    #[test]
    fn refuses_unverifiable_newest_even_with_an_older_valid_archive() {
        let temp = TempDir::new("drill-damaged").unwrap();
        let root = temp.path();
        private_file(&root.join(super::super::RECEIVER_MARKER)).unwrap();
        publish(root, "backup-20-1.dump", b"old", 20);
        let file = root.join("backup-21-2.dump");
        private_file(&file).unwrap().write_all(b"new").unwrap();
        assert!(select_archive(root).is_err());
        let record = file.with_extension("json");
        private_file(&record).unwrap();
        for data in [
            receipt("backup-22-2.dump", b"new", 20),
            receipt("backup-21-2.dump", b"bad", 21),
            receipt("backup-21-2.dump", b"wrong-size", 21),
        ] {
            fs::write(&record, &data).unwrap();
            assert!(select_archive(root).is_err());
            assert_eq!(fs::read(&record).unwrap(), data);
        }
        fs::write(&record, receipt("backup-21-2.dump", b"new", 21)).unwrap();
        publish(root, "backup-21-7.dump", b"undecidable", 21);
        assert!(select_archive(root).is_err());
    }

    #[test]
    fn relocated_freshness_requires_receiver_marker_and_matching_archive() {
        use super::super::{observe_backup_status, RECEIVER_MARKER};
        use std::time::{SystemTime, UNIX_EPOCH};
        let temp = TempDir::new("drill-receiver-freshness").unwrap();
        let root = temp.path();
        let record = publish(root, "backup-24-1.dump", b"new", 22);
        let input = std::str::from_utf8(&record).unwrap();
        let receipt_path = root.join("backup-24-1.json");
        assert!(observe_backup_status(root, 172800).is_err());
        fs::write(
            &receipt_path,
            input.replace("/source/scheduled", root.to_str().unwrap()),
        )
        .unwrap();
        select_archive(root).unwrap();
        assert!(!observe_backup_status(root, 172800).unwrap());
        fs::write(receipt_path, &record).unwrap();
        let marker = root.join(RECEIVER_MARKER);
        private_file(&marker).unwrap();
        assert!(!observe_backup_status(root, 172800).unwrap());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let recent = format!("backup-{now}-1.dump");
        publish(root, &recent, b"fresh", now);
        assert!(observe_backup_status(root, 172800).unwrap());
        fs::write(root.join(recent), b"truncated").unwrap();
        assert!(observe_backup_status(root, 172800).is_err());
        fs::write(&marker, b"unexpected").unwrap();
        assert!(super::super::receiver_initialized(root).is_err());
    }

    #[test]
    fn receipt_shape_and_names_are_bounded_before_parsing_or_display() {
        let base = receipt("backup-10-1.dump", b"data", 10);
        assert_eq!(parse_receipt(&base).unwrap().sequence, 10);
        let base = String::from_utf8(base).unwrap();
        for (from, to) in [
            ("backup-10-1.dump", "backup-10-1.dump.extra"),
            ("backup-10-1.dump", "backup-10-1.dump/"),
            ("backup-10-1.dump", "backup-+10-1.dump"),
            ("backup-10-1.dump", "backup-10-01.dump"),
            ("backup-10-1.dump", "backup-10-0.dump"),
            ("scheduled/", "../"),
            ("\"schema\":1", "\"schema\":1,\"schema\":1"),
            ("\"restored\":false", "\"restored\":true"),
            ("\"bytes\":4", "\"bytes\":1099511627777"),
            ("\"created_unix_seconds\":10", "\"created_unix_seconds\":11"),
            ("pg_dump (PostgreSQL) 17.", "pg_dump (PostgreSQL) 16."),
        ] {
            assert!(parse_receipt(base.replace(from, to).as_bytes()).is_err());
        }
        assert!(parse_receipt(&vec![b'['; 65536]).is_err());
    }

    #[test]
    fn restore_drill_requires_each_directory_once() {
        let args = ["--scratch-directory", "/work", "--destination", "/backups"].map(String::from);
        let (backup, scratch) = options(&args).unwrap();
        assert_eq!(backup, Path::new("/backups"));
        assert_eq!(scratch, Path::new("/work"));
        assert!(options(&args[..2]).is_err());
        let twice = [args.as_slice(), &args[..2]].concat();
        assert!(options(&twice).is_err());
        for scratch in ["/", "/backups", "/backups/scratch"] {
            let nested =
                ["--destination", "/backups", "--scratch-directory", scratch].map(String::from);
            assert!(options(&nested).is_err());
        }
    }
}
