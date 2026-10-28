use super::{create_runtime_login, deadline, grant_runtime_mail, sql, Fixture};
use std::sync::Arc;
use td_crypto::TrustStore;
use td_mta::{
    ids::EmailId,
    postgres_replica_apply::{Error, Publisher},
    postgres_replica_source::{self, Source},
    postgres_schema,
    replica_effect::Effect,
    replica_pending::Ticket,
    replica_seed::{Limits, Seed},
    replica_source::{Binding, Bounds, Inventory, MailboxPage, MetadataPage, VerifiedBody},
};

pub(super) fn exercise(
    source: Source,
    inventory: Inventory,
    folders: MailboxPage,
    page: &MetadataPage,
    body: VerifiedBody,
    mut body_again: impl FnMut() -> VerifiedBody,
) {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let local = postgres_schema::initialize(&owner, &trust, deadline())
        .unwrap()
        .store_epoch;
    postgres_replica_source::initialize(&owner, &trust, deadline(), local, &source).unwrap();
    let metadata = page.view().unwrap();
    let email = metadata.emails.first().unwrap();
    let id = email.id;
    let account = metadata.observed.account;
    let seed = Arc::new(
        Seed::prepare(
            Source::new(source.origin(), source.epoch()).unwrap(),
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
    td_mta::postgres_replica_accounts::initialize(&owner, &trust, deadline(), local, seed).unwrap();
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    sql(&fixture,"GRANT INSERT,UPDATE,DELETE ON td_mta.replica_progress,td_mta.replica_pending,td_mta.replica_mailbox_receipts,td_mta.replica_email_receipts,td_mta.replica_blob_identities TO mail_runtime");
    let runtime = td_mta::postgres_probe::Request {
        user: "mail_runtime",
        mode: td_mta::postgres_probe::Mode::Runtime,
        ..fixture.request()
    };
    let open = || {
        Publisher::open(
            &runtime,
            &trust,
            deadline(),
            local,
            Source::new(source.origin(), source.epoch()).unwrap(),
            account,
        )
        .unwrap()
    };
    let ticket = Ticket {
        generation: 1,
        page: 1,
    };
    let effect = Arc::new(
        Effect::prepare(&source, account, ticket, page, id, Some(body), deadline()).unwrap(),
    );
    let mut publisher = open();
    sql(
        &fixture,
        "UPDATE td_mta.replica_accounts SET receipt_limit=2",
    );
    assert_eq!(
        publisher.publish(effect.clone(), deadline()),
        Err(Error::Quota)
    );
    drop(publisher);
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    assert_eq!(
        sql(
            &fixture,
            "SELECT get_byte(done,0) FROM td_mta.replica_pending"
        ),
        "0"
    );
    sql(&fixture,"UPDATE td_mta.replica_accounts SET receipt_limit=100; CREATE FUNCTION public.reject_replica_completion() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END $$; CREATE TRIGGER reject_replica_completion BEFORE UPDATE ON td_mta.replica_pending FOR EACH ROW EXECUTE FUNCTION public.reject_replica_completion()");
    let mut publisher = open();
    assert_eq!(
        publisher.publish(effect.clone(), deadline()),
        Err(Error::Database)
    );
    drop(publisher);
    for table in [
        "emails",
        "threads",
        "blobs",
        "blob_ids",
        "blob_chunks",
        "replica_email_receipts",
        "replica_blob_identities",
    ] {
        assert_eq!(
            sql(&fixture, &format!("SELECT count(*) FROM td_mta.{table}")),
            "0",
            "{table}"
        );
    }
    assert_eq!(
        sql(
            &fixture,
            "SELECT body_bytes,blob_count FROM td_mta.account_usage"
        ),
        "0|0"
    );
    sql(
        &fixture,
        "DROP TRIGGER reject_replica_completion ON td_mta.replica_pending",
    );
    sql(&fixture, "CREATE CONSTRAINT TRIGGER reject_replica_completion AFTER UPDATE ON td_mta.replica_pending DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.reject_replica_completion()");
    let mut publisher = open();
    assert_eq!(
        publisher.publish(effect.clone(), deadline()),
        Err(Error::Indeterminate)
    );
    drop(publisher);
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    assert_eq!(
        sql(
            &fixture,
            "SELECT get_byte(done,0) FROM td_mta.replica_pending"
        ),
        "0"
    );
    sql(
        &fixture,
        "DROP TRIGGER reject_replica_completion ON td_mta.replica_pending",
    );
    let mut publisher = open();
    let pid = sql(
        &fixture,
        "SELECT pid FROM pg_stat_activity WHERE usename='mail_runtime'",
    );
    let copied = publisher.publish(effect.clone(), deadline()).unwrap();
    assert!(copied.changed && copied.present);
    assert_eq!(copied.local_sequence, 2);
    let repeated = publisher.publish(effect.clone(), deadline()).unwrap();
    assert!(!repeated.changed && repeated.present);
    assert_eq!(repeated.local_sequence, 2);
    assert_eq!(
        sql(
            &fixture,
            "SELECT pid FROM pg_stat_activity WHERE usename='mail_runtime'"
        ),
        pid
    );
    assert_eq!(sql(&fixture,"SELECT encode(id,'hex'),encode(blob_id,'hex'),encode(thread_id,'hex'),received_at FROM td_mta.emails"),format!("{}|{}|{}|{}",id,email.blob,email.thread,email.received_at));
    assert_eq!(sql(&fixture,"SELECT encode(sha256(string_agg(bytes,''::bytea ORDER BY ordinal)),'hex') FROM td_mta.blob_chunks"),hex(&email.digest));
    assert_eq!(
        sql(
            &fixture,
            "SELECT get_byte(done,0) FROM td_mta.replica_pending"
        ),
        "1"
    );
    sql(
        &fixture,
        "UPDATE td_mta.replica_blob_identities SET digest=decode(repeat('00',32),'hex')",
    );
    assert_eq!(
        publisher.publish(effect.clone(), deadline()),
        Err(Error::Integrity)
    );
    drop(publisher);
    sql(
        &fixture,
        &format!(
            "UPDATE td_mta.replica_blob_identities SET digest=decode('{}','hex')",
            hex(&email.digest)
        ),
    );
    let mut publisher = open();
    let update = observation(&source, page, id, 1, Some(&["$seen"]));
    let updated = Arc::new(
        Effect::prepare(
            &source,
            account,
            ticket,
            &update,
            id,
            Some(body_again()),
            deadline(),
        )
        .unwrap(),
    );
    assert_eq!(
        publisher
            .publish(updated.clone(), deadline())
            .unwrap()
            .local_sequence,
        3
    );
    assert_eq!(
        sql(&fixture, "SELECT keyword FROM td_mta.keywords"),
        "$seen"
    );
    assert_eq!(publisher.publish(effect, deadline()), Err(Error::Stale));
    drop(publisher);
    let absent = observation(&source, page, id, 2, None);
    let erased =
        Arc::new(Effect::prepare(&source, account, ticket, &absent, id, None, deadline()).unwrap());
    let mut publisher = open();
    let removed = publisher.publish(erased.clone(), deadline()).unwrap();
    assert!(removed.changed && !removed.present);
    assert_eq!(removed.local_sequence, 4);
    assert!(!publisher.publish(erased, deadline()).unwrap().changed);
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blobs"), "0");
    assert_eq!(
        sql(
            &fixture,
            "SELECT body_bytes,blob_count FROM td_mta.account_usage"
        ),
        "0|0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.replica_blob_identities"
        ),
        "1"
    );
    let restored = observation(&source, page, id, 3, Some(&[]));
    let recreated = Arc::new(
        Effect::prepare(
            &source,
            account,
            ticket,
            &restored,
            id,
            Some(body_again()),
            deadline(),
        )
        .unwrap(),
    );
    assert_eq!(
        publisher
            .publish(recreated.clone(), deadline())
            .unwrap()
            .local_sequence,
        5
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blob_ids"), "1");
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.blobs"), "1");
    let conflict = observation(&source, page, id, 3, Some(&["$flagged"]));
    let conflict = Arc::new(
        Effect::prepare(
            &source,
            account,
            ticket,
            &conflict,
            id,
            Some(body_again()),
            deadline(),
        )
        .unwrap(),
    );
    assert_eq!(
        publisher.publish(conflict, deadline()),
        Err(Error::Integrity)
    );
    drop(publisher);
    let mut publisher = open();
    for _ in 0..16 {
        assert!(
            !publisher
                .publish(recreated.clone(), deadline())
                .unwrap()
                .changed
        );
    }
    assert!(!publisher.reusable(deadline()));
    assert_eq!(publisher.publish(recreated, deadline()), Err(Error::Source));

    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.keywords"), "0");
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn observation(
    source: &Source,
    page: &MetadataPage,
    id: EmailId,
    advance: u64,
    keywords: Option<&[&str]>,
) -> MetadataPage {
    use td_json::{json, Json};
    use td_mta::{bounded::TextBuffer, format::Sequence, sync::DataState, wire};
    let metadata = page.view().unwrap();
    let original = metadata.emails.first().unwrap();
    let mut state = metadata.observed;
    state.sequence = Sequence::from_u64(state.sequence.number() + advance);
    let mut state_bytes = [0; DataState::WIRE_BYTES];
    let state_text = state.encode(&mut state_bytes).unwrap();
    let mut id_bytes = [0; wire::IdKind::WIRE_BYTES];
    let account = wire::encode_id(state.account, &mut id_bytes)
        .unwrap()
        .to_owned();
    let id_text = wire::encode_id(id, &mut id_bytes).unwrap().to_owned();
    let blob = wire::encode_id(original.blob, &mut id_bytes)
        .unwrap()
        .to_owned();
    let thread = wire::encode_id(original.thread, &mut id_bytes)
        .unwrap()
        .to_owned();
    let mailboxes = Json::Obj(
        original
            .mailboxes
            .iter()
            .map(|id| {
                let mut b = [0; wire::IdKind::WIRE_BYTES];
                (
                    wire::encode_id(*id, &mut b).unwrap().to_owned(),
                    Json::Bool(true),
                )
            })
            .collect(),
    );
    let mut date = [0; 64];
    let mut out = TextBuffer::new(&mut date);
    td_mta::jmap_date::write_utc_ms(original.received_at, &mut out).unwrap();
    let (list, missing) = match keywords {
        Some(keywords) => (
            vec![
                json!({"id":id_text,"blobId":blob,"threadId":thread,"mailboxIds":mailboxes,"keywords":Json::Obj(keywords.iter().map(|k|(k.to_string(),Json::Bool(true))).collect()),"size":original.size,"receivedAt":out.as_str().unwrap(),"receivedAtMs":original.received_at.to_string(),"bodySha256":hex(&original.digest)}),
            ],
            Vec::<String>::new(),
        ),
        None => (vec![], vec![id_text]),
    };
    let response = json!({"methodResponses":[["TdReplica/metadata",{"accountId":account,"sourceId":source.epoch().to_string(),"state":state_text,"list":list,"notFound":missing},"replica"]]});
    MetadataPage::decode(
        source.origin(),
        Binding {
            account: state.account,
            epoch: state.epoch,
        },
        vec![id],
        Bounds {
            body_bytes: 1024 * 1024,
            values_per_email: 256,
        },
        response,
    )
    .unwrap()
}
