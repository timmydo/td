use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::SubmissionId,
    postgres_attempt_phases as phases, postgres_attempts as attempts,
    postgres_probe::Request,
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
    let submission = SubmissionId::from_bytes([93; 16]);
    let sub = format!("decode('{submission}','hex')");
    let src = format!("decode('{source}','hex')");
    sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,105,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},n,n::text||'@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,104) n WHERE id={sub}"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let dispatch = attempts::prepare(claim, generation, submission).unwrap();
    assert_eq!(
        attempts::envelope(request, trust, deadline(), &dispatch),
        Err(attempts::Error::Missing)
    );
    let outcome = attempts::publish(request, trust, deadline(), &dispatch).unwrap();
    assert_eq!(outcome.ordinals.len(), 100);
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let envelope = attempts::envelope(request, trust, deadline(), &dispatch).unwrap();
    assert_eq!(envelope.attempt, dispatch.attempt());
    assert_eq!(envelope.submission, submission);
    assert_eq!(
        envelope.reverse_path,
        sql(
            fixture,
            &format!("SELECT reverse_path FROM td_mta.submissions WHERE id={src}")
        )
    );
    assert_eq!(envelope.recipients.len(), 100);
    for (n, recipient) in envelope.recipients.iter().enumerate() {
        assert_eq!(recipient.ordinal, n as u32);
        assert_eq!(recipient.address, format!("{n}@example.test"));
        assert!(!recipient.uncertain);
    }
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    let reverse=sql(fixture,&format!("SELECT encode(convert_to(reverse_path,'UTF8'),'hex') FROM td_mta.submissions WHERE id={sub}"));
    for assignment in [
        "address=E'bad\\r\\naddress'",
        "next_attempt_at=0",
        "reason=3",
    ] {
        sql(
            fixture,
            &format!(
                "UPDATE td_mta.recipients SET {assignment} WHERE submission_id={sub} AND ordinal=0"
            ),
        );
        assert_eq!(
            attempts::envelope(request, trust, deadline(), &dispatch),
            Err(attempts::Error::Integrity)
        );
        sql(fixture,&format!("UPDATE td_mta.recipients SET address='0@example.test',next_attempt_at=NULL,reason=0 WHERE submission_id={sub} AND ordinal=0"));
    }
    sql(
        fixture,
        &format!("UPDATE td_mta.submissions SET reverse_path='' WHERE id={sub}"),
    );
    assert!(attempts::envelope(request, trust, deadline(), &dispatch)
        .unwrap()
        .reverse_path
        .is_empty());
    sql(
        fixture,
        &format!("UPDATE td_mta.submissions SET reverse_path='invalid' WHERE id={sub}"),
    );
    assert_eq!(
        attempts::envelope(request, trust, deadline(), &dispatch),
        Err(attempts::Error::Integrity)
    );
    sql(fixture,&format!("UPDATE td_mta.submissions SET reverse_path=convert_from(decode('{reverse}','hex'),'UTF8') WHERE id={sub}"));
    let refusal =
        phases::refused_rcpt(claim, generation, dispatch.attempt(), &[(0, "450 later")]).unwrap();
    phases::publish(request, trust, deadline(), &refusal).unwrap();
    assert_eq!(
        attempts::envelope(request, trust, deadline(), &dispatch),
        Err(attempts::Error::Conflict)
    );
    let cancellation = cancel::prepare(claim, generation, submission).unwrap();
    cancel::publish(request, trust, deadline(), &cancellation, |_| Ok(())).unwrap();
    assert_eq!(
        attempts::envelope(request, trust, deadline(), &dispatch),
        Err(attempts::Error::Missing)
    );
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,rcpt_reply=NULL,data_reply=NULL,reason=0,diagnostic='' WHERE submission_id={sub}; UPDATE td_mta.submissions SET completed_at=NULL WHERE id={sub}"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let dispatch = attempts::prepare(claim, generation, submission).unwrap();
    attempts::publish(request, trust, deadline(), &dispatch).unwrap();
    let body = phases::body(claim, generation, dispatch.attempt(), &[(0, "250 ok")]).unwrap();
    phases::publish(request, trust, deadline(), &body).unwrap();
    assert_eq!(
        attempts::envelope(request, trust, deadline(), &dispatch),
        Err(attempts::Error::Conflict)
    );
    let cancellation = cancel::prepare(claim, generation, submission).unwrap();
    cancel::publish(request, trust, deadline(), &cancellation, |_| Ok(())).unwrap();
    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
}
