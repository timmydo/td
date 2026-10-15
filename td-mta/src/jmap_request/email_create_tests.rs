#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{logical::Cell, DiskLimits, ViewMode, WorkLimits},
    ids::{BlobId, EmailId, ThreadId},
    jmap_request::{dispatch, response, CORE, MAIL},
    limits::Limits,
    ownership::SlotState,
    ports::{Clock, Deadline, Tick, Time},
    store_fs::{tests::Fixture, AuxiliaryUsage, IndexStore, RequestSpool, StoreCoordinator},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
struct Now;
impl Clock for Now {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 1,
            monotonic: Tick(1),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
#[derive(Default)]
struct Creator {
    calls: AtomicUsize,
    mode: u8,
}
impl DraftCreator for Creator {
    fn create_draft(
        &self,
        input: DraftInput<'_>,
        meter: &mut Meter,
    ) -> Result<DraftCompletion, DraftError> {
        assert_eq!(input.account, ACCOUNT);
        assert_eq!(input.device, DEVICE);
        assert_eq!(meter.deadline(), deadline().into());
        assert_eq!(input.deadline, Deadline::after(Tick(0), 90).unwrap());
        assert!(input.value.get("literal").unwrap().as_bool().unwrap());
        self.calls.fetch_add(1, Ordering::Relaxed);
        match self.mode {
            1 => {
                return Err(DraftError::Input(draft_input::Error::Property(
                    "attachments",
                )))
            }
            2 => return Err(DraftError::Store(ports::Error::Forbidden.into())),
            3 => return Err(DraftError::Store(ports::Error::Conflict.into())),
            4 => return Err(DraftError::Store(ports::Error::NotFound.into())),
            5 => return Err(DraftError::Store(ports::Error::Invalid.into())),
            8 => return Err(DraftError::Store(ports::Error::Quota.into())),
            9 => return Err(DraftError::Store(ports::Error::Capacity.into())),
            _ => (),
        }
        Ok(DraftCompletion {
            before_epoch: StoreEpoch::from_bytes([7; 16]),
            before_sequence: Sequence::from_u64(1),
            email: EmailId::from_bytes([3; 16]),
            blob: BlobId::from_bytes([4; 16]),
            thread: ThreadId::from_bytes([5; 16]),
            size: 123,
            outcome: if self.mode == 6 {
                Err(CommitError::Indeterminate(ports::Error::Corrupt))
            } else {
                Ok(Sequence::from_u64(2))
            },
            admission_stopped: self.mode == 7,
            cleanup_error: matches!(self.mode, 7 | 10).then_some(ports::Error::Corrupt),
        })
    }
}
impl dispatch::EmailMutator for Creator {
    fn update(
        &self,
        _: crate::store_fs::EmailUpdateRequest<'_>,
        _: &mut Meter,
    ) -> Result<crate::store_fs::EmailUpdateCompletion, UploadError> {
        panic!("unexpected update")
    }
}
impl dispatch::MailboxMutator for Creator {
    fn create_mailboxes(
        &self,
        _: crate::store_fs::MailboxCreateRequest<'_>,
        _: &mut Meter,
    ) -> Result<crate::store_fs::MailboxCreateCompletion, UploadError> {
        panic!("unexpected folder creation")
    }
}
fn account() -> String {
    wire::encode_id(ACCOUNT, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .into()
}
fn run(
    arguments: td_json::Json,
    creator: &Creator,
    enabled: bool,
    maximum: usize,
) -> td_json::Json {
    let input = td_json::json!({"using":[CORE,MAIL],"methodCalls":[
        ["Core/echo", {"draft":{"literal":true}}, "seed"],
        ["Email/set", arguments, "create"],
        ["Core/echo", {"after":true}, "last"]
    ]})
    .to_string();
    let result = run_batch(&input, creator, enabled, maximum, 4096);
    assert!(result["methodResponses"][2][1]["after"].is_true());
    result
}
fn run_batch(
    input: &str,
    creator: &Creator,
    enabled: bool,
    maximum: usize,
    width: usize,
) -> td_json::Json {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let files = Fixture::new();
    let mut files_root = files.locked();
    let clock = Arc::new(Now);
    let spool = RequestSpool::open(&mut files_root, clock.clone(), deadline()).unwrap();
    let resources = Limits::default().plan().unwrap();
    let plan = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([7; 16]),
        clock,
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { Cell::EMPTY }; 4];
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
    let retention = coordinator.retain_response(&spool, deadline()).unwrap();
    let request = dispatch::tests::request_with_capabilities(
        input,
        Limits {
            objects_per_method: maximum,
            ..Limits::default()
        },
        &[CORE, MAIL],
    );
    let mut preflight = response::Preflight::new(request, "session", retention.maximum()).unwrap();
    while !preflight.step(&mut [0; 4096]).unwrap() {}
    let mut result = dispatch::run_with_mail_access(
        preflight.finish().unwrap(),
        retention,
        Deadline::after(Tick(0), 90).unwrap(),
        ACCOUNT,
        DEVICE,
        creator,
        enabled.then_some(creator as &dyn DraftCreator),
    )
    .unwrap();
    let mut encoder = result.encoder().unwrap();
    while !encoder.count_step(&mut [0; 4096]).unwrap() {}
    encoder.begin_output().unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut chunk = vec![0; width];
        let next = encoder.poll_output(&mut chunk).unwrap();
        bytes.extend_from_slice(&chunk[..next.written]);
        if next.complete {
            break;
        }
    }
    encoder.finish().unwrap();
    td_json::parse_slice(&bytes).unwrap()
}
fn input() -> td_json::Json {
    td_json::json!({"accountId":account(),"create":{"draft":{"literal":true}}})
}
#[test]
fn success_preserves_ids_states_and_known_commit_despite_cleanup_failure() {
    for mode in [0, 7, 10] {
        let creator = Creator {
            mode,
            ..Creator::default()
        };
        let result = run(input(), &creator, true, 256);
        let result = &result["methodResponses"][1];
        assert_eq!(result[0].as_str(), Some("Email/set"));
        let body = &result[1];
        assert_eq!(body["created"]["draft"]["size"].as_u64(), Some(123));
        assert_eq!(
            wire::Id::parse(body["created"]["draft"]["id"].as_str().unwrap())
                .unwrap()
                .local(),
            Some(EmailId::from_bytes([3; 16]))
        );
        assert_eq!(
            wire::Id::parse(body["created"]["draft"]["blobId"].as_str().unwrap())
                .unwrap()
                .local(),
            Some(BlobId::from_bytes([4; 16]))
        );
        assert_eq!(
            wire::Id::parse(body["created"]["draft"]["threadId"].as_str().unwrap())
                .unwrap()
                .local(),
            Some(ThreadId::from_bytes([5; 16]))
        );
        assert_eq!(
            DataState::decode(body["newState"].as_str().unwrap())
                .unwrap()
                .sequence,
            Sequence::from_u64(2)
        );
        assert!(body["notCreated"].is_null());
        assert_eq!(creator.calls.load(Ordering::Relaxed), 1);
    }
}
#[test]
fn refusal_does_not_claim_creation_and_later_calls_continue() {
    for (mode, method_error, property) in [
        (1, None, Some("attachments")),
        (2, Some("forbidden"), None),
        (3, Some("serverUnavailable"), None),
        (4, None, None),
        (5, None, None),
        (6, Some("serverPartialFail"), None),
        (8, None, None),
        (9, Some("serverFail"), None),
    ] {
        let creator = Creator {
            mode,
            ..Creator::default()
        };
        let result = run(input(), &creator, true, 256);
        let body = &result["methodResponses"][1][1];
        if let Some(error) = method_error {
            assert_eq!(body["type"].as_str(), Some(error));
        } else {
            assert!(body["created"].is_null());
            assert_eq!(body["oldState"], body["newState"]);
            assert_eq!(
                body["notCreated"]["draft"]["type"].as_str(),
                Some(match mode {
                    4 | 5 => "serverFail",
                    8 => "tooLarge",
                    _ => "invalidProperties",
                })
            );
            if let Some(property) = property {
                assert_eq!(
                    body["notCreated"]["draft"]["properties"][0].as_str(),
                    Some(property)
                );
            } else {
                assert!(body["notCreated"]["draft"]["properties"].is_null());
            }
        }
    }
}
#[test]
fn unsupported_mixed_writes_references_accounts_and_limits_precede_effects() {
    for (arguments, error, enabled, maximum) in [
        (input(), "invalidArguments", false, 256),
        (
            td_json::json!({"accountId":"foreign","create":{"draft":{}}}),
            "accountNotFound",
            true,
            256,
        ),
        (
            td_json::json!({"accountId":account(),"create":{"a":{},"b":{}}}),
            "invalidArguments",
            true,
            256,
        ),
        (
            td_json::json!({"accountId":account(),"create":{"a":{},"b":{}}}),
            "requestTooLarge",
            true,
            1,
        ),
        (
            td_json::json!({"accountId":account(),"create":{"draft":{}},"destroy":["foreign"]}),
            "invalidArguments",
            true,
            256,
        ),
        (
            td_json::json!({"accountId":account(),"create":{"draft":{}},"update":{"foreign":{}}}),
            "invalidArguments",
            true,
            256,
        ),
        (
            td_json::json!({"accountId":account(),"#create":{"resultOf":"seed","name":"Core/echo","path":""}}),
            "invalidArguments",
            true,
            256,
        ),
        (
            td_json::json!({"accountId":account(),"create":{"bad key":{}}}),
            "invalidArguments",
            true,
            256,
        ),
        (
            td_json::json!({"accountId":account(),"create":{"draft":{}},"ifInState":"invalid"}),
            "stateMismatch",
            true,
            256,
        ),
    ] {
        let creator = Creator::default();
        let result = run(arguments, &creator, enabled, maximum);
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some(error),
            "{result}"
        );
        assert_eq!(creator.calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn insufficient_response_space_refuses_before_calling_the_creator() {
    let creator = Creator::default();
    crate::jmap_request::resolution::tests::run(|coordinator, spool, _, _| {
        let mut retention = coordinator.retain_response(spool, deadline()).unwrap();
        let mut worker = Worker {
            retention: &mut retention,
            execution: Deadline::after(Tick(0), 90).unwrap(),
            optional_end: 1,
            created: Default::default(),
            account: ACCOUNT,
            flags: Some((&creator, DEVICE)),
            mailboxes: None,
            drafts: Some(dispatch::DraftAccess {
                creator: &creator,
                device: DEVICE,
                state: None,
            }),
            identities: None,
            read: None,
            email_updates: false,
            submission: false,
            submissions: None,
            implicit: None,
        };
        let mut bytes = br#"{"draft":{"literal":true}}"#.to_vec();
        let mut tokens = [td_json::arena::Token::EMPTY; 8];
        let document = td_json::arena::parse(
            &mut bytes,
            &mut tokens,
            td_json::arena::ParseLimits {
                max_bytes: 64,
                max_depth: 8,
                max_tokens: 8,
            },
        )
        .unwrap();
        let mut name = Name::new();
        name.push(account().as_bytes()).unwrap();
        assert_eq!(
            worker.email_create(document.root(), name, None, 256),
            Err(Error::ResponseCapacity)
        );
        assert_eq!(creator.calls.load(Ordering::Relaxed), 0);
        assert_eq!(retention.len(), 0);
    });
}

#[test]
fn created_ids_merge_seeds_and_success_but_preserve_prior_values_on_failure() {
    for mode in [0, 1, 6, 7] {
        let creator = Creator {
            mode,
            ..Creator::default()
        };
        let input =
            td_json::json!({"using":[CORE,MAIL], "createdIds":{"draft":"prior","keep":"unchanged"},
            "methodCalls":[["Email/set", input(), "create"]]})
            .to_string();
        let response = run_batch(&input, &creator, true, 256, 1);
        let expected = if matches!(mode, 0 | 7) {
            "e03030303030303030303030303030303"
        } else {
            "prior"
        };
        assert_eq!(response["createdIds"]["draft"].as_str(), Some(expected));
        assert_eq!(response["createdIds"]["keep"].as_str(), Some("unchanged"));
        assert_eq!(creator.calls.load(Ordering::Relaxed), 1);
    }
    for seed in [None, Some(td_json::json!({}))] {
        let creator = Creator::default();
        let mut request =
            td_json::json!({"using":[CORE,MAIL],"methodCalls":[["Email/set",input(),"c"]]});
        if let Some(seed) = seed {
            request.insert("createdIds", seed);
        }
        let present = request.get("createdIds").is_some();
        let response = run_batch(&request.to_string(), &creator, true, 256, 7);
        assert_eq!(response.get("createdIds").is_some(), present);
        if present {
            assert_eq!(
                response["createdIds"]["draft"].as_str(),
                Some("e03030303030303030303030303030303")
            );
        }
    }
}
