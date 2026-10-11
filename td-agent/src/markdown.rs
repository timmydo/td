//! A reply's Markdown as the transcript's styled text (DESIGN.md §4,
//! "Markdown"): td-markdown's blocks and spans mapped onto td-ui's
//! `Rich`, whose copy takes the reply as written. Nothing is fetched: an
//! image shows as its alt text, a link to its source.

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
        let mut base = match block.kind {
            BlockKind::Heading(_) => Look::BOLD,
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
