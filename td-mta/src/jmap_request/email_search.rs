//! Bounded text operands and streaming substring matches across mail fields.
use super::{Error, Worker};
use crate::{
    admission::work::{Charge, Meter},
    ports::Clock,
};
use td_json::{arena, scan};

pub(super) fn charge(work: &mut Meter, clock: &dyn Clock, records: u64) -> Result<(), Error> {
    work.charge(
        clock.sample().map_err(Error::Storage)?.monotonic,
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
impl Search {
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
    pub(super) fn text(
        &mut self,
        text: &str,
        work: &mut Meter,
        clock: &dyn Clock,
    ) -> Result<(), Error> {
        self.field();
        for c in text.chars() {
            self.scalar(c, work, clock)?;
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
