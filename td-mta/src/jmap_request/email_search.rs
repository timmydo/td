//! Bounded text operands and streaming substring matches across mail fields.
use super::{Error, Worker};
use crate::{
    admission::work::{Charge, Meter},
    ports::Clock,
};
use td_json::{arena, scan};

pub(super) fn charge(work: &mut Meter, clock: &dyn Clock, records: u64) -> Result<(), Error> {
    work.charge(
        clock.monotonic().map_err(Error::Storage)?,
        Charge {
            records,
            ..Charge::default()
        },
    )
    .map_err(|stop| match stop {
        crate::admission::work::Stop::Deadline => Error::ExecutionDeadline,
        _ => Error::StorageCapacity,
    })
}
fn space(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\u{a0}'
}
fn lower(c: char) -> Result<char, Error> {
    td_mime::unicode::simple_lowercase(c).map_err(|_| Error::State)
}
pub(super) trait Sink {
    fn reset(&mut self);
    fn field(&mut self);
    fn scalar(&mut self, c: char, work: &mut Meter, clock: &dyn Clock) -> Result<(), Error>;
    fn text(&mut self, text: &str, work: &mut Meter, clock: &dyn Clock) -> Result<(), Error> {
        self.field();
        for c in text.chars() {
            self.scalar(c, work, clock)?;
        }
        Ok(())
    }
}

/// Complete, bounded search interpretation. Field boundaries are significant:
/// phrases cannot match across separate addresses, headers or MIME parts.
pub struct SearchFields {
    fields: Vec<String>,
    bytes: usize,
    new_field: bool,
    whitespace: bool,
    representation_full: bool,
}
impl SearchFields {
    pub const VERSION: u32 = 1;
    pub const MAX_BYTES: usize = 8 * 1024 * 1024;
    pub const MAX_FIELDS: usize = 65536;

    pub(super) fn new() -> Self {
        Self {
            fields: Vec::new(),
            bytes: 0,
            new_field: true,
            whitespace: false,
            representation_full: false,
        }
    }

    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.fields.iter().map(String::as_str)
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn representation_full(&self) -> bool {
        self.representation_full
    }

    /// Match already normalized fields in bounded batches without repeating
    /// MIME decoding or sampling the clock once per scalar.
    pub fn matches(
        &self,
        operand: &str,
        work: &mut Meter,
        clock: &dyn Clock,
    ) -> Result<bool, Error> {
        if operand.len() > 4096 {
            return Err(Error::UnsupportedFilter);
        }
        charge(work, clock, operand.len() as u64)?;
        let mut search = Search::parse(operand)?;
        self.matches_search(&mut search, work, clock)
    }

    pub(super) fn matches_search(
        &self,
        search: &mut Search,
        work: &mut Meter,
        clock: &dyn Clock,
    ) -> Result<bool, Error> {
        search.reset();
        for field in &self.fields {
            charge(work, clock, 1)?;
            if search.matched() {
                break;
            }
            search.field();
            let mut remaining = field.as_str();
            while !remaining.is_empty() && !search.matched() {
                let mut end = remaining.len().min(4096);
                while !remaining.is_char_boundary(end) {
                    end = end.checked_sub(1).ok_or(Error::State)?;
                }
                let chunk = remaining.get(..end).ok_or(Error::State)?;
                let patterns = search.patterns.len();
                work.charge(
                    clock.monotonic().map_err(Error::Storage)?,
                    Charge {
                        io_bytes: (chunk.len() as u64) * (1 + 2 * patterns as u64),
                        records: 1,
                        ..Charge::default()
                    },
                )
                .map_err(|stop| match stop {
                    crate::admission::work::Stop::Deadline => Error::ExecutionDeadline,
                    _ => Error::StorageCapacity,
                })?;
                for c in chunk.chars() {
                    search.normalized_scalar(c)?;
                }
                charge(work, clock, 0)?;
                remaining = remaining.get(end..).ok_or(Error::State)?;
            }
        }
        charge(work, clock, 0)?;
        Ok(search.matched())
    }

    pub(crate) const MAX_ENCODED: usize = Self::MAX_BYTES + 4 * Self::MAX_FIELDS;

    pub(crate) fn encode(&self) -> Result<Vec<u8>, Error> {
        let length = self
            .bytes
            .checked_add(4 * self.fields.len())
            .ok_or(Error::StorageCapacity)?;
        let mut output = Vec::new();
        output
            .try_reserve_exact(length)
            .map_err(|_| Error::StorageCapacity)?;
        for field in &self.fields {
            let length = u32::try_from(field.len()).map_err(|_| Error::StorageCapacity)?;
            output.extend_from_slice(&length.to_be_bytes());
            output.extend_from_slice(field.as_bytes());
        }
        Ok(output)
    }

    #[cfg(test)]
    fn decode(input: &[u8]) -> Result<Self, Error> {
        Self::decode_charged(input, || Ok(()))
    }

    pub(crate) fn decode_charged(
        mut input: &[u8],
        mut before_field: impl FnMut() -> Result<(), Error>,
    ) -> Result<Self, Error> {
        if input.len() > Self::MAX_ENCODED {
            return Err(Error::State);
        }
        let mut fields = Self::new();
        while !input.is_empty() {
            before_field()?;
            let (prefix, rest) = input.split_at_checked(4).ok_or(Error::State)?;
            let length = u32::from_be_bytes(prefix.try_into().map_err(|_| Error::State)?) as usize;
            let (bytes, rest) = rest.split_at_checked(length).ok_or(Error::State)?;
            let text = std::str::from_utf8(bytes).map_err(|_| Error::State)?;
            fields.bytes = fields
                .bytes
                .checked_add(length)
                .filter(|n| *n <= Self::MAX_BYTES)
                .ok_or(Error::State)?;
            if fields.fields.len() == Self::MAX_FIELDS {
                return Err(Error::State);
            }
            let mut field = String::new();
            field
                .try_reserve_exact(length)
                .map_err(|_| Error::StorageCapacity)?;
            field.push_str(text);
            fields
                .fields
                .try_reserve(1)
                .map_err(|_| Error::StorageCapacity)?;
            fields.fields.push(field);
            input = rest;
        }
        Ok(fields)
    }
}
impl Sink for SearchFields {
    fn reset(&mut self) {
        self.fields.clear();
        self.bytes = 0;
        self.representation_full = false;
        self.field();
    }
    fn field(&mut self) {
        self.new_field = true;
        self.whitespace = false;
    }
    fn scalar(&mut self, mut c: char, work: &mut Meter, clock: &dyn Clock) -> Result<(), Error> {
        charge(work, clock, 1)?;
        if space(c) {
            if self.whitespace {
                return Ok(());
            }
            c = ' ';
            self.whitespace = true;
        } else {
            self.whitespace = false;
        }
        let c = lower(c)?;
        let bytes = self
            .bytes
            .checked_add(c.len_utf8())
            .ok_or(Error::StorageCapacity)?;
        if bytes > Self::MAX_BYTES || (self.new_field && self.fields.len() == Self::MAX_FIELDS) {
            self.representation_full = true;
            return Err(Error::StorageCapacity);
        }
        work.charge(
            clock.monotonic().map_err(Error::Storage)?,
            Charge {
                output_bytes: c.len_utf8() as u64,
                ..Charge::default()
            },
        )
        .map_err(|stop| match stop {
            crate::admission::work::Stop::Deadline => Error::ExecutionDeadline,
            _ => Error::StorageCapacity,
        })?;
        if self.new_field {
            self.fields
                .try_reserve(1)
                .map_err(|_| Error::StorageCapacity)?;
            self.fields.push(String::new());
            self.new_field = false;
        }
        let field = self.fields.last_mut().ok_or(Error::State)?;
        field
            .try_reserve(c.len_utf8())
            .map_err(|_| Error::StorageCapacity)?;
        field.push(c);
        self.bytes = bytes;
        Ok(())
    }
}
struct Pattern {
    text: Vec<char>,
    fallback: Vec<usize>,
    position: usize,
    found: bool,
}
pub(super) struct Search {
    patterns: Vec<Pattern>,
    whitespace: bool,
}
impl Sink for Search {
    fn reset(&mut self) {
        Search::reset(self);
    }
    fn field(&mut self) {
        Search::field(self);
    }
    fn scalar(&mut self, c: char, work: &mut Meter, clock: &dyn Clock) -> Result<(), Error> {
        Search::scalar(self, c, work, clock)
    }
}
impl Search {
    /// One literal term is a necessary condition; the field matcher decides
    /// the full query. NUL cannot enter PostgreSQL text parameters.
    pub(super) fn candidate_pattern(&self) -> Result<Option<String>, Error> {
        let Some(pattern) = self
            .patterns
            .iter()
            // Guarantee at least one extractable trigram even in a C locale.
            .filter(|pattern| {
                !pattern.text.contains(&'\0')
                    && pattern
                        .text
                        .windows(3)
                        .any(|window| window.iter().all(char::is_ascii_alphanumeric))
            })
            .max_by_key(|pattern| pattern.text.len())
        else {
            return Ok(None);
        };
        let size = pattern
            .text
            .iter()
            .try_fold(2usize, |size, c| {
                size.checked_add(c.len_utf8() + usize::from(matches!(c, '%' | '_' | '\\')))
            })
            .ok_or(Error::StorageCapacity)?;
        let mut output = String::new();
        output
            .try_reserve_exact(size)
            .map_err(|_| Error::StorageCapacity)?;
        output.push('%');
        for c in &pattern.text {
            if matches!(c, '%' | '_' | '\\') {
                output.push('\\');
            }
            output.push(*c);
        }
        output.push('%');
        Ok(Some(output))
    }
    pub(super) fn parse(text: &str) -> Result<Self, Error> {
        if text.len() > 4096 {
            return Err(Error::UnsupportedFilter);
        }
        let mut patterns = Vec::new();
        patterns
            .try_reserve_exact(64)
            .map_err(|_| Error::StorageCapacity)?;
        let mut rest = text;
        while !rest.is_empty() {
            rest = rest.trim_start_matches(space);
            if rest.is_empty() {
                break;
            }
            if patterns.len() == 64 {
                return Err(Error::UnsupportedFilter);
            }
            let first = rest.chars().next().ok_or(Error::State)?;
            let mut end = None;
            if matches!(first, '\'' | '"') {
                let mut escaped = false;
                for (i, c) in rest.char_indices().skip(1) {
                    if !escaped && c == first {
                        end = Some(i);
                        break;
                    }
                    escaped = !escaped && c == '\\';
                }
            }
            let (word, quoted) = if let Some(end) = end {
                let word = rest.get(1..end).ok_or(Error::State)?;
                rest = rest.get(end + 1..).ok_or(Error::State)?;
                if rest.chars().next().is_some_and(|c| !space(c)) {
                    return Err(Error::UnsupportedFilter);
                }
                (word, true)
            } else {
                let end = rest.find(space).unwrap_or(rest.len());
                let word = rest.get(..end).ok_or(Error::State)?;
                rest = rest.get(end..).ok_or(Error::State)?;
                (word, false)
            };
            let mut chars = Vec::new();
            chars
                .try_reserve_exact(word.len())
                .map_err(|_| Error::StorageCapacity)?;
            let mut input = word.chars();
            let mut whitespace = false;
            while let Some(mut c) = input.next() {
                if quoted && c == '\\' {
                    c = input.next().ok_or(Error::UnsupportedFilter)?;
                    if !matches!(c, '\\' | '\'' | '"') {
                        return Err(Error::UnsupportedFilter);
                    }
                } else if quoted && matches!(c, '\'' | '"') {
                    return Err(Error::UnsupportedFilter);
                }
                if space(c) {
                    if whitespace {
                        continue;
                    }
                    c = ' ';
                    whitespace = true;
                } else {
                    whitespace = false;
                }
                chars.push(lower(c)?);
            }
            let mut fallback = Vec::new();
            fallback
                .try_reserve_exact(chars.len())
                .map_err(|_| Error::StorageCapacity)?;
            fallback.resize(chars.len(), 0);
            let mut prefix = 0;
            for i in 1..chars.len() {
                let c = chars.get(i).ok_or(Error::State)?;
                while prefix != 0 && chars.get(prefix) != Some(c) {
                    prefix = *fallback.get(prefix - 1).ok_or(Error::State)?;
                }
                if chars.get(prefix) == Some(c) {
                    prefix += 1;
                }
                *fallback.get_mut(i).ok_or(Error::State)? = prefix;
            }
            patterns.push(Pattern {
                found: chars.is_empty(),
                text: chars,
                fallback,
                position: 0,
            });
        }
        Ok(Self {
            patterns,
            whitespace: false,
        })
    }
    pub(super) fn reset(&mut self) {
        for p in &mut self.patterns {
            p.found = p.text.is_empty();
        }
        self.field();
    }
    pub(super) fn field(&mut self) {
        for p in &mut self.patterns {
            p.position = 0;
        }
        self.whitespace = false;
    }
    pub(super) fn matched(&self) -> bool {
        self.patterns.iter().all(|p| p.found)
    }
    pub(super) fn scalar(
        &mut self,
        mut c: char,
        work: &mut Meter,
        clock: &dyn Clock,
    ) -> Result<(), Error> {
        // KMP fallback is amortized linear per pattern and input field.
        charge(work, clock, 1 + 2 * self.patterns.len() as u64)?;
        if space(c) {
            if self.whitespace {
                return Ok(());
            }
            c = ' ';
            self.whitespace = true;
        } else {
            self.whitespace = false;
        }
        let c = lower(c)?;
        self.normalized_scalar(c)
    }
    fn normalized_scalar(&mut self, c: char) -> Result<(), Error> {
        for p in &mut self.patterns {
            if p.found {
                continue;
            }
            while p.position != 0 && p.text.get(p.position) != Some(&c) {
                p.position = *p.fallback.get(p.position - 1).ok_or(Error::State)?;
            }
            if p.text.get(p.position) == Some(&c) {
                p.position += 1;
            }
            if p.position == p.text.len() {
                p.found = true;
            }
        }
        Ok(())
    }
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn search_operand(&mut self, item: &super::push::Item) -> Result<Search, Error> {
        if item.kind != scan::Kind::String {
            return Err(Error::InvalidArguments);
        }
        let length = usize::try_from(
            item.range
                .end
                .checked_sub(item.range.start)
                .ok_or(Error::State)?,
        )
        .map_err(|_| Error::UnsupportedFilter)?;
        if length > 6 * 4096 + 2 {
            return Err(Error::UnsupportedFilter);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| Error::StorageCapacity)?;
        bytes.resize(length, 0);
        self.check()?;
        self.retention
            .read_at(item.range.start, &mut bytes)
            .map_err(Error::Storage)?;
        let mut tokens = [arena::Token::EMPTY; 1];
        let document = arena::parse(
            &mut bytes,
            &mut tokens,
            arena::ParseLimits {
                max_bytes: 6 * 4096 + 2,
                max_depth: 1,
                max_tokens: 1,
            },
        )
        .map_err(|_| Error::InvalidArguments)?;
        let text = std::str::from_utf8(document.root().as_string_bytes().ok_or(Error::State)?)
            .map_err(|_| Error::State)?;
        let result = Search::parse(text);
        self.check()?;
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[test]
    fn index_pattern_is_one_normalized_literal_term() {
        let search = super::Search::parse("short \"LONG PHRASE\"").unwrap();
        assert_eq!(
            search.candidate_pattern().unwrap(),
            Some("%long phrase%".into())
        );
        let search = super::Search::parse("αΒΓ 100%_\\literal").unwrap();
        assert_eq!(
            search.candidate_pattern().unwrap(),
            Some("%100\\%\\_\\\\literal%".into())
        );
        assert_eq!(
            super::Search::parse("αΒΓ")
                .unwrap()
                .candidate_pattern()
                .unwrap(),
            None
        );
        for term in ["", "ab", "ab\0cd", "%%%", "a_b", "αβγ"] {
            assert!(super::Search::parse(term)
                .unwrap()
                .candidate_pattern()
                .unwrap()
                .is_none());
        }
        assert_eq!(
            super::Search::parse("ab\0cd needle")
                .unwrap()
                .candidate_pattern()
                .unwrap(),
            Some("%needle%".into())
        );
    }
    use super::*;
    struct Fixed;
    impl Clock for Fixed {
        fn sample(&self) -> Result<crate::ports::Time, crate::ports::Error> {
            Ok(crate::ports::Time {
                utc_ms: 0,
                monotonic: crate::ports::Tick(1),
            })
        }
    }
    fn work() -> Meter {
        Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(100)),
            Charge {
                records: 1_000_000,
                ..Charge::default()
            },
        )
    }
    #[test]
    fn retained_field_encoding_rejects_incomplete_or_unbounded_values() {
        let encoded = b"\0\0\0\x05caf\xc3\xa9\0\0\0\x04tail";
        let fields = SearchFields::decode(encoded).unwrap();
        assert_eq!(fields.fields().collect::<Vec<_>>(), ["café", "tail"]);
        assert_eq!(fields.encode().unwrap(), encoded);
        let mut count = 0;
        assert!(SearchFields::decode_charged(encoded, || {
            count += 1;
            if count == 2 {
                Err(Error::StorageCapacity)
            } else {
                Ok(())
            }
        })
        .is_err());
        assert_eq!(count, 2);
        assert!(SearchFields::decode(&[]).unwrap().fields().next().is_none());
        for bad in [&b"\0"[..], &b"\0\0\0\x04abc"[..], &b"\0\0\0\x01\xff"[..]] {
            assert!(SearchFields::decode(bad).is_err());
        }
        assert!(matches!(
            SearchFields::decode(&vec![0; 4 * (SearchFields::MAX_FIELDS + 1)]),
            Err(Error::State)
        ));
        assert!(matches!(
            SearchFields::decode(&vec![0; SearchFields::MAX_ENCODED + 1]),
            Err(Error::State)
        ));
    }

    #[test]
    fn retained_fields_preserve_boundaries_unicode_and_bounded_matching_work() {
        let mut fields = SearchFields::new();
        let mut build = Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(100)),
            Charge {
                records: 100_000_000,
                output_bytes: 16 * 1024 * 1024,
                ..Charge::default()
            },
        );
        fields
            .text("CAFÉ\r\n\t harvest", &mut build, &Fixed)
            .unwrap();
        fields.text("Orchard", &mut build, &Fixed).unwrap();
        let large = format!("{} tailneedle", "x".repeat(1024 * 1024));
        fields.text(&large, &mut build, &Fixed).unwrap();
        assert_eq!(fields.fields().next(), Some("café harvest"));
        let mut query = Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(100)),
            Charge {
                records: 2_000_000,
                io_bytes: 1024 * 1024 * 1024,
                ..Charge::default()
            },
        );
        for _ in 0..40 {
            assert!(fields
                .matches("CAFÉ tailneedle", &mut query, &Fixed)
                .unwrap());
            assert!(!fields.matches("absent", &mut query, &Fixed).unwrap());
        }
        assert!(query.remaining().records > 1_900_000);
        assert!(fields
            .matches("\"café harvest\" orchard", &mut query, &Fixed)
            .unwrap());
        assert!(!fields
            .matches("\"harvest orchard\"", &mut query, &Fixed)
            .unwrap());
        assert!(!fields.matches("cafe\u{301}", &mut query, &Fixed).unwrap());
        assert_eq!(
            fields.matches("x", &mut work(), &Fixed),
            Err(Error::StorageCapacity)
        );
        assert_eq!(
            fields.matches(&"x".repeat(4097), &mut work(), &Fixed),
            Err(Error::UnsupportedFilter)
        );
    }

    #[test]
    fn retained_field_capacity_and_expired_work_are_explicit() {
        let mut fields = SearchFields::new();
        let mut build = Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(100)),
            Charge {
                records: 100_000_000,
                output_bytes: 16 * 1024 * 1024,
                ..Charge::default()
            },
        );
        fields.bytes = SearchFields::MAX_BYTES;
        assert_eq!(
            fields.scalar('x', &mut build, &Fixed),
            Err(Error::StorageCapacity)
        );
        assert!(fields.fields().next().is_none());
        assert!(fields.representation_full());
        fields.reset();
        assert!(!fields.representation_full());
        assert_eq!(
            fields.scalar('x', &mut work(), &Fixed),
            Err(Error::StorageCapacity)
        );
        assert!(!fields.representation_full());
        for _ in 0..SearchFields::MAX_FIELDS {
            fields.text("x", &mut build, &Fixed).unwrap();
        }
        assert_eq!(
            fields.text("x", &mut build, &Fixed),
            Err(Error::StorageCapacity)
        );
        assert!(fields.representation_full());
        let mut expired = Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(1)),
            Charge::default(),
        );
        assert_eq!(
            fields.matches("", &mut expired, &Fixed),
            Err(Error::ExecutionDeadline)
        );
    }

    #[test]
    fn tokens_span_fields_but_phrases_do_not_and_matching_is_not_normalization() {
        let mut search = Search::parse("GARDENER \"café harvest\"").unwrap();
        search.text("Gardener", &mut work(), &Fixed).unwrap();
        search.text("café", &mut work(), &Fixed).unwrap();
        search.text("harvest", &mut work(), &Fixed).unwrap();
        assert!(!search.matched());
        search
            .text("CAFÉ\r\n\t harvest", &mut work(), &Fixed)
            .unwrap();
        assert!(search.matched());
        search.reset();
        search
            .text("gardener cafe\u{301} harvest", &mut work(), &Fixed)
            .unwrap();
        assert!(!search.matched());
    }
    #[test]
    fn overlapping_prefixes_quotes_empty_terms_and_limits() {
        for (query, value) in [
            ("ababac", "ababababac"),
            ("\"a\\\"b\"", "a\"b"),
            ("can't", "can't"),
            ("'unclosed", "'unclosed"),
            ("\"\"", ""),
            ("", ""),
        ] {
            let mut search = Search::parse(query).unwrap();
            search.text(value, &mut work(), &Fixed).unwrap();
            assert!(search.matched(), "{query}");
        }
        for query in [
            "\"a\"tail".to_owned(),
            "\"a\\qb\"".to_owned(),
            "x ".repeat(65),
            "x".repeat(4097),
        ] {
            assert!(
                matches!(Search::parse(&query), Err(Error::UnsupportedFilter)),
                "{query}"
            );
        }
        assert!(Search::parse(&"x".repeat(4096)).is_ok());
    }
    #[test]
    fn matching_spends_the_original_work_and_deadline() {
        let mut search = Search::parse("x").unwrap();
        let mut exhausted = Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(100)),
            Charge::default(),
        );
        assert_eq!(
            search.text("x", &mut exhausted, &Fixed),
            Err(Error::StorageCapacity)
        );
        let mut expired = Meter::new(
            td_mime::time::Deadline::at(crate::ports::Tick(1)),
            Charge {
                records: 100,
                ..Charge::default()
            },
        );
        assert_eq!(
            search.text("x", &mut expired, &Fixed),
            Err(Error::ExecutionDeadline)
        );
        assert!(!search.matched());
    }
}
