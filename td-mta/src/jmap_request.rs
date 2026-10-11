//! Whole-request admission before JMAP method execution. No network activation.
use crate::limits::ResourcePlan;
use std::fmt;
use td_json::arena::{self, Object, ParseLimit, ParseLimits, Token, Value};

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
    maximum_calls: usize,
    stage: Stage<'a>,
    raw_using: Option<Value<'a>>,
    raw_calls: Option<Value<'a>>,
    raw_created: Option<Value<'a>>,
    using: Option<arena::Array<'a>>,
    calls: Option<arena::Array<'a>>,
    created: Option<Object<'a>>,
    unknown_capability: bool,
    failed: Option<Error>,
}
#[derive(Clone, Copy)]
enum Stage<'a> {
    Parsing,
    Strings {
        token: usize,
        offset: usize,
    },
    Fields(Object<'a>),
    Using(arena::Array<'a>),
    Capability {
        rest: arena::Array<'a>,
        name: &'a [u8],
        index: usize,
        offset: usize,
    },
    Calls(arena::Array<'a>),
    Created(Object<'a>),
    CreatedKey {
        rest: Object<'a>,
        value: Value<'a>,
        key: &'a [u8],
        offset: usize,
    },
    CreatedValue {
        rest: Object<'a>,
        bytes: &'a [u8],
        offset: usize,
    },
    Done,
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
            maximum_calls: limits.json_methods,
            stage: Stage::Parsing,
            raw_using: None,
            raw_calls: None,
            raw_created: None,
            using: None,
            calls: None,
            created: None,
            unknown_capability: false,
            failed: None,
        })
    }
    /// One parser step, up to 256 scalar/capability transitions, or at most
    /// sixteen structural transitions. Creation IDs use eight-byte chunks.
    /// Failed/completed polls are inert; enforce live admission in the owner.
    pub fn step(&mut self) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        // End scalar/comparison batches at their phase boundary so a cheap
        // byte budget cannot become hundreds of structural operations.
        let scalar_pass = matches!(self.stage, Stage::Strings { .. });
        let capability_pass = matches!(self.stage, Stage::Capability { .. });
        let turns = if matches!(self.stage, Stage::Parsing) {
            1
        } else if scalar_pass || capability_pass {
            256
        } else {
            16
        };
        for _ in 0..turns {
            if let Err(error) = self.transition() {
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
    pub fn finish(self) -> Result<Request<'a>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if !matches!(self.stage, Stage::Done) {
            return Err(Error::Storage);
        }
        Ok(Request {
            using: self.using.ok_or(Error::Storage)?,
            calls: self.calls.ok_or(Error::Storage)?,
            created: self.created,
        })
    }
    fn transition(&mut self) -> Result<(), Error> {
        match self.stage {
            Stage::Parsing => {
                if self
                    .parser
                    .as_mut()
                    .ok_or(Error::Storage)?
                    .step()
                    .map_err(parser_error)?
                {
                    self.document = Some(
                        self.parser
                            .take()
                            .ok_or(Error::Storage)?
                            .finish()
                            .map_err(parser_error)?,
                    );
                    self.stage = Stage::Strings {
                        token: 0,
                        offset: 0,
                    };
                }
            }
            Stage::Strings { token, offset } => {
                let document = self.document.as_ref().ok_or(Error::Storage)?;
                if token == document.token_count() {
                    self.stage =
                        Stage::Fields(document.root().as_object().ok_or(Error::NotRequest)?);
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
                if let Some((key, value)) = fields.next_bytes() {
                    match key {
                        b"using" => self.raw_using = Some(value),
                        b"methodCalls" => self.raw_calls = Some(value),
                        b"createdIds" => self.raw_created = Some(value),
                        _ => (),
                    }
                    self.stage = Stage::Fields(fields);
                } else {
                    let using = self
                        .raw_using
                        .and_then(Value::as_array)
                        .ok_or(Error::NotRequest)?;
                    self.using = Some(using);
                    self.stage = Stage::Using(using);
                }
            }
            Stage::Using(mut using) => {
                self.stage = if let Some(value) = using.next() {
                    Stage::Capability {
                        rest: using,
                        name: value.as_string_bytes().ok_or(Error::NotRequest)?,
                        index: 0,
                        offset: 0,
                    }
                } else {
                    let calls = self
                        .raw_calls
                        .and_then(Value::as_array)
                        .ok_or(Error::NotRequest)?;
                    if calls.len() > self.maximum_calls {
                        return Err(Error::MaxCallsInRequest);
                    }
                    self.calls = Some(calls);
                    Stage::Calls(calls)
                };
            }
            Stage::Capability {
                rest,
                name,
                index,
                offset,
            } => {
                if let Some(capability) = self.capabilities.get(index) {
                    if name.len() != capability.len() {
                        self.stage = Stage::Capability {
                            rest,
                            name,
                            index: index + 1,
                            offset: 0,
                        };
                    } else if offset == name.len() {
                        self.stage = Stage::Using(rest);
                    } else if name.get(offset) == capability.as_bytes().get(offset) {
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
                if let Some(value) = calls.next() {
                    invocation(value).ok_or(Error::NotRequest)?;
                    self.stage = Stage::Calls(calls);
                } else if let Some(value) = self.raw_created {
                    let created = value.as_object().ok_or(Error::NotRequest)?;
                    self.created = Some(created);
                    self.stage = Stage::Created(created);
                } else {
                    self.complete()?;
                }
            }
            Stage::Created(mut created) => {
                if let Some((key, value)) = created.next_bytes() {
                    if !(1..=255).contains(&key.len()) {
                        return Err(Error::NotRequest);
                    }
                    self.stage = Stage::CreatedKey {
                        rest: created,
                        value,
                        key,
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
                let end = offset.saturating_add(8).min(key.len());
                if !id_chunk(key.get(offset..end).ok_or(Error::Storage)?) {
                    return Err(Error::NotRequest);
                }
                if end == key.len() {
                    let bytes = value.as_string_bytes().ok_or(Error::NotRequest)?;
                    if !(1..=255).contains(&bytes.len()) {
                        return Err(Error::NotRequest);
                    }
                    self.stage = Stage::CreatedValue {
                        rest,
                        bytes,
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
                bytes,
                offset,
            } => {
                let end = offset.saturating_add(8).min(bytes.len());
                if !id_chunk(bytes.get(offset..end).ok_or(Error::Storage)?) {
                    return Err(Error::NotRequest);
                }
                self.stage = if end == bytes.len() {
                    Stage::Created(rest)
                } else {
                    Stage::CreatedValue {
                        rest,
                        bytes,
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
            if matches!(admission.stage, Stage::Capability { .. }) {
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
        while !matches!(validation.stage, Stage::Calls(_)) {
            assert!(!validation.step().unwrap());
        }
        assert!(validation.using.is_some());
        assert!(validation.calls.is_some());
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
