#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::Sequence,
    ids::{AccountId, DeviceId, MailboxId, StoreEpoch},
    jmap_request::{dispatch, resolution, response, CORE, MAIL},
    limits::Limits,
    ports::{self, Deadline, Tick, ViewIdentity},
    store_fs::{EmailUpdateCompletion, EmailUpdateRequest},
};
use std::cell::RefCell;
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
#[derive(Default)]
struct Mutator {
    names: RefCell<Vec<Vec<String>>>,
    refusal: Option<ports::Error>,
    indeterminate: bool,
    wrong_results: bool,
    expected: RefCell<Option<(StoreEpoch, Sequence)>>,
}
impl dispatch::EmailMutator for Mutator {
    fn update(
        &self,
        _: EmailUpdateRequest<'_>,
        _: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError> {
        panic!("unexpected Email update")
    }
}
impl MailboxMutator for Mutator {
    fn create_mailboxes(
        &self,
        request: MailboxCreateRequest<'_>,
        _: &mut Meter,
    ) -> Result<MailboxCreateCompletion, UploadError> {
        assert_eq!(request.account, ACCOUNT);
        assert_eq!(request.device, DEVICE);
        *self.expected.borrow_mut() = request.expected;
        self.names
            .borrow_mut()
            .push(request.names.iter().map(|name| name.to_string()).collect());
        if let Some(error) = self.refusal {
            return Err(error.into());
        }
        Ok(MailboxCreateCompletion {
            before: ViewIdentity {
                account: ACCOUNT,
                epoch: StoreEpoch::from_bytes([7; 16]),
                committed_sequence: Sequence::from_u64(3),
                history_floor: Sequence::default(),
            },
            outcome: if self.indeterminate {
                Err(crate::store_fs::CommitError::Indeterminate(
                    ports::Error::Io {
                        kind: std::io::ErrorKind::Other,
                        os_code: None,
                    },
                ))
            } else {
                Ok(Sequence::from_u64(4))
            },
            results: request
                .names
                .iter()
                .take(if self.wrong_results { 0 } else { usize::MAX })
                .map(|name| match *name {
                    "" => MailboxCreateResult::InvalidName,
                    "Inbox" => MailboxCreateResult::AlreadyExists,
                    _ => MailboxCreateResult::Created(MailboxId::from_bytes([8; 16])),
                })
                .collect(),
            admission_stopped: false,
        })
    }
}
fn account() -> String {
    wire::encode_id(ACCOUNT, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .into()
}
fn run(arguments: td_json::Json, mutator: &Mutator, maximum: usize) -> td_json::Json {
    run_profile(arguments, mutator, maximum, true)
}
fn run_profile(
    arguments: td_json::Json,
    mutator: &Mutator,
    maximum: usize,
    writable: bool,
) -> td_json::Json {
    let input = td_json::json!({"using":[CORE, MAIL],"createdIds":{},"methodCalls":[
        ["Mailbox/set", arguments, "create"], ["Core/echo", {"after":true}, "last"]
    ]})
    .to_string();
    let mut result = None;
    resolution::tests::run(|coordinator, spool, _, _| {
        let retention = coordinator
            .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
            .unwrap();
        let request = dispatch::tests::request_with_capabilities(
            &input,
            Limits {
                objects_per_method: maximum,
                ..Limits::default()
            },
            &[CORE, MAIL],
        );
        let mut plan = response::Preflight::new(request, "session", retention.maximum()).unwrap();
        while !plan.step(&mut [0; 4096]).unwrap() {}
        let mut completed = if writable {
            dispatch::run_with_mail_access(
                plan.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), 90).unwrap(),
                ACCOUNT,
                DEVICE,
                mutator,
                None,
            )
        } else {
            dispatch::run(
                plan.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), 90).unwrap(),
                ACCOUNT,
            )
        }
        .unwrap();
        let mut encoder = completed.encoder().unwrap();
        while !encoder.count_step(&mut [0; 4096]).unwrap() {}
        encoder.begin_output().unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let next = encoder.poll_output(&mut chunk).unwrap();
            bytes.extend_from_slice(&chunk[..next.written]);
            if next.complete {
                break;
            }
        }
        result = Some(td_json::parse_slice(&bytes).unwrap());
    });
    let result = result.unwrap();
    assert!(result["methodResponses"][1][1]["after"].is_true());
    result
}
#[test]
fn creation_preserves_creation_keys_and_returns_partial_results_and_mailbox_state() {
    let mutator = Mutator::default();
    let response = run(
        td_json::json!({"accountId":account(), "create":{
            "archive":{"name":"archive"}, "existing":{"name":"Inbox"},
            "bad":{"name":""}, "defaultParent":{"name":"root", "parentId":null},
            "child":{"name":"child", "parentId":"parent"},
            "unicode":{"name":"Café"}
        }}),
        &mutator,
        256,
    );
    let result = &response["methodResponses"][0];
    assert_eq!(result[0].as_str(), Some("Mailbox/set"));
    assert_eq!(result[2].as_str(), Some("create"));
    let result = &result[1];
    for name in ["archive", "unicode"] {
        assert!(result["created"][name]["id"].as_str().is_some());
        assert_eq!(response["createdIds"][name], result["created"][name]["id"]);
    }
    for name in ["existing", "bad", "child", "defaultParent"] {
        assert!(response["createdIds"].get(name).is_none());
        assert_eq!(
            result["notCreated"][name]["type"].as_str(),
            Some("invalidProperties")
        );
    }
    assert_eq!(
        DataState::decode(result["oldState"].as_str().unwrap())
            .unwrap()
            .kind,
        DataType::Mailbox
    );
    assert_eq!(
        DataState::decode(result["newState"].as_str().unwrap())
            .unwrap()
            .sequence,
        Sequence::from_u64(4)
    );
    assert!(mutator.names.borrow()[0].contains(&"Café".to_string()));
    assert!(!mutator.names.borrow()[0].contains(&"child".to_string()));
}
#[test]
fn creation_refuses_unsupported_methods_wrong_accounts_states_and_object_limits_before_effects() {
    for (extra, expected) in [
        (
            td_json::json!({"update":{"x":{"name":"renamed"}}}),
            "invalidArguments",
        ),
        (td_json::json!({"destroy":["x"]}), "invalidArguments"),
        (td_json::json!({"create":[]}), "invalidArguments"),
        (td_json::json!({"ifInState":"stale"}), "stateMismatch"),
        (td_json::json!({"accountId":"foreign"}), "accountNotFound"),
        (
            td_json::json!({"create":{"a":{"name":"a"},"b":{"name":"b"}}}),
            "requestTooLarge",
        ),
    ] {
        let mut input = td_json::ObjectBuilder::new().set("accountId", account());
        for (key, value) in extra.as_object().unwrap() {
            input.insert(key.clone(), value.clone());
        }
        let mutator = Mutator::default();
        let result = run(input.build(), &mutator, 1);
        assert_eq!(
            result["methodResponses"][0][1]["type"].as_str(),
            Some(expected)
        );
        assert!(mutator.names.borrow().is_empty());
    }
}
#[test]
fn invalid_or_oversized_names_cannot_be_created_and_storage_refusal_is_preserved() {
    for name in [
        td_json::json!(true),
        td_json::json!("x".repeat(1025)),
        td_json::json!("\u{1}".repeat(1025)),
    ] {
        let mutator = Mutator::default();
        let result = run(
            td_json::json!({"accountId":account(),"create":{"newMailbox":{"name":name}}}),
            &mutator,
            256,
        );
        assert_eq!(
            result["methodResponses"][0][1]["notCreated"]["newMailbox"]["type"].as_str(),
            Some("invalidProperties")
        );
        assert_eq!(mutator.names.borrow()[0], [String::new()]);
    }
    let mutator = Mutator {
        refusal: Some(ports::Error::Forbidden),
        ..Default::default()
    };
    let result = run(
        td_json::json!({"accountId":account(),"create":{"newMailbox":{"name":"archive"}}}),
        &mutator,
        256,
    );
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("forbidden")
    );
}

#[test]
fn output_capacity_is_reserved_before_creation_and_uncertain_commits_are_not_successes() {
    let mut creates = td_json::ObjectBuilder::new();
    for index in 0..256 {
        creates.insert(
            format!("{}{:03}", "x".repeat(220), index),
            td_json::json!({"name":"archive"}),
        );
    }
    let mutator = Mutator::default();
    let result = run(
        td_json::json!({"accountId":account(), "create":creates.build()}),
        &mutator,
        256,
    );
    assert_eq!(
        result["methodResponses"][0][1]["description"].as_str(),
        Some("responseCapacity")
    );
    assert!(mutator.names.borrow().is_empty());
    let mutator = Mutator {
        indeterminate: true,
        ..Default::default()
    };
    let result = run(
        td_json::json!({"accountId":account(),"create":{"newMailbox":{"name":"archive"}}}),
        &mutator,
        256,
    );
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("serverPartialFail")
    );
    assert!(result["methodResponses"][0][1].get("created").is_none());
}

#[test]
fn creation_profile_state_forwarding_and_postcommit_failure_are_explicit() {
    let state = DataState {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([7; 16]),
        kind: DataType::Mailbox,
        sequence: Sequence::from_u64(3),
    };
    let mut bytes = [0; DataState::WIRE_BYTES];
    let text = state.encode(&mut bytes).unwrap();
    let input = td_json::json!({"accountId":account(),"ifInState":text,"create":{"newMailbox":{"name":"archive"}}});
    let mutator = Mutator::default();
    let result = run(input.clone(), &mutator, 256);
    assert_eq!(
        result["methodResponses"][0][0].as_str(),
        Some("Mailbox/set")
    );
    assert_eq!(
        *mutator.expected.borrow(),
        Some((state.epoch, state.sequence))
    );
    let refused = Mutator {
        refusal: Some(ports::Error::Conflict),
        ..Default::default()
    };
    let result = run(input.clone(), &refused, 256);
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("stateMismatch")
    );
    let readonly = Mutator::default();
    let result = run_profile(input.clone(), &readonly, 256, false);
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("unknownMethod")
    );
    assert!(readonly.names.borrow().is_empty());
    let broken = Mutator {
        wrong_results: true,
        ..Default::default()
    };
    let result = run(input, &broken, 256);
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("serverPartialFail")
    );
}

#[test]
fn creation_overlay_limit_refuses_the_whole_call_before_effects() {
    let mut creates = td_json::ObjectBuilder::new();
    for index in 0..65 {
        creates.insert(
            format!("c{index:02}"),
            td_json::json!({"name":format!("folder{index}")}),
        );
    }
    let mutator = Mutator::default();
    let response = run(
        td_json::json!({"accountId":account(),"create":creates.build()}),
        &mutator,
        256,
    );
    assert_eq!(
        response["methodResponses"][0][1]["description"].as_str(),
        Some("responseCapacity")
    );
    assert!(mutator.names.borrow().is_empty());
    assert_eq!(response["createdIds"], td_json::json!({}));
}
