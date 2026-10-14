//! Count then stream one exact JMAP Response from its pinned private file.
use super::{response, result_index::Responses};
use crate::{ports, store_fs::ResponseRetention};
use td_json::{
    arena_emit::{self, BorrowedEmitter},
    string,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
    Capacity,
    Storage(ports::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "JMAP response encoding failure: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy)]
enum Phase {
    Head,
    Entry,
    Name,
    NameEnd,
    Arguments,
    IdPrefix,
    Id,
    EntryEnd,
    StatePrefix,
    State,
    CreatedPrefix,
    Created,
    CreatedEntry,
    End,
    Done,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    /// The only EOF indicator; zero written bytes alone do not imply EOF.
    pub complete: bool,
}

/// Created only by a completed batch, which owns this exact index and file.
/// Counted scratch is private. Finish counting and call begin_output before
/// publishing HTTP headers with the returned length. Only output polls are body
/// fragments. They become eligible for transmission only on successful return.
///
/// Counting uses the same framing and arena emitters as output; argument spans
/// contribute exact lengths without rereading bodies. Output reads at most
/// 4096 retained bytes per turn. All turns retain the original clock, deadline
/// and work meter; no transmission allowance is renewed here. Any output-time
/// refusal requires aborting the HTTP exchange, never emitting a second body.
pub struct Encoder<'q, 'state, 'c, 's, 'r, 'sr, 'a, P> {
    responses: &'q Responses,
    created: Option<&'q super::creation_ids::CreationIds>,
    created_entry: usize,
    created_first: bool,
    created_seed: Option<td_json::arena::Object<'q>>,
    state: &'state str,
    output_started: &'q mut bool,
    retention: &'q mut ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    phase: Phase,
    entry: usize,
    literal_offset: usize,
    arguments_offset: u64,
    emitter: Option<BorrowedEmitter<'q>>,
    state_frame: string::Frame<response::Error>,
    state_offset: usize,
    state_charged: u64,
    total: u64,
    counted: Option<u64>,
    output: bool,
    failure: Option<Error>,
}
impl<P> std::fmt::Debug for Encoder<'_, '_, '_, '_, '_, '_, '_, P> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Encoder { .. }")
    }
}
impl<'q, 'state, 'c, 's, 'r, 'sr, 'a, P> Encoder<'q, 'state, 'c, 's, 'r, 'sr, 'a, P> {
    pub(super) fn new(
        responses: &'q Responses,
        created: Option<&'q super::creation_ids::CreationIds>,
        state: &'state str,
        retention: &'q mut ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
        output_started: &'q mut bool,
    ) -> Self {
        Self {
            responses,
            created,
            created_entry: 0,
            created_first: true,
            created_seed: None,
            state,
            retention,
            output_started,
            phase: Phase::Head,
            entry: 0,
            literal_offset: 0,
            arguments_offset: 0,
            emitter: None,
            state_frame: string::Frame::new(),
            state_offset: 0,
            state_charged: 0,
            total: 0,
            counted: None,
            output: false,
            failure: None,
        }
    }
    /// Bounded private counting. Empty scratch makes no progress.
    pub fn count_step(&mut self, scratch: &mut [u8]) -> Result<bool, Error> {
        if self.output {
            return self.fail(Error::State);
        }
        let next = self.poll(scratch)?;
        if next.complete {
            self.counted = Some(self.total);
        }
        Ok(next.complete)
    }
    /// Freshly admit body emission without renewing work or time. Counting
    /// must be complete, and the transition is allowed exactly once.
    pub fn begin_output(&mut self) -> Result<u64, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.output || self.counted.is_none() || !matches!(self.phase, Phase::Done) {
            return self.fail(Error::State);
        }
        if let Err(error) = self.retention.admit_work_turn() {
            return self.fail(Error::Storage(error));
        }
        self.phase = Phase::Head;
        self.entry = 0;
        self.created_entry = 0;
        self.created_first = true;
        self.created_seed = None;
        self.literal_offset = 0;
        self.arguments_offset = 0;
        self.emitter = None;
        self.state_frame = string::Frame::new();
        self.state_offset = 0;
        self.state_charged = 0;
        self.total = 0;
        self.output = true;
        *self.output_started = true;
        self.counted.ok_or(Error::State)
    }
    pub fn poll_output(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        if !self.output {
            return self.fail(Error::State);
        }
        self.poll(output)
    }
    pub fn finish(self) -> Result<u64, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.output || !matches!(self.phase, Phase::Done) || self.counted != Some(self.total) {
            return Err(Error::State);
        }
        self.retention.admit_work_turn().map_err(Error::Storage)?;
        Ok(self.total)
    }
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        Err(*self.failure.get_or_insert(error))
    }
    fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.turn(output);
        match result {
            Ok(next) => Ok(next),
            Err(error) => self.fail(error),
        }
    }
    fn turn(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        self.retention.admit_work_turn().map_err(Error::Storage)?;
        let length = output.len().min(4096);
        let output = output.get_mut(..length).ok_or(Error::State)?;
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                complete: matches!(self.phase, Phase::Done),
            });
        }
        let written = match self.phase {
            Phase::Head => self.literal(
                br#"{"methodResponses":["#,
                if self.responses.is_empty() {
                    Phase::StatePrefix
                } else {
                    Phase::Entry
                },
                output,
            )?,
            Phase::Entry => self.literal(
                if self.entry == 0 { b"[\"" } else { b",[\"" },
                Phase::Name,
                output,
            )?,
            Phase::Name => {
                let name = self
                    .responses
                    .get(self.entry)
                    .map_err(|_| Error::State)?
                    .ok_or(Error::State)?
                    .name;
                self.literal(name.as_bytes(), Phase::NameEnd, output)?
            }
            Phase::NameEnd => self.literal(b"\",", Phase::Arguments, output)?,
            Phase::Arguments => self.arguments(output)?,
            Phase::IdPrefix => self.literal(b",", Phase::Id, output)?,
            Phase::Id => {
                let token = self
                    .responses
                    .get(self.entry)
                    .map_err(|_| Error::State)?
                    .ok_or(Error::State)?
                    .id_token;
                self.value(token, Phase::EntryEnd, output)?
            }
            Phase::EntryEnd => {
                let n = self.literal(b"]", Phase::Entry, output)?;
                if matches!(self.phase, Phase::Entry) {
                    self.entry = self.entry.checked_add(1).ok_or(Error::State)?;
                    if self.entry == self.responses.len() {
                        self.phase = Phase::StatePrefix;
                    }
                }
                n
            }
            Phase::StatePrefix => self.literal(br#"],"sessionState":"#, Phase::State, output)?,
            Phase::State => self.state(output)?,
            Phase::CreatedPrefix => self.literal(br#","createdIds":"#, Phase::Created, output)?,
            Phase::Created if self.created.is_some() => {
                self.created_seed = self
                    .responses
                    .request()
                    .view()
                    .map_err(|_| Error::State)?
                    .created_ids();
                self.literal(b"{", Phase::CreatedEntry, output)?
            }
            Phase::CreatedEntry => self.creation_pair(output)?,
            Phase::Created => {
                let token = self
                    .responses
                    .request()
                    .envelope
                    .created
                    .ok_or(Error::State)?;
                self.value(token, Phase::End, output)?
            }
            Phase::End => self.literal(b"}", Phase::Done, output)?,
            Phase::Done => 0,
        };
        self.retention.admit_work_turn().map_err(Error::Storage)?;
        let complete = matches!(self.phase, Phase::Done);
        if complete && self.output && self.counted != Some(self.total) {
            return Err(Error::State);
        }
        Ok(Progress { written, complete })
    }
    fn creation_pair(&mut self, output: &mut [u8]) -> Result<usize, Error> {
        let map = self.created.ok_or(Error::State)?;
        let overlay = map.entry(self.created_entry).map_err(|_| Error::State)?;
        let mut seed = self.created_seed;
        let initial = seed.as_mut().and_then(|items| items.next_bytes());
        let (key, value, advance_seed, advance_map) = match (initial, overlay) {
            (None, None) => return self.literal(b"}", Phase::End, output),
            (Some((key, value)), None) => (
                key,
                value.as_string_bytes().ok_or(Error::State)?,
                true,
                false,
            ),
            (None, Some((key, value))) => (key, value, false, true),
            (Some((key, value)), Some((new_key, new_value))) => match key.cmp(new_key) {
                std::cmp::Ordering::Less => (
                    key,
                    value.as_string_bytes().ok_or(Error::State)?,
                    true,
                    false,
                ),
                std::cmp::Ordering::Greater => (new_key, new_value, false, true),
                std::cmp::Ordering::Equal => (
                    key,
                    if new_value.is_empty() {
                        value.as_string_bytes().ok_or(Error::State)?
                    } else {
                        new_value
                    },
                    true,
                    true,
                ),
            },
        };
        let mut bytes = [0; 520];
        let length = if value.is_empty() {
            0
        } else {
            super::creation_ids::pair(key, value, self.created_first, &mut bytes)
                .map_err(|_| Error::State)?
        };
        let written = self.literal(
            bytes.get(..length).ok_or(Error::State)?,
            Phase::CreatedEntry,
            output,
        )?;
        if self.literal_offset == 0 {
            if advance_seed {
                self.created_seed = seed;
            }
            if advance_map {
                self.created_entry += 1;
            }
            if length != 0 {
                self.created_first = false;
            }
        }
        Ok(written)
    }
    fn add(&mut self, bytes: u64) -> Result<(), Error> {
        let maximum = if self.output {
            self.counted.ok_or(Error::State)?
        } else {
            self.retention.maximum()
        };
        self.total = self
            .total
            .checked_add(bytes)
            .filter(|n| *n <= maximum)
            .ok_or(Error::Capacity)?;
        Ok(())
    }
    fn literal(&mut self, bytes: &[u8], next: Phase, output: &mut [u8]) -> Result<usize, Error> {
        let rest = bytes.get(self.literal_offset..).ok_or(Error::State)?;
        let amount = rest.len().min(output.len());
        self.add(amount as u64)?;
        output
            .get_mut(..amount)
            .ok_or(Error::State)?
            .copy_from_slice(rest.get(..amount).ok_or(Error::State)?);
        self.literal_offset = self
            .literal_offset
            .checked_add(amount)
            .ok_or(Error::State)?;
        if self.literal_offset == bytes.len() {
            self.literal_offset = 0;
            self.phase = next;
        }
        Ok(amount)
    }
    fn arguments(&mut self, output: &mut [u8]) -> Result<usize, Error> {
        let span = self
            .responses
            .get(self.entry)
            .map_err(|_| Error::State)?
            .ok_or(Error::State)?
            .arguments;
        if !self.output {
            self.add(span.length())?;
            self.phase = Phase::IdPrefix;
            return Ok(0);
        }
        let left = span
            .length()
            .checked_sub(self.arguments_offset)
            .ok_or(Error::State)?;
        let amount = left.min(output.len() as u64) as usize;
        let offset = span
            .offset()
            .checked_add(self.arguments_offset)
            .ok_or(Error::State)?;
        self.retention
            .read_at(offset, output.get_mut(..amount).ok_or(Error::State)?)
            .map_err(Error::Storage)?;
        self.add(amount as u64)?;
        self.arguments_offset = self
            .arguments_offset
            .checked_add(amount as u64)
            .ok_or(Error::State)?;
        if self.arguments_offset == span.length() {
            self.arguments_offset = 0;
            self.phase = Phase::IdPrefix;
        }
        Ok(amount)
    }
    fn value(&mut self, token: usize, next: Phase, output: &mut [u8]) -> Result<usize, Error> {
        if self.emitter.is_none() {
            let document = self
                .responses
                .request()
                .document
                .view()
                .map_err(|_| Error::State)?;
            self.emitter = Some(
                BorrowedEmitter::new(
                    document,
                    token,
                    self.retention
                        .maximum()
                        .checked_sub(self.total)
                        .ok_or(Error::Capacity)?,
                )
                .map_err(emit_error)?,
            );
        }
        let next_value = self
            .emitter
            .as_mut()
            .ok_or(Error::State)?
            .poll(output)
            .map_err(emit_error)?;
        self.add(next_value.written as u64)?;
        if next_value.complete {
            self.emitter = None;
            self.phase = next;
        }
        Ok(next_value.written)
    }
    fn state(&mut self, output: &mut [u8]) -> Result<usize, Error> {
        let mut written = 0;
        for _ in 0..256 {
            let mut source = response::StateSource {
                value: self.state,
                offset: &mut self.state_offset,
                charged: &mut self.state_charged,
                limit: self.retention.maximum(),
            };
            let next = self
                .state_frame
                .poll(
                    &mut source,
                    (),
                    output.get_mut(written..).ok_or(Error::State)?,
                )
                .map_err(|error| match error {
                    string::Error::Source(response::Error::Capacity) => Error::Capacity,
                    _ => Error::State,
                })?;
            self.add(next.written as u64)?;
            written = written.checked_add(next.written).ok_or(Error::State)?;
            if next.status == string::Status::Complete {
                self.phase = if self.responses.request().envelope.created.is_some() {
                    Phase::CreatedPrefix
                } else {
                    Phase::End
                };
                break;
            }
            if written == output.len() {
                break;
            }
        }
        Ok(written)
    }
}
fn emit_error(error: arena_emit::Error) -> Error {
    match error {
        arena_emit::Error::Invalid => Error::State,
        arena_emit::Error::Limit => Error::Capacity,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::jmap_request::{dispatch, resolution};
    use crate::ports::{Deadline, Tick};
    use std::sync::atomic::Ordering;

    fn with_response(input: &str, state: &str, width: usize) -> Vec<u8> {
        let mut body = Vec::new();
        resolution::tests::run(|coordinator, spool, _, _| {
            let retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let mut preflight = response::Preflight::new(
                dispatch::tests::request(input),
                state,
                retention.maximum(),
            )
            .unwrap();
            while !preflight.step(&mut [0; 4096]).unwrap() {}
            let mut completed = dispatch::run(
                preflight.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), 90).unwrap(),
                crate::ids::AccountId::from_bytes([1; 16]),
            )
            .unwrap();
            let mut encoder = completed.encoder().unwrap();
            let mut scratch = vec![0; width];
            assert_eq!(encoder.counted, None);
            assert!(!encoder.count_step(&mut []).unwrap());
            while !encoder.count_step(&mut scratch).unwrap() {}
            let length = encoder.begin_output().unwrap();
            assert_eq!(encoder.poll_output(&mut []).unwrap().written, 0);
            loop {
                let next = encoder.poll_output(&mut scratch).unwrap();
                assert!(next.written <= 4096);
                body.extend_from_slice(&scratch[..next.written]);
                if next.complete {
                    break;
                }
            }
            assert_eq!(encoder.finish().unwrap(), length);
            assert_eq!(body.len() as u64, length);
            assert!(matches!(completed.encoder(), Err(Error::State)));
        });
        body
    }
    #[test]
    fn exact_framing_preserves_order_references_and_escaped_ids() {
        let input = r##"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{"v":"é\n"},"a\""],["Core/echo",{"#v":{"resultOf":"a\"","name":"Core/echo","path":"/v"}},"a\""],["Missing",{},"c"]]}"##;
        let expected = r#"{"methodResponses":[["Core/echo",{"v":"é\n"},"a\""],["Core/echo",{"v":"é\n"},"a\""],["error",{"type":"unknownMethod"},"c"]],"sessionState":"s\n\"é"}"#;
        for width in [1, 2, 3, 7, 256, 4096, 8192] {
            assert_eq!(with_response(input, "s\n\"é", width), expected.as_bytes());
        }
    }
    #[test]
    fn initial_created_ids_preserve_absent_empty_and_populated_maps() {
        for (suffix, response_suffix) in [
            ("", ""),
            (r#", "createdIds":{}"#, r#","createdIds":{}"#),
            (
                r#", "createdIds":{"a":"id"}"#,
                r#","createdIds":{"a":"id"}"#,
            ),
        ] {
            let input = format!(r#"{{"using":[],"methodCalls":[]{suffix}}}"#);
            let expected =
                format!(r#"{{"methodResponses":[],"sessionState":"s"{response_suffix}}}"#);
            assert_eq!(with_response(&input, "s", 1), expected.as_bytes());
        }
    }
    #[test]
    fn large_seed_maps_stream_without_a_second_map_or_quadratic_seeding() {
        let entries = (0..2000)
            .map(|n| format!("\"{n:04}\":\"v\""))
            .collect::<Vec<_>>()
            .join(",");
        let input = format!(r#"{{"using":[],"methodCalls":[],"createdIds":{{{entries}}}}}"#);
        let expected =
            format!(r#"{{"methodResponses":[],"sessionState":"s","createdIds":{{{entries}}}}}"#);
        assert_eq!(with_response(&input, "s", 4096), expected.as_bytes());
    }
    #[test]
    fn wrong_phase_and_fresh_output_admission_are_terminal() {
        resolution::tests::run(|coordinator, spool, _, clock| {
            let retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let mut preflight = response::Preflight::new(
                dispatch::tests::request(r#"{"using":[],"methodCalls":[]}"#),
                "s",
                retention.maximum(),
            )
            .unwrap();
            while !preflight.step(&mut [0; 4096]).unwrap() {}
            let mut completed = dispatch::run(
                preflight.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), 90).unwrap(),
                crate::ids::AccountId::from_bytes([1; 16]),
            )
            .unwrap();
            {
                let mut encoder = completed.encoder().unwrap();
                assert_eq!(encoder.begin_output(), Err(Error::State));
                assert_eq!(encoder.count_step(&mut [0; 8]), Err(Error::State));
            }
            let mut encoder = completed.encoder().unwrap();
            while !encoder.count_step(&mut [0; 8]).unwrap() {}
            clock.0.store(100, Ordering::SeqCst);
            assert_eq!(
                encoder.begin_output(),
                Err(Error::Storage(ports::Error::Deadline))
            );
            assert_eq!(
                encoder.poll_output(&mut [0; 8]),
                Err(Error::Storage(ports::Error::Deadline))
            );
        });
    }
    #[test]
    fn count_skips_retained_reads_but_output_refuses_a_missing_body() {
        resolution::tests::run(|coordinator, spool, files, _| {
            let retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let input =
                r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{},"a"]]}"#;
            let mut preflight =
                response::Preflight::new(dispatch::tests::request(input), "s", retention.maximum())
                    .unwrap();
            while !preflight.step(&mut [0; 4096]).unwrap() {}
            let mut completed = dispatch::run(
                preflight.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), 90).unwrap(),
                crate::ids::AccountId::from_bytes([1; 16]),
            )
            .unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(files.path.join("request-00/response"))
                .unwrap()
                .set_len(0)
                .unwrap();
            let mut encoder = completed.encoder().unwrap();
            while !encoder.count_step(&mut [0; 8]).unwrap() {}
            encoder.begin_output().unwrap();
            let error = loop {
                match encoder.poll_output(&mut [0; 8]) {
                    Ok(progress) => assert!(!progress.complete),
                    Err(error) => break error,
                }
            };
            assert!(matches!(error, Error::Storage(_)));
            assert_eq!(encoder.poll_output(&mut [0; 8]), Err(error));
            assert_eq!(encoder.finish(), Err(error));
            assert!(matches!(completed.encoder(), Err(Error::State)));
        });
    }
    #[test]
    fn complete_output_still_requires_fresh_final_admission() {
        resolution::tests::run(|coordinator, spool, _, clock| {
            let retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let mut preflight = response::Preflight::new(
                dispatch::tests::request(r#"{"using":[],"methodCalls":[]}"#),
                "s",
                retention.maximum(),
            )
            .unwrap();
            while !preflight.step(&mut [0; 4096]).unwrap() {}
            let mut completed = dispatch::run(
                preflight.finish().unwrap(),
                retention,
                Deadline::after(Tick(0), 90).unwrap(),
                crate::ids::AccountId::from_bytes([1; 16]),
            )
            .unwrap();
            let mut encoder = completed.encoder().unwrap();
            while !encoder.count_step(&mut [0; 8]).unwrap() {}
            encoder.begin_output().unwrap();
            while !encoder.poll_output(&mut [0; 8]).unwrap().complete {}
            clock.0.store(100, Ordering::SeqCst);
            assert_eq!(
                encoder.finish(),
                Err(Error::Storage(ports::Error::Deadline))
            );
        });
    }
    #[test]
    fn long_state_yields_and_large_output_windows_remain_bounded() {
        let input = r#"{"using":[],"methodCalls":[]}"#;
        let state = "x".repeat(1024);
        let expected = format!(r#"{{"methodResponses":[],"sessionState":"{state}"}}"#);
        assert_eq!(with_response(input, &state, 8192), expected.as_bytes());
        let input = format!(
            r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{{"large":"{}"}},"a"]]}}"#,
            "x".repeat(40000)
        );
        assert!(with_response(&input, "s", 8192).len() > 40000);
    }
    #[test]
    fn count_capacity_is_sticky_and_partial_output_cannot_restart() {
        resolution::tests::run(|coordinator, spool, _, clock| {
            let mut retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let responses =
                Responses::new(dispatch::tests::request(r#"{"using":[],"methodCalls":[]}"#));
            let state = "x".repeat(retention.maximum() as usize);
            let mut started = false;
            {
                // Exercise the encoder's own cap independently of preflight's
                // stronger admission, which normally rules this input out.
                let mut encoder =
                    Encoder::new(&responses, None, &state, &mut retention, &mut started);
                let error = loop {
                    match encoder.count_step(&mut [0; 8192]) {
                        Ok(complete) => assert!(!complete),
                        Err(error) => break error,
                    }
                };
                assert_eq!(error, Error::Capacity);
                assert_eq!(encoder.begin_output(), Err(error));
            }
            {
                let mut encoder = Encoder::new(&responses, None, "s", &mut retention, &mut started);
                while !encoder.count_step(&mut [0; 64]).unwrap() {}
                encoder.begin_output().unwrap();
                assert!(!encoder.poll_output(&mut [0; 64]).unwrap().complete);
                clock.0.store(100, Ordering::SeqCst);
                assert_eq!(
                    encoder.poll_output(&mut [0; 64]),
                    Err(Error::Storage(ports::Error::Deadline))
                );
            }
            assert!(started);
        });
    }
}
