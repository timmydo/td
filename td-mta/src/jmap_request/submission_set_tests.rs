#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{logical::Cell, DiskLimits, ViewMode, WorkLimits},
    ids::{AccountId, BlobId, EmailId, IdentityId, StoreEpoch, SubmissionId, ThreadId},
    jmap_request::{dispatch, response, CORE, MAIL, SUBMISSION},
    limits::Limits,
    ownership::SlotState,
    ports::{Deadline, Tick, Time},
    store_fs::{tests::Fixture, RequestSpool, ResponseCoordinator},
};
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
const EMAIL: EmailId = EmailId::from_bytes([3; 16]);
const IDENTITY: IdentityId = IdentityId::from_bytes([4; 16]);
const SUBMITTED: SubmissionId = SubmissionId::from_bytes([5; 16]);
const EPOCH: StoreEpoch = StoreEpoch::from_bytes([7; 16]);
struct Now(AtomicU64);
impl Clock for Now {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 1,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
struct Service {
    clock: Arc<Now>,
    creates: AtomicUsize,
    cancels: AtomicUsize,
    cancel_mode: u8,
    files: AtomicUsize,
    drafts: AtomicUsize,
    draft_mode: u8,
    create_mode: u8,
    file_mode: u8,
}
impl Service {
    fn new(create_mode: u8, file_mode: u8) -> Self {
        Self {
            clock: Arc::new(Now(AtomicU64::new(1))),
            creates: AtomicUsize::new(0),
            cancels: AtomicUsize::new(0),
            cancel_mode: 0,
            files: AtomicUsize::new(0),
            drafts: AtomicUsize::new(0),
            draft_mode: 0,
            create_mode,
            file_mode,
        }
    }
}
fn deadline(n: u64) -> Deadline {
    Deadline::after(Tick(0), n).unwrap()
}
fn id(value: impl wire::LocalId) -> String {
    wire::encode_id(value, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}
fn state() -> DataState {
    DataState {
        account: ACCOUNT,
        epoch: EPOCH,
        kind: DataType::Submission,
        sequence: Sequence::from_u64(1),
    }
}
impl SubmissionCreator for Service {
    fn recipient_maximum(&self) -> usize {
        100
    }
    fn create(
        &self,
        input: sending::Input,
        meter: &mut Meter,
    ) -> Result<submissions::Outcome, sending::Error> {
        assert_eq!(input.account, ACCOUNT);
        assert_eq!(input.device, DEVICE);
        assert_eq!(input.email, EMAIL);
        assert_eq!(input.identity, IDENTITY);
        assert_eq!(input.deadline, deadline(90));
        assert_eq!(meter.deadline(), deadline(100).into());
        self.creates.fetch_add(1, Ordering::Relaxed);
        match self.create_mode {
            1 => return Err(sending::Error::Publication(submissions::Error::Quota)),
            2 => {
                return Err(sending::Error::Publication(
                    submissions::Error::Indeterminate,
                ))
            }
            3 => self.clock.0.store(95, Ordering::Relaxed),
            4 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::State,
                ))
            }
            5 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::Envelope(
                        crate::submission_envelope::Error::NoRecipients,
                    ),
                ))
            }
            6 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::Envelope(
                        crate::submission_envelope::Error::TooManyRecipients,
                    ),
                ))
            }
            7 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::Identity,
                ))
            }
            8 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::Envelope(
                        crate::submission_envelope::Error::ForbiddenFrom,
                    ),
                ))
            }
            9 => return Err(sending::Error::Admission(ports::Error::Busy)),
            10 => {
                return Err(sending::Error::Publication(submissions::Error::Work(
                    ports::Error::Deadline,
                )))
            }
            11 => return Err(sending::Error::Admission(ports::Error::Capacity)),
            12 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::Read(
                        crate::postgres_read::Error::Connection(
                            crate::postgres_probe::Error::Transport,
                        ),
                    ),
                ))
            }
            13 | 14 => {
                return Err(sending::Error::Input(
                    crate::postgres_submission_input::Error::Read(
                        crate::postgres_read::Error::Body(crate::postgres_body::Error::Read(
                            if self.create_mode == 13 {
                                ports::Error::Deadline
                            } else {
                                ports::Error::Io {
                                    kind: std::io::ErrorKind::ConnectionAborted,
                                    os_code: None,
                                }
                            },
                        )),
                    ),
                ))
            }
            _ => (),
        }
        Ok(submissions::Outcome {
            submission: SUBMITTED,
            email: EMAIL,
            thread: ThreadId::from_bytes([8; 16]),
            blob: BlobId::from_bytes([9; 16]),
            length: 123,
            send_at: 1,
            expires_at: 2,
            old_sequence: 1 + self.drafts.load(Ordering::Relaxed) as u64,
            new_sequence: 2 + self.drafts.load(Ordering::Relaxed) as u64,
        })
    }
}
impl SubmissionFiler for Service {
    fn file(
        &self,
        input: filing::FilingRequest,
        meter: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError> {
        assert_eq!(self.creates.load(Ordering::Relaxed), 1);
        assert_eq!(input.account, ACCOUNT);
        assert_eq!(input.device, DEVICE);
        assert_eq!(input.email, EMAIL);
        assert_eq!(input.movement, None);
        assert_eq!(input.deadline, deadline(90));
        assert_eq!(meter.deadline(), deadline(100).into());
        self.files.fetch_add(1, Ordering::Relaxed);
        if self.file_mode == 1 {
            return Err(ports::Error::Busy.into());
        }
        Ok(EmailUpdateCompletion {
            before_epoch: if self.file_mode == 3 {
                StoreEpoch::from_bytes([99; 16])
            } else {
                EPOCH
            },
            before_sequence: Sequence::from_u64(2 + self.drafts.load(Ordering::Relaxed) as u64),
            outcome: if self.file_mode == 2 {
                Err(CommitError::Indeterminate(ports::Error::Corrupt))
            } else {
                Ok(Sequence::from_u64(
                    3 + self.drafts.load(Ordering::Relaxed) as u64,
                ))
            },
            results: vec![match self.file_mode {
                4 => EmailUpdateResult::InvalidMailbox,
                5 => EmailUpdateResult::NotFound,
                _ => EmailUpdateResult::Updated,
            }],
            admission_stopped: false,
        })
    }
}
fn input(hook: bool) -> td_json::Json {
    let mut args = td_json::json!({"accountId":id(ACCOUNT),"create":{"send":{"emailId":id(EMAIL),"identityId":id(IDENTITY)}}});
    if hook {
        let _ = args.insert(
            "onSuccessUpdateEmail",
            td_json::json!({"#send":{"keywords/$draft":null}}),
        );
    }
    td_json::json!({"using":[CORE,MAIL,SUBMISSION],"methodCalls":[["EmailSubmission/set",args,"call\n☃"]],"createdIds":{}})
}
fn run(input: td_json::Json, service: &Service) -> Result<td_json::Json, Error> {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool = RequestSpool::open(&mut root, service.clock.clone(), deadline(100)).unwrap();
    let resources = Limits::default().plan().unwrap();
    let disk = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let mut slots = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { Cell::EMPTY }; 4];
    let owner =
        ResponseCoordinator::new(spool, &disk, &mut slots, &mut cells, service.clock.clone())
            .unwrap();
    let retention = owner.retain_response(deadline(100)).unwrap();
    let request = dispatch::tests::request_with_capabilities(
        &input.to_string(),
        Limits::default(),
        &[CORE, MAIL, SUBMISSION],
    );
    let mut preflight = response::Preflight::new(request, "session", retention.maximum()).unwrap();
    while !preflight.step(&mut [0; 4096]).unwrap() {}
    let access = SubmissionAccess {
        creator: service,
        canceler: Some(service),
        filer: service,
        clock: &*service.clock,
        device: DEVICE,
        state: state(),
    };
    let plan = preflight.finish().unwrap();
    let mut completed = if input["methodCalls"].as_array().unwrap().len() == 2 {
        dispatch::run_with_postgres_send(plan, retention, deadline(90), access, service, service)
    } else {
        dispatch::run_with_postgres_submissions(plan, retention, deadline(90), access)
    }?;
    let mut encoder = completed.encoder().unwrap();
    while !encoder.count_step(&mut [0; 4096]).unwrap() {}
    let length = encoder.begin_output().unwrap();
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 7];
        let progress = encoder.poll_output(&mut buffer).unwrap();
        bytes.extend_from_slice(&buffer[..progress.written]);
        if progress.complete {
            break;
        }
    }
    assert_eq!(encoder.finish().unwrap(), length);
    assert_eq!(bytes.len() as u64, length);
    let result = td_json::parse_slice(&bytes).unwrap();
    drop(completed);
    assert_eq!(owner.used().unwrap(), 0);
    Ok(result)
}
#[test]
fn creation_and_filing_have_distinct_states_and_the_same_escaped_call_id() {
    let service = Service::new(0, 0);
    let result = run(input(true), &service).unwrap();
    assert_eq!(result["methodResponses"].as_array().unwrap().len(), 2);
    let first = &result["methodResponses"][0];
    let second = &result["methodResponses"][1];
    assert_eq!(first[0].as_str(), Some("EmailSubmission/set"));
    assert_eq!(
        first[1]["created"]["send"]["id"].as_str(),
        Some(id(SUBMITTED).as_str())
    );
    assert_eq!(
        DataState::decode(first[1]["oldState"].as_str().unwrap()).unwrap(),
        state()
    );
    assert_eq!(
        DataState::decode(first[1]["newState"].as_str().unwrap())
            .unwrap()
            .sequence
            .number(),
        2
    );
    assert_eq!(second[0].as_str(), Some("Email/set"));
    assert!(second[1]["updated"]
        .get(id(EMAIL).as_str())
        .unwrap()
        .is_null());
    assert_eq!(
        DataState::decode(second[1]["newState"].as_str().unwrap())
            .unwrap()
            .kind,
        DataType::Email
    );
    assert_eq!(
        DataState::decode(second[1]["newState"].as_str().unwrap())
            .unwrap()
            .sequence
            .number(),
        3
    );
    assert_eq!(first[2].as_str(), Some("call\n☃"));
    assert_eq!(first[2], second[2]);
    assert_eq!(
        result["createdIds"]["send"].as_str(),
        Some(id(SUBMITTED).as_str())
    );
}
#[test]
fn filing_failures_never_replace_known_submission_success() {
    for mode in 1..=5 {
        let service = Service::new(0, mode);
        let result = run(input(true), &service).unwrap();
        assert_eq!(
            result["methodResponses"][0][1]["created"]["send"]["id"].as_str(),
            Some(id(SUBMITTED).as_str())
        );
        let second = &result["methodResponses"][1];
        if mode <= 3 {
            assert_eq!(second[0].as_str(), Some("error"));
        } else {
            assert_eq!(second[0].as_str(), Some("Email/set"));
            assert!(second[1]["notUpdated"][id(EMAIL).as_str()]["type"]
                .as_str()
                .is_some());
        }
    }
    let service = Service::new(3, 0);
    let result = run(input(true), &service).unwrap();
    assert_eq!(
        result["methodResponses"][0][1]["created"]["send"]["id"].as_str(),
        Some(id(SUBMITTED).as_str())
    );
    assert_eq!(
        result["methodResponses"][1][1]["type"].as_str(),
        Some("serverUnavailable")
    );
    assert_eq!(service.files.load(Ordering::Relaxed), 0);
}
#[test]
fn failures_and_absent_hooks_do_not_file_and_uncertainty_is_explicit() {
    for mode in [1, 2] {
        let service = Service::new(mode, 0);
        let result = run(input(true), &service).unwrap();
        assert_eq!(result["methodResponses"].as_array().unwrap().len(), 1);
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
        if mode == 1 {
            assert_eq!(
                result["methodResponses"][0][1]["notCreated"]["send"]["type"].as_str(),
                Some("overQuota")
            );
        } else {
            assert_eq!(
                result["methodResponses"][0][1]["type"].as_str(),
                Some("serverPartialFail")
            );
        }
        assert!(result["createdIds"]["send"].is_null());
    }
    let service = Service::new(0, 0);
    let result = run(input(false), &service).unwrap();
    assert_eq!(result["methodResponses"].as_array().unwrap().len(), 1);
    assert_eq!(service.files.load(Ordering::Relaxed), 0);
}
#[test]
fn initial_creation_references_and_conditional_state_are_resolved() {
    let service = Service::new(0, 0);
    let mut request = input(false);
    let _ = request
        .get_mut("createdIds")
        .unwrap()
        .insert("draft", id(EMAIL));
    let _ = args_mut(&mut request)
        .get_mut("create")
        .unwrap()
        .get_mut("send")
        .unwrap()
        .insert("emailId", "#draft");
    assert!(
        run(request, &service).unwrap()["methodResponses"][0][1]["created"]["send"]["id"]
            .as_str()
            .is_some()
    );
    let service = Service::new(4, 0);
    let mut request = input(true);
    let _ = args_mut(&mut request).insert(
        "ifInState",
        state().encode(&mut [0; DataState::WIRE_BYTES]).unwrap(),
    );
    let result = run(request, &service).unwrap();
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("stateMismatch")
    );
    assert_eq!(service.files.load(Ordering::Relaxed), 0);
}
#[test]
fn missing_capability_bad_shapes_and_mixed_batches_refuse_before_effects() {
    let service = Service::new(0, 0);
    let mut request = input(true);
    let _ = request.insert("using", td_json::json!([CORE, MAIL]));
    assert_eq!(
        run(request, &service).unwrap()["methodResponses"][0][1]["type"].as_str(),
        Some("unknownMethod")
    );
    let mut request = input(true);
    let _ = args_mut(&mut request)
        .get_mut("create")
        .unwrap()
        .get_mut("send")
        .unwrap()
        .insert("emailId", "#missing");
    assert_eq!(
        run(request, &service).unwrap()["methodResponses"][0][1]["type"].as_str(),
        Some("invalidArguments")
    );
    let mut request = input(true);
    let _ = request.insert(
        "methodCalls",
        td_json::json!([["EmailSubmission/set", {}, "a"], ["Core/echo", {}, "b"]]),
    );
    assert!(matches!(
        run(request, &service),
        Err(Error::InvalidArguments)
    ));
    assert_eq!(service.creates.load(Ordering::Relaxed), 0);
    assert_eq!(service.files.load(Ordering::Relaxed), 0);
}

fn args_mut(request: &mut td_json::Json) -> &mut td_json::Json {
    let td_json::Json::Arr(calls) = request.get_mut("methodCalls").unwrap() else {
        panic!("calls");
    };
    let td_json::Json::Arr(call) = &mut calls[0] else {
        panic!("call");
    };
    &mut call[1]
}

#[test]
fn output_capacity_and_wrong_typed_overlays_refuse_before_effects() {
    for shadow in [false, true] {
        let service = Service::new(0, 0);
        crate::jmap_request::resolution::tests::run(|coordinator, spool, _, _| {
            let mut retention = coordinator.retain_response(spool, deadline(100)).unwrap();
            let mut created = super::super::super::creation_ids::CreationIds::default();
            if shadow {
                let slot = created.prepare(b"draft", &mut 10000, || Ok(())).unwrap();
                created
                    .created(slot, crate::ids::MailboxId::from_bytes([6; 16]))
                    .unwrap();
            }
            let mut worker = Worker {
                retention: &mut retention,
                execution: deadline(90),
                optional_end: if shadow { 10000 } else { 1 },
                created,
                account: ACCOUNT,
                flags: None,
                mailboxes: None,
                drafts: None,
                identities: None,
                read: None,
                email_updates: false,
                submission: true,
                submissions: Some(SubmissionAccess {
                    creator: &service,
                    canceler: Some(&service),
                    filer: &service,
                    clock: &*service.clock,
                    device: DEVICE,
                    state: state(),
                }),
                implicit: None,
            };
            let mut input = input(true);
            if shadow {
                let _ = input
                    .get_mut("createdIds")
                    .unwrap()
                    .insert("draft", id(EMAIL));
                let _ = args_mut(&mut input)
                    .get_mut("create")
                    .unwrap()
                    .get_mut("send")
                    .unwrap()
                    .insert("emailId", "#draft");
            }
            let request = dispatch::tests::request_with_capabilities(
                &input.to_string(),
                Limits::default(),
                &[CORE, MAIL, SUBMISSION],
            );
            let view = request.view().unwrap();
            assert_eq!(
                worker.submission_set(view.calls().next().unwrap(), request.view().unwrap(), 256),
                Err(if shadow {
                    Error::InvalidArguments
                } else {
                    Error::ResponseCapacity
                })
            );
            assert_eq!(service.creates.load(Ordering::Relaxed), 0);
            assert_eq!(service.files.load(Ordering::Relaxed), 0);
            assert_eq!(retention.len(), 0);
        });
    }
}

#[test]
fn creation_policy_errors_preserve_the_specific_refusal() {
    for (mode, kind) in [
        (5, "noRecipients"),
        (6, "tooManyRecipients"),
        (7, "invalidProperties"),
        (8, "forbiddenFrom"),
    ] {
        let service = Service::new(mode, 0);
        let result = run(input(true), &service).unwrap();
        let error = &result["methodResponses"][0][1]["notCreated"]["send"];
        assert_eq!(error["type"].as_str(), Some(kind));
        if mode == 6 {
            assert_eq!(error["maxRecipients"].as_u64(), Some(100));
        }
        if mode == 7 {
            assert_eq!(error["properties"][0].as_str(), Some("identityId"));
        }
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn opaque_mailboxes_refuse_only_filing_after_known_creation() {
    for (source, destination) in [
        ("opaque", id(crate::ids::MailboxId::from_bytes([6; 16]))),
        ("m06060606060606060606060606060606", "opaque".to_owned()),
    ] {
        let service = Service::new(0, 0);
        let mut request = input(true);
        let patch = td_json::json!({"keywords/$draft":null,format!("mailboxIds/{source}"):null,format!("mailboxIds/{destination}"):true});
        let _ =
            args_mut(&mut request).insert("onSuccessUpdateEmail", td_json::json!({"#send":patch}));
        let result = run(request, &service).unwrap();
        assert_eq!(
            result["methodResponses"][0][1]["created"]["send"]["id"].as_str(),
            Some(id(SUBMITTED).as_str())
        );
        assert_eq!(
            result["methodResponses"][1][1]["notUpdated"][id(EMAIL).as_str()]["type"].as_str(),
            Some("invalidProperties")
        );
        assert_eq!(service.creates.load(Ordering::Relaxed), 1);
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn transient_failures_and_deterministic_work_have_method_errors() {
    for (mode, kind, description) in [
        (9, "serverUnavailable", "storageBusy"),
        (10, "serverUnavailable", "executionDeadline"),
        (11, "serverFail", "storageWorkLimit"),
        (12, "serverUnavailable", "storageBusy"),
        (13, "serverUnavailable", "executionDeadline"),
        (14, "serverUnavailable", "storageBusy"),
    ] {
        let service = Service::new(mode, 0);
        let result = run(input(true), &service).unwrap();
        assert_eq!(result["methodResponses"][0][0].as_str(), Some("error"));
        assert_eq!(result["methodResponses"][0][1]["type"].as_str(), Some(kind));
        assert_eq!(
            result["methodResponses"][0][1]["description"].as_str(),
            Some(description)
        );
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}

impl dispatch::DraftCreator for Service {
    fn create_draft(
        &self,
        input: dispatch::DraftInput<'_>,
        meter: &mut Meter,
    ) -> Result<crate::store_fs::DraftCompletion, dispatch::DraftError> {
        assert_eq!(input.account, ACCOUNT);
        assert_eq!(input.device, DEVICE);
        assert_eq!(input.deadline, deadline(90));
        assert_eq!(meter.deadline(), deadline(100).into());
        self.drafts.fetch_add(1, Ordering::Relaxed);
        if self.draft_mode == 1 {
            return Err(dispatch::DraftError::Input(
                crate::draft_input::Error::Property("from"),
            ));
        }
        Ok(crate::store_fs::DraftCompletion {
            before_epoch: EPOCH,
            before_sequence: Sequence::from_u64(1),
            outcome: if self.draft_mode == 2 {
                Err(CommitError::Indeterminate(ports::Error::Corrupt))
            } else {
                Ok(Sequence::from_u64(2))
            },
            email: EMAIL,
            blob: BlobId::from_bytes([9; 16]),
            thread: ThreadId::from_bytes([8; 16]),
            size: 123,
            admission_stopped: false,
            cleanup_error: None,
        })
    }
}
impl dispatch::EmailMutator for Service {
    fn update(
        &self,
        _: crate::store_fs::EmailUpdateRequest<'_>,
        _: &mut Meter,
    ) -> Result<EmailUpdateCompletion, UploadError> {
        panic!("send batch must create its draft");
    }
}
fn batch() -> td_json::Json {
    let mut request = input(true);
    let _ = args_mut(&mut request)
        .get_mut("create")
        .unwrap()
        .get_mut("send")
        .unwrap()
        .insert("emailId", "#draft");
    let td_json::Json::Arr(calls) = request.get_mut("methodCalls").unwrap() else {
        panic!("calls");
    };
    calls.insert(0, td_json::json!(["Email/set", {"accountId":id(ACCOUNT),"create":{"draft":{"subject":"fixture"}}}, "draft-call"]));
    request
}
#[test]
fn draft_submission_and_filing_keep_order_and_transaction_states() {
    let service = Service::new(0, 0);
    let result = run(batch(), &service).unwrap();
    let responses = result["methodResponses"].as_array().unwrap();
    assert_eq!(responses.len(), 3);
    for (index, name) in ["Email/set", "EmailSubmission/set", "Email/set"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(responses[index][0].as_str(), Some(name));
        assert_eq!(
            DataState::decode(responses[index][1]["oldState"].as_str().unwrap())
                .unwrap()
                .sequence
                .number(),
            index as u64 + 1
        );
        assert_eq!(
            DataState::decode(responses[index][1]["newState"].as_str().unwrap())
                .unwrap()
                .sequence
                .number(),
            index as u64 + 2
        );
    }
    assert_eq!(responses[0][2].as_str(), Some("draft-call"));
    assert_eq!(responses[1][2], responses[2][2]);
    assert_eq!(
        result["createdIds"]["draft"].as_str(),
        Some(id(EMAIL).as_str())
    );
    assert_eq!(
        result["createdIds"]["send"].as_str(),
        Some(id(SUBMITTED).as_str())
    );
}
#[test]
fn submission_failure_keeps_the_draft_and_its_latest_state() {
    let service = Service::new(1, 0);
    let result = run(batch(), &service).unwrap();
    let responses = result["methodResponses"].as_array().unwrap();
    assert_eq!(responses.len(), 2);
    assert_eq!(
        responses[0][1]["created"]["draft"]["id"].as_str(),
        Some(id(EMAIL).as_str())
    );
    assert_eq!(
        responses[1][1]["notCreated"]["send"]["type"].as_str(),
        Some("overQuota")
    );
    for key in ["oldState", "newState"] {
        assert_eq!(
            DataState::decode(responses[1][1][key].as_str().unwrap())
                .unwrap()
                .sequence
                .number(),
            2
        );
    }
    assert_eq!(service.files.load(Ordering::Relaxed), 0);
}
#[test]
fn failed_or_uncertain_draft_is_not_submitted() {
    for mode in [1, 2] {
        let mut service = Service::new(0, 0);
        service.draft_mode = mode;
        let result = run(batch(), &service).unwrap();
        assert_eq!(result["methodResponses"].as_array().unwrap().len(), 2);
        assert_eq!(
            result["methodResponses"][1][1]["type"].as_str(),
            Some("invalidArguments")
        );
        assert_eq!(service.creates.load(Ordering::Relaxed), 0);
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn send_batch_rejects_update_and_destroy_first_calls_before_effects() {
    for destroy in [false, true] {
        let service = Service::new(1, 0);
        let mut request = batch();
        let args = args_mut(&mut request);
        let _ = args.insert("create", td_json::Json::Null);
        if destroy {
            let _ = args.insert("destroy", td_json::json!([id(EMAIL)]));
        } else {
            let _ = args.insert(
                "update",
                td_json::json!({id(EMAIL):{"keywords/$seen":true}}),
            );
        }
        assert!(matches!(
            run(request, &service),
            Err(Error::InvalidArguments)
        ));
        assert_eq!(service.drafts.load(Ordering::Relaxed), 0);
        assert_eq!(service.creates.load(Ordering::Relaxed), 0);
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}

impl crate::jmap_request::dispatch::SubmissionCanceler for Service {
    fn cancel(
        &self,
        input: crate::postgres_http_cancellations::Input,
        meter: &mut Meter,
    ) -> Result<crate::postgres_submission_cancel::Outcome, crate::postgres_http_cancellations::Error>
    {
        use crate::{
            postgres_http_cancellations::Error as Http, postgres_submission_cancel as cancel,
        };
        assert_eq!(input.account, ACCOUNT);
        assert_eq!(input.device, DEVICE);
        assert_eq!(input.submission, SUBMITTED);
        assert_eq!(input.deadline, deadline(90));
        assert_eq!(meter.deadline(), deadline(100).into());
        self.cancels.fetch_add(1, Ordering::Relaxed);
        let error = match self.cancel_mode {
            1 => Some(cancel::Error::CannotUnsend),
            2 => Some(cancel::Error::Indeterminate),
            4 => Some(cancel::Error::StateMismatch),
            5 => Some(cancel::Error::Missing),
            6 => Some(cancel::Error::Unauthenticated),
            7 => Some(cancel::Error::Capacity),
            8 => Some(cancel::Error::Deadline),
            12 => Some(cancel::Error::Fence),
            _ => None,
        };
        if let Some(error) = error {
            return Err(Http::Publication(error));
        }
        if self.cancel_mode == 9 {
            return Err(Http::Admission(ports::Error::Capacity));
        }
        if self.cancel_mode == 13 {
            return Err(Http::Admission(ports::Error::Busy));
        }
        if self.cancel_mode == 3 {
            self.clock.0.store(95, Ordering::Relaxed);
        }
        Ok(cancel::Outcome {
            submission: SUBMITTED,
            recipients: 1000,
            changed: match self.cancel_mode {
                10 => 1,
                11 => 0,
                _ => 1000,
            },
            completed_at: 0,
            old_sequence: 1,
            new_sequence: if self.cancel_mode == 11 { 1 } else { 2 },
        })
    }
}
fn cancellation_input(patch: td_json::Json) -> td_json::Json {
    td_json::json!({"using":[CORE,MAIL,SUBMISSION],"methodCalls":[["EmailSubmission/set",{
        "accountId":id(ACCOUNT),"update":{id(SUBMITTED):patch}
    },"cancel\n☃"]]})
}
#[test]
fn cancellation_dispatch_preserves_committed_and_no_change_results() {
    for mode in [0, 3, 11] {
        let mut service = Service::new(0, 0);
        service.cancel_mode = mode;
        let result = run(
            cancellation_input(td_json::json!({"undoStatus":"canceled"})),
            &service,
        )
        .unwrap();
        let response = &result["methodResponses"][0];
        assert_eq!(response[0].as_str(), Some("EmailSubmission/set"));
        assert_eq!(response[2].as_str(), Some("cancel\n☃"));
        assert!(response[1]["updated"]
            .get(id(SUBMITTED).as_str())
            .unwrap()
            .is_null());
        assert_eq!(
            DataState::decode(response[1]["oldState"].as_str().unwrap()).unwrap(),
            state()
        );
        assert_eq!(
            DataState::decode(response[1]["newState"].as_str().unwrap())
                .unwrap()
                .sequence
                .number(),
            if mode == 11 { 1 } else { 2 }
        );
        assert_eq!(service.cancels.load(Ordering::Relaxed), 1);
        assert_eq!(service.creates.load(Ordering::Relaxed), 0);
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn cancellation_dispatch_distinguishes_refusal_from_uncertainty_and_limits() {
    for (mode, kind, method) in [
        (1, "cannotUnsend", false),
        (2, "serverPartialFail", true),
        (5, "notFound", false),
        (6, "forbidden", false),
        (7, "cannotUnsend", false),
        (8, "serverUnavailable", true),
        (9, "serverFail", true),
        (10, "serverPartialFail", true),
        (12, "cannotUnsend", false),
        (13, "serverUnavailable", true),
    ] {
        let mut service = Service::new(0, 0);
        service.cancel_mode = mode;
        let result = run(
            cancellation_input(td_json::json!({"undoStatus":"canceled"})),
            &service,
        )
        .unwrap();
        let response = &result["methodResponses"][0];
        if method {
            assert_eq!(response[0].as_str(), Some("error"));
            assert_eq!(response[1]["type"].as_str(), Some(kind));
        } else {
            assert_eq!(response[0].as_str(), Some("EmailSubmission/set"));
            assert_eq!(
                response[1]["notUpdated"][id(SUBMITTED).as_str()]["type"].as_str(),
                Some(kind)
            );
        }
        assert_eq!(service.cancels.load(Ordering::Relaxed), 1);
    }
}
#[test]
fn cancellation_dispatch_rejects_unsupported_patches_before_publication() {
    for patch in [
        td_json::json!({}),
        td_json::json!({"undoStatus":"pending"}),
        td_json::json!({"undoStatus":"canceled","emailId":id(EMAIL)}),
    ] {
        let service = Service::new(0, 0);
        let result = run(cancellation_input(patch), &service).unwrap();
        assert_eq!(
            result["methodResponses"][0][1]["notUpdated"][id(SUBMITTED).as_str()]["type"].as_str(),
            Some("invalidProperties")
        );
        assert_eq!(service.cancels.load(Ordering::Relaxed), 0);
    }
}
#[test]
fn cancellation_dispatch_checks_state_before_patch_and_again_at_publication() {
    let service = Service::new(0, 0);
    let mut request = cancellation_input(td_json::json!({"undoStatus":"pending"}));
    let stale = DataState {
        sequence: Sequence::from_u64(0),
        ..state()
    };
    let _ = args_mut(&mut request).insert(
        "ifInState",
        stale.encode(&mut [0; DataState::WIRE_BYTES]).unwrap(),
    );
    let result = run(request, &service).unwrap();
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("stateMismatch")
    );
    assert_eq!(service.cancels.load(Ordering::Relaxed), 0);
    let mut service = Service::new(0, 0);
    service.cancel_mode = 4;
    let mut request = cancellation_input(td_json::json!({"undoStatus":"canceled"}));
    let _ = args_mut(&mut request).insert(
        "ifInState",
        state().encode(&mut [0; DataState::WIRE_BYTES]).unwrap(),
    );
    let result = run(request, &service).unwrap();
    assert_eq!(
        result["methodResponses"][0][1]["type"].as_str(),
        Some("stateMismatch")
    );
    assert_eq!(service.cancels.load(Ordering::Relaxed), 1);
}
#[test]
fn cancellation_dispatch_refuses_mixed_and_multiple_updates_before_effects() {
    for variant in 0..3 {
        let service = Service::new(0, 0);
        let mut request = cancellation_input(td_json::json!({"undoStatus":"canceled"}));
        let args = args_mut(&mut request);
        match variant {
            0 => {
                let _ = args.insert(
                    "create",
                    td_json::json!({"send":{"emailId":id(EMAIL),"identityId":id(IDENTITY)}}),
                );
            }
            1 => {
                let _ = args.insert(
                    "onSuccessUpdateEmail",
                    td_json::json!({id(SUBMITTED):{"keywords/$draft":null}}),
                );
            }
            _ => {
                let _ = args
                    .get_mut("update")
                    .unwrap()
                    .insert("other", td_json::json!({"undoStatus":"canceled"}));
            }
        }
        let result = run(request, &service).unwrap();
        assert_eq!(
            result["methodResponses"][0][1]["type"].as_str(),
            Some("invalidArguments")
        );
        assert_eq!(service.cancels.load(Ordering::Relaxed), 0);
        assert_eq!(service.creates.load(Ordering::Relaxed), 0);
        assert_eq!(service.files.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn send_batch_rejects_cancellation_before_creating_a_draft() {
    for reference in [false, true] {
        let service = Service::new(0, 0);
        let mut request = cancellation_input(td_json::json!({"undoStatus":"canceled"}));
        if reference {
            let args = args_mut(&mut request);
            let _ = args.remove("update");
            let _ = args.insert(
                "#update",
                td_json::json!({"resultOf":"draft-call","name":"Email/set","path":"/created"}),
            );
        }
        let td_json::Json::Arr(calls) = request.get_mut("methodCalls").unwrap() else {
            panic!("calls");
        };
        calls.insert(0, td_json::json!(["Email/set", {"accountId":id(ACCOUNT),"create":{"draft":{"subject":"fixture"}}}, "draft-call"]));
        assert!(matches!(
            run(request, &service),
            Err(Error::InvalidArguments)
        ));
        assert_eq!(service.drafts.load(Ordering::Relaxed), 0);
        assert_eq!(service.cancels.load(Ordering::Relaxed), 0);
    }
}
