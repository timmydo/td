#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::time::Deadline;
fn meter() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10000,
            records: 10000,
            output_bytes: 10000,
            unlinks: 0,
        },
    )
}
fn encode(input: &[u8], read: usize, write: usize) -> (Vec<u8>, Charge) {
    let mut work = meter();
    let before = work.remaining();
    let mut encoder = Encoder::new(input.len() as u64, Tick(1), &mut work).unwrap();
    let mut result = Vec::new();
    let mut position = 0;
    for polls in 1..=10000 {
        let end = input.len().min(position + read);
        let mut output = vec![0; write];
        let step = encoder
            .poll(&input[position..end], &mut output, Tick(1), &mut work)
            .unwrap();
        assert!(step.consumed + step.written <= STEP_TRANSITIONS);
        position += step.consumed;
        result.extend_from_slice(&output[..step.written]);
        if step.status == Status::Complete {
            assert_eq!(position, input.len());
            let completed = work.remaining();
            encoder.finish(Tick(1), &mut work).unwrap();
            assert_eq!(work.remaining(), completed);
            assert_eq!(before.records - completed.records, polls);
            return (
                result,
                Charge {
                    io_bytes: before.io_bytes - work.remaining().io_bytes,
                    output_bytes: before.output_bytes - work.remaining().output_bytes,
                    records: before.records - work.remaining().records,
                    unlinks: 0,
                },
            );
        }
    }
    panic!("encoder failed to finish");
}
#[test]
fn literal_vectors_fragments_and_padding() {
    for (input, expected) in [
        ("", ""),
        ("f", "Zg==\r\n"),
        ("fo", "Zm8=\r\n"),
        ("foo", "Zm9v\r\n"),
        ("foob", "Zm9vYg==\r\n"),
        ("fooba", "Zm9vYmE=\r\n"),
        ("foobar", "Zm9vYmFy\r\n"),
    ] {
        for read in 1..=8 {
            for write in 1..=8 {
                let (result, charge) = encode(input.as_bytes(), read, write);
                assert_eq!(result, expected.as_bytes());
                assert_eq!(charge.io_bytes, input.len() as u64);
                assert_eq!(charge.output_bytes, expected.len() as u64);
            }
        }
    }
}
#[test]
fn binary_oracle_and_line_boundaries_do_not_depend_on_chunk_sizes() {
    let input: Vec<u8> = (0..=255).collect();
    for read in [1, 2, 3, 56, 57, 58, 255, 4096] {
        for write in [1, 2, 3, 4, 75, 76, 77, 78, 4096] {
            let (result, charge) = encode(&input, read, write);
            assert_eq!(result, BINARY_ORACLE);
            assert_eq!(charge.io_bytes, 256);
            assert_eq!(charge.output_bytes, result.len() as u64);
        }
    }
    for length in [0usize, 1, 2, 3, 55, 56, 57, 58, 113, 114, 115, 1024] {
        let input: Vec<u8> = (0..length).map(|i| (i * 151) as u8).collect();
        let (output, _) = encode(&input, 13, 7);
        assert_eq!(
            output.len(),
            length.div_ceil(3) * 4 + length.div_ceil(57) * 2
        );
        for line in output.split(|b| *b == b'\n') {
            assert!(line.len() <= 77);
            assert!(line.is_empty() || line.ends_with(b"\r"));
        }
        let mut decoder = crate::base64::Decoder::default();
        let mut work = meter();
        let mut position = 0;
        let mut decoded = Vec::new();
        loop {
            let mut bytes = [0; 31];
            let step = decoder
                .poll(&output[position..], &mut bytes, true, Tick(1), &mut work)
                .unwrap();
            position += step.consumed;
            decoded.extend_from_slice(&bytes[..step.written]);
            if step.status == crate::base64::Status::Complete {
                break;
            }
        }
        assert!(!decoder.is_encoding_problem());
        assert_eq!(decoded, input);
    }
}
#[test]
fn extent_backpressure_and_fresh_finish_are_explicit() {
    let mut work = meter();
    let mut encoder = Encoder::new(1, Tick(1), &mut work).unwrap();
    assert_eq!(
        encoder
            .poll(&[], &mut [0; 8], Tick(1), &mut work)
            .unwrap()
            .status,
        Status::NeedInput
    );
    let step = encoder.poll(b"foo", &mut [], Tick(1), &mut work).unwrap();
    assert_eq!(step.consumed, 1);
    assert_eq!(step.status, Status::NeedOutput);
    let mut output = [0; 8];
    let step = encoder
        .poll(b"ignored", &mut output, Tick(1), &mut work)
        .unwrap();
    assert_eq!(step.consumed, 0);
    assert_eq!(step.status, Status::Complete);
    assert_eq!(&output[..step.written], b"Zg==\r\n");
    assert_eq!(
        encoder.finish(Tick(100), &mut work),
        Err(Error::Work(Stop::Deadline))
    );
    let mut work = meter();
    let encoder = Encoder::new(2, Tick(1), &mut work).unwrap();
    assert_eq!(encoder.finish(Tick(1), &mut work), Err(Error::InvalidState));
}
#[test]
fn original_meter_and_all_budget_refusals_are_sticky() {
    for stop in [Stop::IoBytes, Stop::Records, Stop::OutputBytes] {
        let total = match stop {
            Stop::IoBytes => 3,
            Stop::Records => 1,
            _ => 6,
        };
        for cap in 0..total {
            let mut capacity = meter().remaining();
            match stop {
                Stop::IoBytes => capacity.io_bytes = cap,
                Stop::Records => capacity.records = cap,
                _ => capacity.output_bytes = cap,
            }
            let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), capacity);
            let mut encoder = Encoder::new(3, Tick(1), &mut work).unwrap();
            assert_eq!(
                encoder.poll(b"foo", &mut [0; 16], Tick(1), &mut work),
                Err(Error::Work(stop))
            );
            let mut replacement = meter();
            let before = replacement.remaining();
            assert_eq!(
                encoder.poll(b"", &mut [], Tick(1), &mut replacement),
                Err(Error::Work(stop))
            );
            assert_eq!(replacement.remaining(), before);
        }
    }
    let mut original = meter();
    let mut encoder = Encoder::new(0, Tick(1), &mut original).unwrap();
    assert_eq!(
        encoder
            .poll(b"", &mut [], Tick(1), &mut original)
            .unwrap()
            .status,
        Status::Complete
    );
    let mut replacement = meter();
    let before = replacement.remaining();
    assert_eq!(
        encoder.poll(b"", &mut [], Tick(1), &mut replacement),
        Err(Error::InvalidOwner)
    );
    assert_eq!(replacement.remaining(), before);
    assert_eq!(
        encoder.finish(Tick(1), &mut original),
        Err(Error::InvalidOwner)
    );
    let mut original = meter();
    let mut encoder = Encoder::new(0, Tick(1), &mut original).unwrap();
    encoder.poll(b"", &mut [], Tick(1), &mut original).unwrap();
    assert_eq!(
        encoder.poll(b"", &mut [], Tick(100), &mut original),
        Err(Error::Work(Stop::Deadline))
    );
}

const BINARY_ORACLE: &[u8] = b"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4\r\nOTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3Bx\r\ncnN0dXZ3eHl6e3x9fn+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmq\r\nq6ytrq+wsbKztLW2t7i5uru8vb6/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj\r\n5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==\r\n";

#[test]
fn exact_turn_ceiling_cached_cost_and_undrained_finish() {
    let mut work = meter();
    let mut encoder = Encoder::new(1024, Tick(1), &mut work).unwrap();
    assert_eq!(
        encoder
            .poll(&[0; 1024], &mut [0; 2048], Tick(1), &mut work)
            .unwrap(),
        Progress {
            consumed: 110,
            written: 146,
            status: Status::Yield
        }
    );
    assert_eq!(STEP_TRANSITIONS, 256);
    let mut work = meter();
    let mut encoder = Encoder::new(1, Tick(1), &mut work).unwrap();
    assert_eq!(
        encoder
            .poll(b"f", &mut [0; 4], Tick(1), &mut work)
            .unwrap()
            .status,
        Status::NeedOutput
    );
    assert_eq!(encoder.finish(Tick(1), &mut work), Err(Error::InvalidState));
    let mut work = meter();
    let mut encoder = Encoder::new(0, Tick(1), &mut work).unwrap();
    encoder.poll(&[], &mut [], Tick(1), &mut work).unwrap();
    let before = work.remaining();
    assert_eq!(
        encoder
            .poll(&[], &mut [], Tick(1), &mut work)
            .unwrap()
            .status,
        Status::Complete
    );
    assert_eq!(
        work.remaining(),
        Charge {
            records: before.records - 1,
            ..before
        }
    );
    let completed = work.remaining();
    encoder.finish(Tick(1), &mut work).unwrap();
    assert_eq!(work.remaining(), completed);
}

#[test]
fn completed_finish_rejects_fresh_meter_without_debit() {
    let mut original = meter();
    let mut encoder = Encoder::new(0, Tick(1), &mut original).unwrap();
    encoder.poll(&[], &mut [], Tick(1), &mut original).unwrap();
    let mut replacement = meter();
    let before = replacement.remaining();
    assert_eq!(
        encoder.finish(Tick(1), &mut replacement),
        Err(Error::InvalidOwner)
    );
    assert_eq!(replacement.remaining(), before);
    assert_eq!(replacement.stopped(), None);
}
