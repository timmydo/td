//! Exact mandatory-response planning over one admitted, immutable request.
//! This is passive capacity evidence, not quota, execution or publication authority.
use super::{Envelope, OwnedRequest, Request, Walk};
use td_json::{arena, arena_emit, string};

const OUTPUT_QUANTUM: usize = 16 * 1024;
const STRING_STEPS: usize = 256;
const DIAGNOSTIC_BYTES: usize = 512;
// All method-error names admitted by this response contract use only ASCII letters or
// digits, at most 64 bytes. Method-specific property/path payloads need separate plans.
const ERROR_NAME_BYTES: usize = 64;
const ERROR_PREFIX: &[u8] = br#"["error",{"type":"serverFail","description":"responseCapacity"},"#;
const ERROR_STRUCTURE_BYTES: usize = br#"["error",{"type":,"description":},"#.len();
const HEAD: &[u8] = br#"{"methodResponses":["#;
const STATE: &[u8] = br#"],"sessionState":"#;
const CREATED: &[u8] = br#","createdIds":"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    Incomplete,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Invalid => "JMAP response planning state invalid",
            Self::Capacity => "JMAP mandatory response capacity exhausted",
            Self::Incomplete => "JMAP response planning incomplete",
        })
    }
}
impl std::error::Error for Error {}

/// Counts with the same encoder used to emit the complete fallback response.
/// Each step uses at most 16 KiB of caller scratch and one bounded writer turn.
/// Scratch contains private response fragments and must not be transmitted.
/// The caller checks original live admission before/after steps and finish.
pub struct Preflight<'s> {
    writer: Writer<'s>,
    extra: u64,
    cap: u64,
    required: Option<u64>,
    failed: Option<Error>,
}
impl<'s> Preflight<'s> {
    /// `state` is the server's captured Session state, never client input.
    /// `cap` must come from the enclosing response reservation. This does not
    /// acquire that reservation or establish that physical writes will succeed.
    /// Constructor refusal drops the request. After construction, recover its
    /// arenas with into_request on cancellation or step refusal.
    pub fn new(request: OwnedRequest, state: &'s str, cap: u64) -> Result<Self, Error> {
        let count = request.view().map_err(|_| Error::Invalid)?.call_count();
        let description = string::capacity_bound(DIAGNOSTIC_BYTES).ok_or(Error::Capacity)?;
        let extra_per_call = description
            .checked_add(ERROR_STRUCTURE_BYTES)
            .and_then(|n| n.checked_add(ERROR_NAME_BYTES + 2))
            .and_then(|n| n.checked_sub(ERROR_PREFIX.len()))
            .ok_or(Error::Capacity)?;
        let extra = widen(count)?
            .checked_mul(widen(extra_per_call)?)
            .ok_or(Error::Capacity)?;
        let wire_cap = cap.checked_sub(extra).ok_or(Error::Capacity)?;
        Ok(Self {
            writer: Writer::new(request, state, wire_cap)?,
            extra,
            cap,
            required: None,
            failed: None,
        })
    }
    /// Empty scratch makes no progress. Refusals latch; previously counted
    /// fragments never constitute a successful capacity plan.
    pub fn step(&mut self, scratch: &mut [u8]) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.required.is_some() {
            return Ok(true);
        }
        let result = self.advance(scratch);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn advance(&mut self, scratch: &mut [u8]) -> Result<bool, Error> {
        if !self.writer.poll(scratch)?.complete {
            return Ok(false);
        }
        // The seed creation map also occupies the private lookup area, apart
        // from its response copy. Future index/overlay bytes are additional.
        let required = self
            .writer
            .total
            .checked_add(self.writer.created_bytes)
            .and_then(|n| n.checked_add(self.extra))
            .filter(|n| *n <= self.cap)
            .ok_or(Error::Capacity)?;
        self.required = Some(required);
        Ok(true)
    }
    /// Consuming refusal drops the request; use into_request to recover it
    /// after a step error or before abandoning incomplete counting.
    pub fn finish(self) -> Result<Plan<'s>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let required = self.required.ok_or(Error::Incomplete)?;
        let state = self.writer.state;
        let seed_bytes = self.writer.created_bytes;
        Ok(Plan {
            request: self.writer.into_request()?,
            state,
            required,
            seed_bytes,
        })
    }
    /// Cancel counting, including after refusal, and recover the same arenas.
    pub fn into_request(self) -> Result<OwnedRequest, Error> {
        self.writer.into_request()
    }
}
impl std::fmt::Debug for Preflight<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Preflight(<redacted>)")
    }
}
/// Bound to the request and captured Session state that were actually counted.
/// The total covers framing, exact call IDs, the seed map and its response copy,
/// plus one error per actual call with a 64-byte ASCII alphanumeric type and a diagnostic of
/// up to 512 UTF-8 bytes. Additional properties, successes, implicit responses,
/// creation records and index bytes must be reserved before method effects.
pub struct Plan<'s> {
    request: OwnedRequest,
    state: &'s str,
    required: u64,
    seed_bytes: u64,
}
impl<'s> Plan<'s> {
    pub const fn session_state(&self) -> &'s str {
        self.state
    }
    pub const fn required_bytes(&self) -> u64 {
        self.required
    }
    pub const fn seed_bytes(&self) -> u64 {
        self.seed_bytes
    }
    pub fn request(&self) -> Result<Request<'_>, super::Error> {
        self.request.view()
    }
    pub fn into_request(self) -> OwnedRequest {
        self.request
    }
    /// Serialize serverFail for every call. Use only before any method effects
    /// and only for a whole-request deterministic failure; a single method
    /// failure must not suppress later calls. Transient pressure needs its own
    /// error mapping. State is the captured pre-dispatch value. A normal
    /// dispatcher must replan if it needs to emit a different Session state.
    /// This is not the method dispatcher or an HTTP response. Constructor
    /// refusal drops the request; no new allocation is made to retain it.
    pub fn fallback(self) -> Result<Fallback<'s>, Error> {
        Ok(Fallback {
            writer: Writer::new(self.request, self.state, self.required)?,
        })
    }
}
impl std::fmt::Debug for Plan<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Plan(<redacted>)")
    }
}
pub struct Fallback<'s> {
    writer: Writer<'s>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub complete: bool,
}
impl Fallback<'_> {
    /// Bytes remain provisional until complete retention and fresh admission.
    /// One call performs one bounded turn; an error retires the whole output.
    pub fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        self.writer.poll(output)
    }
    pub const fn total(&self) -> u64 {
        self.writer.total
    }
    /// Consuming refusal drops the request; into_request cancels with recovery.
    pub fn finish(self) -> Result<OwnedRequest, Error> {
        if let Some(error) = self.writer.failed {
            return Err(error);
        }
        if !matches!(self.writer.phase, Phase::Done) {
            return Err(Error::Incomplete);
        }
        self.writer.into_request()
    }
    pub fn into_request(self) -> Result<OwnedRequest, Error> {
        self.writer.into_request()
    }
}
impl std::fmt::Debug for Fallback<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("Fallback(<redacted>)")
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Head,
    Next,
    Separator,
    Error,
    CallId,
    CallEnd,
    StatePrefix,
    State,
    CreatedPrefix,
    Created,
    End,
    Done,
}
struct Writer<'s> {
    document: Option<arena::OwnedDocument>,
    emitter: Option<arena_emit::OwnedEmitter>,
    envelope: Envelope,
    calls: Walk,
    first: bool,
    call_id: usize,
    state: &'s str,
    state_offset: usize,
    state_charged: u64,
    state_limit: u64,
    state_frame: string::Frame<Error>,
    phase: Phase,
    literal_offset: usize,
    total: u64,
    cap: u64,
    created_bytes: u64,
    failed: Option<Error>,
}
impl<'s> Writer<'s> {
    fn new(request: OwnedRequest, state: &'s str, cap: u64) -> Result<Self, Error> {
        let calls = Walk::array(
            request
                .document
                .view()
                .map_err(|_| Error::Invalid)?
                .value_at(request.envelope.calls.ok_or(Error::Invalid)?)
                .ok_or(Error::Invalid)?,
        )
        .map_err(|_| Error::Invalid)?;
        Ok(Self {
            document: Some(request.document),
            emitter: None,
            envelope: request.envelope,
            calls,
            first: true,
            call_id: 0,
            state,
            state_offset: 0,
            state_charged: 0,
            state_limit: 0,
            state_frame: string::Frame::new(),
            phase: Phase::Head,
            literal_offset: 0,
            total: 0,
            cap,
            created_bytes: 0,
            failed: None,
        })
    }
    fn into_request(self) -> Result<OwnedRequest, Error> {
        let document = match (self.document, self.emitter) {
            (Some(document), None) => document,
            (None, Some(emitter)) => emitter.into_document(),
            _ => return Err(Error::Invalid),
        };
        Ok(OwnedRequest {
            document,
            envelope: self.envelope,
        })
    }
    fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self.turn(output);
        if let Err(error) = result {
            self.failed = Some(error);
        }
        result
    }
    fn turn(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        let length = output.len().min(OUTPUT_QUANTUM);
        let output = output.get_mut(..length).ok_or(Error::Invalid)?;
        if output.is_empty() || matches!(self.phase, Phase::Done) {
            return Ok(Progress {
                written: 0,
                complete: matches!(self.phase, Phase::Done),
            });
        }
        let written = match self.phase {
            Phase::Head => self.literal(HEAD, Phase::Next, output)?,
            Phase::Next => {
                let view = self
                    .document
                    .as_ref()
                    .ok_or(Error::Invalid)?
                    .view()
                    .map_err(|_| Error::Invalid)?;
                if let Some(call) = self.calls.value(&view).map_err(|_| Error::Invalid)? {
                    let mut tuple = call.as_array().ok_or(Error::Invalid)?;
                    let _ = tuple.next().ok_or(Error::Invalid)?;
                    let _ = tuple.next().ok_or(Error::Invalid)?;
                    self.call_id = tuple.next().ok_or(Error::Invalid)?.token_index();
                    self.phase = if self.first {
                        Phase::Error
                    } else {
                        Phase::Separator
                    };
                    self.first = false;
                } else {
                    self.phase = Phase::StatePrefix;
                }
                0
            }
            Phase::Separator => self.literal(b",", Phase::Error, output)?,
            Phase::Error => self.literal(ERROR_PREFIX, Phase::CallId, output)?,
            Phase::CallId => self.value(self.call_id, Phase::CallEnd, false, output)?,
            Phase::CallEnd => self.literal(b"]", Phase::Next, output)?,
            Phase::StatePrefix => {
                let written = self.literal(STATE, Phase::State, output)?;
                if matches!(self.phase, Phase::State) {
                    self.state_limit = self.cap.checked_sub(self.total).ok_or(Error::Capacity)?;
                }
                written
            }
            Phase::State => self.state_value(output)?,
            Phase::CreatedPrefix => self.literal(CREATED, Phase::Created, output)?,
            Phase::Created => self.value(
                self.envelope.created.ok_or(Error::Invalid)?,
                Phase::End,
                true,
                output,
            )?,
            Phase::End => self.literal(b"}", Phase::Done, output)?,
            Phase::Done => 0,
        };
        Ok(Progress {
            written,
            complete: matches!(self.phase, Phase::Done),
        })
    }
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.total = self
            .total
            .checked_add(widen(bytes)?)
            .filter(|n| *n <= self.cap)
            .ok_or(Error::Capacity)?;
        Ok(())
    }
    fn literal(&mut self, bytes: &[u8], next: Phase, output: &mut [u8]) -> Result<usize, Error> {
        let rest = bytes.get(self.literal_offset..).ok_or(Error::Invalid)?;
        let count = rest.len().min(output.len());
        self.charge(count)?;
        output
            .get_mut(..count)
            .ok_or(Error::Invalid)?
            .copy_from_slice(rest.get(..count).ok_or(Error::Invalid)?);
        self.literal_offset = self
            .literal_offset
            .checked_add(count)
            .ok_or(Error::Invalid)?;
        if self.literal_offset == bytes.len() {
            self.literal_offset = 0;
            self.phase = next;
        }
        Ok(count)
    }
    fn value(
        &mut self,
        index: usize,
        next: Phase,
        created: bool,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        if self.emitter.is_none() {
            let cap = self.cap.checked_sub(self.total).ok_or(Error::Capacity)?;
            let document = self.document.take().ok_or(Error::Invalid)?;
            match arena_emit::OwnedEmitter::new(document, index, cap) {
                Ok(emitter) => self.emitter = Some(emitter),
                Err(refusal) => {
                    let error = emission_error(refusal.error());
                    self.document = Some(refusal.into_document());
                    return Err(error);
                }
            }
        }
        let progress = self
            .emitter
            .as_mut()
            .ok_or(Error::Invalid)?
            .poll(output)
            .map_err(emission_error)?;
        self.charge(progress.written)?;
        if progress.complete {
            let emitter = self.emitter.take().ok_or(Error::Invalid)?;
            if created {
                self.created_bytes = emitter.total();
            }
            self.document = Some(emitter.into_document());
            self.phase = next;
        }
        Ok(progress.written)
    }
    fn state_value(&mut self, output: &mut [u8]) -> Result<usize, Error> {
        let mut written = 0;
        for _ in 0..STRING_STEPS {
            let mut source = StateSource {
                value: self.state,
                offset: &mut self.state_offset,
                charged: &mut self.state_charged,
                limit: self.state_limit,
            };
            let progress = self
                .state_frame
                .poll(
                    &mut source,
                    (),
                    output.get_mut(written..).ok_or(Error::Invalid)?,
                )
                .map_err(|e| match e {
                    string::Error::Source(error) => error,
                    string::Error::InvalidState => Error::Invalid,
                })?;
            self.charge(progress.written)?;
            written = written
                .checked_add(progress.written)
                .ok_or(Error::Invalid)?;
            if progress.status == string::Status::Complete {
                self.phase = if self.envelope.created.is_some() {
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
fn widen(bytes: usize) -> Result<u64, Error> {
    u64::try_from(bytes).map_err(|_| Error::Capacity)
}
fn emission_error(error: arena_emit::Error) -> Error {
    match error {
        arena_emit::Error::Invalid => Error::Invalid,
        arena_emit::Error::Limit => Error::Capacity,
    }
}

pub(super) struct StateSource<'a, 's> {
    pub(super) value: &'s str,
    pub(super) offset: &'a mut usize,
    pub(super) charged: &'a mut u64,
    pub(super) limit: u64,
}
impl string::Source for StateSource<'_, '_> {
    type Context = ();
    type Error = Error;
    fn charge_output(&mut self, _: (), bytes: u64) -> Result<(), Error> {
        *self.charged = self
            .charged
            .checked_add(bytes)
            .filter(|n| *n <= self.limit)
            .ok_or(Error::Capacity)?;
        Ok(())
    }
    fn poll(&mut self, _: ()) -> Result<string::Scalar, Error> {
        match self
            .value
            .get(*self.offset..)
            .ok_or(Error::Invalid)?
            .chars()
            .next()
        {
            Some(value) => {
                if crate::unicode::is_noncharacter(value) {
                    return Err(Error::Invalid);
                }
                *self.offset = self
                    .offset
                    .checked_add(value.len_utf8())
                    .ok_or(Error::Invalid)?;
                Ok(string::Scalar::Value(value))
            }
            None => Ok(string::Scalar::Complete),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        jmap_request::{OwnedValidation, CORE},
        limits::Limits,
    };

    fn request(input: &str) -> OwnedRequest {
        request_fixture(input).0
    }
    fn request_fixture(input: &str) -> (OwnedRequest, (*const u8, *const arena::Token)) {
        let limits = Limits::default();
        let mut bytes = vec![0xee; limits.json_bytes].into_boxed_slice();
        bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![arena::Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let pointers = (bytes.as_ptr(), tokens.as_ptr());
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
        (validation.finish().unwrap(), pointers)
    }
    fn plan<'s>(input: &str, state: &'s str, cap: u64, chunk: usize) -> Plan<'s> {
        let mut preflight = Preflight::new(request(input), state, cap).unwrap();
        let mut scratch = vec![0; chunk];
        while !preflight.step(&mut scratch).unwrap() {}
        preflight.finish().unwrap()
    }
    fn render(plan: Plan<'_>, chunk: usize) -> (Vec<u8>, OwnedRequest) {
        let mut writer = plan.fallback().unwrap();
        let mut scratch = vec![0; chunk];
        let mut bytes = Vec::new();
        loop {
            let progress = writer.poll(&mut scratch).unwrap();
            assert!(progress.written <= OUTPUT_QUANTUM);
            bytes.extend_from_slice(&scratch[..progress.written]);
            if progress.complete {
                break;
            }
        }
        assert_eq!(writer.total(), bytes.len() as u64);
        assert_eq!(
            writer.poll(&mut scratch).unwrap(),
            Progress {
                written: 0,
                complete: true
            }
        );
        (bytes, writer.finish().unwrap())
    }
    #[test]
    fn complete_fallback_preserves_order_duplicate_ids_and_exact_escaping() {
        let input = r#"{"using":[],"methodCalls":[["A",{},"é\n\""],["B",{},"é\n\""]],"createdIds":{"z":"id_z","a":"id_a"}}"#;
        let plan = plan(input, "state\n雪", u64::MAX, 1);
        assert_eq!(
            plan.seed_bytes(),
            br#"{"a":"id_a","z":"id_z"}"#.len() as u64
        );
        assert_eq!(plan.request().unwrap().call_count(), 2);
        let (output, original) = render(plan, 1);
        let expected = "{\"methodResponses\":[[\"error\",{\"type\":\"serverFail\",\"description\":\"responseCapacity\"},\"é\\n\\\"\"],[\"error\",{\"type\":\"serverFail\",\"description\":\"responseCapacity\"},\"é\\n\\\"\"]],\"sessionState\":\"state\\n雪\",\"createdIds\":{\"a\":\"id_a\",\"z\":\"id_z\"}}";
        assert_eq!(output, expected.as_bytes());
        assert_eq!(original.view().unwrap().calls().next().unwrap().name, b"A");
    }
    #[test]
    fn absent_and_empty_creation_maps_and_zero_calls_stay_distinct() {
        for (input, expected, seed) in [
            (
                r#"{"using":[],"methodCalls":[]}"#,
                r#"{"methodResponses":[],"sessionState":"s"}"#,
                0,
            ),
            (
                r#"{"using":[],"methodCalls":[],"createdIds":{}}"#,
                r#"{"methodResponses":[],"sessionState":"s","createdIds":{}}"#,
                2,
            ),
        ] {
            let plan = plan(input, "s", u64::MAX, 7);
            assert_eq!(plan.seed_bytes(), seed);
            assert_eq!(plan.required_bytes(), expected.len() as u64 + seed);
            assert_eq!(render(plan, 3).0, expected.as_bytes());
        }
    }
    #[test]
    fn exact_capacity_includes_worst_diagnostic_and_seed_copy() {
        let input = r#"{"using":[],"methodCalls":[["A",{},"c"]],"createdIds":{"a":"b"}}"#;
        let initial = plan(input, "s", u64::MAX, 29);
        let required = initial.required_bytes();
        let seed = initial.seed_bytes();
        let output = String::from_utf8(render(initial, 11).0).unwrap();
        let worst = output
            .replace("serverFail", &"x".repeat(ERROR_NAME_BYTES))
            .replace("responseCapacity", &"\\u0000".repeat(DIAGNOSTIC_BYTES));
        assert_eq!(required, worst.len() as u64 + seed);
        assert_eq!(plan(input, "s", required, 1).required_bytes(), required);
        let mut refused = Preflight::new(request(input), "s", required - 1).unwrap();
        let mut scratch = [0; 19];
        loop {
            match refused.step(&mut scratch) {
                Ok(false) => (),
                Ok(true) => panic!("under-reserved response admitted"),
                Err(error) => {
                    assert_eq!(error, Error::Capacity);
                    break;
                }
            }
        }
        assert_eq!(refused.step(&mut []), Err(Error::Capacity));
        assert_eq!(refused.finish().unwrap_err(), Error::Capacity);
    }
    #[test]
    fn late_seed_capacity_failure_cannot_expose_a_plan() {
        let input = r#"{"using":[],"methodCalls":[],"createdIds":{"key":"value"}}"#;
        let full = plan(input, "s", u64::MAX, 32);
        let cap = full.required_bytes() - 1;
        assert!(cap > full.seed_bytes());
        let mut preflight = Preflight::new(request(input), "s", cap).unwrap();
        let mut scratch = [0; 32];
        while matches!(preflight.step(&mut scratch), Ok(false)) {}
        // The entire wire response fits; the second map copy does not.
        assert!(matches!(preflight.writer.phase, Phase::Done));
        assert_eq!(preflight.step(&mut scratch), Err(Error::Capacity));
        assert_eq!(preflight.finish().unwrap_err(), Error::Capacity);
    }
    #[test]
    fn large_strings_yield_and_moving_keeps_the_original_arenas() {
        let call_id = "a".repeat(60_000);
        let input = format!(r#"{{"using":[],"methodCalls":[["A",{{}},"{call_id}"]]}}"#);
        let (owner, pointers) = request_fixture(&input);
        let mut preflight = Preflight::new(owner, "s", u64::MAX).unwrap();
        assert!(!preflight.step(&mut []).unwrap());
        assert_eq!(preflight.writer.total, 0);
        let mut turns = 0;
        let mut scratch = [0; OUTPUT_QUANTUM * 2];
        while !preflight.step(&mut scratch).unwrap() {
            turns += 1;
            preflight = *Box::new(preflight);
        }
        assert!(turns > 8);
        let (_, owner) = render(preflight.finish().unwrap(), OUTPUT_QUANTUM * 2);
        let (bytes, tokens) = owner.into_buffers();
        assert_eq!(pointers, (bytes.as_ptr(), tokens.as_ptr()));
    }
    #[test]
    fn cancellation_and_early_finish_do_not_grant_a_plan() {
        let input = r#"{"using":[],"methodCalls":[["A",{},"id"]]}"#;
        assert_eq!(
            Preflight::new(request(input), "s", u64::MAX)
                .unwrap()
                .finish()
                .unwrap_err(),
            Error::Incomplete
        );
        let mut preflight = Preflight::new(request(input), "s", u64::MAX).unwrap();
        let mut scratch = [0; 1];
        while preflight.writer.emitter.is_none() {
            assert!(!preflight.step(&mut scratch).unwrap());
        }
        let owner = preflight.into_request().unwrap();
        assert_eq!(owner.view().unwrap().calls().next().unwrap().call_id, b"id");
        let fallback = plan(input, "s", u64::MAX, 9).fallback().unwrap();
        assert_eq!(fallback.finish().err(), Some(Error::Incomplete));
    }

    #[test]
    fn refused_steps_and_fallback_cancellation_recover_original_storage() {
        let input = format!(
            r#"{{"using":[],"methodCalls":[["A",{{}},"{}"]]}}"#,
            "x".repeat(50_000)
        );
        for (state, cap, expected, emitter_active) in [
            ("s", 4096, Error::Capacity, true),
            ("\u{fdd0}", u64::MAX, Error::Invalid, false),
        ] {
            let (owner, pointers) = request_fixture(&input);
            let mut preflight = Preflight::new(owner, state, cap).unwrap();
            let mut scratch = [0; 256];
            while matches!(preflight.step(&mut scratch), Ok(false)) {}
            assert_eq!(preflight.step(&mut scratch), Err(expected));
            assert_eq!(preflight.writer.emitter.is_some(), emitter_active);
            let (bytes, tokens) = preflight.into_request().unwrap().into_buffers();
            assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
        }
        let (owner, pointers) = request_fixture(&input);
        let mut preflight = Preflight::new(owner, "s", u64::MAX).unwrap();
        let mut scratch = [0; 4096];
        while !preflight.step(&mut scratch).unwrap() {}
        let mut fallback = preflight.finish().unwrap().fallback().unwrap();
        while fallback.writer.emitter.is_none() {
            assert!(!fallback.poll(&mut scratch[..1]).unwrap().complete);
        }
        let (bytes, tokens) = fallback.into_request().unwrap().into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
    }
    #[test]
    fn worker_owners_are_send_and_fit_the_existing_writer_estimate() {
        fn send<T: Send>() {}
        send::<Preflight<'static>>();
        send::<Plan<'static>>();
        send::<Fallback<'static>>();
        assert!(std::mem::size_of::<Preflight<'static>>() <= 8192);
        assert!(std::mem::size_of::<Fallback<'static>>() <= 8192);
    }
    #[test]
    fn invalid_server_state_retires_counting_and_debug_is_redacted() {
        let input = r#"{"using":[],"methodCalls":[["SECRET",{},"SECRET"]]}"#;
        let mut preflight = Preflight::new(request(input), "SECRET\u{fdd0}", u64::MAX).unwrap();
        assert!(!format!("{preflight:?}").contains("SECRET"));
        let mut scratch = [0; 13];
        while matches!(preflight.step(&mut scratch), Ok(false)) {}
        assert_eq!(preflight.step(&mut scratch), Err(Error::Invalid));
        assert_eq!(preflight.finish().unwrap_err(), Error::Invalid);
        let plan = plan(input, "SECRET", u64::MAX, 13);
        assert!(!format!("{plan:?}").contains("SECRET"));
        assert!(!format!("{:?}", plan.fallback().unwrap()).contains("SECRET"));
    }
}
