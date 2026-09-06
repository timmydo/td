//! Explicit, bounded English word-list checking. No I/O, clocks or triggers.

use crate::model::{Editor, RevisionPoint, TabId};
use crate::{Error, Result};
use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::Arc;

pub const DICTIONARY_BYTES: usize = 16 * 1024 * 1024;
pub const DICTIONARY_ENTRIES: usize = 250_000;
pub const WORD_SCALARS: usize = 64;
pub const STEP_SCALARS: usize = 4096;
pub const MARKS: usize = 10_000;

pub struct Dictionary {
    identity: Arc<()>,
    words: Vec<String>,
}

impl Dictionary {
    /// Parse caller-supplied data; a failed replacement cannot mutate an old list.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > DICTIONARY_BYTES {
            return Err(Error::Limit);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Error::InvalidText)?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut words = BTreeSet::new();
        for record in text.split_inclusive('\n') {
            let line = if let Some(line) = record.strip_suffix('\n') {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                record
            };
            if line.is_empty() {
                continue;
            }
            if !line.is_ascii() {
                return Err(Error::InvalidText);
            }
            if line.len() > WORD_SCALARS {
                return Err(Error::Limit);
            }
            for (at, byte) in line.bytes().enumerate() {
                if !byte.is_ascii_alphabetic()
                    && !(byte == b'\''
                        && at
                            .checked_sub(1)
                            .and_then(|i| line.as_bytes().get(i))
                            .is_some_and(u8::is_ascii_alphabetic)
                        && line
                            .as_bytes()
                            .get(at + 1)
                            .is_some_and(u8::is_ascii_alphabetic))
                {
                    return Err(Error::InvalidText);
                }
            }
            let word = line.to_ascii_lowercase();
            if words.len() == DICTIONARY_ENTRIES {
                if !words.contains(&word) {
                    return Err(Error::Limit);
                }
                continue;
            }
            words.insert(word);
        }
        if words.is_empty() {
            return Err(Error::InvalidText);
        }
        Ok(Self {
            identity: Arc::new(()),
            words: words.into_iter().collect(),
        })
    }

    pub fn entry_count(&self) -> usize {
        self.words.len()
    }

    fn contains(&self, normalized: &str) -> bool {
        self.words
            .binary_search_by(|word| word.as_str().cmp(normalized))
            .is_ok()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub checked: usize,
    pub unknown: usize,
    pub skipped: usize,
    pub truncated: bool,
}

pub struct Report {
    point: RevisionPoint,
    dictionary: Arc<()>,
    counts: Counts,
    marks: Vec<Range<usize>>,
}

impl Report {
    fn validate(&self, editor: &Editor, dictionary: &Dictionary) -> Result<()> {
        editor.check_revision(&self.point)?;
        if !Arc::ptr_eq(&self.dictionary, &dictionary.identity) {
            return Err(Error::StaleRevision);
        }
        Ok(())
    }

    pub fn tab(&self) -> TabId {
        self.point.tab
    }
    pub fn revision(&self) -> u64 {
        self.point.revision
    }

    /// No stale marks or counts may be reused after an edit or list replacement.
    pub fn results<'a>(
        &'a self,
        editor: &Editor,
        dictionary: &Dictionary,
    ) -> Result<(Counts, &'a [Range<usize>])> {
        self.validate(editor, dictionary)?;
        Ok((self.counts, &self.marks))
    }
}

#[derive(Default)]
struct Token {
    start: usize,
    end: usize,
    scalars: usize,
    skipped: bool,
    last_letter: bool,
    normalized: String,
}

impl Token {
    fn push(&mut self, at: usize, c: char) {
        if self.scalars == 0 {
            self.start = at;
        }
        self.end = at + c.len_utf8();
        self.scalars += 1;
        self.last_letter = c.is_alphabetic();
        self.skipped |= self.scalars > WORD_SCALARS
            || !(c.is_ascii_alphabetic() || matches!(c, '\'' | '\u{2019}'));
        if !self.skipped {
            self.normalized.push(if c == '\u{2019}' {
                '\''
            } else {
                c.to_ascii_lowercase()
            });
        }
    }

    fn finish(&mut self, dictionary: &Dictionary, report: &mut Report, limit: usize) {
        if self.scalars == 0 {
            return;
        }
        if self.skipped {
            report.counts.skipped += 1;
        } else {
            report.counts.checked += 1;
            if !dictionary.contains(&self.normalized) {
                report.counts.unknown += 1;
                if report.marks.len() < limit {
                    report.marks.push(self.start..self.end);
                } else {
                    report.counts.truncated = true;
                }
            }
        }
        self.scalars = 0;
        self.skipped = false;
        self.last_letter = false;
        self.normalized.clear();
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    Pending,
    Complete,
    Failed,
}

/// One caller-driven scan. Dropping cancels it; no partial results are exposed.
pub struct Scan {
    report: Report,
    offset: usize,
    token: Token,
    state: State,
    mark_limit: usize,
}

impl Scan {
    pub fn begin(
        editor: &Editor,
        tab: TabId,
        revision: u64,
        dictionary: &Dictionary,
    ) -> Result<Self> {
        Self::limited(editor, tab, revision, dictionary, MARKS)
    }

    fn limited(
        editor: &Editor,
        tab: TabId,
        revision: u64,
        dictionary: &Dictionary,
        mark_limit: usize,
    ) -> Result<Self> {
        if mark_limit > MARKS {
            return Err(Error::Limit);
        }
        Ok(Self {
            report: Report {
                point: editor.revision_point(tab, revision)?,
                dictionary: dictionary.identity.clone(),
                counts: Counts::default(),
                marks: Vec::with_capacity(mark_limit),
            },
            offset: 0,
            token: Token {
                normalized: String::with_capacity(WORD_SCALARS),
                ..Token::default()
            },
            state: State::Pending,
            mark_limit,
        })
    }

    /// Consume at most STEP_SCALARS; a tab switch alone does not cancel work.
    pub fn step(&mut self, editor: &Editor, dictionary: &Dictionary) -> Result<bool> {
        if self.state == State::Failed {
            return Err(Error::InvalidArgument);
        }
        let result = self.advance(editor, dictionary);
        if result.is_err() {
            self.state = State::Failed;
            self.report.marks.clear();
            self.report.counts = Counts::default();
            self.token = Token::default();
        }
        result
    }

    fn advance(&mut self, editor: &Editor, dictionary: &Dictionary) -> Result<bool> {
        self.report.validate(editor, dictionary)?;
        if self.state == State::Complete {
            return Ok(true);
        }
        let text = editor.document(self.report.point.tab)?.text();
        let remaining = text.get(self.offset..).ok_or(Error::InvalidPosition)?;
        let base = self.offset;
        let mut chars = remaining.char_indices().peekable();
        for _ in 0..STEP_SCALARS {
            let Some((relative, c)) = chars.next() else {
                break;
            };
            let at = base + relative;
            self.offset = at + c.len_utf8();
            // Unicode letters keep unsupported contractions one skipped token.
            let internal_apostrophe = matches!(c, '\'' | '\u{2019}')
                && self.token.last_letter
                && chars.peek().is_some_and(|(_, next)| next.is_alphabetic());
            if c.is_alphanumeric() || internal_apostrophe {
                self.token.push(at, c);
            } else {
                self.token
                    .finish(dictionary, &mut self.report, self.mark_limit);
            }
        }
        if self.offset == text.len() {
            self.token
                .finish(dictionary, &mut self.report, self.mark_limit);
            self.state = State::Complete;
        }
        Ok(self.state == State::Complete)
    }

    pub fn finish(self, editor: &Editor, dictionary: &Dictionary) -> Result<Report> {
        if self.state != State::Complete {
            return Err(Error::InvalidArgument);
        }
        self.report.validate(editor, dictionary)?;
        Ok(self.report)
    }
}

/// Window ownership: one dictionary, one scan, one shared mark budget.
#[derive(Default)]
pub(crate) struct WindowState {
    dictionary: Option<Dictionary>,
    reports: std::collections::BTreeMap<TabId, Report>,
    scan: Option<Scan>,
}

impl WindowState {
    pub(crate) fn install(&mut self, dictionary: Dictionary) {
        self.scan = None;
        self.reports.clear();
        self.dictionary = Some(dictionary);
    }

    pub(crate) fn cancel(&mut self) -> bool {
        self.scan.take().is_some()
    }

    pub(crate) fn running(&self) -> bool {
        self.scan.is_some()
    }

    pub(crate) fn dictionary_entries(&self) -> Option<usize> {
        self.dictionary.as_ref().map(Dictionary::entry_count)
    }

    pub(crate) fn checking_active(&self, editor: &Editor) -> bool {
        self.scan.as_ref().is_some_and(|scan| {
            editor.active() == Some(scan.report.tab())
                && self
                    .dictionary
                    .as_ref()
                    .is_some_and(|dictionary| scan.report.validate(editor, dictionary).is_ok())
        })
    }

    fn current(&self, editor: &Editor) -> Option<(Counts, &[Range<usize>])> {
        if self.checking_active(editor) {
            return None;
        }
        self.reports
            .get(&editor.active()?)?
            .results(editor, self.dictionary.as_ref()?)
            .ok()
    }

    pub(crate) fn observe(&mut self, editor: &Editor) -> bool {
        let Some(dictionary) = &self.dictionary else {
            return false;
        };
        let before = self.reports.len();
        self.reports
            .retain(|_, report| report.validate(editor, dictionary).is_ok());
        let stale = self
            .scan
            .as_ref()
            .is_some_and(|scan| scan.report.validate(editor, dictionary).is_err());
        if stale {
            self.scan = None;
        }
        stale || self.reports.len() != before
    }

    pub(crate) fn start(&mut self, editor: &Editor, tab: TabId, revision: u64) -> Result<bool> {
        self.observe(editor);
        editor.revision_point(tab, revision)?;
        let Some(dictionary) = &self.dictionary else {
            return Ok(false);
        };
        let used: usize = self
            .reports
            .iter()
            .filter(|(id, _)| **id != tab)
            .map(|(_, report)| report.marks.len())
            .sum();
        let remaining = MARKS.checked_sub(used).ok_or(Error::Limit)?;
        let scan = Scan::limited(editor, tab, revision, dictionary, remaining)?;
        self.reports.remove(&tab);
        self.scan = Some(scan);
        Ok(true)
    }

    /// Exactly one chunk, even if a timer or input batch contains many events.
    pub(crate) fn step(&mut self, editor: &Editor) -> Result<bool> {
        let changed = self.observe(editor);
        let Some(scan) = self.scan.as_mut() else {
            return Ok(changed);
        };
        let dictionary = self.dictionary.as_ref().ok_or(Error::InvalidArgument)?;
        let complete = match scan.step(editor, dictionary) {
            Ok(complete) => complete,
            Err(error) => {
                self.scan = None;
                return Err(error);
            }
        };
        if !complete {
            return Ok(changed);
        }
        let scan = self.scan.take().ok_or(Error::InvalidArgument)?;
        let mut report = scan.finish(editor, dictionary)?;
        report.marks.shrink_to_fit();
        self.reports.insert(report.tab(), report);
        Ok(true)
    }

    pub(crate) fn view<'a>(&'a self, editor: &Editor) -> (String, &'a [Range<usize>]) {
        if self.dictionary.is_none() {
            return ("Spelling: no dictionary".into(), &[]);
        }
        if editor.active().is_none() {
            return ("Spelling: no document".into(), &[]);
        }
        if self.checking_active(editor) {
            return ("Spelling: checking (Escape cancels)".into(), &[]);
        }
        match self.current(editor) {
            Some((counts, marks)) => (
                format!(
                    "Spelling: {} unknown / {} checked; {} skipped{}",
                    counts.unknown,
                    counts.checked,
                    counts.skipped,
                    if counts.truncated {
                        "; marks capped"
                    } else {
                        ""
                    },
                ),
                marks,
            ),
            None => ("Spelling: not checked".into(), &[]),
        }
    }

    pub(crate) fn select(&self, editor: &Editor, previous: bool) -> Result<Option<Range<usize>>> {
        let tab = editor.active().ok_or(Error::MissingTab)?;
        let selection = editor.document(tab)?.selection().range();
        let marks = self.current(editor).map_or(&[][..], |(_, marks)| marks);
        Ok(if previous {
            marks
                .partition_point(|range| range.end <= selection.start)
                .checked_sub(1)
                .and_then(|index| marks.get(index))
        } else {
            marks.get(marks.partition_point(|range| range.start < selection.end))
        }
        .cloned())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{Command, Selection};
    use crate::ui::{Controller, Event};

    fn document(text: &str) -> Controller {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(text.as_bytes())).unwrap();
        ui
    }

    fn complete(ui: &Controller, dictionary: &Dictionary) -> Report {
        let mut scan = Scan::begin(ui.editor(), 1, 0, dictionary).unwrap();
        while !scan.step(ui.editor(), dictionary).unwrap() {}
        scan.finish(ui.editor(), dictionary).unwrap()
    }

    fn finish_window(state: &mut WindowState, ui: &Controller) {
        let mut turns = 0;
        while state.running() {
            state.step(ui.editor()).unwrap();
            turns += 1;
            assert!(turns < 1000);
        }
    }

    #[test]
    fn window_scans_are_explicit_atomic_cancellable_and_revision_bound() {
        let mut ui = document(&format!("{}known wrong", " ".repeat(STEP_SCALARS)));
        let mut state = WindowState::default();
        assert!(!state.start(ui.editor(), 1, 0).unwrap());
        assert!(state.view(ui.editor()).0.contains("no dictionary"));
        state.install(Dictionary::parse(b"known").unwrap());
        assert!(!state.running());
        assert!(state.start(ui.editor(), 1, 0).unwrap());
        assert!(!state.step(ui.editor()).unwrap());
        assert!(state.view(ui.editor()).1.is_empty());
        assert!(state.view(ui.editor()).0.contains("checking"));
        assert!(state.cancel());
        assert!(!state.cancel());
        assert!(state.view(ui.editor()).0.contains("not checked"));
        state.start(ui.editor(), 1, 0).unwrap();
        finish_window(&mut state, &ui);
        assert_eq!(
            state.view(ui.editor()).1,
            std::slice::from_ref(&(STEP_SCALARS + 6..STEP_SCALARS + 11))
        );
        assert!(state.view(ui.editor()).0.contains("1 unknown / 2 checked"));
        assert_eq!(state.start(ui.editor(), 1, 99), Err(Error::StaleRevision));
        assert_eq!(state.view(ui.editor()).1.len(), 1);
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 0,
                caret: 0,
            }),
        })
        .unwrap();
        assert!(!state.observe(ui.editor()));
        let range = state.select(ui.editor(), false).unwrap().unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: range.start,
                caret: range.end,
            }),
        })
        .unwrap();
        assert!(state.select(ui.editor(), false).unwrap().is_none());
        assert!(state.select(ui.editor(), true).unwrap().is_none());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("changed".into()),
        })
        .unwrap();
        assert!(state.view(ui.editor()).1.is_empty());
        assert!(state.observe(ui.editor()));
        assert!(!state.running());
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Undo,
        })
        .unwrap();
        assert!(state.view(ui.editor()).1.is_empty());
        state.start(ui.editor(), 1, 2).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 2,
            command: Command::Insert("x".into()),
        })
        .unwrap();
        assert!(state.step(ui.editor()).unwrap());
        assert!(!state.running());
    }

    #[test]
    fn window_budget_counts_all_tabs_and_keeps_nonactive_results() {
        let mut ui = document(&"wrong ".repeat(MARKS));
        let mut state = WindowState::default();
        state.install(Dictionary::parse(b"known").unwrap());
        state.start(ui.editor(), 1, 0).unwrap();
        finish_window(&mut state, &ui);
        assert_eq!(state.view(ui.editor()).1.len(), MARKS);
        ui.dispatch(Event::Load(b"also wrong")).unwrap();
        state.start(ui.editor(), 2, 0).unwrap();
        ui.dispatch(Event::SelectTab(1)).unwrap();
        finish_window(&mut state, &ui);
        assert_eq!(state.view(ui.editor()).1.len(), MARKS);
        ui.dispatch(Event::SelectTab(2)).unwrap();
        let (status, marks) = state.view(ui.editor());
        assert!(marks.is_empty());
        assert!(status.contains("2 unknown / 2 checked"));
        assert!(status.contains("marks capped"));
        assert_eq!(
            state.reports.values().map(|r| r.marks.len()).sum::<usize>(),
            MARKS
        );
        ui.dispatch(Event::Close {
            tab: 1,
            revision: 0,
        })
        .unwrap();
        state.start(ui.editor(), 2, 0).unwrap();
        finish_window(&mut state, &ui);
        assert_eq!(state.view(ui.editor()).1, &[0..4, 5..10]);
        assert!(!state.view(ui.editor()).0.contains("marks capped"));
        state.install(Dictionary::parse(b"known").unwrap());
        assert!(state.view(ui.editor()).1.is_empty());
        assert!(state.reports.is_empty());
    }

    #[test]
    fn dictionary_is_strict_bounded_case_folded_deduplicated_data() {
        let dictionary = Dictionary::parse("\u{feff}Word\r\n\nWORD\ncan't".as_bytes()).unwrap();
        assert_eq!(dictionary.entry_count(), 2);
        assert!(dictionary.contains("word"));
        assert!(dictionary.contains("can't"));
        assert!(!dictionary.contains("Word"));
        for bytes in [
            b"".as_slice(),
            b"\n\r\n",
            b" word",
            b"word ",
            b"word\r",
            b"ab\rcd",
            b"a1",
            b"'word",
            b"word'",
            b"a''b",
            b"a_b",
            b"\xff",
            "naïve".as_bytes(),
            "can’t".as_bytes(),
            "a\n\u{feff}b".as_bytes(),
        ] {
            assert!(Dictionary::parse(bytes).is_err(), "{bytes:?}");
        }
        assert!(Dictionary::parse(&[b'a'; WORD_SCALARS]).is_ok());
        assert!(matches!(
            Dictionary::parse(&[b'a'; WORD_SCALARS + 1]),
            Err(Error::Limit)
        ));
        assert!(matches!(
            Dictionary::parse("é".repeat(WORD_SCALARS).as_bytes()),
            Err(Error::InvalidText)
        ));
        assert!(matches!(
            Dictionary::parse(&vec![b'\n'; DICTIONARY_BYTES + 1]),
            Err(Error::Limit)
        ));
        let mut exact = vec![b'\n'; DICTIONARY_BYTES];
        *exact.first_mut().unwrap() = b'a';
        assert_eq!(Dictionary::parse(&exact).unwrap().entry_count(), 1);
        assert!(dictionary.contains("word")); // malformed replacement has no mutation path
    }

    #[test]
    fn distinct_entry_limit_does_not_charge_duplicates() {
        let mut bytes = Vec::new();
        for mut value in 0..DICTIONARY_ENTRIES {
            bytes.push(b'w');
            for _ in 0..5 {
                bytes.push(b'a' + (value % 26) as u8);
                value /= 26;
            }
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(b"WAAAAA\n");
        assert_eq!(
            Dictionary::parse(&bytes).unwrap().entry_count(),
            DICTIONARY_ENTRIES
        );
        bytes.extend_from_slice(b"different\n");
        assert!(matches!(Dictionary::parse(&bytes), Err(Error::Limit)));
    }

    #[test]
    fn tokens_follow_the_explicit_ascii_profile_without_editing() {
        let dictionary = Dictionary::parse(b"can't\nwell\nknown\na\nb\nword").unwrap();
        let ui = document("can't can’t abc123 naïve well-known a_b 'word' a''b naïve's café’s BAD");
        let generation = ui.generation();
        let report = complete(&ui, &dictionary);
        let (counts, marks) = report.results(ui.editor(), &dictionary).unwrap();
        assert_eq!(
            counts,
            Counts {
                checked: 10,
                unknown: 1,
                skipped: 4,
                truncated: false
            }
        );
        assert_eq!(marks.len(), 1);
        assert_eq!(
            ui.editor()
                .document(1)
                .unwrap()
                .text()
                .get(marks.first().unwrap().clone()),
            Some("BAD")
        );
        assert_eq!(ui.generation(), generation);
        assert_eq!(
            ui.editor().document(1).unwrap().selection(),
            Selection::default()
        );
        assert_eq!(ui.editor().document(1).unwrap().history_depth(), (0, 0));
        assert!(!ui.editor().document(1).unwrap().dirty());
        assert_eq!(report.tab(), 1);
        assert_eq!(report.revision(), 0);
    }

    #[test]
    fn chunk_boundary_preserves_internal_apostrophe_and_hides_partial_marks() {
        let dictionary = Dictionary::parse(b"a'b").unwrap();
        let ui = document(&format!("{}a’b bad", " ".repeat(STEP_SCALARS - 2)));
        let mut scan = Scan::begin(ui.editor(), 1, 0, &dictionary).unwrap();
        assert!(!scan.step(ui.editor(), &dictionary).unwrap());
        assert_eq!(scan.offset, STEP_SCALARS + 2); // the curly apostrophe is three UTF-8 bytes
        assert_eq!(scan.token.normalized, "a'");
        assert!(scan.step(ui.editor(), &dictionary).unwrap());
        assert!(scan.step(ui.editor(), &dictionary).unwrap());
        let report = scan.finish(ui.editor(), &dictionary).unwrap();
        let (counts, marks) = report.results(ui.editor(), &dictionary).unwrap();
        assert_eq!(counts.checked, 2);
        assert_eq!(counts.unknown, 1);
        assert_eq!(marks.len(), 1);
        assert!(Scan::begin(ui.editor(), 1, 0, &dictionary)
            .unwrap()
            .finish(ui.editor(), &dictionary)
            .is_err());
    }

    #[test]
    fn oversized_tokens_and_excess_unknowns_finish_with_bounded_storage() {
        let dictionary = Dictionary::parse(b"known").unwrap();
        let text = format!("{} {}", "a".repeat(70_000), "x ".repeat(MARKS + 5));
        let ui = document(&text);
        let mut scan = Scan::begin(ui.editor(), 1, 0, &dictionary).unwrap();
        let mut turns = 0;
        loop {
            let old = scan.offset;
            let done = scan.step(ui.editor(), &dictionary).unwrap();
            assert!(scan.offset - old <= STEP_SCALARS); // ASCII fixture
            assert!(scan.token.normalized.len() <= WORD_SCALARS);
            assert!(scan.report.marks.len() <= MARKS);
            turns += 1;
            if done {
                break;
            }
        }
        assert!(turns > 1);
        let report = scan.finish(ui.editor(), &dictionary).unwrap();
        let (counts, marks) = report.results(ui.editor(), &dictionary).unwrap();
        assert_eq!(
            counts,
            Counts {
                checked: MARKS + 5,
                unknown: MARKS + 5,
                skipped: 1,
                truncated: true
            }
        );
        assert_eq!(marks.len(), MARKS);
        let empty = document("");
        assert_eq!(
            complete(&empty, &dictionary)
                .results(empty.editor(), &dictionary)
                .unwrap()
                .0,
            Counts::default()
        );
    }

    #[test]
    fn sixty_four_scalar_tokens_are_checked_and_sixty_five_are_skipped() {
        let word = "a".repeat(WORD_SCALARS);
        let dictionary = Dictionary::parse(word.as_bytes()).unwrap();
        let ui = document(&format!("{word} {word}a"));
        let report = complete(&ui, &dictionary);
        let (counts, marks) = report.results(ui.editor(), &dictionary).unwrap();
        assert_eq!(
            counts,
            Counts {
                checked: 1,
                unknown: 0,
                skipped: 1,
                truncated: false
            }
        );
        assert!(marks.is_empty());
    }

    #[test]
    fn combining_marks_and_format_characters_are_scalar_token_delimiters() {
        let dictionary = Dictionary::parse(b"known").unwrap();
        for (text, fragments) in [
            ("nai\u{308}ve", ["nai", "ve"]),
            ("re\u{301}sume\u{301}", ["re", "sume"]),
            ("hy\u{ad}phen", ["hy", "phen"]),
            ("a\u{200d}b", ["a", "b"]),
        ] {
            let ui = document(text);
            let report = complete(&ui, &dictionary);
            let (counts, marks) = report.results(ui.editor(), &dictionary).unwrap();
            assert_eq!(counts.checked, 2);
            assert_eq!(counts.unknown, 2);
            assert_eq!(counts.skipped, 0);
            let marked: Vec<_> = marks
                .iter()
                .map(|range| text.get(range.clone()).unwrap())
                .collect();
            assert_eq!(marked, fragments);
        }
        let ui = document("naïve");
        assert_eq!(
            complete(&ui, &dictionary)
                .results(ui.editor(), &dictionary)
                .unwrap()
                .0
                .skipped,
            1
        );
    }

    #[test]
    fn defensive_slice_failure_is_terminal_and_clears_partial_work() {
        let dictionary = Dictionary::parse(b"word").unwrap();
        let ui = document("é");
        let mut scan = Scan::begin(ui.editor(), 1, 0, &dictionary).unwrap();
        scan.offset = 1; // test-only corruption; public API only stores scalar boundaries
        scan.report.counts.checked = 1;
        scan.report.marks.push(0..2);
        assert_eq!(
            scan.step(ui.editor(), &dictionary),
            Err(Error::InvalidPosition)
        );
        assert_eq!(scan.report.counts, Counts::default());
        assert!(scan.report.marks.is_empty());
        scan.offset = 0;
        assert_eq!(
            scan.step(ui.editor(), &dictionary),
            Err(Error::InvalidArgument)
        );
    }

    #[test]
    fn edits_undo_and_dictionary_replacement_invalidate_but_motion_and_tabs_do_not() {
        let dictionary = Dictionary::parse(b"word").unwrap();
        let mut ui = document("bad");
        let report = complete(&ui, &dictionary);
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Select(Selection {
                anchor: 1,
                caret: 2,
            }),
        })
        .unwrap();
        ui.dispatch(Event::New).unwrap();
        assert!(report.results(ui.editor(), &dictionary).is_ok());
        let mut job = Scan::begin(ui.editor(), 1, 0, &dictionary).unwrap();
        assert!(job.step(ui.editor(), &dictionary).unwrap());
        let replacement = Dictionary::parse(b"word").unwrap();
        assert!(report.results(ui.editor(), &replacement).is_err());
        assert_eq!(
            job.step(ui.editor(), &replacement),
            Err(Error::StaleRevision)
        );
        assert_eq!(
            job.step(ui.editor(), &dictionary),
            Err(Error::InvalidArgument)
        );
        let foreign = document("bad");
        assert!(report.results(foreign.editor(), &dictionary).is_err());
        ui.dispatch(Event::SelectTab(1)).unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 0,
            command: Command::Insert("x".into()),
        })
        .unwrap();
        ui.dispatch(Event::Edit {
            tab: 1,
            revision: 1,
            command: Command::Undo,
        })
        .unwrap();
        assert_eq!(ui.editor().document(1).unwrap().text(), "bad");
        assert!(report.results(ui.editor(), &dictionary).is_err());
    }
}
