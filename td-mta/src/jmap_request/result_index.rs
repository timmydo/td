//! Bounded response metadata and first-match lookup for RFC 8620 section 3.7.
//! File spans are passive metadata, not authority to read or publish a spool.
use super::OwnedRequest;
use std::fmt;

const ENTRIES: usize = 128;
const NAME_BYTES: usize = 64;
const ID_BYTES_PER_STEP: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
    Capacity,
    Span,
    Name,
}
impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::State => "invalid response index state",
            Self::Capacity => "response index capacity exhausted",
            Self::Span => "invalid retained response span",
            Self::Name => "invalid response index name",
        })
    }
}
impl std::error::Error for Error {}

/// Exact serialized arguments range. Only the matching retention owner can
/// establish that these bytes are complete JSON and are actually published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Span {
    offset: u64,
    length: u64,
}
impl Span {
    pub fn offset(self) -> u64 {
        self.offset
    }
    pub fn length(self) -> u64 {
        self.length
    }
}
/// Borrowed wire-order metadata; the retention owner still controls reads.
pub struct Response<'a> {
    pub name: &'static str,
    pub call_id: &'a [u8],
    pub(super) id_token: usize,
    pub arguments: Span,
}
#[derive(Clone, Copy)]
struct Entry {
    call: usize,
    id_token: usize,
    name: &'static str,
    arguments: Span,
}

/// Owns the admitted request so entry tokens cannot outlive or change arenas.
/// Response names are bounded server literals; opaque call IDs are never copied.
/// This is an inert index: the dispatcher must retain complete JSON and publish
/// it through its actual ResponseRetention before appending the corresponding
/// entry. The supplied published length is checked, not independently attested.
/// Cancellation returns the request allocations, dropping only this metadata.
pub struct Responses {
    request: OwnedRequest,
    entries: [Option<Entry>; ENTRIES],
    count: usize,
}
impl fmt::Debug for Responses {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Responses { .. }")
    }
}
impl Responses {
    pub fn new(request: OwnedRequest) -> Self {
        Self {
            request,
            entries: [None; ENTRIES],
            count: 0,
        }
    }
    pub fn request(&self) -> &OwnedRequest {
        &self.request
    }
    pub fn get(&self, index: usize) -> Result<Option<Response<'_>>, Error> {
        if index >= self.count {
            return Ok(None);
        }
        let entry = self
            .entries
            .get(index)
            .copied()
            .flatten()
            .ok_or(Error::State)?;
        let document = self.request.document.view().map_err(|_| Error::State)?;
        let call_id = document
            .string_bytes_at(entry.id_token)
            .ok_or(Error::State)?;
        Ok(Some(Response {
            name: entry.name,
            call_id,
            id_token: entry.id_token,
            arguments: entry.arguments,
        }))
    }
    pub fn into_request(self) -> OwnedRequest {
        self.request
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Append in wire order: one or two replies per call, no skipped calls.
    /// The second reply is an implicit method result under the same call ID.
    /// Span arithmetic, name, order and capacity failures leave the index intact.
    /// The dispatcher must preflight these bounds before method effects.
    pub fn append(
        &mut self,
        call: usize,
        name: &'static str,
        offset: u64,
        length: u64,
        published: u64,
    ) -> Result<(), Error> {
        if name.is_empty()
            || name.len() > NAME_BYTES
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_'))
        {
            return Err(Error::Name);
        }
        let end = offset.checked_add(length).ok_or(Error::Span)?;
        if length < 2 || end > published {
            return Err(Error::Span);
        }
        let view = self.request.view().map_err(|_| Error::State)?;
        if call >= view.call_count() {
            return Err(Error::State);
        }
        let previous = self
            .count
            .checked_sub(1)
            .and_then(|i| self.entries.get(i))
            .copied()
            .flatten();
        if let Some(previous) = previous {
            if call != previous.call && call != previous.call.saturating_add(1) {
                return Err(Error::State);
            }
            if call == previous.call
                && self
                    .count
                    .checked_sub(2)
                    .and_then(|i| self.entries.get(i))
                    .copied()
                    .flatten()
                    .is_some_and(|entry| entry.call == call)
            {
                return Err(Error::State);
            }
            if offset
                < previous
                    .arguments
                    .offset
                    .checked_add(previous.arguments.length)
                    .ok_or(Error::Span)?
            {
                return Err(Error::Span);
            }
        } else if call != 0 {
            return Err(Error::State);
        }
        let id_token = if let Some(previous) = previous.filter(|entry| entry.call == call) {
            previous.id_token
        } else {
            // At most 64 tuples; links skip arguments and all string bytes.
            let doc = self.request.document.view().map_err(|_| Error::State)?;
            let calls = doc
                .value_at(self.request.envelope.calls.ok_or(Error::State)?)
                .and_then(|v| v.as_array())
                .ok_or(Error::State)?;
            calls
                .into_iter()
                .nth(call)
                .and_then(|v| v.as_array())
                .and_then(|mut tuple| tuple.nth(2))
                .map(|v| v.token_index())
                .ok_or(Error::State)?
        };
        let slot = self.entries.get_mut(self.count).ok_or(Error::Capacity)?;
        *slot = Some(Entry {
            call,
            id_token,
            name,
            arguments: Span { offset, length },
        });
        self.count += 1;
        Ok(())
    }
    /// Look only at earlier calls. Strings are decoded argument bytes from the
    /// result reference; malformed reference shape is the dispatcher's error.
    /// Every earlier call must be indexed; no current or later call may be.
    /// The borrow freezes the index and strings until lookup is dropped.
    pub fn lookup<'a>(
        &'a self,
        current_call: usize,
        result_of: &'a [u8],
        name: &'a [u8],
    ) -> Result<Lookup<'a>, Error> {
        if current_call >= self.request.view().map_err(|_| Error::State)?.call_count() {
            return Err(Error::State);
        }
        let last_call = self
            .count
            .checked_sub(1)
            .and_then(|i| self.entries.get(i))
            .copied()
            .flatten()
            .map(|entry| entry.call);
        if last_call != current_call.checked_sub(1) {
            return Err(Error::State);
        }
        Ok(Lookup {
            responses: self,
            current_call,
            result_of,
            name,
            entry: 0,
            offset: 0,
            outcome: Progress::Pending,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    Pending,
    Found(Span),
    InvalidResultReference,
}
/// One primary ID and at most 256 call-ID bytes per step, followed by a bounded
/// server-name comparison. The caller enforces its original work/deadline
/// allowance between steps; exhaustion must not become invalidResultReference.
pub struct Lookup<'a> {
    responses: &'a Responses,
    current_call: usize,
    result_of: &'a [u8],
    name: &'a [u8],
    entry: usize,
    offset: usize,
    outcome: Progress,
}
impl fmt::Debug for Lookup<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Lookup { .. }")
    }
}
impl Lookup<'_> {
    pub fn step(&mut self) -> Result<Progress, Error> {
        if self.outcome != Progress::Pending {
            return Ok(self.outcome);
        }
        let Some(entry) = self.responses.entries.get(self.entry).copied().flatten() else {
            self.outcome = Progress::InvalidResultReference;
            return Ok(self.outcome);
        };
        if entry.call >= self.current_call {
            return Err(Error::State);
        }
        let doc = self
            .responses
            .request
            .document
            .view()
            .map_err(|_| Error::State)?;
        let id = doc
            .value_at(entry.id_token)
            .and_then(|v| v.as_string_bytes())
            .ok_or(Error::State)?;
        let end = self.offset.saturating_add(ID_BYTES_PER_STEP).min(id.len());
        if id.len() != self.result_of.len()
            || id.get(self.offset..end).ok_or(Error::State)?
                != self.result_of.get(self.offset..end).ok_or(Error::State)?
        {
            self.entry += 1;
            // An implicit reply has this same token and cannot match if its
            // primary did not. Preserve first-ID semantics without a rescan.
            if self
                .responses
                .entries
                .get(self.entry)
                .copied()
                .flatten()
                .is_some_and(|next| next.id_token == entry.id_token)
            {
                self.entry += 1;
            }
            self.offset = 0;
        } else if end == id.len() {
            // RFC 8620: select the first ID match, THEN check its method name.
            self.outcome = if entry.name.as_bytes() == self.name {
                Progress::Found(entry.arguments)
            } else {
                Progress::InvalidResultReference
            };
        } else {
            self.offset = end;
        }
        Ok(self.outcome)
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
    use td_json::arena::{OwnedCursor, ParseLimits, Token};

    fn responses(ids: &[&str]) -> Responses {
        fixture(ids).0
    }
    fn fixture(ids: &[&str]) -> (Responses, (*const u8, *const Token)) {
        let mut input = format!(r#"{{"using":["{CORE}"],"methodCalls":["#);
        for (i, id) in ids.iter().enumerate() {
            if i != 0 {
                input.push(',');
            }
            input.push_str(&format!(r#"["Core/echo",{{}},"{id}"]"#));
        }
        input.push_str("]}");
        let limits = Limits {
            json_methods: 64,
            ..Limits::default()
        };
        let input_len = input.len();
        let bytes = input.into_bytes().into_boxed_slice();
        let tokens = vec![Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let pointers = (bytes.as_ptr(), tokens.as_ptr());
        let mut parser = OwnedCursor::with_input_length(
            bytes,
            tokens,
            input_len,
            ParseLimits {
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
        (Responses::new(validation.finish().unwrap()), pointers)
    }
    fn find(index: &Responses, call: usize, id: &[u8], name: &[u8]) -> Progress {
        let mut lookup = index.lookup(call, id, name).unwrap();
        loop {
            let result = lookup.step().unwrap();
            if result != Progress::Pending {
                assert_eq!(lookup.step().unwrap(), result);
                return result;
            }
        }
    }
    #[test]
    fn first_duplicate_id_wins_before_name_check_including_implicit_reply() {
        let mut index = responses(&["same", "same", "read"]);
        index.append(0, "EmailSubmission/set", 10, 2, 12).unwrap();
        index.append(0, "Email/set", 20, 2, 22).unwrap();
        index.append(1, "Core/echo", 30, 2, 32).unwrap();
        assert_eq!(
            find(&index, 2, b"same", b"EmailSubmission/set"),
            Progress::Found(Span {
                offset: 10,
                length: 2
            })
        );
        assert_eq!(
            find(&index, 2, b"same", b"Email/set"),
            Progress::InvalidResultReference
        );
        assert_eq!(
            find(&index, 2, b"same", b"Core/echo"),
            Progress::InvalidResultReference
        );
    }
    #[test]
    fn missing_forward_self_and_empty_ids() {
        let mut index = responses(&["", "later", "last"]);
        assert_eq!(
            find(&index, 0, b"", b"Core/echo"),
            Progress::InvalidResultReference
        );
        assert_eq!(
            find(&index, 0, b"later", b"Core/echo"),
            Progress::InvalidResultReference
        );
        assert_eq!(index.lookup(1, b"", b"").err(), Some(Error::State));
        index.append(0, "Core/echo", 0, 2, 2).unwrap();
        assert_eq!(
            find(&index, 1, b"later", b"Core/echo"),
            Progress::InvalidResultReference
        );
        assert_eq!(
            find(&index, 1, b"", b"Core/echo"),
            Progress::Found(Span {
                offset: 0,
                length: 2
            })
        );
        assert_eq!(index.lookup(0, b"", b"").err(), Some(Error::State));
        assert_eq!(index.lookup(2, b"", b"").err(), Some(Error::State));
        index.append(1, "Core/echo", 2, 2, 4).unwrap();
        assert_eq!(
            find(&index, 2, b"missing", b"Core/echo"),
            Progress::InvalidResultReference
        );
        assert_eq!(index.lookup(1, b"", b"").err(), Some(Error::State));
        assert_eq!(index.lookup(3, b"", b"").err(), Some(Error::State));
    }
    #[test]
    fn bounded_long_id_comparison_and_late_mismatch() {
        let id = "x".repeat(1025);
        let mut index = responses(&[&id, "next"]);
        index.append(0, "Core/echo", 0, 2, 2).unwrap();
        let mut lookup = index.lookup(1, id.as_bytes(), b"Core/echo").unwrap();
        for _ in 0..4 {
            assert_eq!(lookup.step().unwrap(), Progress::Pending);
        }
        assert!(matches!(lookup.step().unwrap(), Progress::Found(_)));
        let mismatch = format!("{}y", "x".repeat(1024));
        let mut lookup = index.lookup(1, mismatch.as_bytes(), b"Core/echo").unwrap();
        for _ in 0..5 {
            assert_eq!(lookup.step().unwrap(), Progress::Pending);
        }
        assert_eq!(lookup.step().unwrap(), Progress::InvalidResultReference);
    }
    #[test]
    fn rejected_metadata_never_changes_index() {
        let mut index = responses(&["a", "b", "c"]);
        for (call, name, offset, length, published, error) in [
            (1, "Core/echo", 0, 2, 2, Error::State),
            (0, "", 0, 2, 2, Error::Name),
            (0, "Core/echo", u64::MAX, 2, u64::MAX, Error::Span),
            (0, "Core/echo", 0, 1, 2, Error::Span),
            (0, "Core/echo", 0, 3, 2, Error::Span),
        ] {
            assert_eq!(
                index.append(call, name, offset, length, published),
                Err(error)
            );
            assert!(index.is_empty());
        }
        index.append(0, "error", 2, 2, 4).unwrap();
        assert_eq!(index.append(2, "error", 4, 2, 6), Err(Error::State));
        assert_eq!(index.append(1, "error", 3, 2, 6), Err(Error::Span));
        index.append(0, "error", 4, 2, 6).unwrap();
        assert_eq!(index.append(0, "error", 6, 2, 8), Err(Error::State));
        assert_eq!(index.len(), 2);
        index.append(1, "error", 6, 2, 8).unwrap();
        assert_eq!(index.append(0, "error", 8, 2, 10), Err(Error::State));
    }
    #[test]
    fn all_admitted_calls_fit_with_two_responses() {
        let mut index = responses(&["opaque"; 64]);
        for call in 0..64 {
            for reply in 0..2 {
                let offset = (call * 4 + reply * 2) as u64;
                index
                    .append(call, "Core/echo", offset, 2, offset + 2)
                    .unwrap();
            }
        }
        assert_eq!(index.len(), ENTRIES);
        assert!(Limits {
            json_methods: ENTRIES / 2 + 1,
            ..Limits::default()
        }
        .plan()
        .is_err());
        assert_eq!(index.append(63, "error", 256, 2, 258), Err(Error::State));
        assert!(std::mem::size_of::<Responses>() <= 8192);
        fn send<T: Send>() {}
        send::<Responses>();
        send::<Lookup<'static>>();
    }
    #[test]
    fn decoded_opaque_ids_and_cancellation_keep_original_request() {
        let (mut index, pointers) = fixture(&[r"secret\u002f\uD83D\uDE00", "next"]);
        index.append(0, "Core/echo", 0, 2, 2).unwrap();
        assert!(matches!(
            find(&index, 1, "secret/😀".as_bytes(), b"Core/echo"),
            Progress::Found(_)
        ));
        let text = format!(
            "{index:?} {:?}",
            index.lookup(1, b"secret", b"secret").unwrap()
        );
        assert!(!text.contains("secret"));
        let request = index.into_request();
        assert_eq!(request.view().unwrap().call_count(), 2);
        let (bytes, tokens) = request.into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
    }

    #[test]
    fn implicit_reply_mismatch_is_not_compared_twice() {
        let id = "x".repeat(1025);
        let mut index = responses(&[&id, "next"]);
        index.append(0, "Core/echo", 0, 2, 2).unwrap();
        index.append(0, "Email/set", 2, 2, 4).unwrap();
        let mismatch = format!("{}y", "x".repeat(1024));
        let mut lookup = index.lookup(1, mismatch.as_bytes(), b"Core/echo").unwrap();
        for _ in 0..5 {
            assert_eq!(lookup.step().unwrap(), Progress::Pending);
        }
        assert_eq!(lookup.step().unwrap(), Progress::InvalidResultReference);
    }
    #[test]
    fn server_name_capacity_and_character_bounds() {
        let mut index = responses(&["a"]);
        const LONG: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const EXACT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        for invalid in [LONG, "Core.echo", "Core/écho"] {
            assert_eq!(index.append(0, invalid, 0, 2, 2), Err(Error::Name));
            assert!(index.is_empty());
        }
        index.append(0, EXACT, 0, 2, 2).unwrap();
    }
}
