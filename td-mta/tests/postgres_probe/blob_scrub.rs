use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, BlobId, StoreEpoch},
    postgres_accounts, postgres_blob_scrub as scrub, postgres_schema,
};
#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
pub(super) fn real_postgres_blob_scrub_pages_keep_corruption_visible_and_advance() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let options = scrub::Options {
        after: None,
        through: None,
        maximum: 1,
    };
    // Large-body correctness fixtures use the body worker's full allowance;
    // they assert accounting and integrity, never host throughput.
    let inspect = |options| {
        scrub::inspect(
            &fixture.request(),
            &trust,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            epoch,
            options,
        )
    };
    let mut runtime = fixture.request();
    runtime.mode = td_mta::postgres_probe::Mode::Runtime;
    assert_eq!(
        scrub::inspect(&runtime, &trust, deadline(), epoch, options),
        Err(scrub::Error::Owner)
    );
    let empty = inspect(options).unwrap();
    assert!(empty.done);
    assert_eq!((empty.scanned, empty.through), (0, None));
    for n in [1u8, 2] {
        let account = AccountId::from_bytes([n; 16]);
        postgres_accounts::publish(
            &fixture.request(),
            &trust,
            deadline(),
            postgres_accounts::prepare(epoch, account).unwrap(),
        )
        .unwrap();
        sql(&fixture,&format!("INSERT INTO td_mta.blob_ids VALUES(decode(repeat('{n:02x}',16),'hex'),decode(repeat('03',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('{n:02x}',16),'hex'),decode(repeat('03',16),'hex'),3,decode('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad','hex'),0,decode('0000000000000001','hex'),1); INSERT INTO td_mta.blob_chunks VALUES(decode(repeat('{n:02x}',16),'hex'),decode(repeat('03',16),'hex'),0,decode('616263','hex'))"));
    }
    let first = inspect(options).unwrap();
    assert_eq!(
        (
            first.scanned,
            first.failed,
            first.verified_bytes,
            first.done
        ),
        (1, 0, 3, false)
    );
    let last = first.through.unwrap();
    assert_eq!(last.account, AccountId::from_bytes([2; 16]));
    let next = scrub::Options {
        after: first.after,
        through: first.through,
        maximum: 1,
    };
    let second = inspect(next).unwrap();
    assert_eq!(
        (
            second.scanned,
            second.failed,
            second.verified_bytes,
            second.done
        ),
        (1, 0, 3, true)
    );
    assert_eq!(second.after, Some(last));
    assert_eq!(
        inspect(scrub::Options {
            after: second.after,
            ..next
        })
        .unwrap()
        .scanned,
        0
    );
    // Corruption is reported with a publishable cursor, not mistaken for a
    // transport failure that would repeatedly starve later bodies.
    sql(&fixture,"UPDATE td_mta.blob_chunks SET bytes=decode('78797a','hex') WHERE account=decode(repeat('01',16),'hex')");
    let broken = inspect(options).unwrap();
    assert_eq!(
        (broken.scanned, broken.failed, broken.verified_bytes),
        (1, 1, 0)
    );
    assert_eq!(broken.first_failure, broken.after);
    assert_eq!(inspect(next).unwrap(), second);
    sql(
        &fixture,
        "DELETE FROM td_mta.blob_chunks WHERE account=decode(repeat('01',16),'hex')",
    );
    assert_eq!(inspect(options).unwrap().failed, 1);
    assert!(matches!(
        scrub::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            options
        ),
        Err(scrub::Error::Invalid)
    ));
    assert_eq!(
        inspect(scrub::Options {
            maximum: 257,
            ..options
        }),
        Err(scrub::Error::Invalid)
    );
    assert_eq!(
        inspect(scrub::Options {
            after: Some(last),
            ..options
        }),
        Err(scrub::Error::Invalid)
    );
    // An explicit high water excludes later larger IDs, even with spare room.
    sql(&fixture,"INSERT INTO td_mta.blob_ids VALUES(decode(repeat('02',16),'hex'),decode(repeat('04',16),'hex')); INSERT INTO td_mta.blobs VALUES(decode(repeat('02',16),'hex'),decode(repeat('04',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0)");
    assert_eq!(
        inspect(scrub::Options {
            maximum: 256,
            ..next
        })
        .unwrap(),
        second
    );
    let all = inspect(scrub::Options {
        maximum: 256,
        ..options
    })
    .unwrap();
    assert_eq!(
        (all.scanned, all.failed, all.verified_bytes, all.done),
        (3, 1, 3, true)
    );
    assert_eq!(all.through.unwrap().blob, BlobId::from_bytes([4; 16]));
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blobs"), "3");
    use std::{fs, os::unix::fs::PermissionsExt};
    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("scrub-ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("scrub-connection");
    fs::write(
        &connection,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(super::PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let state = fixture.root.join("scrub-state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let invoke = |command: &str, exit| {
        let epoch = epoch.to_string();
        let mut args = vec![
            "postgres",
            command,
            "--epoch",
            &epoch,
            "--state-dir",
            state.to_str().unwrap(),
        ];
        if command == "blob-scrub" {
            args.extend([
                "--connection-file",
                connection.to_str().unwrap(),
                "--limit",
                "1",
            ]);
        }
        // A held operational lock deliberately consumes its 15-second wait.
        let output = super::command_output_with_timeout(
            &args,
            fixture.root.path(),
            exit,
            std::time::Duration::from_secs(45),
        );
        assert!(!output.contains(std::str::from_utf8(super::PASSWORD).unwrap()));
        td_json::parse(&output).unwrap()
    };
    assert_eq!(
        invoke("blob-scrub-status", 1)
            .get("status")
            .and_then(td_json::Json::as_str),
        Some("attention")
    );

    let lock = td_mta::store_fs::PrivateRoot::open(state.to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    assert_eq!(
        invoke("blob-scrub", 1)
            .get("error")
            .and_then(td_json::Json::as_str),
        Some("scrub-state-locked")
    );
    drop(lock);
    assert_eq!(
        fs::metadata(state.join("state.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    fs::write(state.join("partial"), "interrupted").unwrap();
    fs::set_permissions(state.join("partial"), fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        invoke("blob-scrub", 1)
            .get("phase")
            .and_then(td_json::Json::as_str),
        Some("progress")
    );
    assert_eq!(
        invoke("blob-scrub", 1)
            .get("phase")
            .and_then(td_json::Json::as_str),
        Some("progress")
    );
    assert_eq!(
        invoke("blob-scrub", 1)
            .get("phase")
            .and_then(td_json::Json::as_str),
        Some("complete")
    );
    assert_eq!(
        invoke("blob-scrub", 1)
            .get("phase")
            .and_then(td_json::Json::as_str),
        Some("waiting")
    );
    assert_eq!(
        invoke("blob-scrub-status", 1)
            .get("status")
            .and_then(td_json::Json::as_str),
        Some("attention")
    );

    assert!(!state.join("partial").exists());
    std::os::unix::fs::symlink("state.json", state.join("partial")).unwrap();
    assert_eq!(
        invoke("blob-scrub", 1)
            .get("error")
            .and_then(td_json::Json::as_str),
        Some("scrub-state-file")
    );
    fs::remove_file(state.join("partial")).unwrap();
    // Failed hashes still consume the page's attempted-byte budget.
    sql(&fixture,"INSERT INTO td_mta.blob_ids SELECT decode(repeat('02',16),'hex'),decode(repeat(x,16),'hex') FROM unnest(ARRAY['05','06']) x; INSERT INTO td_mta.blobs SELECT decode(repeat('02',16),'hex'),decode(repeat(x,16),'hex'),41943040,decode(repeat('00',32),'hex'),0,decode('0000000000000001','hex'),640 FROM unnest(ARRAY['05','06']) x; INSERT INTO td_mta.blob_chunks SELECT decode(repeat('02',16),'hex'),decode(repeat(x,16),'hex'),n,decode(repeat('00',65536),'hex') FROM unnest(ARRAY['05','06']) x CROSS JOIN generate_series(0,639) n");
    let bounded = inspect(scrub::Options {
        maximum: 256,
        ..options
    })
    .unwrap();
    assert_eq!(
        (
            bounded.scanned,
            bounded.failed,
            bounded.verified_bytes,
            bounded.done
        ),
        (4, 2, 3, false)
    );
    let final_page = inspect(scrub::Options {
        maximum: 256,
        after: bounded.after,
        through: bounded.through,
    })
    .unwrap();
    assert_eq!(
        (
            final_page.scanned,
            final_page.failed,
            final_page.verified_bytes,
            final_page.done
        ),
        (1, 1, 0, true)
    );
}
