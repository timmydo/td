#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, deadline, id, mailbox, run_setup};
use super::*;
use crate::{
    format::{operation::Operation, Sequence},
    ids::AccountId,
    store_fs::{CommitRequest, IndexStore},
};
use std::sync::Mutex;
static FIXTURE: Mutex<()> = Mutex::new(());
fn call(extra: &str) -> String {
    batch(&format!(
        r#"[["Mailbox/query",{{"accountId":"{}"{extra}}},"q"],["Core/echo",{{"ok":true}},"after"]]"#,
        account()
    ))
}
fn run(extra: &str, count: u16) -> td_json::Json {
    run_setup(&call(extra), count, 256, 90, false, None, |_| {})
}
fn ids(result: &td_json::Json) -> Vec<String> {
    result["methodResponses"][0][1]["ids"]
        .as_arr()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect()
}
fn seed_tree(store: &IndexStore<'_>) {
    let rows: Vec<_> = [
        (1, "z-root", None, false),
        (2, "a-child", Some(1), true),
        (3, "b-child", Some(1), true),
        (4, "a-root", None, true),
        (5, "z-leaf", Some(2), true),
    ]
    .into_iter()
    .map(|(n, name, parent, subscribed)| {
        let mut value = vec![0; 2048];
        let length = Row::Mailbox(MailboxRow {
            name,
            parent: parent.map(mailbox),
            role: None,
            sort_order: 0,
            subscribed,
        })
        .encode(&mut value)
        .unwrap();
        value.truncate(length);
        (mailbox(n), value)
    })
    .collect();
    let operations: Vec<_> = rows
        .iter()
        .map(|(id, row)| Operation::put(Table::Mailboxes, id.as_bytes(), row).unwrap())
        .collect();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: AccountId::from_bytes([1; 16]),
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
#[test]
fn flat_default_order_paging_total_and_query_state() {
    let _guard = FIXTURE.lock().unwrap();
    let response = run(r#", "calculateTotal":true"#, 3);
    assert_eq!(ids(&response), vec![id(1), id(2), id(3)]);
    let result = &response["methodResponses"][0][1];
    assert_eq!(result["position"].as_u64(), Some(0));
    assert_eq!(result["total"].as_u64(), Some(3));
    assert_eq!(result["limit"].as_u64(), Some(256));
    assert_eq!(result["canCalculateChanges"].as_bool(), Some(false));
    assert_eq!(
        QueryState::decode(result["queryState"].as_str().unwrap())
            .unwrap()
            .kind,
        QueryType::Mailbox
    );
    assert_eq!(
        response["methodResponses"][1][1]["ok"].as_bool(),
        Some(true)
    );
    let response = run(r#", "position":-2e0,"limit":1,"calculateTotal":true"#, 3);
    assert_eq!(ids(&response), vec![id(2)]);
    assert_eq!(
        response["methodResponses"][0][1]["position"].as_u64(),
        Some(1)
    );
    assert_eq!(response["methodResponses"][0][1]["total"].as_u64(), Some(3));
    let response = run(r#", "position":100,"limit":0,"calculateTotal":true"#, 3);
    assert!(ids(&response).is_empty());
    assert_eq!(
        response["methodResponses"][0][1]["position"].as_u64(),
        Some(100)
    );
    assert_eq!(response["methodResponses"][0][1]["total"].as_u64(), Some(3));
}
#[test]
fn sorts_anchors_and_complete_totals_across_a_merge() {
    let _guard = FIXTURE.lock().unwrap();
    let response = run(
        &format!(
            r#", "sort":[{{"property":"sortOrder","isAscending":false,"collation":"ignored"}}],"anchor":"{}","anchorOffset":-1,"position":100,"limit":2,"calculateTotal":true"#,
            id(299)
        ),
        300,
    );
    assert_eq!(ids(&response), vec![id(300), id(299)]);
    assert_eq!(
        response["methodResponses"][0][1]["total"].as_u64(),
        Some(300)
    );
    let response = run(r#", "limit":10000,"calculateTotal":true"#, 300);
    assert_eq!(ids(&response), (1..=256).map(id).collect::<Vec<_>>());
    assert_eq!(
        response["methodResponses"][0][1]["limit"].as_u64(),
        Some(256)
    );
    let response = run(r#", "anchor":"absent","limit":0"#, 3);
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("anchorNotFound")
    );
}
#[test]
fn all_filter_conditions_boolean_empty_sets_and_late_validation() {
    let _guard = FIXTURE.lock().unwrap();
    for (filter, expected) in [
        (
            r#"{"name":"FOLD","parentId":null,"role":null,"hasAnyRole":false,"isSubscribed":true}"#
                .to_owned(),
            vec![id(2), id(3)],
        ),
        (
            r#"{"operator":"NOT","conditions":[{"role":"inbox"}]}"#.to_owned(),
            vec![id(2), id(3)],
        ),
        (
            r#"{"conditions":[],"operator":"AND"}"#.to_owned(),
            vec![id(1), id(2), id(3)],
        ),
        (r#"{"operator":"OR","conditions":[]}"#.to_owned(), vec![]),
        (
            r#"{"operator":"NOT","conditions":[]}"#.to_owned(),
            vec![id(1), id(2), id(3)],
        ),
        (format!(r#"{{"parentId":"{}"}}"#, account()), vec![]),
        (r#"{"isSubscribed":false}"#.to_owned(), vec![]),
        (r#"{"hasAnyRole":true}"#.to_owned(), vec![id(1)]),
        (
            r#"{"operator":"NOT","conditions":[{"role":"inbox"},{"name":"Folder"}]}"#.to_owned(),
            vec![],
        ),
    ] {
        assert_eq!(
            ids(&run(&format!(",\"filter\":{filter}"), 3)),
            expected,
            "{filter}"
        );
    }
    for (extra, error) in [
        (
            r#", "filter":{"operator":"AND","conditions":[{"isSubscribed":false},{"unknown":true}]}"#,
            "unsupportedFilter",
        ),
        (
            r#", "filter":{"operator":"X","conditions":[]}"#,
            "unsupportedFilter",
        ),
        (r#", "filter":{"operator":"AND"}"#, "invalidArguments"),
        (r#", "filter":{"conditions":[]}"#, "invalidArguments"),
        (
            r#", "filter":{"operator":"OR","conditions":[],"name":"x"}"#,
            "invalidArguments",
        ),
        (r#", "filter":{"name":null}"#, "invalidArguments"),
        (r#", "sort":[{"property":"unknown"}]"#, "unsupportedSort"),
        (
            r#", "sort":[{"property":"name","collation":"unknown"}]"#,
            "unsupportedSort",
        ),
        (r#", "position":1.1"#, "invalidArguments"),
        (r#", "position":-9223372036854775808"#, "invalidArguments"),
        (r#", "position":18446744073709551615"#, "invalidArguments"),
        (r#", "position":9007199254740992"#, "invalidArguments"),
        (r#", "limit":-1"#, "invalidArguments"),
        (r#", "sortAsTree":null"#, "invalidArguments"),
    ] {
        let response = run(extra, 0);
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some(error),
            "{extra}"
        );
        assert_eq!(
            response["methodResponses"][1][1]["ok"].as_bool(),
            Some(true)
        );
    }
}
#[test]
fn tree_sort_and_ancestor_filtering_use_the_same_snapshot() {
    let _guard = FIXTURE.lock().unwrap();
    for (extra, expected) in [
        (
            r#", "sortAsTree":true,"sort":[{"property":"name"}]"#,
            vec![id(4), id(1), id(2), id(5), id(3)],
        ),
        (
            r#", "sortAsTree":true,"sort":[{"property":"name","isAscending":false}]"#,
            vec![id(1), id(3), id(2), id(5), id(4)],
        ),
        (
            r#", "filterAsTree":true,"filter":{"isSubscribed":true}"#,
            vec![id(4)],
        ),
        (
            r#", "filterAsTree":false,"filter":{"isSubscribed":true},"sort":[{"property":"name"}]"#,
            vec![id(2), id(4), id(3), id(5)],
        ),
        (
            r#", "sortAsTree":true,"filter":{"isSubscribed":true},"sort":[{"property":"name"}]"#,
            vec![id(4), id(2), id(5), id(3)],
        ),
        (
            r#", "sortAsTree":true,"filterAsTree":true,"filter":{"isSubscribed":true}"#,
            vec![id(4)],
        ),
    ] {
        let response = run_setup(&call(extra), 0, 256, 90, false, None, seed_tree);
        assert_eq!(ids(&response), expected, "{extra}");
    }
    assert_eq!(
        ids(&run_setup(
            &call(&format!(r#", "filter":{{"parentId":"{}"}}"#, id(1))),
            0,
            256,
            90,
            false,
            None,
            seed_tree
        )),
        vec![id(2), id(3)]
    );
}
#[test]
fn capability_admission_view_pressure_and_deadline_errors_preserve_later_calls() {
    let _guard = FIXTURE.lock().unwrap();
    for (input, busy, phase, error) in [
        (
            call("").replace(",\"urn:ietf:params:jmap:mail\"", ""),
            false,
            90,
            "unknownMethod",
        ),
        (call(""), true, 90, "serverUnavailable"),
        (call(""), false, 0, "serverUnavailable"),
    ] {
        let response = run_setup(&input, 2, 256, phase, busy, None, |_| {});
        if phase != 0 {
            assert_eq!(
                response["methodResponses"][1][1]["ok"].as_bool(),
                Some(true)
            );
        } else {
            assert_eq!(
                response["methodResponses"][1][1]["description"].as_str(),
                Some("executionDeadline")
            );
        }
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some(error)
        );
    }
}

#[test]
fn configured_page_limits_refs_and_repeated_methods_keep_custody() {
    use super::super::mailbox_get::tests::run_configured;
    let _guard = FIXTURE.lock().unwrap();
    let input = batch(&format!(
        r##"[
        ["Core/echo",{{"filter":{{"name":"folder"}},"sort":[{{"property":"sortOrder","isAscending":false}}]}},"args"],
        ["Mailbox/query",{{"accountId":"{}","#filter":{{"resultOf":"args","name":"Core/echo","path":"/filter"}},"#sort":{{"resultOf":"args","name":"Core/echo","path":"/sort"}},"calculateTotal":true}},"q"],
        ["Mailbox/get",{{"accountId":"{}","#ids":{{"resultOf":"q","name":"Mailbox/query","path":"/ids"}},"properties":["name"]}},"g"],
        ["Mailbox/query",{{"accountId":"{}","limit":0,"calculateTotal":true}},"q2"]]"##,
        account(),
        account(),
        account()
    ));
    let response = run_configured(
        &input,
        6,
        crate::limits::Limits {
            query_page: 2,
            ..crate::limits::Limits::default()
        },
        90,
        false,
        None,
        |_| {},
    );
    let result = &response["methodResponses"][1][1];
    assert_eq!(result["ids"][0].as_str(), Some(id(6).as_str()));
    assert_eq!(result["ids"][1].as_str(), Some(id(5).as_str()));
    assert_eq!(result["limit"].as_u64(), Some(2));
    assert_eq!(result["total"].as_u64(), Some(5));
    assert_eq!(
        response["methodResponses"][2][1]["list"]
            .as_arr()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(response["methodResponses"][3][1]["total"].as_u64(), Some(6));
}
#[test]
fn compiled_filter_limits_count_distinct_decoded_operands_and_validate_empty_accounts() {
    let _guard = FIXTURE.lock().unwrap();
    for (count, unique, expected) in [
        (64, true, None),
        (65, true, Some("unsupportedFilter")),
        (65, false, None),
    ] {
        let conditions = (0..count)
            .map(|n| format!(r#"{{"name":"value{}"}}"#, if unique { n } else { 0 }))
            .collect::<Vec<_>>()
            .join(",");
        let response = run(
            &format!(r#", "filter":{{"operator":"OR","conditions":[{conditions}]}}"#),
            0,
        );
        if let Some(error) = expected {
            assert_eq!(
                response["methodResponses"][0][1]["type"].as_str(),
                Some(error)
            );
        } else {
            assert!(ids(&response).is_empty());
        }
    }
    for (extra, expected) in [
        (
            format!(r#", "filter":{{"name":"{}"}}"#, "x".repeat(4097)),
            Some("unsupportedFilter"),
        ),
        (
            format!(r#", "filter":{{"name":"{}"}}"#, "x".repeat(4096)),
            None,
        ),
        (
            format!(r#", "filter":{{"role":"{}"}}"#, "x".repeat(300)),
            None,
        ),
        (
            format!(
                r#", "filter":{{"operator":"{}","conditions":[]}}"#,
                "x".repeat(300)
            ),
            Some("unsupportedFilter"),
        ),
        (
            format!(r#", "filter":{{"{}":true}}"#, "x".repeat(300)),
            Some("unsupportedFilter"),
        ),
        (
            format!(r#", "sort":[{{"property":"{}"}}]"#, "x".repeat(300)),
            Some("unsupportedSort"),
        ),
        (
            format!(
                r#", "sort":[{{"property":"name","collation":"{}"}}]"#,
                "x".repeat(300)
            ),
            Some("unsupportedSort"),
        ),
        (
            format!(
                r#", "sort":[{{"property":"sortOrder","collation":"{}"}}]"#,
                "x".repeat(300)
            ),
            None,
        ),
    ] {
        let response = run(&extra, 0);
        if let Some(error) = expected {
            assert_eq!(
                response["methodResponses"][0][1]["type"].as_str(),
                Some(error)
            );
        } else {
            assert!(ids(&response).is_empty());
        }
    }
}

#[test]
fn complete_merge_pages_anchor_edges_and_failed_query_release() {
    let _guard = FIXTURE.lock().unwrap();
    for descending in [false, true] {
        let mut got = Vec::new();
        for position in [0, 256] {
            got.extend(ids(&run(&format!(r#", "sort":[{{"property":"sortOrder","isAscending":{}}}],"position":{position}"#, !descending), 300)));
        }
        let mut expected: Vec<_> = (1..=300).map(id).collect();
        if descending {
            expected.reverse();
        }
        assert_eq!(got, expected);
    }
    for (anchor, extra, error, expected, position) in [
        (id(9), "", Some("anchorNotFound"), vec![], 0),
        (
            id(1),
            r#", "filter":{"name":"Folder"}"#,
            Some("anchorNotFound"),
            vec![],
            0,
        ),
        (
            id(2),
            r#", "anchorOffset":-10,"limit":1"#,
            None,
            vec![id(1)],
            0,
        ),
        (id(2), r#", "anchorOffset":10"#, None, vec![], 11),
        (
            id(2),
            r#", "anchorOffset":9007199254740991"#,
            None,
            vec![],
            3,
        ),
    ] {
        let response = run(&format!(r#", "anchor":"{anchor}"{extra}"#), 3);
        if let Some(error) = error {
            assert_eq!(
                response["methodResponses"][0][1]["type"].as_str(),
                Some(error)
            );
        } else {
            assert_eq!(ids(&response), expected);
            assert_eq!(
                response["methodResponses"][0][1]["position"].as_u64(),
                Some(position)
            );
        }
    }
    let input = batch(&format!(
        r#"[["Mailbox/query",{{"accountId":"{}","filter":{{"unknown":true}}}},"bad"],["Mailbox/query",{{"accountId":"{}","limit":1}},"good"]]"#,
        account(),
        account()
    ));
    let response = run_setup(&input, 3, 256, 90, false, None, |_| {});
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("unsupportedFilter")
    );
    let page = &response["methodResponses"][1][1];
    assert_eq!(page["ids"][0].as_str(), Some(id(1).as_str()));
    assert!(!page
        .as_object()
        .unwrap()
        .iter()
        .any(|(key, _)| key == "limit"));
    let mut conditions = (0..64)
        .map(|n| format!(r#"{{"name":"value{n}"}}"#))
        .collect::<Vec<_>>();
    conditions.push(r#"{"name":"valu\u00650"}"#.to_owned());
    assert!(ids(&run(
        &format!(
            r#", "filter":{{"operator":"OR","conditions":[{}]}}"#,
            conditions.join(",")
        ),
        0
    ))
    .is_empty());
}
#[test]
fn mid_query_execution_expiry_withholds_partial_results() {
    let _guard = FIXTURE.lock().unwrap();
    for cutoff in [100, 500, 1500] {
        let response = run_setup(
            &call(r#", "calculateTotal":true"#),
            100,
            256,
            90,
            false,
            Some(cutoff),
            |_| {},
        );
        let page = &response["methodResponses"][0][1];
        assert_eq!(page["type"].as_str(), Some("serverUnavailable"));
        assert_eq!(page["description"].as_str(), Some("executionDeadline"));
        assert!(!page
            .as_object()
            .unwrap()
            .iter()
            .any(|(key, _)| key == "ids" || key == "total"));
    }
}
