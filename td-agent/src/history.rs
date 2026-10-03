//! `history_search` and `history_read` (DESIGN.md §12): a conversation's
//! whole log as text, searched or read in pages, including everything
//! compaction later prunes from the model's view. Both run over a log's
//! events: the conversation's own in memory, another's as the store holds
//! it, a final line still being written left out.
//!
//! Every event renders as a header line (`#SEQ KIND TIME`) and its text.
//! The rendering is a pure function of the event, so a cursor into it,
//! a sequence number and a byte offset, means the same thing from one
//! page to the next. A tool result renders whole as the log keeps it. An
//! approval renders as its outcome and who decided it, never Jev's
//! probabilities or the reasoning stage's reason, so that a model cannot
//! tune itself against the classifier.

use crate::client;
use crate::store::{Event, Held, Kind, Role};

/// The most a page may take once escaped as a JSON string, which a log
/// line and a request carry it as: a page is cut shorter rather than
/// let a tool result pass what one log line holds.
pub const MAX_PAGE_ESCAPED: usize = 512 * 1024;
/// The bytes of an excerpt on either side of a match.
const EXCERPT: usize = 120;

/// The kinds of event `history_search` narrows to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Searchable {
    User,
    Orchestrator,
    Conversation,
    Schedule,
    Notice,
    Assistant,
    ToolCall,
    ToolResult,
    Approval,
    Compaction,
}

impl Searchable {
    pub const ALL: [Self; 10] = [
        Self::User,
        Self::Orchestrator,
        Self::Conversation,
        Self::Schedule,
        Self::Notice,
        Self::Assistant,
        Self::ToolCall,
        Self::ToolResult,
        Self::Approval,
        Self::Compaction,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Orchestrator => "orchestrator",
            Self::Conversation => "conversation",
            Self::Schedule => "schedule",
            Self::Notice => "notice",
            Self::Assistant => "assistant",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Approval => "approval",
            Self::Compaction => "compaction",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.word() == word)
    }
}

/// A time, seconds since the epoch, as UTC: `2026-10-02T17:22:05Z`.
pub fn utc(seconds: u64) -> String {
    let days = seconds / 86_400;
    let rest = seconds % 86_400;
    // Howard Hinnant's civil-from-days, over days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// An approval as a model sees it: its outcome and who decided it.
fn approval(outcome: &str, by: &str) -> String {
    format!("{outcome}, decided by {by}")
}

/// A message from another conversation's label, with why it started no
/// turn when it did not.
fn message(
    from: &crate::store::Id,
    role: Role,
    status: Option<&str>,
    held: Option<Held>,
) -> String {
    let label = client::label(from, role, status);
    match held {
        None => label,
        Some(Held::Paused) => format!("{label} (it started no turn: this conversation was paused)"),
        Some(Held::Budget) => {
            format!("{label} (it started no turn: the wake budget was spent)")
        }
    }
}

/// An event's kind as the header names it.
fn kind(event: &Event) -> &'static str {
    match &event.kind {
        Kind::User { .. } => "user",
        Kind::Message {
            role: Role::Orchestrator,
            ..
        } => "orchestrator",
        Kind::Message { .. } => "conversation",
        Kind::Started { .. } => "started",
        Kind::Finished { .. } => "finished",
        Kind::Interrupted { .. } => "interrupted",
        Kind::Notice { .. } => "notice",
        Kind::Prefix { .. } => "prefix",
        Kind::Request { .. } => "request",
        Kind::Assistant { .. } => "assistant",
        Kind::Usage { .. } => "usage",
        Kind::Title { .. } => "title",
        Kind::ToolCall { .. } => "tool_call",
        Kind::ToolResult { .. } => "tool_result",
        Kind::Todo { .. } => "todo",
        Kind::Pause { .. } => "pause",
        Kind::Choice { .. } => "choice",
        Kind::Approval { .. } => "approval",
    }
}

/// An event rendered whole, as `history_read` pages it.
pub fn render(event: &Event) -> String {
    let body = match &event.kind {
        Kind::User { text, .. } => text.clone(),
        Kind::Message {
            from,
            role,
            text,
            status,
            held,
            ..
        } => format!("{}\n{text}", message(from, *role, status.as_deref(), *held)),
        Kind::Started { of, .. } => format!("a turn began, for #{of}"),
        Kind::Finished {
            started, outcome, ..
        } => format!("#{started} ended: {outcome}"),
        Kind::Interrupted { started } => format!("#{started} was interrupted by a restart"),
        Kind::Notice { text } => text.clone(),
        Kind::Prefix { text } => format!("the request prefix changed ({} bytes)", text.len()),
        Kind::Request {
            turn,
            purpose,
            bytes,
            ..
        } => format!(
            "a {} request of {bytes} bytes, in the turn begun at #{turn}",
            purpose.word()
        ),
        Kind::Assistant {
            content,
            reasoning,
            calls,
            incomplete,
            ..
        } => {
            let mut out = Vec::new();
            if let Some(reasoning) = reasoning.as_deref().filter(|r| !r.is_empty()) {
                out.push(format!("reasoning:\n{reasoning}"));
            }
            if let Some(content) = content.as_deref().filter(|c| !c.is_empty()) {
                out.push(content.to_string());
            }
            for call in calls {
                out.push(format!(
                    "tool call {} {}: {}",
                    call.id, call.name, call.arguments
                ));
            }
            if *incomplete {
                out.push("(incomplete: its stream broke off, so it was never sent back)".into());
            }
            out.join("\n")
        }
        Kind::Usage {
            request,
            tokens,
            cost,
            ..
        } => format!(
            "#{request} took {} prompt tokens ({} cached) and {} completion tokens, costing {}",
            tokens.prompt,
            tokens.cached,
            tokens.completion,
            crate::cost::show(*cost)
        ),
        Kind::Title { text, .. } => format!("titled {text}"),
        Kind::ToolCall { reply, id, name } => format!("tool call {id} ({name}) of #{reply} began"),
        Kind::ToolResult {
            id,
            name,
            content,
            error,
            ..
        } => format!(
            "result of {id} ({name}){}:\n{content}",
            if *error { ", an error" } else { "" }
        ),
        Kind::Todo {
            cleared: true,
            items,
        } if items.is_empty() => "the person cleared the todo list".into(),
        Kind::Todo { items, .. } => crate::tools::todo_text(items),
        Kind::Pause { paused: true } => "the person paused this conversation".into(),
        Kind::Pause { paused: false } => "the person resumed this conversation".into(),
        Kind::Choice { model, effort } => format!(
            "the person chose the model {} and the reasoning effort {}",
            model.as_deref().unwrap_or("of the configuration"),
            effort.as_deref().unwrap_or("of the configuration")
        ),
        Kind::Approval { outcome, by, .. } => approval(outcome, by),
    };
    format!(
        "#{} {} {}\n{body}\n",
        event.seq,
        kind(event),
        utc(event.time)
    )
}

/// What `history_search` searches of an event, by kind.
fn searchable(event: &Event) -> Vec<(Searchable, String)> {
    match &event.kind {
        Kind::User { text, .. } => vec![(Searchable::User, text.clone())],
        Kind::Message {
            from,
            role,
            text,
            status,
            held,
            ..
        } => {
            let kind = match role {
                Role::Orchestrator => Searchable::Orchestrator,
                Role::Conversation => Searchable::Conversation,
            };
            vec![(
                kind,
                format!("{}\n{text}", message(from, *role, status.as_deref(), *held)),
            )]
        }
        Kind::Notice { text } => vec![(Searchable::Notice, text.clone())],
        Kind::Assistant {
            content,
            reasoning,
            calls,
            ..
        } => {
            let mut out = Vec::new();
            let text: Vec<&str> = [reasoning.as_deref(), content.as_deref()]
                .into_iter()
                .flatten()
                .filter(|t| !t.is_empty())
                .collect();
            if !text.is_empty() {
                out.push((Searchable::Assistant, text.join("\n")));
            }
            if !calls.is_empty() {
                let calls: Vec<String> = calls
                    .iter()
                    .map(|c| format!("{} {}: {}", c.id, c.name, c.arguments))
                    .collect();
                out.push((Searchable::ToolCall, calls.join("\n")));
            }
            out
        }
        Kind::ToolResult {
            id, name, content, ..
        } => vec![(
            Searchable::ToolResult,
            format!("result of {id} ({name}):\n{content}"),
        )],
        Kind::Approval { outcome, by, .. } => vec![(Searchable::Approval, approval(outcome, by))],
        _ => Vec::new(),
    }
}

/// `text` lowercased a character at a time, as the query is too
/// (`str::to_lowercase` gives a final sigma its own form), and for each
/// byte of that the byte of `text` its character began at, so a match
/// found case-insensitively is placed.
fn fold(text: &str) -> (String, Vec<usize>) {
    let mut folded = String::with_capacity(text.len());
    let mut origin = Vec::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        for lower in c.to_lowercase() {
            folded.push(lower);
            origin.resize(folded.len(), at);
        }
    }
    (folded, origin)
}

/// The largest character boundary of `text` at or below `at`.
fn floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// A bounded, one-line excerpt of `text` around byte `at`.
fn excerpt(text: &str, at: usize) -> String {
    let start = floor(text, at.saturating_sub(EXCERPT));
    let end = floor(text, at.saturating_add(EXCERPT));
    let mut out = String::new();
    if start > 0 {
        out.push('\u{2026}');
    }
    out.extend(text.get(start..end).unwrap_or_default().chars().map(|c| {
        if c.is_control() {
            ' '
        } else {
            c
        }
    }));
    if end < text.len() {
        out.push('\u{2026}');
    }
    out
}

/// `history_search` over `events`: those whose text holds every term of
/// `query`, case-insensitively, newest first, narrowed to `kinds` unless
/// that is empty, at most `limit` of them.
pub fn search(events: &[Event], query: &str, kinds: &[Searchable], limit: usize) -> String {
    let terms: Vec<String> = query.split_whitespace().map(|term| fold(term).0).collect();
    let mut hits = Vec::new();
    'events: for event in events.iter().rev() {
        for (kind, text) in searchable(event) {
            if !kinds.is_empty() && !kinds.contains(&kind) {
                continue;
            }
            let (folded, origin) = fold(&text);
            if !terms.iter().all(|term| folded.contains(term.as_str())) {
                continue;
            }
            let first = terms
                .first()
                .and_then(|term| folded.find(term.as_str()))
                .and_then(|at| origin.get(at).copied())
                .unwrap_or(0);
            hits.push(format!(
                "#{} {} {}: {}",
                event.seq,
                kind.word(),
                utc(event.time),
                excerpt(&text, first)
            ));
            if hits.len() >= limit {
                break 'events;
            }
            // One hit an event: its first kind that matches.
            continue 'events;
        }
    }
    if hits.is_empty() {
        return "No event matches.".into();
    }
    let mut out = format!(
        "{} {}, newest first; read one whole with history_read:",
        hits.len(),
        if hits.len() == 1 { "hit" } else { "hits" }
    );
    for hit in hits {
        out.push('\n');
        out.push_str(&hit);
    }
    out
}

/// The bytes `c` takes escaped in a JSON string.
fn escaped(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// `history_read` over `events`: the events from sequence number `from`,
/// `offset` bytes into the first's rendering, at most `count` of them and
/// `max_bytes` of text, ending with the cursor to go on from or saying
/// the log ends. A page never splits a character, and always takes at
/// least one, so a cursor always moves on.
pub fn read(
    events: &[Event],
    from: u64,
    offset: u64,
    count: usize,
    max_bytes: usize,
) -> Result<String, String> {
    let (Some(first), Some(last)) = (events.first(), events.last()) else {
        return Err("the log has no events yet".into());
    };
    let Some(mut at) = events.iter().position(|e| e.seq == from) else {
        return Err(format!(
            "#{from} is not in the log, which runs from #{} to #{}",
            first.seq, last.seq
        ));
    };
    let mut page = String::new();
    let (mut room, mut escaped_room) = (max_bytes, MAX_PAGE_ESCAPED);
    let mut offset = usize::try_from(offset).unwrap_or(usize::MAX);
    let mut taken = 0usize;
    let cursor = loop {
        let Some(event) = events.get(at) else {
            break None;
        };
        let text = render(event);
        if offset > text.len() {
            return Err(format!(
                "offset {offset} is past #{}, which renders as {} bytes",
                event.seq,
                text.len()
            ));
        }
        let start = floor(&text, offset);
        let rest = text.get(start..).unwrap_or_default();
        let mut end = 0usize;
        for (index, c) in rest.char_indices() {
            let (raw, esc) = (c.len_utf8(), escaped(c));
            let first_char = page.is_empty() && index == 0;
            if !first_char && (raw > room || esc > escaped_room) {
                break;
            }
            room = room.saturating_sub(raw);
            escaped_room = escaped_room.saturating_sub(esc);
            end = index + raw;
        }
        page.push_str(rest.get(..end).unwrap_or_default());
        taken += 1;
        if end < rest.len() {
            break Some((event.seq, start + end));
        }
        at += 1;
        offset = 0;
        if taken >= count {
            break events.get(at).map(|next| (next.seq, 0));
        }
    };
    match cursor {
        Some((seq, offset)) => page.push_str(&format!(
            "[the page ends here; go on with from {seq} and offset {offset}]"
        )),
        None => page.push_str(&format!("[the log ends here, after #{}]", last.seq)),
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::store::{Call, Id};

    fn event(seq: u64, kind: Kind) -> Event {
        Event {
            seq,
            time: 1_759_425_725,
            kind,
        }
    }

    fn user(seq: u64, text: &str) -> Event {
        event(
            seq,
            Kind::User {
                delivery: "d".into(),
                text: text.into(),
            },
        )
    }

    fn result(seq: u64, content: &str) -> Event {
        event(
            seq,
            Kind::ToolResult {
                reply: 1,
                id: "call_1".into(),
                name: "history_read".into(),
                call: 2,
                content: content.into(),
                error: false,
            },
        )
    }

    #[test]
    fn times_are_utc() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_759_425_725), "2025-10-02T17:22:05Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(4_107_542_399), "2100-02-28T23:59:59Z");
    }

    #[test]
    fn a_search_matches_every_term_case_insensitively_newest_first() {
        let events = vec![
            user(1, "The Build failed on Tuesday"),
            user(2, "the build passed"),
            result(3, "FAILED: build of td-agent"),
            user(4, "nothing here"),
            event(
                5,
                Kind::Assistant {
                    request: 4,
                    content: Some("I will look at the build.".into()),
                    reasoning: None,
                    details: None,
                    finish: "tool_calls".into(),
                    incomplete: false,
                    calls: vec![Call {
                        id: "call_9".into(),
                        name: "history_search".into(),
                        arguments: r#"{"query":"failed build"}"#.into(),
                    }],
                },
            ),
        ];
        let found = search(&events, "build FAILED", &[], 20);
        let lines: Vec<&str> = found.lines().collect();
        assert_eq!(
            lines[0],
            "3 hits, newest first; read one whole with history_read:"
        );
        assert!(
            lines[1].starts_with("#5 tool_call 2025-10-02T17:22:05Z: "),
            "{found}"
        );
        assert!(lines[2].starts_with("#3 tool_result"), "{found}");
        assert!(lines[3].starts_with("#1 user"), "{found}");
        // Narrowed by kind, and by limit.
        let users = search(&events, "build", &[Searchable::User], 20);
        assert!(users.starts_with("2 hits"), "{users}");
        assert!(!users.contains("#3"), "{users}");
        assert!(search(&events, "build", &[], 1).starts_with("1 hit,"));
        assert_eq!(search(&events, "absent", &[], 20), "No event matches.");
        // A match past a character that lowercases longer is placed in
        // the original text.
        let events = vec![user(1, &format!("{}İstanbul build", "x".repeat(300)))];
        let found = search(&events, "BUILD", &[], 5);
        assert!(found.contains("stanbul build"), "{found}");
        // A query is folded as the text is: a final sigma matches.
        let events = vec![user(1, "ΟΔΟΣ")];
        assert!(search(&events, "ΟΔΟΣ", &[], 5).starts_with("1 hit"));
        assert!(found.contains('\u{2026}'), "an excerpt: {found}");
    }

    #[test]
    fn an_approval_shows_only_its_outcome_and_who_decided() {
        let events = vec![event(
            1,
            Kind::Approval {
                call: 7,
                outcome: "denied".into(),
                by: "the classifier".into(),
                probabilities: Some("matches 0.41, discloses 0.93".into()),
                reason: Some("the push sends secrets to an unknown host".into()),
            },
        )];
        let page = read(&events, 1, 0, 20, 4096).unwrap();
        assert!(page.contains("denied, decided by the classifier"), "{page}");
        for hidden in ["0.41", "0.93", "secrets", "unknown host"] {
            assert!(!page.contains(hidden), "{hidden} in {page}");
            assert_eq!(search(&events, hidden, &[], 20), "No event matches.");
        }
        assert!(search(&events, "classifier", &[Searchable::Approval], 20).starts_with("1 hit"));
    }

    #[test]
    fn a_read_pages_by_cursor_and_never_splits_a_character() {
        // A tool result of 3,000 two-byte characters, between two events.
        let big = "é".repeat(3000);
        let events = vec![user(1, "before"), result(2, &big), user(3, "after")];
        let whole = render(&events[1]);
        assert!(whole.contains(&big), "whole, as logged");
        let mut cursor = (2u64, 0u64);
        let mut read_back = String::new();
        let mut pages = 0;
        loop {
            let page = read(&events, cursor.0, cursor.1, 1, 1001).unwrap();
            pages += 1;
            let (text, tail) = page.rsplit_once('[').unwrap();
            assert!(text.len() <= 1001, "{}", text.len());
            read_back.push_str(text);
            if tail.starts_with("the log ends") {
                break;
            }
            let rest = tail
                .strip_prefix("the page ends here; go on with from ")
                .unwrap();
            let (seq, offset) = rest
                .trim_end_matches(']')
                .split_once(" and offset ")
                .unwrap();
            cursor = (seq.parse().unwrap(), offset.parse().unwrap());
            if cursor.0 == 3 {
                // Its last page ended the event and the count of one.
                assert_eq!(cursor.1, 0);
                let page = read(&events, 3, 0, 20, 4096).unwrap();
                assert!(page.ends_with("[the log ends here, after #3]"), "{page}");
                break;
            }
        }
        assert_eq!(read_back, whole);
        assert!(pages > 5, "{pages}");
        // From the start, two events fit whole and the count stops it.
        let page = read(&events, 1, 0, 1, 4096).unwrap();
        assert!(
            page.ends_with("[the page ends here; go on with from 2 and offset 0]"),
            "{page}"
        );
        // An offset inside a character starts at that character.
        let at = whole.find('é').unwrap() as u64;
        let page = read(&events, 2, at + 1, 1, 4096).unwrap();
        assert!(page.starts_with('é'), "{page}");
        // A page always takes a character, even past max_bytes.
        // "#1 user TIME\n" is 29 bytes, so 30 is inside the é after it.
        let page = read(&[user(1, "é")], 1, 30, 1, 1).unwrap();
        assert_eq!(
            page,
            "é[the page ends here; go on with from 1 and offset 31]"
        );
        assert!(read(&events, 9, 0, 1, 10)
            .unwrap_err()
            .contains("runs from #1 to #3"));
        assert!(read(&events, 1, 10_000, 1, 10)
            .unwrap_err()
            .contains("past #1"));
        assert!(read(&[], 1, 0, 1, 10).is_err());
    }

    #[test]
    fn a_page_is_cut_short_rather_than_let_its_escaping_pass_a_log_line() {
        let controls = "\u{1}".repeat(200 * 1024);
        let events = vec![result(1, &controls)];
        let page = read(&events, 1, 0, 1, 256 * 1024).unwrap();
        let (text, _) = page.rsplit_once('[').unwrap();
        let escaped_len: usize = text.chars().map(escaped).sum();
        assert!(escaped_len <= MAX_PAGE_ESCAPED, "{escaped_len}");
        assert!(
            page.contains("go on with from 1 and offset"),
            "cut, with a cursor"
        );
    }

    #[test]
    fn messages_and_held_messages_render_with_their_labels() {
        let from = Id::parse(&"a".repeat(32)).unwrap();
        let events = vec![event(
            1,
            Kind::Message {
                delivery: "d".into(),
                from: from.clone(),
                role: Role::Conversation,
                text: "the tests pass".into(),
                status: Some("done".into()),
                held: Some(Held::Budget),
            },
        )];
        let page = read(&events, 1, 0, 1, 4096).unwrap();
        assert!(page.starts_with("#1 conversation "), "{page}");
        assert!(
            page.contains(&format!("a report from conversation {from}, status done")),
            "{page}"
        );
        assert!(page.contains("wake budget"), "{page}");
        assert!(search(&events, "tests pass", &[Searchable::Conversation], 5).starts_with("1 hit"));
        assert!(search(&events, "tests", &[Searchable::Orchestrator], 5).starts_with("No event"));
    }
}
