//! The bounded, non-eavesdropping subset of D-Bus match rules.
//!
//! Match rules select broadcasts; a directed message never consults them.
//! Parsing happens at `AddMatch`, so routing compares a compact value and
//! cannot be made to repeatedly parse attacker-controlled text. The parser's
//! quote handling follows the D-Bus grammar rather than shell quoting: a
//! backslash is literal inside single quotes, while `\'` outside quotes is the
//! spelling of one apostrophe.

use std::fmt;

use crate::message::{Message, MessageType};
use crate::name;

pub const MAX_RULES_PER_CONNECTION: usize = 256;
pub const MAX_RULE_TEXT_PER_CONNECTION: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rule {
    kind: Option<MessageType>,
    sender: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    path: Option<String>,
    path_namespace: Option<String>,
    destination: Option<String>,
    args: Vec<Arg>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Arg {
    index: usize,
    kind: ArgKind,
    value: String,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ArgKind {
    Exact,
    Path,
    Namespace,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuleError {
    Empty,
    TooLong,
    MissingEquals,
    EmptyKey,
    UnterminatedQuote,
    TrailingComma,
    UnknownKey(String),
    DuplicateKey(String),
    BadValue(String),
    Eavesdropping,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("a match rule is empty"),
            Self::TooLong => f.write_str("a match rule exceeds the text ceiling"),
            Self::MissingEquals => f.write_str("a match-rule term has no '='"),
            Self::EmptyKey => f.write_str("a match-rule term has an empty key"),
            Self::UnterminatedQuote => f.write_str("a match rule has an unterminated quote"),
            Self::TrailingComma => f.write_str("a match rule ends with an empty term"),
            Self::UnknownKey(key) => write!(f, "unknown match-rule key {key:?}"),
            Self::DuplicateKey(key) => write!(f, "match-rule key {key:?} appears twice"),
            Self::BadValue(key) => write!(f, "invalid value for match-rule key {key:?}"),
            Self::Eavesdropping => f.write_str("eavesdropping match rules are not supported"),
        }
    }
}

impl Rule {
    pub fn parse(text: &str) -> Result<Self, RuleError> {
        if text.trim().is_empty() {
            return Err(RuleError::Empty);
        }
        if text.len() > MAX_RULE_TEXT_PER_CONNECTION {
            return Err(RuleError::TooLong);
        }
        let terms = terms(text)?;
        let mut rule = Self {
            kind: None,
            sender: None,
            interface: None,
            member: None,
            path: None,
            path_namespace: None,
            destination: None,
            args: Vec::new(),
        };
        let mut seen = Vec::<String>::new();
        for (key, value) in terms {
            if seen.iter().any(|prior| prior == &key) {
                return Err(RuleError::DuplicateKey(key));
            }
            seen.push(key.clone());
            match key.as_str() {
                "type" => {
                    rule.kind = Some(match value.as_str() {
                        "method_call" => MessageType::MethodCall,
                        "method_return" => MessageType::MethodReturn,
                        "error" => MessageType::Error,
                        "signal" => MessageType::Signal,
                        _ => return Err(RuleError::BadValue(key)),
                    });
                }
                "sender" if name::valid_bus_name(&value) => rule.sender = Some(value),
                "interface" if name::valid_interface_name(&value) => {
                    rule.interface = Some(value);
                }
                "member" if name::valid_member_name(&value) => rule.member = Some(value),
                "path" if name::valid_object_path(&value) => rule.path = Some(value),
                "path_namespace" if name::valid_object_path(&value) => {
                    rule.path_namespace = Some(value);
                }
                "destination" if name::valid_unique_name(&value) => {
                    rule.destination = Some(value);
                }
                "eavesdrop" if value == "true" => return Err(RuleError::Eavesdropping),
                "eavesdrop" if value == "false" => {}
                _ => {
                    if let Some((index, kind)) = argument_key(&key) {
                        if kind == ArgKind::Namespace && !valid_name_namespace(&value) {
                            return Err(RuleError::BadValue(key));
                        }
                        rule.args.push(Arg { index, kind, value });
                    } else if is_known_key(&key) {
                        return Err(RuleError::BadValue(key));
                    } else {
                        return Err(RuleError::UnknownKey(key));
                    }
                }
            }
        }
        if rule.path.is_some() && rule.path_namespace.is_some() {
            return Err(RuleError::BadValue("path_namespace".into()));
        }
        rule.args.sort_by_key(|arg| (arg.index, arg.kind));
        Ok(rule)
    }

    /// Whether one broadcast satisfies this rule.
    ///
    /// `sender_owns` resolves a well-known sender against the SAME directory
    /// snapshot that selected subscribers. A client-supplied SENDER never
    /// reaches here; `actual_sender` is the unique name the broker stamped.
    pub fn matches<F>(&self, message: &Message<'_>, actual_sender: &str, mut sender_owns: F) -> bool
    where
        F: FnMut(&str) -> bool,
    {
        if self.kind.is_some_and(|kind| kind != message.kind)
            || self
                .sender
                .as_deref()
                .is_some_and(|sender| sender != actual_sender && !sender_owns(sender))
            || self
                .interface
                .as_deref()
                .is_some_and(|interface| message.fields.interface != Some(interface))
            || self
                .member
                .as_deref()
                .is_some_and(|member| message.fields.member != Some(member))
            || self
                .path
                .as_deref()
                .is_some_and(|path| message.fields.path != Some(path))
            || self.path_namespace.as_deref().is_some_and(|namespace| {
                !message
                    .fields
                    .path
                    .is_some_and(|path| in_path_namespace(path, namespace))
            })
            || self
                .destination
                .as_deref()
                .is_some_and(|destination| message.fields.destination != Some(destination))
        {
            return false;
        }
        self.args.iter().all(|wanted| {
            let Some(argument) = message.args().get(wanted.index) else {
                return false;
            };
            match wanted.kind {
                ArgKind::Exact => {
                    matches!(argument, crate::wire::Value::Str(actual) if *actual == wanted.value)
                }
                ArgKind::Path => match argument {
                    crate::wire::Value::Str(actual)
                    | crate::wire::Value::ObjectPath(actual) => arg_path_matches(actual, &wanted.value),
                    _ => false,
                },
                ArgKind::Namespace => {
                    matches!(argument, crate::wire::Value::Str(actual) if in_name_namespace(actual, &wanted.value))
                }
            }
        })
    }
}

fn is_known_key(key: &str) -> bool {
    matches!(
        key,
        "sender" | "interface" | "member" | "path" | "path_namespace" | "destination" | "eavesdrop"
    )
}

fn argument_key(key: &str) -> Option<(usize, ArgKind)> {
    let rest = key.strip_prefix("arg")?;
    let (digits, kind) = if let Some(digits) = rest.strip_suffix("path") {
        (digits, ArgKind::Path)
    } else if let Some(digits) = rest.strip_suffix("namespace") {
        (digits, ArgKind::Namespace)
    } else {
        (rest, ArgKind::Exact)
    };
    if digits.is_empty()
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
        || (digits.len() > 1 && digits.starts_with('0'))
    {
        return None;
    }
    let index = digits.parse::<usize>().ok()?;
    if index > 63 || (kind == ArgKind::Namespace && index != 0) {
        return None;
    }
    Some((index, kind))
}

fn in_path_namespace(path: &str, namespace: &str) -> bool {
    if namespace == "/" {
        return path.starts_with('/');
    }
    path == namespace
        || path
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn in_name_namespace(name: &str, namespace: &str) -> bool {
    name == namespace
        || name
            .strip_prefix(namespace)
            .is_some_and(|rest| rest.starts_with('.'))
}

fn arg_path_matches(actual: &str, wanted: &str) -> bool {
    actual == wanted
        || (actual.ends_with('/') && wanted.starts_with(actual))
        || (wanted.ends_with('/') && actual.starts_with(wanted))
}

fn valid_name_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace.len() <= name::MAX_NAME_LEN
        && namespace.split('.').all(|element| {
            element.as_bytes().first().is_some_and(|first| {
                !first.is_ascii_digit()
                    && element
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
        })
}

fn terms(text: &str) -> Result<Vec<(String, String)>, RuleError> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < text.len() {
        while text
            .as_bytes()
            .get(at)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            at = at.saturating_add(1);
        }
        let key_start = at;
        while text
            .as_bytes()
            .get(at)
            .is_some_and(|byte| *byte != b'=' && *byte != b',')
        {
            at = at.saturating_add(1);
        }
        match text.as_bytes().get(at) {
            Some(b'=') => {}
            _ => return Err(RuleError::MissingEquals),
        }
        let key = text
            .get(key_start..at)
            .ok_or(RuleError::MissingEquals)?
            .trim()
            .to_string();
        if key.is_empty() {
            return Err(RuleError::EmptyKey);
        }
        at = at.saturating_add(1);
        while text
            .as_bytes()
            .get(at)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            at = at.saturating_add(1);
        }
        let mut value = Vec::<(char, bool)>::new();
        let mut quoted = false;
        loop {
            let Some(byte) = text.as_bytes().get(at).copied() else {
                if quoted {
                    return Err(RuleError::UnterminatedQuote);
                }
                break;
            };
            if byte == b'\'' {
                quoted = !quoted;
                at = at.saturating_add(1);
                continue;
            }
            if !quoted && byte == b',' {
                break;
            }
            if !quoted && byte == b'\\' && text.as_bytes().get(at.saturating_add(1)) == Some(&b'\'')
            {
                value.push(('\'', false));
                at = at.saturating_add(2);
                continue;
            }
            let rest = text.get(at..).ok_or(RuleError::UnterminatedQuote)?;
            let Some(character) = rest.chars().next() else {
                return Err(RuleError::UnterminatedQuote);
            };
            value.push((character, quoted));
            at = at.saturating_add(character.len_utf8());
        }
        let leading = value
            .iter()
            .take_while(|(character, was_quoted)| !was_quoted && character.is_whitespace())
            .count();
        let trailing = value
            .iter()
            .rev()
            .take_while(|(character, was_quoted)| !was_quoted && character.is_whitespace())
            .count();
        let keep = value.len().saturating_sub(leading).saturating_sub(trailing);
        out.push((
            key,
            value
                .into_iter()
                .skip(leading)
                .take(keep)
                .map(|(character, _)| character)
                .collect(),
        ));
        if at == text.len() {
            break;
        }
        at = at.saturating_add(1);
        if at == text.len() {
            return Err(RuleError::TrailingComma);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message;
    use crate::wire::Endian;

    fn signal<'a>(bytes: &'a [u8]) -> message::Message<'a> {
        message::decode(bytes, 0).expect("decode the test signal").0
    }

    #[test]
    fn the_dbus_quote_grammar_is_not_shell_quoting() {
        let parsed =
            Rule::parse("type='signal',arg0='a\\b',arg1=a\\'b,arg2='x,y'").expect("parse the rule");
        assert_eq!(parsed.args[0].value, "a\\b");
        assert_eq!(parsed.args[1].value, "a'b");
        assert_eq!(parsed.args[2].value, "x,y");
        assert_eq!(
            Rule::parse("type='signal"),
            Err(RuleError::UnterminatedQuote)
        );
        assert_eq!(Rule::parse("type='signal',"), Err(RuleError::TrailingComma));
    }

    #[test]
    fn duplicate_unknown_and_eavesdropping_terms_are_refused() {
        assert!(matches!(
            Rule::parse("type='signal',type='signal'"),
            Err(RuleError::DuplicateKey(_))
        ));
        assert!(matches!(
            Rule::parse("wat='signal'"),
            Err(RuleError::UnknownKey(_))
        ));
        assert_eq!(
            Rule::parse("eavesdrop='true'"),
            Err(RuleError::Eavesdropping)
        );
        assert!(Rule::parse("eavesdrop='false'").is_ok());
        let oversized = "x".repeat(MAX_RULE_TEXT_PER_CONNECTION.saturating_add(1));
        assert_eq!(Rule::parse(&oversized), Err(RuleError::TooLong));
    }

    #[test]
    fn header_argument_and_namespace_terms_all_select() {
        let frame = message::Builder::signal(
            Endian::Little,
            "/org/example/Child",
            "org.example.Thing",
            "Changed",
        )
        .sender(":1.7")
        .serial(9)
        .body("ss", |writer| {
            writer.string("org.example.Child")?;
            writer.string("second")
        })
        .expect("build body")
        .encode()
        .expect("encode signal");
        let decoded = signal(&frame);
        let rule = Rule::parse(
            "type='signal',sender='org.example.Service',interface='org.example.Thing',\
             member='Changed',path_namespace='/org/example',arg0namespace='org.example'",
        )
        .expect("parse the rule");
        assert!(rule.matches(&decoded, ":1.7", |name| name == "org.example.Service"));
        assert!(!rule.matches(&decoded, ":1.8", |_| false));
    }

    #[test]
    fn argument_indexes_are_bounded() {
        assert!(Rule::parse("arg63='yes'").is_ok());
        assert!(Rule::parse("arg63path='/yes/'").is_ok());
        assert!(matches!(
            Rule::parse("arg1namespace='org.example'"),
            Err(RuleError::UnknownKey(_))
        ));
        assert!(matches!(
            Rule::parse("arg64='no'"),
            Err(RuleError::UnknownKey(_))
        ));
        for alias in ["arg+1='no'", "arg00='no'"] {
            assert!(
                matches!(Rule::parse(alias), Err(RuleError::UnknownKey(_))),
                "accepted noncanonical key {alias}"
            );
        }
    }

    #[test]
    fn quoted_space_and_argument_types_keep_their_specified_meaning() {
        let rule = Rule::parse("arg0=' space ',arg1path='/aa/bb/'").expect("parse");
        assert_eq!(rule.args[0].value, " space ");

        let frame = message::Builder::signal(Endian::Little, "/p", "a.b", "Changed")
            .sender(":1.7")
            .serial(9)
            .body("so", |writer| {
                writer.string(" space ")?;
                writer.object_path("/aa/bb/child")
            })
            .expect("body")
            .encode()
            .expect("encode");
        assert!(rule.matches(&signal(&frame), ":1.7", |_| false));
        let exact_path = Rule::parse("arg1='/aa/bb/child'").expect("parse");
        assert!(
            !exact_path.matches(&signal(&frame), ":1.7", |_| false),
            "an exact arg match accepts STRING only"
        );
    }
}
