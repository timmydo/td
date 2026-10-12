//! Whole-request admission before JMAP method execution. No network activation.
use crate::limits::ResourcePlan;
use std::fmt;
use td_json::arena::{self, Object, ParseLimit, ParseLimits, Token, Value};

pub mod response;
pub mod result_index;

pub const CORE: &str = "urn:ietf:params:jmap:core";

/// Redacted classifications, with internal parser ceilings kept separate from
/// advertised Core limits. Resource errors must not be reported as notJSON.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Configuration,
    Storage,
    NotJson,
    NotRequest,
    UnknownCapability,
    MaxSizeRequest,
    MaxCallsInRequest,
    Resource(ParseLimit),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Configuration => "JMAP request limits cannot admit the basic envelope",
            Self::Storage => "request validation storage or state invalid",
            Self::NotJson => "notJSON",
            Self::NotRequest => "notRequest",
            Self::UnknownCapability => "unknownCapability",
            Self::MaxSizeRequest => "maxSizeRequest",
            Self::MaxCallsInRequest => "maxCallsInRequest",
            Self::Resource(ParseLimit::Bytes) => "request parser byte ceiling",
            Self::Resource(ParseLimit::Depth) => "request parser depth ceiling",
            Self::Resource(ParseLimit::Tokens) => "request parser token ceiling",
        })
    }
}
impl std::error::Error for Error {}

/// Borrows the admitted byte/token arenas. No response or resolved result is retained here.
/// Intentionally has no Debug implementation: arguments can contain secrets.
///
/// ```compile_fail,E0499
/// use td_mta::{jmap_request::{Request, CORE}, limits::Limits};
/// use td_json::arena::Token;
/// let mut bytes = br#"{"using":[],"methodCalls":[]}"#.to_vec();
/// let mut tokens = [Token::EMPTY; 32768];
/// let limits = Limits::default().plan().unwrap();
/// let request = Request::parse(&mut bytes, &mut tokens, &limits, &[CORE]).unwrap();
/// bytes.fill(0);
/// let _ = request.call_count();
/// ```
///
/// ```compile_fail,E0499
/// use td_mta::{jmap_request::{Request, CORE}, limits::Limits};
/// use td_json::arena::Token;
/// let mut bytes = br#"{"using":[],"methodCalls":[]}"#.to_vec();
/// let mut tokens = [Token::EMPTY; 32768];
/// let limits = Limits::default().plan().unwrap();
/// let request = Request::parse(&mut bytes, &mut tokens, &limits, &[CORE]).unwrap();
/// tokens.fill(Token::EMPTY);
/// let _ = request.call_count();
/// ```
pub struct Request<'a> {
    using: arena::Array<'a>,
    calls: arena::Array<'a>,
    created: Option<Object<'a>>,
}
/// Method names and call IDs are decoded, validated UTF-8 bytes. Byte views
/// avoid rescanning unbounded strings when iterating admitted calls.
#[derive(Clone, Copy)]
pub struct Invocation<'a> {
    pub name: &'a [u8],
    pub arguments: Value<'a>,
    pub call_id: &'a [u8],
}
impl<'a> Invocation<'a> {
    pub fn argument(&self, name: &str) -> Option<Value<'a>> {
        self.arguments.get(name)
    }
}
impl<'a> Request<'a> {
    /// Also required before advertising Core limits: admit the envelope with
    /// one Core capability and the configured number of empty-argument calls.
    pub fn validate_resources(resources: &ResourcePlan) -> Result<(), Error> {
        let limits = resources.limits();
        let minimum = limits
            .json_methods
            .checked_mul(4)
            .and_then(|count| count.checked_add(6))
            .ok_or(Error::Configuration)?;
        let minimum_bytes = limits
            .json_methods
            .checked_mul(br#"["Core/echo",{},""],"#.len())
            .and_then(|bytes| {
                bytes.checked_add(
                    br#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[]}"#.len(),
                )
            })
            .and_then(|bytes| bytes.checked_sub(1))
            .ok_or(Error::Configuration)?;
        if limits.json_depth < 4
            || limits.json_tokens < minimum
            || limits.json_bytes < minimum_bytes
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }

    /// The caller must first authenticate and validate the HTTP content type.
    /// `capabilities` is the trusted set actually advertised by the server.
    /// All calls are checked before a Request is returned, including later calls.
    pub fn parse(
        input: &'a mut [u8],
        tokens: &'a mut [Token],
        resources: &ResourcePlan,
        capabilities: &[&str],
    ) -> Result<Self, Error> {
        let mut admission = Validation::new(input, tokens, resources, capabilities)?;
        while !admission.step()? {}
        admission.finish()
    }
    pub fn uses(&self, capability: &str) -> bool {
        self.using
            .into_iter()
            .any(|value| value.as_string_bytes() == Some(capability.as_bytes()))
    }
    pub fn call_count(&self) -> usize {
        self.calls.len()
    }
    /// Input order and duplicate call IDs are preserved; IDs are opaque Strings.
    pub fn calls(&self) -> impl Iterator<Item = Invocation<'a>> + '_ {
        self.calls.into_iter().filter_map(invocation)
    }
    /// Absence and an explicitly empty creation map have different semantics.
    pub fn created_ids(&self) -> Option<Object<'a>> {
        self.created
    }
}
fn invocation(value: Value<'_>) -> Option<Invocation<'_>> {
    let mut values = value.as_array()?;
    if values.len() != 3 {
        return None;
    }
    let name = values.next()?.as_string_bytes()?;
    let arguments = values.next()?;
    arguments.as_object()?;
    let call_id = values.next()?.as_string_bytes()?;
    Some(Invocation {
        name,
        arguments,
        call_id,
    })
}
fn id_chunk(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Resumable whole-envelope admission. No invocation escapes before every
/// string, tuple and creation ID has been checked. The caller supplies live
/// admission/deadline checks between steps and before consuming finish.
pub struct Validation<'a, 'c> {
    parser: Option<arena::Cursor<'a>>,
    document: Option<arena::Document<'a>>,
    capabilities: &'c [&'c str],
    envelope: Envelope,
}
const _: () = assert!(std::mem::size_of::<Validation<'_, '_>>() <= 8192);
impl<'a, 'c> Validation<'a, 'c> {
    pub fn new(
        input: &'a mut [u8],
        tokens: &'a mut [Token],
        resources: &ResourcePlan,
        capabilities: &'c [&'c str],
    ) -> Result<Self, Error> {
        Request::validate_resources(resources)?;
        let limits = resources.limits();
        if tokens.len() < limits.json_tokens {
            return Err(Error::Storage);
        }
        let parser = arena::Cursor::new(
            input,
            tokens,
            ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .map_err(parser_error)?;
        Ok(Self {
            parser: Some(parser),
            document: None,
            capabilities,
            envelope: Envelope::new(limits.json_methods),
        })
    }
    /// One parser step, up to 256 scalar/comparison transitions, or sixteen
    /// structural transitions. The caller checks live admission between turns.
    pub fn step(&mut self) -> Result<bool, Error> {
        if let Some(error) = self.envelope.failed {
            return Err(error);
        }
        if let Some(parser) = self.parser.as_mut() {
            match parser.step().map_err(parser_error) {
                Ok(true) => {
                    match self
                        .parser
                        .take()
                        .ok_or(Error::Storage)?
                        .finish()
                        .map_err(parser_error)
                    {
                        Ok(document) => self.document = Some(document),
                        Err(error) => {
                            self.envelope.failed = Some(error);
                            return Err(error);
                        }
                    }
                }
                Ok(false) => (),
                Err(error) => {
                    self.envelope.failed = Some(error);
                    return Err(error);
                }
            }
            return Ok(false);
        }
        self.envelope.step(
            self.document.as_ref().ok_or(Error::Storage)?,
            self.capabilities,
        )
    }
    pub fn finish(self) -> Result<Request<'a>, Error> {
        self.envelope.request(
            self.document
                .as_ref()
                .ok_or(self.envelope.failed.unwrap_or(Error::Storage))?,
        )
    }
}

/// Whole-envelope validation of an already parsed, owned arena. Construction
/// binds its recorded parser ceilings and token capacity to the ResourcePlan.
/// The caller still enforces live work/deadline admission between every step.
pub struct OwnedValidation<'c> {
    document: arena::OwnedDocument,
    capabilities: &'c [&'c str],
    envelope: Envelope,
}
impl<'c> OwnedValidation<'c> {
    pub fn new(
        document: arena::OwnedDocument,
        resources: &ResourcePlan,
        capabilities: &'c [&'c str],
    ) -> Result<Self, ValidationRefusal> {
        let limits = resources.limits();
        let result = Request::validate_resources(resources).and_then(|()| {
            if document.parse_limits()
                != (ParseLimits {
                    max_bytes: limits.json_bytes,
                    max_depth: limits.json_depth,
                    max_tokens: limits.json_tokens,
                })
            {
                Err(Error::Configuration)
            } else if document.token_capacity() < limits.json_tokens {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        });
        match result {
            Ok(()) => Ok(Self {
                document,
                capabilities,
                envelope: Envelope::new(limits.json_methods),
            }),
            Err(error) => Err(ValidationRefusal { error, document }),
        }
    }
    pub fn step(&mut self) -> Result<bool, Error> {
        self.envelope.step(
            &self.document.view().map_err(parser_error)?,
            self.capabilities,
        )
    }
    pub fn finish(self) -> Result<OwnedRequest, ValidationRefusal> {
        let result = self
            .document
            .view()
            .map_err(parser_error)
            .and_then(|view| self.envelope.request(&view).map(|_| ()));
        match result {
            Ok(()) => Ok(OwnedRequest {
                document: self.document,
                envelope: self.envelope,
            }),
            Err(error) => Err(ValidationRefusal {
                error,
                document: self.document,
            }),
        }
    }
    pub fn into_buffers(self) -> (Box<[u8]>, Box<[Token]>) {
        self.document.into_buffers()
    }
}
/// Failed validation retains the arenas for recycling and exposes no Request.
pub struct ValidationRefusal {
    error: Error,
    document: arena::OwnedDocument,
}
impl ValidationRefusal {
    pub fn error(&self) -> Error {
        self.error
    }
    pub fn into_buffers(self) -> (Box<[u8]>, Box<[Token]>) {
        self.document.into_buffers()
    }
}
impl fmt::Debug for ValidationRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.debug_tuple("ValidationRefusal")
            .field(&self.error)
            .finish()
    }
}
impl fmt::Display for ValidationRefusal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(out)
    }
}
impl std::error::Error for ValidationRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
/// The complete validated envelope owns the immutable arenas it lends.
///
/// ```compile_fail,E0505
/// use td_mta::jmap_request::OwnedRequest;
/// fn recycle_while_borrowed(owner: OwnedRequest) {
///     let request = owner.view().unwrap();
///     let _buffers = owner.into_buffers();
///     let _ = request.calls().next();
/// }
/// ```
pub struct OwnedRequest {
    document: arena::OwnedDocument,
    envelope: Envelope,
}
impl OwnedRequest {
    pub fn view(&self) -> Result<Request<'_>, Error> {
        self.envelope
            .request(&self.document.view().map_err(parser_error)?)
    }
    pub fn into_buffers(self) -> (Box<[u8]>, Box<[Token]>) {
        self.document.into_buffers()
    }
}
const _: () = assert!(std::mem::size_of::<OwnedValidation<'_>>() <= 8192);
#[derive(Clone, Copy)]
enum Stage {
    Strings {
        token: usize,
        offset: usize,
    },
    Fields(Walk),
    Using(Walk),
    Capability {
        rest: Walk,
        name: usize,
        index: usize,
        offset: usize,
    },
    Calls(Walk),
    Created(Walk),
    CreatedKey {
        rest: Walk,
        value: usize,
        key: usize,
        offset: usize,
    },
    CreatedValue {
        rest: Walk,
        value_index: usize,
        offset: usize,
    },
    Done,
}
struct Member<'a> {
    index: usize,
    key: &'a [u8],
    value: Value<'a>,
}
#[derive(Clone, Copy)]
struct Walk {
    next: Option<usize>,
    left: usize,
}
impl Walk {
    fn array(value: Value<'_>) -> Result<Self, Error> {
        Ok(Self {
            next: value.first_child_index(),
            left: value.as_array().ok_or(Error::NotRequest)?.len(),
        })
    }
    fn object(value: Value<'_>) -> Result<Self, Error> {
        Ok(Self {
            next: value.first_child_index(),
            left: value.as_object().ok_or(Error::NotRequest)?.len(),
        })
    }
    fn advance(&mut self, document: &arena::Document<'_>) -> Result<Option<usize>, Error> {
        if self.left == 0 {
            return Ok(None);
        }
        let index = self.next.ok_or(Error::Storage)?;
        self.next = document.next_index(index);
        self.left -= 1;
        Ok(Some(index))
    }
    fn value<'a>(&mut self, document: &arena::Document<'a>) -> Result<Option<Value<'a>>, Error> {
        self.advance(document)?
            .map(|index| document.value_at(index).ok_or(Error::Storage))
            .transpose()
    }
    fn member<'a>(&mut self, document: &arena::Document<'a>) -> Result<Option<Member<'a>>, Error> {
        self.advance(document)?
            .map(|index| {
                let (key, value) = document.member_at(index).ok_or(Error::Storage)?;
                Ok(Member { index, key, value })
            })
            .transpose()
    }
}
struct Envelope {
    maximum_calls: usize,
    stage: Stage,
    raw_using: Option<usize>,
    raw_calls: Option<usize>,
    raw_created: Option<usize>,
    using: Option<usize>,
    calls: Option<usize>,
    created: Option<usize>,
    unknown_capability: bool,
    failed: Option<Error>,
}
impl Envelope {
    fn new(maximum_calls: usize) -> Self {
        Self {
            maximum_calls,
            stage: Stage::Strings {
                token: 0,
                offset: 0,
            },
            raw_using: None,
            raw_calls: None,
            raw_created: None,
            using: None,
            calls: None,
            created: None,
            unknown_capability: false,
            failed: None,
        }
    }
    fn step(
        &mut self,
        document: &arena::Document<'_>,
        capabilities: &[&str],
    ) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        // Stop scalar batches at phase boundaries: byte credits must not
        // become hundreds of structural transitions in the same worker turn.
        let scalar_pass = matches!(self.stage, Stage::Strings { .. });
        let capability_pass = matches!(self.stage, Stage::Capability { .. });
        let turns = if scalar_pass || capability_pass {
            256
        } else {
            16
        };
        for _ in 0..turns {
            if let Err(error) = self.transition(document, capabilities) {
                self.failed = Some(error);
                return Err(error);
            }
            if matches!(self.stage, Stage::Done) {
                return Ok(true);
            }
            if (scalar_pass && !matches!(self.stage, Stage::Strings { .. }))
                || (capability_pass && !matches!(self.stage, Stage::Capability { .. }))
            {
                break;
            }
        }
        Ok(false)
    }
    fn request<'a>(&self, document: &arena::Document<'a>) -> Result<Request<'a>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !matches!(self.stage, Stage::Done) {
            return Err(Error::Storage);
        }
        Ok(Request {
            using: document
                .value_at(self.using.ok_or(Error::Storage)?)
                .and_then(Value::as_array)
                .ok_or(Error::Storage)?,
            calls: document
                .value_at(self.calls.ok_or(Error::Storage)?)
                .and_then(Value::as_array)
                .ok_or(Error::Storage)?,
            created: self
                .created
                .map(|index| {
                    document
                        .value_at(index)
                        .and_then(Value::as_object)
                        .ok_or(Error::Storage)
                })
                .transpose()?,
        })
    }
    fn transition(
        &mut self,
        document: &arena::Document<'_>,
        capabilities: &[&str],
    ) -> Result<(), Error> {
        match self.stage {
            Stage::Strings { token, offset } => {
                if token == document.token_count() {
                    self.stage = Stage::Fields(Walk::object(document.root())?);
                } else if let Some(bytes) = document
                    .string_bytes_at(token)
                    .filter(|bytes| offset < bytes.len())
                {
                    let first = *bytes.get(offset).ok_or(Error::Storage)?;
                    let width = match first {
                        0..=0x7f => 1,
                        0xc2..=0xdf => 2,
                        0xe0..=0xef => 3,
                        0xf0..=0xf4 => 4,
                        _ => return Err(Error::Storage),
                    };
                    let scalar = bytes
                        .get(offset..offset + width)
                        .and_then(|bytes| std::str::from_utf8(bytes).ok())
                        .and_then(|text| text.chars().next())
                        .ok_or(Error::Storage)?;
                    if crate::unicode::is_noncharacter(scalar) {
                        return Err(Error::NotJson);
                    }
                    self.stage = Stage::Strings {
                        token,
                        offset: offset + width,
                    };
                } else {
                    self.stage = Stage::Strings {
                        token: token + 1,
                        offset: 0,
                    };
                }
            }
            Stage::Fields(mut fields) => {
                if let Some(Member { key, value, .. }) = fields.member(document)? {
                    match key {
                        b"using" => self.raw_using = Some(value.token_index()),
                        b"methodCalls" => self.raw_calls = Some(value.token_index()),
                        b"createdIds" => self.raw_created = Some(value.token_index()),
                        _ => (),
                    }
                    self.stage = Stage::Fields(fields);
                } else {
                    let using = Walk::array(
                        self.raw_using
                            .and_then(|index| document.value_at(index))
                            .ok_or(Error::NotRequest)?,
                    )?;
                    self.using = self.raw_using;
                    self.stage = Stage::Using(using);
                }
            }
            Stage::Using(mut using) => {
                self.stage = if let Some(value) = using.value(document)? {
                    Stage::Capability {
                        rest: using,
                        name: {
                            value.as_string_bytes().ok_or(Error::NotRequest)?;
                            value.token_index()
                        },
                        index: 0,
                        offset: 0,
                    }
                } else {
                    let calls = Walk::array(
                        self.raw_calls
                            .and_then(|index| document.value_at(index))
                            .ok_or(Error::NotRequest)?,
                    )?;
                    if calls.left > self.maximum_calls {
                        return Err(Error::MaxCallsInRequest);
                    }
                    self.calls = self.raw_calls;
                    Stage::Calls(calls)
                };
            }
            Stage::Capability {
                rest,
                name,
                index,
                offset,
            } => {
                let name_bytes = document
                    .value_at(name)
                    .and_then(Value::as_string_bytes)
                    .ok_or(Error::Storage)?;
                if let Some(capability) = capabilities.get(index) {
                    if name_bytes.len() != capability.len() {
                        self.stage = Stage::Capability {
                            rest,
                            name,
                            index: index + 1,
                            offset: 0,
                        };
                    } else if offset == name_bytes.len() {
                        self.stage = Stage::Using(rest);
                    } else if name_bytes.get(offset) == capability.as_bytes().get(offset) {
                        self.stage = Stage::Capability {
                            rest,
                            name,
                            index,
                            offset: offset + 1,
                        };
                    } else {
                        self.stage = Stage::Capability {
                            rest,
                            name,
                            index: index + 1,
                            offset: 0,
                        };
                    }
                } else {
                    self.unknown_capability = true;
                    self.stage = Stage::Using(rest);
                }
            }
            Stage::Calls(mut calls) => {
                if let Some(value) = calls.value(document)? {
                    invocation(value).ok_or(Error::NotRequest)?;
                    self.stage = Stage::Calls(calls);
                } else if let Some(value) = self.raw_created {
                    let created = Walk::object(document.value_at(value).ok_or(Error::Storage)?)?;
                    self.created = Some(value);
                    self.stage = Stage::Created(created);
                } else {
                    self.complete()?;
                }
            }
            Stage::Created(mut created) => {
                if let Some(Member {
                    index: key_index,
                    key,
                    value,
                }) = created.member(document)?
                {
                    if !(1..=255).contains(&key.len()) {
                        return Err(Error::NotRequest);
                    }
                    self.stage = Stage::CreatedKey {
                        rest: created,
                        value: value.token_index(),
                        key: key_index,
                        offset: 0,
                    };
                } else {
                    self.complete()?;
                }
            }
            Stage::CreatedKey {
                rest,
                value,
                key,
                offset,
            } => {
                let key_bytes = document.string_bytes_at(key).ok_or(Error::Storage)?;
                let end = offset.saturating_add(8).min(key_bytes.len());
                if !id_chunk(key_bytes.get(offset..end).ok_or(Error::Storage)?) {
                    return Err(Error::NotRequest);
                }
                if end == key_bytes.len() {
                    let bytes = document
                        .value_at(value)
                        .and_then(Value::as_string_bytes)
                        .ok_or(Error::NotRequest)?;
                    if !(1..=255).contains(&bytes.len()) {
                        return Err(Error::NotRequest);
                    }
                    self.stage = Stage::CreatedValue {
                        rest,
                        value_index: value,
                        offset: 0,
                    };
                } else {
                    self.stage = Stage::CreatedKey {
                        rest,
                        value,
                        key,
                        offset: end,
                    };
                }
            }
            Stage::CreatedValue {
                rest,
                value_index,
                offset,
            } => {
                let value_bytes = document
                    .value_at(value_index)
                    .and_then(Value::as_string_bytes)
                    .ok_or(Error::Storage)?;
                let end = offset.saturating_add(8).min(value_bytes.len());
                if !id_chunk(value_bytes.get(offset..end).ok_or(Error::Storage)?) {
                    return Err(Error::NotRequest);
                }
                self.stage = if end == value_bytes.len() {
                    Stage::Created(rest)
                } else {
                    Stage::CreatedValue {
                        rest,
                        value_index,
                        offset: end,
                    }
                };
            }
            Stage::Done => (),
        }
        Ok(())
    }
    fn complete(&mut self) -> Result<(), Error> {
        if self.unknown_capability {
            return Err(Error::UnknownCapability);
        }
        self.stage = Stage::Done;
        Ok(())
    }
}
fn parser_error(error: arena::Error) -> Error {
    match error.kind {
        arena::ErrorKind::Syntax => Error::NotJson,
        arena::ErrorKind::Limit(ParseLimit::Bytes) => Error::MaxSizeRequest,
        arena::ErrorKind::Limit(kind) => Error::Resource(kind),
        arena::ErrorKind::Storage => Error::Storage,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::limits::Limits;
    struct Fixture {
        bytes: Vec<u8>,
        tokens: Vec<Token>,
    }
    impl Fixture {
        fn new(input: &str) -> Self {
            Self {
                bytes: input.as_bytes().to_vec(),
                tokens: vec![Token::EMPTY; Limits::default().json_tokens],
            }
        }
        fn parse(&mut self, limits: Limits) -> Result<Request<'_>, Error> {
            Request::parse(
                &mut self.bytes,
                &mut self.tokens,
                &limits.plan().unwrap(),
                &[CORE],
            )
        }
    }
    fn error(input: &str) -> Error {
        Fixture::new(input).parse(Limits::default()).err().unwrap()
    }
    fn owned_fixture(input: &str) -> (OwnedValidation<'static>, (*const u8, *const Token)) {
        let limits = Limits::default();
        let mut bytes = vec![0xff; limits.json_bytes].into_boxed_slice();
        bytes[..input.len()].copy_from_slice(input.as_bytes());
        let tokens = vec![Token::EMPTY; limits.json_tokens].into_boxed_slice();
        let pointers = (bytes.as_ptr(), tokens.as_ptr());
        let mut parser = arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            input.len(),
            ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        (
            OwnedValidation::new(parser.finish().unwrap(), &limits.plan().unwrap(), &[CORE])
                .unwrap(),
            pointers,
        )
    }
    fn owned(input: &str) -> OwnedValidation<'static> {
        owned_fixture(input).0
    }
    #[test]
    fn scalar_and_capability_batches_end_at_their_phase_boundary() {
        let input = format!(r#"{{"using":["{CORE}"],"methodCalls":[]}}"#);
        let mut validation = owned(&input);
        assert!(!validation.step().unwrap());
        assert!(matches!(validation.envelope.stage, Stage::Fields(_)));
        assert!(!validation.step().unwrap());
        assert!(matches!(
            validation.envelope.stage,
            Stage::Capability { .. }
        ));
        assert!(!validation.step().unwrap());
        assert!(matches!(validation.envelope.stage, Stage::Using(_)));
        assert!(validation.step().unwrap());
    }

    #[test]
    fn owned_handoff_binds_advertised_limits_and_returns_refused_storage() {
        let plan = Limits::default().plan().unwrap();
        let limits = plan.limits();
        let expected = ParseLimits {
            max_bytes: limits.json_bytes,
            max_depth: limits.json_depth,
            max_tokens: limits.json_tokens,
        };
        for (parser_limits, capacity, error) in [
            (
                ParseLimits {
                    max_bytes: expected.max_bytes + 1,
                    ..expected
                },
                limits.json_tokens,
                Error::Configuration,
            ),
            (
                ParseLimits {
                    max_depth: expected.max_depth + 1,
                    ..expected
                },
                limits.json_tokens,
                Error::Configuration,
            ),
            (
                ParseLimits {
                    max_tokens: expected.max_tokens + 1,
                    ..expected
                },
                limits.json_tokens,
                Error::Configuration,
            ),
            (expected, 8, Error::Storage),
        ] {
            let bytes = b"{}".to_vec().into_boxed_slice();
            let tokens = vec![Token::EMPTY; capacity].into_boxed_slice();
            let pointers = (bytes.as_ptr(), tokens.as_ptr());
            let mut parser =
                arena::OwnedCursor::with_input_length(bytes, tokens, 2, parser_limits).unwrap();
            while !parser.step().unwrap() {}
            let refusal = OwnedValidation::new(parser.finish().unwrap(), &plan, &[CORE])
                .err()
                .unwrap();
            assert_eq!(refusal.error(), error);
            let (bytes, tokens) = refusal.into_buffers();
            assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
        }
        let document = owned("{}").document;
        let impossible = Limits {
            json_depth: 1,
            ..Limits::default()
        }
        .plan()
        .unwrap();
        assert_eq!(
            OwnedValidation::new(document, &impossible, &[CORE])
                .err()
                .unwrap()
                .error(),
            Error::Configuration
        );
    }

    #[test]
    fn owned_validation_moves_without_exposing_unchecked_calls() {
        fn send<T: Send>() {}
        send::<OwnedValidation<'static>>();
        send::<OwnedRequest>();
        let input = format!("{{\"using\":[\"{CORE}\"],\"methodCalls\":[[\"Core/echo\",{{}},\"{}\"]],\"createdIds\":{{\"k\":\"v\"}}}}", "x".repeat(4096));
        let mut queue = Some(owned(&input));
        let mut steps = 0;
        let request = loop {
            let mut validation = queue.take().unwrap();
            steps += 1;
            if validation.step().unwrap() {
                break validation.finish().unwrap();
            }
            queue = Some(validation);
        };
        assert!(steps > 16);
        let view = request.view().unwrap();
        assert!(view.uses(CORE));
        assert_eq!(view.call_count(), 1);
        assert_eq!(view.calls().next().unwrap().call_id.len(), 4096);
        assert_eq!(view.created_ids().unwrap().next().unwrap().0, "k");
        let (bytes, tokens) = request.into_buffers();
        assert_eq!(bytes.len(), Limits::default().json_bytes);
        assert_eq!(tokens.len(), Limits::default().json_tokens);
    }
    #[test]
    fn owned_failure_and_premature_finish_preserve_arenas() {
        let input = format!(
            r#"{{"using":[],"methodCalls":[{}[false,{{}},"bad"]]}}"#,
            r#"["Core/echo",{},"ok"],"#.repeat(15)
        );
        let (mut validation, pointers) = owned_fixture(&input);
        while !matches!(validation.envelope.stage, Stage::Calls(_)) {
            assert!(!validation.step().unwrap());
        }
        let refusal = validation.finish().err().unwrap();
        assert_eq!(refusal.error(), Error::Storage);
        let (bytes, tokens) = refusal.into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
        assert_eq!(bytes.len(), Limits::default().json_bytes);
        assert_eq!(tokens.len(), Limits::default().json_tokens);
        let (mut validation, pointers) = owned_fixture(&input);
        let error = loop {
            match validation.step() {
                Ok(false) => (),
                Ok(true) => panic!("bad call admitted"),
                Err(error) => break error,
            }
        };
        assert_eq!(error, Error::NotRequest);
        assert_eq!(validation.step(), Err(error));
        let refusal = validation.finish().err().unwrap();
        assert_eq!(refusal.error(), error);
        assert!(!format!("{refusal:?}").contains("Core/echo"));
        let (bytes, tokens) = refusal.into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
        assert_eq!(bytes.len(), Limits::default().json_bytes);
        assert_eq!(tokens.len(), Limits::default().json_tokens);
    }
    #[test]
    fn owned_and_borrowed_envelopes_have_the_same_error_precedence() {
        for input in [
            r#"[]"#,
            r#"{}"#,
            r#"{"using":[],"methodCalls":false}"#,
            r#"{"using":["unknown"],"methodCalls":[],"createdIds":null}"#,
            r#"{"using":["unknown"],"methodCalls":[]}"#,
            r#"{"using":[],"methodCalls":[],"createdIds":{"!":"v"}}"#,
            r#"{"using":[],"methodCalls":[],"ignored":"\uffff"}"#,
            r#"{"using":[false],"methodCalls":[]}"#,
            r#"{"using":["unknown"],"methodCalls":[[false,{},"id"]]}"#,
            r#"{"using":[],"methodCalls":[],"createdIds":{"":"v"}}"#,
            r#"{"using":[],"methodCalls":[],"createdIds":{"k":false}}"#,
        ] {
            let expected = error(input);
            let mut validation = owned(input);
            loop {
                match validation.step() {
                    Ok(false) => (),
                    Ok(true) => panic!("invalid request admitted: {input}"),
                    Err(actual) => {
                        assert_eq!(actual, expected, "{input}");
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn owned_call_limit_and_success_match_borrowed_validation() {
        let calls = vec![r#"["Core/echo",{},"id"]"#; 17].join(",");
        let input = format!(r#"{{"using":[],"methodCalls":[{calls}]}}"#);
        assert_eq!(error(&input), Error::MaxCallsInRequest);
        let mut validation = owned(&input);
        loop {
            match validation.step() {
                Ok(false) => (),
                Ok(true) => panic!("call limit escaped"),
                Err(error) => {
                    assert_eq!(error, Error::MaxCallsInRequest);
                    break;
                }
            }
        }
        let input = format!(
            r#"{{"using":["{CORE}"],"methodCalls":[["Core/echo",{{"x":"é"}},"\u0069d"]],"createdIds":{{"seed":"value"}}}}"#
        );
        let mut fixture = Fixture::new(&input);
        let borrowed = fixture.parse(Limits::default()).unwrap();
        let mut validation = owned(&input);
        while !validation.step().unwrap() {}
        let owned = validation.finish().unwrap();
        let view = owned.view().unwrap();
        assert_eq!(borrowed.call_count(), view.call_count());
        assert_eq!(borrowed.uses(CORE), view.uses(CORE));
        let left = borrowed.calls().next().unwrap();
        let right = view.calls().next().unwrap();
        assert_eq!(left.name, right.name);
        assert_eq!(left.call_id, right.call_id);
        assert_eq!(
            left.argument("x").unwrap().as_string_bytes(),
            right.argument("x").unwrap().as_string_bytes()
        );
        assert_eq!(
            borrowed.created_ids().unwrap().next().unwrap().0,
            view.created_ids().unwrap().next().unwrap().0
        );
        let (bytes, tokens) = owned.into_buffers();
        assert_eq!(bytes.len(), Limits::default().json_bytes);
        assert_eq!(tokens.len(), Limits::default().json_tokens);
    }

    #[test]
    fn rejects_short_backing_before_touching_input() {
        let original = br#"{"using":[],"methodCalls":[]}"#;
        let mut input = original.to_vec();
        let mut tokens = [Token::EMPTY; 16];
        assert_eq!(
            Request::parse(
                &mut input,
                &mut tokens,
                &Limits::default().plan().unwrap(),
                &[CORE]
            )
            .err(),
            Some(Error::Storage)
        );
        assert_eq!(input, original);
    }
    fn temporary_argument<'a>(request: &Request<'a>) -> Option<Value<'a>> {
        request.calls().next()?.argument("x")
    }
    #[test]
    fn validates_whole_envelope_before_exposing_calls() {
        for input in [
            "null",
            "[]",
            "{}",
            r#"{"using":[],"methodCalls":null}"#,
            r#"{"using":[3],"methodCalls":[]}"#,
            r#"{"using":[],"methodCalls":[["Core/echo",{},"a"],["x",null,"b"]]}"#,
            r#"{"using":[],"methodCalls":[["x",{},"a",0]]}"#,
            r#"{"using":[],"methodCalls":[["x",{},1]]}"#,
            r#"{"using":[],"methodCalls":[],"createdIds":null}"#,
        ] {
            assert_eq!(error(input), Error::NotRequest, "{input}");
        }
    }
    #[test]
    fn preserves_calls_and_optional_creation_map_without_client_subset_rules() {
        let mut fixture = Fixture::new(
            r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{"x":1},"/same ☃"],["Future/method",{},"/same ☃"]],"createdIds":{"A-_":"9"},"future":[true]}"#,
        );
        let request = fixture.parse(Limits::default()).unwrap();
        assert!(request.uses(CORE));
        assert_eq!(
            temporary_argument(&request).and_then(Value::as_number),
            Some("1")
        );
        let calls: Vec<_> = request.calls().collect();
        assert_eq!(request.call_count(), 2);
        assert_eq!(calls[0].call_id, calls[1].call_id);
        assert_eq!(calls[1].name, b"Future/method");
        assert_eq!(calls[0].argument("x").and_then(Value::as_number), Some("1"));
        assert!(calls[1].argument("x").is_none());
        assert_eq!(request.created_ids().unwrap().len(), 1);
        assert!(Fixture::new(r#"{"using":[],"methodCalls":[]}"#)
            .parse(Limits::default())
            .unwrap()
            .created_ids()
            .is_none());
        assert_eq!(
            Fixture::new(r#"{"using":[],"methodCalls":[],"createdIds":{}}"#)
                .parse(Limits::default())
                .unwrap()
                .created_ids()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            error(r#"{"using":["unknown"],"methodCalls":[]}"#),
            Error::UnknownCapability
        );
    }
    #[test]
    fn rejects_noncharacters_even_in_unknown_properties_and_escaped_keys() {
        for suffix in [
            r#""\ufdd0""#,
            r#"{"\uffff":1}"#,
            r#"["\udbff\udfff"]"#,
            "\"\u{1fffe}\"",
        ] {
            assert_eq!(
                error(&format!(
                    r#"{{"using":[],"methodCalls":[],"future":{suffix}}}"#
                )),
                Error::NotJson
            );
        }
        for input in [
            r#"{"using":[],"methodCalls":[],"using":[]}"#,
            r#"{"x":"\ud800"}"#,
        ] {
            assert_eq!(error(input), Error::NotJson);
        }
    }
    #[test]
    fn creation_ids_use_protocol_grammar_not_local_hex_ids() {
        for invalid in ["", "a/b", "a=b", "é", &"a".repeat(256)] {
            let encoded = td_json::Json::Str(invalid.into()).to_string();
            for pair in [format!("{encoded}:\"ok\""), format!("\"ok\":{encoded}")] {
                assert_eq!(
                    error(&format!(
                        r#"{{"using":[],"methodCalls":[],"createdIds":{{{pair}}}}}"#
                    )),
                    Error::NotRequest
                );
            }
        }
        let key = "k".repeat(255);
        let value = "v".repeat(255);
        let text = format!(r#"{{"using":[],"methodCalls":[],"createdIds":{{"{key}":"{value}"}}}}"#);
        assert!(Fixture::new(&text).parse(Limits::default()).is_ok());
    }
    #[test]
    fn rejects_limits_that_cannot_admit_basic_calls_before_advertisement() {
        for limits in [
            Limits {
                json_bytes: 10,
                ..Limits::default()
            },
            Limits {
                json_depth: 3,
                ..Limits::default()
            },
            Limits {
                json_tokens: 32,
                ..Limits::default()
            },
        ] {
            assert_eq!(
                Request::validate_resources(&limits.plan().unwrap()),
                Err(Error::Configuration)
            );
        }
        assert!(Request::validate_resources(&Limits::default().plan().unwrap()).is_ok());
    }
    #[test]
    fn distinguishes_advertised_limits_from_parser_resource_ceiling() {
        let body = r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{"v":[0]},"a"],["Core/echo",{},"b"]]}"#;
        let mut limits = Limits {
            json_bytes: body.len(),
            json_methods: 2,
            json_depth: 5,
            json_tokens: 17,
            ..Limits::default()
        };
        assert!(Fixture::new(body).parse(limits).is_ok());
        limits.json_bytes -= 1;
        assert_eq!(
            Fixture::new(body).parse(limits).err(),
            Some(Error::MaxSizeRequest)
        );
        limits.json_bytes += 1;
        limits.json_methods = 1;
        assert_eq!(
            Fixture::new(body).parse(limits).err(),
            Some(Error::MaxCallsInRequest)
        );
        limits.json_methods = 2;
        limits.json_depth = 4;
        assert_eq!(
            Fixture::new(body).parse(limits).err(),
            Some(Error::Resource(ParseLimit::Depth))
        );
        limits.json_depth = 5;
        limits.json_tokens = 16;
        assert_eq!(
            Fixture::new(body).parse(limits).err(),
            Some(Error::Resource(ParseLimit::Tokens))
        );
    }
    #[test]
    fn admission_yields_through_unknown_strings_and_long_opaque_ids() {
        let opaque = "é".repeat(30000);
        let text = format!(
            r#"{{"using":["{CORE}"],"methodCalls":[["Future/method",{{}},"{opaque}"]],"future":"{opaque}"}}"#
        );
        let mut fixture = Fixture::new(&text);
        let plan = Limits::default().plan().unwrap();
        let mut admission =
            Validation::new(&mut fixture.bytes, &mut fixture.tokens, &plan, &[CORE]).unwrap();
        let mut turns = 0;
        while !admission.step().unwrap() {
            turns += 1;
            assert!(turns < text.len() * 2);
        }
        assert!(turns > 200);
        let request = admission.finish().unwrap();
        assert_eq!(request.calls().next().unwrap().call_id, opaque.as_bytes());
        assert!(std::mem::size_of::<Validation<'_, '_>>() <= 8192);
    }
    #[test]
    fn admission_checks_long_capabilities_incrementally_and_latches_errors() {
        let capability = "x".repeat(10000);
        let other = format!("{}y", "x".repeat(9999));
        let capabilities = [other.as_str(), capability.as_str()];
        let text = format!(r#"{{"using":["{capability}"],"methodCalls":[]}}"#);
        let mut fixture = Fixture::new(&text);
        let plan = Limits::default().plan().unwrap();
        let mut admission = Validation::new(
            &mut fixture.bytes,
            &mut fixture.tokens,
            &plan,
            &capabilities,
        )
        .unwrap();
        let mut comparison_turns = 0;
        while !admission.step().unwrap() {
            if matches!(admission.envelope.stage, Stage::Capability { .. }) {
                comparison_turns += 1;
            }
        }
        assert!(comparison_turns > 70);
        assert!(admission.finish().unwrap().uses(&capability));
        let mut fixture =
            Fixture::new(r#"{"using":[],"methodCalls":[["x",{},"ok"],["x",false,"late"]]}"#);
        let mut admission =
            Validation::new(&mut fixture.bytes, &mut fixture.tokens, &plan, &[CORE]).unwrap();
        let error = loop {
            match admission.step() {
                Err(error) => break error,
                Ok(false) => (),
                Ok(true) => panic!("invalid late call accepted"),
            }
        };
        assert_eq!(error, Error::NotRequest);
        assert_eq!(admission.step(), Err(error));
        assert_eq!(admission.finish().err(), Some(error));
    }
    #[test]
    fn unfinished_admission_cannot_expose_an_invocation() {
        for turns in 0..20 {
            let text = format!(
                r#"{{"using":[],"methodCalls":[["Core/echo",{{}},"{}"]]}}"#,
                "x".repeat(10000)
            );
            let mut fixture = Fixture::new(&text);
            let plan = Limits::default().plan().unwrap();
            let mut admission =
                Validation::new(&mut fixture.bytes, &mut fixture.tokens, &plan, &[CORE]).unwrap();
            for _ in 0..turns {
                assert!(!admission.step().unwrap());
            }
            assert_eq!(admission.finish().err(), Some(Error::Storage));
        }
    }
    #[test]
    fn parsed_fields_do_not_publish_unchecked_later_calls() {
        let mut calls = vec![r#"["Core/echo",{},"ok"]"#; 63];
        calls.push(r#"["bad",false,"late"]"#);
        let text = format!(r#"{{"using":[],"methodCalls":[{}]}}"#, calls.join(","));
        let mut fixture = Fixture::new(&text);
        let plan = Limits {
            json_methods: 64,
            ..Limits::default()
        }
        .plan()
        .unwrap();
        let mut validation =
            Validation::new(&mut fixture.bytes, &mut fixture.tokens, &plan, &[CORE]).unwrap();
        while !matches!(validation.envelope.stage, Stage::Calls(_)) {
            assert!(!validation.step().unwrap());
        }
        assert!(validation.envelope.using.is_some());
        assert!(validation.envelope.calls.is_some());
        assert_eq!(validation.finish().err(), Some(Error::Storage));
    }
    #[test]
    fn envelope_error_precedence_is_independent_of_field_order() {
        let calls = vec![r#"["x",{},""]"#; 17].join(",");
        for text in [
            format!(r#"{{"methodCalls":[{calls}]}}"#),
            format!(r#"{{"using":[3],"methodCalls":[{calls}]}}"#),
        ] {
            assert_eq!(error(&text), Error::NotRequest);
        }
        for text in [
            format!(r#"{{"createdIds":null,"methodCalls":[{calls}],"using":[]}}"#),
            format!(r#"{{"using":[],"methodCalls":[{calls}],"createdIds":null}}"#),
        ] {
            assert_eq!(error(&text), Error::MaxCallsInRequest);
        }
    }
}
