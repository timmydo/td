#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::Sequence,
    ids::{AccountId, DeviceId, StoreEpoch},
    jmap_request::{dispatch, resolution, response, CORE, MAIL},
    limits::Limits,
    ports::{Deadline, Tick, ViewIdentity},
    wire,
};
use std::cell::RefCell;
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
#[derive(Default)]
struct Flags {
    calls: RefCell<Vec<Vec<EmailUpdate>>>,
    refuse: bool,
    commit_error: Option<crate::store_fs::CommitError>,
    broken: bool,
    stopped: bool,
}
impl FlagMutator for Flags {
    fn update(
        &self,
        request: EmailUpdateRequest<'_>,
        _: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError> {
        assert_eq!(request.account, ACCOUNT);
        assert_eq!(request.device, DEVICE);
        self.calls.borrow_mut().push(request.updates.to_vec());
        if self.refuse {
            return Err(ports::Error::Conflict.into());
        }
        Ok(EmailUpdateCompletion {
            before: ViewIdentity {
                account: ACCOUNT,
                epoch: StoreEpoch::from_bytes([7; 16]),
                committed_sequence: Sequence::from_u64(3),
                history_floor: Sequence::from_u64(0),
            },
            outcome: self.commit_error.map_or(Ok(Sequence::from_u64(4)), Err),
            results: if self.broken {
                vec![EmailUpdateResult::Updated]
            } else {
                request
                    .updates
                    .iter()
                    .map(|u| {
                        if u.email == EmailId::from_bytes([99; 16]) {
                            EmailUpdateResult::NotFound
                        } else {
                            EmailUpdateResult::Updated
                        }
                    })
                    .collect()
            },
            admission_stopped: self.stopped,
        })
    }
}
fn id<T: wire::LocalId>(value: T) -> String {
    wire::encode_id(value, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .into()
}
fn run(arguments: td_json::Json, flags: &Flags, maximum: usize) -> td_json::Json {
    run_timing(arguments, flags, maximum, false).unwrap()
}
fn run_timing(
    arguments: td_json::Json,
    flags: &Flags,
    maximum: usize,
    expire: bool,
) -> Result<td_json::Json, Error> {
    struct Timed<'a> {
        flags: &'a Flags,
        clock: &'a resolution::tests::Timer,
        expire: bool,
    }
    impl FlagMutator for Timed<'_> {
        fn update(
            &self,
            request: EmailUpdateRequest<'_>,
            meter: &mut Meter,
        ) -> Result<EmailUpdateCompletion, UploadError> {
            let result = self.flags.update(request, meter);
            if self.expire {
                self.clock.0.store(95, std::sync::atomic::Ordering::SeqCst);
            }
            result
        }
    }
    let input = td_json::json!({"using":[CORE, MAIL],"methodCalls":[
        ["Core/echo",{"before":true},"first"],
        ["Email/set",arguments,"flags"],
        ["Core/echo",{"after":true},"last"]
    ]})
    .to_string();
    let mut result = None;
    resolution::tests::run(|coordinator, spool, _, clock| {
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
        let mut complete = match dispatch::run_with_flags(
            plan.finish().unwrap(),
            retention,
            Deadline::after(Tick(0), 90).unwrap(),
            ACCOUNT,
            DEVICE,
            &Timed {
                flags,
                clock,
                expire,
            },
        ) {
            Ok(complete) => complete,
            Err(error) => {
                result = Some(Err(error));
                return;
            }
        };
        let mut encoder = complete.encoder().unwrap();
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
        result = Some(Ok(
            td_json::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
        ));
    });
    let response = result.unwrap()?;
    assert!(response["methodResponses"][0][1]["before"].is_true());
    if expire {
        assert_eq!(
            response["methodResponses"][2][1]["description"].as_str(),
            Some("executionDeadline")
        );
    } else {
        assert!(response["methodResponses"][2][1]["after"].is_true());
    }
    Ok(response)
}
#[test]
fn mixed_flag_results_keep_request_ids_and_both_states() {
    let flags = Flags::default();
    let a = id(EmailId::from_bytes([3; 16]));
    let missing = id(EmailId::from_bytes([99; 16]));
    let bad = id(EmailId::from_bytes([4; 16]));
    let input = format!(
        r#"{{"accountId":"{}","update":{{"{a}":{{"keywords/$seen":true,"keywords/$flagged":null}},"{missing}":{{}},"{bad}":{{"keywords/$seen":false}}}}}}"#,
        id(ACCOUNT)
    );
    let response = run(td_json::parse(&input).unwrap(), &flags, 256);
    assert_eq!(
        response["methodResponses"][1][0].as_str(),
        Some("Email/set")
    );
    let answer = &response["methodResponses"][1][1];
    assert!(answer["updated"][a.as_str()].is_null());
    assert_eq!(
        answer["notUpdated"][missing.as_str()]["type"].as_str(),
        Some("notFound")
    );
    assert_eq!(
        answer["notUpdated"][bad.as_str()]["type"].as_str(),
        Some("invalidProperties")
    );
    assert_eq!(
        DataState::decode(answer["oldState"].as_str().unwrap())
            .unwrap()
            .sequence,
        Sequence::from_u64(3)
    );
    assert_eq!(
        DataState::decode(answer["newState"].as_str().unwrap())
            .unwrap()
            .sequence,
        Sequence::from_u64(4)
    );
    let calls = flags.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].len(), 2);
    assert_eq!(calls[0][0].seen, Some(true));
    assert_eq!(calls[0][0].flagged, Some(false));
}
#[test]
fn malformed_and_oversized_sets_never_reach_mutation() {
    let flags = Flags::default();
    let a = id(EmailId::from_bytes([3; 16]));
    let b = id(EmailId::from_bytes([4; 16]));
    for (arguments, expected) in [
        (
            format!(r#"{{"accountId":"{}","ifInState":"bad"}}"#, id(ACCOUNT)),
            "stateMismatch",
        ),
        (
            format!(
                r#"{{"accountId":"{}","update":{{"{a}":{{}},"{b}":{{}}}}}}"#,
                id(ACCOUNT)
            ),
            "requestTooLarge",
        ),
        (
            format!(r#"{{"accountId":"{}","destroy":["{a}"]}}"#, id(ACCOUNT)),
            "invalidArguments",
        ),
    ] {
        let response = run(td_json::parse(&arguments).unwrap(), &flags, 1);
        assert_eq!(response["methodResponses"][1][0].as_str(), Some("error"));
        assert_eq!(
            response["methodResponses"][1][1]["type"].as_str(),
            Some(expected)
        );
    }
    assert!(flags.calls.borrow().is_empty());
}
#[test]
fn store_state_refusal_keeps_other_method_results() {
    let flags = Flags {
        refuse: true,
        ..Flags::default()
    };
    for (arguments, expected) in [
        (
            td_json::json!({"accountId":id(ACCOUNT)}),
            "serverUnavailable",
        ),
        (
            td_json::json!({"accountId":id(ACCOUNT),"ifInState":state()}),
            "stateMismatch",
        ),
    ] {
        let response = run(arguments, &flags, 256);
        assert_eq!(
            response["methodResponses"][1][1]["type"].as_str(),
            Some(expected)
        );
    }
}

#[test]
fn oversized_result_bound_refuses_before_calling_storage() {
    let flags = Flags::default();
    let entries = (0..256)
        .map(|n| format!("\"{}{:03}\":{{}}", "x".repeat(220), n))
        .collect::<Vec<_>>()
        .join(",");
    let input = format!(
        r#"{{"accountId":"{}","update":{{{entries}}}}}"#,
        id(ACCOUNT)
    );
    let response = run(td_json::parse(&input).unwrap(), &flags, 256);
    assert_eq!(
        response["methodResponses"][1][1]["description"].as_str(),
        Some("responseCapacity")
    );
    assert!(flags.calls.borrow().is_empty());
}

#[test]
fn committed_result_survives_execution_phase_expiry() {
    let flags = Flags::default();
    let response =
        run_timing(td_json::json!({"accountId":id(ACCOUNT)}), &flags, 256, true).unwrap();
    assert_eq!(
        response["methodResponses"][1][0].as_str(),
        Some("Email/set")
    );
    assert!(response["methodResponses"][1][1]["newState"]
        .as_str()
        .is_some());
}

fn state() -> String {
    DataState {
        account: ACCOUNT,
        epoch: StoreEpoch::from_bytes([7; 16]),
        kind: DataType::Email,
        sequence: Sequence::from_u64(3),
    }
    .encode(&mut [0; DataState::WIRE_BYTES])
    .unwrap()
    .to_owned()
}
#[test]
fn commit_outcomes_never_fabricate_failure_of_a_known_success() {
    for (commit_error, expected) in [
        (
            Some(crate::store_fs::CommitError::Rejected(ports::Error::Busy)),
            "serverUnavailable",
        ),
        (
            Some(crate::store_fs::CommitError::Indeterminate(
                ports::Error::Io {
                    kind: std::io::ErrorKind::Other,
                    os_code: None,
                },
            )),
            "serverPartialFail",
        ),
    ] {
        let flags = Flags {
            commit_error,
            ..Flags::default()
        };
        let response = run(td_json::json!({"accountId":id(ACCOUNT)}), &flags, 256);
        assert_eq!(
            response["methodResponses"][1][1]["type"].as_str(),
            Some(expected)
        );
    }
    let flags = Flags {
        broken: true,
        ..Flags::default()
    };
    assert_eq!(
        run_timing(
            td_json::json!({"accountId":id(ACCOUNT)}),
            &flags,
            256,
            false
        ),
        Err(Error::State)
    );
    let flags = Flags {
        stopped: true,
        ..Flags::default()
    };
    let response = run(td_json::json!({"accountId":id(ACCOUNT)}), &flags, 256);
    assert_eq!(
        response["methodResponses"][1][0].as_str(),
        Some("Email/set")
    );
    assert!(response["methodResponses"][1][1]["updated"].is_null());
    assert!(response["methodResponses"][1][1]["notUpdated"].is_null());
}
#[test]
fn foreign_account_and_non_email_ids_have_typed_absence() {
    let flags = Flags::default();
    let response = run(td_json::json!({"accountId":"foreign"}), &flags, 256);
    assert_eq!(
        response["methodResponses"][1][1]["type"].as_str(),
        Some("accountNotFound")
    );
    assert!(flags.calls.borrow().is_empty());
    let other = id(crate::ids::MailboxId::from_bytes([1; 16]));
    let input = format!(
        r#"{{"accountId":"{}","update":{{"foreign":{{}},"{other}":{{}}}}}}"#,
        id(ACCOUNT)
    );
    let response = run(td_json::parse(&input).unwrap(), &flags, 256);
    for name in ["foreign", other.as_str()] {
        assert_eq!(
            response["methodResponses"][1][1]["notUpdated"][name]["type"].as_str(),
            Some("notFound")
        );
    }
    assert!(flags.calls.borrow()[0].is_empty());
}
