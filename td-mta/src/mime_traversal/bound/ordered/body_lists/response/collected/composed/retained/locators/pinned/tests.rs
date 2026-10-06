#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::super::super::Status;
use super::super::tests::exercise;
use super::*;
use crate::ports::{Clock, Time};
use std::sync::atomic::{AtomicU64, Ordering};
use td_crypto::Provider;
struct TestClock(AtomicU64);
impl TestClock {
    fn new() -> Self {
        Self(AtomicU64::new(1))
    }
    fn expire(&self) {
        self.0.store(u64::MAX, Ordering::SeqCst);
    }
}
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, PolicyError> {
        let now = self.0.load(Ordering::SeqCst);
        if now == u64::MAX - 1 {
            return Err(PolicyError::Busy);
        }
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(now),
        })
    }
}

fn mapped<T>(
    source: &[u8],
    base: u64,
    run: impl FnOnce(Mapped<'_, '_, '_, '_, '_, '_, '_>) -> T,
) -> T {
    exercise(source, base, |mut cursor| {
        while cursor.poll(Tick(1)).unwrap() != Status::Complete {}
        run(cursor.finish(Tick(1)).unwrap())
    })
}
fn costs<C: Crypto>(cursor: &Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, C>) -> [u64; 5] {
    let structure = &cursor.original.source.original.source.projected.structure;
    let left = structure.work.remaining();
    [
        structure.budget.source_bytes_remaining(),
        structure.budget.steps_remaining(),
        left.io_bytes,
        left.records,
        left.output_bytes,
    ]
}
#[test]
fn matches_whole_source_and_returns_original_candidates_and_live_pin() {
    let mut source = b"\r\n".to_vec();
    source.extend(std::iter::repeat_n(b'x', 8200));
    for base in [0, 17, u64::MAX - source.len() as u64] {
        let clock = TestClock::new();
        crate::store_fs::with_pinned_fixture(&source, &clock, |parent| {
            mapped(&source, base, |original| {
                let candidates = original.value().unwrap().candidates.as_ptr();
                let members = original.value().unwrap().original.members.as_ptr();
                let mut cursor = Cursor::new(original, parent, &Provider, Tick(1)).unwrap();
                let mut previous = costs(&cursor);
                for (end, records) in [(4096, 2), (8192, 1)] {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
                    let current = costs(&cursor);
                    assert_eq!(
                        std::array::from_fn::<_, 5, _>(|i| previous[i] - current[i]),
                        [0, 1, 4096, records, 0]
                    );
                    previous = current;
                    assert_eq!(cursor.position, end);
                    assert!(cursor.value().is_none());
                }
                assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                assert_eq!(cursor.position, source.len());
                let current = costs(&cursor);
                assert_eq!(
                    std::array::from_fn::<_, 5, _>(|i| previous[i] - current[i]),
                    [0, 1, 10, 1, 0]
                );
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                assert_eq!(costs(&cursor), current);
                let mut bound = cursor.finish(Tick(1)).unwrap();
                assert_eq!(bound.value().unwrap().candidates.as_ptr(), candidates);
                assert_eq!(bound.value().unwrap().original.members.as_ptr(), members);
                bound.check_deadline(Tick(1)).unwrap();
                let (original, mut parent) = bound.finish(Tick(1)).unwrap();
                assert_eq!(original.value().unwrap().candidates.as_ptr(), candidates);
                let mut bytes = [0; 2];
                assert_eq!(parent.read_at(0, &mut bytes).unwrap(), 2);
                assert_eq!(&bytes, b"\r\n");
            });
        });
    }
}
#[test]
fn matching_id_and_length_cannot_substitute_different_bytes() {
    for (stored, parsed) in [
        (b"\r\nabc".as_slice(), b"\r\nabd".as_slice()),
        (
            b"X-Test: a\r\n\r\nbody".as_slice(),
            b"X-Test: b\r\n\r\nbody".as_slice(),
        ),
    ] {
        let clock = TestClock::new();
        crate::store_fs::with_pinned_fixture(stored, &clock, |parent| {
            mapped(parsed, 0, |original| {
                let mut cursor = Cursor::new(original, parent, &Provider, Tick(1)).unwrap();
                assert_eq!(cursor.poll(Tick(1)), Err(Error::SourceMismatch));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(100)), Err(Error::SourceMismatch));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::SourceMismatch));
            });
        });
    }
}

#[test]
fn id_and_length_mismatch_refuse_before_hash_creation() {
    for different_id in [false, true] {
        let clock = TestClock::new();
        let calls = AtomicU64::new(0);
        let largest = AtomicU64::new(0);
        let crypto = FaultCrypto {
            clock: &clock,
            calls: &calls,
            largest: &largest,
            fault: 0,
            expire: false,
        };
        crate::store_fs::with_pinned_fixture(b"\r\nabc", &clock, |parent| {
            mapped(
                if different_id { b"\r\nabc" } else { b"\r\nab" },
                0,
                |mut original| {
                    if different_id {
                        original.parent = crate::ids::BlobId::from_bytes([0x55; 16]);
                    }
                    assert_eq!(
                        Cursor::new(original, parent, &crypto, Tick(1)).err(),
                        Some(Error::SourceMismatch)
                    );
                },
            );
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn actual_parent_clock_refusal_is_sticky_even_at_a_healthy_supplied_tick() {
    for phase in 0..7 {
        let clock = TestClock::new();
        crate::store_fs::with_pinned_fixture(b"\r\nabc", &clock, |mut parent| {
            mapped(b"\r\nabc", 0, |original| {
                if phase == 4 {
                    clock.expire();
                    assert_eq!(parent.check_deadline(), Err(PolicyError::Deadline));
                    clock.0.store(1, Ordering::SeqCst);
                    assert_eq!(
                        Cursor::new(original, parent, &Provider, Tick(1)).err(),
                        Some(Error::Parent(PolicyError::Deadline))
                    );
                    return;
                }
                if phase >= 5 {
                    clock
                        .0
                        .store(if phase == 5 { 0 } else { u64::MAX - 1 }, Ordering::SeqCst);
                    assert_eq!(
                        Cursor::new(original, parent, &Provider, Tick(1)).err(),
                        Some(Error::Parent(if phase == 5 {
                            PolicyError::Invalid
                        } else {
                            PolicyError::Busy
                        }))
                    );
                    return;
                }
                if phase == 0 {
                    clock.expire();
                    assert_eq!(
                        Cursor::new(original, parent, &Provider, Tick(1)).err(),
                        Some(Error::Parent(PolicyError::Deadline))
                    );
                    return;
                }
                let mut cursor = Cursor::new(original, parent, &Provider, Tick(1)).unwrap();
                if phase >= 2 {
                    assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                }
                if phase == 3 {
                    let mut bound = cursor.finish(Tick(1)).unwrap();
                    clock.expire();
                    assert_eq!(
                        bound.check_deadline(Tick(1)),
                        Err(Error::Parent(PolicyError::Deadline))
                    );
                    assert!(bound.value().is_none());
                    assert_eq!(bound.failure, Some(Error::Parent(PolicyError::Deadline)));
                    clock.0.store(1, Ordering::SeqCst);
                    assert_eq!(
                        bound.finish(Tick(1)).err(),
                        Some(Error::Parent(PolicyError::Deadline))
                    );
                } else {
                    clock.expire();
                    if phase == 1 {
                        assert_eq!(
                            cursor.poll(Tick(1)),
                            Err(Error::Parent(PolicyError::Deadline))
                        );
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.failure, Some(Error::Parent(PolicyError::Deadline)));
                        clock.0.store(1, Ordering::SeqCst);
                    } else {
                        assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
                    }
                    assert_eq!(
                        cursor.finish(Tick(1)).err(),
                        Some(Error::Parent(PolicyError::Deadline))
                    );
                }
            });
        });
    }
}

struct FaultCrypto<'t> {
    clock: &'t TestClock,
    calls: &'t AtomicU64,
    largest: &'t AtomicU64,
    fault: u8,
    expire: bool,
}
struct FaultDigest<'t> {
    original: <Provider as Crypto>::Sha256,
    clock: &'t TestClock,
    calls: &'t AtomicU64,
    largest: &'t AtomicU64,
    fault: u8,
    expire: bool,
    failed: bool,
}
impl Digest for FaultDigest<'_> {
    fn update(&mut self, bytes: &[u8]) -> Result<(), td_crypto::Error> {
        if self.failed {
            return Err(td_crypto::Error::Crypto);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.largest.fetch_max(bytes.len() as u64, Ordering::SeqCst);
        if self.fault == 1 {
            if self.expire {
                self.clock.expire();
            }
            self.failed = true;
            return Err(td_crypto::Error::Crypto);
        }
        let result = self.original.update(bytes);
        if self.fault == 5 && self.expire {
            self.clock.expire();
        }
        result
    }
    fn finish(self) -> Result<[u8; 32], td_crypto::Error> {
        if self.failed {
            return Err(td_crypto::Error::Crypto);
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fault == 2 {
            if self.expire {
                self.clock.expire();
            }
            return Err(td_crypto::Error::Crypto);
        }
        let result = self.original.finish();
        if self.fault == 6 && self.expire {
            self.clock.expire();
        }
        result
    }
}
impl<'t> Crypto for FaultCrypto<'t> {
    type Sha256 = FaultDigest<'t>;
    type SigningKey = <Provider as Crypto>::SigningKey;
    fn sha256(&self) -> Result<Self::Sha256, td_crypto::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fault == 0 {
            if self.expire {
                self.clock.expire();
            }
            return Err(td_crypto::Error::Crypto);
        }
        if self.fault == 4 && self.expire {
            self.clock.expire();
        }
        Ok(FaultDigest {
            original: Provider.sha256()?,
            clock: self.clock,
            calls: self.calls,
            largest: self.largest,
            fault: self.fault,
            expire: self.expire,
            failed: false,
        })
    }
    fn equal_digest(&self, left: &[u8; 32], right: &[u8; 32]) -> bool {
        if self.fault == 3 {
            if self.expire {
                self.clock.expire();
            }
            return false;
        }
        let result = Provider.equal_digest(left, right);
        if self.fault == 7 && self.expire {
            self.clock.expire();
        }
        result
    }
    fn generate_p256(&self, output: &mut [u8]) -> Result<usize, td_crypto::Error> {
        Provider.generate_p256(output)
    }
    fn load_p256(&self, bytes: &[u8]) -> Result<Self::SigningKey, td_crypto::Error> {
        Provider.load_p256(bytes)
    }
    fn p256_public(
        &self,
        key: &Self::SigningKey,
        output: &mut [u8; 65],
    ) -> Result<(), td_crypto::Error> {
        Provider.p256_public(key, output)
    }
    fn sign_es256(
        &self,
        key: &Self::SigningKey,
        bytes: &[u8],
        output: &mut [u8; 64],
    ) -> Result<(), td_crypto::Error> {
        Provider.sign_es256(key, bytes, output)
    }
}
#[test]
fn digest_failures_are_terminal_and_post_work_parent_clock_overrides_errors() {
    for fault in 0..4 {
        for expire in [false, true] {
            let clock = TestClock::new();
            let calls = AtomicU64::new(0);
            let largest = AtomicU64::new(0);
            let crypto = FaultCrypto {
                clock: &clock,
                calls: &calls,
                largest: &largest,
                fault,
                expire,
            };
            crate::store_fs::with_pinned_fixture(b"\r\nabc", &clock, |parent| {
                mapped(b"\r\nabc", 0, |original| {
                    let error = if expire {
                        Error::Parent(PolicyError::Deadline)
                    } else if fault == 3 {
                        Error::SourceMismatch
                    } else {
                        Error::Crypto(td_crypto::Error::Crypto)
                    };
                    if fault == 0 {
                        assert_eq!(
                            Cursor::new(original, parent, &crypto, Tick(1)).err(),
                            Some(error)
                        );
                        assert_eq!(calls.load(Ordering::SeqCst), 1);
                        return;
                    }
                    let mut cursor = Cursor::new(original, parent, &crypto, Tick(1)).unwrap();
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(cursor.value().is_none());
                    assert!(cursor.digest.is_none());
                    let count = calls.load(Ordering::SeqCst);
                    assert_eq!(count, if fault == 1 { 2 } else { 3 });
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    assert_eq!(calls.load(Ordering::SeqCst), count);
                });
            });
        }
    }
}
#[test]
fn hash_visits_are_funded_before_digest_and_exhaustion_preserves_position() {
    for counter in 0..3 {
        let count = [5, 1, 2][counter];
        for cutoff in 0..=count {
            let clock = TestClock::new();
            let calls = AtomicU64::new(0);
            let largest = AtomicU64::new(0);
            let crypto = FaultCrypto {
                clock: &clock,
                calls: &calls,
                largest: &largest,
                fault: 8,
                expire: false,
            };
            crate::store_fs::with_pinned_fixture(b"\r\nabc", &clock, |parent| {
                mapped(b"\r\nabc", 0, |mut original| {
                    let structure = &mut original.source.original.source.projected.structure;
                    let before = structure.work.remaining();
                    if counter == 1 {
                        let left = structure.budget.steps_remaining();
                        structure
                            .budget
                            .charge(structure.work, Tick(1), 0, left - cutoff, &mut 0)
                            .unwrap();
                    } else {
                        structure
                            .work
                            .charge(
                                Tick(1),
                                Charge {
                                    io_bytes: if counter == 0 {
                                        before.io_bytes - cutoff
                                    } else {
                                        0
                                    },
                                    records: if counter == 2 {
                                        before.records - cutoff
                                    } else {
                                        0
                                    },
                                    ..Charge::default()
                                },
                            )
                            .unwrap();
                    }
                    let mut cursor = Cursor::new(original, parent, &crypto, Tick(1)).unwrap();
                    let result = cursor.poll(Tick(1));
                    if cutoff == count {
                        assert_eq!(result, Ok(Status::Complete));
                        cursor.finish(Tick(1)).unwrap();
                    } else {
                        let error =
                            Error::Original(super::super::Error::Admission(if counter == 1 {
                                crate::nfc::Error::InterpretationLimit
                            } else {
                                crate::nfc::Error::Work(if counter == 0 {
                                    crate::admission::work::Stop::IoBytes
                                } else {
                                    crate::admission::work::Stop::Records
                                })
                            }));
                        assert_eq!(result, Err(error));
                        assert_eq!(cursor.position, 0);
                        assert_eq!(calls.load(Ordering::SeqCst), 1);
                        assert_eq!(largest.load(Ordering::SeqCst), 0);
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(100)), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    }
                });
            });
        }
    }
}

#[test]
fn every_hash_prefix_and_complete_bound_require_fresh_original_job_admission() {
    let mut source = b"\r\n".to_vec();
    source.extend(std::iter::repeat_n(b'x', 8200));
    let error = Error::Original(super::super::Error::Admission(crate::nfc::Error::Work(
        crate::admission::work::Stop::Deadline,
    )));
    let clock = TestClock::new();
    crate::store_fs::with_pinned_fixture(&source, &clock, |parent| {
        mapped(&source, 0, |original| {
            assert_eq!(
                Cursor::new(original, parent, &Provider, Tick(100)).err(),
                Some(error)
            );
        });
    });
    for prefix in 0..=3 {
        for explicit in [false, true] {
            let clock = TestClock::new();
            crate::store_fs::with_pinned_fixture(&source, &clock, |parent| {
                mapped(&source, 0, |original| {
                    let mut cursor = Cursor::new(original, parent, &Provider, Tick(1)).unwrap();
                    for _ in 0..prefix {
                        cursor.poll(Tick(1)).unwrap();
                    }
                    assert_eq!(cursor.value().is_some(), prefix == 3);
                    if explicit {
                        assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                        assert!(cursor.value().is_none());
                        assert_eq!(cursor.poll(Tick(1)), Err(error));
                        assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    } else {
                        assert_eq!(cursor.finish(Tick(100)).err(), Some(error));
                    }
                });
            });
        }
    }
    for explicit in [false, true] {
        let clock = TestClock::new();
        crate::store_fs::with_pinned_fixture(&source, &clock, |parent| {
            mapped(&source, 0, |original| {
                let mut cursor = Cursor::new(original, parent, &Provider, Tick(1)).unwrap();
                for _ in 0..3 {
                    cursor.poll(Tick(1)).unwrap();
                }
                let mut bound = cursor.finish(Tick(1)).unwrap();
                if explicit {
                    assert_eq!(bound.check_deadline(Tick(100)), Err(error));
                    assert!(bound.value().is_none());
                    assert_eq!(bound.finish(Tick(1)).err(), Some(error));
                } else {
                    assert_eq!(bound.finish(Tick(100)).err(), Some(error));
                }
            });
        });
    }
}

#[test]
fn successful_crypto_creation_update_finish_and_comparison_are_fenced() {
    for fault in 4..=7 {
        for large in [false, true] {
            let clock = TestClock::new();
            let calls = AtomicU64::new(0);
            let largest = AtomicU64::new(0);
            let crypto = FaultCrypto {
                clock: &clock,
                calls: &calls,
                largest: &largest,
                fault,
                expire: true,
            };
            let mut source = b"\r\nabc".to_vec();
            if large {
                source.extend(std::iter::repeat_n(b'x', 8200));
            }
            crate::store_fs::with_pinned_fixture(&source, &clock, |parent| {
                mapped(&source, 0, |original| {
                    let error = Error::Parent(PolicyError::Deadline);
                    if fault == 4 {
                        assert_eq!(
                            Cursor::new(original, parent, &crypto, Tick(1)).err(),
                            Some(error)
                        );
                        assert_eq!(calls.load(Ordering::SeqCst), 1);
                        return;
                    }
                    let mut cursor = Cursor::new(original, parent, &crypto, Tick(1)).unwrap();
                    let mut refused = false;
                    for _ in 0..4 {
                        match cursor.poll(Tick(1)) {
                            Ok(Status::Yield) => assert!(cursor.value().is_none()),
                            Err(e) => {
                                assert_eq!(e, error);
                                refused = true;
                                break;
                            }
                            result => {
                                panic!("successful crypto skipped post-work fence: {result:?}")
                            }
                        }
                    }
                    assert!(refused);
                    assert!(largest.load(Ordering::SeqCst) <= 4096);
                    assert_eq!(cursor.failure, Some(error));
                    assert!(cursor.digest.is_none());
                    assert!(cursor.value().is_none());
                    clock.0.store(1, Ordering::SeqCst);
                    let count = calls.load(Ordering::SeqCst);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                    assert_eq!(calls.load(Ordering::SeqCst), count);
                });
            });
        }
    }
    let clock = TestClock::new();
    let calls = AtomicU64::new(0);
    let largest = AtomicU64::new(0);
    let crypto = FaultCrypto {
        clock: &clock,
        calls: &calls,
        largest: &largest,
        fault: 8,
        expire: false,
    };
    let mut source = b"\r\n".to_vec();
    source.extend(std::iter::repeat_n(b'x', 8200));
    crate::store_fs::with_pinned_fixture(&source, &clock, |parent| {
        mapped(&source, 0, |original| {
            let mut cursor = Cursor::new(original, parent, &crypto, Tick(1)).unwrap();
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Yield));
            assert_eq!(cursor.poll(Tick(1)), Ok(Status::Complete));
            assert_eq!(largest.load(Ordering::SeqCst), 4096);
            assert_eq!(calls.load(Ordering::SeqCst), 5);
            cursor.finish(Tick(1)).unwrap();
        });
    });
}

const SIZED_DIGEST_BYTES: usize = std::mem::size_of::<td_crypto::Sha256>() + 1024
    - std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, Provider>>();
#[repr(C, align(8))]
struct SizedDigest {
    padding: [u8; SIZED_DIGEST_BYTES - 1],
    flag: bool,
}
impl Digest for SizedDigest {
    fn update(&mut self, _bytes: &[u8]) -> Result<(), td_crypto::Error> {
        let _ = (self.padding.len(), self.flag);
        Err(td_crypto::Error::Crypto)
    }
    fn finish(self) -> Result<[u8; 32], td_crypto::Error> {
        let _ = (self.padding.len(), self.flag);
        Err(td_crypto::Error::Crypto)
    }
}
struct SizedCrypto<'t>(&'t AtomicU64);
impl Crypto for SizedCrypto<'_> {
    type Sha256 = SizedDigest;
    type SigningKey = <Provider as Crypto>::SigningKey;
    fn sha256(&self) -> Result<Self::Sha256, td_crypto::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(td_crypto::Error::Crypto)
    }
    fn equal_digest(&self, _left: &[u8; 32], _right: &[u8; 32]) -> bool {
        false
    }
    fn generate_p256(&self, _output: &mut [u8]) -> Result<usize, td_crypto::Error> {
        Err(td_crypto::Error::Crypto)
    }
    fn load_p256(&self, _bytes: &[u8]) -> Result<Self::SigningKey, td_crypto::Error> {
        Err(td_crypto::Error::Crypto)
    }
    fn p256_public(
        &self,
        _key: &Self::SigningKey,
        _output: &mut [u8; 65],
    ) -> Result<(), td_crypto::Error> {
        Err(td_crypto::Error::Crypto)
    }
    fn sign_es256(
        &self,
        _key: &Self::SigningKey,
        _bytes: &[u8],
        _output: &mut [u8; 64],
    ) -> Result<(), td_crypto::Error> {
        Err(td_crypto::Error::Crypto)
    }
}
#[test]
fn generic_cursor_includes_header_and_final_digest_in_its_size_cap() {
    let bytes = std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_, SizedCrypto<'_>>>();
    assert!(bytes <= 1024);
    assert!(bytes + std::mem::size_of::<crate::nfc::HeaderBudget>() + 32 > 1024);
    let clock = TestClock::new();
    let calls = AtomicU64::new(0);
    let crypto = SizedCrypto(&calls);
    crate::store_fs::with_pinned_fixture(b"\r\nabc", &clock, |parent| {
        mapped(b"\r\nabc", 0, |original| {
            assert_eq!(
                Cursor::new(original, parent, &crypto, Tick(1)).err(),
                Some(Error::InvalidState)
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
    });
}
