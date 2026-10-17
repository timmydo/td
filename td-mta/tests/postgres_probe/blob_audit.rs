use super::{deadline, sql, Fixture};
use std::time::Instant;
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_accounts, postgres_blob_audit as audit, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_blob_audit_verifies_snapshot_bytes_without_device_authority() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let blob = BlobId::from_bytes([3; 16]);
    let inspect = || audit::inspect(&fixture.request(), &trust, deadline(), epoch, account, blob);
    assert_eq!(inspect(), Err(audit::Error::Missing));
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    assert_eq!(inspect(), Err(audit::Error::Missing));
    sql(&fixture,"INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),3,decode('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad','hex'),0,decode('0000000000000001','hex'),1); INSERT INTO td_mta.blob_chunks VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode('616263','hex'))");
    let report = inspect().unwrap();
    assert_eq!(report.identity.account, account);
    assert_eq!(report.identity.epoch, epoch);
    assert_eq!(report.identity.committed_sequence.number(), 1);
    assert_eq!(report.identity.history_floor.number(), 0);
    assert_eq!(report.blob, blob);
    assert_eq!(report.body.length, 3);
    assert_eq!(report.body.created_at, 0);
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            AccountId::from_bytes([2; 16]),
            blob
        ),
        Err(audit::Error::Missing)
    );
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account,
            blob
        ),
        Err(audit::Error::Epoch)
    );
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now(),
            epoch,
            account,
            blob
        ),
        Err(audit::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    );
    // Body verification supplies no usage, ownership or metadata certificate.
    assert_eq!(
        sql(
            &fixture,
            "SELECT body_bytes,blob_count FROM td_mta.account_usage"
        ),
        "0|0"
    );
    for (damage, restore) in [
        (
            "UPDATE td_mta.blob_chunks SET bytes=decode('78797a','hex')",
            "UPDATE td_mta.blob_chunks SET bytes=decode('616263','hex')",
        ),
        (
            "UPDATE td_mta.blob_chunks SET bytes=decode('6162','hex')",
            "UPDATE td_mta.blob_chunks SET bytes=decode('616263','hex')",
        ),
        (
            "UPDATE td_mta.blob_chunks SET ordinal=1",
            "UPDATE td_mta.blob_chunks SET ordinal=0",
        ),
        (
            "UPDATE td_mta.blobs SET length=65537,chunk_count=2",
            "UPDATE td_mta.blobs SET length=3,chunk_count=1",
        ),
        (
            "UPDATE td_mta.blobs SET changed=decode('0000000000000002','hex')",
            "UPDATE td_mta.blobs SET changed=decode('0000000000000001','hex')",
        ),
    ] {
        sql(&fixture, damage);
        assert!(matches!(inspect(), Err(audit::Error::Body(_))), "{damage}");
        sql(&fixture, restore);
        assert_eq!(inspect().unwrap(), report);
    }
    sql(&fixture,"DELETE FROM td_mta.blob_chunks; UPDATE td_mta.blobs SET length=0,chunk_count=0,digest=decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex')");
    assert_eq!(inspect().unwrap().body.length, 0);
    sql(
        &fixture,
        "UPDATE td_mta.blobs SET digest=decode(repeat('00',32),'hex')",
    );
    assert!(matches!(inspect(), Err(audit::Error::Body(_))));
    assert_eq!(
        sql(&fixture, "SELECT encode(digest,'hex') FROM td_mta.blobs"),
        "00".repeat(32)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001"
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "0");
}
