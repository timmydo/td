#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, deadline, run_setup};
use super::*;
use crate::{
    format::{
        key::Key,
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, Row},
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, BlobId, StoreEpoch},
    ports::{Crypto, Digest},
    store_fs::{BlobSource, CommitRequest, IndexStore},
};
use std::sync::Mutex;
static FIXTURE: Mutex<()> = Mutex::new(());
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const BLOB: BlobId = BlobId::from_bytes([8; 16]);
fn email(n: u8) -> EmailId {
    EmailId::from_bytes([n; 16])
}
fn thread(n: u8) -> ThreadId {
    ThreadId::from_bytes([n; 16])
}
fn id(n: u8) -> String {
    wire::encode_id(email(n), &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
fn state(seq: u64) -> String {
    DataState {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([7; 16]),
        kind: DataType::Email,
        sequence: Sequence::from_u64(seq),
    }
    .encode(&mut [0; DataState::WIRE_BYTES])
    .unwrap()
    .to_owned()
}
fn encoded(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 256];
    let len = row.encode(&mut bytes).unwrap();
    bytes.truncate(len);
    bytes
}
fn commit(store: &IndexStore<'_>, seq: u64, ops: &[Operation<'_>], body: bool) {
    let mut input = b"".as_slice();
    let mut bodies = [BlobSource {
        id: BLOB,
        source: &mut input,
    }];
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(seq),
                utc_ms: 0,
                deadline: deadline(),
            },
            ops,
            if body { &mut bodies } else { &mut [] },
        )
        .unwrap();
}
fn history(store: &IndexStore<'_>) {
    let blob = encoded(Row::Blob(BlobRow {
        length: 0,
        digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
        created_at: 0,
    }));
    let row = |t| {
        encoded(Row::Email(EmailRow {
            blob: BLOB,
            thread: thread(t),
            received_at: 0,
            origin: EmailOrigin::Jmap,
        }))
    };
    commit(
        store,
        0,
        &[
            Operation::put(Table::Blobs, BLOB.as_bytes(), &blob).unwrap(),
            Operation::put(Table::Threads, thread(1).as_bytes(), &[]).unwrap(),
            Operation::put(Table::Emails, email(1).as_bytes(), &row(1)).unwrap(),
            Operation::put(Table::Emails, email(2).as_bytes(), &row(1)).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Created,
                thread(1).as_bytes(),
            ),
            Operation::change(
                ObjectType::Email,
                ChangeAction::Created,
                email(1).as_bytes(),
            ),
            Operation::change(
                ObjectType::Email,
                ChangeAction::Created,
                email(2).as_bytes(),
            ),
        ],
        true,
    );
    let mut keyword = [0; 128];
    let len = Key::Keyword(email(1), "$seen")
        .encode(&mut keyword)
        .unwrap();
    commit(
        store,
        1,
        &[
            Operation::put(Table::Keywords, &keyword[..len], &[]).unwrap(),
            Operation::change(
                ObjectType::Email,
                ChangeAction::Updated,
                email(1).as_bytes(),
            ),
        ],
        false,
    );
    commit(
        store,
        2,
        &[
            Operation::put(Table::Threads, thread(2).as_bytes(), &[]).unwrap(),
            Operation::put(Table::Emails, email(3).as_bytes(), &row(2)).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Created,
                thread(2).as_bytes(),
            ),
            Operation::change(
                ObjectType::Email,
                ChangeAction::Created,
                email(3).as_bytes(),
            ),
        ],
        false,
    );
    commit(
        store,
        3,
        &[
            Operation::delete(Table::Emails, email(2).as_bytes()).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Updated,
                thread(1).as_bytes(),
            ),
            Operation::change(
                ObjectType::Email,
                ChangeAction::Destroyed,
                email(2).as_bytes(),
            ),
        ],
        false,
    );
    commit(
        store,
        4,
        &[
            Operation::delete(Table::Emails, email(3).as_bytes()).unwrap(),
            Operation::delete(Table::Threads, thread(2).as_bytes()).unwrap(),
            Operation::change(
                ObjectType::Thread,
                ChangeAction::Destroyed,
                thread(2).as_bytes(),
            ),
            Operation::change(
                ObjectType::Email,
                ChangeAction::Destroyed,
                email(3).as_bytes(),
            ),
        ],
        false,
    );
}
fn request(since: &str, extra: &str) -> String {
    batch(&format!(
        r#"[["Email/changes",{{"accountId":"{}","sinceState":"{since}"{extra}}},"c"],["Core/echo",{{"ok":true}},"after"]]"#,
        account()
    ))
}
#[test]
fn native_email_history_coalesces_creation_update_and_destruction() {
    let _guard = FIXTURE.lock().unwrap();
    for (since, created, updated, destroyed) in [
        (0, vec![1], vec![], vec![]),
        (1, vec![], vec![1], vec![2]),
        (2, vec![], vec![], vec![2]),
        (3, vec![], vec![], vec![2, 3]),
        (4, vec![], vec![], vec![3]),
        (5, vec![], vec![], vec![]),
    ] {
        let response = run_setup(
            &request(&state(since), ""),
            0,
            256,
            90,
            false,
            None,
            history,
        );
        let page = &response["methodResponses"][0][1];
        assert_eq!(
            response["methodResponses"][0][0].as_str(),
            Some("Email/changes")
        );
        assert_eq!(page["oldState"].as_str(), Some(state(since).as_str()));
        assert_eq!(page["newState"].as_str(), Some(state(5).as_str()));
        assert_eq!(page["hasMoreChanges"].as_bool(), Some(false));
        for (key, values) in [
            ("created", created),
            ("updated", updated),
            ("destroyed", destroyed),
        ] {
            let got: Vec<_> = page[key]
                .as_arr()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_owned())
                .collect();
            assert_eq!(got, values.into_iter().map(id).collect::<Vec<_>>());
        }
        assert!(!page
            .as_object()
            .unwrap()
            .iter()
            .any(|(key, _)| key == "updatedProperties"));
        assert_eq!(
            response["methodResponses"][1][1]["ok"].as_bool(),
            Some(true)
        );
    }
}
#[test]
fn pages_never_split_the_first_atomic_email_transaction() {
    let _guard = FIXTURE.lock().unwrap();
    let response = run_setup(
        &request(&state(0), r#", "maxChanges":1"#),
        0,
        256,
        90,
        false,
        None,
        history,
    );
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
    let response = run_setup(
        &request(&state(0), r#", "maxChanges":2e0"#),
        0,
        256,
        90,
        false,
        None,
        history,
    );
    let page = &response["methodResponses"][0][1];
    assert_eq!(page["newState"].as_str(), Some(state(2).as_str()));
    assert_eq!(
        page["created"]
            .as_arr()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec![id(1), id(2)]
    );
    assert!(page["updated"].as_arr().unwrap().is_empty());
    assert!(page["destroyed"].as_arr().unwrap().is_empty());
    assert_eq!(page["hasMoreChanges"].as_bool(), Some(true));
    let response = run_setup(
        &request(page["newState"].as_str().unwrap(), r#", "maxChanges":2"#),
        0,
        256,
        90,
        false,
        None,
        history,
    );
    assert_eq!(
        response["methodResponses"][0][1]["newState"].as_str(),
        Some(state(5).as_str())
    );
    assert_eq!(
        response["methodResponses"][0][1]["destroyed"][0].as_str(),
        Some(id(2).as_str())
    );
    let page = &response["methodResponses"][0][1];
    assert_eq!(page["destroyed"].as_arr().unwrap().len(), 1);
    assert!(page["created"].as_arr().unwrap().is_empty());
    assert!(page["updated"].as_arr().unwrap().is_empty());
    assert_eq!(page["hasMoreChanges"].as_bool(), Some(false));
}
#[test]
fn email_state_scope_capability_limits_and_view_errors_keep_classification() {
    let _guard = FIXTURE.lock().unwrap();
    let mut wrong = DataState::decode(&state(0)).unwrap();
    wrong.kind = DataType::Thread;
    let wrong = wrong
        .encode(&mut [0; DataState::WIRE_BYTES])
        .unwrap()
        .to_owned();
    for (input, busy, phase, error) in [
        (request(&wrong, ""), false, 90, "cannotCalculateChanges"),
        (request(&state(6), ""), false, 90, "cannotCalculateChanges"),
        (request("invalid", ""), false, 90, "cannotCalculateChanges"),
        (
            request(&state(0), "").replace("Email/changes", "Thread/changes"),
            false,
            90,
            "cannotCalculateChanges",
        ),
        (
            request(&state(0), "").replace("Email/changes", "Mailbox/changes"),
            false,
            90,
            "cannotCalculateChanges",
        ),
        (
            request(&state(0), r#", "maxChanges":1.5"#),
            false,
            90,
            "invalidArguments",
        ),
        (
            request(&state(0), r#", "maxChanges":0"#),
            false,
            90,
            "invalidArguments",
        ),
        (
            request(&state(0), r#", "maxChanges":-1"#),
            false,
            90,
            "invalidArguments",
        ),
        (
            request(&state(0), "").replace(",\"urn:ietf:params:jmap:mail\"", ""),
            false,
            90,
            "unknownMethod",
        ),
        (request(&state(0), ""), true, 90, "serverUnavailable"),
        (request(&state(0), ""), false, 0, "serverUnavailable"),
        (
            request(&state(0), "").replace(&account(), "foreign"),
            false,
            90,
            "accountNotFound",
        ),
    ] {
        let response = run_setup(&input, 0, 256, phase, busy, None, history);
        assert_eq!(response["methodResponses"][0][0].as_str(), Some("error"));
        if phase != 0 {
            assert_eq!(
                response["methodResponses"][1][1]["ok"].as_bool(),
                Some(true)
            );
        }
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some(error)
        );
    }
}
#[test]
fn email_changes_result_references_preserve_ordered_method_batches() {
    let _guard = FIXTURE.lock().unwrap();
    let input = batch(&format!(
        r##"[["Core/echo",{{"state":"{}"}},"s"],["Email/changes",{{"accountId":"{}","#sinceState":{{"resultOf":"s","name":"Core/echo","path":"/state"}}}},"c"],["Core/echo",{{"#ids":{{"resultOf":"c","name":"Email/changes","path":"/created"}}}},"out"]]"##,
        state(0),
        account()
    ));
    let response = run_setup(&input, 0, 256, 90, false, None, history);
    assert_eq!(
        response["methodResponses"][2][1]["ids"][0].as_str(),
        Some(id(1).as_str())
    );
}
