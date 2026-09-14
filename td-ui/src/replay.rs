//! The consecutive-frame runner behind a consumer's `--replay`: frames on a
//! reader until EOF, each answered on a writer by one framed reply from the
//! consumer's handler. The runner owns no state and never reads past the
//! frame it was told about; EOF between frames ends the session normally.

use crate::control::{frame, MAX_FRAME};
use std::io::{self, Read, Write};

pub fn run(
    input: &mut impl Read,
    output: &mut impl Write,
    mut handle: impl FnMut(&[u8]) -> String,
) -> io::Result<()> {
    loop {
        let mut header = [0u8; 4];
        // EOF between frames is normal. A partial header is an error.
        loop {
            match input.read(
                header
                    .get_mut(..1)
                    .ok_or_else(|| io::Error::other("header"))?,
            ) {
                Ok(0) => return Ok(()),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        input.read_exact(
            header
                .get_mut(1..)
                .ok_or_else(|| io::Error::other("header"))?,
        )?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(io::Error::other("replay frame length outside 1..=1048576"));
        }
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes)?;
        let reply = handle(&bytes);
        let framed = frame(reply.as_bytes()).map_err(io::Error::other)?;
        output.write_all(&framed)?;
        output.flush()?;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::control::{envelope, ok, Decoder, Error, Refusal};

    fn echo(payload: &[u8]) -> String {
        match envelope::<Error>(payload) {
            Ok(envelope) => ok(envelope.id, envelope.name),
            Err(refusal) => refusal.response(),
        }
    }

    /// One byte per read, so every frame boundary is crossed mid-header.
    struct Bytewise<'a>(&'a [u8]);

    impl Read for Bytewise<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let Some((first, rest)) = self.0.split_first() else {
                return Ok(0);
            };
            let Some(slot) = out.first_mut() else {
                return Ok(0);
            };
            *slot = *first;
            self.0 = rest;
            Ok(1)
        }
    }

    fn replies(output: &[u8]) -> Vec<String> {
        let mut decoder = Decoder::default();
        let mut replies = Vec::new();
        // Bytes since the last complete frame: a trailing partial header or
        // body is a runner fault the reply list alone would not show.
        let mut pending = 0;
        for byte in output {
            decoder.push(std::slice::from_ref(byte)).unwrap();
            pending += 1;
            if decoder.payload().is_some() {
                let payload = std::mem::take(&mut decoder).finish().unwrap();
                replies.push(String::from_utf8(payload).unwrap());
                pending = 0;
            }
        }
        assert_eq!(pending, 0, "partial trailing reply");
        replies
    }

    #[test]
    fn consecutive_frames_are_each_answered_once_and_eof_between_frames_is_normal() {
        let mut stream = Vec::new();
        for request in [&b"1\t1\tnew"[..], b"2\t2\tnew", b"1\t3\tstate\tx"] {
            stream.extend_from_slice(&frame(request).unwrap());
        }
        let mut output = Vec::new();
        run(&mut Bytewise(&stream), &mut output, echo).unwrap();
        assert_eq!(
            replies(&output),
            [
                "1\t1\tok\tnew".to_string(),
                Refusal {
                    id: 0,
                    error: Error::Protocol
                }
                .response(),
                "1\t3\tok\tstate".to_string(),
            ]
        );
        let mut output = Vec::new();
        run(&mut &b""[..], &mut output, echo).unwrap();
        assert!(output.is_empty());
        // The handler sees exactly the payload, in order, once each.
        let mut seen = Vec::new();
        run(&mut &stream[..], &mut Vec::new(), |payload| {
            seen.push(payload.to_vec());
            "1\t0\tok\t".into()
        })
        .unwrap();
        assert_eq!(seen, [&b"1\t1\tnew"[..], b"2\t2\tnew", b"1\t3\tstate\tx"]);
    }

    #[test]
    fn partial_headers_zero_and_oversized_lengths_and_short_bodies_are_errors() {
        for bad in [
            vec![0, 0, 0],
            vec![0, 0, 0, 0],
            ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
            vec![0, 0, 0, 5, b'1'],
        ] {
            assert!(
                run(&mut bad.as_slice(), &mut Vec::new(), echo).is_err(),
                "{bad:?}"
            );
        }
        // Frames before the bad one were still answered.
        let mut stream = frame(b"1\t4\tnew").unwrap();
        stream.extend_from_slice(&[0, 0, 0, 0]);
        let mut output = Vec::new();
        assert!(run(&mut stream.as_slice(), &mut output, echo).is_err());
        assert_eq!(replies(&output), ["1\t4\tok\tnew".to_string()]);
        // A handler reply the frame encoder refuses is an error, not a
        // silent drop; the ceiling is the frame's.
        let request = frame(b"1\t5\tnew").unwrap();
        assert!(run(&mut request.as_slice(), &mut Vec::new(), |_| String::new()).is_err());
        assert!(run(&mut request.as_slice(), &mut Vec::new(), |_| "x"
            .repeat(MAX_FRAME + 1))
        .is_err());
        assert!(run(&mut request.as_slice(), &mut Vec::new(), |_| "x"
            .repeat(MAX_FRAME))
        .is_ok());
    }
}
