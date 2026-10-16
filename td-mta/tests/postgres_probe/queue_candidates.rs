use super::{deadline, sql, Fixture};
use std::collections::BTreeSet;
use td_crypto::TrustStore;
use td_mta::{
    ids::SubmissionId,
    postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_queue_candidates::{self as candidates, Kind},
    postgres_route_owners::{self as owners, Claim},
    postgres_submission_cancel as cancel,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
) {
    // A disjoint ID interval keeps cleanup independent of the shared scenario.
    let id = |n: u8| {
        let mut bytes = [92; 16];
        bytes[15] = n;
        SubmissionId::from_bytes(bytes)
    };
    for n in 0..70 {
        let submission = id(n);
        sql(fixture, &format!("INSERT INTO td_mta.submissions SELECT account,decode('{submission}','hex'),email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,0,expires_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id=decode('{source}','hex'); INSERT INTO td_mta.submission_routes SELECT account,decode('{submission}','hex'),submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id=decode('{source}','hex'); INSERT INTO td_mta.recipients SELECT account,id,0,'candidate@example.test',1,0,NULL,0,NULL,0,0,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id=decode('{submission}','hex')"));
    }
    sql(fixture, &format!("UPDATE td_mta.submissions SET send_at=expires_at-1 WHERE id=decode('{}','hex'); UPDATE td_mta.submissions SET expires_at=1 WHERE id=decode('{}','hex'); UPDATE td_mta.submission_routes SET destination=decode(repeat('fe',32),'hex') WHERE submission_id=decode('{}','hex'); UPDATE td_mta.recipients SET state=3,attempt_id=decode(repeat('5d',16),'hex'),attempt_count=1,last_attempt_at=0,phase=4,next_attempt_at=300000,reason=3 WHERE submission_id=decode('{}','hex')", id(66),id(67),id(68),id(69)));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let first = candidates::scan(
        request,
        trust,
        deadline(),
        claim,
        generation,
        Kind::Fresh,
        None,
    )
    .unwrap();
    assert_eq!(first.candidates.len(), candidates::PAGE_SIZE);
    let cursor = first.next.unwrap();
    assert!(matches!(
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Retry,
            Some(cursor)
        ),
        Err(candidates::Error::Cursor)
    ));
    assert!(matches!(
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation + 1,
            Kind::Fresh,
            None
        ),
        Err(candidates::Error::Owner(owners::Error::Stale))
    ));
    let mut seen = BTreeSet::new();
    let mut previous = None;
    let mut page = first;
    for turn in 0..32 {
        assert!(page.candidates.len() <= candidates::PAGE_SIZE);
        for candidate in &page.candidates {
            assert!(candidate.due_at <= page.observed_at);
            let key = (candidate.due_at, candidate.submission);
            assert!(previous.is_none_or(|last| last < key));
            previous = Some(key);
            assert!(seen.insert(candidate.submission));
        }
        let Some(next) = page.next else {
            break;
        };
        assert!(turn < 31, "candidate pagination did not terminate");
        page = candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Fresh,
            Some(next),
        )
        .unwrap();
    }
    for n in 0..66 {
        assert!(seen.contains(&id(n)));
    }
    for n in 66..70 {
        assert!(!seen.contains(&id(n)));
    }
    let retry = candidates::scan(
        request,
        trust,
        deadline(),
        claim,
        generation,
        Kind::Retry,
        None,
    )
    .unwrap();
    assert!(retry
        .candidates
        .iter()
        .any(|candidate| candidate.submission == id(69)));
    assert!(retry
        .candidates
        .iter()
        .all(|candidate| !(0..66).any(|n| candidate.submission == id(n))));
    let expiry = || {
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Expiry,
            None,
        )
        .unwrap()
    };
    assert!(matches!(
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Expiry,
            Some(cursor)
        ),
        Err(candidates::Error::Cursor)
    ));
    let expired = id(67);
    let contains = |page: &candidates::Page, submission| {
        page.candidates.iter().any(|c| c.submission == submission)
    };
    let page = expiry();
    assert!(contains(&page, expired));
    assert_eq!(
        page.candidates
            .iter()
            .find(|c| c.submission == expired)
            .unwrap()
            .due_at,
        1
    );
    assert!(!contains(&page, id(66)));
    assert!(!contains(&page, id(69)));
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.submissions SET expires_at=1 WHERE id=decode('{}','hex')",
            id(68)
        ),
    );
    assert!(!contains(&expiry(), id(68)));
    for (assignment,eligible) in [
        ("state=1,next_attempt_at=9223372036854775807",true),
        ("state=2,attempt_id=decode(repeat('5d',16),'hex'),attempt_count=1,last_attempt_at=0,phase=1,next_attempt_at=NULL",true),
        ("state=3,phase=4,reason=3",true),
        ("state=7,uncertain=1,reason=6,next_attempt_at=NULL",false),
        ("next_attempt_at=300000",true),
        ("state=4,uncertain=0,rcpt_reply='250 ok',data_reply='250 ok',next_attempt_at=NULL",false),
        ("state=6,reason=7",false),
    ] {
        sql(fixture,&format!("UPDATE td_mta.recipients SET {assignment} WHERE submission_id=decode('{expired}','hex')"));
        assert_eq!(contains(&expiry(),expired),eligible,"{assignment}");
    }
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,rcpt_reply=NULL,data_reply=NULL,reason=0 WHERE submission_id=decode('{expired}','hex'); UPDATE td_mta.submissions SET completed_at=1 WHERE id=decode('{expired}','hex')"));
    assert!(!contains(&expiry(), expired));
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.submissions SET completed_at=NULL WHERE id=decode('{expired}','hex')"
        ),
    );
    // This private database has no concurrent fixture writer. Preserve all
    // prior route health while injecting independently observable pauses.
    sql(
        fixture,
        "CREATE TABLE public.candidate_health_backup AS TABLE td_mta.route_health",
    );
    sql(fixture,&format!("INSERT INTO td_mta.route_health SELECT account,route_name,destination,decode('0000000000000001','hex'),1,3,(extract(epoch FROM clock_timestamp())*1000)::bigint,(extract(epoch FROM clock_timestamp())*1000)::bigint+300000,NULL FROM td_mta.submission_routes WHERE submission_id=decode('{source}','hex') ON CONFLICT(account,route_name,destination) DO UPDATE SET failure_count=1,reason=3,failed_at=EXCLUDED.failed_at,next_at=EXCLUDED.next_at,repair_generation=NULL"));
    assert!(matches!(
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Fresh,
            None
        ),
        Err(candidates::Error::RouteHealth(
            td_mta::postgres_route_health::Error::Backoff
        ))
    ));
    assert!(contains(&expiry(), expired));
    sql(fixture,"UPDATE td_mta.route_health SET reason=5,repair_generation=decode('0000000000000001','hex') WHERE failure_count>0");
    assert!(matches!(
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Fresh,
            None
        ),
        Err(candidates::Error::RouteHealth(
            td_mta::postgres_route_health::Error::RepairRequired
        ))
    ));
    assert!(contains(&expiry(), expired));
    sql(fixture,"DELETE FROM td_mta.route_health; INSERT INTO td_mta.route_health SELECT * FROM public.candidate_health_backup; DROP TABLE public.candidate_health_backup");
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "0"
    );
    let dispatch = attempts::prepare(claim, generation, id(0)).unwrap();
    attempts::publish(request, trust, deadline(), &dispatch).unwrap();
    assert!(matches!(
        candidates::scan(
            request,
            trust,
            deadline(),
            claim,
            generation,
            Kind::Fresh,
            None
        ),
        Err(candidates::Error::Busy)
    ));
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.submissions SET expires_at=1 WHERE id=decode('{}','hex')",
            id(0)
        ),
    );
    let page = expiry();
    assert!(contains(&page, id(0)));
    assert!(contains(&page, expired));
    let cancellation = cancel::prepare(claim, generation, id(0)).unwrap();
    cancel::publish(request, trust, deadline(), &cancellation, |_| Ok(())).unwrap();
    let page = candidates::scan(
        request,
        trust,
        deadline(),
        claim,
        generation,
        Kind::Fresh,
        None,
    )
    .unwrap();
    assert!(page
        .candidates
        .iter()
        .all(|candidate| candidate.submission != id(0)));
    for n in 0..70 {
        let submission = id(n);
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex'); DELETE FROM td_mta.submissions WHERE id=decode('{submission}','hex')"));
    }
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
}
