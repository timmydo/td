//! HTTP write receipts last one minute (POSTGRES.md section
//! 217): a full ledger refuses only while its receipts are young,
//! and the next publication of that family deletes only its own account's
//! expired ones. Ages are literals, so a longer retention fails here.
use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{ids::StoreEpoch, postgres_probe::Request};

const NOW: &str = "(extract(epoch FROM clock_timestamp())*1000)::bigint";
const OTHER: &str = "decode(repeat('7c',16),'hex')";

fn stamp(table: &str) -> &'static str {
    if table == "submission_receipts" {
        "send_at"
    } else {
        "created_at"
    }
}

/// How far `hold` moves a receipt's stamp ahead.
pub(super) const HELD_MS: i64 = 86_400_000;

fn shift(table: &str, by: &str) -> String {
    let stamp = stamp(table);
    if matches!(table, "upload_receipts" | "submission_receipts") {
        format!("{stamp}={stamp}{by},expires_at=expires_at{by}")
    } else {
        format!("{stamp}={stamp}{by}")
    }
}

/// Keep `account`'s receipts (not the filler) through publications that
/// would otherwise sweep them, for replays later in a long fixture: their
/// stamps move a day ahead until `release`.
pub(super) fn hold(fixture: &Fixture, table: &str, account: &str) {
    let set = shift(table, &format!("+{HELD_MS}"));
    sql(fixture, &format!("UPDATE td_mta.{table} SET {set} WHERE account=decode('{account}','hex') AND request_digest<>decode(repeat('00',32),'hex')"));
}

/// Undo one `hold`, leaving receipts published meanwhile untouched.
pub(super) fn release(fixture: &Fixture, table: &str, account: &str) {
    let stamp = stamp(table);
    let set = shift(table, &format!("-{HELD_MS}"));
    sql(fixture, &format!("UPDATE td_mta.{table} SET {set} WHERE account=decode('{account}','hex') AND {stamp}>{NOW}+{}", HELD_MS / 2));
}

/// Fill `account`'s ledger to the 1,024 ceiling with live copies of one of
/// its receipts whose request digest is all zeroes.
pub(super) fn fill(fixture: &Fixture, table: &str, account: &str) {
    let stamp = stamp(table);
    let expiry = if matches!(table, "upload_receipts" | "submission_receipts") {
        format!(",expires_at={NOW}+86400000")
    } else {
        String::new()
    };
    sql(fixture, &format!("CREATE TEMP TABLE filler AS SELECT r.*,n FROM (SELECT * FROM td_mta.{table} WHERE account=decode('{account}','hex') LIMIT 1) r CROSS JOIN generate_series(1,1024) n; UPDATE filler SET operation=decode(lpad(to_hex(n),32,'0'),'hex'),request_digest=decode(repeat('00',32),'hex'),{stamp}={NOW}{expiry}; ALTER TABLE filler DROP COLUMN n; INSERT INTO td_mta.{table} SELECT * FROM filler"));
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT count(*) FROM td_mta.{table} WHERE request_digest=decode(repeat('00',32),'hex')")
        ),
        "1024"
    );
}

/// Backdate the filler past retention, add a receipt 30 seconds old and an
/// expired receipt of another account, and require capacity inspection to
/// count only `account`'s live receipts.
pub(super) fn expire(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    epoch: StoreEpoch,
    table: &str,
    account: &str,
) {
    let stamp = stamp(table);
    sql(fixture, &format!("UPDATE td_mta.{table} SET {stamp}={stamp}-61000 WHERE request_digest=decode(repeat('00',32),'hex'); INSERT INTO td_mta.accounts VALUES({OTHER},decode(repeat('00',8),'hex'),decode(repeat('00',8),'hex')); INSERT INTO td_mta.account_usage VALUES({OTHER},0,0,0,0,0); CREATE TEMP TABLE kept AS SELECT * FROM td_mta.{table} WHERE request_digest=decode(repeat('00',32),'hex') LIMIT 2; UPDATE kept SET operation=decode(repeat('11',16),'hex'),request_digest=decode(repeat('11',32),'hex'),{stamp}={NOW}-30000 WHERE operation=(SELECT operation FROM kept ORDER BY operation LIMIT 1); UPDATE kept SET account={OTHER},request_digest=decode(repeat('22',32),'hex') WHERE request_digest=decode(repeat('00',32),'hex'); INSERT INTO td_mta.{table} SELECT * FROM kept"));
    let capacity = td_mta::postgres_capacity::inspect(request, trust, deadline(), epoch).unwrap();
    let used = capacity
        .get("metrics")
        .unwrap()
        .as_arr()
        .unwrap()
        .iter()
        .find(|r| {
            r.get("name").and_then(td_json::Json::as_str) == Some(table)
                && r.get("scope").and_then(td_json::Json::as_str) == Some(account)
        })
        .unwrap()
        .get("used")
        .unwrap()
        .to_string();
    assert_eq!(
        used,
        sql(
            fixture,
            &format!("SELECT count(*) FROM td_mta.{table} WHERE account=decode('{account}','hex') AND {stamp}>={NOW}-60000")
        ),
        "{table}"
    );
    assert!(used.parse::<u64>().unwrap() < 1024, "{table}");
}

/// The publication deleted exactly this account's expired filler.
pub(super) fn swept(fixture: &Fixture, table: &str) {
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT count(*) FROM td_mta.{table} WHERE request_digest=decode(repeat('00',32),'hex')")
        ),
        "0",
        "{table}"
    );
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT count(*) FROM td_mta.{table} WHERE request_digest IN (decode(repeat('11',32),'hex'),decode(repeat('22',32),'hex'))")
        ),
        "2",
        "{table}"
    );
    sql(fixture, &format!("DELETE FROM td_mta.{table} WHERE request_digest IN (decode(repeat('11',32),'hex'),decode(repeat('22',32),'hex')); DELETE FROM td_mta.account_usage WHERE account={OTHER}; DELETE FROM td_mta.accounts WHERE id={OTHER}"));
}
