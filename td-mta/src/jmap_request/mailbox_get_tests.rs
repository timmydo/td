#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{logical::Cell as LeaseCell, DiskLimits, ViewMode, WorkLimits},
    format::{
        operation::Operation,
        row::{BlobRow, EmailOrigin, EmailRow, MailboxRow},
        Sequence,
    },
    ids::{AccountId, BlobId, EmailId, StoreEpoch, ThreadId},
    jmap_request::{dispatch, response},
    limits::Limits,
    ownership::SlotState,
    ports::{Clock, Crypto, Deadline, Digest, Tick, Time},
    store_fs::{
        tests::Fixture, AuxiliaryUsage, BlobSource, CommitRequest, IndexStore, RequestSpool,
        StoreCoordinator,
    },
    wire,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
struct Timer(AtomicU64, AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            monotonic: Tick(
                if self.0.fetch_add(1, Ordering::Relaxed) >= self.1.load(Ordering::Relaxed) {
                    90
                } else {
                    1
                },
            ),
            utc_ms: 0,
        })
    }
}
pub(crate) fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
pub(crate) fn mailbox(n: u16) -> MailboxId {
    let mut bytes = [0; 16];
    bytes[..2].copy_from_slice(&n.to_be_bytes());
    MailboxId::from_bytes(bytes)
}
pub(crate) fn id(n: u16) -> String {
    wire::encode_id(mailbox(n), &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
pub(crate) fn account() -> String {
    wire::encode_id(ACCOUNT, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
pub(crate) fn batch(calls: &str) -> String {
    format!(
        r#"{{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:mail"],"methodCalls":{calls}}}"#
    )
}
fn run(input: &str, count: u16, maximum: usize, phase: u64) -> td_json::Json {
    run_pressure(input, count, maximum, phase, false)
}
fn run_pressure(input: &str, count: u16, maximum: usize, phase: u64, busy: bool) -> td_json::Json {
    run_cutoff(input, count, maximum, phase, busy, None)
}
fn run_cutoff(
    input: &str,
    count: u16,
    maximum: usize,
    phase: u64,
    busy: bool,
    cutoff: Option<u64>,
) -> td_json::Json {
    run_setup(input, count, maximum, phase, busy, cutoff, |_| {})
}
pub(crate) fn run_setup(
    input: &str,
    count: u16,
    maximum: usize,
    phase: u64,
    busy: bool,
    cutoff: Option<u64>,
    setup: impl FnOnce(&IndexStore<'_>),
) -> td_json::Json {
    run_configured(
        input,
        count,
        Limits {
            objects_per_method: maximum,
            ..Limits::default()
        },
        phase,
        busy,
        cutoff,
        setup,
    )
}
pub(crate) fn run_configured(
    input: &str,
    count: u16,
    limits: Limits,
    phase: u64,
    busy: bool,
    cutoff: Option<u64>,
    setup: impl FnOnce(&IndexStore<'_>),
) -> td_json::Json {
    run_configured_response(input, count, (limits, 72000), phase, busy, cutoff, setup)
}
pub(crate) fn run_configured_response(
    input: &str,
    count: u16,
    profile: (Limits, u64),
    phase: u64,
    busy: bool,
    cutoff: Option<u64>,
    setup: impl FnOnce(&IndexStore<'_>),
) -> td_json::Json {
    let (limits, response_bytes) = profile;
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let files = Fixture::new();
    let mut file_root = files.locked();
    let clock = Arc::new(Timer(AtomicU64::new(0), AtomicU64::new(u64::MAX)));
    let spool = RequestSpool::open(&mut file_root, clock.clone(), deadline()).unwrap();
    let resources = Limits {
        json_bytes: 64,
        json_methods: 1,
        ..limits
    }
    .plan()
    .unwrap();
    let plan = DiskLimits {
        response_bytes,
        response_total_bytes: 2 * response_bytes,
        ..DiskLimits::default()
    }
    .plan(
        &resources,
        WorkLimits::default(),
        ViewMode::OnlineBackground,
    )
    .unwrap();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([7; 16]),
        clock.clone(),
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    if count > 0 {
        let rows: Vec<_> = (1..=count)
            .map(|n| {
                let key = *mailbox(n).as_bytes();
                let mut value = vec![0; 2048];
                let length = Row::Mailbox(MailboxRow {
                    name: if n == 1 { "Inbox" } else { "Folder" },
                    parent: None,
                    role: if n == 1 { Some("inbox") } else { None },
                    sort_order: n as u32,
                    subscribed: true,
                })
                .encode(&mut value)
                .unwrap();
                value.truncate(length);
                (key, value)
            })
            .collect();
        let operations: Vec<_> = rows
            .iter()
            .map(|(key, value)| Operation::put(Table::Mailboxes, key, value).unwrap())
            .collect();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: store.epoch(),
                    expected: Sequence::from_u64(0),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &operations,
                &mut [],
            )
            .unwrap();
    }
    if count >= 257 {
        seed_window_counts(&store);
    }
    setup(&store);
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { LeaseCell::EMPTY }; 4];
    let coordinator = StoreCoordinator::new(
        store,
        &plan,
        AuxiliaryUsage {
            sort_bytes: 0,
            response_bytes: 0,
            cache_bytes: 0,
            log_bytes: 0,
            cold_bytes: 0,
        },
        &mut states,
        &mut cells,
        (),
        deadline(),
    )
    .unwrap();
    let mut retention = coordinator.retain_response(&spool, deadline()).unwrap();
    let _views = if busy {
        Some([
            retention
                .method_view(ACCOUNT, Deadline::after(Tick(0), 90).unwrap())
                .unwrap(),
            retention
                .method_view(ACCOUNT, Deadline::after(Tick(0), 90).unwrap())
                .unwrap(),
        ])
    } else {
        None
    };
    let request = dispatch::tests::request_with_capabilities(
        input,
        limits,
        &[super::super::CORE, super::super::MAIL],
    );
    let mut preflight = response::Preflight::new(request, "session", retention.maximum()).unwrap();
    while !preflight.step(&mut [0; 4096]).unwrap() {}
    if let Some(cutoff) = cutoff {
        clock
            .1
            .store(clock.0.load(Ordering::Relaxed) + cutoff, Ordering::Relaxed);
    }
    let mut completed = dispatch::run(
        preflight.finish().unwrap(),
        retention,
        Deadline::after(Tick(0), phase).unwrap(),
        ACCOUNT,
    )
    .unwrap();
    let mut encoder = completed.encoder().unwrap();
    while !encoder.count_step(&mut [0; 4096]).unwrap() {}
    let expected = encoder.begin_output().unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0; 127];
        let next = encoder.poll_output(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..next.written]);
        if next.complete {
            break;
        }
    }
    assert_eq!(bytes.len() as u64, expected);
    td_json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
}
fn call(arguments: &str) -> String {
    batch(&format!(
        r#"[["Mailbox/get",{arguments},"get"],["Core/echo",{{"ok":true}},"last"]]"#
    ))
}
#[test]
fn all_mailboxes_properties_state_and_personal_policy() {
    for suffix in ["", ",\"ids\":null,\"properties\":null"] {
        let response = run(
            &call(&format!(r#"{{"accountId":"{}"{suffix}}}"#, account())),
            2,
            256,
            90,
        );
        let result = &response["methodResponses"][0][1];
        assert_eq!(
            response["methodResponses"][0][0].as_str(),
            Some("Mailbox/get")
        );
        assert_eq!(result["accountId"].as_str(), Some(account().as_str()));
        assert_eq!(result["list"].as_arr().unwrap().len(), 2);
        assert_eq!(result["notFound"].as_arr().unwrap().len(), 0);
        assert_eq!(result["list"][0]["id"].as_str(), Some(id(1).as_str()));
        assert_eq!(
            result["list"][0]["myRights"]["mayDelete"].as_bool(),
            Some(false)
        );
        assert_eq!(
            result["list"][1]["myRights"]["mayRename"].as_bool(),
            Some(true)
        );
        assert_eq!(result["list"][0]["totalEmails"].as_u64(), Some(0));
        let state = DataState::decode(result["state"].as_str().unwrap()).unwrap();
        assert_eq!(state.account, ACCOUNT);
        assert_eq!(state.kind, DataType::Mailbox);
        assert_eq!(state.sequence, Sequence::from_u64(1));
        assert_eq!(
            response["methodResponses"][1][1]["ok"].as_bool(),
            Some(true)
        );
    }
}
#[test]
fn ids_deduplicate_missing_and_foreign_types_and_empty_properties() {
    let response = run(
        &call(&format!(
            r#"{{"accountId":"{}","ids":["{}","missing","{}","\u006dissing","{}","{}"],"properties":[]}}"#,
            account(),
            id(2),
            id(2),
            id(99),
            account()
        )),
        2,
        256,
        90,
    );
    let result = &response["methodResponses"][0][1];
    assert_eq!(result["list"].as_arr().unwrap().len(), 1);
    assert_eq!(result["list"][0].as_obj().unwrap().len(), 1);
    assert_eq!(result["list"][0]["id"].as_str(), Some(id(2).as_str()));
    assert_eq!(result["notFound"].as_arr().unwrap().len(), 3);
    assert_eq!(result["notFound"][0].as_str(), Some("missing"));
    assert_eq!(result["notFound"][1].as_str(), Some(id(99).as_str()));
    assert_eq!(result["notFound"][2].as_str(), Some(account().as_str()));
}
#[test]
fn validation_account_opt_in_and_limits_keep_later_calls() {
    for (arguments, error) in [
        ("{}".to_owned(), "invalidArguments"),
        (r#"{"accountId":"foreign"}"#.to_owned(), "accountNotFound"),
        (
            r#"{"accountId":"foreign","properties":["bad"]}"#.to_owned(),
            "invalidArguments",
        ),
        (
            format!(r#"{{"accountId":"{}","ids":false}}"#, account()),
            "invalidArguments",
        ),
        (
            format!(r#"{{"accountId":"{}","ids":["bad id"]}}"#, account()),
            "invalidArguments",
        ),
        (
            format!(r#"{{"accountId":"{}","extra":null}}"#, account()),
            "invalidArguments",
        ),
        (
            format!(r#"{{"accountId":"{}"}}"#, account()),
            "requestTooLarge",
        ),
        (
            format!(
                r#"{{"accountId":"{}","ids":["{}","{}"]}}"#,
                account(),
                id(1),
                id(1)
            ),
            "requestTooLarge",
        ),
    ] {
        let response = run(&call(&arguments), 2, 1, 90);
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some(error),
            "{arguments}"
        );
        assert_eq!(
            response["methodResponses"][1][1]["ok"].as_bool(),
            Some(true)
        );
    }
    let input = call(&format!(r#"{{"accountId":"{}"}}"#, account()));
    let response = run(
        &input.replace(",\"urn:ietf:params:jmap:mail\"", ""),
        1,
        256,
        90,
    );
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("unknownMethod")
    );
    let response = run(&input, 1, 256, 0);
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("serverUnavailable")
    );
}
#[test]
fn result_references_and_counts_span_multiple_bounded_windows() {
    let input = batch(&format!(
        r##"[
        ["Core/echo",{{"ids":["{}","{}"]}},"ids"],
        ["Mailbox/get",{{"accountId":"{}","#ids":{{"resultOf":"ids","name":"Core/echo","path":"/ids"}},"properties":["name"]}},"get"],
        ["Core/echo",{{"#names":{{"resultOf":"get","name":"Mailbox/get","path":"/list/*/name"}}}},"names"]]"##,
        id(2),
        id(1),
        account()
    ));
    let response = run(&input, 2, 256, 90);
    assert_eq!(
        response["methodResponses"][2][1]["names"][0].as_str(),
        Some("Folder")
    );
    assert_eq!(
        response["methodResponses"][2][1]["names"][1].as_str(),
        Some("Inbox")
    );
    let response = run(
        &call(&format!(
            r#"{{"accountId":"{}","properties":["totalEmails","unreadEmails","totalThreads","unreadThreads"]}}"#,
            account()
        )),
        257,
        1024,
        90,
    );
    assert_eq!(
        response["methodResponses"][0][1]["list"]
            .as_arr()
            .unwrap()
            .len(),
        257
    );
    assert_eq!(
        response["methodResponses"][0][1]["list"][256]["totalEmails"].as_u64(),
        Some(3)
    );
    for (index, total, unread) in [(0, 1, 1), (255, 2, 0), (256, 3, 2)] {
        let item = &response["methodResponses"][0][1]["list"][index];
        assert_eq!(item["id"].as_str(), Some(id(index as u16 + 1).as_str()));
        for key in ["totalEmails", "totalThreads"] {
            assert_eq!(item[key].as_u64(), Some(total));
        }
        for key in ["unreadEmails", "unreadThreads"] {
            assert_eq!(item[key].as_u64(), Some(unread));
        }
    }
}

#[test]
fn empty_selections_and_response_capacity_never_publish_partial_lists() {
    for arguments in [
        format!(r#"{{"accountId":"{}"}}"#, account()),
        format!(r#"{{"accountId":"{}","ids":[]}}"#, account()),
    ] {
        let response = run(&call(&arguments), 0, 256, 90);
        let result = &response["methodResponses"][0][1];
        assert!(result["list"].as_arr().unwrap().is_empty());
        assert!(result["notFound"].as_arr().unwrap().is_empty());
    }
    let response = run(
        &call(&format!(r#"{{"accountId":"{}"}}"#, account())),
        257,
        1024,
        90,
    );
    assert_eq!(response["methodResponses"][0][0].as_str(), Some("error"));
    assert_eq!(
        response["methodResponses"][0][1]["description"].as_str(),
        Some("responseCapacity")
    );
    assert_eq!(
        response["methodResponses"][1][1]["ok"].as_bool(),
        Some(true)
    );
    assert!(std::mem::size_of::<Requested<MailboxId>>() <= 128);
}

#[test]
fn exhausted_read_pool_is_a_method_error_and_preserves_following_echo() {
    let response = run_pressure(
        &call(&format!(r#"{{"accountId":"{}"}}"#, account())),
        1,
        256,
        90,
        true,
    );
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("serverUnavailable")
    );
    assert_eq!(
        response["methodResponses"][0][1]["description"].as_str(),
        Some("storageBusy")
    );
    assert_eq!(
        response["methodResponses"][1][1]["ok"].as_bool(),
        Some(true)
    );
}

fn seed_window_counts(store: &IndexStore<'_>) {
    let blob = BlobId::from_bytes([90; 16]);
    let mut rows = vec![(
        Key::Blob(blob),
        Row::Blob(BlobRow {
            length: 0,
            digest: td_crypto::Provider.sha256().unwrap().finish().unwrap(),
            created_at: 0,
        }),
    )];
    let assignments = [
        (1, 1, false),
        (2, 256, true),
        (3, 256, true),
        (4, 257, true),
        (5, 257, false),
        (6, 257, false),
    ];
    for (n, box_id, seen) in assignments {
        let email = EmailId::from_bytes([n; 16]);
        let thread = ThreadId::from_bytes([n; 16]);
        rows.extend([
            (Key::Thread(thread), Row::Thread),
            (
                Key::Email(email),
                Row::Email(EmailRow {
                    blob,
                    thread,
                    received_at: 0,
                    origin: EmailOrigin::Jmap,
                }),
            ),
            (Key::Membership(email, mailbox(box_id)), Row::Membership),
        ]);
        if seen {
            rows.push((Key::Keyword(email, "$seen"), Row::Keyword));
        }
    }
    let values: Vec<_> = rows
        .iter()
        .map(|(key, row)| {
            let mut k = vec![0; format::MAX_KEY_BYTES];
            let n = key.encode(&mut k).unwrap();
            k.truncate(n);
            let mut v = vec![0; format::MAX_VALUE_BYTES];
            let n = row.encode(&mut v).unwrap();
            v.truncate(n);
            (key.table(), k, v)
        })
        .collect();
    let ops: Vec<_> = values
        .iter()
        .map(|(table, key, value)| Operation::put(*table, key, value).unwrap())
        .collect();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(1),
                utc_ms: 0,
                deadline: deadline(),
            },
            &ops,
            &mut [BlobSource {
                id: blob,
                source: &mut b"".as_slice(),
            }],
        )
        .unwrap();
}
#[test]
fn argument_shapes_escaped_ids_core_independence_and_exact_rights() {
    for tail in [
        r#", "properties":false"#,
        r#", "properties":[null]"#,
        r#", "ids":[1]"#,
        r#", "properties":["unknown"]"#,
        r##", "ids":[],"#ids":{"resultOf":"absent","name":"Core/echo","path":"/ids"}"##,
    ] {
        let response = run(
            &call(&format!(r#"{{"accountId":"{}"{tail}}}"#, account())),
            2,
            256,
            90,
        );
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some("invalidArguments")
        );
    }
    for value in ["null", "1", "true"] {
        let response = run(&call(&format!(r#"{{"accountId":{value}}}"#)), 2, 256, 90);
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some("invalidArguments")
        );
    }
    let typed = id(1);
    let escaped = typed.replacen('m', r"\u006d", 1);
    let response = run(
        &call(&format!(
            r#"{{"accountId":"{}","ids":["\u006dissing","missing","{escaped}","{typed}"],"properties":["name","name"]}}"#,
            account()
        )),
        2,
        256,
        90,
    );
    assert_eq!(
        response["methodResponses"][0][1]["list"]
            .as_arr()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        response["methodResponses"][0][1]["notFound"]
            .as_arr()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        response["methodResponses"][0][1]["notFound"][0].as_str(),
        Some("missing")
    );
    let response = run(
        &call(&format!(r#"{{"accountId":"{}"}}"#, account()))
            .replace("\"urn:ietf:params:jmap:core\",", ""),
        2,
        256,
        90,
    );
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Mailbox/get")
    );
    assert_eq!(
        response["methodResponses"][1][1]["type"].as_str(),
        Some("unknownMethod")
    );
    for index in 0..2 {
        let object = &response["methodResponses"][0][1]["list"][index];
        assert_eq!(object.as_obj().unwrap().len(), 11);
        for key in [
            "mayReadItems",
            "mayAddItems",
            "mayRemoveItems",
            "maySetSeen",
            "maySetKeywords",
            "mayCreateChild",
            "mayRename",
            "mayDelete",
            "maySubmit",
        ] {
            assert_eq!(
                object["myRights"][key].as_bool(),
                Some(index != 0 || !matches!(key, "mayRename" | "mayDelete"))
            );
        }
    }
}
#[test]
fn method_failure_preserves_earlier_reply_and_storage_errors_are_distinct() {
    let input = batch(&format!(
        r#"[["Core/echo",{{"before":1}},"first"],["Mailbox/get",{{"accountId":"{}"}},"get"],["Core/echo",{{"after":2}},"last"]]"#,
        account()
    ));
    let response = run(&input, 2, 1, 90);
    assert_eq!(
        response["methodResponses"][0][1]["before"].as_u64(),
        Some(1)
    );
    assert_eq!(
        response["methodResponses"][1][1]["type"].as_str(),
        Some("requestTooLarge")
    );
    assert_eq!(response["methodResponses"][2][1]["after"].as_u64(), Some(2));
    assert_eq!(storage(ports::Error::Capacity), Error::StorageCapacity);
    assert_eq!(storage(ports::Error::Deadline), Error::ExecutionDeadline);
    assert_eq!(
        storage(ports::Error::Corrupt),
        Error::Storage(ports::Error::Corrupt)
    );
    let response = run(
        &call(&format!(r#"{{"accountId":"{}"}}"#, account())),
        2,
        256,
        0,
    );
    assert_eq!(
        response["methodResponses"][0][1]["description"].as_str(),
        Some("executionDeadline")
    );
}

#[test]
fn phase_cutoffs_never_publish_a_partial_mailbox_result() {
    let input = batch(&format!(
        r#"[["Core/echo",{{"before":1}},"first"],["Mailbox/get",{{"accountId":"{}"}},"get"]]"#,
        account()
    ));
    let mut failures = 0;
    let mut successes = 0;
    let mut preserved = 0;
    for cutoff in [32, 256, 2048, 16384, 131072] {
        let response = run_cutoff(&input, 16, 256, 90, false, Some(cutoff));
        let before = &response["methodResponses"][0];
        let before_succeeded = before[0].as_str() == Some("Core/echo");
        if before_succeeded {
            assert_eq!(before[1]["before"].as_u64(), Some(1));
        } else {
            assert_eq!(before[1]["description"].as_str(), Some("executionDeadline"));
        }
        let method = &response["methodResponses"][1];
        if method[0].as_str() == Some("error") {
            failures += 1;
            preserved += usize::from(before_succeeded);
            assert_eq!(method[1]["type"].as_str(), Some("serverUnavailable"));
            assert_eq!(method[1]["description"].as_str(), Some("executionDeadline"));
        } else {
            successes += 1;
            assert_eq!(method[1]["list"].as_arr().unwrap().len(), 16);
        }
    }
    assert!(failures > 0 && successes > 0 && preserved > 0);
}
#[test]
fn core_only_admission_refuses_mail_and_local_ids_require_canonical_hex() {
    let mut input = batch("[]").into_bytes();
    let limits = Limits::default();
    let mut tokens = vec![td_json::arena::Token::EMPTY; limits.json_tokens];
    let mut validation = crate::jmap_request::Validation::new(
        &mut input,
        &mut tokens,
        &limits.plan().unwrap(),
        &[super::super::CORE],
    )
    .unwrap();
    let result = loop {
        match validation.step() {
            Ok(false) => (),
            result => break result,
        }
    };
    assert_eq!(result, Err(crate::jmap_request::Error::UnknownCapability));
    let canonical = wire::encode_id(
        MailboxId::from_bytes([0xab; 16]),
        &mut [0; wire::IdKind::WIRE_BYTES],
    )
    .unwrap()
    .to_owned();
    assert!(wire::Id::parse(&canonical)
        .unwrap()
        .local::<MailboxId>()
        .is_some());
    assert!(
        wire::Id::parse(&format!("m{}", canonical.get(1..).unwrap().to_uppercase()))
            .unwrap()
            .local::<MailboxId>()
            .is_none()
    );
}

#[test]
fn default_counts_with_only_foreign_ids_has_no_local_count_batch() {
    let response = run(
        &call(&format!(
            r#"{{"accountId":"{}","ids":["missing","{}"]}}"#,
            account(),
            account()
        )),
        1,
        256,
        90,
    );
    assert!(response["methodResponses"][0][1]["list"]
        .as_arr()
        .unwrap()
        .is_empty());
    assert_eq!(
        response["methodResponses"][0][1]["notFound"]
            .as_arr()
            .unwrap()
            .len(),
        2
    );
}
