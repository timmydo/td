//! A failure notice always stores (POSTGRES.md section 213): in full while
//! recent and within quota, otherwise compact, past the quota if needed, and
//! one that keeps failing never blocks the others.
use super::submission_retention::{count, id, resync, submission, Group, NOW};
use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::{BootId, SubmissionId},
    postgres_failure_notice as notice,
    postgres_probe::Request,
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
};

/// Three Failed recipients whose replies alone outgrow a compact notice.
const LONG: &str = "5,0,NULL,0,NULL,0,NULL,repeat('r',4000),repeat('d',4000),2";
const COMPACT: i64 = 16 * 1024;

fn notice_length(fixture: &Fixture, sub: u8) -> i64 {
    sql(fixture, &format!("SELECT b.length FROM td_mta.submissions s JOIN td_mta.emails e ON e.account=s.account AND e.id=s.notification_email_id JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id WHERE s.id={} AND s.notification=2", id(sub))).parse().unwrap()
}

/// The stored notice's decoded text body (a single base64 text part).
fn notice_text(fixture: &Fixture, sub: u8) -> String {
    // A full report is about 24 KiB; sql() refuses output over 4 KiB.
    super::sql_bounded(fixture, &format!("SELECT convert_from(decode(split_part(convert_from(string_agg(c.bytes,''::bytea ORDER BY c.ordinal),'UTF8'),E'\\r\\n\\r\\n',2),'base64'),'UTF8') FROM td_mta.submissions s JOIN td_mta.emails e ON e.account=s.account AND e.id=s.notification_email_id JOIN td_mta.blob_chunks c ON c.account=e.account AND c.blob_id=e.blob_id WHERE s.id={} AND s.notification=2", id(sub)), 65536)
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
    let blob = format!("(SELECT transmitted_blob_id FROM td_mta.submissions WHERE id={src})");
    let recent = NOW.to_string();
    let old = format!("{NOW}-90000000");
    let group = |sub: u8, completed: &str| {
        submission(
            fixture,
            &src,
            Group {
                id: sub,
                blob: blob.as_str(),
                recipients: 3,
                recipient: LONG,
                completed,
                notification: 1,
            },
        )
    };
    let usage = |column: &str| -> i64 {
        sql(
            fixture,
            &format!("SELECT {column} FROM td_mta.account_usage WHERE account={account}"),
        )
        .parse()
        .unwrap()
    };
    let pending = || count(fixture, "td_mta.submissions WHERE notification=1");
    let claim = owners::prepare(binding, BootId::from_bytes([94; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let publish = || notice::publish_next(request, trust, deadline(), &claim, grant.generation);
    // Retention's fixture leaves one notice pending (0xe7); store it first.
    for _ in 0..8 {
        if publish() != Ok(true) {
            break;
        }
    }
    assert_eq!(pending(), "0");
    resync(fixture, &account);

    // Recent and within quota: the full report, every reply included.
    group(0xd1, &recent);
    resync(fixture, &account);
    assert_eq!(publish(), Ok(true));
    assert!(notice_length(fixture, 0xd1) > COMPACT);
    assert!(notice_text(fixture, 0xd1).contains(&"r".repeat(4000)));

    // At the body cap the full report is refused for quota; the compact one
    // is stored anyway, past the cap by exactly its own size.
    group(0xd2, &recent);
    resync(fixture, &account);
    let cap = binding.quotas().body_bytes as i64;
    sql(
        fixture,
        &format!("UPDATE td_mta.account_usage SET body_bytes={cap} WHERE account={account}"),
    );
    let blobs = usage("blob_count");
    assert_eq!(publish(), Ok(true));
    let length = notice_length(fixture, 0xd2);
    assert!(length <= COMPACT, "{length}");
    assert_eq!(usage("body_bytes"), cap + length);
    assert_eq!(usage("blob_count"), blobs + 1);
    resync(fixture, &account);

    // The overshoot is bounded: past one compact message per configured
    // queued submission, the notice waits until the owner makes room.
    group(0xd6, &old);
    resync(fixture, &account);
    let margin = binding.quotas().queue_submissions as i64 * COMPACT;
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.account_usage SET body_bytes={} WHERE account={account}",
            cap + margin
        ),
    );
    assert_eq!(publish(), Err(notice::Error::Quota));
    assert_eq!(pending(), "1");
    resync(fixture, &account);
    let blob_cap = binding.quotas().blob_count as i64;
    let queued = binding.quotas().queue_submissions as i64;
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.account_usage SET blob_count={} WHERE account={account}",
            blob_cap + queued
        ),
    );
    assert_eq!(publish(), Err(notice::Error::Quota));
    assert_eq!(pending(), "1");
    resync(fixture, &account);
    assert_eq!(publish(), Ok(true));
    assert!(notice_length(fixture, 0xd6) <= COMPACT);

    // Completed over a day ago: compact even with quota to spare.
    group(0xd3, &old);
    assert_eq!(publish(), Ok(true));
    assert!(notice_length(fixture, 0xd3) <= COMPACT);
    let text = notice_text(fixture, 0xd3);
    assert!(
        text.lines().any(|line| line.trim_end() == "Failed: 3"),
        "{text}"
    );
    assert_eq!(text.matches("Failed: retain@example.test").count(), 3);
    assert_eq!(text.matches("Reason: Permanent SMTP refusal").count(), 3);
    assert!(!text.contains("rrrrrrrr") && !text.contains("dddddddd"));

    // Random order: a notice whose storage keeps failing does not hold back
    // another; once it can store, it does.
    group(0xd4, &recent);
    group(0xd5, &recent);
    let (d4, d5) = (id(0xd4), id(0xd5));
    sql(fixture, &format!("CREATE FUNCTION public.refuse_d4_notice() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'refused'; END $$; CREATE TRIGGER refuse_d4_notice BEFORE UPDATE ON td_mta.submissions FOR EACH ROW WHEN (NEW.id={d4} AND NEW.notification=2) EXECUTE FUNCTION public.refuse_d4_notice()"));
    let mut refused = 0;
    for _ in 0..64 {
        match publish() {
            Ok(true) => break,
            Err(notice::Error::Database) => refused += 1,
            other => panic!("{other:?}"),
        }
    }
    // Only d4 remains, so the next turn takes it and is refused again.
    assert_eq!(publish(), Err(notice::Error::Database));
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.submissions WHERE id={d5} AND notification=2")
        ),
        "1"
    );
    assert_eq!(
        count(
            fixture,
            &format!("td_mta.submissions WHERE id={d4} AND notification=1")
        ),
        "1",
        "after {refused} refusals"
    );
    sql(
        fixture,
        "DROP TRIGGER refuse_d4_notice ON td_mta.submissions; DROP FUNCTION public.refuse_d4_notice()",
    );
    assert_eq!(publish(), Ok(true));
    assert_eq!(pending(), "0");
    assert_eq!(publish(), Ok(false));
    owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();

    // Later fixtures see none of these submissions; their notices stay as
    // ordinary Inbox Emails.
    let ours = [0xd1, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6].map(id).join(",");
    sql(fixture, &format!("DELETE FROM td_mta.recipients WHERE submission_id IN ({ours}); DELETE FROM td_mta.submissions WHERE id IN ({ours})"));
    resync(fixture, &account);
}
