//! Disposable PostgreSQL 17 regression for the evidence consumed at the fence.
use super::{check_continuity, parse_lsn, read_manifest};
use crate::{
    postgres_fixture::{checked, run, wait, PgTools},
    R,
};
use std::{fs, path::Path};

fn query(tools: &PgTools, socket: &Path, sql: &str) -> R<String> {
    checked(tools.cmd("psql").args(["-XwqAt", "-h"]).arg(socket).args([
        "-p",
        "35432",
        "-U",
        "fixture",
        "-d",
        "postgres",
        "-v",
        "ON_ERROR_STOP=1",
        "-c",
        sql,
    ]))
}

#[test]
#[ignore = "requires PostgreSQL 17 tools as a non-root user"]
fn real_postgres_backup_replays_to_exact_marker_without_later_mail() -> R<()> {
    let tools = PgTools::new()?;
    let query = |socket: &Path, sql: &str| query(&tools, socket, sql);
    // The gate provides a private /tmp. Keep socket addresses below Linux's
    // sockaddr_un limit even when the build's TMPDIR lies deep in a worktree.
    let directory = td_fs::TempDir::new_in(Path::new("/tmp"), "tdwal")?;
    let root = directory.path();
    let primary = root.join("primary");
    let base = root.join("base");
    let wal = root.join("wal");
    let socket = root.join("s");
    let standby = root.join("r");
    for dir in [&wal, &socket, &standby] {
        fs::create_dir(dir)?;
    }
    checked(tools.cmd("initdb").arg("-D").arg(&primary).args([
        "--no-sync",
        "--no-locale",
        "-U",
        "fixture",
        "--auth-local=trust",
        "--auth-host=reject",
    ]))?;
    let server_opts = ["-p", "35432", "-c", "listen_addresses=", "-k"];
    let _primary = run(
        tools
            .cmd("postgres")
            .arg("-D")
            .arg(&primary)
            .args(server_opts)
            .arg(&socket),
        &root.join("primary.log"),
    )?;
    wait("primary startup", || query(&socket, "SELECT 1").is_ok())?;
    let identity = query(&socket, "SELECT system_identifier FROM pg_control_system()")?;
    let timeline =
        query(&socket, "SELECT timeline_id FROM pg_control_checkpoint()")?.parse::<u32>()?;
    query(
        &socket,
        "CREATE TABLE received(id bigint PRIMARY KEY); INSERT INTO received VALUES(1)",
    )?;
    let receiver = run(
        tools
            .cmd("pg_receivewal")
            .args(["-wn", "--synchronous", "-D"])
            .arg(&wal)
            .arg("-h")
            .arg(&socket)
            .args([
                "-p",
                "35432",
                "-U",
                "fixture",
                "-d",
                "dbname=postgres application_name=td_cutover_wal",
            ]),
        &root.join("wal.log"),
    )?;
    wait("streaming WAL", || {
        query(&socket,"SELECT state='streaming' FROM pg_stat_replication WHERE application_name='td_cutover_wal'").as_deref().ok()==Some("t")
    })?;
    checked(
        tools
            .cmd("pg_basebackup")
            .args([
                "-w",
                "--checkpoint=fast",
                "--no-slot",
                "--wal-method=stream",
                "-D",
            ])
            .arg(&base)
            .arg("-h")
            .arg(&socket)
            .args(["-p", "35432", "-U", "fixture"]),
    )?;
    checked(tools.cmd("pg_verifybackup").arg(&base))?;
    let (_, first) = read_manifest(&base.join("backup_manifest"), &identity, timeline)?;
    query(&socket, "INSERT INTO received VALUES(2)")?;
    let lsn = query(&socket, "SELECT pg_create_restore_point('td_cut_fixture')")?;
    parse_lsn(&lsn)?;
    let segment = query(&socket, &format!("SELECT pg_walfile_name('{lsn}'::pg_lsn)"))?;
    query(&socket, "INSERT INTO received VALUES(3)")?;
    query(&socket, "SELECT pg_switch_wal()")?;
    wait("completed WAL segment", || wal.join(&segment).is_file())?;
    drop(receiver);
    let size = query(
        &socket,
        "SELECT pg_size_bytes(current_setting('wal_segment_size'))",
    )?
    .parse::<u64>()?;
    check_continuity(&wal, timeline, first, &segment, size)?;
    fs::write(base.join("standby.signal"), b"")?;
    let restore = format!("restore_command=cp -- {}/%f %p", wal.display());
    let _trial = run(
        tools
            .cmd("postgres")
            .arg("-D")
            .arg(&base)
            .args(server_opts)
            .arg(&standby)
            .arg("-c")
            .arg(restore)
            .args([
                "-c",
                "recovery_target_action=pause",
                "-c",
                "recovery_target_timeline=current",
                "-c",
                "recovery_target_name=td_cut_fixture",
            ]),
        &root.join("trial.log"),
    )?;
    wait("exact archive pause", || {
        query(&standby,&format!("SELECT pg_is_in_recovery() AND pg_get_wal_replay_pause_state()='paused' AND pg_last_wal_replay_lsn()='{lsn}'::pg_lsn")).as_deref().ok()==Some("t")
    })?;
    assert_eq!(
        query(&standby, "SELECT array_agg(id ORDER BY id) FROM received")?,
        "{1,2}"
    );
    assert_eq!(
        query(&socket, "SELECT array_agg(id ORDER BY id) FROM received")?,
        "{1,2,3}"
    );
    Ok(())
}
