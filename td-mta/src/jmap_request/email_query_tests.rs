#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::{
    mailbox_get::tests::{account, batch, deadline, mailbox, run_setup},
    thread_get::tests::{date, email, id, seed},
};
use super::*;
use crate::{
    format::{operation::Operation, row::MailboxRow, Sequence},
    ids::AccountId,
    store_fs::CommitRequest,
};
use std::sync::Mutex;
static FIXTURE: Mutex<()> = Mutex::new(());
fn execute(extra: &str, count: u16, busy: bool, cutoff: Option<u64>) -> td_json::Json {
    let _guard = FIXTURE.lock().unwrap();
    let input = batch(&format!(
        r#"[["Email/query",{{"accountId":"{}"{extra}}},"q"],["Core/echo",{{"after":true}},"a"]]"#,
        account()
    ));
    run_setup(&input, 0, 256, 90, busy, cutoff, |store| {
        seed(store, count);
        let mut row = vec![0; 2048];
        let len = Row::Mailbox(MailboxRow {
            name: "Inbox",
            parent: None,
            role: Some("inbox"),
            sort_order: 0,
            subscribed: true,
        })
        .encode(&mut row)
        .unwrap();
        row.truncate(len);
        let keys: Vec<_> = (0..count)
            .filter(|n| n % 2 == 0)
            .map(|n| {
                let mut key = [0; 32];
                Key::Membership(email(n), mailbox(1))
                    .encode(&mut key)
                    .unwrap();
                key
            })
            .collect();
        let mailbox = mailbox(1);
        let mut ops = vec![Operation::put(Table::Mailboxes, mailbox.as_bytes(), &row).unwrap()];
        ops.extend(
            keys.iter()
                .map(|key| Operation::put(Table::Memberships, key, &[]).unwrap()),
        );
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
                &ops,
                &mut [],
            )
            .unwrap();
    })
}
fn ids(response: &td_json::Json) -> Vec<String> {
    response["methodResponses"][0][1]["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}
#[test]
fn client_mailbox_sort_and_multiple_pages_use_one_stable_order() {
    for ascending in [false, true] {
        let mut expected: Vec<_> = (0..600).filter(|n| n % 2 == 0).collect();
        expected.sort_by(|a, b| {
            let order = date(*a).cmp(&date(*b));
            (if ascending { order } else { order.reverse() })
                .then_with(|| email(*a).cmp(&email(*b)))
        });
        let mut actual = Vec::new();
        for position in [0, 256, 300] {
            let result = execute(
                &format!(
                    r#", "filter":{{"inMailbox":"{}"}},"sort":[{{"property":"receivedAt","isAscending":{ascending}}}],"collapseThreads":false,"position":{position},"limit":256,"calculateTotal":true"#,
                    id(mailbox(1))
                ),
                600,
                false,
                None,
            );
            let value = &result["methodResponses"][0][1];
            assert_eq!(value["position"].as_u64(), Some(position));
            assert_eq!(value["total"].as_u64(), Some(300));
            assert_eq!(value["canCalculateChanges"].as_bool(), Some(false));
            let state = QueryState::decode(value["queryState"].as_str().unwrap()).unwrap();
            assert_eq!(state.kind, QueryType::Email);
            actual.extend(ids(&result));
            assert_eq!(
                result["methodResponses"][1][1]["after"].as_bool(),
                Some(true)
            );
        }
        assert_eq!(
            actual,
            expected
                .into_iter()
                .map(|n| id(email(n)))
                .collect::<Vec<_>>()
        );
    }
}
#[test]
fn empty_zero_limit_and_bounded_large_limit_do_not_fabricate_total() {
    for (extra, count, total, length) in [
        (String::from(",\"calculateTotal\":true"), 0, 0, 0),
        (
            String::from(",\"calculateTotal\":true,\"limit\":0"),
            10,
            10,
            0,
        ),
        (
            String::from(",\"calculateTotal\":true,\"limit\":500"),
            300,
            300,
            256,
        ),
        (
            String::from(",\"calculateTotal\":true,\"filter\":{\"inMailbox\":\"foreign\"}"),
            10,
            0,
            0,
        ),
        (
            format!(
                r#", "calculateTotal":true,"filter":{{"inMailbox":"{}"}}"#,
                id(mailbox(2))
            ),
            10,
            0,
            0,
        ),
    ] {
        let result = execute(&extra, count, false, None);
        assert_eq!(ids(&result).len(), length);
        assert_eq!(
            result["methodResponses"][0][1]["total"].as_u64(),
            Some(total)
        );
    }
}
#[test]
fn unsupported_or_malformed_queries_refuse_before_empty_account_success() {
    for (extra, error) in [
        (r#", "filter":{"text":"later"}"#, "unsupportedFilter"),
        (r#", "filter":{"inThread":"foreign"}"#, "unsupportedFilter"),
        (r#", "sort":[{"property":"subject"}]"#, "unsupportedSort"),
        (
            r#", "sort":[{"property":"receivedAt","collation":3}]"#,
            "invalidArguments",
        ),
        (
            r#", "sort":[{"property":"receivedAt"},{"property":"receivedAt"}]"#,
            "unsupportedSort",
        ),
        (r#", "position":-1"#, "invalidArguments"),
        (r#", "limit":-1"#, "invalidArguments"),
        (r#", "collapseThreads":true"#, "invalidArguments"),
        (r#", "filter":[]"#, "invalidArguments"),
    ] {
        let result = execute(extra, 0, false, None);
        assert_eq!(result["methodResponses"][0][0].as_str(), Some("error"));
        assert_eq!(
            result["methodResponses"][0][1]["type"].as_str(),
            Some(error)
        );
        assert_eq!(
            result["methodResponses"][1][1]["after"].as_bool(),
            Some(true)
        );
    }
}
#[test]
fn view_pressure_and_execution_expiry_preserve_ordered_method_results() {
    let result = execute("", 3, true, None);
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("serverUnavailable")
    );
    assert_eq!(
        result["methodResponses"][0][1]["description"].as_str(),
        Some("storageBusy")
    );
    assert_eq!(result["methodResponses"][0][0].as_str(), Some("error"));
    assert_eq!(
        result["methodResponses"][1][1]["after"].as_bool(),
        Some(true)
    );
    for cutoff in [80, 500, 1500] {
        let result = execute("", 300, false, Some(cutoff));
        for index in 0..2 {
            assert_eq!(result["methodResponses"][index][0].as_str(), Some("error"));
            assert_eq!(
                result["methodResponses"][index][1]["type"].as_str(),
                Some("serverUnavailable")
            );
            assert_eq!(
                result["methodResponses"][index][1]["description"].as_str(),
                Some("executionDeadline")
            );
            assert!(result["methodResponses"][index][1]
                .as_object()
                .unwrap()
                .iter()
                .all(|(k, _)| k != "ids" && k != "total"));
        }
    }
}

#[test]
fn result_references_use_the_retained_page_and_foreign_accounts_refuse() {
    let _guard = FIXTURE.lock().unwrap();
    let input = batch(&format!(
        r##"[["Core/echo",{{"filter":{{}}}},"a"],["Email/query",{{"accountId":"{}","#filter":{{"resultOf":"a","name":"Core/echo","path":"/filter"}},"limit":2}},"q"],["Core/echo",{{"#ids":{{"resultOf":"q","name":"Email/query","path":"/ids"}}}},"b"]]"##,
        account()
    ));
    let response = run_setup(&input, 0, 256, 90, false, None, |store| seed(store, 3));
    assert_eq!(
        response["methodResponses"][1][0].as_str(),
        Some("Email/query")
    );
    assert_eq!(
        response["methodResponses"][1][1]["ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        response["methodResponses"][1][1]["ids"],
        response["methodResponses"][2][1]["ids"]
    );
    let response = run_setup(
        &batch(r#"[["Email/query",{"accountId":"foreign"},"q"]]"#),
        0,
        256,
        90,
        false,
        None,
        |_| {},
    );
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("accountNotFound")
    );
}

#[test]
fn date_sort_ignores_string_collation() {
    let expected = execute(
        r#", "sort":[{"property":"receivedAt","isAscending":true}]"#,
        5,
        false,
        None,
    );
    let actual = execute(
        r#", "sort":[{"property":"receivedAt","isAscending":true,"collation":"unused"}]"#,
        5,
        false,
        None,
    );
    assert_eq!(ids(&actual), ids(&expected));
    let actual = execute(
        &format!(
            r#", "sort":[{{"property":"receivedAt","isAscending":true,"collation":"{}"}}]"#,
            "x".repeat(2000)
        ),
        5,
        false,
        None,
    );
    assert_eq!(ids(&actual), ids(&expected));
}

#[test]
fn client_request_defaults_and_server_limit_fields_are_exact() {
    let mut descending: Vec<_> = (0..5).collect();
    descending.sort_by(|a, b| {
        date(*b)
            .cmp(&date(*a))
            .then_with(|| email(*a).cmp(&email(*b)))
    });
    let descending: Vec<_> = descending.into_iter().map(|n| id(email(n))).collect();
    for extra in [
        "",
        r#", "filter":null,"sort":null"#,
        r#", "filter":{},"sort":[]"#,
    ] {
        let result = execute(extra, 5, false, None);
        assert_eq!(ids(&result), descending);
        assert_eq!(result["methodResponses"][0][1]["limit"].as_u64(), Some(256));
        assert!(result["methodResponses"][0][1].get("total").is_none());
    }
    let result = execute(
        &format!(
            r#", "filter":{{"inMailbox":"{}"}},"sort":[{{"property":"receivedAt","isAscending":false}}],"collapseThreads":false,"position":0,"limit":50"#,
            id(mailbox(1))
        ),
        5,
        false,
        None,
    );
    let expected: Vec<_> = descending
        .into_iter()
        .filter(|id_| (0..5).filter(|n| n % 2 == 0).any(|n| id(email(n)) == *id_))
        .collect();
    assert_eq!(ids(&result), expected);
    assert!(result["methodResponses"][0][1].get("total").is_none());
    assert!(result["methodResponses"][0][1].get("limit").is_none());
    let result = execute(r#", "limit":500"#, 3, false, None);
    assert_eq!(result["methodResponses"][0][1]["limit"].as_u64(), Some(256));
    let result = execute(r#", "limit":0,"position":3"#, 5, false, None);
    assert!(ids(&result).is_empty());
    assert_eq!(
        result["methodResponses"][0][1]["position"].as_u64(),
        Some(3)
    );
    let result = execute(r#", "sort":[{"property":"receivedAt"}]"#, 5, false, None);
    let mut expected: Vec<_> = (0..5).collect();
    expected.sort_by_key(|n| (date(*n), email(*n)));
    assert_eq!(
        ids(&result),
        expected
            .into_iter()
            .map(|n| id(email(n)))
            .collect::<Vec<_>>()
    );
    let result = execute(
        &format!(r#", "sort":[{{"property":"{}"}}]"#, "x".repeat(257)),
        0,
        false,
        None,
    );
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("unsupportedSort")
    );
}
