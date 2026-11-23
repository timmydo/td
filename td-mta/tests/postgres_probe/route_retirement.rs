//! An acquisition retires route rows no relay is using (POSTGRES.md, route
//! owners): owner rows idle for a week, then health rows no owner row holds,
//! keeping repair latches, unelapsed backoff, and rows an active attempt or
//! uncompleted submission names.
use super::submission_retention::{count, id, submission, Group, NOW};
use super::{deadline, sql, Fixture};
use td_mta::{
    ids::{BootId, SubmissionId},
    postgres_probe::Request,
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
};

fn fill(byte: &str, bytes: usize) -> String {
    format!("decode(repeat('{byte}',{bytes}),'hex')")
}

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    binding: &Binding,
    source: SubmissionId,
) {
    let src = format!("decode('{source}','hex')");
    let account = format!("decode('{}','hex')", binding.account());
    let current = binding.route().name();
    let destination = |byte: &str| fill(byte, 32);
    let (boot, operation) = (fill("bb", 16), fill("cc", 16));
    let one = "decode('0000000000000001','hex')";
    let blob = format!("(SELECT transmitted_blob_id FROM td_mta.submissions WHERE id={src})");
    let idle = format!("{NOW}-{}-1", owners::IDLE_OWNER_MS);
    let ours = format!("account={account} AND route_name LIKE 'old-%'");
    let hex_destination = binding
        .route()
        .destination()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let current_health = format!(
        "td_mta.route_health WHERE account={account} AND route_name='{current}' AND destination=decode('{hex_destination}','hex')"
    );
    // The current route's own health row must survive; seed one if no earlier
    // fixture left it.
    let seeded = count(fixture, &current_health) == "0";
    if seeded {
        sql(fixture, &format!("INSERT INTO td_mta.route_health(account,route_name,destination,revision,failure_count,reason) SELECT account,route_name,destination,revision,0,0 FROM (SELECT {account} AS account,'{current}'::text AS route_name,decode('{}','hex') AS destination,decode('0000000000000001','hex') AS revision) row", hex_destination));
    }

    // Owners of other route names, all at destination aa: released or expired
    // a week ago, released just now (a sibling relay that stopped), leased,
    // and idle but named by an active attempt.
    for (name, owner, clock, expires) in [
        ("old-free", "NULL,NULL", idle.as_str(), "NULL"),
        (
            "old-expired",
            &*format!("{boot},{operation}"),
            &*idle,
            &*format!("{idle}+1"),
        ),
        ("old-recent", "NULL,NULL", NOW, "NULL"),
        (
            "old-live",
            &*format!("{boot},{}", fill("dd", 16)),
            &*idle,
            "9000000000000000000",
        ),
        ("old-busy", "NULL,NULL", &*idle, "NULL"),
    ] {
        sql(fixture, &format!("INSERT INTO td_mta.route_owners(account,route_name,destination,generation,owner_boot,operation,last_clock,expires_at) VALUES({account},'{name}',{},{one},{owner},{clock},{expires})", destination("aa")));
    }
    sql(fixture, &format!("INSERT INTO td_mta.submission_attempts(account,attempt_id,request_digest,submission_id,route_name,destination,owner_boot,owner_operation,generation,prepared_at,ordinals,old_sequence,new_sequence,active) VALUES({account},{},{},{src},'old-busy',{},{boot},{operation},{one},0,decode('00000000','hex'),decode('0000000000000001','hex'),decode('0000000000000002','hex'),true)", fill("a1", 16), fill("00", 32), destination("ad")));
    submission(
        fixture,
        &src,
        Group {
            id: 0xd7,
            blob: &blob,
            recipients: 1,
            recipient: "1,0,NULL,0,NULL,0,0,NULL,NULL,0",
            completed: "NULL",
            notification: 0,
        },
    );
    let queued = id(0xd7);
    sql(fixture, &format!("UPDATE td_mta.submission_routes SET route_name='old-queued',destination={} WHERE submission_id={queued}", destination("ac")));
    // Health rows: (failure_count,reason,failed_at,next_at,repair_generation).
    let neutral = "0,0,NULL,NULL,NULL";
    let latched = format!("1,4,0,300000,{one}");
    let backoff = format!("1,3,{NOW},{NOW}+600000,NULL");
    for (name, byte, state) in [
        (current, "aa", neutral),
        ("old-free", "ab", neutral),
        ("old-queued", "ac", neutral),
        ("old-busy", "ad", neutral),
        ("old-recent", "aa", neutral),
        ("old-recent", "ae", neutral),
        ("old-latched", "af", latched.as_str()),
        ("old-backoff", "b0", backoff.as_str()),
    ] {
        sql(fixture, &format!("INSERT INTO td_mta.route_health(account,route_name,destination,revision,failure_count,reason,failed_at,next_at,repair_generation) VALUES({account},'{name}',{},{one},{state})", destination(byte)));
    }
    for (name, byte, operation) in [
        ("old-free", "ab", "e1"),
        ("old-queued", "ac", "e2"),
        (current, "aa", "e3"),
    ] {
        sql(fixture, &format!("INSERT INTO td_mta.route_health_receipts(account,operation,request_digest,route_name,destination,action,old_revision,new_revision,failure_count,reason) VALUES({account},{},{},'{name}',{},2,decode('0000000000000001','hex'),decode('0000000000000002','hex'),0,0)", fill(operation, 16), fill("00", 32), destination(byte)));
    }

    let names = |table: &str, filter: &str| {
        sql(fixture, &format!("SELECT string_agg(route_name||':'||encode(substr(destination,1,1),'hex'),',' ORDER BY route_name,destination) FROM td_mta.{table} WHERE {filter}"))
    };
    let current_old = format!(
        "account={account} AND route_name='{current}' AND destination={}",
        destination("aa")
    );
    let acquire = |boot: u8| {
        let claim = owners::prepare(binding, BootId::from_bytes([boot; 16])).unwrap();
        let grant = owners::acquire(request, deadline(), &claim).unwrap();
        owners::release(request, deadline(), &claim, grant.generation).unwrap();
    };
    acquire(95);
    assert_eq!(
        names("route_owners", &ours),
        "old-busy:aa,old-live:aa,old-recent:aa"
    );
    // The recently used sibling keeps its current destination's row, not its
    // old one; the current route name keeps only this claim's destination.
    assert_eq!(
        names("route_health", &ours),
        "old-backoff:b0,old-busy:ad,old-latched:af,old-queued:ac,old-recent:aa"
    );
    assert_eq!(
        count(fixture, &format!("td_mta.route_health WHERE {current_old}")),
        "0"
    );
    assert_eq!(count(fixture, &current_health), "1");
    assert_eq!(
        names(
            "route_health_receipts",
            &format!(
                "account={account} AND action=2 AND (route_name LIKE 'old-%' OR ({current_old}))"
            )
        ),
        "old-queued:ac"
    );

    // Once nothing names them, the next acquisition retires the attempt's
    // idle owner and both freed health rows; latch and backoff stay.
    sql(fixture, &format!("DELETE FROM td_mta.submission_attempts WHERE attempt_id={}; DELETE FROM td_mta.recipients WHERE submission_id={queued}; DELETE FROM td_mta.submission_routes WHERE submission_id={queued}; DELETE FROM td_mta.submissions WHERE id={queued}", fill("a1", 16)));
    acquire(96);
    assert_eq!(names("route_owners", &ours), "old-live:aa,old-recent:aa");
    assert_eq!(
        names("route_health", &ours),
        "old-backoff:b0,old-latched:af,old-recent:aa"
    );
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.route_health_receipts WHERE {ours} AND action=2")
        ),
        "0"
    );
    sql(fixture, &format!("DELETE FROM td_mta.route_owners WHERE {ours}; DELETE FROM td_mta.route_health WHERE {ours}"));
    if seeded {
        sql(fixture, &format!("DELETE FROM {current_health}"));
    }
}
