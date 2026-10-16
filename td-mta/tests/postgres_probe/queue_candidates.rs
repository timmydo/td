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
