#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::super::mailbox_get::tests::{account, run_identities};
use crate::{
    config::preimage::tests::{lock, resolved, BASE},
    identity_catalog::Catalog,
};

fn request(arguments: &str) -> String {
    format!(
        r#"{{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],"methodCalls":[["Core/echo",{{"before":true}},"a"],["Identity/get",{arguments},"i"],["Core/echo",{{"after":true}},"z"]]}}"#
    )
}
fn arguments(extra: &str) -> String {
    format!(r#"{{"accountId":"{}"{extra}}}"#, account())
}
fn catalog() -> Catalog {
    let source = BASE.replace(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "01010101010101010101010101010101",
    );
    Catalog::prepare(&resolved(&source), &td_crypto::Provider).unwrap()
}
#[test]
fn defaults_ids_missing_and_result_references_use_real_retained_json() {
    let _lock = lock();
    let catalog = catalog();
    for extra in [
        "",
        r#", "ids":null, "properties":null"#,
        r#", "ids":["foreign","ibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","foreign","ibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]"#,
    ] {
        let response = run_identities(
            &request(&arguments(extra)),
            Some(&catalog),
            64,
            72000,
            90,
            false,
        )
        .unwrap();
        let result = &response["methodResponses"][1];
        assert_eq!(result[0].as_str(), Some("Identity/get"));
        assert_eq!(result[1]["list"].as_array().unwrap().len(), 1);
        assert_eq!(
            result[1]["list"][0]["email"].as_str(),
            Some("main@example.test")
        );
        assert_eq!(result[1]["list"][0]["mayDelete"].as_bool(), Some(false));
        assert!(result[1]["state"].as_str().unwrap().starts_with("c1_"));
        assert_eq!(
            result[1]["notFound"].as_array().unwrap().len(),
            usize::from(extra.contains("foreign"))
        );
        assert_eq!(
            response["methodResponses"][2][1]["after"].as_bool(),
            Some(true)
        );
    }
    let calls = format!(
        r##"{{"using":["urn:ietf:params:jmap:core","urn:ietf:params:jmap:submission"],"methodCalls":[["Core/echo",{{"ids":["ibbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]}},"a"],["Identity/get",{{"accountId":"{}","#ids":{{"resultOf":"a","name":"Core/echo","path":"/ids"}}}},"i"],["Core/echo",{{"#addresses":{{"resultOf":"i","name":"Identity/get","path":"/list/*/email"}}}},"z"]]}}"##,
        account()
    );
    let response = run_identities(&calls, Some(&catalog), 64, 72000, 90, false).unwrap();
    assert_eq!(
        response["methodResponses"][2][1]["addresses"][0].as_str(),
        Some("main@example.test")
    );
}
#[test]
fn capability_account_shape_and_duplicate_limits_refuse_without_hiding_later_calls() {
    let _lock = lock();
    let catalog = catalog();
    for (input, maximum, expected) in [
        (request(r#"{"accountId":"foreign"}"#), 64, "accountNotFound"),
        (
            request(&arguments(r#", "properties":["id"]"#)),
            64,
            "invalidArguments",
        ),
        (request(&arguments(r#", "ids":{}"#)), 64, "invalidArguments"),
        (
            request(&arguments(r#", "ids":["foreign","foreign"]"#)),
            1,
            "requestTooLarge",
        ),
        (
            request(&arguments("")).replace(",\"urn:ietf:params:jmap:submission\"", ""),
            64,
            "unknownMethod",
        ),
    ] {
        let response = run_identities(&input, Some(&catalog), maximum, 72000, 90, false).unwrap();
        assert_eq!(
            response["methodResponses"][1][1]["type"].as_str(),
            Some(expected),
            "{response}"
        );
        assert!(response["methodResponses"][2][1]["after"].is_true());
    }
    let response = run_identities(&request(&arguments("")), None, 64, 72000, 90, false).unwrap();
    assert_eq!(
        response["methodResponses"][1][1]["type"].as_str(),
        Some("unknownMethod")
    );
    let mismatch = Catalog::prepare(&resolved(BASE), &td_crypto::Provider).unwrap();
    let response = run_identities(
        &request(&arguments("")),
        Some(&mismatch),
        64,
        72000,
        90,
        false,
    )
    .unwrap_err();
    assert_eq!(response, crate::jmap_request::dispatch::Error::State);
}
#[test]
fn snapshot_pressure_and_expired_execution_do_not_publish_identity_objects() {
    let _lock = lock();
    let catalog = catalog();
    for (phase, busy) in [(90, true), (1, false)] {
        let response = run_identities(
            &request(&arguments("")),
            Some(&catalog),
            64,
            72000,
            phase,
            busy,
        )
        .unwrap();
        assert_eq!(response["methodResponses"][1][0].as_str(), Some("error"));
        assert_eq!(
            response["methodResponses"][1][1]["type"].as_str(),
            Some("serverUnavailable")
        );
        assert_eq!(
            response["methodResponses"][1][1]["description"].as_str(),
            Some(if busy {
                "storageBusy"
            } else {
                "executionDeadline"
            })
        );
    }
}

fn many_catalog(count: u8, name_bytes: usize) -> Catalog {
    let mut source = BASE
        .replace(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "00000000000000000000000000000001",
        )
        .replace(
            "[resolver \"primary\"]",
            &format!(
                "name = \"{}\"\n[resolver \"primary\"]",
                "x".repeat(name_bytes)
            ),
        );
    for n in 2..=count {
        source.push_str(&format!("[identity \"{n:032x}\"]\naccount = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nname = \"{}\"\nemail = \"main@example.test\"\n", "x".repeat(name_bytes)));
    }
    let source = source.replace(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "01010101010101010101010101010101",
    );
    Catalog::prepare(&resolved(&source), &td_crypto::Provider).unwrap()
}

#[test]
fn empty_selection_catalog_limit_order_and_exact_state() {
    let _lock = lock();
    let catalog = many_catalog(2, 0);
    let expected = catalog
        .state(crate::ports::ViewIdentity {
            account: catalog.account(),
            epoch: crate::ids::StoreEpoch::from_bytes([7; 16]),
            committed_sequence: crate::format::Sequence::from_u64(0),
            history_floor: crate::format::Sequence::from_u64(0),
        })
        .unwrap();
    for (extra, expected_ids) in [
        (r#", "ids":[]"#, vec![]),
        ("", vec![1, 2]),
        (
            r#", "ids":["i00000000000000000000000000000002","i00000000000000000000000000000001","i00000000000000000000000000000002"]"#,
            vec![2, 1],
        ),
    ] {
        let response = run_identities(
            &request(&arguments(extra)),
            Some(&catalog),
            64,
            72000,
            90,
            false,
        )
        .unwrap();
        let result = &response["methodResponses"][1][1];
        assert_eq!(
            crate::sync::IdentityState::decode(result["state"].as_str().unwrap()).unwrap(),
            expected
        );
        let list = result["list"].as_array().unwrap();
        assert_eq!(list.len(), expected_ids.len());
        for (object, id) in list.iter().zip(expected_ids) {
            assert_eq!(object["id"].as_str(), Some(format!("i{id:032x}").as_str()));
        }
        assert!(result["notFound"].as_array().unwrap().is_empty());
    }
    for extra in ["", r#", "ids":null"#] {
        let response = run_identities(
            &request(&arguments(extra)),
            Some(&catalog),
            1,
            72000,
            90,
            false,
        )
        .unwrap();
        assert_eq!(
            response["methodResponses"][1][1]["type"].as_str(),
            Some("requestTooLarge")
        );
        assert!(response["methodResponses"][2][1]["after"].is_true());
    }
}

#[test]
fn output_pressure_rolls_back_objects_and_preserves_later_calls() {
    let _lock = lock();
    let catalog = many_catalog(64, 2980);
    let response = run_identities(
        &request(&arguments("")),
        Some(&catalog),
        64,
        72000,
        90,
        false,
    )
    .unwrap();
    assert_eq!(
        response["methodResponses"][1][1]["type"].as_str(),
        Some("serverFail")
    );
    assert_eq!(
        response["methodResponses"][1][1]["description"].as_str(),
        Some("responseCapacity")
    );
    assert!(response["methodResponses"][0][1]["before"].is_true());
    assert!(response["methodResponses"][2][1]["after"].is_true());
    assert!(!response.to_string().contains("main@example.test"));
}

#[test]
fn phase_cutoffs_never_publish_a_partial_identity_result() {
    let _lock = lock();
    let catalog = many_catalog(16, 2980);
    let mut failures = 0;
    let mut successes = 0;
    let mut preserved = 0;
    for cutoff in [32, 256, 2048, 16384, 131072] {
        let response = super::super::mailbox_get::tests::run_identities_cutoff(
            &request(&arguments("")),
            Some(&catalog),
            64,
            72000,
            90,
            false,
            Some(cutoff),
        )
        .unwrap();
        let method = &response["methodResponses"][1];
        if method[0].as_str() == Some("error") {
            failures += 1;
            preserved += usize::from(response["methodResponses"][0][1]["before"].is_true());
            assert_eq!(method[1]["type"].as_str(), Some("serverUnavailable"));
            assert_eq!(method[1]["description"].as_str(), Some("executionDeadline"));
            assert!(!response.to_string().contains("main@example.test"));
        } else {
            successes += 1;
            assert_eq!(method[1]["list"].as_array().unwrap().len(), 16);
        }
    }
    assert!(failures > 0 && successes > 0 && preserved > 0);
}
