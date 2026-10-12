//! Ordered, retained Core/echo batches. This worker adapter activates no API.
use super::{pointer, resolution, response, result_index, Invocation, CORE};
use crate::{ports, store_fs::ResponseRetention};
use std::{cmp::Ordering, ops::Range};
use td_json::{arena, arena_emit::BorrowedEmitter};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
    Storage(ports::Error),
    ExecutionDeadline,
    InvalidArguments,
    InvalidResultReference,
    UnknownMethod,
    ResponseCapacity,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "JMAP dispatch failure: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Indexed complete arguments in the exclusively borrowed retention file.
/// Initial createdIds and the captured Session state remain available for the
/// final envelope writer. No API/Session or Core capability is activated here.
pub struct Completed<'s> {
    responses: result_index::Responses,
    state: &'s str,
}
impl<'s> Completed<'s> {
    pub fn responses(&self) -> &result_index::Responses {
        &self.responses
    }
    pub fn session_state(&self) -> &'s str {
        self.state
    }
    pub fn into_responses(self) -> result_index::Responses {
        self.responses
    }
}

/// Execute on a storage worker after authentication and whole-request
/// admission/preflight. The fresh file and index are bound for this entire
/// invocation. All methods here are side-effect free; errors never stop later
/// calls. Terminal original work/I/O/outer-deadline failure aborts the batch.
/// Refusal drops the request arenas; allocation recycling is not promised.
/// PushSubscription, final wire framing and HTTP activation remain separate.
pub fn run<'state, P>(
    plan: response::Plan<'state>,
    retention: &mut ResponseRetention<'_, '_, '_, '_, '_, P>,
    execution: ports::Deadline,
) -> Result<Completed<'state>, Error> {
    if !retention.is_empty() || plan.required_bytes() > retention.maximum() {
        return Err(Error::State);
    }
    // Validate the phase even for an empty batch. An expired valid phase
    // still permits per-method errors, or an empty response with no methods.
    let _ = retention.admit_before(execution).map_err(Error::Storage)?;
    let state = plan.session_state();
    let optional_end = retention
        .maximum()
        .checked_sub(plan.required_bytes())
        .ok_or(Error::State)?;
    let mut responses = result_index::Responses::new(plan.into_request());
    let request = responses.request().view().map_err(|_| Error::State)?;
    let mut core = false;
    for capability in request.using {
        retention.admit_work_turn().map_err(Error::Storage)?;
        core |= capability.as_string_bytes() == Some(CORE.as_bytes());
    }
    let mut calls = Vec::new();
    calls
        .try_reserve_exact(request.call_count())
        .map_err(|_| Error::ResponseCapacity)?;
    for call in request.calls {
        retention.admit_work_turn().map_err(Error::Storage)?;
        calls.push(call.token_index());
    }
    let mut worker = Worker {
        retention,
        execution,
        optional_end,
    };
    for (ordinal, token) in calls.into_iter().enumerate() {
        let result = worker.method(&responses, ordinal, token, core);
        let (name, range) = match result {
            Ok(range) => ("Core/echo", range),
            Err(error) => {
                if let Some(error) = worker.retention.failure() {
                    return Err(Error::Storage(error));
                }
                let body: &[u8] = match error {
                    Error::InvalidArguments => br#"{"type":"invalidArguments"}"#,
                    Error::InvalidResultReference => br#"{"type":"invalidResultReference"}"#,
                    Error::UnknownMethod => br#"{"type":"unknownMethod"}"#,
                    Error::ExecutionDeadline => {
                        br#"{"type":"serverUnavailable","description":"executionDeadline"}"#
                    }
                    Error::ResponseCapacity => {
                        br#"{"type":"serverFail","description":"responseCapacity"}"#
                    }
                    Error::Storage(_) | Error::State => return Err(error),
                };
                worker.retention.rollback_tail().map_err(Error::Storage)?;
                let start = worker.retention.len();
                worker.retention.append(body).map_err(Error::Storage)?;
                let end = worker.retention.publish().map_err(Error::Storage)?;
                ("error", start..end)
            }
        };
        responses
            .append(
                ordinal,
                name,
                range.start,
                range.end.checked_sub(range.start).ok_or(Error::State)?,
                worker.retention.published_len(),
            )
            .map_err(|_| Error::State)?;
    }
    worker.retention.admit_work_turn().map_err(Error::Storage)?;
    Ok(Completed { responses, state })
}
struct Resolved {
    value: usize,
    range: Range<u64>,
}
struct Worker<'io, 'c, 's, 'r, 'sr, 'a, P> {
    retention: &'io mut ResponseRetention<'c, 's, 'r, 'sr, 'a, P>,
    execution: ports::Deadline,
    optional_end: u64,
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    fn check(&mut self) -> Result<(), Error> {
        if !self
            .retention
            .admit_before(self.execution)
            .map_err(Error::Storage)?
        {
            return Err(Error::ExecutionDeadline);
        }
        Ok(())
    }
    fn available(&self) -> Result<u64, Error> {
        self.optional_end
            .checked_sub(self.retention.len())
            .ok_or(Error::ResponseCapacity)
    }
    fn append(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.check()?;
        if bytes.len() as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        self.retention.append(bytes).map_err(Error::Storage)
    }
    fn compare(&mut self, left: &[u8], right: &[u8]) -> Result<Ordering, Error> {
        for (a, b) in left.chunks(256).zip(right.chunks(256)) {
            self.check()?;
            let order = a.cmp(b);
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(left.len().cmp(&right.len()))
    }
    fn collisions(&mut self, call: Invocation<'_>) -> Result<(), Error> {
        let mut references = call.arguments.as_object().ok_or(Error::State)?;
        let mut names = references;
        let mut name = names.next_bytes();
        while let Some((key, _)) = references.next_bytes() {
            self.check()?;
            let Some(normal) = key.strip_prefix(b"#") else {
                continue;
            };
            while let Some((key, _)) = name {
                self.check()?;
                match self.compare(key, normal)? {
                    Ordering::Less => name = names.next_bytes(),
                    Ordering::Equal => return Err(Error::InvalidArguments),
                    Ordering::Greater => break,
                }
            }
        }
        Ok(())
    }
    fn method(
        &mut self,
        responses: &result_index::Responses,
        ordinal: usize,
        token: usize,
        core: bool,
    ) -> Result<Range<u64>, Error> {
        self.check()?;
        let document = responses
            .request()
            .document
            .view()
            .map_err(|_| Error::State)?;
        let call = document
            .value_at(token)
            .and_then(super::invocation)
            .ok_or(Error::State)?;
        self.collisions(call)?;
        let mut resolved = Vec::new();
        let mut arguments = call.arguments.as_object().ok_or(Error::State)?;
        while let Some((key, value)) = arguments.next_bytes() {
            self.check()?;
            if key.first() != Some(&b'#') {
                continue;
            }
            resolved
                .try_reserve(1)
                .map_err(|_| Error::ResponseCapacity)?;
            let range = self.reference(responses, ordinal, value)?;
            resolved.push(Resolved {
                value: value.token_index(),
                range,
            });
        }
        if call.name != b"Core/echo" || !core {
            return Err(Error::UnknownMethod);
        }
        let start = self.retention.len();
        self.arguments(responses, call, &resolved)?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
    fn reference(
        &mut self,
        responses: &result_index::Responses,
        ordinal: usize,
        value: arena::Value<'_>,
    ) -> Result<Range<u64>, Error> {
        let mut fields = value.as_object().ok_or(Error::InvalidResultReference)?;
        let (mut result_of, mut name, mut path) = (None, None, None);
        while let Some((key, value)) = fields.next_bytes() {
            self.check()?;
            match key {
                b"resultOf" => result_of = value.as_string_bytes(),
                b"name" => name = value.as_string_bytes(),
                b"path" => {
                    if let Some(bytes) = value.as_string_bytes() {
                        // Safe str conversion rescans once. Charge that bounded
                        // request-sized scan before invoking the str-based walker.
                        for _ in bytes.chunks(256) {
                            self.check()?;
                        }
                        path = value.as_str();
                        self.check()?;
                    }
                }
                _ => (),
            }
        }
        self.check()?;
        let result_of = result_of.ok_or(Error::InvalidResultReference)?;
        let name = name.ok_or(Error::InvalidResultReference)?;
        let path = path.ok_or(Error::InvalidResultReference)?;
        let mut lookup = responses
            .lookup(ordinal, result_of, name)
            .map_err(|_| Error::State)?;
        let span = loop {
            self.check()?;
            match lookup.step().map_err(|_| Error::State)? {
                result_index::Progress::Pending => (),
                result_index::Progress::Found(span) => break span,
                result_index::Progress::InvalidResultReference => {
                    return Err(Error::InvalidResultReference)
                }
            }
        };
        let end = span
            .offset()
            .checked_add(span.length())
            .ok_or(Error::State)?;
        let maximum = self.available()?;
        let mut resolution =
            resolution::Resolution::new(self.retention, span.offset()..end, path, maximum)
                .map_err(resolution_error)?;
        let mut scratch = [0; 4096];
        loop {
            // The resolution holds the same owner; its poll admits the original
            // deadline. The shorter phase is checked through that owner's clock.
            if !resolution
                .admit_before(self.execution)
                .map_err(resolution_error)?
            {
                resolution.cancel().map_err(resolution_error)?;
                return Err(Error::ExecutionDeadline);
            }
            if resolution.poll(&mut scratch).map_err(resolution_error)? {
                break;
            }
        }
        if !resolution
            .admit_before(self.execution)
            .map_err(resolution_error)?
        {
            resolution.cancel().map_err(resolution_error)?;
            return Err(Error::ExecutionDeadline);
        }
        resolution.finish().map_err(resolution_error)
    }
    fn arguments(
        &mut self,
        responses: &result_index::Responses,
        call: Invocation<'_>,
        resolved: &[Resolved],
    ) -> Result<(), Error> {
        self.append(b"{")?;
        let mut arguments = call.arguments.as_object().ok_or(Error::State)?;
        let mut first = true;
        let mut selected = resolved.iter();
        while let Some((key, value)) = arguments.next_bytes() {
            self.check()?;
            if !first {
                self.append(b",")?;
            }
            first = false;
            let hash = key.first() == Some(&b'#');
            let document = responses
                .request()
                .document
                .view()
                .map_err(|_| Error::State)?;
            let key_token = value.token_index().checked_sub(1).ok_or(Error::State)?;
            let emitter = BorrowedEmitter::member_name(
                document,
                key_token,
                usize::from(hash),
                self.available()?,
            )
            .map_err(emission_error)?;
            self.emit(emitter)?;
            self.append(b":")?;
            if hash {
                let selected = selected
                    .next()
                    .filter(|r| r.value == value.token_index())
                    .ok_or(Error::State)?;
                self.copy(selected.range.clone())?;
            } else {
                let document = responses
                    .request()
                    .document
                    .view()
                    .map_err(|_| Error::State)?;
                let emitter =
                    BorrowedEmitter::new(document, value.token_index(), self.available()?)
                        .map_err(emission_error)?;
                self.emit(emitter)?;
            }
        }
        if selected.next().is_some() {
            return Err(Error::State);
        }
        self.append(b"}")
    }
    fn emit(&mut self, mut emitter: BorrowedEmitter<'_>) -> Result<(), Error> {
        let mut scratch = [0; 4096];
        loop {
            self.check()?;
            let next = emitter.poll(&mut scratch).map_err(emission_error)?;
            self.append(scratch.get(..next.written).ok_or(Error::State)?)?;
            if next.complete {
                return Ok(());
            }
        }
    }
    fn copy(&mut self, range: Range<u64>) -> Result<(), Error> {
        let mut position = range.start;
        let mut scratch = [0; 4096];
        while position < range.end {
            self.check()?;
            let amount = (range.end - position).min(scratch.len() as u64) as usize;
            let chunk = scratch.get_mut(..amount).ok_or(Error::State)?;
            self.retention
                .read_at(position, chunk)
                .map_err(Error::Storage)?;
            self.append(chunk)?;
            position = position.checked_add(amount as u64).ok_or(Error::State)?;
        }
        Ok(())
    }
}
fn emission_error(error: td_json::arena_emit::Error) -> Error {
    match error {
        td_json::arena_emit::Error::Limit => Error::ResponseCapacity,
        td_json::arena_emit::Error::Invalid => Error::State,
    }
}
fn resolution_error(error: resolution::Error) -> Error {
    match error {
        resolution::Error::Pointer(pointer::Error::InvalidResultReference) => {
            Error::InvalidResultReference
        }
        resolution::Error::Store(ports::Error::Capacity) => Error::ResponseCapacity,
        resolution::Error::Store(error) => Error::Storage(error),
        resolution::Error::Pointer(_) | resolution::Error::State => Error::State,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        jmap_request::{OwnedRequest, OwnedValidation},
        limits::Limits,
        ports::{Deadline, Tick},
    };
    fn request(input: &str) -> OwnedRequest {
        let limits = Limits::default();
        let mut bytes = vec![0; limits.json_bytes].into_boxed_slice();
        bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let mut parser = arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            input.len(),
            arena::ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        let mut validation =
            OwnedValidation::new(parser.finish().unwrap(), &limits.plan().unwrap(), &[CORE])
                .unwrap();
        while !validation.step().unwrap() {}
        validation.finish().unwrap()
    }
    fn execute(input: &str, phase: u64) -> Vec<(String, Vec<u8>, Vec<u8>)> {
        execute_control(input, phase, None, None).unwrap()
    }
    type Reply = (String, Vec<u8>, Vec<u8>);
    fn execute_control(
        input: &str,
        phase: u64,
        advance: Option<u64>,
        records: Option<u64>,
    ) -> Result<Vec<Reply>, Error> {
        let mut replies = Vec::new();
        let mut failure = None;
        resolution::tests::run(|coordinator, spool, _, clock| {
            let deadline = Deadline::after(Tick(0), 100).unwrap();
            let mut retention = coordinator.retain_response(spool, deadline).unwrap();
            let mut plan =
                response::Preflight::new(request(input), "session-1", retention.maximum()).unwrap();
            while !plan.step(&mut [0; 4096]).unwrap() {}
            if let Some(target) = advance {
                clock.advance_until(target);
            }
            if let Some(records) = records {
                retention.test_record_budget(records);
            }
            let complete = match run(
                plan.finish().unwrap(),
                &mut retention,
                Deadline::after(Tick(0), phase).unwrap(),
            ) {
                Ok(complete) => complete,
                Err(error) => {
                    failure = Some(error);
                    return;
                }
            };
            assert_eq!(complete.session_state(), "session-1");
            for i in 0..complete.responses().len() {
                let reply = complete.responses().get(i).unwrap().unwrap();
                let mut arguments = vec![0; reply.arguments.length() as usize];
                retention
                    .read_at(reply.arguments.offset(), &mut arguments)
                    .unwrap();
                replies.push((reply.name.into(), arguments, reply.call_id.to_vec()));
            }
            assert!(complete
                .responses()
                .get(complete.responses().len())
                .unwrap()
                .is_none());
            assert_eq!(retention.len(), retention.published_len());
        });
        match failure {
            Some(error) => Err(error),
            None => Ok(replies),
        }
    }
    fn batch(calls: &str) -> String {
        format!(r#"{{"using":["urn:ietf:params:jmap:core"],"methodCalls":{calls}}}"#)
    }
    #[test]
    fn echo_resolves_wildcards_and_first_duplicate_id_in_wire_order() {
        let replies = execute(
            &batch(
                r##"[
            ["Core/echo",{"list":[{"ids":["m1","m2"]},{"ids":["m3"]}]},"a"],
            ["Core/echo",{"#ids":{"resultOf":"a","name":"Core/echo","path":"/list/*/ids"}},"a"],
            ["Core/echo",{"#first":{"resultOf":"a","name":"Core/echo","path":"/list/0/ids/0"}},"c"]
        ]"##,
            ),
            90,
        );
        assert_eq!(replies.len(), 3);
        assert_eq!(
            replies[1],
            (
                "Core/echo".into(),
                br#"{"ids":["m1","m2","m3"]}"#.to_vec(),
                b"a".to_vec()
            )
        );
        assert_eq!(replies[2].1, br#"{"first":"m1"}"#);
    }
    #[test]
    fn errors_are_referenceable_and_later_calls_continue() {
        let replies = execute(
            &batch(
                r##"[
            ["No/such",{},"bad"],
            ["Core/echo",{"#x":{"resultOf":"bad","name":"error","path":"/type"}},"b"],
            ["Core/echo",{"x":0,"#x":{"resultOf":"bad","name":"error","path":"/type"}},"c"],
            ["Core/echo",{"#x":{"resultOf":1,"name":"error","path":"/type"}},"d"],
            ["Core/echo",{"ok":true},"e"],
            ["No/such",{"#x":{"resultOf":"missing","name":"error","path":"/type"}},"f"]
        ]"##,
            ),
            90,
        );
        assert_eq!(replies[0].1, br#"{"type":"unknownMethod"}"#);
        assert_eq!(replies[1].1, br#"{"x":"unknownMethod"}"#);
        assert_eq!(replies[2].1, br#"{"type":"invalidArguments"}"#);
        assert_eq!(replies[3].1, br#"{"type":"invalidResultReference"}"#);
        assert_eq!(replies[4].1, br#"{"ok":true}"#);
        assert_eq!(replies[5].1, br#"{"type":"invalidResultReference"}"#);
    }
    #[test]
    fn terminal_outer_deadline_and_record_exhaustion_abort_the_batch() {
        let input = batch(r#"[["Core/echo",{"x":"value"},"a"]]"#);
        assert_eq!(
            execute_control(&input, 90, Some(100), Some(1)),
            Err(Error::Storage(ports::Error::Capacity))
        );
        let input = batch(&format!(
            r#"[["Core/echo",{{"x":"{}"}},"a"]]"#,
            "x".repeat(50000)
        ));
        assert_eq!(
            execute_control(&input, 99, Some(100), None),
            Err(Error::Storage(ports::Error::Deadline))
        );
    }
    #[test]
    fn mid_method_phase_expiry_rolls_back_and_keeps_later_error_capacity() {
        let input = batch(&format!(
            r#"[["Core/echo",{{"x":"{}"}},"a"],["Core/echo",{{}},"b"]]"#,
            "x".repeat(50000)
        ));
        let replies = execute_control(&input, 50, Some(50), None).unwrap();
        assert_eq!(replies.len(), 2);
        for reply in replies {
            assert_eq!(reply.0, "error");
            assert_eq!(
                reply.1,
                br#"{"type":"serverUnavailable","description":"executionDeadline"}"#
            );
        }
    }
    #[test]
    fn sorted_collision_scan_handles_large_valid_reference_sets() {
        let arguments = (0..1600)
            .map(|i| format!(r##""#r{i:04}":{{"resultOf":"a","name":"Core/echo","path":"/v"}}"##))
            .collect::<Vec<_>>()
            .join(",");
        let input = batch(&format!(
            r#"[["Core/echo",{{"v":1}},"a"],["Core/echo",{{{arguments}}},"b"]]"#
        ));
        let replies = execute(&input, 90);
        assert_eq!(replies[1].0, "Core/echo");
        assert!(replies[1].1.starts_with(br#"{"r0000":1,"#));
        assert!(replies[1].1.ends_with(br#""r1599":1}"#));
    }
    #[test]
    fn collisions_precede_bad_references_independent_of_key_order() {
        let input = batch(r##"[["Core/echo",{"#a":{},"#z":{},"z":1},"x"]]"##);
        let replies = execute(&input, 90);
        assert_eq!(replies[0].0, "error");
        assert_eq!(replies[0].1, br#"{"type":"invalidArguments"}"#);
    }
    #[test]
    fn reference_phase_expiry_cancels_its_unpublished_tail() {
        resolution::tests::run(|coordinator, spool, _, clock| {
            let mut retention = coordinator
                .retain_response(spool, Deadline::after(Tick(0), 100).unwrap())
                .unwrap();
            let source = format!(r#"{{"x":[{}]}}"#, vec!["1"; 100].join(","));
            retention.append(source.as_bytes()).unwrap();
            retention.publish().unwrap();
            let input = batch(
                r##"[["Core/echo",{},"a"],["Core/echo",{"#x":{"resultOf":"a","name":"Core/echo","path":"/x/*"}},"b"]]"##,
            );
            let mut responses = result_index::Responses::new(request(&input));
            responses
                .append(
                    0,
                    "Core/echo",
                    0,
                    source.len() as u64,
                    retention.published_len(),
                )
                .unwrap();
            let view = responses.request().view().unwrap();
            let value = view.calls().nth(1).unwrap().argument("#x").unwrap();
            clock.advance_until(50);
            let mut worker = Worker {
                retention: &mut retention,
                execution: Deadline::after(Tick(0), 50).unwrap(),
                optional_end: 70000,
            };
            assert_eq!(
                worker.reference(&responses, 1, value),
                Err(Error::ExecutionDeadline)
            );
            assert_eq!(retention.len(), source.len() as u64);
            assert_eq!(retention.published_len(), source.len() as u64);
            assert_eq!(retention.failure(), None);
        });
    }
    #[test]
    fn capacity_failure_preserves_room_for_errors_and_later_echo() {
        let calls = format!(
            r#"[["Core/echo",{{"large":"{}"}},"a"],["Core/echo",{{"ok":1}},"b"]]"#,
            "x".repeat(70000)
        );
        let replies = execute(&batch(&calls), 90);
        assert_eq!(
            replies[0].1,
            br#"{"type":"serverFail","description":"responseCapacity"}"#
        );
        assert_eq!(replies[1].1, br#"{"ok":1}"#);
    }
    #[test]
    fn expansion_capacity_after_a_completed_reference_preserves_later_calls() {
        let calls = format!(
            r##"[["Core/echo",{{"large":"{}","small":1}},"a"],["Core/echo",{{"#a":{{"resultOf":"a","name":"Core/echo","path":"/small"}},"#b":{{"resultOf":"a","name":"Core/echo","path":"/large"}}}},"b"],["Core/echo",{{"ok":1}},"c"]]"##,
            "x".repeat(40000)
        );
        let replies = execute(&batch(&calls), 90);
        assert_eq!(replies[0].0, "Core/echo");
        assert_eq!(replies[1].0, "error");
        assert_eq!(
            replies[1].1,
            br#"{"type":"serverFail","description":"responseCapacity"}"#
        );
        assert_eq!(replies[2].0, "Core/echo");
        assert_eq!(replies[2].1, br#"{"ok":1}"#);
    }
    #[test]
    fn phase_expiry_uses_outer_budget_to_retain_every_method_error() {
        let replies = execute(&batch(r#"[["Core/echo",{},"a"],["Core/echo",{},"b"]]"#), 1);
        assert_eq!(replies.len(), 2);
        assert!(replies
            .iter()
            .all(|r| r.1 == br#"{"type":"serverUnavailable","description":"executionDeadline"}"#));
    }
    #[test]
    fn absent_core_declaration_and_empty_batches_remain_distinct() {
        let replies = execute(r#"{"using":[],"methodCalls":[["Core/echo",{},"a"]]}"#, 90);
        assert_eq!(replies[0].1, br#"{"type":"unknownMethod"}"#);
        assert!(execute(r#"{"using":[],"methodCalls":[]}"#, 90).is_empty());
        assert!(execute(r#"{"using":[],"methodCalls":[]}"#, 1).is_empty());
        for phase in [100, 101] {
            assert_eq!(
                execute_control(r#"{"using":[],"methodCalls":[]}"#, phase, None, None),
                Err(Error::Storage(ports::Error::Invalid))
            );
        }
    }
}
