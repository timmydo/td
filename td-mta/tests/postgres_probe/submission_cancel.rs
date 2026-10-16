use super::{deadline, sql, Fixture};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::{BootId, StoreEpoch, SubmissionId},
    postgres_attempt_phases as phases, postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
    postgres_submission_cancel as cancel,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    source: SubmissionId,
) {
    let submission = SubmissionId::from_bytes([91; 16]);
    let sub = format!("decode('{submission}','hex')");
    let src = format!("decode('{source}','hex')");
    sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,1000,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},n,'cancel@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,999) n WHERE id={sub}"));
    let claim = owners::prepare(binding, BootId::from_bytes([91; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let prepare = || cancel::prepare(&claim, grant.generation, submission).unwrap();
    let reset = || {
        sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,rcpt_reply=NULL,data_reply=NULL,reason=0,diagnostic='' WHERE submission_id={sub}; UPDATE td_mta.submissions SET completed_at=NULL,notification=0,notification_email_id=NULL WHERE id={sub}"));
    };
    for assignment in [
        "state=2,attempt_id=decode(repeat('01',16),'hex'),attempt_count=1,last_attempt_at=0,phase=3,next_attempt_at=NULL,rcpt_reply='250 ok'",
        "state=2,uncertain=1,attempt_id=decode(repeat('01',16),'hex'),attempt_count=1,last_attempt_at=0,phase=1,next_attempt_at=NULL",
        "state=4,attempt_id=decode(repeat('01',16),'hex'),attempt_count=1,last_attempt_at=0,phase=4,next_attempt_at=NULL,rcpt_reply='250 ok',data_reply='250 accepted'",
    ] {
        sql(fixture,&format!("UPDATE td_mta.recipients SET {assignment} WHERE submission_id={sub} AND ordinal=0"));
        assert_eq!(cancel::publish(request,trust,deadline(),&prepare(), |_| panic!("ineligible group fenced")),Err(cancel::Error::CannotUnsend));
        reset();
    }
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=6,reason=7,next_attempt_at=NULL WHERE submission_id={sub} AND ordinal=0"));
    assert_eq!(
        cancel::publish(request, trust, deadline(), &prepare(), |_| panic!(
            "partial cancellation fenced"
        )),
        Err(cancel::Error::Integrity)
    );
    reset();
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=3,attempt_id=decode(repeat('01',16),'hex'),attempt_count=1,last_attempt_at=0,phase=4,reason=1,rcpt_reply='450 '||repeat('x',4092) WHERE submission_id={sub}"));
    assert_eq!(
        cancel::publish(request, trust, deadline(), &prepare(), |_| panic!(
            "oversized group fenced"
        )),
        Err(cancel::Error::Capacity)
    );
    reset();
    let sequence = u64::from_str_radix(
        &sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        ),
        16,
    )
    .unwrap();
    assert!(matches!(
        cancel::prepare_if_state(
            &claim,
            grant.generation,
            submission,
            (StoreEpoch::from_bytes([0; 16]), sequence)
        ),
        Err(cancel::Error::StateMismatch)
    ));
    let stale = cancel::prepare_if_state(
        &claim,
        grant.generation,
        submission,
        (binding.epoch(), sequence - 1),
    )
    .unwrap();
    assert_eq!(
        cancel::publish(request, trust, deadline(), &stale, |_| panic!(
            "stale state fenced"
        )),
        Err(cancel::Error::StateMismatch)
    );
    assert_eq!(
        cancel::inspect(request, trust, deadline(), &stale).unwrap(),
        None
    );
    let value = cancel::prepare_if_state(
        &claim,
        grant.generation,
        submission,
        (binding.epoch(), sequence),
    )
    .unwrap();
    assert_eq!(
        cancel::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    assert_eq!(
        cancel::publish(request, trust, deadline(), &value, |_| Err(
            cancel::Error::Fence
        )),
        Err(cancel::Error::Fence)
    );
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=1"
            )
        ),
        "1000"
    );
    sql(fixture,"CREATE FUNCTION public.reject_cancel() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_cancel BEFORE INSERT ON td_mta.submission_cancel_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_cancel()");
    let closed = Arc::new(AtomicBool::new(false));
    let observed = closed.clone();
    assert_eq!(
        cancel::publish(request, trust, deadline(), &value, move |_| {
            observed.store(true, Ordering::Relaxed);
            Ok(())
        }),
        Err(cancel::Error::Database)
    );
    assert!(closed.load(Ordering::Relaxed));
    assert_eq!(
        cancel::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=1"
            )
        ),
        "1000"
    );
    sql(fixture,"DROP TRIGGER reject_cancel ON td_mta.submission_cancel_receipts; CREATE FUNCTION public.delay_cancel() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_cancel AFTER INSERT ON td_mta.submission_cancel_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_cancel()");
    assert_eq!(
        cancel::publish(
            request,
            trust,
            Instant::now() + Duration::from_secs(2),
            &value,
            |_| Ok(())
        ),
        Err(cancel::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = cancel::inspect(request, trust, deadline(), &value)
        .unwrap()
        .unwrap();
    assert_eq!((outcome.recipients, outcome.changed), (1000, 1000));
    assert_eq!(outcome.old_sequence + 1, outcome.new_sequence);
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=6 AND phase=0 AND reason=7 AND next_attempt_at IS NULL")),"1000");
    assert_eq!(
        cancel::publish(request, trust, deadline(), &value, |_| panic!(
            "replay fenced"
        )),
        Ok(outcome)
    );
    sql(
        fixture,
        "DROP TRIGGER delay_cancel ON td_mta.submission_cancel_receipts",
    );
    let stale_noop = cancel::prepare_if_state(
        &claim,
        grant.generation,
        submission,
        (binding.epoch(), sequence),
    )
    .unwrap();
    assert_eq!(
        cancel::publish(request, trust, deadline(), &stale_noop, |_| panic!(
            "stale no-op fenced"
        )),
        Err(cancel::Error::StateMismatch)
    );
    let conditional_noop = cancel::prepare_if_state(
        &claim,
        grant.generation,
        submission,
        (binding.epoch(), outcome.new_sequence),
    )
    .unwrap();
    let unchanged = cancel::publish(request, trust, deadline(), &conditional_noop, |_| {
        panic!("conditional no-op fenced")
    })
    .unwrap();
    assert_eq!(
        (
            unchanged.changed,
            unchanged.old_sequence,
            unchanged.new_sequence
        ),
        (0, outcome.new_sequence, outcome.new_sequence)
    );
    let receipt = format!("decode('{}','hex')", value.operation());
    sql(fixture, &format!("UPDATE td_mta.submission_cancel_receipts SET old_sequence=decode('{:016x}','hex'),new_sequence=decode('{:016x}','hex') WHERE operation={receipt}", sequence+1, sequence+2));
    assert_eq!(
        cancel::inspect(request, trust, deadline(), &value),
        Err(cancel::Error::Integrity)
    );
    sql(fixture, &format!("UPDATE td_mta.submission_cancel_receipts SET old_sequence=decode('{:016x}','hex'),new_sequence=decode('{:016x}','hex') WHERE operation={receipt}", sequence, sequence+1));
    let noop = cancel::publish(request, trust, deadline(), &prepare(), |_| {
        panic!("no-op fenced")
    })
    .unwrap();
    assert_eq!(
        (noop.changed, noop.old_sequence, noop.new_sequence),
        (0, outcome.new_sequence, outcome.new_sequence)
    );
    assert_eq!(noop.completed_at, outcome.completed_at);
    // Completed failures retain their signed observation, notice and replies.
    reset();
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=5,reason=2,attempt_id=decode(repeat('01',16),'hex'),attempt_count=1,last_attempt_at=0,phase=4,next_attempt_at=NULL,rcpt_reply='550 refused' WHERE submission_id={sub}; UPDATE td_mta.submissions SET completed_at=-1,notification=2,notification_email_id=email_id WHERE id={sub}"));
    let failed = cancel::publish(request, trust, deadline(), &prepare(), |_| Ok(())).unwrap();
    assert_eq!(failed.completed_at, -1);
    assert_eq!(sql(fixture,&format!("SELECT completed_at,notification,notification_email_id=email_id FROM td_mta.submissions WHERE id={sub}")),"-1|2|t");
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND rcpt_reply='550 refused' AND attempt_count=1")),"1000");
    reset();
    owners::renew(request, trust, deadline(), &claim, grant.generation).unwrap();
    let attempt = attempts::publish(
        request,
        trust,
        deadline(),
        &attempts::prepare(&claim, grant.generation, submission).unwrap(),
    )
    .unwrap();
    let body = phases::body(&claim, grant.generation, attempt.attempt, &[(0, "250 ok")]).unwrap();
    phases::publish(request, trust, deadline(), &body).unwrap();
    let late =
        phases::acceptance_possible(&claim, grant.generation, attempt.attempt, &[0]).unwrap();
    cancel::publish(request, trust, deadline(), &prepare(), |_| Ok(())).unwrap();
    assert!(phases::publish(request, trust, deadline(), &late).is_err());
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.submission_attempts WHERE submission_id={sub} AND active")),"0");
    reset();
    owners::renew(request, trust, deadline(), &claim, grant.generation).unwrap();
    exercise_device_authorization(
        fixture,
        request,
        trust,
        binding,
        &claim,
        grant.generation,
        submission,
    );
    reset();
    owners::renew(request, trust, deadline(), &claim, grant.generation).unwrap();
    super::cancel_worker::exercise(
        fixture,
        request,
        trust,
        binding,
        &claim,
        grant.generation,
        submission,
    );
    owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();
    assert_eq!(
        cancel::publish(request, trust, deadline(), &value, |_| panic!(
            "released replay fenced"
        )),
        Ok(outcome)
    );
    assert!(
        cancel::publish(request, trust, deadline(), &prepare(), |_| panic!(
            "stale owner fenced"
        ))
        .is_err()
    );
    // Remove only the extra fixture object; retained receipts are independent.
    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    assert_eq!(
        cancel::inspect(request, trust, deadline(), &value).unwrap(),
        Some(outcome)
    );
}

fn exercise_device_authorization(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    claim: &owners::Claim,
    generation: u64,
    submission: SubmissionId,
) {
    use td_mta::{
        generations::GenerationSet, postgres_devices as devices,
        postgres_runtime_profile as runtime, tls_policy::TlsPolicies,
    };
    let domain = GenerationSet::<TlsPolicies>::at_startup();
    let jmap = runtime::load(
        request,
        trust,
        deadline(),
        binding.epoch(),
        "desktop",
        domain.reserve().unwrap(),
    )
    .unwrap();
    let device =
        devices::prepare_creation(binding.epoch(), binding.account(), "cancel-client", 0).unwrap();
    devices::publish_creation(request, trust, deadline(), &device).unwrap();
    let sequence = u64::from_str_radix(
        &sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        ),
        16,
    )
    .unwrap();
    let input = || cancel::ClientRequest {
        binding: jmap.binding(),
        device: device.device_id(),
        expected: Some((binding.epoch(), sequence)),
    };
    let value = cancel::prepare_for_device(claim, generation, submission, input()).unwrap();
    let later = cancel::prepare_for_device(claim, generation, submission, input()).unwrap();
    assert_eq!(
        cancel::publish(request, trust, deadline(), &value, |_| panic!(
            "client preparation bypassed credential"
        )),
        Err(cancel::Error::Unauthenticated)
    );
    let internal = cancel::prepare(claim, generation, submission).unwrap();
    assert_eq!(
        cancel::publish_authenticated(
            request,
            trust,
            deadline(),
            &internal,
            device.password(),
            |_| panic!("unbound request authenticated")
        ),
        Err(cancel::Error::Policy)
    );
    assert_eq!(
        cancel::publish_authenticated(request, trust, deadline(), &value, b"bad", |_| panic!(
            "malformed credential fenced"
        )),
        Err(cancel::Error::Unauthenticated)
    );
    let mut incorrect = device.password().to_vec();
    let byte = incorrect.last_mut().unwrap();
    *byte = if *byte == b'0' { b'1' } else { b'0' };
    assert_eq!(
        cancel::publish_authenticated(request, trust, deadline(), &value, &incorrect, |_| panic!(
            "incorrect credential fenced"
        )),
        Err(cancel::Error::Unauthenticated)
    );
    let foreign =
        devices::prepare_creation(binding.epoch(), binding.account(), "cancel-other", 0).unwrap();
    assert_eq!(
        cancel::publish_authenticated(
            request,
            trust,
            deadline(),
            &value,
            foreign.password(),
            |_| panic!("different device fenced")
        ),
        Err(cancel::Error::Unauthenticated)
    );
    let closed = Arc::new(AtomicBool::new(false));
    let observed = closed.clone();
    let outcome = cancel::publish_authenticated(
        request,
        trust,
        deadline(),
        &value,
        device.password(),
        move |_| {
            observed.store(true, Ordering::Relaxed);
            Ok(())
        },
    )
    .unwrap();
    assert!(closed.load(Ordering::Relaxed));
    assert_eq!(outcome.changed, 1000);
    let revocation =
        devices::prepare_revocation(binding.epoch(), binding.account(), device.device_id(), 1)
            .unwrap();
    devices::publish_revocation(request, trust, deadline(), &revocation).unwrap();
    assert_eq!(
        cancel::publish_authenticated(
            request,
            trust,
            deadline(),
            &later,
            device.password(),
            |_| panic!("revoked credential fenced")
        ),
        Err(cancel::Error::Unauthenticated)
    );
    assert_eq!(
        cancel::inspect(request, trust, deadline(), &later).unwrap(),
        None
    );
    assert_eq!(
        cancel::publish_authenticated(request, trust, deadline(), &value, b"bad", |_| panic!(
            "historical client replay fenced"
        )),
        Ok(outcome)
    );
    assert_eq!(
        cancel::publish(request, trust, deadline(), &value, |_| panic!(
            "historical settlement fenced"
        )),
        Ok(outcome)
    );
    sql(
        fixture,
        &format!(
            "DELETE FROM td_mta.devices WHERE account=decode('{}','hex') AND id=decode('{}','hex')",
            binding.account(),
            device.device_id()
        ),
    );
}
