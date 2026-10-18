use super::{deadline, sql, Fixture};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use td_mta::{
    admission::{DiskLimits, ViewMode, WorkLimits},
    clock::RuntimeClock,
    ids::{DeviceId, SubmissionId},
    jmap_request::{dispatch, response, OwnedValidation, CORE, MAIL, SUBMISSION},
    limits::Limits,
    ports::{Clock, Deadline},
    postgres_http_cancellations as canceling, postgres_http_submissions as sending,
    postgres_http_updates as filing,
    postgres_input::Loaded,
    postgres_read::{ReadSlots, Snapshot},
    postgres_route_owners as owners,
    postgres_runtime_profile::Binding,
    store_fs::{PrivateRoot, RequestSpool, ResponseCoordinator},
    sync::{DataState, DataType},
    wire,
};

pub(super) struct Context<'a> {
    pub loaded: &'a Loaded,
    pub binding: &'a Binding,
    pub device: DeviceId,
    pub password: &'a [u8],
    pub claim: &'a owners::Claim,
    pub generation: u64,
    pub submission: SubmissionId,
}
#[derive(Clone, Copy)]
enum Patch {
    Valid,
    Invalid,
    Stale,
}
struct Response {
    json: td_json::Json,
    state: DataState,
    closed: usize,
}
fn id(value: impl wire::LocalId) -> String {
    wire::encode_id(value, &mut [0; wire::IdKind::WIRE_BYTES])
        .unwrap()
        .to_owned()
}

pub(super) fn exercise(fixture: &Fixture, context: Context<'_>) {
    for frontend in [false, true] {
        exercise_mode(fixture, &context, frontend);
    }
}
fn exercise_mode(fixture: &Fixture, context: &Context<'_>, frontend: bool) {
    let pending = canceling::Pending::default();
    let sub = format!("decode('{}','hex')", context.submission);
    let reset = || {
        sql(fixture, &format!("UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,rcpt_reply=NULL,data_reply=NULL,reason=0,diagnostic='' WHERE submission_id={sub}; UPDATE td_mta.submissions SET completed_at=NULL WHERE id={sub}"));
    };
    let invalid = run(context, &pending, Patch::Invalid, 9000, frontend);
    assert_eq!(
        invalid.json["methodResponses"][0][1]["notUpdated"][id(context.submission).as_str()]
            ["type"]
            .as_str(),
        Some("invalidProperties")
    );
    assert_eq!(invalid.closed, 0);
    let stale = run(context, &pending, Patch::Stale, 9000, frontend);
    assert_eq!(
        stale.json["methodResponses"][0][1]["type"].as_str(),
        Some("stateMismatch")
    );
    assert_eq!(stale.closed, 0);
    sql(fixture, &format!("UPDATE td_mta.recipients SET state=4,attempt_id=decode(repeat('01',16),'hex'),attempt_count=1,last_attempt_at=0,phase=4,next_attempt_at=NULL,rcpt_reply='250 ok',data_reply='250 accepted' WHERE submission_id={sub} AND ordinal=0"));
    let ineligible = run(context, &pending, Patch::Valid, 9000, frontend);
    assert_eq!(
        ineligible.json["methodResponses"][0][1]["notUpdated"][id(context.submission).as_str()]
            ["type"]
            .as_str(),
        Some("cannotUnsend")
    );
    assert_eq!(ineligible.closed, 0);
    reset();
    let changed = run(context, &pending, Patch::Valid, 9000, frontend);
    assert_updated(&changed, context.submission, 1);
    assert_eq!(changed.closed, usize::from(!frontend));
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=6"
            )
        ),
        "1000"
    );
    let noop = run(context, &pending, Patch::Valid, 9000, frontend);
    assert_updated(&noop, context.submission, 0);
    assert_eq!(noop.closed, 0);
    assert_eq!(
        noop.state.sequence.number(),
        changed.state.sequence.number() + 1
    );
    assert_eq!(pending.operation().unwrap(), None);
    reset();
    let mut connection = context
        .loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    owners::renew(
        &connection,
        &context.loaded.trust,
        deadline(),
        context.claim,
        context.generation,
    )
    .unwrap();
    sql(fixture, "CREATE CONSTRAINT TRIGGER delay_jmap_cancel AFTER INSERT ON td_mta.submission_cancel_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_cancel()");
    let lost = run(context, &pending, Patch::Valid, 2000, frontend);
    assert_eq!(lost.json["methodResponses"][0][0].as_str(), Some("error"));
    assert_eq!(
        lost.json["methodResponses"][0][1]["type"].as_str(),
        Some("serverPartialFail")
    );
    assert_eq!(lost.closed, usize::from(!frontend));
    let operation = pending.operation().unwrap().unwrap();
    thread::sleep(Duration::from_secs(4));
    sql(
        fixture,
        "DROP TRIGGER delay_jmap_cancel ON td_mta.submission_cancel_receipts",
    );
    let clock = RuntimeClock::new();
    let until = Deadline::after(clock.sample().unwrap().monotonic, 5000).unwrap();
    let settled = pending
        .settle(
            context.loaded,
            Path::new(env!("CARGO_BIN_EXE_td-mta")),
            &ReadSlots::new(1).unwrap(),
            &clock,
            until,
        )
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(settled.outcome.old_sequence, lost.state.sequence.number());
    assert_eq!(
        settled.outcome.new_sequence,
        lost.state.sequence.number() + 1
    );
    assert_eq!(settled.outcome.changed, 1000);
    assert_eq!(pending.operation().unwrap(), None);
    reset();
    owners::renew(
        &connection,
        &context.loaded.trust,
        deadline(),
        context.claim,
        context.generation,
    )
    .unwrap();
}
fn assert_updated(response: &Response, submission: SubmissionId, advance: u64) {
    let methods = response.json["methodResponses"].as_array().unwrap();
    assert_eq!(methods.len(), 1);
    let method = &methods[0];
    assert_eq!(method[0].as_str(), Some("EmailSubmission/set"));
    assert_eq!(method[2].as_str(), Some("cancel\n☃"));
    assert!(method[1]["updated"]
        .get(id(submission).as_str())
        .unwrap()
        .is_null());
    assert_eq!(
        DataState::decode(method[1]["oldState"].as_str().unwrap()).unwrap(),
        response.state
    );
    let after = DataState::decode(method[1]["newState"].as_str().unwrap()).unwrap();
    assert_eq!(
        after.sequence.number(),
        response.state.sequence.number() + advance
    );
    assert_eq!(
        (after.account, after.epoch, after.kind),
        (
            response.state.account,
            response.state.epoch,
            DataType::Submission
        )
    );
    assert!(response.json["createdIds"].as_object().unwrap().is_empty());
}
fn run(
    context: &Context<'_>,
    pending: &canceling::Pending,
    patch: Patch,
    execution_ms: u64,
    frontend: bool,
) -> Response {
    let slots = ReadSlots::new(1).unwrap();
    let mut connection = context
        .loaded
        .bootstrap
        .request(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let snapshot = Snapshot::open_runtime(
        &slots,
        &connection,
        &context.loaded.trust,
        deadline(),
        context.binding,
        context.password,
    )
    .unwrap();
    let view = snapshot.identity();
    let state = DataState {
        account: view.account,
        epoch: view.epoch,
        kind: DataType::Submission,
        sequence: view.committed_sequence,
    };
    drop(snapshot);
    let expected = DataState {
        sequence: td_mta::format::Sequence::from_u64(
            state.sequence.number() - u64::from(matches!(patch, Patch::Stale)),
        ),
        ..state
    };
    let input = td_json::json!({"using":[CORE,MAIL,SUBMISSION],"createdIds":{},"methodCalls":[["EmailSubmission/set",{
        "accountId":id(context.binding.account()), "ifInState":expected.encode(&mut [0;DataState::WIRE_BYTES]).unwrap(),
        "update":{id(context.submission):{"undoStatus":if matches!(patch, Patch::Invalid) {"pending"} else {"canceled"}}}
    },"cancel\n☃"]]}).to_string();
    let limits = Limits::default();
    let mut bytes = vec![0; limits.json_bytes].into_boxed_slice();
    bytes[..input.len()].copy_from_slice(input.as_bytes());
    let tokens = vec![td_json::arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
    let mut parser = td_json::arena::OwnedCursor::with_input_length(
        bytes,
        tokens,
        input.len(),
        td_json::arena::ParseLimits {
            max_bytes: limits.json_bytes,
            max_depth: limits.json_depth,
            max_tokens: limits.json_tokens,
        },
    )
    .unwrap();
    while !parser.step().unwrap() {}
    let resources = limits.plan().unwrap();
    let mut validation = OwnedValidation::new(
        parser.finish().unwrap(),
        &resources,
        &[CORE, MAIL, SUBMISSION],
    )
    .unwrap();
    while !validation.step().unwrap() {}
    let request = validation.finish().unwrap();
    let directory = td_fs::TempDir::new("td-mta-cancel-jmap").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut root = PrivateRoot::open(directory.path().to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let clock = Arc::new(RuntimeClock::new());
    let now = clock.sample().unwrap().monotonic;
    let until = Deadline::after(now, 10_000).unwrap();
    let execution = Deadline::after(now, execution_ms).unwrap();
    let spool = RequestSpool::open(&mut root, clock.clone(), until).unwrap();
    let disk = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let mut states = [const { td_mta::ownership::SlotState::EMPTY }; 4];
    let mut cells = [const { td_mta::admission::logical::Cell::EMPTY }; 4];
    let owner =
        ResponseCoordinator::new(spool, &disk, &mut states, &mut cells, clock.clone()).unwrap();
    let retention = owner.retain_response(until).unwrap();
    let mut preflight = response::Preflight::new(request, "session", retention.maximum()).unwrap();
    while !preflight.step(&mut [0; 4096]).unwrap() {}
    let executable = Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let other = filing::Pending::default();
    let creator = sending::Submissions {
        loaded: context.loaded,
        executable,
        binding: context.binding,
        slots: &slots,
        clock: &clock,
        pending: &other.submissions,
        password: context.password,
    };
    let filer = filing::Updates {
        loaded: context.loaded,
        executable,
        binding: context.binding,
        slots: &slots,
        clock: &clock,
        pending: &other,
        password: context.password,
    };
    let worker = canceling::Cancellations {
        loaded: context.loaded,
        executable,
        binding: context.binding,
        slots: &slots,
        clock: &clock,
        pending,
        password: context.password,
    };
    let closed = Arc::new(AtomicUsize::new(0));
    let observed = closed.clone();
    let canceler = dispatch::RouteCancellation {
        worker: &worker,
        claim: context.claim,
        generation: context.generation,
        close: Arc::new(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }),
    };
    let access = dispatch::SubmissionAccess {
        creator: &creator,
        canceler: Some(if frontend { &worker } else { &canceler }),
        filer: &filer,
        clock: &*clock,
        device: context.device,
        state,
    };
    let mut completed = dispatch::run_with_postgres_submissions(
        preflight.finish().unwrap(),
        retention,
        execution,
        access,
    )
    .unwrap();
    let mut encoder = completed.encoder().unwrap();
    while !encoder.count_step(&mut [0; 4096]).unwrap() {}
    let length = encoder.begin_output().unwrap();
    let mut output = Vec::new();
    loop {
        let mut bytes = [0; 17];
        let progress = encoder.poll_output(&mut bytes).unwrap();
        output.extend_from_slice(&bytes[..progress.written]);
        if progress.complete {
            break;
        }
    }
    assert_eq!(encoder.finish().unwrap(), length);
    assert_eq!(output.len() as u64, length);
    let json = td_json::parse_slice(&output).unwrap();
    drop(completed);
    assert_eq!(owner.used().unwrap(), 0);
    Response {
        json,
        state,
        closed: closed.load(Ordering::Relaxed),
    }
}
