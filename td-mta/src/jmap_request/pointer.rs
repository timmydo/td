//! Streaming RFC 8620 result-reference Pointer selection over retained JSON.
//! Selected byte ranges remain provisional until the whole walk completes.
use std::fmt;
use td_json::scan::{self, Event, Kind, Scanner};

const PATH_BYTES_PER_TURN: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidResultReference,
    Source(scan::Error),
    State,
}
impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidResultReference => out.write_str("invalidResultReference"),
            Self::Source(error) => error.fmt(out),
            Self::State => out.write_str("invalid retained Pointer state"),
        }
    }
}
impl std::error::Error for Error {}
/// A passive range or generated array delimiter, never a file-read capability.
/// Wildcard results stream as one array of selected values, flattening each
/// selected leaf array exactly one level and preserving all element order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Piece {
    ArrayStart,
    Value { offset: u64, length: u64 },
    ArrayEnd,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Poll until complete even when consumed is zero: initial path validation
/// takes bounded turns, and ArrayEnd precedes the final complete turn.
pub struct Progress {
    pub consumed: usize,
    pub piece: Option<Piece>,
    pub complete: bool,
}
#[derive(Clone, Copy)]
struct Segment {
    next: Option<usize>,
    index: u64,
    wildcard: bool,
}
#[derive(Clone, Copy)]
enum Rule {
    Ignore,
    Flatten,
    Object {
        start: usize,
        position: usize,
        matches: bool,
        found: bool,
    },
    Array {
        segment: Segment,
        ordinal: u64,
        found: bool,
    },
}
#[derive(Clone, Copy)]
struct Frame {
    kind: Kind,
    rule: Rule,
}
const EMPTY: Frame = Frame {
    kind: Kind::Array,
    rule: Rule::Ignore,
};
#[derive(Clone, Copy)]
enum Select {
    Ignore,
    Capture,
    Path(Option<usize>),
}
#[derive(Clone, Copy)]
struct Capture {
    kind: Kind,
    depth: usize,
    start: u64,
}
/// Feed the exact immutable retained arguments stream in order, honoring
/// consumed. Source offsets are relative to that stream; the enclosing owner
/// must check addition of its file base, authorize reads and charge work/time.
/// The source producer proves unique object keys and I-JSON scalar policy.
/// Duplicate detection here covers only a matched target, not all object keys.
/// Each turn validates at most 256 path bytes or polls the shared Scanner
/// once and handles its one event. Object keys compare lazily; array tokens
/// inspect at most 21 bytes. The first detected error wins: producer integrity
/// is a prerequisite, and a bad reference may stop before a corrupt tail.
/// Paths are borrowed, never copied or limited to the source's nesting depth:
/// an empty wildcard can resolve even with arbitrarily many remaining tokens.
/// All output is provisional; a later missing wildcard branch invalidates it.
/// Complete success, not an early Piece, is permission to use the resolution.
pub struct Walker<'p> {
    path: &'p str,
    validate: Option<usize>,
    escaped: bool,
    scanner: Scanner,
    frames: [Frame; scan::MAX_DEPTH],
    depth: usize,
    started: bool,
    capture: Option<Capture>,
    array: bool,
    array_closed: bool,
    complete: bool,
    failure: Option<Error>,
}
impl fmt::Debug for Walker<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Walker { .. }")
    }
}
impl<'p> Walker<'p> {
    pub fn new(path: &'p str, byte_limit: u64, depth_limit: usize) -> Result<Self, Error> {
        if !path.is_empty() && !path.starts_with('/') {
            return Err(Error::InvalidResultReference);
        }
        Ok(Self {
            path,
            validate: Some(0),
            escaped: false,
            scanner: Scanner::new(byte_limit, depth_limit).map_err(Error::Source)?,
            frames: [EMPTY; scan::MAX_DEPTH],
            depth: 0,
            started: false,
            capture: None,
            array: false,
            array_closed: false,
            complete: false,
            failure: None,
        })
    }
    pub fn poll(&mut self, input: &[u8], eof: bool) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.inner(input, eof);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn inner(&mut self, input: &[u8], eof: bool) -> Result<Progress, Error> {
        if self.complete {
            return if input.is_empty() {
                Ok(progress(0, None, true))
            } else {
                Err(Error::State)
            };
        }
        if let Some(position) = self.validate {
            let end = position
                .saturating_add(PATH_BYTES_PER_TURN)
                .min(self.path.len());
            for &byte in self
                .path
                .as_bytes()
                .get(position..end)
                .ok_or(Error::State)?
            {
                if self.escaped {
                    if !matches!(byte, b'0' | b'1') {
                        return Err(Error::InvalidResultReference);
                    }
                    self.escaped = false;
                } else if byte == b'~' {
                    self.escaped = true;
                }
            }
            if end == self.path.len() {
                if self.escaped {
                    return Err(Error::InvalidResultReference);
                }
                self.validate = None;
            } else {
                self.validate = Some(end);
            }
            return Ok(progress(0, None, false));
        }
        let scanned = self.scanner.poll(input, eof).map_err(Error::Source)?;
        let piece = scanned
            .event
            .map(|event| self.event(event))
            .transpose()?
            .flatten();
        if scanned.complete {
            if self.depth != 0 || self.capture.is_some() {
                return Err(Error::State);
            }
            if self.array && !self.array_closed {
                self.array_closed = true;
                return Ok(progress(scanned.consumed, Some(Piece::ArrayEnd), false));
            }
            self.complete = true;
        }
        Ok(progress(scanned.consumed, piece, self.complete))
    }
    fn push(&mut self, kind: Kind, rule: Rule) -> Result<(), Error> {
        *self.frames.get_mut(self.depth).ok_or(Error::State)? = Frame { kind, rule };
        self.depth += 1;
        Ok(())
    }
    fn event(&mut self, event: Event) -> Result<Option<Piece>, Error> {
        match event {
            Event::Start { kind, offset } => {
                let select = if self.depth == 0 {
                    if self.started {
                        return Err(Error::State);
                    }
                    self.started = true;
                    Select::Path(if self.path.is_empty() { None } else { Some(1) })
                } else {
                    let frame = self.frames.get_mut(self.depth - 1).ok_or(Error::State)?;
                    match &mut frame.rule {
                        Rule::Ignore => Select::Ignore,
                        Rule::Flatten => Select::Capture,
                        Rule::Object {
                            position, matches, ..
                        } => {
                            let select = if *matches {
                                Select::Path(following(self.path, *position))
                            } else {
                                Select::Ignore
                            };
                            *matches = false;
                            select
                        }
                        Rule::Array {
                            segment,
                            ordinal,
                            found,
                        } => {
                            let select = if segment.wildcard || segment.index == *ordinal {
                                *found = true;
                                Select::Path(segment.next)
                            } else {
                                Select::Ignore
                            };
                            *ordinal = ordinal.checked_add(1).ok_or(Error::State)?;
                            select
                        }
                    }
                };
                match select {
                    Select::Path(Some(start)) => {
                        let mut piece = None;
                        let rule = match kind {
                            Kind::Object => Rule::Object {
                                start,
                                position: start,
                                matches: false,
                                found: false,
                            },
                            Kind::Array => {
                                let segment = array_segment(self.path, start)?;
                                if segment.wildcard && !self.array {
                                    self.array = true;
                                    piece = Some(Piece::ArrayStart);
                                }
                                Rule::Array {
                                    segment,
                                    ordinal: 0,
                                    found: false,
                                }
                            }
                            _ => return Err(Error::InvalidResultReference),
                        };
                        self.push(kind, rule)?;
                        return Ok(piece);
                    }
                    Select::Path(None) if self.array && kind == Kind::Array => {
                        self.push(kind, Rule::Flatten)?
                    }
                    Select::Path(None) | Select::Capture => {
                        if self.capture.is_some() {
                            return Err(Error::State);
                        }
                        if matches!(kind, Kind::Object | Kind::Array) {
                            self.push(kind, Rule::Ignore)?;
                        }
                        self.capture = Some(Capture {
                            kind,
                            depth: self.depth,
                            start: offset,
                        });
                    }
                    Select::Ignore => {
                        if matches!(kind, Kind::Object | Kind::Array) {
                            self.push(kind, Rule::Ignore)?;
                        }
                    }
                }
            }
            Event::End { kind, offset } => {
                let capture = self
                    .capture
                    .filter(|c| c.kind == kind && c.depth == self.depth);
                if matches!(kind, Kind::Object | Kind::Array) {
                    let frame = self
                        .frames
                        .get(self.depth.checked_sub(1).ok_or(Error::State)?)
                        .copied()
                        .ok_or(Error::State)?;
                    if frame.kind != kind {
                        return Err(Error::State);
                    }
                    match frame.rule {
                        Rule::Object { found: false, .. } => {
                            return Err(Error::InvalidResultReference)
                        }
                        Rule::Array {
                            segment,
                            found: false,
                            ..
                        } if !segment.wildcard => return Err(Error::InvalidResultReference),
                        _ => (),
                    }
                    self.depth -= 1;
                }
                if let Some(capture) = capture {
                    self.capture = None;
                    return Ok(Some(Piece::Value {
                        offset: capture.start,
                        length: offset.checked_sub(capture.start).ok_or(Error::State)?,
                    }));
                }
            }
            Event::KeyStart | Event::KeyChar(_) | Event::KeyEnd => {
                let frame = self
                    .frames
                    .get_mut(self.depth.checked_sub(1).ok_or(Error::State)?)
                    .ok_or(Error::State)?;
                if let Rule::Object {
                    start,
                    position,
                    matches,
                    found,
                } = &mut frame.rule
                {
                    match event {
                        Event::KeyStart => {
                            *position = *start;
                            *matches = true;
                        }
                        Event::KeyChar(character) if *matches => {
                            if let Some((expected, next)) = token_char(self.path, *position)? {
                                *matches = expected == character;
                                *position = next;
                            } else {
                                *matches = false;
                            }
                        }
                        Event::KeyEnd => {
                            *matches &= boundary(self.path, *position);
                            if *matches {
                                if *found {
                                    return Err(Error::Source(scan::Error::Syntax));
                                }
                                *found = true;
                            }
                        }
                        _ => (),
                    }
                }
            }
        }
        Ok(None)
    }
}
fn boundary(path: &str, position: usize) -> bool {
    position == path.len() || path.as_bytes().get(position) == Some(&b'/')
}
fn following(path: &str, position: usize) -> Option<usize> {
    (position < path.len()).then_some(position + 1)
}
/// A u64 index is at most 20 digits; inspect at most 21 bytes including its
/// delimiter. Object keys use lazy scalar comparison and have no such limit.
fn array_segment(path: &str, start: usize) -> Result<Segment, Error> {
    if path.as_bytes().get(start) == Some(&b'*') {
        if !boundary(path, start + 1) {
            return Err(Error::InvalidResultReference);
        }
        return Ok(Segment {
            next: following(path, start + 1),
            index: 0,
            wildcard: true,
        });
    }
    let mut index = 0_u64;
    for position in (start..).take(21) {
        if boundary(path, position) {
            if position == start {
                return Err(Error::InvalidResultReference);
            }
            return Ok(Segment {
                next: following(path, position),
                index,
                wildcard: false,
            });
        }
        let byte = *path.as_bytes().get(position).ok_or(Error::State)?;
        if !byte.is_ascii_digit() || (position > start && path.as_bytes().get(start) == Some(&b'0'))
        {
            return Err(Error::InvalidResultReference);
        }
        index = index
            .checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(byte - b'0')))
            .ok_or(Error::InvalidResultReference)?;
    }
    Err(Error::InvalidResultReference)
}
fn token_char(path: &str, position: usize) -> Result<Option<(char, usize)>, Error> {
    let Some(character) = path.get(position..).ok_or(Error::State)?.chars().next() else {
        return Ok(None);
    };
    if character == '/' {
        return Ok(None);
    }
    if character == '~' {
        let decoded = match path.as_bytes().get(position + 1) {
            Some(b'0') => '~',
            Some(b'1') => '/',
            _ => return Err(Error::State),
        };
        Ok(Some((decoded, position + 2)))
    } else {
        Ok(Some((character, position + character.len_utf8())))
    }
}
fn progress(consumed: usize, piece: Option<Piece>, complete: bool) -> Progress {
    Progress {
        consumed,
        piece,
        complete,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use td_json::{json, Json};

    fn resolve(input: &str, path: &str, width: usize) -> Result<Json, Error> {
        let mut walker = Walker::new(path, input.len().max(1) as u64, scan::MAX_DEPTH)?;
        let mut offset: usize = 0;
        let mut result = String::new();
        let mut array = false;
        let mut first = true;
        for _ in 0..(input.len() + path.len()) * 10 + 100 {
            let end = offset.saturating_add(width).min(input.len());
            let next = walker.poll(
                input.as_bytes().get(offset..end).unwrap(),
                end == input.len(),
            )?;
            assert!(next.consumed <= 4096 && next.consumed <= end - offset);
            offset += next.consumed;
            match next.piece {
                Some(Piece::ArrayStart) => {
                    assert!(!array);
                    array = true;
                    result.push('[');
                }
                Some(Piece::Value { offset, length }) => {
                    if array && !first {
                        result.push(',');
                    }
                    first = false;
                    result.push_str(
                        input
                            .get(offset as usize..(offset + length) as usize)
                            .unwrap(),
                    );
                }
                Some(Piece::ArrayEnd) => {
                    assert!(array);
                    result.push(']');
                }
                None => (),
            }
            if next.complete {
                assert_eq!(offset, input.len());
                assert!(walker.poll(&[], true)?.complete);
                return Ok(td_json::parse(&result).unwrap());
            }
        }
        panic!("walk did not complete");
    }
    #[test]
    fn escaped_empty_and_unicode_keys_and_root_values() {
        let input = r#"{"a/b":{"~1":{"":42}},"é😀":true,"*":{"00":"ok"}}"#;
        for width in 1..=input.len() {
            assert_eq!(resolve(input, "/a~1b/~01/", width).unwrap(), json!(42));
            assert_eq!(resolve(input, "/é😀", width).unwrap(), json!(true));
            assert_eq!(resolve(input, "/*/00", width).unwrap(), json!("ok"));
            assert_eq!(
                resolve(input, "", width).unwrap(),
                td_json::parse(input).unwrap()
            );
        }
        assert_eq!(
            resolve(r#"{"\u00e9\uD83D\uDE00":4}"#, "/é😀", 1).unwrap(),
            json!(4)
        );
        for input in ["42", "true", "null", "\"hello\"", "[1,2]", "{}"] {
            assert_eq!(
                resolve(input, "", 1).unwrap(),
                td_json::parse(input).unwrap()
            );
        }
    }
    #[test]
    fn canonical_array_indexes_and_wrong_type_are_distinct_from_object_keys() {
        assert_eq!(
            resolve(r#"{"a":[0,{"x":[2,3]}]}"#, "/a/1/x/0", 1).unwrap(),
            json!(2)
        );
        for path in [
            "/00",
            "/01",
            "/-",
            "/+1",
            "/1.0",
            "/2",
            "/18446744073709551616",
            "/",
            "/a",
        ] {
            assert_eq!(
                resolve("[0,1]", path, 1),
                Err(Error::InvalidResultReference),
                "{path}"
            );
        }
        assert_eq!(
            resolve(r#"{"00":1,"-":2,"":3}"#, "/00", 1).unwrap(),
            json!(1)
        );
        assert_eq!(
            resolve(r#"{"00":1,"-":2,"":3}"#, "/-", 1).unwrap(),
            json!(2)
        );
        assert_eq!(resolve(r#"{"00":1,"-":2,"":3}"#, "/", 1).unwrap(), json!(3));
        assert_eq!(resolve("42", "/x", 1), Err(Error::InvalidResultReference));
        assert_eq!(
            resolve("{}", "/missing", 1),
            Err(Error::InvalidResultReference)
        );
    }
    #[test]
    fn wildcard_preserves_order_and_flattens_exactly_one_leaf_array_level() {
        let input = r#"{"list":[{"emailIds":["m2","m1"]},{"emailIds":["m3"]},{"emailIds":[]},{"emailIds":null}]}"#;
        for width in 1..=input.len() {
            assert_eq!(
                resolve(input, "/list/*/emailIds", width).unwrap(),
                json!(["m2", "m1", "m3", null])
            );
        }
        assert_eq!(
            resolve("[[[1,2]],3,[4,[5]]]", "/*", 1).unwrap(),
            json!([[1, 2], 3, 4, [5]])
        );
        assert_eq!(
            resolve("[[[1,2]],[[3]],[]]", "/*/*", 1).unwrap(),
            json!([1, 2, 3])
        );
        assert_eq!(
            resolve(r#"[{"x":{"*": [1,[2]]}},{"x":[[3,[4]],5]}]"#, "/*/x/*", 1).unwrap(),
            json!([1, [2], 3, [4], 5])
        );
    }
    #[test]
    fn missing_wildcard_branch_invalidates_provisional_output_and_is_sticky() {
        let input = r#"[{"x":1},{}]"#;
        assert_eq!(
            resolve(input, "/*/x", 1),
            Err(Error::InvalidResultReference)
        );
        let mut walker = Walker::new("/*/x", 1000, 160).unwrap();
        let mut offset = 0;
        let mut emitted = false;
        let mut refused = false;
        for _ in 0..input.len() * 10 {
            match walker.poll(&input.as_bytes()[offset..], true) {
                Ok(next) => {
                    offset += next.consumed;
                    emitted |= matches!(next.piece, Some(Piece::Value { .. }));
                }
                Err(error) => {
                    assert_eq!(error, Error::InvalidResultReference);
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert!(emitted);
        assert_eq!(walker.poll(b"[]", true), Err(Error::InvalidResultReference));
    }
    #[test]
    fn empty_wildcard_does_not_impose_a_pointer_depth_limit() {
        let path = format!("/*{}", "/unused".repeat(1000));
        assert_eq!(resolve("[]", &path, 1).unwrap(), json!([]));
        assert_eq!(
            resolve(r#"{"list":[]}"#, "/list/*/x", 1).unwrap(),
            json!([])
        );
        for path in ["x", "#/x", "/~", "/~2", "/*/~x", "/~00~"] {
            assert_eq!(resolve("[]", path, 1), Err(Error::InvalidResultReference));
        }
    }
    #[test]
    fn long_pointer_validation_and_lazy_key_matching_yield() {
        let key = "x".repeat(1025);
        let input = format!("{{\"{key}\":7}}");
        let path = format!("/{key}");
        let mut walker = Walker::new(&path, input.len() as u64, 160).unwrap();
        for _ in 0..5 {
            assert_eq!(
                walker.poll(input.as_bytes(), true).unwrap(),
                progress(0, None, false)
            );
        }
        assert_eq!(walker.poll(input.as_bytes(), true).unwrap().consumed, 1);
        assert_eq!(
            walker.poll(&input.as_bytes()[1..], true).unwrap().consumed,
            1
        );
        assert_eq!(resolve(&input, &path, 17).unwrap(), json!(7));
        assert_eq!(
            resolve(&input, &format!("/{}y", "x".repeat(1024)), 17),
            Err(Error::InvalidResultReference)
        );
    }
    #[test]
    fn source_limits_and_malformed_json_are_not_client_reference_errors() {
        assert_eq!(
            Walker::new("", 10, scan::MAX_DEPTH + 1).err(),
            Some(Error::Source(scan::Error::State))
        );
        for (input, bytes, depth, expected) in [
            ("[1]", 2, 160, scan::Error::Bytes),
            ("[[]]", 100, 1, scan::Error::Depth),
            ("[1,]", 100, 160, scan::Error::Syntax),
        ] {
            let mut walker = Walker::new("", bytes, depth).unwrap();
            let mut offset = 0;
            let mut refused = false;
            for _ in 0..32 {
                match walker.poll(&input.as_bytes()[offset..], true) {
                    Ok(next) => offset += next.consumed,
                    Err(error) => {
                        assert_eq!(error, Error::Source(expected));
                        refused = true;
                        break;
                    }
                }
            }
            assert!(refused);
            assert_eq!(walker.poll(&[], true), Err(Error::Source(expected)));
        }
        assert!(std::mem::size_of::<Walker<'_>>() <= 16384);
        fn send<T: Send>() {}
        send::<Walker<'static>>();
        assert!(!format!("{:?}", Walker::new("/secret", 100, 160).unwrap()).contains("secret"));
    }
    #[test]
    fn deepest_source_and_wide_array_selection_preserve_results() {
        let input = format!("{}0{}", "[".repeat(160), "]".repeat(160));
        let path = "/0".repeat(160);
        assert_eq!(resolve(&input, &path, 1).unwrap(), json!(0));
        let input = format!(
            "[{}]",
            (0..10000)
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        assert_eq!(resolve(&input, "/9999", 8192).unwrap(), json!(9999));
    }
    #[test]
    fn generated_trees_match_an_independent_recursive_pointer_oracle() {
        fn oracle(value: &Json, tokens: &[&str]) -> Option<Json> {
            let Some((token, rest)) = tokens.split_first() else {
                return Some(value.clone());
            };
            match value {
                Json::Obj(_) => oracle(value.get(token)?, rest),
                Json::Arr(items) if *token == "*" => {
                    let mut out = Vec::new();
                    for item in items {
                        match oracle(item, rest)? {
                            Json::Arr(children) => out.extend(children),
                            scalar => out.push(scalar),
                        }
                    }
                    Some(Json::Arr(out))
                }
                Json::Arr(items) => {
                    if token.is_empty()
                        || (token.len() > 1 && token.starts_with('0'))
                        || !token.bytes().all(|b| b.is_ascii_digit())
                    {
                        return None;
                    }
                    oracle(items.get(token.parse::<usize>().ok()?)?, rest)
                }
                _ => None,
            }
        }
        fn tree(seed: &mut u64, depth: usize) -> Json {
            *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let pick = (*seed >> 32) % 7;
            if depth == 0 || pick < 2 {
                return json!(pick);
            }
            if pick < 5 {
                Json::Arr((0..pick - 2).map(|_| tree(seed, depth - 1)).collect())
            } else {
                Json::Obj(vec![
                    ("a".into(), tree(seed, depth - 1)),
                    ("*".into(), tree(seed, depth - 1)),
                ])
            }
        }
        let mut seed = 7;
        for _ in 0..200 {
            let value = tree(&mut seed, 4);
            let input = value.to_string();
            for path in [
                "", "/a", "/a/*", "/*", "/*/a", "/*/*", "/*/*/a", "/0", "/0/*", "/a/0", "/*/a/*",
                "/*/a/*/*",
            ] {
                let tokens: Vec<_> = path.split('/').skip(1).collect();
                let expected = oracle(&value, &tokens).ok_or(Error::InvalidResultReference);
                assert_eq!(resolve(&input, path, 7), expected, "{input} {path}");
            }
        }
    }
    #[test]
    fn duplicate_target_keys_trailing_whitespace_and_first_detected_error() {
        assert_eq!(
            resolve(r#"{"x":1,"x":2}"#, "/x", 1),
            Err(Error::Source(scan::Error::Syntax))
        );
        assert_eq!(resolve("[1] \n ", "/*", 100).unwrap(), json!([1]));
        assert_eq!(
            resolve("[broken", "/x", 1),
            Err(Error::InvalidResultReference)
        );
        assert_eq!(
            resolve("42garbage", "/x", 1),
            Err(Error::InvalidResultReference)
        );
        assert_eq!(
            resolve("[broken", "", 1),
            Err(Error::Source(scan::Error::Syntax))
        );
    }
}
