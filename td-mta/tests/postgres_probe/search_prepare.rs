//! Native missing-evidence selection, bounded continuation and transaction faults.
use super::*;
use td_crypto::Digest;
use td_mta::{
    ids::{AccountId, BlobId},
    postgres_schema,
    postgres_search_prepare::{self as preparation, Key, Options},
};

fn key(account: u8, blob: u8) -> Key {
    Key {
        account: AccountId::from_bytes([account; 16]),
        blob: BlobId::from_bytes([blob; 16]),
    }
}
fn hash(bytes: &[u8]) -> String {
    let mut hash = td_crypto::BulkSha256::try_new().unwrap();
    hash.update(bytes).unwrap();
    hash.finish()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn seed(fixture: &Fixture, id: Key, bytes: &[u8], linked: bool) {
    let account = id.account;
    let blob = id.blob;
    let body: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let digest = hash(bytes);
    sql(fixture,&format!("INSERT INTO td_mta.accounts VALUES(decode('{account}','hex'),decode('0000000000000001','hex'),decode('0000000000000001','hex')) ON CONFLICT DO NOTHING; INSERT INTO td_mta.blob_ids VALUES(decode('{account}','hex'),decode('{blob}','hex')); INSERT INTO td_mta.blobs VALUES(decode('{account}','hex'),decode('{blob}','hex'),{},decode('{digest}','hex'),1704067200000,decode('0000000000000001','hex'),1); INSERT INTO td_mta.blob_chunks VALUES(decode('{account}','hex'),decode('{blob}','hex'),0,decode('{body}','hex'));",bytes.len()));
    if linked {
        link(fixture, id);
    }
}
fn link(fixture: &Fixture, id: Key) {
    let account = id.account;
    let blob = id.blob;
    sql(fixture,&format!("INSERT INTO td_mta.threads VALUES(decode('{account}','hex'),decode('{blob}','hex'),decode('0000000000000001','hex')); INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES(decode('{account}','hex'),decode('{blob}','hex'),decode('{blob}','hex'),decode('{blob}','hex'),1704067200000,2,decode('0000000000000001','hex'));"));
}
fn predicate(id: Key) -> String {
    format!(
        "account=decode('{}','hex') AND blob_id=decode('{}','hex')",
        id.account, id.blob
    )
}
fn evidence(fixture: &Fixture, id: Key, text: &str) {
    sql(fixture,&format!("INSERT INTO td_mta.blob_search(account,blob_id,version,body_length,body_digest,fields,fields_digest,candidate_text) SELECT account,id,1,length,digest,''::bytea,decode(repeat('00',32),'hex'),{text} FROM td_mta.blobs WHERE account=decode('{}','hex') AND id=decode('{}','hex')",id.account,id.blob));
}
fn large(fixture: &Fixture, id: Key) {
    let chunk = [b'x'; 65536];
    let mut digest = td_crypto::BulkSha256::try_new().unwrap();
    for _ in 0..768 {
        digest.update(&chunk).unwrap();
    }
    let digest: String = digest
        .finish()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let account = id.account;
    let blob = id.blob;
    sql(fixture,&format!("INSERT INTO td_mta.accounts VALUES(decode('{account}','hex'),decode('0000000000000001','hex'),decode('0000000000000001','hex')) ON CONFLICT DO NOTHING; INSERT INTO td_mta.blob_ids VALUES(decode('{account}','hex'),decode('{blob}','hex')); INSERT INTO td_mta.blobs VALUES(decode('{account}','hex'),decode('{blob}','hex'),50331648,decode('{digest}','hex'),1704067200000,decode('0000000000000001','hex'),768); INSERT INTO td_mta.blob_chunks SELECT decode('{account}','hex'),decode('{blob}','hex'),n,convert_to(repeat('x',65536),'UTF8') FROM generate_series(0,767) n;"));
    link(fixture, id);
}

pub(super) fn exercise() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::initialize(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let good = b"Subject: prepared needle\r\n\r\nbody\r\n";
    let bad = b"Content-Type: message/global\r\n\r\nbody";
    for id in [
        key(1, 20),
        key(1, 30),
        key(1, 40),
        key(2, 10),
        key(2, 20),
        key(2, 30),
        key(2, 40),
        key(2, 50),
    ] {
        seed(&fixture, id, good, true);
    }
    seed(&fixture, key(1, 10), bad, true);
    seed(&fixture, key(3, 99), good, false);
    evidence(&fixture, key(1, 30), "''");
    evidence(&fixture, key(1, 40), "NULL");
    for id in [key(2, 20), key(2, 30), key(2, 40)] {
        evidence(&fixture, id, "''");
    }
    sql(&fixture,&format!("UPDATE td_mta.blob_search SET version=2 WHERE {}; UPDATE td_mta.blob_search SET body_length=body_length+1 WHERE {}; UPDATE td_mta.blob_search SET body_digest=decode(repeat('00',32),'hex') WHERE {};",predicate(key(2,20)),predicate(key(2,30)),predicate(key(2,40))));
    let run = |options| {
        preparation::run(
            &request,
            &trust,
            Instant::now() + Duration::from_secs(30),
            epoch,
            options,
        )
    };
    let first = run(Options {
        after: None,
        through: None,
        maximum: 1,
    })
    .unwrap();
    assert_eq!((first.scanned, first.written, first.skipped), (1, 0, 1));
    assert_eq!(first.after, Some(key(1, 10)));
    assert_eq!(first.through, Some(key(3, 99)));
    assert!(!first.done);
    seed(&fixture, key(1, 5), good, true);
    seed(&fixture, key(4, 10), good, true);
    let second = run(Options {
        after: first.after,
        through: first.through,
        maximum: 16,
    })
    .unwrap();
    assert_eq!((second.scanned, second.written, second.skipped), (7, 7, 0));
    assert!(second.done);
    assert_eq!(second.through, first.through);
    assert_eq!(
        sql(
            &fixture,
            &format!(
                "SELECT count(*) FROM td_mta.blob_search WHERE {} OR {} OR {}",
                predicate(key(1, 5)),
                predicate(key(4, 10)),
                predicate(key(3, 99))
            )
        ),
        "0"
    );
    // Matching identity and non-NULL text qualify even damaged cached fields.
    assert_eq!(
        sql(
            &fixture,
            &format!(
                "SELECT octet_length(fields),candidate_text='' FROM td_mta.blob_search WHERE {}",
                predicate(key(1, 30))
            )
        ),
        "0|t"
    );
    let next = run(Options {
        after: None,
        through: None,
        maximum: 16,
    })
    .unwrap();
    assert_eq!((next.scanned, next.written, next.skipped), (3, 2, 1));
    assert_eq!(next.through, Some(key(4, 10)));
    assert!(next.done);
    // Corruption after an earlier successful row must roll the whole page back.
    sql(&fixture,&format!("UPDATE td_mta.blob_search SET candidate_text=NULL WHERE {}; DELETE FROM td_mta.blob_search WHERE {}; UPDATE td_mta.blob_chunks SET bytes=set_byte(bytes,0,get_byte(bytes,0)#1) WHERE {};",predicate(key(1,20)),predicate(key(2,10)),predicate(key(2,10))));
    assert!(matches!(
        run(Options {
            after: None,
            through: None,
            maximum: 16
        }),
        Err(preparation::Error::Body(_))
    ));
    assert_eq!(
        sql(
            &fixture,
            &format!(
                "SELECT candidate_text IS NULL FROM td_mta.blob_search WHERE {}",
                predicate(key(1, 20))
            )
        ),
        "t"
    );
    sql(
        &fixture,
        &format!(
            "UPDATE td_mta.blob_chunks SET bytes=set_byte(bytes,0,get_byte(bytes,0)#1) WHERE {}",
            predicate(key(2, 10))
        ),
    );
    run(Options {
        after: None,
        through: None,
        maximum: 16,
    })
    .unwrap();
    evidence(&fixture, key(1, 10), "''");
    let empty = run(Options {
        after: None,
        through: None,
        maximum: 16,
    })
    .unwrap();
    assert_eq!((empty.scanned, empty.verified_bytes), (0, 0));
    assert!(empty.done);
    assert!(matches!(
        run(Options {
            after: Some(key(1, 1)),
            through: None,
            maximum: 16
        }),
        Err(preparation::Error::Invalid)
    ));
    let mut runtime = fixture.request();
    runtime.mode = td_mta::postgres_probe::Mode::Runtime;
    assert!(matches!(
        preparation::run(
            &runtime,
            &trust,
            deadline(),
            epoch,
            Options {
                after: None,
                through: None,
                maximum: 16
            }
        ),
        Err(preparation::Error::Owner)
    ));
    sql(
        &fixture,
        "CREATE ROLE search_outsider LOGIN PASSWORD 'disposable-postgres-fixture-password'",
    );
    let outsider = Request {
        user: "search_outsider",
        ..fixture.request()
    };
    assert!(matches!(
        preparation::run(
            &outsider,
            &trust,
            deadline(),
            epoch,
            Options {
                after: None,
                through: None,
                maximum: 16
            }
        ),
        Err(preparation::Error::Owner)
    ));

    large(&fixture, key(5, 10));
    large(&fixture, key(5, 20));
    let bounded = run(Options {
        after: Some(key(4, 255)),
        through: Some(key(5, 20)),
        maximum: 16,
    })
    .unwrap();
    assert_eq!(
        (bounded.scanned, bounded.skipped, bounded.verified_bytes),
        (1, 1, 48 * 1024 * 1024)
    );
    assert_eq!(bounded.after, Some(key(5, 10)));
    assert!(!bounded.done);
    let remaining = run(Options {
        after: bounded.after,
        through: bounded.through,
        maximum: 16,
    })
    .unwrap();
    assert_eq!((remaining.scanned, remaining.skipped), (1, 1));
    assert!(remaining.done);

    seed(&fixture, key(6, 10), good, true);
    // End our own backend during COMMIT, without a timing threshold or sleep.
    sql(&fixture,"CREATE FUNCTION public.end_search_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_terminate_backend(pg_catalog.pg_backend_pid()); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER end_search_commit AFTER INSERT OR UPDATE ON td_mta.blob_search DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.end_search_commit()");
    assert!(matches!(
        run(Options {
            after: Some(key(5, 255)),
            through: Some(key(6, 10)),
            maximum: 16
        }),
        Err(preparation::Error::Indeterminate)
    ));
    sql(&fixture,"DROP TRIGGER end_search_commit ON td_mta.blob_search; DROP FUNCTION public.end_search_commit()");
}
