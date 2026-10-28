use super::{deadline, sql, Fixture};
use std::sync::Arc;
use td_crypto::TrustStore;
use td_json::json;
use td_mta::{
    format::Sequence,
    ids::{AccountId, EmailId, MailboxId, StoreEpoch},
    postgres_read::ReplicaInventoryCursor,
    postgres_replica_accounts::{self as accounts, Error},
    postgres_replica_source::{self as source, Source},
    postgres_schema,
    replica_seed::{Limits, Seed},
    replica_source::{Binding, MailboxPage},
    sync::{DataState, DataType},
    wire,
};

fn seed(origin: &str, limits: Limits) -> Arc<Seed> {
    Arc::new(prepare(origin, limits, false).unwrap())
}

fn prepare(
    origin: &str,
    limits: Limits,
    continuation: bool,
) -> Result<Seed, td_mta::replica_seed::Error> {
    let account = AccountId::from_bytes([2; 16]);
    let epoch = StoreEpoch::from_bytes([7; 16]);
    let inbox = MailboxId::from_bytes([9; 16]);
    let child = MailboxId::from_bytes([3; 16]);
    let state = DataState {
        account,
        epoch,
        kind: DataType::Email,
        sequence: Sequence::from_u64(5),
    };
    let mailbox = DataState {
        kind: DataType::Mailbox,
        sequence: Sequence::from_u64(6),
        ..state
    };
    let wire_id = |id| {
        let mut out = [0; wire::IdKind::WIRE_BYTES];
        wire::encode_id(id, &mut out).unwrap().to_owned()
    };
    let mut out = [0; wire::IdKind::WIRE_BYTES];
    let account_id = wire::encode_id(account, &mut out).unwrap().to_owned();
    let mut out = [0; DataState::WIRE_BYTES];
    let observed = mailbox.encode(&mut out).unwrap().to_owned();
    let graph = json!({"methodResponses":[["Mailbox/get",{"accountId":account_id,"state":observed,"notFound":[],"list":[
        {"id":wire_id(child),"name":"Child archive","parentId":wire_id(inbox),"role":null,"sortOrder":87,"isSubscribed":false},
        {"id":wire_id(inbox),"name":"Source inbox name","parentId":null,"role":"inbox","sortOrder":19,"isSubscribed":true}
    ]},"replica"]]});
    let page = MailboxPage::decode(origin, Binding { account, epoch }, graph).unwrap();
    let mut state_wire = [0; DataState::WIRE_BYTES];
    let state_text = state.encode(&mut state_wire).unwrap();
    let next = ReplicaInventoryCursor {
        base: state,
        after: EmailId::from_bytes([1; 16]),
        upper: EmailId::from_bytes([9; 16]),
    };
    let previous = continuation.then_some(ReplicaInventoryCursor {
        after: EmailId::from_bytes([0; 16]),
        ..next
    });
    let mut cursor_wire = [0; ReplicaInventoryCursor::WIRE_BYTES];
    let mut id_wire = [0; wire::IdKind::WIRE_BYTES];
    let one = wire::encode_id(EmailId::from_bytes([1; 16]), &mut id_wire)
        .unwrap()
        .to_owned();
    let zero = wire::encode_id(EmailId::from_bytes([0; 16]), &mut id_wire)
        .unwrap()
        .to_owned();
    let ids = if continuation {
        vec![one]
    } else {
        vec![zero, one]
    };
    let maximum = ids.len();
    let response = json!({"accountId":account_id,"sourceId":"07070707070707070707070707070707",
        "baseState":state_text,"state":state_text,"ids":ids,
        "nextCursor":next.encode(&mut cursor_wire).unwrap()});
    let inventory =
        td_mta::replica_source::inventory(&response, Binding { account, epoch }, previous, maximum)
            .unwrap();
    Seed::prepare(Source::new(origin, epoch).unwrap(), inventory, page, limits)
}

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_replica_seed_preserves_ids_and_resumes_without_reset() {
    exercise();
}

pub(super) fn exercise() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let local = postgres_schema::initialize(&owner, &trust, deadline())
        .unwrap()
        .store_epoch;
    let binding = Source::new(
        "https://source.example.test",
        StoreEpoch::from_bytes([7; 16]),
    )
    .unwrap();
    source::initialize(&owner, &trust, deadline(), local, &binding).unwrap();
    let limits = Limits {
        body_bytes: 1024 * 1024,
        blobs: 100,
        emails: 200,
        receipts: 400,
    };
    assert!(matches!(
        prepare(binding.origin(), limits, true),
        Err(td_mta::replica_seed::Error::Source)
    ));
    let seed = seed(binding.origin(), limits);
    let publish = || accounts::initialize(&owner, &trust, deadline(), local, seed.clone());
    sql(&fixture,"CREATE FUNCTION public.reject_replica_page() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture'; END $$; CREATE TRIGGER reject_replica_page BEFORE INSERT ON td_mta.replica_pending FOR EACH ROW EXECUTE FUNCTION public.reject_replica_page()");
    assert_eq!(publish(), Err(Error::Database));
    for table in [
        "accounts",
        "mailboxes",
        "changes",
        "account_bootstraps",
        "account_usage",
        "replica_accounts",
        "replica_progress",
        "replica_pending",
        "replica_mailbox_receipts",
    ] {
        assert_eq!(
            sql(&fixture, &format!("SELECT count(*) FROM td_mta.{table}")),
            "0",
            "{table}"
        );
    }
    sql(
        &fixture,
        "DROP TRIGGER reject_replica_page ON td_mta.replica_pending",
    );
    publish().unwrap();
    let progress = td_mta::postgres_replica_progress::inspect(
        &owner,
        &trust,
        deadline(),
        local,
        seed.account(),
    )
    .unwrap();
    assert_eq!(progress.seed_digest, seed.digest());
    assert_eq!(progress.email_cursor.sequence.number(), 5);
    assert_eq!(progress.mailbox_checkpoint.sequence.number(), 6);
    assert!(!progress.inventory_complete);
    assert_eq!(progress.pending.unwrap().unfinished().count(), 2);
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET done=set_byte(done,0,1)",
    );
    let progress = td_mta::postgres_replica_progress::inspect(
        &owner,
        &trust,
        deadline(),
        local,
        seed.account(),
    )
    .unwrap();
    assert_eq!(
        progress.pending.unwrap().unfinished().collect::<Vec<_>>(),
        vec![EmailId::from_bytes([1; 16])]
    );
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET done=set_byte(done,0,4)",
    );
    assert!(matches!(
        td_mta::postgres_replica_progress::inspect(
            &owner,
            &trust,
            deadline(),
            local,
            seed.account()
        ),
        Err(td_mta::postgres_replica_progress::Error::Integrity)
    ));
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET done=set_byte(done,0,0)",
    );
    let inspect = || {
        td_mta::postgres_replica_progress::inspect(
            &owner,
            &trust,
            deadline(),
            local,
            seed.account(),
        )
    };
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET observed=decode('0000000000000006','hex')",
    );
    assert!(matches!(
        inspect(),
        Err(td_mta::postgres_replica_progress::Error::Integrity)
    ));
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET observed=decode('0000000000000005','hex')",
    );
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET generation=decode('0000000000000002','hex')",
    );
    assert!(matches!(
        inspect(),
        Err(td_mta::postgres_replica_progress::Error::Integrity)
    ));
    sql(&fixture,"UPDATE td_mta.replica_pending SET generation=decode('0000000000000001','hex'),ids=decode(repeat('00',32),'hex')");
    assert!(matches!(
        inspect(),
        Err(td_mta::postgres_replica_progress::Error::Integrity)
    ));
    sql(
        &fixture,
        "UPDATE td_mta.replica_pending SET ids=decode(repeat('00',16)||repeat('01',16),'hex')",
    );
    inspect().unwrap();
    let initial_next = sql(&fixture, "SELECT next_cursor FROM td_mta.replica_pending");
    sql(&fixture, &format!("UPDATE td_mta.replica_progress SET inventory_cursor='{initial_next}'; UPDATE td_mta.replica_pending SET page=decode('0000000000000002','hex'),observed=decode('0000000000000007','hex'),ids=decode(repeat('02',16)||repeat('03',16),'hex'),next_cursor=NULL"));
    let later = inspect().unwrap();
    assert_eq!(
        later.pending.unwrap().unfinished().collect::<Vec<_>>(),
        vec![EmailId::from_bytes([2; 16]), EmailId::from_bytes([3; 16])]
    );
    sql(&fixture, "UPDATE td_mta.replica_progress SET inventory_complete=true; UPDATE td_mta.replica_pending SET kind=2,observed=decode('0000000000000009','hex'),next_state=decode('0000000000000009','hex'),more=true,done=set_byte(done,0,1)");
    let changes = inspect().unwrap();
    let pending = changes.pending.unwrap();
    assert_eq!(
        pending.unfinished().collect::<Vec<_>>(),
        vec![EmailId::from_bytes([3; 16])]
    );
    assert!(
        matches!(pending.advance(), td_mta::replica_pending::Advance::Changes { state, more:true } if state.sequence.number()==9)
    );
    assert_eq!(changes.email_cursor.sequence.number(), 5);
    sql(&fixture, &format!("UPDATE td_mta.replica_progress SET inventory_cursor=NULL,inventory_complete=false; UPDATE td_mta.replica_pending SET page=decode('0000000000000001','hex'),kind=1,observed=decode('0000000000000005','hex'),ids=decode(repeat('00',16)||repeat('01',16),'hex'),done=set_byte(done,0,0),next_cursor='{initial_next}',next_state=NULL,more=false"));

    assert!(matches!(
        td_mta::postgres_replica_progress::inspect(
            &owner,
            &trust,
            deadline(),
            local,
            AccountId::from_bytes([8; 16])
        ),
        Err(td_mta::postgres_replica_progress::Error::Missing)
    ));
    assert!(matches!(
        td_mta::postgres_replica_progress::inspect(
            &owner,
            &trust,
            deadline(),
            StoreEpoch::from_bytes([8; 16]),
            seed.account()
        ),
        Err(td_mta::postgres_replica_progress::Error::Epoch)
    ));

    assert_eq!(sql(&fixture,"SELECT encode(id,'hex'),encode(sequence,'hex'),encode(floor,'hex') FROM td_mta.accounts"),format!("{}|0000000000000001|0000000000000000","02".repeat(16)));
    assert_eq!(sql(&fixture,"SELECT encode(id,'hex'),name,COALESCE(encode(parent_id,'hex'),'none'),COALESCE(role,'none'),sort_order,subscribed FROM td_mta.mailboxes ORDER BY id"),format!("{}|Child archive|{}|none|87|0\n{}|Source inbox name|none|inbox|19|1","03".repeat(16),"09".repeat(16),"09".repeat(16)));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(inbox,'hex') FROM td_mta.account_bootstraps"
        ),
        "09".repeat(16)
    );
    assert_eq!(sql(&fixture,"SELECT encode(email_cursor,'hex'),encode(mailbox_checkpoint,'hex'),inventory_complete,email_count,receipt_count FROM td_mta.replica_progress"),"0000000000000005|0000000000000006|f|0|2");
    assert_eq!(sql(&fixture,"SELECT encode(ids,'hex'),octet_length(done),encode(done,'hex')=repeat('00',128),next_cursor IS NOT NULL FROM td_mta.replica_pending"),format!("{}{}|128|t|t","00".repeat(16),"01".repeat(16)));
    assert_eq!(sql(&fixture,"SELECT count(*) FROM td_mta.changes WHERE kind=1 AND action=1 AND sequence=decode('0000000000000001','hex')"),"2");
    assert_eq!(sql(&fixture,"SELECT body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions FROM td_mta.account_usage"),"0|0|0|0|0");
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    publish().unwrap();
    // Simulated later progress must survive a retained bootstrap retry.
    sql(&fixture,"UPDATE td_mta.accounts SET sequence=decode('0000000000000002','hex'); UPDATE td_mta.mailboxes SET name='Later name',changed=decode('0000000000000002','hex') WHERE role='inbox'; UPDATE td_mta.replica_progress SET generation=decode('0000000000000002','hex'); UPDATE td_mta.replica_pending SET generation=decode('0000000000000002','hex'); INSERT INTO td_mta.changes SELECT account,changed,1,1,2,id FROM td_mta.mailboxes WHERE role='inbox'");
    publish().unwrap();
    assert_eq!(
        sql(
            &fixture,
            "SELECT name FROM td_mta.mailboxes WHERE role='inbox'"
        ),
        "Later name"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(generation,'hex') FROM td_mta.replica_progress"
        ),
        "0000000000000002"
    );
    let different = seed_for_different_limits(limits);
    assert_eq!(
        accounts::initialize(&owner, &trust, deadline(), local, different),
        Err(Error::Conflict)
    );
    let other = seed_for_other_source(limits);
    assert_eq!(
        accounts::initialize(&owner, &trust, deadline(), local, other),
        Err(Error::Source)
    );
}
fn seed_for_different_limits(mut limits: Limits) -> Arc<Seed> {
    limits.emails += 1;
    seed("https://source.example.test", limits)
}
fn seed_for_other_source(limits: Limits) -> Arc<Seed> {
    seed("https://other.example.test", limits)
}
