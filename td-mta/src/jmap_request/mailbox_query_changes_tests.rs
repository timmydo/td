#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, batch, id, run_setup};
fn input(extra: &str) -> String {
    batch(&format!(
        r#"[["Mailbox/queryChanges",{{"accountId":"{}"{extra}}},"c"],["Mailbox/query",{{"accountId":"{}","limit":1}},"q"],["Core/echo",{{"ok":true}},"after"]]"#,
        account(),
        account()
    ))
}
#[test]
fn supported_shapes_refuse_delta_and_release_the_workspace() {
    let cases = [
        r#", "sinceQueryState":"unknown""#.to_owned(),
        format!(r#", "sinceQueryState":"{}","maxChanges":0,"upToId":"{}","calculateTotal":true,"filter":{{"name":"Inbox"}},"sort":[{{"property":"name"}}]"#, "s".repeat(4096), id(1)),
        r#", "sinceQueryState":"","maxChanges":null,"upToId":null,"filter":null,"sort":null"#.to_owned(),
        r#", "sinceQueryState":"unknown","maxChanges":2e0,"filter":{"operator":"NOT","conditions":[]}"#.to_owned(),
    ];
    for count in [0, 3] {
        for extra in &cases {
            let response = run_setup(&input(extra), count, 256, 90, false, None, |_| {});
            assert_eq!(response["methodResponses"][0][0].as_str(), Some("error"));
            assert_eq!(
                response["methodResponses"][0][1]["type"].as_str(),
                Some("cannotCalculateChanges")
            );
            let ids = response["methodResponses"][1][1]["ids"].as_arr().unwrap();
            assert_eq!(ids.len(), usize::from(count != 0));
            if count != 0 {
                assert_eq!(ids[0].as_str(), Some(id(1).as_str()));
            }
            assert_eq!(
                response["methodResponses"][2][1]["ok"].as_bool(),
                Some(true)
            );
        }
    }
}
#[test]
fn malformed_arguments_and_unsupported_semantics_keep_specific_errors() {
    for (extra, error) in [
        ("", "invalidArguments"),
        (r#", "sinceQueryState":null"#, "invalidArguments"),
        (r#", "sinceQueryState":1"#, "invalidArguments"),
        (
            r#", "sinceQueryState":"s","maxChanges":1.5"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","maxChanges":-1"#,
            "invalidArguments",
        ),
        (r#", "sinceQueryState":"s","upToId":[]"#, "invalidArguments"),
        (
            r#", "sinceQueryState":"s","calculateTotal":null"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","position":0"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","filter":{"operator":"AND","conditions":[{"isSubscribed":false},{"unknown":true}] }"#,
            "unsupportedFilter",
        ),
        (
            r#", "sinceQueryState":"s","filter":{"name":false}"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","sort":[{"property":"name","collation":"unknown"}]"#,
            "unsupportedSort",
        ),
        (
            r#", "sinceQueryState":"s","sortAsTree":false"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","filterAsTree":false"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","anchorOffset":0"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","anchor":null"#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","anchor":"id""#,
            "invalidArguments",
        ),
        (
            r#", "sinceQueryState":"s","limit":null"#,
            "invalidArguments",
        ),
        (r#", "sinceQueryState":"s","limit":0"#, "invalidArguments"),
        (r#", "sinceQueryState":"s","upToId":"""#, "invalidArguments"),
        (
            r#", "sinceQueryState":"s","upToId":"a b""#,
            "invalidArguments",
        ),
    ] {
        let response = run_setup(&input(extra), 0, 256, 90, false, None, |_| {});
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some(error),
            "{extra}"
        );
        assert_eq!(
            response["methodResponses"][2][1]["ok"].as_bool(),
            Some(true)
        );
    }
}
#[test]
fn account_capability_deadline_and_query_state_references_remain_scoped() {
    let normal = input(r#", "sinceQueryState":"s""#);
    for (request, busy, phase, expected) in [
        (
            normal.replace(&account(), "foreign"),
            false,
            90,
            "accountNotFound",
        ),
        (
            normal.replace(",\"urn:ietf:params:jmap:mail\"", ""),
            false,
            90,
            "unknownMethod",
        ),
        (normal.clone(), true, 90, "serverUnavailable"),
        (normal, false, 0, "serverUnavailable"),
    ] {
        let response = run_setup(&request, 0, 256, phase, busy, None, |_| {});
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some(expected)
        );
    }
    let request = batch(&format!(
        r##"[["Mailbox/query",{{"accountId":"{}"}},"q"],["Mailbox/queryChanges",{{"accountId":"{}","#sinceQueryState":{{"resultOf":"q","name":"Mailbox/query","path":"/queryState"}}}},"c"]]"##,
        account(),
        account()
    ));
    let response = run_setup(&request, 1, 256, 90, false, None, |_| {});
    assert_eq!(
        response["methodResponses"][0][0].as_str(),
        Some("Mailbox/query")
    );
    assert_eq!(
        response["methodResponses"][1][1]["type"].as_str(),
        Some("cannotCalculateChanges")
    );
}

#[test]
fn mode_specific_arguments_and_id_lengths_are_validated() {
    for extra in [
        r#", "sinceQueryState":"s""#,
        r#", "maxChanges":null"#,
        r#", "upToId":null"#,
    ] {
        let request = batch(&format!(
            r#"[["Mailbox/query",{{"accountId":"{}"{extra}}},"q"]]"#,
            account()
        ));
        let response = run_setup(&request, 0, 256, 90, false, None, |_| {});
        assert_eq!(
            response["methodResponses"][0][1]["type"].as_str(),
            Some("invalidArguments")
        );
    }
    let response = run_setup(
        &input(&format!(
            r#", "sinceQueryState":"s","upToId":"{}""#,
            "x".repeat(256)
        )),
        0,
        256,
        90,
        false,
        None,
        |_| {},
    );
    assert_eq!(
        response["methodResponses"][0][1]["type"].as_str(),
        Some("invalidArguments")
    );
}
