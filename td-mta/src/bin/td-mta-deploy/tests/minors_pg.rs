#![allow(clippy::unwrap_used)]
//! Every check of the minor-release table runs on a real PostgreSQL 17 and
//! counts nothing on an empty cluster; the checks of what core and pg_trgm
//! can create count it once created; the GIN rebuild rebuilds a trigram
//! index.
use super::*;
use crate::postgres_fixture::{checked, run, wait, PgTools, Process};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path, process::Stdio};

fn script(tools: &PgTools, sockets: &Path, database: &str, sql: &str) -> R<String> {
    let mut child = tools
        .cmd("psql")
        .args(["-XwqAt", "-v", "ON_ERROR_STOP=1", "-h"])
        .arg(sockets)
        .args(["-p", "35432", "-U", "fixture", "-d", database, "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(sql.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("psql: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn checks() -> impl Iterator<Item = (&'static str, &'static Check)> {
    MINORS
        .iter()
        .flat_map(|m| m.notes.iter().map(|n| (n.id, &n.check)))
}

#[test]
#[ignore = "requires PostgreSQL 17 tools with pg_trgm as a non-root user"]
fn real_postgres_minor_release_checks_count_and_gin_indexes_rebuild() -> R<()> {
    let tools = PgTools::new()?;
    let directory = td_fs::TempDir::new_in(Path::new("/tmp"), "tdminor")?;
    let root = directory.path();
    let data = root.join("data");
    let sockets = root.join("s");
    fs::create_dir(&sockets)?;
    fs::set_permissions(&sockets, fs::Permissions::from_mode(0o700))?;
    checked(tools.cmd("initdb").arg("-D").arg(&data).args([
        "--no-sync",
        "--no-locale",
        "-E",
        "UTF8",
        "-U",
        "fixture",
        "--auth-local=trust",
        "--auth-host=reject",
    ]))?;
    let log = root.join("server.log");
    let _server: Process = run(
        tools
            .cmd("postgres")
            .arg("-D")
            .arg(&data)
            .args(["-c", "listen_addresses=", "-c", "port=35432", "-c"])
            .arg(format!("unix_socket_directories={}", sockets.display())),
        &log,
    )?;
    wait("server startup", || {
        fs::read_to_string(&log)
            .is_ok_and(|text| text.contains("database system is ready to accept connections"))
    })?;
    let sql = |database: &str, text: &str| script(&tools, &sockets, database, text);
    sql("postgres", "CREATE DATABASE td_mta")?;
    // td-mta's own search index: a trigram GIN index in its own schema.
    sql(
        "td_mta",
        "CREATE SCHEMA td_mta_trgm; CREATE EXTENSION pg_trgm SCHEMA td_mta_trgm;
         CREATE SCHEMA td_mta;
         CREATE TABLE td_mta.blob_search (candidate_text text);
         INSERT INTO td_mta.blob_search SELECT 'message ' || g FROM generate_series(1, 200) g;
         CREATE INDEX blob_search_candidates ON td_mta.blob_search
           USING gin (candidate_text td_mta_trgm.gin_trgm_ops);",
    )?;
    for database in ["postgres", "td_mta"] {
        for (id, check) in checks() {
            if let Check::Database(probe) | Check::Cluster(probe) = check {
                assert_eq!(sql(database, probe)?, "0", "{id} in {database}");
            }
        }
    }
    assert_eq!(sql("postgres", GIN_INDEXES)?, "0");
    assert_eq!(sql("td_mta", GIN_INDEXES)?, "1");
    let before = sql(
        "td_mta",
        "SELECT relfilenode FROM pg_class WHERE relname = 'blob_search_candidates'",
    )?;
    assert_eq!(sql("td_mta", REINDEX_GIN)?, "1");
    let after = sql(
        "td_mta",
        "SELECT relfilenode FROM pg_class WHERE relname = 'blob_search_candidates'",
    )?;
    assert_ne!(before, after, "the GIN index was not rebuilt");
    assert_eq!(
        sql(
            "td_mta",
            "SET enable_seqscan = off; SELECT count(*) FROM td_mta.blob_search \
             WHERE candidate_text LIKE '%sage 17%'"
        )?,
        "11"
    );

    // Each concerned object is counted by the check that names it.
    let probe = |id: &str| -> &'static str {
        checks()
            .find_map(|(name, check)| match check {
                Check::Database(p) | Check::Cluster(p) if name == id => Some(*p),
                _ => None,
            })
            .unwrap()
    };
    for (id, create) in [
        (
            "17.5-partitioned-self-reference-foreign-key",
            "CREATE TABLE tree (id int PRIMARY KEY, parent int REFERENCES tree) \
             PARTITION BY RANGE (id)",
        ),
        (
            "17.5-brin-bloom",
            "CREATE TABLE bloom (n int); \
             CREATE INDEX ON bloom USING brin (n int4_bloom_ops)",
        ),
        (
            "17.6-brin-numeric-minmax-multi",
            "CREATE TABLE multi (n numeric); \
             CREATE INDEX ON multi USING brin (n numeric_minmax_multi_ops)",
        ),
        (
            "17.12-bpchar-btree",
            "CREATE TABLE padded (x bpchar, y char(3)); \
             CREATE INDEX ON padded (x); CREATE INDEX ON padded (y)",
        ),
        (
            "17.12-pg-trgm-gist",
            "CREATE TABLE fuzzy (t text); \
             CREATE INDEX ON fuzzy USING gist (t td_mta_trgm.gist_trgm_ops)",
        ),
    ] {
        sql("td_mta", create)?;
        assert_eq!(sql("td_mta", probe(id))?, "1", "{id}");
        assert_eq!(sql("postgres", probe(id))?, "0", "{id}");
    }
    // An unconstrained bpchar behind a domain is still a bpchar key.
    sql(
        "td_mta",
        "CREATE DOMAIN padded_domain AS bpchar; CREATE TABLE wrapped (d padded_domain); \
         CREATE INDEX ON wrapped (d)",
    )?;
    assert_eq!(sql("td_mta", probe("17.12-bpchar-btree"))?, "2");
    sql(
        "postgres",
        "ALTER SYSTEM SET summarize_wal = on; SELECT pg_reload_conf()",
    )?;
    wait("summarize_wal", || {
        sql("postgres", "SHOW summarize_wal").is_ok_and(|v| v == "on")
    })?;
    assert_eq!(sql("postgres", probe("17.12-incremental-backup"))?, "1");
    Ok(())
}
