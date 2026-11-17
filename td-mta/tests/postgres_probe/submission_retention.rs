use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::{BootId, SubmissionId},
    postgres_probe::Request,
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
    postgres_submission_retention as retention, postgres_usage_audit as audit,
};

const NOW: &str = "(extract(epoch FROM clock_timestamp())*1000)::bigint";
const PREFIX: &str = "72657461696e";

// "retain" prefix: other fixtures repeat one byte, and blob IDs stay
// reserved after retirement, so a repeated byte would collide later.
fn hex(byte: u8) -> String {
    format!("{PREFIX}000000000000000000{byte:02x}")
}

fn id(byte: u8) -> String {
    format!("decode('{}','hex')", hex(byte))
}

fn ids(bytes: &[u8]) -> String {
    bytes.iter().map(|b| id(*b)).collect::<Vec<_>>().join(",")
}

fn bytea(byte: u8) -> String {
    format!("'\\x{}'", hex(byte))
}

// Copy one existing row with its keys replaced, so the fixture holds every
// receipt kind without restating each schema.
fn clone(fixture: &Fixture, table: &str, overrides: &[(&str, String)]) {
    let fields = overrides
        .iter()
        .map(|(name, value)| format!("'{name}',{value}"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        sql(
            fixture,
            &format!("WITH c AS (INSERT INTO td_mta.{table} SELECT (jsonb_populate_record(NULL::td_mta.{table},to_jsonb(r)||jsonb_build_object({fields}))).* FROM td_mta.{table} r LIMIT 1 RETURNING 1) SELECT count(*) FROM c")
        ),
        "1",
        "{table} clone"
    );
}

// A 100-byte blob carries its chunk; a larger one is length only, which is
// all retention and the usage audit read.
fn blob(fixture: &Fixture, src: &str, blob: u8, length: i64) {
    let b = id(blob);
    let chunks = (length + 65535) / 65536;
    sql(fixture, &format!("INSERT INTO td_mta.blob_ids SELECT account,{b} FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.blobs SELECT account,{b},{length},decode(repeat('00',32),'hex'),0,changed,{chunks} FROM td_mta.submissions WHERE id={src}"));
    if length == 100 {
        sql(fixture, &format!("INSERT INTO td_mta.blob_chunks SELECT account,{b},0,decode(repeat('ab',100),'hex') FROM td_mta.submissions WHERE id={src}"));
    }
}

struct Group<'a> {
    id: u8,
    blob: &'a str,
    recipients: i64,
    // state, uncertain, attempt, count, last, phase, next, replies, reason
    recipient: &'a str,
    completed: &'a str,
    notification: i64,
}

fn submission(fixture: &Fixture, src: &str, group: Group<'_>) {
    let sub = id(group.id);
    let notice = if group.notification == 2 {
        id(group.id ^ 0xff)
    } else {
        "NULL".into()
    };
    sql(fixture, &format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,{},reverse_path,send_at,expires_at,{},{},{},{notice},changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},n,'retain@example.test',{},'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,{}) n WHERE id={sub}", group.blob, group.recipients, group.completed, group.notification, group.recipient, group.recipients - 1));
}

fn attempt(fixture: &Fixture, attempt: u8, sub: u8, extra: &[(&str, String)]) {
    let mut fields = vec![
        ("attempt_id", bytea(attempt)),
        ("submission_id", bytea(sub)),
        ("active", "false".into()),
    ];
    fields.extend_from_slice(extra);
    clone(fixture, "submission_attempts", &fields);
}

fn count(fixture: &Fixture, query: &str) -> String {
    sql(fixture, &format!("SELECT count(*) FROM {query}"))
}

// Earlier fixtures insert queue rows without charging them; start from the
// audited truth so every decrement is checked exactly.
fn resync(fixture: &Fixture, account: &str) {
    sql(fixture, &format!("UPDATE td_mta.account_usage u SET body_bytes=o.body,blob_count=o.blobs,upload_bytes=o.upload,queue_bytes=o.queue,queue_submissions=o.submissions FROM (SELECT coalesce(sum(b.length),0)::bigint body,count(*) blobs,coalesce(sum(b.length) FILTER (WHERE EXISTS (SELECT 1 FROM td_mta.leases l WHERE l.account=b.account AND l.blob_id=b.id)),0)::bigint upload,coalesce(sum(b.length) FILTER (WHERE EXISTS (SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id)),0)::bigint queue,(SELECT count(*) FROM td_mta.submissions s WHERE s.account={account}) submissions FROM td_mta.blobs b WHERE b.account={account}) o WHERE u.account={account}"));
}

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    source: SubmissionId,
) {
    let src = format!("decode('{source}','hex')");
    let expired = format!("{NOW}-{}-60000", retention::RETENTION_MS);
    let kept = format!("{NOW}-{}+3600000", retention::RETENTION_MS);
    let accepted =
        |attempt: u8| format!("4,0,{},1,0,4,NULL,'250 ok','250 accepted',0", id(attempt));
    let uncertain =
        |attempt: u8| format!("4,1,{},1,0,4,NULL,'250 ok','250 accepted',0", id(attempt));
    let unknown = |attempt: u8| format!("7,1,{},1,0,3,NULL,'250 ok',NULL,9", id(attempt));
    let refused_then_canceled =
        |attempt: u8| format!("6,0,{},1,0,4,NULL,'550 refused',NULL,7", id(attempt));
    let canceled = "6,0,NULL,0,NULL,0,NULL,NULL,NULL,7";
    let failed = "5,0,NULL,0,NULL,0,NULL,NULL,NULL,2";
    let queued = "1,0,NULL,0,NULL,0,0,NULL,NULL,0";
    for b in [0xa4, 0xb2, 0xb3] {
        blob(fixture, &src, b, 100);
    }
    blob(fixture, &src, 0xa5, 32 * 1024 * 1024);
    // The second blob stays leased after its only submission retires.
    sql(fixture, &format!("INSERT INTO td_mta.leases SELECT account,{},decode(repeat('77',16),'hex'),0,1,changed FROM td_mta.submissions WHERE id={src}", id(0xb2)));
    let source_blob =
        format!("(SELECT transmitted_blob_id FROM td_mta.submissions WHERE id={src})");
    let (a4, b2, b3, a5) = (id(0xa4), id(0xb2), id(0xb3), id(0xa5));
    let groups: &[(u8, &str, i64, String, &str, i64)] = &[
        // Retired: accepted with every receipt kind; its blob is reclaimed.
        (0xe2, a4.as_str(), 1, accepted(0xe2), expired.as_str(), 0),
        // Retired: canceled with a stored notice; its leased blob stays.
        (0xe3, b2.as_str(), 1, canceled.into(), expired.as_str(), 2),
        // Kept: completed inside retention, sharing a blob with a retired group.
        (0xe4, b3.as_str(), 1, accepted(0xe4), kept.as_str(), 0),
        // Retired: the shared blob stays queued for the kept group.
        (0xe5, b3.as_str(), 1, accepted(0xe5), expired.as_str(), 0),
        // Retired: a failure whose notice is stored.
        (
            0xe6,
            source_blob.as_str(),
            1,
            failed.into(),
            expired.as_str(),
            2,
        ),
        // Kept: its failure notice is still pending.
        (
            0xe7,
            source_blob.as_str(),
            1,
            canceled.into(),
            expired.as_str(),
            1,
        ),
        // Retired over two batches: 3000 recipients exceed one.
        (
            0xe8,
            source_blob.as_str(),
            1000,
            canceled.into(),
            expired.as_str(),
            0,
        ),
        (
            0xe9,
            source_blob.as_str(),
            1000,
            canceled.into(),
            expired.as_str(),
            0,
        ),
        (
            0xea,
            source_blob.as_str(),
            1000,
            canceled.into(),
            expired.as_str(),
            0,
        ),
        // Retired: one Accepted and one Failed recipient (made below).
        (
            0xeb,
            source_blob.as_str(),
            2,
            accepted(0xeb),
            expired.as_str(),
            2,
        ),
        // Retired: OutcomeUnknown with its notice stored.
        (
            0xec,
            source_blob.as_str(),
            1,
            unknown(0xec),
            expired.as_str(),
            2,
        ),
        // Kept: never completed.
        (0xed, source_blob.as_str(), 1, queued.into(), "NULL", 0),
        // Kept until released: an active attempt, then a final reservation.
        (
            0xee,
            source_blob.as_str(),
            1,
            accepted(0xee),
            expired.as_str(),
            0,
        ),
        (
            0xef,
            source_blob.as_str(),
            1,
            accepted(0xef),
            expired.as_str(),
            0,
        ),
        // Retired: Accepted with recorded duplicate risk.
        (
            0xf0,
            source_blob.as_str(),
            1,
            uncertain(0xf0),
            expired.as_str(),
            0,
        ),
        // Retired: a refused group later canceled keeps its completedAt and
        // joins the Canceled category (QUEUE.md).
        (
            0xf1,
            source_blob.as_str(),
            1,
            refused_then_canceled(0xf1),
            expired.as_str(),
            2,
        ),
        // Retired: more attempts than the connection's frame budget holds rows.
        (
            0xf2,
            source_blob.as_str(),
            1,
            accepted(0xf2),
            expired.as_str(),
            0,
        ),
        // Retired: a 32 MiB transmitted blob is reclaimed.
        (0xf3, a5.as_str(), 1, accepted(0xf3), expired.as_str(), 0),
    ];
    for (group, blob, recipients, recipient, completed, notification) in groups {
        submission(
            fixture,
            &src,
            Group {
                id: *group,
                blob,
                recipients: *recipients,
                recipient,
                completed,
                notification: *notification,
            },
        );
    }
    sql(fixture, &format!("UPDATE td_mta.recipients SET state=5,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=NULL,rcpt_reply=NULL,data_reply=NULL,reason=2 WHERE submission_id={} AND ordinal=1", id(0xeb)));
    for group in [0xe2, 0xe4, 0xe5, 0xef, 0xf0, 0xf1, 0xf2] {
        attempt(fixture, group, group, &[]);
    }
    attempt(
        fixture,
        0xee,
        0xee,
        &[
            ("active", "true".into()),
            ("route_name", "'retain-route'".into()),
        ],
    );
    sql(fixture, &format!("INSERT INTO td_mta.attempt_final_reservations SELECT account,attempt_id,ordinals FROM td_mta.submission_attempts WHERE attempt_id={}", id(0xef)));
    sql(fixture, &format!("INSERT INTO td_mta.submission_attempts SELECT (jsonb_populate_record(NULL::td_mta.submission_attempts,to_jsonb(a)||jsonb_build_object('attempt_id','\\x{PREFIX}0000000000f2'||lpad(to_hex(n),8,'0'),'submission_id',{},'active',false))).* FROM (SELECT * FROM td_mta.submission_attempts LIMIT 1) a CROSS JOIN generate_series(1,900) n", bytea(0xf2)));
    let e2 = bytea(0xe2);
    for table in [
        "attempt_phase_receipts",
        "attempt_acceptance_receipts",
        "attempt_refusal_receipts",
        "attempt_rcpt_receipts",
        "attempt_command_receipts",
    ] {
        clone(fixture, table, &[("attempt_id", e2.clone())]);
    }
    // A kept attempt's receipt stays.
    clone(
        fixture,
        "attempt_acceptance_receipts",
        &[("attempt_id", bytea(0xe4))],
    );
    clone(
        fixture,
        "attempt_recovery_receipts",
        &[("operation", bytea(0xc1)), ("attempt_id", e2.clone())],
    );
    clone(
        fixture,
        "route_health_receipts",
        &[("operation", e2.clone()), ("action", "3".into())],
    );
    clone(
        fixture,
        "route_health_receipts",
        &[("operation", bytea(0xe5)), ("action", "1".into())],
    );
    for (table, operation) in [
        ("submission_receipts", 0xc2),
        ("submission_cancel_receipts", 0xc3),
        ("submission_expiry_receipts", 0xc4),
    ] {
        clone(
            fixture,
            table,
            &[
                ("operation", bytea(operation)),
                ("submission_id", e2.clone()),
            ],
        );
    }
    let account = format!("decode('{}','hex')", binding.account());
    resync(fixture, &account);
    let usage = || {
        audit::inspect(
            request,
            trust,
            deadline(),
            binding.epoch(),
            binding.account(),
        )
        .unwrap()
        .usage
    };
    let before = usage();
    let sequence = || {
        sql(
            fixture,
            &format!("SELECT encode(sequence,'hex') FROM td_mta.accounts WHERE id={account}"),
        )
    };
    let old_sequence = sequence();
    let now = || -> i64 { sql(fixture, &format!("SELECT {NOW}")).parse().unwrap() };

    let claim = owners::prepare(binding, BootId::from_bytes([93; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let retire = || retention::retire(request, trust, deadline(), &claim, grant.generation, now());
    assert_eq!(
        retention::retire(
            request,
            trust,
            deadline(),
            &claim,
            grant.generation + 1,
            now()
        ),
        Err(retention::Error::Owner(owners::Error::Stale))
    );
    assert_eq!(
        retention::retire(
            request,
            trust,
            deadline(),
            &claim,
            grant.generation,
            now() - 2 * retention::CLOCK_SLACK_MS
        ),
        Err(retention::Error::Clock),
        "a database clock that moved since the stable observation"
    );
    assert_eq!(sequence(), old_sequence);
    let first = retire().unwrap();
    assert!(first > 0);
    assert!(
        count(
            fixture,
            &format!(
                "td_mta.submissions WHERE id IN ({})",
                ids(&[0xe8, 0xe9, 0xea])
            )
        ) != "0",
        "recipient bound splits the batch"
    );
    assert_ne!(sequence(), old_sequence);
    let drain = || {
        let mut retired = 0;
        loop {
            match retire().unwrap() {
                0 => break retired,
                n => retired += n,
            }
        }
    };
    let mut total = first + drain();
    let gone = ids(&[
        0xe2, 0xe3, 0xe5, 0xe6, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xf0, 0xf1, 0xf2, 0xf3,
    ]);
    let stay = ids(&[0xe4, 0xe7, 0xed, 0xee, 0xef]);
    for (query, expected) in [
        (format!("td_mta.submissions WHERE id IN ({gone})"), "0"),
        (
            format!("td_mta.recipients WHERE submission_id IN ({gone})"),
            "0",
        ),
        (
            format!("td_mta.submission_routes WHERE submission_id IN ({gone})"),
            "0",
        ),
        (
            format!("td_mta.submission_attempts WHERE submission_id IN ({gone})"),
            "0",
        ),
        (
            format!("td_mta.changes WHERE kind=5 AND action=3 AND object IN ({gone})"),
            "13",
        ),
        (format!("td_mta.submissions WHERE id IN ({stay})"), "5"),
        (
            format!("td_mta.recipients WHERE submission_id IN ({stay})"),
            "5",
        ),
        (
            format!("td_mta.changes WHERE kind=5 AND action=3 AND object IN ({stay})"),
            "0",
        ),
        (
            format!(
                "td_mta.submission_attempts WHERE attempt_id IN ({})",
                ids(&[0xe4, 0xee, 0xef])
            ),
            "3",
        ),
        (
            format!(
                "td_mta.attempt_acceptance_receipts WHERE attempt_id={}",
                id(0xe4)
            ),
            "1",
        ),
        (
            format!(
                "td_mta.route_health_receipts WHERE operation IN ({})",
                ids(&[0xe2, 0xe5])
            ),
            "0",
        ),
    ] {
        assert_eq!(count(fixture, &query), expected, "{query}");
    }
    let e2 = id(0xe2);
    for table in [
        "attempt_phase_receipts",
        "attempt_acceptance_receipts",
        "attempt_refusal_receipts",
        "attempt_rcpt_receipts",
        "attempt_command_receipts",
        "attempt_recovery_receipts",
    ] {
        assert_eq!(
            count(fixture, &format!("td_mta.{table} WHERE attempt_id={e2}")),
            "0",
            "{table}"
        );
    }
    for table in [
        "submission_receipts",
        "submission_cancel_receipts",
        "submission_expiry_receipts",
    ] {
        assert_eq!(
            count(fixture, &format!("td_mta.{table} WHERE submission_id={e2}")),
            "0",
            "{table}"
        );
    }
    // The small and large reclaimed blobs go; the leased one stays and the
    // shared one stays queued; every ID stays reserved.
    assert_eq!(
        count(fixture, &format!("td_mta.blobs WHERE id IN ({a4},{a5})")),
        "0"
    );
    assert_eq!(
        count(fixture, &format!("td_mta.blob_chunks WHERE blob_id={a4}")),
        "0"
    );
    assert_eq!(
        count(fixture, &format!("td_mta.blobs WHERE id IN ({b2},{b3})")),
        "2"
    );
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.blob_ids WHERE id IN ({a4},{a5},{b2},{b3})")
        ),
        "4"
    );
    let after = usage();
    assert_eq!(before.queue_submissions - after.queue_submissions, total);
    assert!(before.blob_count >= after.blob_count + 2);
    assert!(before.body_bytes >= after.body_bytes + 100 + 32 * 1024 * 1024);
    assert!(before.queue_bytes >= after.queue_bytes + 200 + 32 * 1024 * 1024);
    assert_eq!(after.upload_bytes, before.upload_bytes);

    // Releasing the active attempt and the final reservation admits both.
    sql(fixture, &format!("UPDATE td_mta.submission_attempts SET active=false WHERE attempt_id={}; DELETE FROM td_mta.attempt_final_reservations WHERE attempt_id={}", id(0xee), id(0xef)));
    total += drain();
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.submissions WHERE id IN ({})", ids(&[0xee, 0xef]))
        ),
        "0"
    );
    assert_eq!(before.queue_submissions - usage().queue_submissions, total);
    assert_eq!(retire(), Ok(0));
    owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();
    assert!(
        matches!(retire(), Err(retention::Error::Owner(_))),
        "released owner fenced"
    );

    // Later fixtures see none of these rows.
    let rest = ids(&[0xe4, 0xe6, 0xe7, 0xeb, 0xec, 0xed]);
    sql(fixture, &format!("DELETE FROM td_mta.attempt_acceptance_receipts WHERE attempt_id={}; DELETE FROM td_mta.submission_attempts WHERE attempt_id={}; DELETE FROM td_mta.recipients WHERE submission_id IN ({rest}); DELETE FROM td_mta.submissions WHERE id IN ({rest}); DELETE FROM td_mta.leases WHERE blob_id={b2}", id(0xe4), id(0xe4)));
    resync(fixture, &account);
}
