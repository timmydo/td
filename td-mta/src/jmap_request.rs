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
            Self::Storage => "request arena backing invalid or exhausted",
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
#[derive(Clone, Copy)]
pub struct Invocation<'a> {
    pub name: &'a str,
    pub arguments: Value<'a>,
    pub call_id: &'a str,
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
        Self::validate_resources(resources)?;
        let limits = resources.limits();
        if tokens.len() < limits.json_tokens {
            return Err(Error::Storage);
        }
        let value = arena::parse(
            input,
            tokens,
            ParseLimits {
                max_bytes: limits.json_bytes,
                max_depth: limits.json_depth,
                max_tokens: limits.json_tokens,
            },
        )
        .map_err(|error| match error.kind {
            arena::ErrorKind::Syntax => Error::NotJson,
            arena::ErrorKind::Limit(ParseLimit::Bytes) => Error::MaxSizeRequest,
            arena::ErrorKind::Limit(kind) => Error::Resource(kind),
            arena::ErrorKind::Storage => Error::Storage,
        })?;
        if value
            .strings()
            .any(|text| text.chars().any(crate::unicode::is_noncharacter))
        {
            return Err(Error::NotJson);
        }
        let object = value.root();
        if object.as_object().is_none() {
            return Err(Error::NotRequest);
        }
        let using = object
            .get("using")
            .and_then(Value::as_array)
            .ok_or(Error::NotRequest)?;
        if using.into_iter().any(|value| value.as_str().is_none()) {
            return Err(Error::NotRequest);
        }
        let calls = object
            .get("methodCalls")
            .and_then(Value::as_array)
            .ok_or(Error::NotRequest)?;
        if calls.len() > limits.json_methods {
            return Err(Error::MaxCallsInRequest);
        }
        if calls.into_iter().any(|value| invocation(value).is_none()) {
            return Err(Error::NotRequest);
        }
        let created = object
            .get("createdIds")
            .map(|value| value.as_object().ok_or(Error::NotRequest))
            .transpose()?;
        if let Some(mut created) = created {
            if created.any(|(key, value)| !id(key) || !value.as_str().is_some_and(id)) {
                return Err(Error::NotRequest);
            }
        }
        if using.into_iter().any(|value| {
            !value
                .as_str()
                .is_some_and(|name| capabilities.contains(&name))
        }) {
            return Err(Error::UnknownCapability);
        }
        Ok(Self {
            using,
            calls,
            created,
        })
    }
    pub fn uses(&self, capability: &str) -> bool {
        self.using
            .into_iter()
            .any(|value| value.as_str() == Some(capability))
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
    let name = values.next()?.as_str()?;
    let arguments = values.next()?;
    arguments.as_object()?;
    let call_id = values.next()?.as_str()?;
    Some(Invocation {
        name,
        arguments,
        call_id,
    })
}
fn id(value: &str) -> bool {
    (1..=255).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
        assert_eq!(calls[1].name, "Future/method");
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
        assert!(id(&"a".repeat(255)));
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
}
