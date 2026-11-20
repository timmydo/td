//! Runs only inside the disposable namespace. The archive gets a plain database
//! login. An independent read-only login verifies ordered rows and body bytes.
use super::{Process, Result, Verified};
use crate::install_mta::restore_layout::version_17;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::process::{ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};
use td_engine::sha256::Sha256;

const ADMIN: &str = "drill_admin";
const WRITER: &str = "drill_writer";
const AUDITOR: &str = "drill_reader";
const DB: &str = "td_mta";
const SOCKET: &str = "/work/socket";
const SHAPE: &str = include_str!("restore_shape.sql");
// Version 3 introduced the mail tables. Every later accepted version must be a
// prefix of the immutable application ledger, including migrations stored inline.
const MINIMUM_VERSION: usize = 3;
const MIGRATION_HASHES: &str = include_str!("restore_migrations.sha256");
const TABLE_HASHES: &str = include_str!("restore_table_sets.sha256");
const TABLE_SET: &str = include_str!("restore_table_set.sql");
const LEDGER: &str = "COPY (SELECT version,encode(checksum,'hex'),encode(store_epoch,'hex') FROM ONLY td_mta.schema_migrations ORDER BY version) TO STDOUT";
const BLOBS: &str = "COPY (SELECT encode(account,'hex'),encode(id,'hex'),length,chunk_count,encode(digest,'hex') FROM ONLY td_mta.blobs ORDER BY account,id) TO STDOUT";
const CHUNKS: &str = "COPY (SELECT encode(account,'hex'),encode(blob_id,'hex'),ordinal,encode(bytes,'hex') FROM ONLY td_mta.blob_chunks ORDER BY account,blob_id,ordinal) TO STDOUT";
const COUNTS: &str = "SELECT (SELECT count(*) FROM ONLY td_mta.accounts),(SELECT count(*) FROM ONLY td_mta.mailboxes),(SELECT count(*) FROM ONLY td_mta.emails),(SELECT count(*) FROM ONLY td_mta.blobs),(SELECT coalesce(sum(length),0) FROM ONLY td_mta.blobs)";

fn tool(name: &str) -> Result<Command> {
    let mut command = Command::new(name);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH")?)
        .env("HOME", "/tmp")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env("PGCONNECT_TIMEOUT", "3")
        .env(
            "PGOPTIONS",
            "-c search_path=pg_catalog -c client_min_messages=error",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(ld) = std::env::var_os("TD_MTA_DRILL_PG_LIBRARIES") {
        command.env("LD_LIBRARY_PATH", ld);
    }
    Ok(command)
}

fn pg_tools() -> Result<()> {
    for name in ["postgres", "initdb", "pg_restore", "psql"] {
        let out = tool(name)?
            .arg("--version")
            .stdout(Stdio::piped())
            .output()?;
        if !out.status.success() || !version_17(name, std::str::from_utf8(&out.stdout)?) {
            return Err("restore requires matching PostgreSQL 17 tools".into());
        }
    }
    Ok(())
}

fn query(db: &str, role: &str, sql: &str) -> Result<Command> {
    let mut command = tool("psql")?;
    command.args([
        "-XAtqw",
        "-h",
        SOCKET,
        "-p",
        "55439",
        "-U",
        role,
        "-d",
        db,
        "--set=ON_ERROR_STOP=1",
        "-c",
        sql,
    ]);
    Ok(command)
}
fn bounded_answer(mut command: Command, buf: &mut [u8]) -> Result<&str> {
    let (mut process, mut reader) = stream(&mut command)?;
    let bytes = line(&mut reader, buf)?.ok_or("missing verification row")?;
    if !reader.fill_buf()?.is_empty() || !process.0.wait()?.success() {
        return Err("verification statement failed".into());
    }
    Ok(std::str::from_utf8(bytes)?)
}

fn initialize() -> Result<Process> {
    super::private_dir(std::path::Path::new(SOCKET))?;
    if !tool("initdb")?
        .args([
            "-D",
            "/work/db",
            "-U",
            ADMIN,
            "--auth-local=trust",
            "--auth-host=reject",
            "--no-locale",
            "--encoding=UTF8",
            "--no-sync",
        ])
        .status()?
        .success()
    {
        return Err("disposable initdb failed".into());
    }
    let mut server = Process(
        tool("postgres")?
            .args([
                "-D",
                "/work/db",
                "-p",
                "55439",
                "-k",
                SOCKET,
                "-c",
                "listen_addresses=",
                "-c",
                "unix_socket_permissions=0700",
                "-c",
                "max_connections=10",
                "-c",
                "shared_buffers=32MB",
                "-c",
                "work_mem=4MB",
                "-c",
                "maintenance_work_mem=64MB",
                "-c",
                "temp_file_limit=4GB",
                "-c",
                "max_parallel_workers=0",
                "-c",
                "autovacuum=off",
                "-c",
                "fsync=off",
                "-c",
                "full_page_writes=off",
            ])
            .spawn()?,
    );
    let start = Instant::now();
    while !query("postgres", ADMIN, "SELECT 1")?.status()?.success() {
        if server.0.try_wait()?.is_some() || start.elapsed() > Duration::from_secs(30) {
            return Err("disposable PostgreSQL startup failed".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for sql in ["CREATE ROLE drill_writer LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS",
        "CREATE ROLE drill_reader LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS",
        "GRANT pg_read_all_data TO drill_reader", "CREATE DATABASE td_mta OWNER drill_writer"] {
        if !query("postgres",ADMIN,sql)?.status()?.success() { return Err("disposable database setup failed".into()); }
    }
    fs::write(
        "/work/db/pg_hba.conf",
        b"local td_mta drill_writer trust\nlocal td_mta drill_reader trust\nlocal all all reject\n",
    )?;
    if !query("postgres", ADMIN, "SELECT pg_reload_conf()")?
        .status()?
        .success()
    {
        return Err("restore login isolation failed".into());
    }
    while query("postgres", ADMIN, "SELECT 1")?.status()?.success() {
        if start.elapsed() > Duration::from_secs(30) {
            return Err("restore login isolation timed out".into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(server)
}

fn stream(cmd: &mut Command) -> Result<(Process, BufReader<ChildStdout>)> {
    let mut child = Process(cmd.stdout(Stdio::piped()).spawn()?);
    let out = child
        .0
        .stdout
        .take()
        .ok_or("cannot read verification stream")?;
    Ok((child, BufReader::new(out)))
}

// COPY text encodes each 64 KiB chunk as two hex digits per byte. Never let a
// malformed row extend the buffer; consume the newline before returning.
fn line<'a>(reader: &mut impl BufRead, buf: &'a mut [u8]) -> Result<Option<&'a [u8]>> {
    let mut used: usize = 0;
    loop {
        let data = reader.fill_buf()?;
        if data.is_empty() {
            if used == 0 {
                return Ok(None);
            }
            return Err("truncated verification stream".into());
        }
        let newline = data.iter().position(|b| *b == b'\n');
        let take = newline.unwrap_or(data.len());
        let end = used.checked_add(take).ok_or("invalid verification row")?;
        buf.get_mut(used..end)
            .ok_or("oversized verification row")?
            .copy_from_slice(data.get(..take).ok_or("invalid verification row")?);
        reader.consume(take + usize::from(newline.is_some()));
        used = end;
        if newline.is_some() {
            return Ok(Some(buf.get(..used).ok_or("invalid verification row")?));
        }
    }
}

fn hex_digit(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err("invalid body encoding".into()),
    }
}
fn decode(input: &str, out: &mut [u8]) -> Result<()> {
    if input.len() != out.len() * 2 {
        return Err("restored body length mismatch".into());
    }
    for (byte, pair) in out.iter_mut().zip(input.as_bytes().chunks_exact(2)) {
        let [hi, lo] = pair else {
            return Err("invalid body encoding".into());
        };
        *byte = hex_digit(*hi)? << 4 | hex_digit(*lo)?;
    }
    Ok(())
}
fn field<'a>(fields: &mut impl Iterator<Item = &'a str>) -> Result<&'a str> {
    fields
        .next()
        .ok_or_else(|| "invalid restored body metadata".into())
}

fn verify_ledger(rows: &mut impl BufRead) -> Result<usize> {
    let mut hashes = MIGRATION_HASHES.lines();
    let mut version = 0;
    let mut epoch = None;
    let mut row = [0u8; 128];
    while let Some(bytes) = line(rows, &mut row)? {
        let mut fields = std::str::from_utf8(bytes)?.split('\t');
        version += 1;
        if field(&mut fields)?.parse::<usize>()? != version
            || hashes.next() != Some(field(&mut fields)?)
        {
            return Err(
                "restored migration history differs from the supported application schema".into(),
            );
        }
        let mut observed = [0u8; 16];
        decode(field(&mut fields)?, &mut observed)?;
        if epoch.is_some_and(|prior| prior != observed) || fields.next().is_some() {
            return Err("invalid restored migration epoch".into());
        }
        epoch = Some(observed);
    }
    if version < MINIMUM_VERSION {
        return Err("restored schema predates mail storage".into());
    }
    Ok(version)
}

fn verify_bodies(blobs: &mut impl BufRead, chunks: &mut impl BufRead) -> Result<(u64, u64)> {
    let mut head = [0u8; 192];
    let mut record = [0u8; 131200];
    let mut bytes = [0u8; 65536];
    let mut digest = [0u8; 32];
    let mut bodies = 0u64;
    let mut total = 0u64;
    while let Some(row) = line(blobs, &mut head)? {
        let mut f = std::str::from_utf8(row)?.split('\t');
        let account = field(&mut f)?;
        let id = field(&mut f)?;
        let length: usize = field(&mut f)?.parse()?;
        let count: usize = field(&mut f)?.parse()?;
        decode(field(&mut f)?, &mut digest)?;
        if account.len() != 32
            || id.len() != 32
            || f.next().is_some()
            || length > 67108864
            || count != length.div_ceil(65536)
        {
            return Err("invalid restored body metadata".into());
        }
        let mut hasher = Sha256::new();
        for ordinal in 0..count {
            let row = line(chunks, &mut record)?.ok_or("missing restored body chunk")?;
            let mut parts = std::str::from_utf8(row)?.split('\t');
            if field(&mut parts)? != account
                || field(&mut parts)? != id
                || field(&mut parts)?.parse::<usize>()? != ordinal
            {
                return Err("restored chunk identity or ordinal mismatch".into());
            }
            let size = (length - ordinal * 65536).min(65536);
            let chunk = bytes.get_mut(..size).ok_or("invalid body chunk length")?;
            decode(field(&mut parts)?, chunk)?;
            if parts.next().is_some() {
                return Err("invalid body chunk row".into());
            }
            hasher.update(chunk);
        }
        if hasher.finalize() != digest {
            return Err("restored body SHA-256 mismatch".into());
        }
        bodies = bodies.checked_add(1).ok_or("body count overflow")?;
        total = total
            .checked_add(u64::try_from(length)?)
            .ok_or("body byte count overflow")?;
    }
    if line(chunks, &mut record)?.is_some() {
        return Err("unexpected restored body chunk".into());
    }
    Ok((bodies, total))
}

pub(super) fn run() -> Result<()> {
    let executable = fs::metadata("/drill-controller").map_err(|_| "missing restore namespace")?;
    let this = fs::metadata("/proc/self/exe")?;
    let uid = crate::sys::getuid();
    let uid_map = fs::read_to_string("/proc/self/uid_map")?;
    let mut ids = uid_map.split_whitespace();
    if uid == 0
        || crate::sys::getppid() != 1
        || ids.next().and_then(|n| n.parse().ok()) != Some(uid)
        || ids.next().and_then(|n| n.parse().ok()) != Some(uid)
        || ids.next() != Some("1")
        || ids.next().is_some()
        || std::env::var_os("TD_HOST_SANDBOX").as_deref() != Some("1".as_ref())
        || (executable.dev(), executable.ino()) != (this.dev(), this.ino())
        || std::env::current_dir()?.as_os_str() != "/work"
    {
        return Err("restore worker requires its private namespace".into());
    }
    pg_tools()?;
    let _server = initialize()?;
    if !tool("pg_restore")?
        .args([
            "--format=custom",
            "--exit-on-error",
            "--single-transaction",
            "--no-owner",
            "--no-privileges",
            "--no-tablespaces",
            "--no-subscriptions",
            "--no-password",
            "-h",
            SOCKET,
            "-p",
            "55439",
            "-U",
            WRITER,
            "-d",
            DB,
        ])
        .stdin(Stdio::inherit())
        .status()?
        .success()
    {
        return Err("scheduled archive restore failed".into());
    }
    let mut answer = [0u8; 128];
    if bounded_answer(query(DB, AUDITOR, SHAPE)?, &mut answer)? != "t" {
        return Err("restored mail schema or references are invalid".into());
    }
    let (mut ledger, mut rows) = stream(&mut query(DB, AUDITOR, LEDGER)?)?;
    let version = verify_ledger(&mut rows)?;
    if !ledger.0.wait()?.success() {
        return Err("restored migration verification failed".into());
    }
    let expected = TABLE_HASHES
        .lines()
        .nth(version - 1)
        .ok_or("unsupported restore table set")?;
    if bounded_answer(query(DB, AUDITOR, TABLE_SET)?, &mut answer)? != expected {
        return Err("restored application tables differ from migration history".into());
    }
    let counts = Verified::parse(bounded_answer(query(DB, AUDITOR, COUNTS)?, &mut answer)?)?;
    let (mut blob, mut metadata) = stream(&mut query(DB, AUDITOR, BLOBS)?)?;
    let (mut chunk, mut data) = stream(&mut query(DB, AUDITOR, CHUNKS)?)?;
    let verified = verify_bodies(&mut metadata, &mut data)?;
    if !blob.0.wait()?.success()
        || !chunk.0.wait()?.success()
        || verified != (counts.bodies, counts.body_bytes)
    {
        return Err("restored body stream verification failed".into());
    }
    let mut output = File::options().write(true).truncate(true).open("/counts")?;
    writeln!(
        output,
        "{}|{}|{}|{}|{}",
        counts.accounts, counts.mailboxes, counts.emails, counts.bodies, counts.body_bytes
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use td_engine::sha256::hex_digest;
    const ACCOUNT: &str = "00112233445566778899aabbccddeeff";
    const BODY: &str = "00112233445566778899aabbccddeefe";

    #[test]
    fn ledger_requires_a_known_prefix_with_one_epoch_and_mail_storage() {
        let hashes: Vec<_> = MIGRATION_HASHES.lines().collect();
        let epoch = "a0".repeat(16);
        let ledger = |end: usize| {
            (0..end)
                .map(|i| {
                    format!(
                        "{}\t{}\t{epoch}\n",
                        i + 1,
                        hashes.get(i).unwrap_or(&"unrecognized")
                    )
                })
                .collect::<String>()
        };
        assert!(verify_ledger(&mut Cursor::new(ledger(2))).is_err());
        assert_eq!(verify_ledger(&mut Cursor::new(ledger(3))).ok(), Some(3));
        assert_eq!(
            verify_ledger(&mut Cursor::new(ledger(hashes.len()))).ok(),
            Some(hashes.len())
        );
        assert!(verify_ledger(&mut Cursor::new(ledger(hashes.len() + 1))).is_err());
        assert!(verify_ledger(&mut Cursor::new(
            ledger(3).replace(hashes.get(1).copied().unwrap_or_default(), &"bb".repeat(32))
        ))
        .is_err());
        assert!(verify_ledger(&mut Cursor::new(ledger(3).replacen(
            &epoch,
            &"af".repeat(16),
            1
        )))
        .is_err());
        assert!(verify_ledger(&mut Cursor::new(ledger(3).replace("2\t", "4\t"))).is_err());
    }
    #[test]
    fn version_accepts_distribution_suffixes_and_rejects_other_majors() {
        assert!(version_17("pg_restore", "pg_restore (PostgreSQL) 17.10"));
        assert!(version_17(
            "pg_restore",
            "pg_restore (PostgreSQL) 17.7 (Debian 17.7-3.pgdg13+1)"
        ));
        assert!(!version_17("pg_restore", "pg_restore (PostgreSQL) 16.7"));
        assert!(!version_17("pg_restore", "pg_restore (PostgreSQL) 17."));
    }
    #[test]
    fn body_stream_rejects_missing_extra_disordered_short_and_corrupt_chunks() {
        let head = format!(
            "{ACCOUNT}\t{BODY}\t65538\t2\t{}\n",
            hex_digest(&[vec![0x81; 65536], vec![255, 0]].concat())
        );
        let first = format!("{ACCOUNT}\t{BODY}\t0\t{}\n", "81".repeat(65536));
        let last = format!("{ACCOUNT}\t{BODY}\t1\tff00\n");
        let run = |chunks: String| verify_bodies(&mut Cursor::new(&head), &mut Cursor::new(chunks));
        assert_eq!(run(format!("{first}{last}")).ok(), Some((1, 65538)));
        assert!(run(first.clone()).is_err());
        assert!(run(format!("{first}{last}{last}")).is_err());
        assert!(run(format!("{last}{first}")).is_err());
        assert!(run(format!("{first}{}", last.replace("ff00", "ff"))).is_err());
        assert!(run(format!("{first}{}", last.replace("ff00", "f001"))).is_err());
        assert!(run(format!("{first}{}", last.replace(BODY, ACCOUNT))).is_err());
    }

    #[test]
    fn oversized_record_fails_before_reading_the_rest() {
        let mut reader = Cursor::new(b"abcdef\n");
        let mut buf = [0u8; 3];
        assert!(line(&mut reader, &mut buf).is_err());
    }

    #[test]
    fn verifies_empty_bodies_and_rejects_unexpected_data() {
        let head = format!("{ACCOUNT}\t{BODY}\t0\t0\t{}\n", hex_digest(&[]));
        assert_eq!(
            verify_bodies(&mut Cursor::new(&head), &mut Cursor::new(b"")).ok(),
            Some((1, 0))
        );
        assert!(verify_bodies(&mut Cursor::new(&head), &mut Cursor::new(b"extra\n")).is_err());
    }
}
