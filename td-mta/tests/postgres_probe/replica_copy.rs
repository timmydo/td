use super::{create_runtime_login, deadline, grant_runtime_mail, sql, Fixture};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    outbound_http::Client,
    postgres_replica_source::{self, Source},
    postgres_schema,
    replica_copy::{Error, Local, Remote, Worker},
    replica_seed::{Limits, Seed},
    replica_source::{Binding, ReadLimits, Reader},
};

pub(super) fn exercise(
    client: Client<'_>,
    authorization: &str,
    limits: ReadLimits,
    binding: Binding,
) {
    let remote = |limits| Remote {
        client: Client {
            deadline: Instant::now() + Duration::from_secs(60),
            ..client
        },
        authorization,
        limits,
    };
    let source = || Source::new(client.origin, binding.epoch).unwrap();
    let mut reader =
        Reader::connect(remote(limits).client, authorization, Some(binding), limits).unwrap();
    let inventory = reader.inventory(None).unwrap();
    let page = reader.metadata(&inventory.ids).unwrap();
    let metadata = page.view().unwrap();
    let mail = metadata.emails.first().unwrap();
    let folders = reader.mailboxes().unwrap();
    drop(reader);
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let epoch = postgres_schema::initialize(&owner, &trust, deadline())
        .unwrap()
        .store_epoch;
    postgres_replica_source::initialize(&owner, &trust, deadline(), epoch, &source()).unwrap();
    let seed = Arc::new(
        Seed::prepare(
            source(),
            inventory,
            folders,
            Limits {
                body_bytes: 1024 * 1024,
                blobs: 100,
                emails: 100,
                receipts: 100,
            },
        )
        .unwrap(),
    );
    td_mta::postgres_replica_accounts::initialize(&owner, &trust, deadline(), epoch, seed).unwrap();
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    sql(&fixture, "GRANT INSERT,UPDATE,DELETE ON td_mta.replica_progress,td_mta.replica_pending,td_mta.replica_mailbox_receipts,td_mta.replica_email_receipts,td_mta.replica_blob_identities TO mail_runtime");
    let request = td_mta::postgres_probe::Request {
        user: "mail_runtime",
        mode: td_mta::postgres_probe::Mode::Runtime,
        ..fixture.request()
    };
    let local = || Local {
        request: &request,
        trust: &trust,
    };
    let mut worker = Worker::new(source(), binding.account, epoch, local()).unwrap();
    assert!(matches!(
        Worker::new(source(), binding.account, binding.epoch, local()),
        Err(Error::Invalid)
    ));
    let mut invalid = remote(limits);
    invalid.client.deadline = Instant::now() + Duration::from_secs(120);
    assert_eq!(worker.step(invalid), Err(Error::Invalid));
    let mut invalid = remote(limits);
    invalid.client.origin = "https://wrong.invalid";
    assert_eq!(worker.step(invalid), Err(Error::Invalid));
    let mut small = limits;
    small.email.body_bytes = 0;
    assert!(worker.step(remote(small)).is_err());
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    assert_eq!(
        sql(
            &fixture,
            "SELECT get_byte(done,0) FROM td_mta.replica_pending"
        ),
        "0"
    );
    sql(&fixture, "CREATE FUNCTION public.reject_copy() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END $$; CREATE TRIGGER reject_copy BEFORE UPDATE ON td_mta.replica_pending FOR EACH ROW EXECUTE FUNCTION public.reject_copy()");
    assert_eq!(
        worker.step(remote(limits)),
        Err(Error::Storage(
            td_mta::postgres_replica_apply::Error::Database
        ))
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blobs"), "0");
    assert_eq!(
        sql(
            &fixture,
            "SELECT get_byte(done,0) FROM td_mta.replica_pending"
        ),
        "0"
    );
    sql(
        &fixture,
        "DROP TRIGGER reject_copy ON td_mta.replica_pending",
    );
    let copied = worker.step(remote(limits)).unwrap();
    assert_eq!(copied.applied, 1);
    assert_eq!(copied.changed, 1);
    assert_eq!(copied.downloaded_bytes, mail.size);
    assert!(!copied.advanced);
    assert!(copied.empty_changes.is_none());
    assert_eq!(sql(&fixture, "SELECT encode(id,'hex'),encode(blob_id,'hex'),encode(thread_id,'hex') FROM td_mta.emails"), format!("{}|{}|{}", mail.id, mail.blob, mail.thread));
    let digest = mail
        .digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(sql(&fixture, "SELECT encode(sha256(string_agg(bytes,''::bytea ORDER BY ordinal)),'hex') FROM td_mta.blob_chunks"), digest);
    // A different local configuration requires a new worker and requalification.
    let rejected_request = td_mta::postgres_probe::Request {
        mode: td_mta::postgres_probe::Mode::Preparation,
        ..request
    };
    let mut rejected = Worker::new(
        source(),
        binding.account,
        epoch,
        Local {
            request: &rejected_request,
            trust: &trust,
        },
    )
    .unwrap();
    assert!(rejected.step(remote(limits)).is_err());
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "1");
    drop(worker);
    let mut worker = Worker::new(source(), binding.account, epoch, local()).unwrap();
    for _ in 0..2 {
        let caught_up = worker.step(remote(limits)).unwrap();
        assert_eq!(caught_up.applied, 0);
        assert_eq!(caught_up.downloaded_bytes, 0);
        assert!(caught_up.advanced);
        assert!(caught_up.empty_changes.is_some());
        assert_eq!(caught_up.local_sequence, copied.local_sequence);
    }
    worker.retire();
    let progress = td_mta::postgres_replica_progress::inspect(
        &owner,
        &trust,
        deadline(),
        epoch,
        binding.account,
    )
    .unwrap();
    assert!(progress.inventory_complete);
    assert_eq!(progress.email_count, 1);
    assert!(progress.pending.unwrap().complete());
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "1");
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blobs"), "1");
}
