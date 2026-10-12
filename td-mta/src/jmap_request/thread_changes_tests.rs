#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, deadline, run_setup};
use super::*;
use crate::{
    format::{
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, MailboxRow, Row},
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, BlobId, EmailId, StoreEpoch},
    ports::{Crypto, Digest},
    store_fs::{BlobSource, CommitRequest, IndexStore},
};
use std::sync::Mutex;
static FIXTURE: Mutex<()> = Mutex::new(());
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const BODY: BlobId = BlobId::from_bytes([8; 16]);
fn thread(n: u8) -> ThreadId {
    ThreadId::from_bytes([n; 16])
}
fn id(n: u8) -> String {
    wire::encode_id(thread(n), &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
fn state(kind: DataType, seq: u64) -> String {
    DataState {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([7; 16]),
        kind,
        sequence: Sequence::from_u64(seq),
    }
    .encode(&mut [0; DataState::WIRE_BYTES])
    .unwrap()
    .to_owned()
}
fn row(row: Row<'_>) -> Vec<u8> {
    let mut b = vec![0; 256];
    let len = row.encode(&mut b).unwrap();
    b.truncate(len);
    b
}
fn commit(store: &IndexStore<'_>, sequence: u64, ops: &[Operation<'_>], body: bool) {
    let mut source = b"".as_slice();
    let mut sources = [BlobSource {
        id: BODY,
        source: &mut source,
    }];
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(sequence),
                utc_ms: 0,
                deadline: deadline(),
            },
            ops,
            if body { &mut sources } else { &mut [] },
        )
        .unwrap();
}
fn history(store: &IndexStore<'_>) {
    let blob = row(Row::Blob(BlobRow {
        length: 0,
        digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
        created_at: 0,
    }));
    let mail = row(Row::Mailbox(MailboxRow {
        name: "Folder",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    }));
    let email = |n: u8, t: u8| {
        row(Row::Email(EmailRow {
            blob: BODY,
            thread: thread(t),
            received_at: i64::from(n),
            origin: EmailOrigin::Jmap,
        }))
    };
    let e1 = EmailId::from_bytes([1; 16]);
    let e2 = EmailId::from_bytes([2; 16]);
    let e3 = EmailId::from_bytes([3; 16]);
    commit(
        store,
        0,
        &[
            Operation::put(Table::Blobs, BODY.as_bytes(), &blob).unwrap(),
            Operation::put(Table::Threads, thread(1).as_bytes(), &[]).unwrap(),
            Operation::put(Table::Emails, e1.as_bytes(), &email(1, 1)).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Created,
                thread(1).as_bytes(),
            ),
            Operation::put(Table::Mailboxes, thread(1).as_bytes(), &mail).unwrap(),
            Operation::change(
                ObjectType::Mailbox,
                ChangeAction::Created,
                thread(1).as_bytes(),
            ),
        ],
        true,
    );
    commit(
        store,
        1,
        &[
            Operation::put(Table::Emails, e2.as_bytes(), &email(2, 1)).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Updated,
                thread(1).as_bytes(),
            ),
        ],
        false,
    );
    commit(
        store,
        2,
        &[
            Operation::put(Table::Threads, thread(2).as_bytes(), &[]).unwrap(),
            Operation::put(Table::Emails, e3.as_bytes(), &email(3, 2)).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Created,
                thread(2).as_bytes(),
            ),
        ],
        false,
    );
    commit(
        store,
        3,
        &[
            Operation::delete(Table::Emails, e1.as_bytes()).unwrap(),
            Operation::delete(Table::Emails, e2.as_bytes()).unwrap(),
            Operation::delete(Table::Threads, thread(1).as_bytes()).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Destroyed,
                thread(1).as_bytes(),
            ),
        ],
        false,
    );
}
fn args(since: &str, extra: &str) -> String {
    format!(
        r#"{{"accountId":"{}","sinceState":"{since}"{extra}}}"#,
        account()
    )
}
fn request(args: &str) -> String {
    batch(&format!(
        r#"[["Core/echo",{{"before":true}},"a"],["Thread/changes",{args},"c"],["Core/echo",{{"after":true}},"b"]]"#
    ))
}
fn execute(input: &str) -> td_json::Json {
    let _guard = FIXTURE.lock().unwrap();
    run_setup(input, 0, 256, 90, false, None, history)
}
#[test]
fn coalesced_thread_changes_feed_get_and_omit_mailbox_only_fields() {
    let input = batch(&format!(
        r##"[["Thread/changes",{},"c"],["Thread/get",{{"accountId":"{}","#ids":{{"resultOf":"c","name":"Thread/changes","path":"/created"}}}},"g"]]"##,
        args(&state(DataType::Thread, 0), ""),
        account()
    ));
    let result = execute(&input);
    let page = &result["methodResponses"][0][1];
    assert_eq!(
        page["oldState"].as_str(),
        Some(state(DataType::Thread, 0).as_str())
    );
    assert_eq!(
        page["newState"].as_str(),
        Some(state(DataType::Thread, 4).as_str())
    );
    assert_eq!(
        page["created"],
        td_json::parse(&format!(r#"["{}"]"#, id(2))).unwrap()
    );
    assert_eq!(page["updated"], td_json::parse("[]").unwrap());
    assert_eq!(page["destroyed"], td_json::parse("[]").unwrap());
    assert_eq!(page["hasMoreChanges"].as_bool(), Some(false));
    assert!(!page
        .as_object()
        .unwrap()
        .iter()
        .any(|(name, _)| name == "updatedProperties"));
    assert_eq!(result["methodResponses"][1][1]["state"], page["newState"]);
    let object = &result["methodResponses"][1][1]["list"][0];
    assert_eq!(object["id"].as_str(), Some(id(2).as_str()));
    assert_eq!(object["emailIds"].as_array().unwrap().len(), 1);
}
#[test]
fn page_states_preserve_transactions_and_destroyed_ids() {
    for (old, new, created, updated, destroyed, more) in [
        (0, 2, Some(1), None, None, true),
        (1, 2, None, Some(1), None, true),
        (2, 3, Some(2), None, None, true),
        (3, 4, None, None, Some(1), false),
        (4, 4, None, None, None, false),
    ] {
        let result = execute(&request(&args(
            &state(DataType::Thread, old),
            r#", "maxChanges":1"#,
        )));
        let page = &result["methodResponses"][1][1];
        assert_eq!(
            page["newState"].as_str(),
            Some(state(DataType::Thread, new).as_str())
        );
        assert_eq!(page["hasMoreChanges"].as_bool(), Some(more));
        assert_eq!(
            page["oldState"].as_str(),
            Some(state(DataType::Thread, old).as_str())
        );
        assert_eq!(page["accountId"].as_str(), Some(account().as_str()));
        for (name, value) in [
            ("created", created),
            ("updated", updated),
            ("destroyed", destroyed),
        ] {
            let expected = value
                .map(|n| format!(r#"["{}"]"#, id(n)))
                .unwrap_or("[]".to_owned());
            assert_eq!(page[name], td_json::parse(&expected).unwrap());
        }
    }
}
#[test]
fn wrong_kind_state_and_bad_arguments_are_method_errors() {
    for (arguments, error) in [
        (
            args(&state(DataType::Mailbox, 0), ""),
            "cannotCalculateChanges",
        ),
        (
            args(&state(DataType::Thread, 99), ""),
            "cannotCalculateChanges",
        ),
        (args("unknown", ""), "cannotCalculateChanges"),
        (
            args(&state(DataType::Thread, 0), r#", "maxChanges":0"#),
            "invalidArguments",
        ),
        (
            args(&state(DataType::Thread, 0), r#", "extra":true"#),
            "invalidArguments",
        ),
    ] {
        let result = execute(&request(&arguments));
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some(error)
        );
        assert_eq!(
            result["methodResponses"][0][1]["before"].as_bool(),
            Some(true)
        );
        assert_eq!(
            result["methodResponses"][2][1]["after"].as_bool(),
            Some(true)
        );
    }
    let input = request(&args(&state(DataType::Thread, 0), ""))
        .replace(",\"urn:ietf:params:jmap:mail\"", "");
    let result = execute(&input);
    assert_eq!(
        result["methodResponses"][1][1]["type"].as_str(),
        Some("unknownMethod")
    );
}

#[test]
fn updated_and_destroyed_references_preserve_typed_ids() {
    let input = batch(&format!(
        r##"[["Thread/changes",{},"c"],["Thread/get",{{"accountId":"{}","#ids":{{"resultOf":"c","name":"Thread/changes","path":"/updated"}}}},"g"],["Thread/changes",{},"d"],["Core/echo",{{"#ids":{{"resultOf":"d","name":"Thread/changes","path":"/destroyed"}}}},"e"]]"##,
        args(&state(DataType::Thread, 1), r#", "maxChanges":1"#),
        account(),
        args(&state(DataType::Thread, 3), "")
    ));
    let result = execute(&input);
    let ids = td_json::parse(&format!(r#"["{}"]"#, id(1))).unwrap();
    assert_eq!(result["methodResponses"][0][1]["updated"], ids);
    // /get uses its own current view: the historical update is now destroyed.
    assert_eq!(result["methodResponses"][1][1]["notFound"], ids);
    assert_eq!(result["methodResponses"][3][1]["ids"], ids);
}

#[test]
fn mailbox_only_tail_is_excluded_and_mailbox_keeps_its_fields_and_scope() {
    let _guard = FIXTURE.lock().unwrap();
    let input = batch(&format!(
        r#"[["Thread/changes",{},"t"],["Mailbox/changes",{},"m"],["Mailbox/changes",{},"wrong"]]"#,
        args(&state(DataType::Thread, 0), ""),
        args(&state(DataType::Mailbox, 0), ""),
        args(&state(DataType::Thread, 0), "")
    ));
    let result = run_setup(&input, 0, 256, 90, false, None, |store| {
        history(store);
        let mail = row(Row::Mailbox(MailboxRow {
            name: "Tail",
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: true,
        }));
        commit(
            store,
            4,
            &[
                Operation::put(Table::Mailboxes, thread(9).as_bytes(), &mail).unwrap(),
                Operation::change(
                    ObjectType::Mailbox,
                    ChangeAction::Created,
                    thread(9).as_bytes(),
                ),
            ],
            false,
        );
    });
    let page = &result["methodResponses"][0][1];
    assert_eq!(
        page["created"],
        td_json::parse(&format!(r#"["{}"]"#, id(2))).unwrap()
    );
    assert_eq!(
        page["newState"].as_str(),
        Some(state(DataType::Thread, 5).as_str())
    );
    let mailbox = &result["methodResponses"][1][1];
    assert!(mailbox
        .as_object()
        .unwrap()
        .iter()
        .any(|(name, value)| name == "updatedProperties" && value.is_null()));
    assert_eq!(mailbox["created"].as_array().unwrap().len(), 2);
    assert_eq!(
        result["methodResponses"][2][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
}

#[test]
fn oversized_transaction_and_partial_output_refuse_without_partial_pages() {
    let _guard = FIXTURE.lock().unwrap();
    let seed = |store: &IndexStore<'_>| {
        let blob = row(Row::Blob(BlobRow {
            length: 0,
            digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
            created_at: 0,
        }));
        let ids: Vec<_> = (0u16..600)
            .map(|n| {
                let mut bytes = [0; 16];
                bytes[..2].copy_from_slice(&n.to_be_bytes());
                bytes
            })
            .collect();
        let emails: Vec<_> = ids
            .iter()
            .map(|bytes| {
                row(Row::Email(EmailRow {
                    blob: BODY,
                    thread: ThreadId::from_bytes(*bytes),
                    received_at: 0,
                    origin: EmailOrigin::Jmap,
                }))
            })
            .collect();
        let mut ops = vec![Operation::put(Table::Blobs, BODY.as_bytes(), &blob).unwrap()];
        for (bytes, email) in ids.iter().zip(&emails) {
            ops.push(Operation::put(Table::Threads, bytes, &[]).unwrap());
            ops.push(Operation::put(Table::Emails, bytes, email).unwrap());
            ops.push(Operation::change(
                ObjectType::Thread,
                ChangeAction::Created,
                bytes,
            ));
        }
        commit(store, 0, &ops, true);
    };
    let input = request(&args(&state(DataType::Thread, 0), r#", "maxChanges":1"#));
    let result = run_setup(&input, 0, 1024, 90, false, None, seed);
    assert_eq!(
        result["methodResponses"][1][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
    assert_eq!(
        result["methodResponses"][2][1]["after"].as_bool(),
        Some(true)
    );
    let huge = "x".repeat(55000);
    let input = batch(&format!(
        r#"[["Core/echo",{{"large":"{huge}"}},"a"],["Thread/changes",{},"t"],["Core/echo",{{"after":true}},"b"]]"#,
        args(&state(DataType::Thread, 0), "")
    ));
    let result = run_setup(&input, 0, 1024, 90, false, None, seed);
    assert_eq!(
        result["methodResponses"][0][1]["large"].as_str(),
        Some(huge.as_str())
    );
    assert_eq!(
        result["methodResponses"][1][1]["description"].as_str(),
        Some("responseCapacity")
    );
    assert_eq!(
        result["methodResponses"][2][1]["after"].as_bool(),
        Some(true)
    );
}
