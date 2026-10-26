use super::{command_output, command_output_with_timeout, deadline, sql, Fixture, PASSWORD};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::AccountId, metadata_sweep, postgres_account_audit as audit, postgres_accounts,
    postgres_metadata_audit, postgres_probe, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_owner_audits_scale_with_explicit_finite_limits() {
    exercise();
}

pub(super) fn exercise() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let metadata = metadata_sweep::Limits {
        rows: 3000,
        parent_reads: 10,
    };
    // Many valid manifests exercise cumulative command bytes and frame counts.
    sql(&fixture, "INSERT INTO td_mta.blob_ids SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex') FROM generate_series(1,2000) n; INSERT INTO td_mta.blobs SELECT account,id,0,sha256(''::bytea),0,decode('0000000000000001','hex'),0 FROM td_mta.blob_ids; UPDATE td_mta.account_usage SET blob_count=2000");
    // Named statements use a process-wide counter; cross its next decimal
    // width before initializing another schema, whose SQL budget includes names.
    for _ in 0..6 {
        let report = postgres_metadata_audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(60),
            epoch,
            account,
            0,
            metadata,
        )
        .unwrap();
        assert_eq!(report.references().rows(), 2001);
    }
    {
        let next = Fixture::new();
        let trust = TrustStore::from_pem(&next.ca).unwrap();
        postgres_schema::initialize(&next.request(), &trust, deadline()).unwrap();
    }
    assert!(matches!(
        postgres_metadata_audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            0,
            metadata_sweep::Limits {
                rows: 1,
                ..metadata
            }
        ),
        Err(postgres_metadata_audit::Error::Metadata(_))
    ));
    assert_eq!(
        postgres_metadata_audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(3601),
            epoch,
            account,
            0,
            metadata
        ),
        Err(postgres_metadata_audit::Error::Connection(
            postgres_probe::Error::Invalid
        ))
    );
    let mut runtime = fixture.request();
    runtime.mode = postgres_probe::Mode::Runtime;
    assert_eq!(
        postgres_metadata_audit::inspect(&runtime, &trust, deadline(), epoch, account, 0, metadata),
        Err(postgres_metadata_audit::Error::Connection(
            postgres_probe::Error::Invalid
        ))
    );
    assert_eq!(
        postgres_metadata_audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            0,
            metadata_sweep::Limits {
                rows: u64::MAX,
                ..metadata
            }
        ),
        Err(postgres_metadata_audit::Error::Connection(
            postgres_probe::Error::Invalid
        ))
    );

    // Each body is legal; their total exceeds a single-body worker's 128 MiB.
    sql(&fixture, "DELETE FROM td_mta.blobs WHERE id>decode(lpad(to_hex(5),32,'0'),'hex'); UPDATE td_mta.blobs SET length=33554432,digest=sha256(convert_to(repeat('x',33554432),'UTF8')),chunk_count=512; INSERT INTO td_mta.blob_chunks SELECT account,id,n,convert_to(repeat('x',65536),'UTF8') FROM td_mta.blobs CROSS JOIN generate_series(0,511) n; UPDATE td_mta.account_usage SET blob_count=5,body_bytes=167772160");
    let limits = audit::Limits {
        metadata,
        blobs: 5,
        bytes: 167_772_160,
    };
    let inspect = |limits| {
        audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now() + Duration::from_secs(60),
            epoch,
            account,
            0,
            limits,
        )
    };
    let report = inspect(limits).unwrap();
    assert_eq!((report.blobs(), report.bytes()), (5, 167_772_160));
    assert_eq!(report.metadata().references().rows(), 6);
    assert_eq!(
        inspect(audit::Limits {
            bytes: limits.bytes - 1,
            ..limits
        }),
        Err(audit::Error::Capacity)
    );
    assert_eq!(
        inspect(audit::Limits { blobs: 4, ..limits }),
        Err(audit::Error::Capacity)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT body_bytes,blob_count FROM td_mta.account_usage"
        ),
        "167772160|5"
    );

    fs::set_permissions(fixture.root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let ca = fixture.root.join("ca.pem");
    fs::write(&ca, &fixture.ca).unwrap();
    let connection = fixture.root.join("audit.connection");
    fs::write(
        &connection,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    let base = [
        "postgres",
        "account-audit",
        "--connection-file",
        connection.to_str().unwrap(),
        "--epoch",
        &epoch.to_string(),
        "--account",
        &account.to_string(),
        "--max-rows",
        "3000",
        "--max-parent-reads",
        "10",
        "--max-blobs",
        "5",
        "--max-bytes",
        "167772160",
        "--timeout-seconds",
        "60",
    ];
    let output =
        command_output_with_timeout(&base, fixture.root.path(), 0, Duration::from_secs(65));
    assert!(output.contains("\"verified_bytes\":167772160"));
    assert!(!output.contains(std::str::from_utf8(PASSWORD).unwrap()));
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now(),
            epoch,
            account,
            0,
            limits,
        ),
        Err(audit::Error::Connection(postgres_probe::Error::Deadline))
    );
    let mut bad = base;
    bad[17] = "3601";
    assert!(command_output(&bad, fixture.root.path(), 2).contains("invalid-arguments"));
}
