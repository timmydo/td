#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, deadline, id, mailbox, run_setup};
use super::*;
use crate::{
    format::{
        operation::Operation,
        row::{MailboxRow, Row},
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, StoreEpoch},
    store_fs::{CommitRequest, IndexStore},
};
fn state(sequence: u64) -> String {
    DataState {
        account: AccountId::from_bytes([1; 16]),
        epoch: StoreEpoch::from_bytes([7; 16]),
        kind: DataType::Mailbox,
        sequence: Sequence::from_u64(sequence),
    }
    .encode(&mut [0; DataState::WIRE_BYTES])
    .unwrap()
    .to_owned()
}
fn commit(store: &IndexStore<'_>, expected: u64, changes: &[(u16, ChangeAction)]) {
    let mut bytes = [0; 128];
    let length = Row::Mailbox(MailboxRow {
        name: "Folder",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    })
    .encode(&mut bytes)
    .unwrap();
    let ids: Vec<_> = changes.iter().map(|(n, _)| mailbox(*n)).collect();
    let mut ops = Vec::new();
    for (index, (_, action)) in changes.iter().enumerate() {
        ops.push(if *action == ChangeAction::Destroyed {
            Operation::delete(Table::Mailboxes, ids[index].as_bytes()).unwrap()
        } else {
            Operation::put(Table::Mailboxes, ids[index].as_bytes(), &bytes[..length]).unwrap()
        });
        ops.push(Operation::change(
            ObjectType::Mailbox,
            *action,
            ids[index].as_bytes(),
        ));
    }
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: AccountId::from_bytes([1; 16]),
                epoch: store.epoch(),
                expected: Sequence::from_u64(expected),
                utc_ms: 0,
                deadline: deadline(),
            },
            &ops,
            &mut [],
        )
        .unwrap();
}
fn history(store: &IndexStore<'_>) {
    use ChangeAction::*;
    commit(store, 0, &[(1, Created), (2, Created)]);
    commit(store, 1, &[(1, Updated)]);
    commit(store, 2, &[(3, Created)]);
    commit(store, 3, &[(2, Destroyed)]);
}
fn request(arguments: &str) -> String {
    batch(&format!(
        r#"[["Core/echo",{{"before":true}},"before"],["Mailbox/changes",{arguments},"changes"],["Core/echo",{{"after":true}},"after"]]"#
    ))
}
fn execute(arguments: &str) -> td_json::Json {
    run_setup(&request(arguments), 0, 256, 90, false, None, history)
}
#[test]
fn full_changes_and_transaction_pages_are_real_wire_responses() {
    let response = execute(&format!(
        r#"{{"accountId":"{}","sinceState":"{}"}}"#,
        account(),
        state(0)
    ));
    let calls = &response["methodResponses"];
    assert_eq!(calls[0][1]["before"].as_bool(), Some(true));
    assert_eq!(calls[2][1]["after"].as_bool(), Some(true));
    let result = &calls[1][1];
    assert_eq!(calls[1][0].as_str(), Some("Mailbox/changes"));
    assert_eq!(result["accountId"].as_str(), Some(account().as_str()));
    assert_eq!(result["oldState"].as_str(), Some(state(0).as_str()));
    assert_eq!(result["newState"].as_str(), Some(state(4).as_str()));
    assert_eq!(result["hasMoreChanges"].as_bool(), Some(false));
    assert_eq!(
        result["created"],
        td_json::parse(&format!(r#"["{}","{}"]"#, id(1), id(3))).unwrap()
    );
    assert_eq!(result["updated"], td_json::parse("[]").unwrap());
    assert_eq!(result["destroyed"], td_json::parse("[]").unwrap());
    assert!(result["updatedProperties"].is_null());
    let first = execute(&format!(
        r#"{{"accountId":"{}","sinceState":"{}","maxChanges":2}}"#,
        account(),
        state(0)
    ));
    assert_eq!(
        first["methodResponses"][1][1]["newState"].as_str(),
        Some(state(2).as_str())
    );
    assert_eq!(
        first["methodResponses"][1][1]["hasMoreChanges"].as_bool(),
        Some(true)
    );
    let next = execute(&format!(
        r#"{{"accountId":"{}","sinceState":"{}","maxChanges":2}}"#,
        account(),
        state(2)
    ));
    let result = &next["methodResponses"][1][1];
    assert_eq!(
        result["created"],
        td_json::parse(&format!(r#"["{}"]"#, id(3))).unwrap()
    );
    assert_eq!(
        result["destroyed"],
        td_json::parse(&format!(r#"["{}"]"#, id(2))).unwrap()
    );
    assert_eq!(result["newState"].as_str(), Some(state(4).as_str()));
}
#[test]
fn invalid_arguments_scope_and_nonrepresentable_first_transaction() {
    let a = account();
    let s = state(0);
    for args in [
        "{}".into(),
        format!(r#"{{"accountId":"{a}"}}"#),
        format!(r#"{{"accountId":"{a}","sinceState":null}}"#),
        format!(r#"{{"accountId":"{a}","sinceState":1}}"#),
        format!(r#"{{"accountId":"{a}","sinceState":"{s}","extra":true}}"#),
    ] {
        assert_eq!(
            execute(&args)["methodResponses"][1][1]["type"].as_str(),
            Some("invalidArguments")
        );
    }
    for limit in [
        "0",
        "-1",
        "0.1",
        "1.00000000000000001",
        "9007199254740992",
        "1e1000000",
        "\"2\"",
        "true",
        "[]",
    ] {
        let args = format!(r#"{{"accountId":"{a}","sinceState":"{s}","maxChanges":{limit}}}"#);
        assert_eq!(
            execute(&args)["methodResponses"][1][1]["type"].as_str(),
            Some("invalidArguments"),
            "{limit}"
        );
    }
    for since in [
        "".into(),
        "foreign".into(),
        "x".repeat(2000),
        state(5),
        state(0).replace("d1_01", "d1_03"),
    ] {
        let args = format!(r#"{{"accountId":"{a}","sinceState":"{since}"}}"#);
        assert_eq!(
            execute(&args)["methodResponses"][1][1]["type"].as_str(),
            Some("cannotCalculateChanges")
        );
    }
    let args = format!(r#"{{"accountId":"{a}","sinceState":"{s}","maxChanges":1}}"#);
    assert_eq!(
        execute(&args)["methodResponses"][1][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
    assert_eq!(
        execute(r#"{"accountId":"foreign","sinceState":"unknown"}"#)["methodResponses"][1][1]
            ["type"]
            .as_str(),
        Some("accountNotFound")
    );
}
#[test]
fn exact_numeric_spellings_null_and_clamping() {
    for limit in [
        "null",
        "256",
        "256.0",
        "2.56e2",
        "25600e-2",
        "9007199254740991",
    ] {
        let result = execute(&format!(
            r#"{{"accountId":"{}","sinceState":"{}","maxChanges":{limit}}}"#,
            account(),
            state(0)
        ));
        assert_eq!(
            result["methodResponses"][1][0].as_str(),
            Some("Mailbox/changes"),
            "{limit}"
        );
        assert_eq!(
            result["methodResponses"][1][1]["hasMoreChanges"].as_bool(),
            Some(false)
        );
    }
    for (text, expected) in [
        ("0", Some(0)),
        ("-0.0e999", Some(0)),
        ("0.001e3", Some(1)),
        ("10e-1", Some(1)),
        ("100000000000000000000e-20", Some(1)),
        ("1.00000000000000001", None),
        ("9007199254740991", Some(9007199254740991)),
        ("9007199254740992", None),
    ] {
        let mut number = Unsigned::default();
        for byte in text.bytes() {
            number.byte(byte);
        }
        assert_eq!(number.finish(), expected, "{text}");
    }
}
#[test]
fn changes_references_feed_get_and_echo_and_obey_capability_gates() {
    let a = account();
    let s = state(1);
    let input = batch(&format!(
        r##"[
        ["Core/echo",{{"state":"{s}","limit":2}},"first"],
        ["Mailbox/changes",{{"accountId":"{a}","#sinceState":{{"resultOf":"first","name":"Core/echo","path":"/state"}},"#maxChanges":{{"resultOf":"first","name":"Core/echo","path":"/limit"}}}},"changes"],
        ["Mailbox/get",{{"accountId":"{a}","#ids":{{"resultOf":"changes","name":"Mailbox/changes","path":"/updated"}},"properties":["id"]}},"get"],
        ["Core/echo",{{"#state":{{"resultOf":"changes","name":"Mailbox/changes","path":"/newState"}}}},"last"]]"##
    ));
    let result = run_setup(&input, 0, 256, 90, false, None, history);
    assert_eq!(
        result["methodResponses"][2][1]["list"][0]["id"].as_str(),
        Some(id(1).as_str())
    );
    assert_eq!(
        result["methodResponses"][3][1]["state"].as_str(),
        Some(state(3).as_str())
    );
    let input = request(&format!(r#"{{"accountId":"{a}","sinceState":"{s}"}}"#));
    let core_only = input.replace(",\"urn:ietf:params:jmap:mail\"", "");
    let result = run_setup(&core_only, 0, 256, 90, false, None, history);
    assert_eq!(
        result["methodResponses"][1][1]["type"].as_str(),
        Some("unknownMethod")
    );
    let mail_only = input.replace("\"urn:ietf:params:jmap:core\",", "");
    let result = run_setup(&mail_only, 0, 256, 90, false, None, history);
    assert_eq!(
        result["methodResponses"][1][0].as_str(),
        Some("Mailbox/changes")
    );
}

#[test]
fn empty_expired_busy_and_retired_history_have_distinct_results() {
    let args = format!(
        r#"{{"accountId":"{}","sinceState":"{}"}}"#,
        account(),
        state(0)
    );
    let input = request(&args);
    let empty = run_setup(&input, 0, 256, 90, false, None, |_| {});
    assert_eq!(
        empty["methodResponses"][1][1]["newState"].as_str(),
        Some(state(0).as_str())
    );
    assert_eq!(
        empty["methodResponses"][1][1]["created"],
        td_json::parse("[]").unwrap()
    );
    let busy = run_setup(&input, 0, 256, 90, true, None, history);
    assert_eq!(
        busy["methodResponses"][1][1]["description"].as_str(),
        Some("storageBusy")
    );
    assert_eq!(busy["methodResponses"][2][1]["after"].as_bool(), Some(true));
    let expired = run_setup(&input, 0, 256, 0, false, None, history);
    assert_eq!(
        expired["methodResponses"][1][1]["description"].as_str(),
        Some("executionDeadline")
    );
    let retired = run_setup(&input, 0, 256, 90, false, None, |store| {
        history(store);
        store
            .prune_history(crate::store_fs::HistoryPruneRequest {
                account: AccountId::from_bytes([1; 16]),
                epoch: store.epoch(),
                expected: Sequence::from_u64(4),
                through: Sequence::from_u64(1),
                max_rows: 1,
                deadline: deadline(),
            })
            .unwrap();
    });
    assert_eq!(
        retired["methodResponses"][1][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
    assert_eq!(
        retired["methodResponses"][2][1]["after"].as_bool(),
        Some(true)
    );
}

#[test]
fn escaped_states_and_numeric_chunks_preserve_exact_values() {
    let escaped = state(0)
        .bytes()
        .map(|b| format!("\\u{b:04x}"))
        .collect::<String>();
    let long_number = format!("2{}e-{}", "0".repeat(1024), 1024);
    let args = format!(
        r#"{{"accountId":"{}","sinceState":"{escaped}","maxChanges":{long_number}}}"#,
        account()
    );
    let result = execute(&args);
    assert_eq!(
        result["methodResponses"][1][1]["oldState"].as_str(),
        Some(state(0).as_str())
    );
    assert_eq!(
        result["methodResponses"][1][1]["newState"].as_str(),
        Some(state(2).as_str())
    );
    let input = request(&format!(
        r#"{{"accountId":"{}","sinceState":"{}"}}"#,
        account(),
        state(0)
    ));
    let mut failed_after_echo = false;
    let mut succeeded = false;
    for cutoff in [64, 256, 1024, 4096, 16384] {
        let result = run_setup(&input, 0, 256, 90, false, Some(cutoff), history);
        let calls = &result["methodResponses"];
        if calls[1][0].as_str() == Some("Mailbox/changes") {
            succeeded = true;
        } else {
            assert_eq!(
                calls[1][1]["description"].as_str(),
                Some("executionDeadline")
            );
            failed_after_echo |= calls[0][1]["before"].as_bool() == Some(true);
        }
    }
    assert!(failed_after_echo && succeeded);
}

#[test]
fn change_response_capacity_preserves_preceding_and_following_replies() {
    let padding = "x".repeat(45000);
    let input = batch(&format!(
        r#"[["Core/echo",{{"padding":"{padding}"}},"before"],["Mailbox/changes",{{"accountId":"{}","sinceState":"{}"}},"changes"],["Core/echo",{{"after":true}},"after"]]"#,
        account(),
        state(0)
    ));
    let result = run_setup(&input, 0, 1024, 90, false, None, |store| {
        let changes: Vec<_> = (1..=1024).map(|n| (n, ChangeAction::Created)).collect();
        commit(store, 0, &changes);
    });
    assert_eq!(
        result["methodResponses"][0][1]["padding"].as_str(),
        Some(padding.as_str())
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

#[test]
fn configured_limit_actually_clamps_large_null_and_omitted_values() {
    for field in [
        String::new(),
        ",\"maxChanges\":null".into(),
        ",\"maxChanges\":9007199254740991".into(),
    ] {
        let input = request(&format!(
            r#"{{"accountId":"{}","sinceState":"{}"{field}}}"#,
            account(),
            state(0)
        ));
        let result = run_setup(&input, 0, 2, 90, false, None, history);
        assert_eq!(
            result["methodResponses"][1][1]["newState"].as_str(),
            Some(state(2).as_str())
        );
        assert_eq!(
            result["methodResponses"][1][1]["hasMoreChanges"].as_bool(),
            Some(true)
        );
        let result = run_setup(&input, 0, 1, 90, false, None, history);
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some("cannotCalculateChanges")
        );
    }
}

#[test]
fn argument_shapes_precede_account_availability_and_bad_unicode_never_reaches_dispatch() {
    for account in ["foreign", "bad id"] {
        for args in [
            format!(r#"{{"accountId":"{account}","sinceState":1}}"#),
            format!(r#"{{"accountId":"{account}","sinceState":"unknown","maxChanges":0}}"#),
            format!(r#"{{"accountId":"{account}","sinceState":"unknown","maxChanges":"2"}}"#),
        ] {
            assert_eq!(
                execute(&args)["methodResponses"][1][1]["type"].as_str(),
                Some("invalidArguments")
            );
        }
    }
    assert_eq!(
        execute(r#"{"accountId":null,"sinceState":"unknown"}"#)["methodResponses"][1][1]["type"]
            .as_str(),
        Some("invalidArguments")
    );
    for value in [r#""\ud800""#, r#""\udc00""#, r#""\ud800\u0041""#] {
        let mut input = request(&format!(
            r#"{{"accountId":"{}","sinceState":{value}}}"#,
            account()
        ))
        .into_bytes();
        let limits = crate::limits::Limits::default();
        let plan = limits.plan().unwrap();
        let mut tokens = vec![arena::Token::EMPTY; limits.json_tokens];
        let result = crate::jmap_request::Request::parse(
            &mut input,
            &mut tokens,
            &plan,
            &[crate::jmap_request::CORE, super::super::MAIL],
        );
        assert!(
            matches!(result, Err(crate::jmap_request::Error::NotJson)),
            "{value}"
        );
    }
    for value in [r#""\ud83d\ude00""#, r#""\u0000""#, r#""é""#] {
        assert_eq!(
            execute(&format!(
                r#"{{"accountId":"{}","sinceState":{value}}}"#,
                account()
            ))["methodResponses"][1][1]["type"]
                .as_str(),
            Some("cannotCalculateChanges")
        );
    }
}

#[test]
fn exponent_saturation_and_sign_variants_remain_exact() {
    for (value, expected) in [
        ("1e99999999999", None),
        ("100000000000000000000e-99999999999", None),
        ("2.56e+2", Some(256)),
        ("2.56E2", Some(256)),
        ("0e-99999999999", Some(0)),
    ] {
        let mut number = Unsigned::default();
        for byte in value.bytes() {
            number.byte(byte);
        }
        assert_eq!(number.finish(), expected, "{value}");
    }
    for value in ["2.56e+2", "2.56E2"] {
        let result = execute(&format!(
            r#"{{"accountId":"{}","sinceState":"{}","maxChanges":{value}}}"#,
            account(),
            state(0)
        ));
        assert_eq!(
            result["methodResponses"][1][0].as_str(),
            Some("Mailbox/changes")
        );
    }
}

#[test]
fn history_dependent_deadline_work_refuses_whole_method_and_marks_later_call() {
    let input = request(&format!(
        r#"{{"accountId":"{}","sinceState":"{}"}}"#,
        account(),
        state(0)
    ));
    let mut observed = false;
    for cutoff in [512, 1024, 2048, 4096, 8192, 16384] {
        let small = run_setup(&input, 0, 1024, 90, false, Some(cutoff), |store| {
            commit(store, 0, &[(1, ChangeAction::Created)])
        });
        let big = run_setup(&input, 0, 1024, 90, false, Some(cutoff), |store| {
            let changes: Vec<_> = (1..=512).map(|n| (n, ChangeAction::Created)).collect();
            commit(store, 0, &changes);
        });
        if small["methodResponses"][1][0].as_str() == Some("Mailbox/changes")
            && big["methodResponses"][1][0].as_str() == Some("error")
        {
            observed = true;
            assert_eq!(big["methodResponses"][0][1]["before"].as_bool(), Some(true));
            assert_eq!(
                big["methodResponses"][1][1]["description"].as_str(),
                Some("executionDeadline")
            );
            assert_eq!(
                big["methodResponses"][2][1]["description"].as_str(),
                Some("executionDeadline")
            );
        }
    }
    assert!(
        observed,
        "history work never separated from fixed argument parsing"
    );
}
