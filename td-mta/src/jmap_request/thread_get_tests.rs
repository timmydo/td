#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, deadline, run_setup};
use super::*;
use crate::{
    format::{
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, Row},
        Sequence, Table,
    },
    ids::{AccountId, BlobId, EmailId},
    ports::{Crypto, Digest},
    store_fs::{BlobSource, CommitRequest, IndexStore},
};
use std::sync::Mutex;
static FIXTURE: Mutex<()> = Mutex::new(());
const THREAD: ThreadId = ThreadId::from_bytes([3; 16]);
const EMPTY: ThreadId = ThreadId::from_bytes([4; 16]);
pub(crate) fn id<T: wire::LocalId>(value: T) -> String {
    wire::encode_id(value, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
pub(crate) fn email(n: u16) -> EmailId {
    let mut b = [0; 16];
    b[..2].copy_from_slice(&n.to_be_bytes());
    EmailId::from_bytes(b)
}
pub(crate) fn date(n: u16) -> i64 {
    100 - i64::from(n / 3)
}
fn encoded(row: Row<'_>) -> Vec<u8> {
    let mut b = vec![0; 256];
    let len = row.encode(&mut b).unwrap();
    b.truncate(len);
    b
}
pub(crate) fn seed(store: &IndexStore<'_>, count: u16) {
    let body = BlobId::from_bytes([5; 16]);
    let blob = encoded(Row::Blob(BlobRow {
        length: 0,
        digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
        created_at: 0,
    }));
    let rows: Vec<_> = (0..count)
        .map(|n| {
            (
                email(n),
                encoded(Row::Email(EmailRow {
                    blob: body,
                    thread: THREAD,
                    received_at: date(n),
                    origin: EmailOrigin::Jmap,
                })),
            )
        })
        .collect();
    let mut ops = vec![
        Operation::put(Table::Blobs, body.as_bytes(), &blob).unwrap(),
        Operation::put(Table::Threads, THREAD.as_bytes(), &[]).unwrap(),
        Operation::put(Table::Threads, EMPTY.as_bytes(), &[]).unwrap(),
    ];
    for (id, row) in &rows {
        ops.push(Operation::put(Table::Emails, id.as_bytes(), row).unwrap());
    }
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: AccountId::from_bytes([1; 16]),
                epoch: store.epoch(),
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline(),
            },
            &ops,
            &mut [BlobSource {
                id: body,
                source: &mut b"".as_slice(),
            }],
        )
        .unwrap();
}
fn execute(
    input: &str,
    count: u16,
    maximum: usize,
    phase: u64,
    busy: bool,
    cutoff: Option<u64>,
) -> td_json::Json {
    let _guard = FIXTURE.lock().unwrap();
    run_setup(input, 0, maximum, phase, busy, cutoff, |store| {
        seed(store, count)
    })
}
fn request(args: &str) -> String {
    batch(&format!(
        r#"[["Core/echo",{{"before":true}},"a"],["Thread/get",{args},"t"],["Core/echo",{{"after":true}},"b"]]"#
    ))
}
fn args(extra: &str) -> String {
    format!(
        r#"{{"accountId":"{}","ids":["{}"]{extra}}}"#,
        account(),
        id(THREAD)
    )
}
#[test]
fn member_windows_are_one_ordered_object_and_references_use_real_output() {
    let calls = format!(
        r##"[["Core/echo",{{"ids":["{}","{}","foreign","foreign","{}"]}},"a"],["Thread/get",{{"accountId":"{}","#ids":{{"resultOf":"a","name":"Core/echo","path":"/ids"}}}},"t"],["Core/echo",{{"#members":{{"resultOf":"t","name":"Thread/get","path":"/list/0/emailIds"}}}},"b"]]"##,
        id(THREAD),
        id(THREAD),
        id(EMPTY),
        account()
    );
    let result = execute(&batch(&calls), 513, 256, 90, false, None);
    let response = &result["methodResponses"][1][1];
    assert_eq!(response["accountId"].as_str(), Some(account().as_str()));
    let state = DataState::decode(response["state"].as_str().unwrap()).unwrap();
    assert_eq!(state.kind, DataType::Thread);
    assert_eq!(state.sequence, Sequence::from_u64(1));
    let mut expected: Vec<_> = (0..513).collect();
    expected.sort_by_key(|n| (date(*n), email(*n)));
    let expected = td_json::parse(&format!(
        "[{}]",
        expected
            .iter()
            .map(|n| format!("\"{}\"", id(email(*n))))
            .collect::<Vec<_>>()
            .join(",")
    ))
    .unwrap();
    assert_eq!(response["list"].as_array().unwrap().len(), 1);
    assert_eq!(
        response["list"][0]["id"].as_str(),
        Some(id(THREAD).as_str())
    );
    assert_eq!(response["list"][0]["emailIds"], expected);
    assert_eq!(result["methodResponses"][2][1]["members"], expected);
    assert_eq!(
        response["notFound"],
        td_json::parse(&format!(r#"["foreign","{}"]"#, id(EMPTY))).unwrap()
    );
}
#[test]
fn property_projection_empty_requests_and_capability_admission() {
    for (extra, members) in [
        ("", true),
        (r#", "properties":null"#, true),
        (r#", "properties":[]"#, false),
        (r#", "properties":["id"]"#, false),
        (r#", "properties":["emailIds","emailIds"]"#, true),
    ] {
        let result = execute(&request(&args(extra)), 2, 256, 90, false, None);
        let object = &result["methodResponses"][1][1]["list"][0];
        assert_eq!(object["id"].as_str(), Some(id(THREAD).as_str()));
        assert_eq!(object["emailIds"].is_null(), !members);
    }
    let empty = request(&format!(r#"{{"accountId":"{}","ids":[]}}"#, account()));
    let result = execute(&empty, 2, 256, 90, false, None);
    assert_eq!(
        result["methodResponses"][1][1]["list"],
        td_json::parse("[]").unwrap()
    );
    let no_mail = request(&args("")).replace(",\"urn:ietf:params:jmap:mail\"", "");
    let result = execute(&no_mail, 2, 256, 90, false, None);
    assert_eq!(
        result["methodResponses"][1][1]["type"].as_str(),
        Some("unknownMethod")
    );
}
#[test]
fn invalid_shapes_limits_and_absent_threads_preserve_other_replies() {
    for arguments in [
        args(r#", "properties":["unknown"]"#),
        args(r#", "extra":true"#),
        format!(r#"{{"accountId":"{}","ids":[1]}}"#, account()),
        format!(r#"{{"accountId":"{}","ids":["bad id"]}}"#, account()),
    ] {
        let result = execute(&request(&arguments), 2, 256, 90, false, None);
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some("invalidArguments"),
            "{arguments}"
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
    let duplicate = format!(
        r#"{{"accountId":"{}","ids":["{}","{}"]}}"#,
        account(),
        id(THREAD),
        id(THREAD)
    );
    let result = execute(&request(&duplicate), 2, 1, 90, false, None);
    assert_eq!(
        result["methodResponses"][1][1]["type"].as_str(),
        Some("requestTooLarge")
    );
    let absent = format!(
        r#"{{"accountId":"{}","ids":["{}","foreign"]}}"#,
        account(),
        id(ThreadId::from_bytes([9; 16]))
    );
    let result = execute(&request(&absent), 2, 256, 90, false, None);
    assert_eq!(
        result["methodResponses"][1][1]["list"],
        td_json::parse("[]").unwrap()
    );
    assert_eq!(
        result["methodResponses"][1][1]["notFound"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[test]
fn capacity_and_busy_refusals_roll_back_the_whole_thread_object() {
    let huge = "x".repeat(55000);
    let input = batch(&format!(
        r#"[["Core/echo",{{"large":"{huge}"}},"a"],["Thread/get",{},"t"],["Core/echo",{{"after":true}},"b"]]"#,
        args("")
    ));
    let result = execute(&input, 513, 256, 90, false, None);
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
    let result = execute(&request(&args("")), 2, 256, 90, true, None);
    assert_eq!(
        result["methodResponses"][1][1]["description"].as_str(),
        Some("storageBusy")
    );
}

#[test]
fn late_member_deadline_withholds_partial_object_and_expires_following_call() {
    let input = request(&args(""));
    let mut witnessed = false;
    for cutoff in [512, 1024, 2048, 4096, 8192, 16384] {
        let small = execute(&input, 1, 256, 90, false, Some(cutoff));
        let large = execute(&input, 513, 256, 90, false, Some(cutoff));
        if small["methodResponses"][1][0].as_str() == Some("Thread/get")
            && large["methodResponses"][1][0].as_str() == Some("error")
        {
            assert_eq!(
                large["methodResponses"][0][1]["before"].as_bool(),
                Some(true)
            );
            assert_eq!(
                large["methodResponses"][1][1]["description"].as_str(),
                Some("executionDeadline")
            );
            assert!(large["methodResponses"][1][1]["list"].is_null());
            assert_eq!(
                large["methodResponses"][2][1]["description"].as_str(),
                Some("executionDeadline")
            );
            witnessed = true;
        }
    }
    assert!(witnessed);
}

#[test]
fn unavailable_account_follows_argument_validation() {
    for (arguments, expected) in [
        (
            format!(r#"{{"accountId":"foreign","ids":["{}"]}}"#, id(THREAD)),
            "accountNotFound",
        ),
        (
            r#"{"accountId":"foreign","ids":[1]}"#.to_owned(),
            "invalidArguments",
        ),
        (
            format!(
                r#"{{"accountId":"foreign","ids":["{}"],"properties":["unknown"]}}"#,
                id(THREAD)
            ),
            "invalidArguments",
        ),
        (
            format!(r#"{{"accountId":"bad id","ids":["{}"]}}"#, id(THREAD)),
            "invalidArguments",
        ),
    ] {
        let result = execute(&request(&arguments), 2, 256, 90, false, None);
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some(expected)
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
}

#[test]
fn id_only_and_wrong_kind_ids_preserve_exact_not_found_order() {
    let missing = id(ThreadId::from_bytes([9; 16]));
    let alias = id(THREAD).to_uppercase();
    let email_id = id(email(0));
    let ids = format!(r#"["{}","{missing}","{alias}","{email_id}"]"#, id(EMPTY));
    let arguments = format!(
        r#"{{"accountId":"{}","ids":{ids},"properties":[]}}"#,
        account()
    );
    let result = execute(&request(&arguments), 2, 256, 90, false, None);
    assert_eq!(
        result["methodResponses"][1][1]["list"],
        td_json::parse("[]").unwrap()
    );
    assert_eq!(
        result["methodResponses"][1][1]["notFound"],
        td_json::parse(&ids).unwrap()
    );
    for arguments in [
        format!(r#"{{"accountId":"{}","ids":"{}"}}"#, account(), id(THREAD)),
        args(r#", "properties":"id""#),
        args(r#", "properties":[1]"#),
    ] {
        let result = execute(&request(&arguments), 2, 256, 90, false, None);
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some("invalidArguments")
        );
    }
}

#[test]
fn wildcard_reference_flattens_members_of_two_threads() {
    let _guard = FIXTURE.lock().unwrap();
    let input = batch(&format!(
        r##"[["Thread/get",{{"accountId":"{}","ids":["{}","{}"]}},"t"],["Core/echo",{{"#members":{{"resultOf":"t","name":"Thread/get","path":"/list/*/emailIds"}}}},"e"]]"##,
        account(),
        id(THREAD),
        id(EMPTY)
    ));
    let result = run_setup(&input, 0, 256, 90, false, None, |store| {
        seed(store, 2);
        let bytes = encoded(Row::Email(EmailRow {
            blob: BlobId::from_bytes([5; 16]),
            thread: EMPTY,
            received_at: 0,
            origin: EmailOrigin::Jmap,
        }));
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: AccountId::from_bytes([1; 16]),
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::put(Table::Emails, email(2).as_bytes(), &bytes).unwrap()],
                &mut [],
            )
            .unwrap();
    });
    assert_eq!(
        result["methodResponses"][0][1]["list"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        result["methodResponses"][1][1]["members"],
        td_json::parse(&format!(
            r#"["{}","{}","{}"]"#,
            id(email(0)),
            id(email(1)),
            id(email(2))
        ))
        .unwrap()
    );
}

#[test]
fn omitted_and_null_ids_enumerate_visible_threads_or_refuse_the_whole_set() {
    for ids in ["", r#", "ids":null"#] {
        let arguments = format!(r#"{{"accountId":"{}"{ids}}}"#, account());
        let result = execute(&request(&arguments), 2, 1, 90, false, None);
        assert_eq!(
            result["methodResponses"][1][1]["list"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            result["methodResponses"][1][1]["list"][0]["id"].as_str(),
            Some(id(THREAD).as_str())
        );
        assert_eq!(
            result["methodResponses"][1][1]["notFound"],
            td_json::parse("[]").unwrap()
        );
        let empty = execute(&request(&arguments), 0, 1, 90, false, None);
        assert_eq!(
            empty["methodResponses"][1][1]["list"],
            td_json::parse("[]").unwrap()
        );
        let _guard = FIXTURE.lock().unwrap();
        let result = run_setup(&request(&arguments), 0, 1, 90, false, None, |store| {
            seed(store, 2);
            let bytes = encoded(Row::Email(EmailRow {
                blob: BlobId::from_bytes([5; 16]),
                thread: EMPTY,
                received_at: 0,
                origin: EmailOrigin::Jmap,
            }));
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: AccountId::from_bytes([1; 16]),
                        epoch: store.epoch(),
                        expected: Sequence::from_u64(1),
                        utc_ms: 0,
                        deadline: deadline(),
                    },
                    &[Operation::put(Table::Emails, email(2).as_bytes(), &bytes).unwrap()],
                    &mut [],
                )
                .unwrap();
        });
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some("requestTooLarge")
        );
        assert_eq!(
            result["methodResponses"][2][1]["after"].as_bool(),
            Some(true)
        );
    }
}
