use super::{deadline, sql, Fixture};
use std::time::Instant;
use td_crypto::{Crypto, Digest, Provider, TrustStore};
use td_mta::{
    ids::SubmissionId,
    postgres_attempts::{self as attempts, Dispatch, ReadError},
    postgres_body,
    postgres_probe::Request,
    postgres_route_owners::Claim,
    postgres_submission_cancel as cancel,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    dispatch: &Dispatch,
) {
    let refuse = |_, _: &mut dyn td_mta::ports::BlobReader| -> Result<(), td_mta::ports::Error> {
        panic!("refused body reached callback")
    };
    let stale = attempts::prepare(claim, generation + 1, submission).unwrap();
    assert!(matches!(
        attempts::with_body(
            request,
            trust,
            deadline(),
            &stale,
            postgres_body::MAX_BODY_BYTES,
            refuse
        ),
        Err(ReadError::Attempt(attempts::Error::Owner(
            td_mta::postgres_route_owners::Error::Stale
        )))
    ));
    let uncommitted = attempts::prepare(claim, generation, submission).unwrap();
    assert!(matches!(
        attempts::with_body(
            request,
            trust,
            deadline(),
            &uncommitted,
            postgres_body::MAX_BODY_BYTES,
            refuse
        ),
        Err(ReadError::Attempt(attempts::Error::Missing))
    ));
    assert!(matches!(
        attempts::with_body(
            request,
            trust,
            Instant::now(),
            dispatch,
            postgres_body::MAX_BODY_BYTES,
            refuse
        ),
        Err(ReadError::Attempt(attempts::Error::Connection(_)))
    ));
    assert!(matches!(
        attempts::with_body(request, trust, deadline(), dispatch, 0, refuse),
        Err(ReadError::Body(postgres_body::Error::Capacity))
    ));
    assert!(matches!(
        attempts::with_body(
            request,
            trust,
            deadline(),
            dispatch,
            postgres_body::MAX_BODY_BYTES + 1,
            refuse
        ),
        Err(ReadError::Body(postgres_body::Error::Capacity))
    ));
    let blob=sql(fixture,&format!("SELECT encode(transmitted_blob_id,'hex') FROM td_mta.submissions WHERE id=decode('{submission}','hex')"));
    let original = sql(
        fixture,
        &format!("SELECT encode(digest,'hex') FROM td_mta.blobs WHERE id=decode('{blob}','hex')"),
    );
    sql(fixture,&format!("UPDATE td_mta.blobs SET digest=decode(repeat('ff',32),'hex') WHERE id=decode('{blob}','hex')"));
    assert!(matches!(
        attempts::with_body(
            request,
            trust,
            deadline(),
            dispatch,
            postgres_body::MAX_BODY_BYTES,
            refuse
        ),
        Err(ReadError::Body(postgres_body::Error::Read(
            td_mta::ports::Error::Corrupt
        )))
    ));
    sql(fixture,&format!("UPDATE td_mta.blobs SET digest=decode('{original}','hex') WHERE id=decode('{blob}','hex')"));
    let until = deadline();
    attempts::with_body(
        request,
        trust,
        until,
        dispatch,
        postgres_body::MAX_BODY_BYTES,
        |metadata, body| {
            assert_eq!(metadata.attempt, dispatch.attempt());
            assert_eq!(metadata.submission, submission);
            assert_eq!(body.len(), metadata.blob.body.length);
            let mut hash = Provider.sha256().unwrap();
            let mut buffer = [0; 4096];
            let mut position = 0;
            while position < body.len() {
                let count = body.read_at(position, &mut buffer)?;
                assert!(count > 0 && count <= buffer.len());
                hash.update(&buffer[..count]).unwrap();
                position += count as u64;
            }
            assert_eq!(hash.finish().unwrap(), metadata.blob.body.digest);
            assert_eq!(body.read_at(body.len(), &mut buffer)?, 0);
            let mut first = [0; 17];
            let count = body.read_at(0, &mut first)?;
            let operation = cancel::prepare(claim, generation, submission).unwrap();
            let result = cancel::publish(request, trust, until, &operation, |_| Ok(())).unwrap();
            assert_eq!(result.changed, 1);
            // Independent publication proves the callback holds no account lock.
            // The immutable admitted snapshot remains readable after cancellation.
            let mut again = [0; 17];
            assert_eq!(body.read_at(0, &mut again)?, count);
            assert_eq!(again, first);
            Ok(())
        },
    )
    .unwrap();
    assert!(matches!(
        attempts::with_body(
            request,
            trust,
            deadline(),
            dispatch,
            postgres_body::MAX_BODY_BYTES,
            refuse
        ),
        Err(ReadError::Attempt(attempts::Error::Missing))
    ));
}
