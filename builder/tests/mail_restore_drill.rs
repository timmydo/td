//! A real scheduled pg_dump, its off-host copy, and an isolated verification.
#![cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

use std::{
    error::Error,
    fs::{self, File},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use td_engine::{
    json::{self, Json},
    sha256,
};
use test_temp::TempDir;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const LEDGER: &str = include_str!("../src/install_mta_backup/restore_migrations.sha256");

fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PG") {
            cmd.env_remove(key);
        }
    }
    cmd.env("PGCONNECT_TIMEOUT", "2").stdin(Stdio::null());
    cmd
}

fn checked(command: &mut Command) -> Result<Output> {
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!(
            "{:?} exited {}: {}",
            command.get_program(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(output)
}

fn json(output: &Output) -> Result<Json> {
    Ok(json::parse(std::str::from_utf8(&output.stdout)?)?)
}

fn operator(action: &str, destination: &Path) -> Command {
    let mut cmd = command(env!("CARGO_BIN_EXE_td-builder"));
    // Cargo prepends its target directory to the test library path. The drill
    // runs against the installed PostgreSQL 17 toolchain.
    cmd.env_remove("LD_LIBRARY_PATH");
    cmd.args(["install-td-mta", action, "--destination"])
        .arg(destination);
    cmd
}

struct Cluster {
    data: PathBuf,
    socket: TempDir,
    postgres: Child,
}

impl Cluster {
    fn start(root: &Path) -> Result<Self> {
        let version = checked(command("initdb").arg("--version"))?;
        assert!(String::from_utf8(version.stdout)?.starts_with("initdb (PostgreSQL) 17."));
        let data = root.join("source");
        checked(
            command("initdb")
                .args([
                    "--no-sync",
                    "--no-locale",
                    "-E",
                    "UTF8",
                    "-A",
                    "trust",
                    "-U",
                    "fixture",
                    "-D",
                ])
                .arg(&data),
        )?;
        // Keep the Unix-socket address short even on deeply nested worktrees.
        let socket = TempDir::new_in(Path::new("/tmp"), "td-drill-sock")?;
        let log = File::create(root.join("source.log"))?;
        let postgres = command("postgres")
            .arg("-D")
            .arg(&data)
            .args(["-h", "", "-p", "5432", "-k"])
            .arg(socket.path())
            .args(["-c", "unix_socket_permissions=0700"])
            .stderr(log.try_clone()?)
            .stdout(log)
            .spawn()?;
        let mut cluster = Self {
            data,
            socket,
            postgres,
        };
        let until = Instant::now() + Duration::from_secs(20);
        while !cluster
            .psql()
            .args(["-c", "SELECT 1"])
            .output()?
            .status
            .success()
        {
            if cluster.postgres.try_wait()?.is_some() || Instant::now() >= until {
                return Err("disposable source PostgreSQL did not become ready".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(cluster)
    }

    fn psql(&self) -> Command {
        let mut psql = command("psql");
        psql.args([
            "-X",
            "-w",
            "-v",
            "ON_ERROR_STOP=1",
            "-U",
            "fixture",
            "-d",
            "postgres",
            "-p",
            "5432",
            "-h",
        ])
        .arg(self.socket.path());
        psql
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for mode in ["fast", "immediate"] {
            if self.postgres.try_wait().ok().flatten().is_some() {
                return;
            }
            let status = command("pg_ctl")
                .arg("-D")
                .arg(&self.data)
                .args(["-w", "-t", "10", "-m", mode, "stop"])
                .output();
            if status.is_ok_and(|result| result.status.success()) {
                break;
            }
        }
        let _ = self.postgres.wait();
    }
}

fn seed(cluster: &Cluster, root: &Path) -> Result<()> {
    let path = root.join("seed.sql");
    let mut sql = File::create(&path)?;
    writeln!(sql, "BEGIN;")?;
    let schema_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../td-mta/src");
    // Use the application DDL, in ledger order. One historical migration
    // is currently stored inline in td-mta's MIGRATIONS array.
    let inline = "DROP TABLE td_mta.receiving_receipts;";
    let mut migrations = std::collections::BTreeMap::from([(
        sha256::hex_digest(inline.as_bytes()),
        inline.to_owned(),
    )]);
    for entry in fs::read_dir(schema_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "sql") {
            let ddl = fs::read_to_string(path)?;
            migrations.insert(sha256::hex_digest(ddl.as_bytes()), ddl);
        }
    }
    let table_sets = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/install_mta_backup/restore_table_sets.sha256"),
    )?;
    assert_eq!(LEDGER.lines().count(), table_sets.lines().count());
    for (index, (digest, tables)) in LEDGER.lines().zip(table_sets.lines()).enumerate() {
        let ddl = migrations
            .get(digest)
            .ok_or_else(|| format!("missing migration {digest}"))?;
        writeln!(sql, "{ddl}")?;
        writeln!(sql,
            "INSERT INTO td_mta.schema_migrations VALUES ({}, decode('{digest}','hex'), decode(repeat('23',16),'hex'));",
            index + 1)?;
        writeln!(
            sql,
            r#"
DO $$
DECLARE fingerprint text;
BEGIN
    SELECT encode(sha256(convert_to(string_agg(c.relname::text || chr(10), ''
                ORDER BY c.relname::text COLLATE "C"), 'UTF8')), 'hex')
    INTO fingerprint FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='td_mta' AND c.relkind='r';
    IF fingerprint IS DISTINCT FROM '{tables}' THEN
        RAISE EXCEPTION 'table-set digest differs after migration {}: %', fingerprint;
    END IF;
END $$;"#,
            index + 1
        )?;
    }
    // Two bodies exercise chunk order, zero chunks and the last short chunk.
    sql.write_all(
        br#"
DO $$
DECLARE
    a bytea := decode(repeat('01',16),'hex');
    b bytea := decode(repeat('02',16),'hex');
    empty_id bytea := decode(repeat('03',16),'hex');
    v bytea := decode(repeat('00',8),'hex');
    chunk bytea := decode(repeat('cd',65536),'hex');
    tail bytea := decode('ab','hex');
BEGIN
    INSERT INTO td_mta.accounts VALUES (a,v,v);
    INSERT INTO td_mta.mailboxes VALUES (a,a,'Inbox',NULL,'inbox',0,1,v);
    INSERT INTO td_mta.blob_ids VALUES (a,b),(a,empty_id);
    INSERT INTO td_mta.blobs VALUES
        (a,b,65537,sha256(chunk||tail),1,v,2),
        (a,empty_id,0,sha256(''::bytea),1,v,0);
    INSERT INTO td_mta.blob_chunks VALUES (a,b,1,tail),(a,b,0,chunk);
    INSERT INTO td_mta.threads VALUES (a,a,v);
    INSERT INTO td_mta.emails (account,id,blob_id,thread_id,received_at,origin,changed)
        VALUES (a,a,b,a,1,2,v);
    INSERT INTO td_mta.memberships VALUES (a,a,a,v);
END $$;
COMMIT;
"#,
    )?;
    drop(sql);
    checked(cluster.psql().arg("-f").arg(path))?;
    Ok(())
}

#[test]
#[ignore = "requires PostgreSQL 17, pg_trgm, and unprivileged Linux user namespaces"]
fn real_archive_round_trip() -> Result<()> {
    // The native runner provides an owned mode-0700 /tmp. Production trust
    // checks apply unchanged even when the outer check host remaps its users.
    let tmp = TempDir::new("td-mail-restore-test")?;
    let archives = tmp.join("archives");
    let copies = tmp.join("copies");
    let scratch = tmp.join("scratch");
    let corrupt_ledger = tmp.join("corrupt-ledger");
    let missing_table = tmp.join("missing-table");
    for dir in [
        &archives,
        &copies,
        &scratch,
        &corrupt_ledger,
        &missing_table,
    ] {
        fs::DirBuilder::new().mode(0o700).create(dir)?;
    }
    let cluster = Cluster::start(tmp.path())?;
    seed(&cluster, tmp.path())?;
    let service = tmp.join("service.conf");
    let mut config = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&service)?;
    writeln!(
        config,
        "[drill]\nhost={}\nport=5432\nuser=fixture\ndbname=postgres",
        cluster.socket.path().display()
    )?;
    drop(config);
    let backup = |destination: &Path| {
        checked(
            operator("--backup", destination)
                .args(["--service", "drill", "--service-file"])
                .arg(&service),
        )
    };
    let receipt = json(&backup(&archives)?)?;
    let stream = checked(&mut operator("--export-latest-backup", &archives))?;
    let mut receiver = operator("--receive-backup", &copies)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let written = receiver
        .stdin
        .take()
        .ok_or("missing receiver stdin")?
        .write_all(&stream.stdout);
    let received = receiver.wait_with_output()?;
    assert!(
        received.status.success(),
        "{}",
        String::from_utf8_lossy(&received.stderr)
    );
    written?;
    checked(operator("--backup-status", &copies).args(["--maximum-age-seconds", "172800"]))?;

    // This change satisfies the SQL constraints and invalidates the stored digest.
    checked(cluster.psql().args([
        "-c",
        "UPDATE td_mta.blob_chunks SET bytes=decode('ac','hex') WHERE ordinal=1",
    ]))?;
    backup(&archives)?;
    checked(cluster.psql().args([
        "-c", "UPDATE td_mta.blob_chunks SET bytes=decode('ab','hex') WHERE ordinal=1;
               UPDATE td_mta.schema_migrations SET checksum=decode(repeat('00',32),'hex') WHERE version=3",
    ]))?;
    backup(&corrupt_ledger)?;
    let mail_checksum = LEDGER.lines().nth(2).ok_or("missing mail migration")?;
    checked(cluster.psql().arg("-c").arg(format!(
        "UPDATE td_mta.schema_migrations SET checksum=decode('{mail_checksum}','hex') WHERE version=3;
         DROP TABLE td_mta.smtp_receipt_recipients"
    )))?;
    backup(&missing_table)?;
    drop(cluster);

    let drill = |destination: &Path| {
        let mut cmd = operator("--restore-drill", destination);
        cmd.arg("--scratch-directory").arg(&scratch);
        cmd
    };
    let verified = json(&checked(&mut drill(&copies))?)?;
    assert_eq!(
        verified.get("status").and_then(Json::as_str),
        Some("verified")
    );
    let source_path = receipt
        .get("archive")
        .and_then(Json::as_str)
        .ok_or("missing archive path")?;
    let basename = Path::new(source_path)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("missing archive name")?;
    assert_eq!(
        verified.get("archive").and_then(Json::as_str),
        Some(basename)
    );
    assert_eq!(
        verified.get("created_unix_seconds"),
        receipt.get("created_unix_seconds")
    );
    assert_eq!(verified.get("sha256"), receipt.get("sha256"));
    assert_eq!(verified.get("archive_bytes"), receipt.get("bytes"));
    for (field, value) in [
        ("accounts", 1),
        ("mailboxes", 1),
        ("emails", 1),
        ("bodies", 2),
        ("body_bytes", 65537),
    ] {
        assert_eq!(
            verified.get(field),
            Some(&Json::Num(value.to_string())),
            "{field}"
        );
    }
    for (destination, expected) in [
        (&archives, "restored body SHA-256 mismatch"),
        (&corrupt_ledger, "restored migration history differs"),
        (
            &missing_table,
            "restored application tables differ from migration history",
        ),
    ] {
        let corrupt = drill(destination).output()?;
        assert!(
            !corrupt.status.success(),
            "accepted corrupt backup: {expected}"
        );
        let reason = String::from_utf8_lossy(&corrupt.stderr);
        assert!(reason.contains(expected), "{reason}");
        assert!(
            corrupt.stdout.is_empty(),
            "emitted a verification record for corrupt data"
        );
    }
    assert!(
        fs::read_dir(&scratch)?.next().is_none(),
        "restore left a scratch cluster"
    );
    tmp.cleanup()?;
    Ok(())
}
