use super::{deadline, sql, Fixture};
use std::time::Instant;
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts, postgres_schema,
    postgres_usage_audit::{self as audit, Error, Usage},
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_usage_audit_compares_one_account_without_repair() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let inspect = || audit::inspect(&fixture.request(), &trust, deadline(), epoch, account);
    assert_eq!(inspect(), Err(Error::Missing));
    for account in [account, AccountId::from_bytes([2; 16])] {
        postgres_accounts::publish(
            &fixture.request(),
            &trust,
            deadline(),
            postgres_accounts::prepare(epoch, account).unwrap(),
        )
        .unwrap();
    }
    let empty = inspect().unwrap();
    assert_eq!(
        empty.usage,
        Usage {
            body_bytes: 0,
            blob_count: 0,
            upload_bytes: 0,
            queue_bytes: 0,
            queue_submissions: 0
        }
    );
    assert_eq!(empty.identity.account, account);
    assert_eq!(empty.identity.epoch, epoch);
    assert_eq!(empty.identity.committed_sequence.number(), 1);
    assert_eq!(empty.identity.history_floor.number(), 0);
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account
        ),
        Err(Error::Epoch)
    );
    assert_eq!(
        audit::inspect(&fixture.request(), &trust, Instant::now(), epoch, account),
        Err(Error::Connection(td_mta::postgres_probe::Error::Deadline))
    );
    sql(
        &fixture,
        "DELETE FROM td_mta.account_usage WHERE account=decode(repeat('01',16),'hex')",
    );
    assert_eq!(inspect(), Err(Error::Integrity));
    sql(
        &fixture,
        "INSERT INTO td_mta.account_usage VALUES(decode(repeat('01',16),'hex'),0,0,0,0,0)",
    );
    assert_eq!(inspect().unwrap(), empty);
    // Shape-valid fixture rows exercise aggregates, not complete mail semantics.
    sql(&fixture,"INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex')),(decode(repeat('01',16),'hex'),decode(repeat('04',16),'hex')),(decode(repeat('02',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),3,decode('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad','hex'),0,decode('0000000000000001','hex'),1),(decode(repeat('01',16),'hex'),decode(repeat('04',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0),(decode(repeat('02',16),'hex'),decode(repeat('03',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0); INSERT INTO td_mta.blob_chunks VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode('616263','hex')); UPDATE td_mta.account_usage SET blob_count=1 WHERE account=decode(repeat('02',16),'hex')");
    assert_eq!(inspect(), Err(Error::Integrity));
    sql(&fixture,"INSERT INTO td_mta.leases VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),decode(repeat('08',16),'hex'),-1,1,decode('0000000000000001','hex')); INSERT INTO td_mta.submissions(account,id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,recipient_count,completed_at,notification,notification_email_id,changed) SELECT decode(repeat('01',16),'hex'),decode(repeat(n,16),'hex'),decode(repeat('07',16),'hex'),decode(repeat('08',16),'hex'),decode(repeat('09',16),'hex'),decode(repeat('03',16),'hex'),'',0,1,1,1,0,NULL,decode('0000000000000001','hex') FROM unnest(ARRAY['05','06']) n; UPDATE td_mta.account_usage SET body_bytes=3,blob_count=2,upload_bytes=3,queue_bytes=3,queue_submissions=2 WHERE account=decode(repeat('01',16),'hex')");
    let expected = Usage {
        body_bytes: 3,
        blob_count: 2,
        upload_bytes: 3,
        queue_bytes: 3,
        queue_submissions: 2,
    };
    let report = inspect().unwrap();
    assert_eq!(report.identity, empty.identity);
    assert_eq!(report.usage, expected);
    for (column, delta) in [
        ("body_bytes", 1),
        ("blob_count", 1),
        ("upload_bytes", -1),
        ("queue_bytes", -1),
        ("queue_submissions", 1),
    ] {
        sql(&fixture,&format!("UPDATE td_mta.account_usage SET {column}={column}+({delta}) WHERE account=decode(repeat('01',16),'hex')"));
        assert_eq!(inspect(), Err(Error::Integrity), "{column}");
        sql(&fixture,&format!("UPDATE td_mta.account_usage SET {column}={column}-({delta}) WHERE account=decode(repeat('01',16),'hex')"));
        assert_eq!(inspect().unwrap(), report);
    }
    // Counter consistency cannot certify immutable body contents.
    sql(&fixture,"UPDATE td_mta.blob_chunks SET bytes=decode('78797a','hex') WHERE account=decode(repeat('01',16),'hex')");
    assert_eq!(inspect().unwrap(), report);
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts ORDER BY id"
        ),
        "0000000000000001\n0000000000000001"
    );
}
