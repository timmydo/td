use super::*;
use crate::{
    ports::{BlobReader, Deadline, IoProgress, Transport},
    smtp_data_encode::Body,
    smtp_relay_body::{Exchange, Progress, Refusal, Scratch, INPUT_BYTES, OUTPUT_BYTES},
};
use td_mime::work::{Charge, Meter};
type Connection = TlsConnection<Box<[u8; TLS_WIRE_BYTES]>>;
type Failed = Refusal<Box<[u8; TLS_WIRE_BYTES]>>;
fn cap() -> Deadline {
    Deadline::after(Tick(0), 1000).unwrap()
}
fn meter() -> Meter {
    Meter::new(
        cap(),
        Charge {
            io_bytes: 64 * 1024 * 1024,
            output_bytes: 64 * 1024 * 1024,
            records: 100_000,
            unlinks: 0,
        },
    )
}
struct Source {
    bytes: Vec<u8>,
    length: u64,
    chunk: usize,
    reads: Vec<(u64, usize)>,
    failure: Option<Error>,
    oversized: bool,
    drift: bool,
}
impl Source {
    fn new(bytes: &[u8], chunk: usize) -> Self {
        Self {
            bytes: bytes.to_vec(),
            length: bytes.len() as u64,
            chunk,
            reads: Vec::new(),
            failure: None,
            oversized: false,
            drift: false,
        }
    }
}
impl BlobReader for Source {
    fn len(&self) -> u64 {
        self.length
    }
    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, Error> {
        assert!(output.len() <= INPUT_BYTES);
        self.reads.push((offset, output.len()));
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.oversized {
            return Ok(output.len() + 1);
        }
        let input = self.bytes.get(offset as usize..).unwrap();
        let n = input.len().min(output.len()).min(self.chunk);
        output[..n].copy_from_slice(&input[..n]);
        if self.drift {
            self.length = self.length.saturating_sub(1);
        }
        Ok(n)
    }
}
fn collect(peer: &mut Connection, bytes: &mut Vec<u8>) {
    let mut buffer = [0; 512];
    if let Ok(IoProgress::Bytes(n)) = peer.read(&mut buffer) {
        bytes.extend_from_slice(&buffer[..n]);
    }
}
fn drive(
    owner: Exchange<'_, Box<[u8; TLS_WIRE_BYTES]>>,
    peer: &mut Connection,
    expected_length: usize,
) -> Result<Vec<u8>, Failed> {
    let mut owner = Some(owner);
    let mut bytes = Vec::new();
    for _ in 0..100_000 {
        collect(peer, &mut bytes);
        match owner.take().unwrap().advance() {
            Ok(Progress::Pending(next)) => owner = Some(next),
            Ok(Progress::Ready(sent)) => {
                // Complete flush leaves only already-encrypted bytes to read.
                for _ in 0..100_000 {
                    collect(peer, &mut bytes);
                    if bytes.len() >= expected_length {
                        break;
                    }
                    std::thread::yield_now();
                }
                assert!(bytes.len() >= expected_length);
                collect(peer, &mut bytes);
                drop(sent.into_buffers().unwrap());
                return Ok(bytes);
            }
            Err(error) => return Err(error),
        }
        std::thread::yield_now();
    }
    panic!("body transfer did not finish");
}
#[test]
fn relay_body_streams_exact_transparency_and_flushes_without_terminator() {
    let _serial = serial();
    let material = Material::new();
    let mut long = Vec::new();
    let mut long_expected = Vec::new();
    for _ in 0..200 {
        long.extend_from_slice(b".line\r\n..next\r\nnormal\r\n");
        long_expected.extend_from_slice(b"..line\r\n...next\r\nnormal\r\n");
    }
    for (plain, expected, body) in [
        (b"".as_slice(), b"".as_slice(), Body::SevenBit),
        (b".\r\n", b"..\r\n", Body::SevenBit),
        (
            b"Subject: test\r\n\r\n..\r\nend\r\n",
            b"Subject: test\r\n\r\n...\r\nend\r\n",
            Body::SevenBit,
        ),
        (b"\xff\r\n.\x80\r\n", b"\xff\r\n..\x80\r\n", Body::EightBit),
        (long.as_slice(), long_expected.as_slice(), Body::SevenBit),
    ] {
        for chunk in [1, 4096] {
            let (session, mut peer) = relay_auth_tests::authenticated_with_advertisement(
                &material,
                b"250-local\r\n250-8BITMIME\r\n250 AUTH PLAIN\r\n",
            );
            let mut source = Source::new(plain, chunk);
            let mut input = [0; INPUT_BYTES];
            let mut output = [0; OUTPUT_BYTES];
            let mut meter = meter();
            let owner = Exchange::new(
                session,
                &mut source,
                body,
                Scratch {
                    input: &mut input,
                    output: &mut output,
                },
                material.source.clone(),
                cap(),
                &mut meter,
            )
            .ok()
            .unwrap();
            let bytes = drive(owner, &mut peer, expected.len()).ok().unwrap();
            assert_eq!(bytes, expected);
            assert!(!bytes.starts_with(b".\r\n"));
            assert!(!bytes.windows(5).any(|window| window == b"\r\n.\r\n"));
            let mut offset = 0;
            for &(position, requested) in &source.reads {
                assert_eq!(position, offset);
                offset += (plain.len() as u64 - offset).min(requested.min(chunk) as u64);
            }
            assert_eq!(offset, plain.len() as u64);
            drop(peer.into_buffers().unwrap());
        }
    }
}

#[test]
fn relay_body_aborts_framing_source_and_meter_failures_without_terminator() {
    let _serial = serial();
    let material = Material::new();
    for case in 0..12 {
        let (session, mut peer) = relay_auth_tests::authenticated(&material);
        let mut source = Source::new(b"normal\r\n", 4096);
        let mut meter = meter();
        let expected = match case {
            0 => {
                source.bytes = b"bare\n".to_vec();
                source.length = 5;
                Error::Invalid
            }
            1 => {
                source.bytes = b"unterminated".to_vec();
                source.length = 12;
                Error::Invalid
            }
            2 => {
                source.bytes = b"\xff\r\n".to_vec();
                source.length = 3;
                Error::Invalid
            }
            3 => {
                source.bytes = b"\0\r\n".to_vec();
                source.length = 3;
                Error::Invalid
            }
            4 => {
                source.chunk = 0;
                Error::Invalid
            }
            5 => {
                source.oversized = true;
                Error::Invalid
            }
            6 => {
                source.failure = Some(Error::Deadline);
                Error::Deadline
            }
            7 => {
                source.drift = true;
                Error::Invalid
            }
            8 => {
                meter = Meter::new(
                    cap(),
                    Charge {
                        records: 100,
                        ..Charge::default()
                    },
                );
                Error::Capacity
            }
            9 => {
                meter = Meter::new(
                    cap(),
                    Charge {
                        io_bytes: 64 * 1024 * 1024,
                        records: 100,
                        ..Charge::default()
                    },
                );
                Error::Capacity
            }
            10 => {
                meter = Meter::new(
                    cap(),
                    Charge {
                        io_bytes: 64 * 1024 * 1024,
                        output_bytes: 8192,
                        ..Charge::default()
                    },
                );
                Error::Capacity
            }
            _ => {
                source.bytes = [vec![b'a'; 999], b"\r\n".to_vec()].concat();
                source.length = source.bytes.len() as u64;
                Error::Invalid
            }
        };
        let mut input = [0; INPUT_BYTES];
        let mut output = [0; OUTPUT_BYTES];
        let owner = Exchange::new(
            session,
            &mut source,
            Body::SevenBit,
            Scratch {
                input: &mut input,
                output: &mut output,
            },
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .ok()
        .unwrap();
        let failure = drive(owner, &mut peer, 0).err().unwrap();
        assert_eq!(failure.error, expected);
        if matches!(case, 8 | 10) {
            assert!(source.reads.is_empty());
        }
        drop(failure.into_buffers().unwrap());
        drop(peer.into_buffers().unwrap());
    }
}
#[test]
fn relay_body_requires_capability_size_and_cells_before_source_or_network_work() {
    let _serial = serial();
    let material = Material::new();
    for case in 0..4 {
        let (session, mut peer) = relay_auth_tests::authenticated(&material);
        let mut source = Source::new(b"body\r\n", 4096);
        let mut input = [0; INPUT_BYTES];
        let mut output = [0; OUTPUT_BYTES];
        let mut scratch = Scratch {
            input: &mut input,
            output: &mut output,
        };
        let body = if case == 0 {
            Body::EightBit
        } else {
            Body::SevenBit
        };
        match case {
            1 => source.length = crate::limits::MAX_MESSAGE_BYTES as u64 + 1,
            2 => scratch.input = &mut scratch.input[..INPUT_BYTES - 1],
            3 => scratch.output = &mut scratch.output[..OUTPUT_BYTES - 1],
            _ => {}
        }
        let mut meter = meter();
        let failure = Exchange::new(
            session,
            &mut source,
            body,
            scratch,
            material.source.clone(),
            cap(),
            &mut meter,
        )
        .err()
        .unwrap();
        assert_eq!(
            failure.error,
            if case == 0 {
                Error::Forbidden
            } else {
                Error::Capacity
            }
        );
        assert!(source.reads.is_empty());
        drop(failure.into_buffers().unwrap());
        let mut received = Vec::new();
        collect(&mut peer, &mut received);
        assert!(received.is_empty());
        drop(peer.into_buffers().unwrap());
    }
}
struct ExpiringClock(std::sync::atomic::AtomicUsize);
impl Clock for ExpiringClock {
    fn sample(&self) -> Result<Time, Error> {
        let count = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(if count >= 3 { 1000 } else { 0 }),
        })
    }
}
#[test]
fn relay_body_rechecks_absolute_deadline_after_source_read() {
    let _serial = serial();
    let material = Material::new();
    let (session, mut peer) = relay_auth_tests::authenticated(&material);
    let mut source = Source::new(b"body\r\n", 4096);
    let mut input = [0; INPUT_BYTES];
    let mut output = [0; OUTPUT_BYTES];
    let mut meter = meter();
    let clock = Arc::new(ExpiringClock(std::sync::atomic::AtomicUsize::new(0)));
    let owner = Exchange::new(
        session,
        &mut source,
        Body::SevenBit,
        Scratch {
            input: &mut input,
            output: &mut output,
        },
        clock,
        cap(),
        &mut meter,
    )
    .ok()
    .unwrap();
    let failure = owner.advance().err().unwrap();
    assert_eq!(failure.error, Error::Deadline);
    assert_eq!(source.reads.len(), 1);
    drop(failure.into_buffers().unwrap());
    let mut received = Vec::new();
    collect(&mut peer, &mut received);
    assert!(received.is_empty());
    drop(peer.into_buffers().unwrap());
}
