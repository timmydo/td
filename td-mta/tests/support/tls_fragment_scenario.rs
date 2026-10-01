//! Observe bounded untrusted handshake reassembly through the crypto facade.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use std::{hint::black_box, sync::Arc};
use td_crypto::{
    ClientConfig, ClockHandle, TlsError, TlsPhase, TlsProgress, TlsProtocol, TlsSession,
    TrustStore, UtcClock,
};

pub const PHASES: [&str; 12] = [
    "baseline",
    "policy",
    "storage",
    "large_constructed",
    "large_pending",
    "large_refused",
    "small_constructed",
    "small_pending",
    "small_refused",
    "over_limit",
    "repeated",
    "dropped",
];
struct FixedClock;
impl UtcClock for FixedClock {
    fn now(&self) -> Option<u64> {
        Some(1_800_000_000)
    }
}

fn fragmented(
    config: &Arc<ClientConfig>,
    wire: &mut [u8; 16_389],
    fragment: usize,
    payload: usize,
    certificate_list: bool,
    expected: TlsError,
    mut observe: impl FnMut(),
) {
    let mut session = TlsSession::client(config.clone(), "localhost").unwrap();
    while session.status().ciphertext_pending != 0 {
        assert!(session.drain_ciphertext(wire).unwrap() > 0);
    }
    observe();
    let header = u32::try_from(payload).unwrap().to_be_bytes();
    let total = payload + 4;
    let mut offset = 0;
    while offset < total {
        let count = fragment.min(total - offset);
        wire[..5].copy_from_slice(&[22, 3, 3, 0, 0]);
        wire[3..5].copy_from_slice(&u16::try_from(count).unwrap().to_be_bytes());
        wire[5..5 + count].fill(0);
        if offset == 0 {
            wire[5] = if certificate_list { 11 } else { 2 };
            wire[6..9].copy_from_slice(&header[1..]);
            if certificate_list {
                let list = payload.checked_sub(3).unwrap();
                assert_eq!(list % 3, 0);
                let length = u32::try_from(list).unwrap().to_be_bytes();
                wire[9..12].copy_from_slice(&length[1..]);
            }
        }
        let final_record = offset + count == total;
        if final_record {
            assert_eq!(session.status().phase, TlsPhase::Handshaking);
            assert!(session.status().handshake.is_none());
            observe();
        }
        let result = session.receive_record(black_box(&wire[..5 + count]), false);
        if final_record {
            assert_eq!(result, Err(expected));
            assert_eq!(session.status().phase, TlsPhase::Failed);
            assert!(session.status().handshake.is_none());
            if certificate_list {
                assert_eq!(session.status().ciphertext_pending, 0);
                assert_eq!(session.read_plaintext(&mut [0; 1]), Err(expected));
                assert_eq!(session.queue_plaintext(b"not authorized"), Err(expected));
                assert_eq!(session.drain_ciphertext(wire), Err(expected));
                assert_eq!(
                    session.receive_record(&[22, 3, 3, 0, 0], false),
                    Err(expected)
                );
            }
        } else {
            assert_eq!(result, Ok(TlsProgress::Bytes(5 + count)));
        }
        offset += count;
    }
    drop(session);
    observe();
}

pub fn run(mut observe: impl FnMut()) {
    observe();
    {
        let trust = TrustStore::public_roots().unwrap();
        let config = Arc::new(
            ClientConfig::new(
                &trust,
                Arc::new(ClockHandle::new(FixedClock)),
                TlsProtocol::Smtp,
            )
            .unwrap(),
        );
        drop(trust);
        observe();
        let mut wire: Box<[u8; 16_389]> = vec![0; 16_389].into_boxed_slice().try_into().unwrap();
        observe();
        for (fragment, boundary) in [(16_384, 65_511), (4_096, 65_451)] {
            fragmented(
                &config,
                &mut wire,
                fragment,
                boundary,
                false,
                TlsError::Protocol,
                &mut observe,
            );
        }
        for (fragment, boundary) in [(16_384, 65_511), (4_096, 65_451)] {
            fragmented(
                &config,
                &mut wire,
                fragment,
                boundary + 1,
                false,
                TlsError::Capacity,
                || {},
            );
        }
        observe();
        for _ in 0..32 {
            for (fragment, boundary) in [(16_384, 65_511), (4_096, 65_451)] {
                for (extra, error) in [(0, TlsError::Protocol), (1, TlsError::Capacity)] {
                    fragmented(
                        &config,
                        &mut wire,
                        fragment,
                        boundary + extra,
                        false,
                        error,
                        || {},
                    );
                }
            }
        }
        observe();
        black_box((&config, &wire));
    }
    observe();
}

pub const CERTIFICATE_LIST_PHASES: [&str; 11] = [
    "baseline",
    "policy",
    "storage",
    "large_constructed",
    "large_pending",
    "large_refused",
    "small_constructed",
    "small_pending",
    "small_refused",
    "repeated",
    "dropped",
];

/// Unexpected TLS 1.2 Certificate lists before ServerHello: no peer evidence.
pub fn run_certificate_list(mut observe: impl FnMut()) {
    observe();
    {
        let trust = TrustStore::public_roots().unwrap();
        let config = Arc::new(
            ClientConfig::new(
                &trust,
                Arc::new(ClockHandle::new(FixedClock)),
                TlsProtocol::Smtp,
            )
            .unwrap(),
        );
        drop(trust);
        observe();
        let mut wire: Box<[u8; 16_389]> = vec![0; 16_389].into_boxed_slice().try_into().unwrap();
        observe();
        // 21800 empty DER entries: 65400 bytes plus the list's three-byte length.
        for fragment in [16_384, 4_096] {
            fragmented(
                &config,
                &mut wire,
                fragment,
                65_403,
                true,
                TlsError::Protocol,
                &mut observe,
            );
        }
        for _ in 0..32 {
            for fragment in [16_384, 4_096] {
                fragmented(
                    &config,
                    &mut wire,
                    fragment,
                    65_403,
                    true,
                    TlsError::Protocol,
                    || {},
                );
            }
        }
        observe();
        black_box((&config, &wire));
    }
    observe();
}
