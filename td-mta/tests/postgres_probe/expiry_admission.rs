use super::{deadline, sql, Fixture, PASSWORD};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use td_crypto::TrustStore;
use td_mta::{
    admission::work::{Charge, Meter},
    ids::{AttemptId, SubmissionId},
    ports::{self, Clock, Deadline},
    postgres_dispatch::{self as dispatch, Error, Identity},
    postgres_probe::{Mode, Request},
    postgres_queue_candidates::Kind,
    postgres_queue_expiry as expiry,
    postgres_read::ReadSlots,
    postgres_route_owners::Claim,
    queue_clock::Guard,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    value: &expiry::Expiry,
) -> expiry::Outcome {
    // The preceding HTTP submission fixture owns this Runtime role.
    let runtime = fixture.queue_runtime.as_ref().unwrap();
    let clock = runtime.clock.clone();
    let slots = ReadSlots::new(1).unwrap();
    let settlement_slots = ReadSlots::new(1).unwrap();
    let pending = dispatch::Pending::default();
    let request = Request {
        address: request.address,
        server_name: request.server_name,
        database: request.database,
        user: "mail_runtime",
        password: PASSWORD,
        mode: Mode::Runtime,
    };
    let worker = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &request,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: runtime.guard.clone(),
        pending: &pending,
    };
    let settle = dispatch::Connection {
        request: &request,
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
    let identity = Identity::Expiry(value.operation());
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    for (bytes, records) in [(io - 1, 4096), (io, 4095)] {
        assert_eq!(
            worker.expire(value.clone(), until(10_000), &mut budget(bytes, records)),
            Err(Error::Admission(ports::Error::Capacity))
        );
        assert_eq!(pending.identity().unwrap(), None);
    }
    assert_eq!(
        worker.expire(value.clone(), until(0), &mut work()),
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
        worker.expire(value.clone(), until(10_000), &mut expired),
        Err(Error::Admission(ports::Error::Deadline))
    );
    let preparation = Request {
        mode: Mode::Preparation,
        ..request
    };
    let forbidden = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &preparation,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: runtime.guard.clone(),
        pending: &pending,
    };
    assert_eq!(
        forbidden.expire(value.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Forbidden))
    );
    let held = worker
        .reserve_final(
            AttemptId::from_bytes([212; 16]),
            until(10_000),
            &mut budget(4 * 1024 * 1024, 8192),
        )
        .unwrap();
    assert_eq!(
        worker.expire(value.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    );
    drop(held);
    let unconfirmed = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &request,
            trust,
            slots: &slots,
            clock: clock.clone(),
        },
        guard: Arc::new(Mutex::new(Guard::new(clock.sample().unwrap()).unwrap())),
        pending: &pending,
    };
    assert_eq!(
        unconfirmed.expire(value.clone(), until(10_000), &mut work()),
        Err(Error::Expiry(expiry::Error::Clock(
            td_mta::queue_clock::Error::Unstable
        )))
    );
    let held_guard = unconfirmed.guard.lock().unwrap();
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    thread::scope(|scope| {
        let job = scope.spawn(|| {
            let result = unconfirmed.expire(value.clone(), until(10_000), &mut work());
            send.send(result).unwrap();
        });
        let result = receive.recv_timeout(Duration::from_secs(5));
        drop(held_guard);
        job.join().unwrap();
        assert_eq!(
            result.unwrap(),
            Err(Error::Expiry(expiry::Error::Clock(
                td_mta::queue_clock::Error::Invalid,
            )))
        );
    });
    assert_eq!(pending.identity().unwrap(), None);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        before
    );
    assert_eq!(
        expiry::inspect(&request, trust, deadline(), value).unwrap(),
        None
    );

    assert_eq!(
        worker.expire(value.clone(), until(1500), &mut work()),
        Err(Error::Expiry(expiry::Error::Indeterminate))
    );
    assert_eq!(pending.identity().unwrap(), Some(identity));
    assert_eq!(
        worker.expire(value.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert_eq!(
        worker.acquire_route(claim.clone(), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    );
    for kind in [Kind::Fresh, Kind::Retry, Kind::Expiry] {
        assert_eq!(
            worker
                .candidates(claim, generation, kind, None, until(10_000), &mut work())
                .map(|_| ()),
            Err(Error::Admission(ports::Error::Busy))
        );
    }
    assert_eq!(
        worker
            .publish_prepared(
                claim,
                generation,
                dispatch::Input {
                    submission,
                    retry: false,
                    deadline: until(10_000),
                },
                &mut work()
            )
            .map(|_| ()),
        Err(Error::Admission(ports::Error::Busy))
    );
    assert!(matches!(
        worker.reserve_final(AttemptId::from_bytes([212; 16]), until(10_000), &mut work()),
        Err(Error::Admission(ports::Error::Busy))
    ));
    assert_eq!(
        pending.settle(&settle, until(0), &mut work()),
        Err(Error::Admission(ports::Error::Deadline))
    );
    for (bytes, records) in [(io - 1, 4096), (io, 4095)] {
        assert_eq!(
            pending.settle(&settle, until(10_000), &mut budget(bytes, records)),
            Err(Error::Admission(ports::Error::Capacity))
        );
    }
    assert_eq!(pending.identity().unwrap(), Some(identity));
    thread::sleep(Duration::from_secs(4));
    let outcome = expiry::inspect(&request, trust, deadline(), value)
        .unwrap()
        .unwrap();

    let empty = dispatch::Pending::default();
    let holder = dispatch::Dispatches {
        connection: dispatch::Connection {
            request: &request,
            trust,
            slots: &settlement_slots,
            clock: clock.clone(),
        },
        guard: runtime.guard.clone(),
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
    sql(fixture,&format!("CREATE TABLE public.worker_expiry_receipt AS SELECT * FROM td_mta.submission_expiry_receipts WHERE operation=decode('{}','hex'); DELETE FROM td_mta.submission_expiry_receipts WHERE operation=decode('{}','hex')",value.operation(),value.operation()));
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut work()).unwrap(),
        None
    );
    assert_eq!(pending.identity().unwrap(), Some(identity));
    sql(fixture,"INSERT INTO td_mta.submission_expiry_receipts SELECT * FROM public.worker_expiry_receipt; DROP TABLE public.worker_expiry_receipt");
    assert_eq!(
        pending.settle(&settle, until(10_000), &mut work()).unwrap(),
        Some(dispatch::Settlement::Expiry(outcome))
    );
    assert_eq!(pending.identity().unwrap(), None);
    let after = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let mut exact = work();
    assert_eq!(
        unconfirmed
            .expire(value.clone(), until(10_000), &mut exact)
            .unwrap(),
        outcome
    );
    assert_eq!(
        worker.expire(value.clone(), until(10_000), &mut exact),
        Err(Error::Admission(ports::Error::Capacity))
    );
    assert_eq!(pending.identity().unwrap(), None);
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        after
    );
    drop(
        worker
            .reserve_final(
                AttemptId::from_bytes([212; 16]),
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
    outcome
}
