//! A reply's Markdown as the transcript's styled text (DESIGN.md §4,
//! "Markdown"): td-markdown's blocks and spans mapped onto td-ui's
//! `Rich`, whose copy takes the reply as written. Nothing is fetched: an
//! image shows as its alt text, a link to its source. The person's own
//! text is styled only where it is code (`typed`).

use std::ops::Range;

use td_markdown::{BlockKind, Document, Level, Marker, Span, SpanKind, Style};
use td_ui::rich::{self, Look, Rich, MAX_PREFIX_CHARS};

/// A quote's prefix on every row of its blocks.
const QUOTE: &str = "\u{2502} ";
/// A bullet item's marker.
const BULLET: &str = "\u{2022} ";
/// A thematic break.
const RULE: &str = "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}";

/// `text` as styled text, or `None` when it is no Markdown the parser or
/// the list takes (too long, past a bound), for the caller to show plain.
pub fn rich(text: &str) -> Option<Rich> {
    let document = td_markdown::parse(text).ok()?;
    build(text, &document).ok()
}

/// The person's `text` as styled text: shown as typed, every line,
/// space and character, with its fenced code blocks and code spans in
/// code and nothing else read as Markdown, since what they type and
/// paste (logs, diffs, commands, paths) is rarely meant as markup;
/// `None` past styled text's bounds, for the caller to show it plain.
pub fn typed(text: &str) -> Option<Rich> {
    // A message of matched fences around nothing would show nothing:
    // plain.
    build_typed(text)
        .ok()
        .filter(|rich| !rich.shown().trim().is_empty() || text.trim().is_empty())
}

fn build_typed(text: &str) -> Result<Rich, rich::Error> {
    let mut rich = Rich::new(text)?;
    // The open fence's character and length, inside a fenced block.
    let mut fence: Option<(u8, usize)> = None;
    // Whether the open block is code, none before the first line's text
    // and after a fence's edge.
    let mut open: Option<bool> = None;
    // The line ending before this line in the same block.
    let mut ending: Option<Range<usize>> = None;
    // The shortest backtick and tilde fences found never closed: a later
    // one at least as long cannot close either, so it is text unscanned.
    let mut unclosed = (usize::MAX, usize::MAX);
    let mut at = 0;
    for whole in text.split_inclusive('\n') {
        let start = at;
        at += whole.len();
        let line = whole.trim_end_matches('\n').trim_end_matches('\r');
        let end = start + line.len();
        let next = (end < at).then_some(end..at);
        match fence {
            None => {
                let opened = opens(line).filter(|&(mark, length)| {
                    let shortest = if mark == b'`' {
                        &mut unclosed.0
                    } else {
                        &mut unclosed.1
                    };
                    if length >= *shortest {
                        return false;
                    }
                    let found = closed(text, at, mark, length);
                    if !found {
                        *shortest = length;
                    }
                    found
                });
                if let Some(opened) = opened {
                    // The fence's own line shows nothing; its code is a
                    // block of its own.
                    fence = Some(opened);
                    open = None;
                    ending = None;
                    continue;
                }
            }
            Some((mark, length)) => {
                if closes(line, mark, length) {
                    fence = None;
                    open = None;
                    ending = None;
                    continue;
                }
            }
        }
        let code = fence.is_some();
        if open != Some(code) {
            rich.block("", "", code)?;
            open = Some(code);
        } else if let Some(ending) = ending.take() {
            rich.line(ending)?;
        }
        if code {
            if !line.is_empty() {
                rich.text(start..end, Look::PLAIN, None)?;
            }
        } else {
            spans(&mut rich, text, start..end)?;
        }
        ending = next;
    }
    Ok(rich)
}

/// A fence opening on `line`: up to three spaces, then three or more
/// backticks or tildes, a backtick fence's rest holding none.
fn opens(line: &str) -> Option<(u8, usize)> {
    let rest = line.trim_start_matches(' ');
    if line.len() - rest.len() > 3 {
        return None;
    }
    let mark = *rest.as_bytes().first()?;
    if mark != b'`' && mark != b'~' {
        return None;
    }
    let length = rest.bytes().take_while(|&byte| byte == mark).count();
    let info = rest.get(length..).unwrap_or("");
    (length >= 3 && !(mark == b'`' && info.contains('`'))).then_some((mark, length))
}

/// Whether a line of `text` from byte `from` on closes a fence of
/// `length` `mark`s: one never closed is no fence, its line text (a row
/// of tildes as a separator). A scan that fails is for a fence shorter
/// than every one that failed before (`build_typed` keeps the shortest),
/// so with lengths distinct and summing within the text at most the
/// square root of twice its length fail for each mark; one that succeeds stops at the
/// closer, and the lines up to it are the fence's, so those never
/// overlap.
fn closed(text: &str, from: usize, mark: u8, length: usize) -> bool {
    text.get(from..)
        .unwrap_or("")
        .split('\n')
        .any(|line| closes(line.trim_end_matches('\r'), mark, length))
}

/// Whether `line` closes a fence of `length` `mark`s: up to three
/// spaces, at least as many of them, then only blanks.
fn closes(line: &str, mark: u8, length: usize) -> bool {
    let rest = line.trim_start_matches(' ');
    let count = rest.bytes().take_while(|&byte| byte == mark).count();
    line.len() - rest.len() <= 3
        && count >= length
        && rest
            .get(count..)
            .is_some_and(|after| after.trim_matches([' ', '\t']).is_empty())
}

/// A typed line's `range` of `text`: as typed, a run of backticks and
/// the next run of as many on the line closing a code span, shown
/// without them.
fn spans(rich: &mut Rich, text: &str, range: Range<usize>) -> Result<(), rich::Error> {
    let line = text.get(range.start..range.end).unwrap_or("").as_bytes();
    let mut shown = 0;
    let mut at = 0;
    while at < line.len() {
        if line.get(at) != Some(&b'`') {
            at += 1;
            continue;
        }
        let run = line
            .get(at..)
            .unwrap_or(&[])
            .iter()
            .take_while(|&&b| b == b'`')
            .count();
        let inner = at + run;
        let mut close = None;
        let mut look = inner;
        while look < line.len() {
            let length = line
                .get(look..)
                .unwrap_or(&[])
                .iter()
                .take_while(|&&b| b == b'`')
                .count();
            if length == run {
                close = Some(look);
                break;
            }
            look += length.max(1);
        }
        let Some(close) = close else {
            at = inner;
            continue;
        };
        if shown < at {
            rich.text(range.start + shown..range.start + at, Look::PLAIN, None)?;
        }
        if inner < close {
            rich.text(range.start + inner..range.start + close, Look::CODE, None)?;
        }
        shown = close + run;
        at = shown;
    }
    if shown < line.len() {
        rich.text(range.start + shown..range.end, Look::PLAIN, None)?;
    }
    Ok(())
}

fn build(text: &str, document: &Document) -> Result<Rich, rich::Error> {
    let mut rich = Rich::new(text)?;
    // Each link and image destination once, in the document's order; one
    // the browser would not be given (`open::url`: http or https, one
    // word) is no link, so nothing looks followable that is not.
    let mut targets = Vec::new();
    targets
        .try_reserve(document.links.len())
        .map_err(|_| rich::Error::Limit)?;
    for link in &document.links {
        let target = unescape(td_markdown::text(text, &link.destination));
        let made = if td_ui::open::is_url(&target) {
            Some(rich.link(&target)?)
        } else {
            None
        };
        targets.push(made);
    }
    let mut first = String::new();
    let mut rest = String::new();
    for (number, block) in document.blocks.iter().enumerate() {
        if number > 0 && block.blank_before {
            rich.gap()?;
        }
        prefixes(document.levels(block), &mut first, &mut rest);
        let code = matches!(block.kind, BlockKind::Code { .. });
        rich.block(&first, &rest, code)?;
        if block.kind == BlockKind::Rule {
            rich.put(RULE, block.source.clone(), Look::QUIET, None)?;
            continue;
        }
        let atx = text
            .get(block.source.start..block.source.end)
            .is_some_and(atx);
        let mut base = match block.kind {
            // An ATX heading's top two levels larger, where the faces have
            // sizes. Not a setext one: a streaming paragraph that ends in
            // a line of `-` or `=` is one until the next delta, and would
            // reflow everything below it for that frame.
            BlockKind::Heading(1) if atx => Look::BOLD.with(Look::LARGER),
            BlockKind::Heading(2) if atx => Look::BOLD.with(Look::LARGE),
            BlockKind::Heading(_) => Look::BOLD,
            // Aligned by its columns as written.
            BlockKind::Table => Look::MONO,
            _ => Look::PLAIN,
        };
        if document.levels(block).contains(&Level::Quote) {
            base = base.with(Look::QUIET);
        }
        for span in document.spans(block) {
            piece(&mut rich, span, base, &targets)?;
        }
    }
    Ok(rich)
}

/// A block's prefixes: on its first row each container's mark (a quote's
/// bar, an item's marker where the item starts), on its later rows the
/// bars and as many spaces; cut alike to `MAX_PREFIX_CHARS`.
fn prefixes(levels: &[Level], first: &mut String, rest: &mut String) {
    first.clear();
    rest.clear();
    let mut mark = String::new();
    for level in levels {
        match level {
            Level::Quote => {
                first.push_str(QUOTE);
                rest.push_str(QUOTE);
            }
            Level::Item {
                marker,
                first: opens,
            } => {
                mark.clear();
                match marker {
                    Marker::Bullet(_) => mark.push_str(BULLET),
                    Marker::Ordered { number, delimiter } => {
                        use std::fmt::Write;
                        let _ = write!(mark, "{number}{delimiter} ");
                    }
                }
                let width = mark.chars().count();
                if *opens {
                    first.push_str(&mark);
                } else {
                    first.extend(std::iter::repeat_n(' ', width));
                }
                rest.extend(std::iter::repeat_n(' ', width));
            }
        }
    }
    for prefix in [first, rest] {
        if let Some((cut, _)) = prefix.char_indices().nth(MAX_PREFIX_CHARS) {
            prefix.truncate(cut);
        }
    }
}

/// Whether a heading's source opens as an ATX heading: up to three
/// spaces, one to six `#`, then a space, a tab or its end.
fn atx(source: &str) -> bool {
    let rest = source.trim_start_matches(' ');
    if source.len() - rest.len() > 3 {
        return false;
    }
    let after = rest.trim_start_matches('#');
    let marks = rest.len() - after.len();
    (1..=6).contains(&marks) && (after.is_empty() || after.starts_with([' ', '\t']))
}

fn look(style: Style, base: Look) -> Look {
    let mut look = base;
    if style.has(Style::STRONG) {
        look = look.with(Look::BOLD);
    }
    if style.has(Style::EMPHASIS) {
        look = look.with(Look::ITALIC);
    }
    if style.has(Style::STRIKE) {
        look = look.with(Look::STRIKE);
    }
    look
}

fn piece(
    rich: &mut Rich,
    span: &Span,
    base: Look,
    targets: &[Option<usize>],
) -> Result<(), rich::Error> {
    let target = |link: usize| targets.get(link).copied().flatten();
    let link = span.link.and_then(target);
    let look = look(span.style, base);
    let range = span.range.clone();
    match span.kind {
        SpanKind::Text => rich.text(range, look, link),
        SpanKind::Code => rich.text(range, look.with(Look::CODE), link),
        SpanKind::SoftBreak => rich.put(" ", range, look, link),
        SpanKind::HardBreak => rich.line(range),
        SpanKind::Image {
            source,
            open,
            close,
        } => {
            // Its alt text in brackets standing for its markup, so a copy
            // reaching into either takes the image whole; a link to the
            // image unless the image is a link's own.
            let link = link.or_else(|| target(source));
            let quiet = look.with(Look::QUIET);
            rich.put("[", open..range.start, quiet, link)?;
            if range.is_empty() {
                rich.put("image", range.clone(), quiet, link)?;
            } else {
                rich.text(range.clone(), quiet, link)?;
            }
            rich.put("]", range.end..close, quiet, link)
        }
    }
}

/// A link's tooltip in at most `room` characters: its target, or, when
/// that does not fit or names a user before its host, the host it opens
/// first, then as much of the target as fits, so where it leads always
/// shows.
pub fn tip(url: &str, room: usize) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if room == 0 {
        return String::new();
    }
    let named = authority.contains('@');
    if !named && url.chars().count() <= room {
        return url.to_string();
    }
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let whole = format!("{host} \u{00b7} {url}");
    if whole.chars().count() <= room {
        return whole;
    }
    let mut cut: String = whole.chars().take(room.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// A destination as written, its escapes' backslashes taken out.
fn unescape(destination: &str) -> String {
    let mut out = String::with_capacity(destination.len());
    let mut chars = destination.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                if next.is_ascii_punctuation() {
                    out.push(next);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn a_reply_shows_its_text_without_its_markup() {
        let reply = "# Title\n\nSome **bold**, *it* and `code`.\nNext line.\n\n\
                     - one\n- two\n\n> quoted\n\n---\n\n```rust\nfn x() {}\nlet y;\n```";
        let rich = rich(reply).unwrap();
        assert_eq!(rich.source(), reply);
        assert_eq!(
            rich.shown(),
            "Title\n\nSome bold, it and code. Next line.\n\none\ntwo\n\nquoted\n\n\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n\n\
             fn x() {}\nlet y;"
        );
    }

    /// The person's text shows as typed: nothing but code read as
    /// Markdown, every line kept.
    #[test]
    fn typed_text_shows_as_typed() {
        let typed = "Two things:\n  fix `__init__.py`\n- a\n+ b\n# not a heading\n---\n\
                     2*3*4, \\.rs$, Option<std::path::PathBuf>, 5~10, [a](b)\n\n\\\
                     \r\nend";
        let rich = super::typed(typed).unwrap();
        assert_eq!(rich.source(), typed);
        assert_eq!(rich.shown(), typed.replace(['`', '\r'], ""));
        let code = rich.shown().find("__init__").unwrap();
        assert_eq!(rich.look_at(code), Look::CODE);
        assert_eq!(rich.look_at(0), Look::PLAIN);
    }

    /// A fenced block shows its code without its fences; a fence never
    /// closed, a lone backtick and a row of tildes are text; past styled
    /// text's bounds the text is left to show plain.
    #[test]
    fn typed_fences_and_lone_backticks() {
        let rich = super::typed("see:\n```sh\nls -l\n  cd x\n```\ndone ` here").unwrap();
        assert_eq!(rich.shown(), "see:\nls -l\n  cd x\ndone ` here");
        for typed in ["~~~\nopen\nstill", "a\n~~~~~~~~\nb", "```"] {
            assert_eq!(super::typed(typed).unwrap().shown(), typed);
        }
        let rich = super::typed("~~~~\nin ~~~\n~~~~~\nout").unwrap();
        assert_eq!(rich.shown(), "in ~~~\nout");
        // Openers that cannot close each other scan once, then not again.
        let many = "~~~a\n".repeat(20_000);
        assert_eq!(super::typed(&many).unwrap().shown(), many.trim_end());
        // Fences alone show plain, not an empty bubble.
        assert!(super::typed("```\n```").is_none());
        assert!(super::typed("```\n\n\n```").is_none());
        assert!(super::typed(&"a\n".repeat(rich::MAX_RECORDS)).is_none());
        assert!(super::typed(&"a".repeat(rich::MAX_BYTES + 1)).is_none());
        assert!(opens("   ```rust") == Some((b'`', 3)) && opens("    ```").is_none());
        assert!(opens("``` a`b").is_none() && opens("``").is_none());
        assert!(closes("````  ", b'`', 3) && !closes("``", b'`', 3) && !closes("``` x", b'`', 3));
    }

    /// The pieces `typed` lays stand for the source bytes they show: a
    /// line's text and its ending, a fence's code in a code block, the
    /// fences and a CRLF's carriage return standing for nothing shown;
    /// two spans on a line.
    #[test]
    fn typed_pieces_stand_for_their_bytes() {
        let source = "a\r\ne\r\n```\r\nx\r\n```\r\n`b` c `d`";
        let mut expected = Rich::new(source).unwrap();
        expected.block("", "", false).unwrap();
        expected.text(0..1, Look::PLAIN, None).unwrap();
        expected.line(1..3).unwrap();
        expected.text(3..4, Look::PLAIN, None).unwrap();
        expected.block("", "", true).unwrap();
        expected.text(11..12, Look::PLAIN, None).unwrap();
        expected.block("", "", false).unwrap();
        expected.text(20..21, Look::CODE, None).unwrap();
        expected.text(22..25, Look::PLAIN, None).unwrap();
        expected.text(26..27, Look::CODE, None).unwrap();
        assert_eq!(super::typed(source).unwrap(), expected);
        assert_eq!(expected.shown(), "a\ne\nx\nb c d");
    }

    /// ATX headings of the top two levels are larger, a setext one only
    /// bold, a table monospace.
    #[test]
    fn headings_and_tables_take_their_looks() {
        let reply = "# One\n\n## Two\n\n### Three\n\nFour\n----\n\n\
                     | a | b |\n|---|---|\n| c | d |";
        let rich = rich(reply).unwrap();
        let at = |text: &str| rich.look_at(rich.shown().find(text).unwrap());
        assert_eq!(at("One"), Look::BOLD.with(Look::LARGER));
        assert_eq!(at("Two"), Look::BOLD.with(Look::LARGE));
        assert_eq!(at("Three"), Look::BOLD);
        assert_eq!(at("Four"), Look::BOLD);
        assert_eq!(at("| c"), Look::MONO);
        assert!(atx("   ###### x") && atx("#") && !atx("####### x"));
        assert!(!atx("#hashtag") && !atx("    # code"));
    }

    #[test]
    fn links_and_images_name_their_destinations() {
        let reply = "see [the docs](https://e.x/a\\_b) and <https://f.x/>, \
                     ![a cat](https://e.x/c.png \"t\") or [![](https://e.x/i)](https://e.x/l) \
                     [mail](mailto:a@e.x)";
        let rich = rich(reply).unwrap();
        assert_eq!(
            rich.shown(),
            "see the docs and https://f.x/, [a cat] or [image] mail"
        );
        let targets: Vec<&str> = (0..).map_while(|i| rich.target(i)).collect();
        assert!(targets.contains(&"https://e.x/a_b"), "{targets:?}");
        assert!(targets.contains(&"https://f.x/"), "{targets:?}");
        assert!(targets.contains(&"https://e.x/c.png"), "{targets:?}");
        // What the browser would not be given is no link.
        assert!(
            !targets.iter().any(|t| t.starts_with("mailto")),
            "{targets:?}"
        );
    }

    #[test]
    fn prefixes_mark_containers_and_stay_within_the_bound() {
        let (mut first, mut rest) = (String::new(), String::new());
        let item = |number, first| Level::Item {
            marker: Marker::Ordered {
                number,
                delimiter: '.',
            },
            first,
        };
        let bullet = Level::Item {
            marker: Marker::Bullet('-'),
            first: true,
        };
        prefixes(&[Level::Quote, item(12, true)], &mut first, &mut rest);
        assert_eq!(
            (first.as_str(), rest.as_str()),
            ("\u{2502} 12. ", "\u{2502}     ")
        );
        prefixes(&[bullet, item(3, false)], &mut first, &mut rest);
        assert_eq!((first.as_str(), rest.as_str()), ("\u{2022}    ", "     "));
        let deep = vec![item(123_456_789, true); 20];
        prefixes(&deep, &mut first, &mut rest);
        assert_eq!(first.chars().count(), MAX_PREFIX_CHARS);
        assert_eq!(rest.chars().count(), MAX_PREFIX_CHARS);
        let mut rich = Rich::new("").unwrap();
        rich.block(&first, &rest, false).unwrap();
    }

    #[test]
    fn a_links_tip_shows_where_it_leads() {
        assert_eq!(tip("https://e.x/a", 40), "https://e.x/a");
        assert_eq!(
            tip("https://good.x@evil.x/a", 40),
            "evil.x \u{00b7} https://good.x@evil.x/a"
        );
        let long = format!("https://{}@evil.x/", "a".repeat(200));
        let shown = tip(&long, 30);
        assert_eq!(shown.chars().count(), 30);
        assert!(shown.starts_with("evil.x \u{00b7} https://aaa"), "{shown}");
        assert!(shown.ends_with('\u{2026}'));
        assert_eq!(tip("https://e.x/", 0), "");
        assert_eq!(tip("https://e.x/abcdef", 12), "e.x \u{00b7} https\u{2026}");
    }

    #[test]
    fn what_styled_text_cannot_hold_is_left_plain() {
        let long = "a".repeat(td_ui::rich::MAX_BYTES + 1);
        assert!(rich(&long).is_none());
        assert_eq!(unescape("a\\_b\\c\\"), "a_b\\c\\");
    }
}
