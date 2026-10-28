use super::{deadline, sql, Fixture};
use std::sync::Arc;
use td_crypto::TrustStore;
use td_json::json;
use td_mta::{
    format::Sequence,
    ids::{AccountId, EmailId, MailboxId, StoreEpoch},
    postgres_probe::{Mode, Request},
    postgres_replica_apply::{Error, Publisher},
    postgres_replica_source::Source,
    replica_effect::Effect,
    replica_mailboxes::Graph,
    replica_pending::Ticket,
    replica_source::{Binding, Changes, MailboxPage, MetadataPage},
    replica_transition::Transition,
    sync::{DataState, DataType},
    wire,
};
struct Folder {
    id: MailboxId,
    name: String,
    parent: Option<MailboxId>,
    role: Option<&'static str>,
}
fn graph(source: &Source, account: AccountId, state: DataState, folders: &[Folder]) -> Arc<Graph> {
    let mut state_bytes = [0; DataState::WIRE_BYTES];
    let state_text = state.encode(&mut state_bytes).unwrap();
    let encoded = |id| {
        let mut bytes = [0; wire::IdKind::WIRE_BYTES];
        wire::encode_id(id, &mut bytes).unwrap().to_owned()
    };
    let list=folders.iter().map(|folder|json!({"id":encoded(folder.id),"name":folder.name,"parentId":folder.parent.map(encoded),"role":folder.role,"sortOrder":0,"isSubscribed":true})).collect::<Vec<_>>();
    let mut account_bytes = [0; wire::IdKind::WIRE_BYTES];
    let account_text = wire::encode_id(account, &mut account_bytes).unwrap();
    let response = json!({"methodResponses":[["Mailbox/get",{"accountId":account_text,"state":state_text,"list":list,"notFound":[]},"replica"]]});
    let page = MailboxPage::decode(
        source.origin(),
        Binding {
            account,
            epoch: source.epoch(),
        },
        response,
    )
    .unwrap();
    Arc::new(Graph::prepare(source, account, 2, page).unwrap())
}
pub(super) fn exercise(
    fixture: &Fixture,
    source: &Source,
    account: AccountId,
    local: StoreEpoch,
    page: &MetadataPage,
    email: EmailId,
) {
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let runtime = Request {
        user: "mail_runtime",
        mode: Mode::Runtime,
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
    let base = page.view().unwrap().observed;
    let state = |advance| DataState {
        kind: DataType::Mailbox,
        sequence: Sequence::from_u64(base.sequence.number() + advance),
        ..base
    };
    let inbox = MailboxId::parse(&sql(
        fixture,
        "SELECT encode(inbox,'hex') FROM td_mta.account_bootstraps",
    ))
    .unwrap();
    let inbox_name = sql(
        fixture,
        "SELECT name FROM td_mta.mailboxes WHERE role='inbox'",
    );
    let root = || Folder {
        id: inbox,
        name: inbox_name.clone(),
        parent: None,
        role: Some("inbox"),
    };
    let parent = MailboxId::from_bytes([0x31; 16]);
    let archive = MailboxId::from_bytes([0x32; 16]);
    let replacement = MailboxId::from_bytes([0x33; 16]);
    let initial = graph(
        source,
        account,
        state(10),
        &[
            root(),
            Folder {
                id: parent,
                name: "Parent".into(),
                parent: None,
                role: None,
            },
            Folder {
                id: archive,
                name: "Archive".into(),
                parent: Some(parent),
                role: Some("archive"),
            },
        ],
    );
    let mut publisher = open();
    let before = publisher.inspect(deadline()).unwrap();
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.replica_accounts SET receipt_limit={}",
            before.receipt_count
        ),
    );
    assert_eq!(
        publisher.mailboxes(initial.clone(), deadline()),
        Err(Error::Quota)
    );
    drop(publisher);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "1");
    sql(fixture,"UPDATE td_mta.replica_accounts SET receipt_limit=100; CREATE TRIGGER reject_replica_graph BEFORE UPDATE ON td_mta.replica_progress FOR EACH ROW EXECUTE FUNCTION public.reject_replica_completion()");
    let mut publisher = open();
    assert_eq!(
        publisher.mailboxes(initial.clone(), deadline()),
        Err(Error::Database)
    );
    drop(publisher);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "1");
    assert_eq!(
        sql(fixture, "SELECT receipt_count FROM td_mta.replica_progress"),
        before.receipt_count.to_string()
    );
    sql(
        fixture,
        "DROP TRIGGER reject_replica_graph ON td_mta.replica_progress",
    );
    let mut publisher = open();
    let first = publisher.mailboxes(initial.clone(), deadline()).unwrap();
    assert!(first.changed);
    assert_eq!(first.pending_removals, 0);
    assert!(
        !publisher
            .mailboxes(initial.clone(), deadline())
            .unwrap()
            .changed
    );
    let progress = publisher.inspect(deadline()).unwrap();
    assert_eq!(progress.mailbox_checkpoint, state(10));
    let metadata = super::replica_publish::observation_with_mailboxes(
        source,
        page,
        email,
        20,
        Some(&[]),
        Some(&[archive]),
    );
    let email_state = metadata.view().unwrap().observed;
    let previous = progress.pending.unwrap().ticket();
    let transition = Arc::new(
        Transition::changes(
            source,
            account,
            previous,
            &Changes {
                old: base,
                new: email_state,
                more: false,
                created: vec![],
                updated: vec![email],
                destroyed: vec![],
            },
        )
        .unwrap(),
    );
    let ticket = publisher.advance(transition, deadline()).unwrap();
    assert_eq!(
        ticket,
        Ticket {
            generation: 2,
            page: 3
        }
    );
    let effect = Arc::new(
        Effect::prepare_existing(source, account, ticket, &metadata, email, deadline()).unwrap(),
    );
    assert!(publisher.publish(effect, deadline()).unwrap().changed);
    let removed = graph(
        source,
        account,
        state(21),
        &[
            root(),
            Folder {
                id: replacement,
                name: "New archive".into(),
                parent: None,
                role: Some("archive"),
            },
        ],
    );
    let result = publisher.mailboxes(removed.clone(), deadline()).unwrap();
    assert!(result.changed);
    assert_eq!(result.pending_removals, 2);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "4");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE role='archive'"
        ),
        "1"
    );
    assert_eq!(
        publisher.inspect(deadline()).unwrap().mailbox_checkpoint,
        state(10)
    );
    let mut full = vec![root()];
    for n in 0..255u8 {
        let mut id = [0xee; 16];
        id[15] = n;
        full.push(Folder {
            id: MailboxId::from_bytes(id),
            name: format!("Folder {n}"),
            parent: None,
            role: None,
        });
    }
    assert_eq!(
        publisher.mailboxes(graph(source, account, state(22), &full), deadline()),
        Err(Error::Capacity)
    );
    drop(publisher);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "4");
    let mut publisher = open();
    let metadata = super::replica_publish::observation_with_mailboxes(
        source,
        page,
        email,
        22,
        Some(&[]),
        Some(&[replacement]),
    );
    let effect = Arc::new(
        Effect::prepare_existing(source, account, ticket, &metadata, email, deadline()).unwrap(),
    );
    assert!(publisher.publish(effect, deadline()).unwrap().changed);
    let result = publisher.mailboxes(removed.clone(), deadline()).unwrap();
    assert!(result.changed);
    assert_eq!(result.pending_removals, 0);
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "2");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.replica_mailbox_receipts"
        ),
        "4"
    );
    assert_eq!(
        publisher.inspect(deadline()).unwrap().mailbox_checkpoint,
        state(21)
    );
    assert!(!publisher.mailboxes(removed, deadline()).unwrap().changed);
    let conflict = graph(
        source,
        account,
        state(21),
        &[
            root(),
            Folder {
                id: replacement,
                name: "Conflict".into(),
                parent: None,
                role: Some("archive"),
            },
        ],
    );
    assert_eq!(
        publisher.mailboxes(conflict, deadline()),
        Err(Error::Integrity)
    );
    drop(publisher);
    let mut publisher = open();
    assert_eq!(publisher.mailboxes(initial, deadline()), Err(Error::Stale));
}
