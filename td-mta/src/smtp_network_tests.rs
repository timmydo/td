#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{DiskLimits, ViewMode, WorkLimits},
    config::{
        routing::{AliasSlot, Builder, DomainSlot, Routing},
        syntax::Location,
    },
    ids::AccountId,
    limits::Limits,
    ports::Time,
    smtp_session::Settings,
};
use std::{
    collections::VecDeque,
    num::NonZeroU32,
    sync::atomic::{AtomicU64, Ordering},
};

struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Operation {
    Read,
    Write,
    Flush,
    Close,
}
fn io_error() -> Error {
    Error::Io {
        kind: std::io::ErrorKind::ConnectionReset,
        os_code: None,
    }
}
#[derive(Default)]
struct Wire {
    input: VecDeque<u8>,
    output: Vec<u8>,
    write_max: Option<usize>,
    flush_pending: bool,
    close_pending: bool,
    error: Option<Operation>,
    eof: bool,
    aborts: usize,
    closes: usize,
    reads: usize,
    writes: usize,
    flushes: usize,
}
impl Transport for Wire {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        self.reads += 1;
        if self.error == Some(Operation::Read) {
            return Err(io_error());
        }
        let count = output.len().min(self.input.len());
        if count == 0 {
            return Ok(if self.eof {
                IoProgress::Closed
            } else {
                IoProgress::Pending
            });
        }
        for byte in &mut output[..count] {
            *byte = self.input.pop_front().unwrap();
        }
        Ok(IoProgress::Bytes(count))
    }
    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        self.writes += 1;
        if self.error == Some(Operation::Write) {
            return Err(io_error());
        }
        let count = self.write_max.unwrap_or(input.len()).min(input.len());
        if count == 0 {
            return Ok(IoProgress::Pending);
        }
        self.output.extend_from_slice(&input[..count]);
        Ok(IoProgress::Bytes(count))
    }
    fn flush(&mut self) -> Result<FlushProgress, Error> {
        self.flushes += 1;
        if self.error == Some(Operation::Flush) {
            return Err(io_error());
        }
        Ok(if self.flush_pending {
            FlushProgress::Pending
        } else {
            FlushProgress::Complete
        })
    }
    fn close(&mut self) -> Result<FlushProgress, Error> {
        self.closes += 1;
        if self.error == Some(Operation::Close) {
            return Err(io_error());
        }
        Ok(if self.close_pending {
            FlushProgress::Pending
        } else {
            FlushProgress::Complete
        })
    }
    fn abort(&mut self) {
        self.aborts += 1;
    }
}
fn fixture(run: impl FnOnce(&Routing<'_>, &TimeoutPlan, &Timer)) {
    let mut text = [0; 1024];
    let mut domains = [DomainSlot::EMPTY; 1];
    let mut aliases = [AliasSlot::EMPTY; 1];
    let at = Location {
        line: NonZeroU32::new(1).unwrap(),
        column: 1,
    };
    let account = AccountId::from_bytes([1; 16]);
    let mut routes = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
    routes.account(account, at).unwrap();
    routes.domain("example.test", at).unwrap();
    routes.alias("a@example.test", account, at).unwrap();
    let plan = crate::admission::timers::NetworkLimits::default()
        .plan(
            &DiskLimits::default()
                .plan(
                    &Limits::default().plan().unwrap(),
                    WorkLimits::default(),
                    ViewMode::OnlineBackground,
                )
                .unwrap(),
        )
        .unwrap();
    run(&routes.finish().unwrap(), &plan, &Timer(AtomicU64::new(1)));
}
fn network<'a>(routes: &'a Routing<'a>, plan: &TimeoutPlan, timer: &Timer) -> Network<'a> {
    Network::new(
        routes,
        Settings {
            hostname: "mx.example.test",
            message_bytes: 32768,
            trace_bytes: 1024,
            recipients: 100,
            starttls: true,
        },
        plan,
        timer,
    )
    .unwrap()
}

fn turns(n: &mut Network<'_>, w: &mut Wire, t: &Timer, count: usize) {
    for _ in 0..count {
        n.advance(w, t).unwrap();
    }
}
fn begin(n: &mut Network<'_>, w: &mut Wire, t: &Timer) {
    w.input
        .extend(b"EHLO sender.test\r\nMAIL FROM:<>\r\nRCPT TO:<a@example.test>\r\nDATA\r\n");
    for _ in 0..100 {
        if n.advance(w, t).unwrap() == Progress::Work {
            return;
        }
    }
    panic!("no storage handoff");
}

#[test]
fn partial_reply_and_flush_stall_never_parse_a_command() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire {
            write_max: Some(1),
            flush_pending: true,
            ..Wire::default()
        };
        w.input.extend(b"QUIT\r\n");
        turns(&mut n, &mut w, timer, 100);
        assert_eq!(w.output, b"220 mx.example.test ESMTP ready\r\n");
        assert_eq!(w.reads, 0);
        assert!(n.data_ready(Ok(()), timer).is_err());
        w.flush_pending = false;
        turns(&mut n, &mut w, timer, 100);
        assert!(w.output.ends_with(b"221 2.0.0 Closing connection\r\n"));
        assert_eq!(w.closes, 1);
    });
}

#[test]
fn worker_handoff_keeps_tail_and_has_no_network_effects() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        begin(&mut n, &mut w, timer);
        assert!(matches!(n.session().pending(), Pending::BeginData { .. }));
        let operations = (w.reads, w.writes, w.flushes);
        turns(&mut n, &mut w, timer, 10);
        assert_eq!(operations, (w.reads, w.writes, w.flushes));
        let deadline = n.deadline();
        n.data_ready(Ok(()), timer).unwrap();
        w.input.extend(b"Subject: x\r\n\r\n..dot\r\n.\r\nQUIT\r\n");
        let mut body = Vec::new();
        for _ in 0..100 {
            if n.advance(&mut w, timer).unwrap() != Progress::Work {
                continue;
            }
            match n.session().pending() {
                Pending::Data(bytes) => {
                    body.extend_from_slice(bytes);
                    n.data_written(Ok(()), timer).unwrap();
                }
                Pending::Commit => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(body, b"Subject: x\r\n\r\n.dot\r\n");
        assert_eq!(n.deadline(), deadline);
        assert!(!w.output.windows(16).any(|b| b == b"Message accepted"));
        n.committed(
            Err(crate::ports::CommitFailure::Rejected(Error::Busy)),
            timer,
        )
        .unwrap();
        turns(&mut n, &mut w, timer, 30);
        assert!(w.output.ends_with(b"221 2.0.0 Closing connection\r\n"));
    });
}

#[test]
fn idle_total_and_regressing_time_refuse_without_renewing_work() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        turns(&mut n, &mut w, timer, 2);
        timer.0.store(300001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        turns(&mut n, &mut w, timer, 4);
        assert!(w.output.ends_with(b"421 4.3.2 Service unavailable\r\n"));
        timer.0.store(305001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
        assert_eq!(w.aborts, 1);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));

        timer.0.store(1, Ordering::Relaxed);
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        begin(&mut n, &mut w, timer);
        let deadline = n.deadline();
        n.data_ready(Ok(()), timer).unwrap();
        turns(&mut n, &mut w, timer, 2);
        for now in (100001..1800001).step_by(100000) {
            timer.0.store(now, Ordering::Relaxed);
            w.input.push_back(b'x');
            turns(&mut n, &mut w, timer, 2);
        }
        assert_eq!(n.deadline(), deadline);
        timer.0.store(deadline.tick().0, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Err(Error::Deadline));

        timer.0.store(10, Ordering::Relaxed);
        let mut n = network(routes, plan, timer);
        timer.0.store(9, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Err(Error::Invalid));
    });
}

#[test]
fn close_drain_discards_commands_and_has_a_fixed_byte_and_time_bound() {
    fixture(|routes, plan, timer| {
        for by_time in [false, true] {
            timer.0.store(1, Ordering::Relaxed);
            let mut n = network(routes, plan, timer);
            let mut w = Wire::default();
            w.input.extend(b"QUIT\r\nNOOP\r\n");
            turns(&mut n, &mut w, timer, 8);
            assert_eq!(w.closes, 1);
            let output = w.output.clone();
            if by_time {
                timer.0.store(5001, Ordering::Relaxed);
            } else {
                w.input.extend(std::iter::repeat_n(b'x', DRAIN_BYTES));
            }
            turns(&mut n, &mut w, timer, 10);
            assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
            assert_eq!(w.output, output);
            assert_eq!(w.aborts, 1);
        }
    });
}

#[test]
fn starttls_yields_the_exact_command_and_refuses_buffered_plaintext() {
    fixture(|routes, plan, timer| {
        for tail in [false, true] {
            let mut n = network(routes, plan, timer);
            let mut w = Wire::default();
            w.input.extend(b"EHLO sender.test\r\nStArTtLs\r\n");
            if tail {
                w.input.extend(b"NOOP\r\n");
            }
            for _ in 0..30 {
                if n.advance(&mut w, timer).unwrap() == Progress::Work {
                    break;
                }
            }
            if tail {
                assert!(w.output.windows(3).any(|b| b == b"554"));
                assert!(n.data_ready(Ok(()), timer).is_err());
            } else {
                assert_eq!(
                    n.session().pending(),
                    Pending::StartTls {
                        command: b"StArTtLs\r\n"
                    }
                );
                assert!(!w.output.windows(18).any(|b| b == b"Ready to start TLS"));
                let operations = (w.reads, w.writes, w.flushes);
                turns(&mut n, &mut w, timer, 10);
                assert_eq!(operations, (w.reads, w.writes, w.flushes));
                // No simulated handshake success: abandoning upgrade closes.
                n.abort(&mut w);
                assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
            }
        }
    });
}

#[test]
fn command_progress_does_not_renew_connection_lifetime() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        turns(&mut n, &mut w, timer, 2);
        for now in (250001..3600001).step_by(250000) {
            timer.0.store(now, Ordering::Relaxed);
            w.input.extend(b"NOOP\r\n");
            turns(&mut n, &mut w, timer, 4);
        }
        timer.0.store(3600001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        turns(&mut n, &mut w, timer, 4);
        assert!(w.output.ends_with(b"421 4.3.2 Service unavailable\r\n"));
        timer.0.store(3605001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
    });
}

#[test]
fn eof_during_incomplete_data_never_produces_acceptance() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        begin(&mut n, &mut w, timer);
        n.data_ready(Ok(()), timer).unwrap();
        w.input.extend(b"incomplete line");
        w.eof = true;
        turns(&mut n, &mut w, timer, 10);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
        assert!(!w.output.windows(16).any(|b| b == b"Message accepted"));
        assert_eq!(w.aborts, 1);
    });
}

#[test]
fn progress_and_socket_wait_have_distinct_scheduling_results() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        w.flush_pending = true;
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
        w.flush_pending = false;
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
        w.input.extend(b"NOOP\r\n");
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        w.write_max = Some(0);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
        w.write_max = None;
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
    });
}

#[test]
fn worker_results_are_single_transitions_and_idle_wait_is_separate() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        assert_eq!(n.data_ready(Ok(()), timer), Err(Error::Conflict));
        begin(&mut n, &mut w, timer);
        assert_eq!(n.data_written(Ok(()), timer), Err(Error::Conflict));
        assert_eq!(n.message_too_large(timer), Err(Error::Conflict));
        assert_eq!(n.tls_established(timer), Err(Error::Conflict));
        let cap = n.deadline();
        timer.0.store(300001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Work));
        n.data_ready(Ok(()), timer).unwrap();
        assert_eq!(n.data_ready(Ok(()), timer), Err(Error::Conflict));
        assert_eq!(n.deadline(), cap);
        turns(&mut n, &mut w, timer, 2);
        assert!(w
            .output
            .ends_with(b"354 Send message, end with a dot line\r\n"));
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
    });
}

fn pending_commit(n: &mut Network<'_>, w: &mut Wire, timer: &Timer) {
    begin(n, w, timer);
    n.data_ready(Ok(()), timer).unwrap();
    w.input.extend(b".\r\n");
    for _ in 0..20 {
        if n.advance(w, timer).unwrap() == Progress::Work {
            assert_eq!(n.session().pending(), Pending::Commit);
            return;
        }
    }
    panic!("no commit request");
}
fn committed_result() -> Commit {
    Commit {
        account: AccountId::from_bytes([1; 16]),
        epoch: crate::ids::StoreEpoch::from_bytes([2; 16]),
        sequence: crate::format::Sequence::from_u64(1),
    }
}

#[test]
fn known_final_result_and_service_close_share_one_nonrenewing_grace() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        pending_commit(&mut n, &mut w, timer);
        timer.0.store(3600001, Ordering::Relaxed);
        n.committed(Ok(committed_result()), timer).unwrap();
        let before = w.output.len();
        // No buffered following command may run after the original lifetime.
        w.input.extend(b"MAIL FROM:<>\r\n");
        turns(&mut n, &mut w, timer, 5);
        assert_eq!(
            &w.output[before..],
            b"250 2.0.0 Message accepted\r\n421 4.3.2 Service unavailable\r\n"
        );
        timer.0.store(3605001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
        assert_eq!(w.aborts, 1);

        timer.0.store(1, Ordering::Relaxed);
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        pending_commit(&mut n, &mut w, timer);
        timer.0.store(1800001, Ordering::Relaxed);
        n.committed(Ok(committed_result()), timer).unwrap();
        w.write_max = Some(1);
        for now in 1800001..1800006 {
            timer
                .0
                .store(1800001 + (now - 1800001) * 1000, Ordering::Relaxed);
            assert_eq!(n.advance(&mut w, timer), Ok(Progress::Advanced));
        }
        timer.0.store(1805001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Err(Error::Deadline));
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
    });
}

#[test]
fn unknown_commit_and_failed_completion_clock_never_emit_acceptance() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        pending_commit(&mut n, &mut w, timer);
        let output = w.output.clone();
        n.committed(
            Err(CommitFailure::Indeterminate(Error::WriterStopped)),
            timer,
        )
        .unwrap();
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
        assert_eq!(w.output, output);

        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        pending_commit(&mut n, &mut w, timer);
        timer.0.store(0, Ordering::Relaxed);
        assert_eq!(
            n.committed(Ok(committed_result()), timer),
            Err(Error::Invalid)
        );
        timer.0.store(1, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
        assert_eq!(
            n.committed(Ok(committed_result()), timer),
            Err(Error::Conflict)
        );
        assert!(!w.output.windows(16).any(|b| b == b"Message accepted"));
    });
}

#[test]
fn partial_command_expiry_aborts_instead_of_fabricating_a_reply() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire::default();
        w.input.extend(b"NO");
        turns(&mut n, &mut w, timer, 4);
        timer.0.store(300001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Err(Error::Deadline));
        assert!(!w.output.windows(3).any(|b| b == b"421"));
    });
}

#[test]
fn stalled_shutdown_expires_and_closed_owner_refuses_new_control_or_work() {
    fixture(|routes, plan, timer| {
        let mut n = network(routes, plan, timer);
        let mut w = Wire {
            close_pending: true,
            ..Wire::default()
        };
        w.input.extend(b"QUIT\r\n");
        turns(&mut n, &mut w, timer, 8);
        assert_eq!(w.closes, 2);
        assert_eq!(n.service_unavailable(timer), Err(Error::Conflict));
        timer.0.store(5001, Ordering::Relaxed);
        assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
        assert_eq!(w.closes, 2);
        assert_eq!(w.aborts, 1);
        assert_eq!(n.service_unavailable(timer), Err(Error::Conflict));
        assert_eq!(n.data_ready(Ok(()), timer), Err(Error::Conflict));
    });
}

#[test]
fn transport_errors_are_terminal_in_every_output_and_input_phase() {
    fixture(|routes, plan, timer| {
        for operation in [
            Operation::Read,
            Operation::Write,
            Operation::Flush,
            Operation::Close,
        ] {
            let mut n = network(routes, plan, timer);
            let mut w = Wire {
                error: Some(operation),
                ..Wire::default()
            };
            w.input.extend(b"QUIT\r\n");
            let mut failed = false;
            for _ in 0..20 {
                if let Err(error) = n.advance(&mut w, timer) {
                    assert_eq!(error, io_error());
                    failed = true;
                    break;
                }
            }
            assert!(failed);
            let counts = (w.reads, w.writes, w.flushes, w.closes);
            assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
            assert_eq!(counts, (w.reads, w.writes, w.flushes, w.closes));
            assert_eq!(w.aborts, 1);
        }
    });
}

#[test]
fn late_results_cannot_renew_the_original_transport_finishing_cap() {
    fixture(|routes, plan, timer| {
        for beyond_cap in [false, true] {
            timer.0.store(1, Ordering::Relaxed);
            let mut n = network(routes, plan, timer);
            let mut w = Wire::default();
            let original = n.deadline();
            pending_commit(&mut n, &mut w, timer);
            let before = w.output.len();
            timer.0.store(
                original.tick().0 + if beyond_cap { 5000 } else { 4000 },
                Ordering::Relaxed,
            );
            n.committed(Ok(committed_result()), timer).unwrap();
            w.write_max = Some(0);
            if !beyond_cap {
                assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
                timer.0.store(original.tick().0 + 4999, Ordering::Relaxed);
                assert_eq!(n.advance(&mut w, timer), Ok(Progress::Pending));
                timer.0.store(original.tick().0 + 5000, Ordering::Relaxed);
            }
            assert_eq!(n.advance(&mut w, timer), Err(Error::Deadline));
            assert_eq!(n.advance(&mut w, timer), Ok(Progress::Closed));
            assert_eq!(w.output.len(), before);
            assert_eq!(n.deadline(), original);
        }
    });
}
