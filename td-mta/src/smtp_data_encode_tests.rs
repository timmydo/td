#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use td_mime::time::Deadline;
fn meter() -> Meter {
    Meter::new(
        Deadline::at(Tick(100)),
        Charge {
            io_bytes: 1_000_000,
            output_bytes: 2_000_000,
            records: 100_000,
            unlinks: 0,
        },
    )
}
fn encoded(input: &[u8], body: Body, split: usize, out_size: usize) -> Result<Vec<u8>, Error> {
    let mut meter = meter();
    let before = meter.remaining();
    let mut owner = Encoder::new(input.len() as u64, body, Tick(0), &mut meter)?;
    let mut result = Vec::new();
    let mut consumed = 0;
    for piece in [&input[..split], &input[split..]] {
        let mut used = 0;
        loop {
            let mut out = [0; 256];
            let step = owner.poll(&piece[used..], &mut out[..out_size], Tick(1), &mut meter)?;
            assert!(step.consumed + step.written <= STEP_TRANSITIONS);
            used += step.consumed;
            consumed += step.consumed;
            result.extend_from_slice(&out[..step.written]);
            if matches!(step.status, Status::NeedInput | Status::Complete) {
                break;
            }
        }
    }
    owner.finish(Tick(2), &mut meter)?;
    assert_eq!(consumed, input.len());
    assert_eq!(
        before.io_bytes - meter.remaining().io_bytes,
        input.len() as u64
    );
    assert_eq!(
        before.output_bytes - meter.remaining().output_bytes,
        result.len() as u64
    );
    assert!(!result.starts_with(b".\r\n"));
    assert!(!result.windows(5).any(|s| s == b"\r\n.\r\n"));
    Ok(result)
}
#[test]
fn transparency_is_exact_across_every_split_and_single_byte_output() {
    for (input, want) in [
        (b"".as_slice(), b"".as_slice()),
        (b"\r\n", b"\r\n"),
        (b".\r\n", b"..\r\n"),
        (b"..\r\n", b"...\r\n"),
        (
            b"Subject: .\r\n\r\n.first\r\n..next\r\nlast.\r\n",
            b"Subject: .\r\n\r\n..first\r\n...next\r\nlast.\r\n",
        ),
        (b"a\t\x7fb\r\n", b"a\t\x7fb\r\n"),
    ] {
        for split in 0..=input.len() {
            for out_size in [1, 2, 256] {
                assert_eq!(
                    encoded(input, Body::SevenBit, split, out_size).unwrap(),
                    want
                );
            }
        }
    }
}
#[test]
fn strict_framing_and_seven_bit_policy_refuse_without_repair() {
    for input in [
        b"a\n".as_slice(),
        b"a\rb\r\n",
        b"a\r\r\n",
        b"a\0b\r\n",
        b"unterminated",
        b"last\r",
        b".\nQUIT\r\n",
        b"\xff\r\n",
    ] {
        for split in 0..=input.len() {
            assert_eq!(
                encoded(input, Body::SevenBit, split, 1),
                Err(Error::InvalidData)
            );
        }
    }
    assert_eq!(
        encoded(b"\xff\r\n.\x80\r\n", Body::EightBit, 4, 1).unwrap(),
        b"\xff\r\n..\x80\r\n"
    );
    assert_eq!(
        encoded(b"\0\r\n", Body::EightBit, 0, 1),
        Err(Error::InvalidData)
    );
}
#[test]
fn line_limit_excludes_transparency_and_includes_required_crlf() {
    for first in *b"a." {
        let mut input = vec![b'x'; MAX_LINE_CONTENT];
        input[0] = first;
        input.extend_from_slice(b"\r\n");
        let output = encoded(&input, Body::SevenBit, MAX_LINE_CONTENT, 1).unwrap();
        assert_eq!(output.len(), 1000 + usize::from(first == b'.'));
        input.insert(MAX_LINE_CONTENT, b'x');
        assert_eq!(
            encoded(&input, Body::SevenBit, MAX_LINE_CONTENT, 256),
            Err(Error::InvalidData)
        );
    }
}
#[test]
fn extent_backpressure_and_fresh_completion_checks_hold_original_owner() {
    let mut meter = meter();
    let mut owner = Encoder::new(3, Body::SevenBit, Tick(0), &mut meter).unwrap();
    let first = owner
        .poll(b".\r\nTRAIL", &mut [], Tick(1), &mut meter)
        .unwrap();
    assert_eq!(first.consumed, 1);
    assert_eq!(first.written, 0);
    assert_eq!(first.status, Status::NeedOutput);
    let mut out = [0; 16];
    let step = owner
        .poll(b"\r\nTRAIL", &mut out, Tick(2), &mut meter)
        .unwrap();
    assert_eq!(step.consumed, 2);
    assert_eq!(step.status, Status::Complete);
    assert_eq!(&out[..step.written], b"..\r\n");
    assert_eq!(
        owner.finish(Tick(100), &mut meter),
        Err(Error::Work(Stop::Deadline))
    );

    let mut original = self::meter();
    let mut replacement = self::meter();
    let mut owner = Encoder::new(2, Body::SevenBit, Tick(0), &mut original).unwrap();
    assert_eq!(
        owner.poll(b"\r\n", &mut out, Tick(1), &mut replacement),
        Err(Error::InvalidOwner)
    );
    assert_eq!(
        owner.poll(b"\r\n", &mut out, Tick(1), &mut original),
        Err(Error::InvalidOwner)
    );
    assert_eq!(
        owner.finish(Tick(1), &mut original),
        Err(Error::InvalidOwner)
    );
    let owner = Encoder::new(2, Body::SevenBit, Tick(0), &mut original).unwrap();
    assert_eq!(
        owner.finish(Tick(1), &mut original),
        Err(Error::InvalidState)
    );
}
#[test]
fn budget_and_data_refusals_are_sticky_under_backpressure() {
    for (capacity, expected) in [
        (
            Charge {
                records: 1,
                output_bytes: 10,
                ..Charge::default()
            },
            Error::Work(Stop::IoBytes),
        ),
        (
            Charge {
                records: 1,
                io_bytes: 10,
                ..Charge::default()
            },
            Error::Work(Stop::OutputBytes),
        ),
        (
            Charge {
                io_bytes: 10,
                output_bytes: 10,
                ..Charge::default()
            },
            Error::Work(Stop::Records),
        ),
    ] {
        let mut meter = Meter::new(Deadline::at(Tick(100)), capacity);
        let mut owner = Encoder::new(3, Body::SevenBit, Tick(0), &mut meter).unwrap();
        assert_eq!(
            owner.poll(b".\r\n", &mut [0; 8], Tick(1), &mut meter),
            Err(expected)
        );
        assert_eq!(owner.poll(b"", &mut [], Tick(1), &mut meter), Err(expected));
        assert_eq!(owner.finish(Tick(1), &mut meter), Err(expected));
    }
    let mut meter = meter();
    let mut owner = Encoder::new(2, Body::SevenBit, Tick(0), &mut meter).unwrap();
    assert_eq!(
        owner.poll(b"\n\n", &mut [0; 8], Tick(1), &mut meter),
        Err(Error::InvalidData)
    );
    assert_eq!(
        owner.poll(b"\r\n", &mut [0; 8], Tick(1), &mut meter),
        Err(Error::InvalidData)
    );
}
