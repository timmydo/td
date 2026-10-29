use super::{deadline, sql, Fixture, PASSWORD};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use td_crypto::TrustStore;
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    generations::GenerationSet,
    ids::SubmissionId,
    ports::{self, Clock, Deadline},
    postgres_devices as devices,
    postgres_http_cancellations::{self as http, Error},
    postgres_input::Loaded,
    postgres_probe::Request,
    postgres_read::{ReadSlots, Snapshot},
    postgres_route_owners as owners,
    postgres_runtime_profile::{self as runtime, Binding},
    postgres_submission_cancel as cancel,
    tls_policy::TlsPolicies,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    claim: &owners::Claim,
    generation: u64,
    submission: SubmissionId,
) {
    let domain = GenerationSet::<TlsPolicies>::at_startup();
    let jmap = runtime::load(
        request,
        trust,
        deadline(),
        binding.epoch(),
        "desktop",
        domain.reserve().unwrap(),
    )
    .unwrap();
    let binding = jmap.binding();
    let device =
        devices::prepare_creation(binding.epoch(), binding.account(), "cancel-worker", 0).unwrap();
    devices::publish_creation(request, trust, deadline(), &device).unwrap();
    let loaded = Loaded {
        bootstrap: td_mta::postgres_bootstrap::Bootstrap::parse(
            &format!(
                "postgresql://mail_runtime:{}@localhost:{}/postgres",
                std::str::from_utf8(PASSWORD).unwrap(),
                fixture.port
            ),
            binding.profile(),
        )
        .unwrap(),
        trust: TrustStore::from_pem(&fixture.ca).unwrap(),
    };
    let clock = RuntimeClock::new();
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let capacity = Charge {
        io_bytes: 16 * 1024 * 1024,
        records: 100_000,
        ..Default::default()
    };
    let meter = || Meter::new(until(10_000), capacity);
    let pending = http::Pending::default();
    let slots = ReadSlots::new(1).unwrap();
    let settlement = ReadSlots::new(1).unwrap();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let worker = http::Cancellations {
        loaded: &loaded,
        executable,
        binding,
        slots: &slots,
        clock: &clock,
        pending: &pending,
        password: device.password(),
    };
    let input = |ms| http::Input {
        account: binding.account(),
        device: device.device_id(),
        submission,
        expected: None,
        deadline: until(ms),
    };
    let refused = |_| panic!("refused request fenced");
    let mut empty = Meter::new(until(10_000), Charge::default());
    assert_eq!(
        worker.cancel(input(10_000), claim, generation, &mut empty, refused),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert!(empty.stopped().is_some());
    assert_eq!(
        worker.cancel(input(0), claim, generation, &mut meter(), refused),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut expired = Meter::new(until(0), capacity);
    assert_eq!(
        worker.cancel(input(10_000), claim, generation, &mut expired, refused),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut wrong = input(10_000);
    wrong.device = td_mta::ids::DeviceId::from_bytes([0xff; 16]);
    assert_eq!(
        worker.cancel(wrong, claim, generation, &mut meter(), refused),
        Err(Error::Admission(ports::Error::Forbidden))
    );
    assert_eq!(pending.operation().unwrap(), None);
    assert_eq!(
        worker.cancel(input(10_000), claim, generation, &mut meter(), |_| Err(
            cancel::Error::Fence
        )),
        Err(Error::Publication(cancel::Error::Fence))
    );
    assert_eq!(pending.operation().unwrap(), None);
    super::cancel_jmap::exercise(
        fixture,
        super::cancel_jmap::Context {
            loaded: &loaded,
            binding,
            device: device.device_id(),
            password: device.password(),
            claim,
            generation,
            submission,
        },
    );
    let closed = Arc::new(AtomicBool::new(false));
    let observed = closed.clone();
    let adapter = td_mta::jmap_request::dispatch::RouteCancellation {
        worker: &worker,
        claim,
        generation,
        close: Arc::new(move |_| {
            observed.store(true, Ordering::Relaxed);
            Ok(())
        }),
    };
    let success = td_mta::jmap_request::dispatch::SubmissionCanceler::cancel(
        &adapter,
        input(10_000),
        &mut meter(),
    )
    .unwrap();
    assert_eq!(success.changed, 1000);
    assert!(closed.load(Ordering::Relaxed));
    assert_eq!(pending.operation().unwrap(), None);
    sql(fixture, &format!("UPDATE td_mta.recipients SET state=1,reason=0,next_attempt_at=0 WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL WHERE id=decode('{submission}','hex')"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    sql(fixture, "CREATE CONSTRAINT TRIGGER delay_http_cancel AFTER INSERT ON td_mta.submission_cancel_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_cancel()");
    assert_eq!(
        worker.cancel(input(2000), claim, generation, &mut meter(), |_| Ok(())),
        Err(Error::Publication(cancel::Error::Indeterminate))
    );
    let operation = pending.operation().unwrap().unwrap();
    assert_eq!(
        worker.cancel(input(10_000), claim, generation, &mut meter(), refused),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert_eq!(pending.operation().unwrap(), Some(operation));
    thread::sleep(Duration::from_secs(4));
    sql(
        fixture,
        "DROP TRIGGER delay_http_cancel ON td_mta.submission_cancel_receipts",
    );
    let mut connection = loaded
        .bootstrap
        .request(td_mta::postgres_probe::Endpoint::Tcp(
            ([127, 0, 0, 1], loaded.bootstrap.port()).into(),
        ));
    connection.mode = td_mta::postgres_probe::Mode::Runtime;
    let foreground = Snapshot::open_runtime(
        &slots,
        &connection,
        &loaded.trust,
        deadline(),
        binding,
        device.password(),
    )
    .unwrap();
    let revoke =
        devices::prepare_revocation(binding.epoch(), binding.account(), device.device_id(), 1)
            .unwrap();
    devices::publish_revocation(request, trust, deadline(), &revoke).unwrap();
    assert!(matches!(
        pending.settle(&loaded, executable, &settlement, &clock, until(0)),
        Err(Error::Admission(ports::Error::Deadline))
    ));
    assert_eq!(pending.operation().unwrap(), Some(operation));
    let settled = pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .unwrap();
    assert_eq!(settled.operation, operation);
    assert_eq!(settled.outcome.changed, 1000);
    assert_eq!(settled.outcome.old_sequence, success.new_sequence);
    assert_eq!(pending.operation().unwrap(), None);
    drop(foreground);
    assert!(pending
        .settle(&loaded, executable, &settlement, &clock, until(10_000))
        .unwrap()
        .is_none());
    sql(
        fixture,
        &format!(
            "DELETE FROM td_mta.devices WHERE account=decode('{}','hex') AND id=decode('{}','hex')",
            binding.account(),
            device.device_id()
        ),
    );
}
