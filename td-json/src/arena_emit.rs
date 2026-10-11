//! Resumable JSON emission from immutable owned token arenas.
//! Output is provisional until completion. Callers own retention, work and time.
use crate::{arena, string};

const FRAMES: usize = crate::MAX_DEPTH;
const TURNS: usize = 256;
const NUMBER_BYTES: usize = 256;
const STRING_STEPS: usize = 32;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Limit,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Invalid => "JSON emission state invalid",
            Self::Limit => "JSON emission byte or depth ceiling",
        })
    }
}
impl std::error::Error for Error {}

/// One immutable arena and bounded writer. Moving this owner preserves every
/// source position. Each poll performs at most 256 transitions, each copying
/// at most 256 number bytes, one literal (at most five bytes), or performing
/// up to 32 shared string-frame steps (one scalar or at most six bytes each).
/// No whole-string UTF-8 scan, allocation or recursive traversal is performed.
/// The limit is per value, not a storage reservation or live admission grant.
pub struct OwnedEmitter {
    document: arena::OwnedDocument,
    emitter: Emitter,
}
impl std::fmt::Debug for OwnedEmitter {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("OwnedEmitter(<redacted>)")
    }
}
/// Constructor/completion refusal retains the immutable source for reuse.
pub struct Refusal {
    error: Error,
    document: arena::OwnedDocument,
}
impl Refusal {
    pub const fn error(&self) -> Error {
        self.error
    }
    pub fn into_document(self) -> arena::OwnedDocument {
        self.document
    }
}
impl std::fmt::Debug for Refusal {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_tuple("Refusal").field(&self.error).finish()
    }
}
impl std::fmt::Display for Refusal {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(out)
    }
}
impl std::error::Error for Refusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
impl OwnedEmitter {
    pub fn new(document: arena::OwnedDocument, index: usize, limit: u64) -> Result<Self, Refusal> {
        if !document
            .view()
            .is_ok_and(|view| view.value_at(index).is_some())
        {
            return Err(Refusal {
                error: Error::Invalid,
                document,
            });
        }
        Ok(Self {
            document,
            emitter: Emitter::new(index, limit),
        })
    }
    pub fn view(&self) -> Result<arena::Document<'_>, Error> {
        self.document.view().map_err(|_| Error::Invalid)
    }
    /// Emitted values remain provisional; errors latch. Empty output makes no
    /// progress, and cached completion emits nothing. The caller must check
    /// original admission before and after every poll, including completion.
    pub fn poll(&mut self, output: &mut [u8]) -> Result<Progress, Error> {
        if let Some(error) = self.emitter.failed {
            return Err(error);
        }
        let result = self
            .document
            .view()
            .map_err(|_| Error::Invalid)
            .and_then(|document| self.emitter.poll(&document, output));
        if let Err(error) = result {
            self.emitter.failed = Some(error);
        }
        result
    }
    /// Charged serialized length, including at most six staged string bytes.
    /// It equals the exact emitted length only after successful completion.
    pub const fn total(&self) -> u64 {
        self.emitter.total
    }
    /// Begin another value only after successful completion. Refusal leaves
    /// the current state unchanged. This does not renew external admission.
    pub fn restart(&mut self, index: usize, limit: u64) -> Result<(), Error> {
        if let Some(error) = self.emitter.failed {
            return Err(error);
        }
        if !matches!(self.emitter.mode, Mode::Done) || self.view()?.value_at(index).is_none() {
            return Err(Error::Invalid);
        }
        self.emitter = Emitter::new(index, limit);
        Ok(())
    }
    pub fn finish(self) -> Result<arena::OwnedDocument, Refusal> {
        let error = self
            .emitter
            .failed
            .or_else(|| (!matches!(self.emitter.mode, Mode::Done)).then_some(Error::Invalid));
        match error {
            Some(error) => Err(Refusal {
                error,
                document: self.document,
            }),
            None => Ok(self.document),
        }
    }
    /// Abandon provisional output and recover the unchanged source document.
    pub fn into_document(self) -> arena::OwnedDocument {
        self.document
    }
}
const _: () = assert!(std::mem::size_of::<OwnedEmitter>() <= 8192);

#[derive(Clone, Copy)]
struct Frame {
    next: Option<usize>,
    left: usize,
    object: bool,
    first: bool,
}
impl Frame {
    const EMPTY: Self = Self {
        next: None,
        left: 0,
        object: false,
        first: true,
    };
}
#[derive(Clone, Copy)]
enum After {
    Items,
    Item,
    Value(usize),
    Colon(usize),
    Return,
}
#[derive(Clone, Copy)]
enum Text {
    Key(usize),
    Value(usize),
}
#[derive(Clone, Copy)]
enum Mode {
    Value(usize),
    Items,
    Item,
    Colon(usize),
    Literal {
        bytes: &'static [u8],
        offset: usize,
        after: After,
    },
    Number {
        index: usize,
        offset: usize,
    },
    String {
        text: Text,
        after: After,
    },
    Done,
}
struct Emitter {
    frames: [Frame; FRAMES],
    depth: usize,
    mode: Mode,
    string: string::Frame<Error>,
    string_offset: usize,
    total: u64,
    limit: u64,
    failed: Option<Error>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Bytes written this poll and whether the entire selected value is complete.
pub struct Progress {
    pub written: usize,
    pub complete: bool,
}
impl Emitter {
    fn new(index: usize, limit: u64) -> Self {
        Self {
            frames: [Frame::EMPTY; FRAMES],
            depth: 0,
            mode: Mode::Value(index),
            string: string::Frame::new(),
            string_offset: 0,
            total: 0,
            limit,
            failed: None,
        }
    }
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        charge(&mut self.total, self.limit, bytes as u64)
    }
    fn after(&mut self, after: After) {
        self.mode = match after {
            After::Items => Mode::Items,
            After::Item => Mode::Item,
            After::Value(index) => Mode::Value(index),
            After::Colon(index) => Mode::Colon(index),
            After::Return => {
                if self.depth == 0 {
                    Mode::Done
                } else {
                    Mode::Items
                }
            }
        };
    }
    fn literal(&mut self, bytes: &'static [u8], after: After) {
        self.mode = Mode::Literal {
            bytes,
            offset: 0,
            after,
        };
    }
    fn text(&mut self, text: Text, after: After) {
        self.string = string::Frame::new();
        self.string_offset = 0;
        self.mode = Mode::String { text, after };
    }
    // OwnedEmitter keeps these positions with one immutable document.
    fn poll(
        &mut self,
        document: &arena::Document<'_>,
        output: &mut [u8],
    ) -> Result<Progress, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let mut written = 0;
        for _ in 0..TURNS {
            if matches!(self.mode, Mode::Done) || written == output.len() {
                break;
            }
            let result = (|| {
                let count =
                    self.transition(document, output.get_mut(written..).ok_or(Error::Invalid)?)?;
                written
                    .checked_add(count)
                    .filter(|next| *next <= output.len())
                    .ok_or(Error::Invalid)
            })();
            match result {
                Ok(next) => written = next,
                Err(error) => {
                    self.failed = Some(error);
                    return Err(error);
                }
            }
        }
        Ok(Progress {
            written,
            complete: matches!(self.mode, Mode::Done),
        })
    }
    fn transition(
        &mut self,
        document: &arena::Document<'_>,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        match self.mode {
            Mode::Value(index) => {
                let value = document.value_at(index).ok_or(Error::Invalid)?;
                if value.is_null() {
                    self.literal(b"null", After::Return);
                } else if let Some(value) = value.as_bool() {
                    self.literal(if value { b"true" } else { b"false" }, After::Return);
                } else if value.as_string_bytes().is_some() {
                    self.text(Text::Value(index), After::Return);
                } else if value.as_number_bytes().is_some() {
                    self.mode = Mode::Number { index, offset: 0 };
                } else {
                    let (object, left) = if let Some(array) = value.as_array() {
                        (false, array.len())
                    } else {
                        (true, value.as_object().ok_or(Error::Invalid)?.len())
                    };
                    *self.frames.get_mut(self.depth).ok_or(Error::Limit)? = Frame {
                        next: value.first_child_index(),
                        left,
                        object,
                        first: true,
                    };
                    self.depth += 1;
                    self.literal(if object { b"{" } else { b"[" }, After::Items);
                }
            }
            Mode::Items => {
                let depth = self.depth.checked_sub(1).ok_or(Error::Invalid)?;
                let frame = self.frames.get(depth).ok_or(Error::Invalid)?;
                if frame.left == 0 {
                    let object = frame.object;
                    self.depth = depth;
                    self.literal(if object { b"}" } else { b"]" }, After::Return);
                } else if frame.first {
                    self.mode = Mode::Item;
                } else {
                    self.literal(b",", After::Item);
                }
            }
            Mode::Item => {
                let depth = self.depth.checked_sub(1).ok_or(Error::Invalid)?;
                let frame = self.frames.get_mut(depth).ok_or(Error::Invalid)?;
                let index = frame.next.ok_or(Error::Invalid)?;
                frame.next = document.next_index(index);
                frame.left = frame.left.checked_sub(1).ok_or(Error::Invalid)?;
                frame.first = false;
                if frame.object {
                    let (_, value) = document.member_at(index).ok_or(Error::Invalid)?;
                    self.text(Text::Key(index), After::Colon(value.token_index()));
                } else {
                    self.mode = Mode::Value(index);
                }
            }
            Mode::Colon(index) => self.literal(b":", After::Value(index)),
            Mode::Literal {
                bytes,
                offset,
                after,
            } => {
                let bytes = bytes.get(offset..).ok_or(Error::Invalid)?;
                let count = bytes.len().min(output.len());
                self.charge(count)?;
                output
                    .get_mut(..count)
                    .ok_or(Error::Invalid)?
                    .copy_from_slice(bytes.get(..count).ok_or(Error::Invalid)?);
                if count == bytes.len() {
                    self.after(after);
                } else if let Mode::Literal { offset, .. } = &mut self.mode {
                    *offset = offset.checked_add(count).ok_or(Error::Invalid)?;
                }
                return Ok(count);
            }
            Mode::Number { index, offset } => {
                let bytes = document
                    .value_at(index)
                    .and_then(arena::Value::as_number_bytes)
                    .ok_or(Error::Invalid)?
                    .get(offset..)
                    .ok_or(Error::Invalid)?;
                let count = bytes.len().min(output.len()).min(NUMBER_BYTES);
                self.charge(count)?;
                output
                    .get_mut(..count)
                    .ok_or(Error::Invalid)?
                    .copy_from_slice(bytes.get(..count).ok_or(Error::Invalid)?);
                if count == bytes.len() {
                    self.after(After::Return);
                } else {
                    self.mode = Mode::Number {
                        index,
                        offset: offset.checked_add(count).ok_or(Error::Invalid)?,
                    };
                }
                return Ok(count);
            }
            Mode::String { text, after } => {
                let bytes = match text {
                    Text::Key(index) => document.member_at(index).map(|(key, _)| key),
                    Text::Value(index) => document
                        .value_at(index)
                        .and_then(arena::Value::as_string_bytes),
                }
                .ok_or(Error::Invalid)?;
                let mut source = TextSource {
                    bytes,
                    offset: &mut self.string_offset,
                    total: &mut self.total,
                    limit: self.limit,
                };
                let mut written = 0;
                for _ in 0..STRING_STEPS {
                    if written == output.len() {
                        break;
                    }
                    let progress = self
                        .string
                        .poll(
                            &mut source,
                            (),
                            output.get_mut(written..).ok_or(Error::Invalid)?,
                        )
                        .map_err(|error| match error {
                            string::Error::Source(error) => error,
                            string::Error::InvalidState => Error::Invalid,
                        })?;
                    written = written
                        .checked_add(progress.written)
                        .filter(|next| *next <= output.len())
                        .ok_or(Error::Invalid)?;
                    if progress.status == string::Status::Complete {
                        self.after(after);
                        break;
                    }
                }
                return Ok(written);
            }
            Mode::Done => (),
        }
        Ok(0)
    }
}
fn charge(total: &mut u64, limit: u64, bytes: u64) -> Result<(), Error> {
    let next = total.checked_add(bytes).ok_or(Error::Limit)?;
    if next > limit {
        return Err(Error::Limit);
    }
    *total = next;
    Ok(())
}
struct TextSource<'a, 'b> {
    bytes: &'a [u8],
    offset: &'b mut usize,
    total: &'b mut u64,
    limit: u64,
}
impl string::Source for TextSource<'_, '_> {
    type Context = ();
    type Error = Error;
    fn charge_output(&mut self, _: (), bytes: u64) -> Result<(), Error> {
        charge(self.total, self.limit, bytes)
    }
    fn poll(&mut self, _: ()) -> Result<string::Scalar, Error> {
        if *self.offset == self.bytes.len() {
            return Ok(string::Scalar::Complete);
        }
        let first = *self.bytes.get(*self.offset).ok_or(Error::Invalid)?;
        let width = match first {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(Error::Invalid),
        };
        let end = self.offset.checked_add(width).ok_or(Error::Invalid)?;
        let scalar = if width == 1 {
            char::from(first)
        } else {
            self.bytes
                .get(*self.offset..end)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .and_then(|text| text.chars().next())
                .ok_or(Error::Invalid)?
        };
        *self.offset = end;
        Ok(string::Scalar::Value(scalar))
    }
}
const _: () = assert!(std::mem::size_of::<Emitter>() <= 8192);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn emit(input: &str, chunk: usize, limit: u64) -> Result<(String, u64), Error> {
        let mut bytes = input.as_bytes().to_vec();
        let mut tokens = vec![arena::Token::EMPTY; 4096];
        let document = arena::parse(
            &mut bytes,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: input.len(),
                max_depth: 128,
                max_tokens: 4096,
            },
        )
        .unwrap();
        let mut emitter = Emitter::new(0, limit);
        let mut output = vec![0; chunk];
        let mut result = Vec::new();
        loop {
            let progress = emitter.poll(&document, &mut output)?;
            result.extend_from_slice(&output[..progress.written]);
            if progress.complete {
                assert_eq!(emitter.poll(&document, &mut output)?.written, 0);
                return Ok((String::from_utf8(result).unwrap(), emitter.total));
            }
        }
    }
    fn owned(input: &str) -> (arena::OwnedDocument, (*const u8, *const arena::Token)) {
        let bytes = input.as_bytes().to_vec().into_boxed_slice();
        let tokens = vec![arena::Token::EMPTY; 4096].into_boxed_slice();
        let pointers = (bytes.as_ptr(), tokens.as_ptr());
        let mut parser = arena::OwnedCursor::with_input_length(
            bytes,
            tokens,
            input.len(),
            arena::ParseLimits {
                max_bytes: input.len(),
                max_depth: 128,
                max_tokens: 4096,
            },
        )
        .unwrap();
        while !parser.step().unwrap() {}
        (parser.finish().unwrap(), pointers)
    }
    #[test]
    fn owned_emission_moves_restarts_and_returns_the_same_buffers() {
        fn send<T: Send>() {}
        send::<OwnedEmitter>();
        let (document, pointers) = owned(r#"["é\n",123.4]"#);
        let index = document.view().unwrap().root().first_child_index().unwrap();
        let second = document.view().unwrap().next_index(index).unwrap();
        let mut queue = Some(OwnedEmitter::new(document, index, 64).unwrap());
        let mut output = [0; 1];
        let mut result = Vec::new();
        let mut emitter = loop {
            let mut emitter = queue.take().unwrap();
            assert_eq!(emitter.restart(second, 64), Err(Error::Invalid));
            let progress = emitter.poll(&mut output).unwrap();
            result.extend_from_slice(&output[..progress.written]);
            if progress.complete {
                break emitter;
            }
            queue = Some(emitter);
        };
        assert_eq!(std::str::from_utf8(&result).unwrap(), r#""é\n""#);
        assert_eq!(emitter.total(), result.len() as u64);
        assert!(emitter.view().unwrap().root().as_array().is_some());
        emitter.restart(second, 5).unwrap();
        result.clear();
        loop {
            let progress = emitter.poll(&mut output).unwrap();
            result.extend_from_slice(&output[..progress.written]);
            if progress.complete {
                break;
            }
        }
        assert_eq!(result, b"123.4");
        let (bytes, tokens) = emitter.finish().unwrap().into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
    }
    #[test]
    fn owned_refusals_and_abandonment_retain_source_and_hide_contents() {
        let (document, pointers) = owned(r#""secret""#);
        let refusal = OwnedEmitter::new(document, usize::MAX, 64).err().unwrap();
        assert_eq!(refusal.error(), Error::Invalid);
        assert!(!format!("{refusal:?} {refusal}").contains("secret"));
        let emitter = OwnedEmitter::new(refusal.into_document(), 0, 64).unwrap();
        assert!(!format!("{emitter:?}").contains("secret"));
        let refusal = emitter.finish().err().unwrap();
        assert_eq!(refusal.error(), Error::Invalid);
        let mut emitter = OwnedEmitter::new(refusal.into_document(), 0, 1).unwrap();
        assert!(matches!(emitter.poll(&mut [0; 64]), Err(Error::Limit)));
        assert_eq!(emitter.restart(0, 64), Err(Error::Limit));
        let refusal = emitter.finish().err().unwrap();
        assert_eq!(refusal.error(), Error::Limit);
        let emitter = OwnedEmitter::new(refusal.into_document(), 0, 64).unwrap();
        let (bytes, tokens) = emitter.into_document().into_buffers();
        assert_eq!((bytes.as_ptr(), tokens.as_ptr()), pointers);
    }
    #[test]
    fn oversized_output_does_not_remove_the_transition_quantum() {
        for input in ["1".repeat(200_000), format!("\"{}\"", "x".repeat(200_000))] {
            let (document, _) = owned(&input);
            let mut emitter = OwnedEmitter::new(document, 0, u64::MAX).unwrap();
            let progress = emitter.poll(&mut vec![0; 262144]).unwrap();
            assert!(!progress.complete);
            assert!(progress.written <= NUMBER_BYTES * TURNS);
        }
    }
    #[test]
    fn object_subvalues_number_bytes_and_mid_scalar_limits() {
        let (document, _) = owned(r#"{"outer":{"number":-123.00e4},"nil":null}"#);
        let view = document.view().unwrap();
        let inner = view.root().get("outer").unwrap();
        assert_eq!(
            inner.get("number").unwrap().as_number_bytes(),
            Some(b"-123.00e4".as_slice())
        );
        assert_eq!(view.root().get("nil").unwrap().as_number_bytes(), None);
        let index = inner.token_index();
        let mut emitter = OwnedEmitter::new(document, index, 128).unwrap();
        let mut output = [0; 128];
        let progress = emitter.poll(&mut output).unwrap();
        assert!(progress.complete);
        assert_eq!(&output[..progress.written], br#"{"number":-123.00e4}"#);
        for input in [
            format!("{{\"{}\":0}}", r"é\n".repeat(1000)),
            "1".repeat(10000),
        ] {
            let (document, _) = owned(&input);
            let mut emitter = OwnedEmitter::new(document, 0, 1024).unwrap();
            let mut output = [0; 17];
            let mut successful = 0;
            loop {
                match emitter.poll(&mut output) {
                    Ok(progress) => {
                        assert!(!progress.complete);
                        successful += 1;
                    }
                    Err(Error::Limit) => break,
                    Err(error) => panic!("unexpected {error:?}"),
                }
            }
            assert!(successful > 1);
            assert_eq!(emitter.poll(&mut output), Err(Error::Limit));
        }
    }
    #[test]
    fn ascii_string_batches_fill_useful_output_without_removing_the_quantum() {
        let (document, _) = owned(&format!("\"{}\"", "x".repeat(200_000)));
        let mut emitter = OwnedEmitter::new(document, 0, u64::MAX).unwrap();
        let mut output = [0; 16384];
        let progress = emitter.poll(&mut output).unwrap();
        assert!(!progress.complete);
        assert!(progress.written >= 2048);
        assert!(progress.written <= TURNS * STRING_STEPS * 6);
    }
    #[test]
    fn maximum_parser_nesting_emits_without_recursion() {
        let input = format!(
            "{}0{}",
            "[".repeat(crate::MAX_DEPTH),
            "]".repeat(crate::MAX_DEPTH)
        );
        assert_eq!(emit(&input, 1, u64::MAX).unwrap().0, input);
    }

    #[test]
    fn escaped_nested_values_match_the_shared_encoder_with_short_output() {
        for input in [
            r#"{"é\\\n\"":{"😀":"value"}}"#,
            r#"null"#,
            r#"true"#,
            r#"false"#,
            r#"123.4500e-004"#,
            r#"[{},[],"é😀\n\u0000\\\"",true,null]"#,
            r#"{"z":[1,2],"a":{"b":"\u0061"},"":false}"#,
        ] {
            let mut bytes = input.as_bytes().to_vec();
            let mut tokens = [arena::Token::EMPTY; 128];
            let document = arena::parse(
                &mut bytes,
                &mut tokens,
                arena::ParseLimits {
                    max_bytes: input.len(),
                    max_depth: 128,
                    max_tokens: 128,
                },
            )
            .unwrap();
            let mut expected = String::new();
            document.root().write_json(&mut expected).unwrap();
            for chunk in 1..=17 {
                let (actual, total) = emit(input, chunk, u64::MAX).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(total, expected.len() as u64);
                assert_eq!(emit(input, chunk, total).unwrap().0, expected);
                assert_eq!(emit(input, chunk, total - 1), Err(Error::Limit));
            }
        }
    }
    #[test]
    fn large_scalar_work_yields_and_zero_output_does_not_advance() {
        for input in [format!("\"{}\"", "é".repeat(10000)), "1".repeat(10000)] {
            let mut bytes = input.as_bytes().to_vec();
            let mut tokens = [arena::Token::EMPTY; 1];
            let document = arena::parse(
                &mut bytes,
                &mut tokens,
                arena::ParseLimits {
                    max_bytes: input.len(),
                    max_depth: 128,
                    max_tokens: 1,
                },
            )
            .unwrap();
            let mut emitter = Emitter::new(0, u64::MAX);
            let progress = emitter.poll(&document, &mut []).unwrap();
            assert_eq!(progress.written, 0);
            assert!(!progress.complete);
            assert_eq!(emitter.total, 0);
            let mut output = [0; 1024];
            let mut turns = 0;
            let mut total = 0;
            loop {
                let progress = emitter.poll(&document, &mut output).unwrap();
                assert!(progress.written <= output.len());
                total += progress.written;
                turns += 1;
                if progress.complete {
                    break;
                }
            }
            assert!(turns > 1);
            assert_eq!(total, input.len());
        }
    }
    #[test]
    fn invalid_positions_and_output_limits_stick() {
        let mut bytes = *b"[1]";
        let mut tokens = [arena::Token::EMPTY; 2];
        let document = arena::parse(
            &mut bytes,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: 3,
                max_depth: 1,
                max_tokens: 2,
            },
        )
        .unwrap();
        let mut output = [0; 32];
        for (index, limit, expected) in [(usize::MAX, 8, Error::Invalid), (0, 2, Error::Limit)] {
            let mut emitter = Emitter::new(index, limit);
            assert!(
                matches!(emitter.poll(&document, &mut output), Err(error) if error == expected)
            );
            assert!(
                matches!(emitter.poll(&document, &mut output), Err(error) if error == expected)
            );
        }
    }
}
