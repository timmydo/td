use super::{attempt_clock, deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AttemptId, BootId, SubmissionId},
    postgres_attempt_phases as phases,
    postgres_attempt_reclamation::RECLAIM_ATTEMPTS,
    postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_health::RECEIPT_RETENTION_MS,
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
};

const NOW: &str = "(extract(epoch FROM clock_timestamp())*1000)::bigint";
// "reclaim": other fixtures repeat one byte.
const PREFIX: &str = "7265636c61696d";
const SEEDS: i64 = 1024;
// Seeds this many carry every receipt kind: the first two batches.
const RECEIPTED: i64 = 2 * RECLAIM_ATTEMPTS;
const FAILED: u8 = 0x01;
const UNKNOWN: u8 = 0x02;
const QUEUED: u8 = 0x03;
const SEED: u8 = 0x10;
const RESERVED: u8 = 0x11;
const ACTIVE: u8 = 0x12;
const YOUNG: u8 = 0x13;
const OLD: u8 = 0x14;
const FOREIGN: u8 = 0x15;
const ACCOUNT: u8 = 0x20;
const RECOVERY: u8 = 0x30;
const HEALTH_FILLER: u8 = 0x31;

fn hex(tag: u8, n: i64) -> String {
    format!("{PREFIX}{tag:02x}{n:016x}")
}

fn id(tag: u8, n: i64) -> String {
    format!("decode('{}','hex')", hex(tag, n))
}

// An SQL expression for the ID of `tag` numbered by column `n`.
fn series(tag: u8) -> String {
    format!("decode('{PREFIX}{tag:02x}'||lpad(to_hex(n),16,'0'),'hex')")
}

fn count(fixture: &Fixture, query: &str) -> i64 {
    sql(fixture, &format!("SELECT count(*) FROM {query}"))
        .parse()
        .unwrap()
}

// Every attempt-keyed receipt kind, as (table, attempt column).
const RECEIPTS: &[(&str, &str)] = &[
    ("attempt_phase_receipts", "attempt_id"),
    ("attempt_rcpt_receipts", "attempt_id"),
    ("attempt_command_receipts", "attempt_id"),
    ("attempt_acceptance_receipts", "attempt_id"),
    ("attempt_refusal_receipts", "attempt_id"),
    ("attempt_recovery_receipts", "attempt_id"),
    ("route_health_receipts", "operation"),
];

// Every receipt kind for the attempts `tag` numbers `from..=to`.
fn receipts(fixture: &Fixture, account: &str, tag: u8, from: i64, to: i64) {
    let attempt = series(tag);
    let digest = "decode(repeat('ab',32),'hex')";
    let old = "decode('0000000000000001','hex')";
    let new = "decode('0000000000000002','hex')";
    let rows = format!("FROM generate_series({from},{to}) n");
    sql(fixture, &format!("INSERT INTO td_mta.attempt_phase_receipts SELECT {account},{attempt},p,{digest},1,{old},{new} {rows} CROSS JOIN (VALUES (2),(3)) phases(p); INSERT INTO td_mta.attempt_rcpt_receipts SELECT {account},{attempt},6,{digest},1,{old},{new} {rows}; INSERT INTO td_mta.attempt_command_receipts SELECT {account},{attempt},7,{digest},1,{old},{new} {rows}; INSERT INTO td_mta.attempt_acceptance_receipts SELECT {account},{attempt},4,{digest},1,{old},{new} {rows}; INSERT INTO td_mta.attempt_refusal_receipts SELECT {account},{attempt},5,{digest},1,{old},{new} {rows}; INSERT INTO td_mta.attempt_recovery_receipts SELECT {account},decode('{PREFIX}{RECOVERY:02x}{tag:02x}'||lpad(to_hex(n),14,'0'),'hex'),{digest},{attempt},1,{old},{new} {rows}"));
    // Failure (1) for odd attempts and success (3) for even ones.
    sql(fixture, &format!("INSERT INTO td_mta.route_health_receipts SELECT {account},{attempt},{digest},r.route_name,r.destination,CASE WHEN n%2=0 THEN 3 ELSE 1 END,{old},{new},n%2,CASE WHEN n%2=0 THEN 0 ELSE 3 END,CASE WHEN n%2=0 THEN NULL ELSE 0 END,CASE WHEN n%2=0 THEN NULL ELSE 300000 END,NULL {rows} CROSS JOIN (SELECT route_name,destination FROM td_mta.submission_attempts WHERE attempt_id={}) r", id(tag, from)));
}

// Receipt rows of `tag` numbered `from..=to`, summed over every kind.
fn receipted(fixture: &Fixture, tag: u8, from: i64, to: i64) -> i64 {
    let ids = format!(
        "(SELECT {} FROM generate_series({from},{to}) n)",
        series(tag)
    );
    RECEIPTS
        .iter()
        .map(|(table, column)| count(fixture, &format!("td_mta.{table} WHERE {column} IN {ids}")))
        .sum()
}

// Rows each receipted attempt holds: two phases, one of each other kind.
const PER_ATTEMPT: i64 = 8;

fn submission(fixture: &Fixture, src: &str, sub: u8, fields: &str, recipient: &str, n: i64) {
    let s = id(sub, 0);
    sql(fixture, &format!("INSERT INTO td_mta.submissions SELECT account,{s},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,{fields},changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{s},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{s},n,'reclaim@example.test',{recipient},'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,{}) n WHERE id={s}", n - 1));
}

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    source: SubmissionId,
) {
    let src = format!("decode('{source}','hex')");
    let account = format!("decode('{}','hex')", binding.account());
    let route = format!("'{}'", binding.route().name());
    let destination = format!(
        "decode('{}','hex')",
        binding
            .route()
            .destination()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let now: i64 = sql(fixture, &format!("SELECT {NOW}")).parse().unwrap();
    // Seeds are prepared well inside the other-route margin.
    let base = now - RECEIPT_RETENTION_MS + 20 * 60_000;
    let completed = format!("{now}-60000");
    let notice = |sub: u8| id(sub, 1);
    submission(
        fixture,
        &src,
        FAILED,
        &format!("send_at,expires_at,2,{completed},2,{}", notice(FAILED)),
        &format!("5,0,{},512,{base},4,NULL,'550 refused',NULL,1", id(SEED, 2)),
        2,
    );
    submission(
        fixture,
        &src,
        UNKNOWN,
        &format!("send_at,expires_at,2,{completed},2,{}", notice(UNKNOWN)),
        &format!("7,1,{},512,{base},4,NULL,'250 ok',NULL,9", id(SEED, 1)),
        2,
    );
    submission(
        fixture,
        &src,
        QUEUED,
        &format!("{now}-1000,{now}+3600000,1,NULL,0,NULL"),
        "1,0,NULL,0,NULL,0,0,NULL,NULL,0",
        1,
    );
    let settled = format!("({},{})", id(FAILED, 0), id(UNKNOWN, 0));
    let recipients = || {
        sql(fixture, &format!("SELECT md5(string_agg(r::text,',' ORDER BY submission_id,ordinal)) FROM td_mta.recipients r WHERE submission_id IN {settled}"))
    };
    let held = recipients();
    // A submission-keyed receipt stays with its submission.
    sql(fixture, &format!("INSERT INTO td_mta.submission_expiry_receipts VALUES({account},{},decode(repeat('ab',32),'hex'),{},1,0,{now},decode('0000000000000001','hex'),decode('0000000000000002','hex'))", id(FAILED, 2), id(FAILED, 0)));

    // Seeds first sit on another route inside the margin, where nothing
    // may reclaim them.
    let attempt = |tag: u8, account: &str, route: &str, prepared: &str, active: bool| {
        format!("SELECT {account},{},decode(repeat('aa',32),'hex'),CASE WHEN n%2=0 THEN {} ELSE {} END,{route},{destination},decode(repeat('01',16),'hex'),decode(repeat('02',16),'hex'),decode('0000000000000001','hex'),{prepared},decode('00000000','hex'),decode('0000000000000001','hex'),decode('0000000000000002','hex'),{active}", series(tag), id(FAILED, 0), id(UNKNOWN, 0))
    };
    let insert = |select: String, n: i64| {
        sql(
            fixture,
            &format!(
                "INSERT INTO td_mta.submission_attempts {select} FROM generate_series(1,{n}) n"
            ),
        )
    };
    insert(
        attempt(
            SEED,
            &account,
            "'reclaim-other'",
            &format!("{base}+n"),
            false,
        ),
        SEEDS,
    );
    // Older than every seed, yet kept: a final reservation, an active
    // attempt, and another account's own-route rows.
    insert(
        attempt(
            RESERVED,
            &account,
            &route,
            &format!("{base}-7200000"),
            false,
        ),
        1,
    );
    sql(fixture, &format!("INSERT INTO td_mta.attempt_final_reservations SELECT account,attempt_id,ordinals FROM td_mta.submission_attempts WHERE attempt_id={}", id(RESERVED, 1)));
    insert(
        attempt(
            ACTIVE,
            &account,
            "'reclaim-other'",
            &format!("{base}-7200000"),
            true,
        ),
        1,
    );
    let other = id(ACCOUNT, 0);
    sql(fixture, &format!("INSERT INTO td_mta.accounts(id,sequence,floor) VALUES({other},decode('0000000000000001','hex'),decode('0000000000000000','hex'))"));
    insert(
        attempt(FOREIGN, &other, &route, &format!("{base}-7200000+n"), false),
        2,
    );
    // Another route's retired attempts: kept inside the margin, reclaimed
    // beyond it.
    insert(
        attempt(
            YOUNG,
            &account,
            "'reclaim-other'",
            &format!("{now}-1800000"),
            false,
        ),
        1,
    );
    insert(
        attempt(
            OLD,
            &account,
            "'reclaim-other'",
            &format!("{now}-3660000"),
            false,
        ),
        1,
    );
    receipts(fixture, &account, SEED, 1, RECEIPTED);
    for tag in [RESERVED, ACTIVE, YOUNG, OLD] {
        receipts(fixture, &account, tag, 1, 1);
    }
    receipts(fixture, &other, FOREIGN, 1, 2);

    // No earlier attempt would be reclaimed before a seed.
    let ours = format!("substring(attempt_id FROM 1 FOR 7)=decode('{PREFIX}','hex')");
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.submission_attempts WHERE account={account} AND NOT active AND NOT {ours} AND (route_name={route} OR prepared_at<{now}-{RECEIPT_RETENTION_MS}) AND prepared_at<={base}+{SEEDS}")
        ),
        0,
        "an earlier fixture's attempt would precede the seeds"
    );

    let claim = owners::prepare(binding, BootId::from_bytes([94; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let queued = SubmissionId::parse(&hex(QUEUED, 0)).unwrap();
    let publish = || {
        attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::prepare(&claim, grant.generation, queued).unwrap(),
        )
    };
    // Settle each published attempt and requeue the recipient.
    let retire = |outcome: &attempts::Outcome| {
        sql(fixture, &format!("UPDATE td_mta.submission_attempts SET active=false WHERE attempt_id=decode('{}','hex'); UPDATE td_mta.recipients SET state=1,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0 WHERE submission_id={}", outcome.attempt, id(QUEUED, 0)));
    };
    let attempts_of = |tag: u8| {
        count(
            fixture,
            &format!(
                "td_mta.submission_attempts WHERE substring(attempt_id FROM 1 FOR 8)=decode('{PREFIX}{tag:02x}','hex')"
            ),
        )
    };
    assert_eq!(
        publish(),
        Err(attempts::Error::Capacity),
        "retired attempts inside another route's margin fill the account"
    );
    assert_eq!(attempts_of(SEED), SEEDS);

    // A reclaimed attempt's settlement reads absent.
    let first = AttemptId::parse(&hex(SEED, 1)).unwrap();
    let body = phases::body(&claim, grant.generation, first, &[(0, "250 ok")]).unwrap();
    assert_eq!(
        phases::inspect(request, trust, deadline(), &body),
        Err(phases::Error::Conflict)
    );

    // On the owner's route, retired attempts go at any age.
    sql(fixture, &format!("UPDATE td_mta.submission_attempts SET route_name={route} WHERE substring(attempt_id FROM 1 FOR 8)=decode('{PREFIX}{SEED:02x}','hex')"));
    let sequence = || {
        sql(
            fixture,
            &format!("SELECT encode(sequence,'hex') FROM td_mta.accounts WHERE id={account}"),
        )
    };
    let before = sequence();
    let outcome = publish().unwrap();
    assert_eq!(outcome.ordinals, vec![0]);
    // One publication's change, none for the reclaimed rows.
    assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
    assert_eq!(format!("{:016x}", outcome.old_sequence), before);
    // The other route's attempt beyond the margin is the oldest eligible;
    // then the oldest seeds.
    let batch = RECLAIM_ATTEMPTS - 1;
    assert_eq!(attempts_of(OLD), 0);
    assert_eq!(receipted(fixture, OLD, 1, 1), 0);
    assert_eq!(
        count(
            fixture,
            &format!(
                "td_mta.submission_attempts WHERE attempt_id IN (SELECT {} FROM generate_series(1,{batch}) n)",
                series(SEED)
            )
        ),
        0
    );
    assert_eq!(attempts_of(SEED), SEEDS - batch);
    assert_eq!(receipted(fixture, SEED, 1, batch), 0);
    assert_eq!(
        receipted(fixture, SEED, batch + 1, RECEIPTED),
        (RECEIPTED - batch) * PER_ATTEMPT
    );
    for tag in [RESERVED, ACTIVE, YOUNG] {
        assert_eq!(attempts_of(tag), 1, "{tag:02x}");
        assert_eq!(receipted(fixture, tag, 1, 1), PER_ATTEMPT, "{tag:02x}");
    }
    assert_eq!(attempts_of(FOREIGN), 2);
    assert_eq!(receipted(fixture, FOREIGN, 1, 2), 2 * PER_ATTEMPT);
    assert_eq!(recipients(), held);
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.submission_expiry_receipts WHERE submission_id IN {settled}")
        ),
        1
    );
    assert_eq!(phases::inspect(request, trust, deadline(), &body), Ok(None));
    assert_eq!(
        phases::publish(request, trust, deadline(), &body),
        Err(phases::Error::Missing)
    );

    // Each publication drains another batch until no seed remains.
    retire(&outcome);
    let mut publications = 0;
    while attempts_of(SEED) > 0 {
        let outcome = publish().unwrap();
        retire(&outcome);
        publications += 1;
    }
    assert_eq!(
        publications,
        (SEEDS - batch + RECLAIM_ATTEMPTS - 1) / RECLAIM_ATTEMPTS
    );
    for tag in [RESERVED, ACTIVE, YOUNG] {
        assert_eq!(attempts_of(tag), 1, "{tag:02x}");
        assert_eq!(receipted(fixture, tag, 1, 1), PER_ATTEMPT, "{tag:02x}");
    }
    assert_eq!(attempts_of(FOREIGN), 2);
    assert_eq!(receipted(fixture, FOREIGN, 1, 2), 2 * PER_ATTEMPT);
    assert_eq!(recipients(), held);

    // Fill the health ledger with unprotected receipts so the next dispatch
    // sweeps under pressure. The final reservation still owns its receipt.
    let filler = series(HEALTH_FILLER);
    sql(
        fixture,
        &format!(
            "INSERT INTO td_mta.route_health_receipts
             SELECT account,{filler},request_digest,route_name,destination,action,
                    old_revision,new_revision,failure_count,reason,failed_at,next_at,repair_generation
             FROM td_mta.route_health_receipts
             CROSS JOIN generate_series(1,1024-(SELECT count(*) FROM td_mta.route_health_receipts WHERE account={account})) n
             WHERE account={account} AND operation={}",
            id(RESERVED, 1)
        ),
    );
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.route_health_receipts WHERE account={account}")
        ),
        1024
    );
    let outcome = publish().unwrap();
    assert_eq!(receipted(fixture, RESERVED, 1, 1), PER_ATTEMPT);
    assert_eq!(
        count(
            fixture,
            &format!(
                "td_mta.route_health_receipts WHERE operation={}",
                id(HEALTH_FILLER, 1)
            )
        ),
        0
    );
    retire(&outcome);
    owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();

    // Later fixtures see none of these rows.
    let tagged =
        |column: &str| format!("substring({column} FROM 1 FOR 7)=decode('{PREFIX}','hex')");
    let mut cleanup = String::new();
    for (table, column) in RECEIPTS {
        cleanup.push_str(&format!(
            "DELETE FROM td_mta.{table} WHERE {}; ",
            tagged(column)
        ));
    }
    let published = format!(
        "(SELECT attempt_id FROM td_mta.submission_attempts WHERE submission_id={})",
        id(QUEUED, 0)
    );
    for (table, column) in RECEIPTS {
        cleanup.push_str(&format!(
            "DELETE FROM td_mta.{table} WHERE {column} IN {published}; "
        ));
    }
    cleanup.push_str(&format!("DELETE FROM td_mta.submission_expiry_receipts WHERE {}; DELETE FROM td_mta.attempt_final_reservations WHERE {}; DELETE FROM td_mta.submission_attempts WHERE {} OR submission_id={}; DELETE FROM td_mta.recipients WHERE {}; DELETE FROM td_mta.submission_routes WHERE {}; DELETE FROM td_mta.submissions WHERE {}; DELETE FROM td_mta.accounts WHERE id={other}", tagged("submission_id"), tagged("attempt_id"), tagged("attempt_id"), id(QUEUED, 0), tagged("submission_id"), tagged("submission_id"), tagged("id")));
    sql(fixture, &cleanup);
    assert_eq!(
        count(
            fixture,
            &format!(
                "td_mta.submission_attempts WHERE account={account} AND NOT active AND {ours}"
            )
        ),
        0
    );
}
