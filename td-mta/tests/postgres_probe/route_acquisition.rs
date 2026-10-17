use super::{deadline, sql, Fixture, PASSWORD};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use td_crypto::TrustStore;
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    ids::{AttemptId, BootId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_dispatch::{self as dispatch, Error, Identity},
    postgres_probe::{Mode, Request},
    postgres_queue_candidates::Kind,
    postgres_read::ReadSlots,
    postgres_route_owners::{self as owners, Claim, Grant},
    postgres_runtime_profile::Binding,
    queue_clock::Guard,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    historical: &Claim,
    uncertain: &Claim,
) -> Grant {
    super::create_runtime_login(fixture);
    super::grant_runtime_mail(fixture);
    let clock = Arc::new(RuntimeClock::new());
    let slots = ReadSlots::new(1).unwrap();
    let settlement_slots = ReadSlots::new(1).unwrap();
    let pending = dispatch::Pending::default();
    let runtime = Request {
        address: request.address,
        server_name: request.server_name,
        database: request.database,
        user: "mail_runtime",
        password: PASSWORD,
        mode: Mode::Runtime,
    };
    let worker = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &runtime,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: Arc::new(Mutex::new(Guard::new(clock.sample().unwrap()).unwrap())),
        pending: &pending,
    };
    let settle = dispatch::Connection {
        request: &runtime,
        trust,
        slots: &settlement_slots,
        clock: clock.clone(),
    };
    let until = |ms| Deadline::after(clock.sample().unwrap().monotonic, ms).unwrap();
    let budget = |bytes, records| {
        Meter::new(
            until(10_000),
            Charge {
                io_bytes: bytes,
                records,
                ..Default::default()
            },
        )
    };
    let io = 2 * 1024 * 1024 + 192 * 1024;
    let work = || budget(io, 4096);
    let identity = Identity::RouteClaim(uncertain.operation());
    let sequence = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    for (bytes, records) in [(io - 1, 4096), (io, 4095)] {
        assert_eq!(
            worker.acquire_route(
                uncertain.clone(),
                until(10_000),
                &mut budget(bytes, records)
            ),
            Err(Error::Admission(ports::Error::Capacity))
        );
        assert_eq!(pending.identity().unwrap(), None);
    }
    assert_eq!(
        worker.acquire_route(uncertain.clone(), until(0), &mut work()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let mut expired = Meter::new(
        until(0),
        Charge {
            io_bytes: io,
            records: 4096,
            ..Default::default()
        },
    );
    assert_eq!(
        worker.acquire_route(uncertain.clone(), until(10_000), &mut expired),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let preparation = Request {
        mode: Mode::Preparation,
        ..runtime
    };
    let forbidden = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &preparation,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: &pending,
    };
    assert_eq!(
        forbidden.acquire_route(uncertain.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Forbidden))
    );
    let held = worker
        .reserve_final(
            AttemptId::from_bytes([211; 16]),
            until(10_000),
            &mut budget(4 * 1024 * 1024, 8192),
        )
        .unwrap();
    assert_eq!(
        worker.acquire_route(uncertain.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    );
    drop(held);
    assert_eq!(pending.identity().unwrap(), None);
    assert_eq!(
        owners::inspect(request, trust, deadline(), uncertain).unwrap(),
        None
    );

    let previous = owners::inspect(request, trust, deadline(), historical)
        .unwrap()
        .unwrap();
    let mut exact = work();
    assert_eq!(
        worker
            .acquire_route(historical.clone(), until(10_000), &mut exact)
            .unwrap(),
        previous
    );
    assert_eq!(
        worker.acquire_route(historical.clone(), until(10_000), &mut exact),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert_eq!(
        owners::observe(request, trust, deadline(), historical, previous.generation),
        Err(owners::Error::Stale)
    );
    assert_eq!(pending.identity().unwrap(), None);

    assert_eq!(
        worker.acquire_route(uncertain.clone(), until(1500), &mut work()),
        Err(Error::Owner(owners::Error::Indeterminate))
    );
    assert_eq!(pending.identity().unwrap(), Some(identity));
    assert_eq!(
        worker.acquire_route(historical.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    );
    for kind in [Kind::Fresh, Kind::Retry, Kind::Expiry] {
        assert_eq!(
            worker
                .candidates(uncertain, 4, kind, None, until(10_000), &mut work())
                .map(|_| ()),
            Err(Error::Admission(ports::Error::Busy))
        );
    }
    assert_eq!(
        worker
            .publish_prepared(
                uncertain,
                4,
                dispatch::Input {
                    submission: SubmissionId::from_bytes([211; 16]),
                    retry: false,
                    deadline: until(10_000)
                },
                &mut work()
            )
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert!(matches!(
        worker.reserve_final(AttemptId::from_bytes([211; 16]), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    assert_eq!(
        pending.settle(&settle, until(0), &mut work()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut budget(io - 1, 4096)),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert_eq!(pending.identity().unwrap(), Some(identity));
    thread::sleep(Duration::from_secs(4));
    let recovered = owners::inspect(request, trust, deadline(), uncertain)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.generation, 4);

    let empty = dispatch::Pending::default();
    let holder = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &runtime,
            trust,
            slots: &settlement_slots,
            clock: clock.clone(),
        },
        guard: worker.guard.clone(),
        pending: &empty,
    };
    let held = holder
        .reserve_final(
            AttemptId::from_bytes([212; 16]),
            until(10_000),
            &mut budget(4 * 1024 * 1024, 8192),
        )
        .unwrap();
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    );
    drop(held);
    assert_eq!(pending.identity().unwrap(), Some(identity));
    sql(fixture,&format!("CREATE TABLE public.route_acquisition_receipt AS SELECT * FROM td_mta.route_owner_receipts WHERE operation=decode('{}','hex'); DELETE FROM td_mta.route_owner_receipts WHERE operation=decode('{}','hex')",uncertain.operation(),uncertain.operation()));
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut work()).unwrap(),
        None
    );
    assert_eq!(pending.identity().unwrap(), Some(identity));
    sql(fixture,"INSERT INTO td_mta.route_owner_receipts SELECT * FROM public.route_acquisition_receipt; DROP TABLE public.route_acquisition_receipt");
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut work()).unwrap(),
        Some(dispatch::Settlement::RouteClaim(recovered))
    );
    assert_eq!(pending.identity().unwrap(), None);
    assert_eq!(
        worker
            .acquire_route(uncertain.clone(), until(10_000), &mut work())
            .unwrap(),
        recovered
    );
    let competing = owners::prepare(binding, BootId::from_bytes([211; 16])).unwrap();
    assert_eq!(
        worker.acquire_route(competing, until(10_000), &mut work()),
        Err(Error::Owner(owners::Error::Busy))
    );
    assert_eq!(pending.identity().unwrap(), None);
    drop(
        worker
            .reserve_final(
                AttemptId::from_bytes([211; 16]),
                until(10_000),
                &mut budget(4 * 1024 * 1024, 8192),
            )
            .unwrap(),
    );
    drop(
        holder
            .reserve_final(
                AttemptId::from_bytes([212; 16]),
                until(10_000),
                &mut budget(4 * 1024 * 1024, 8192),
            )
            .unwrap(),
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        sequence
    );
    sql(
        fixture,
        "DROP OWNED BY mail_runtime; DROP ROLE mail_runtime",
    );
    recovered
}
