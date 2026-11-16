#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::{layout, Request};
    use crate::inline::{FontLine, FontMetrics};
    use crate::text::{fit, Fit};
    use crate::{BoxKind, Error, Limits, StyledDocument, MAX_INPUT};
    use std::sync::atomic::AtomicBool;
    use td_css::{ComputedStyle, Origin, StyleSet, HTML_UA_CSS};
    use td_html::document::Document;

    /// 8 px a scalar, 16 px a wide one, none for a zero-width space or a
    /// combining mark; a 20 px line.
    struct Metrics;
    impl FontMetrics for Metrics {
        fn advance(&mut self, _: &ComputedStyle, c: char) -> u32 {
            match c {
                '\u{200b}' | '\u{301}' => 0,
                '界' => 16 * 64,
                _ => 8 * 64,
            }
        }
        fn line(&mut self, _: &ComputedStyle) -> FontLine {
            FontLine {
                height: 20 * 64,
                baseline: 15 * 64,
            }
        }
    }
    const TEXT_CSS: &str = "plaintext{display:block;white-space:pre-wrap;font-family:monospace}";
    /// Margins, padding, borders and a percentage width the text's block
    /// is sized through.
    const BOXED_CSS: &str = "body{margin:3px 7%}div,plaintext{padding:0 5px;\
        border-left:2px solid;width:80%;min-width:30px}";
    fn sheets(boxed: bool) -> StyleSet {
        let extra = if boxed { BOXED_CSS } else { "" };
        StyleSet::new(&[
            (Origin::UserAgent, HTML_UA_CSS),
            (Origin::Author, TEXT_CSS),
            (Origin::Author, extra),
        ])
        .unwrap()
    }
    /// How the text reaches its node: after `<plaintext>`, as td-web
    /// gives it, where td-html makes every newline LF, or escaped in a
    /// `pre-wrap` block, which keeps a carriage return.
    #[derive(Clone, Copy)]
    enum Form {
        Plain,
        Escaped,
    }
    fn parse(text: &str, form: Form) -> Document {
        let html = match form {
            Form::Plain => format!("<!doctype html><plaintext>{text}"),
            Form::Escaped => {
                let mut html = String::from("<!doctype html><div style=white-space:pre-wrap>");
                for c in text.chars() {
                    match c {
                        '\r' => html.push_str("&#13;"),
                        '&' => html.push_str("&amp;"),
                        '<' => html.push_str("&lt;"),
                        _ => html.push(c),
                    }
                }
                html
            }
        };
        td_html::document::parse(&html).unwrap()
    }
    fn limits(line_scalars: usize) -> Limits<'static> {
        Limits {
            input: MAX_INPUT,
            line_scalars,
            ..Limits::default()
        }
    }
    fn styled<'d>(doc: &'d Document, boxed: bool, bound: usize) -> StyledDocument<'d> {
        StyledDocument::new(doc, &sheets(boxed), MAX_INPUT, limits(bound)).unwrap()
    }
    /// The page's text as its node holds it.
    fn node_text(styled: &StyledDocument<'_>) -> String {
        styled
            .boxes()
            .iter()
            .find(|b| b.kind == BoxKind::Text)
            .map(|b| {
                styled
                    .document()
                    .node(b.node)
                    .unwrap()
                    .text()
                    .unwrap()
                    .to_string()
            })
            .unwrap_or_default()
    }
    fn laid(styled: &StyledDocument<'_>, viewport: f64) -> usize {
        layout(styled, &mut Metrics, &Request::new(viewport, 600.0))
            .unwrap()
            .line_count
    }
    fn counted(styled: &StyledDocument<'_>, viewport: f64, cap: usize) -> Fit {
        fit(styled, &mut Metrics, viewport, cap, None).unwrap()
    }
    /// The end of the source line from `at`, before its newline.
    fn line_end(text: &str, at: usize) -> usize {
        text[at..].find(['\n', '\r']).map_or(text.len(), |n| at + n)
    }

    struct Random(u64);
    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }
    /// Words, runs of spaces and tabs, indents, newlines of every kind,
    /// long words, wide, zero-width and combining scalars.
    fn text(random: &mut Random) -> String {
        const PIECES: &[&str] = &[
            "a", "bc", "def", "ghij", " ", "  ", "    ", "\t", "\t\t", "\n", "\n", "\r\n", "\r",
            "é", "界", "\u{200b}", "e\u{301}", "\n    ", "\n\t", "x\ty", "\u{fffd}",
        ];
        let mut out = String::new();
        for _ in 0..random.below(60) {
            match random.below(12) {
                0 => out.push_str(&"w".repeat(random.below(40) + 1)),
                1 => out.push_str(&" ".repeat(random.below(30) + 1)),
                _ => out.push_str(PIECES[random.below(PIECES.len())]),
            }
        }
        out
    }
    const VIEWPORTS: &[f64] = &[
        16.0, 17.0, 23.0, 24.0, 31.0, 40.0, 47.5, 56.0, 64.0, 72.0, 88.0, 100.0, 128.0, 150.0,
        200.0, 256.0, 333.0, 500.0, 1280.0,
    ];
    fn form(case: usize) -> (Form, bool) {
        ([Form::Plain, Form::Escaped][case % 2], case % 3 == 2)
    }

    #[test]
    fn counts_agree_with_layout_and_bound_every_wider_width() {
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        for case in 0..600 {
            let source = text(&mut random);
            let (form, boxed) = form(case);
            let doc = parse(&source, form);
            // Some lines past a small scalar bound, broken or kept whole.
            let bound = [3, 7, 64, crate::MAX_LINE_SCALARS][case % 4];
            let styled = styled(&doc, boxed, bound);
            let mut previous: Option<Fit> = None;
            let mut sweep = Vec::new();
            for viewport in VIEWPORTS {
                let lines = laid(&styled, *viewport);
                let whole = counted(&styled, *viewport, usize::MAX);
                assert_eq!(whole.end, None);
                assert_eq!(whole.laid, lines, "{source:?} at {viewport}");
                assert!(whole.lines >= whole.laid);
                if let Some(narrower) = previous {
                    assert!(whole.lines <= narrower.lines, "{source:?} at {viewport}");
                }
                previous = Some(whole);
                sweep.push((lines, whole.lines));
            }
            // What a width counts bounds what every wider one lays.
            for (at, (_, bound)) in sweep.iter().enumerate() {
                for (wider, _) in &sweep[at..] {
                    assert!(wider <= bound, "{source:?}: {sweep:?}");
                }
            }
        }
    }

    #[test]
    fn a_cut_fits_its_cap_at_its_width_and_every_wider_one_and_keeps_all_it_can() {
        let mut random = Random(77);
        let mut within = 0;
        for case in 0..600 {
            let mut source = text(&mut random);
            source.push_str(&text(&mut random));
            let (form, boxed) = form(case);
            let doc = parse(&source, form);
            let bound = [5, crate::MAX_LINE_SCALARS][case % 2];
            let styled = styled(&doc, boxed, bound);
            let all = node_text(&styled);
            let viewport = VIEWPORTS[random.below(10)];
            let whole = counted(&styled, viewport, usize::MAX);
            let cap = random.below(whole.lines.max(1) + 2) + 1;
            let cut = counted(&styled, viewport, cap);
            assert!(cut.lines <= cap);
            let Some(end) = cut.end else {
                assert_eq!(
                    cut,
                    Fit {
                        work: cut.work,
                        ..whole
                    }
                );
                continue;
            };
            assert!(whole.lines > cap);
            let at_newline = end == all.len() || all[end..].starts_with(['\n', '\r']);
            let line_start = all[..end].rfind(['\n', '\r']).map_or(0, |n| n + 1);
            // At a source line's end, or within one keeping some of it.
            assert!(end == 0 || at_newline || end > line_start, "{all:?} {end}");
            within += usize::from(!at_newline);
            let kept = &all[..end];
            let doc = parse(kept, form);
            let prefix = self::styled(&doc, boxed, bound);
            assert_eq!(node_text(&prefix), kept);
            let again = counted(&prefix, viewport, usize::MAX);
            if at_newline || end == 0 {
                assert_eq!((again.laid, again.lines), (cut.laid, cut.lines), "{all:?}");
            } else {
                // Within a line the count is the kept text's, or one more
                // where a word split after a dropped indent, which the
                // kept text keeps.
                assert_eq!(again.laid, cut.laid, "{all:?} {end}");
                assert!(
                    (cut.lines - 1..=cut.lines).contains(&again.lines),
                    "{all:?} {end}"
                );
                // The next laid line of the passing source line, or the
                // rest of it, would have passed the cap but for that one.
                let out = layout(&styled, &mut Metrics, &Request::new(viewport, 600.0)).unwrap();
                let next = out
                    .lines
                    .iter()
                    .filter_map(|l| l.fragments.first().map(|f| f.range.start))
                    .find(|start| *start > end)
                    .map_or(line_end(&all, end), |start| start.min(line_end(&all, end)));
                let doc = parse(&all[..next], form);
                let more = self::styled(&doc, boxed, bound);
                assert!(
                    counted(&more, viewport, usize::MAX).lines >= cap,
                    "{all:?} {end}"
                );
            }
            for wider in VIEWPORTS.iter().filter(|v| **v >= viewport) {
                assert!(laid(&prefix, *wider) <= cap, "{all:?} at {wider}");
            }
            // A cut at a line's end: the next source line, or its first
            // laid line, would have passed the cap.
            if at_newline && end < all.len() {
                let mut next = end + 1;
                if all[end..].starts_with("\r\n") {
                    next += 1;
                }
                let doc = parse(&all[..line_end(&all, next)], form);
                let more = self::styled(&doc, boxed, bound);
                assert!(
                    counted(&more, viewport, usize::MAX).lines > cap,
                    "{all:?} {end}"
                );
            }
        }
        assert!(within > 20, "{within}");
    }

    #[test]
    fn a_wider_line_can_lay_more_where_an_indent_no_longer_drops() {
        // At 4 columns the indent is dropped before "ab", which moves, and
        // "ab c" fits; at 5 "   ab" fits, so "c" takes a line of its own.
        // The body's margins take 16 px.
        let doc = parse("   ab c", Form::Plain);
        let styled = styled(&doc, false, 64);
        let narrow = counted(&styled, 48.0, usize::MAX);
        let wide = counted(&styled, 56.0, usize::MAX);
        assert_eq!((narrow.laid, narrow.lines), (1, 2));
        assert_eq!((wide.laid, wide.lines), (2, 2));
        // A cap of one keeps nothing: the source line's first laid line
        // passes it with the indent counted.
        let cut = counted(&styled, 48.0, 1);
        assert_eq!((cut.end, cut.laid, cut.lines), (Some(0), 0, 0));
    }

    #[test]
    fn a_line_that_passes_the_cap_is_cut_within_after_the_lines_kept() {
        // 4 MiB of four-letter words at 11 columns of 8 px: two words a
        // line, the first 1,000 kept, after a banner line or blank lines.
        let line = "abcd ".repeat(MAX_INPUT / 5 - 16);
        for (head, lines) in [("", 0), ("/*! license */\n", 2), ("\n\n", 2)] {
            let doc = parse(&format!("{head}{line}\nnext"), Form::Plain);
            let styled = styled(&doc, false, 65_536);
            let cut = counted(&styled, 104.0, 1000);
            let end = head.len() + (1000 - lines) * 10;
            assert_eq!((cut.end, cut.laid, cut.lines), (Some(end), 1000, 1000));
            // The count stopped within a chunk of the cut.
            assert!(cut.work < 200_000, "{}", cut.work);
        }
        // An unterminated last line, three words a 14-column line, its
        // last laid line the one that passes.
        let words = "abcd ".repeat(100);
        for head in ["", "banner\n"] {
            let doc = parse(&format!("{head}{words}"), Form::Plain);
            let styled = styled(&doc, false, 64);
            let whole = counted(&styled, 128.0, usize::MAX);
            let lines = usize::from(!head.is_empty());
            assert_eq!(whole.laid, 34 + lines);
            let cut = counted(&styled, 128.0, whole.laid - 1);
            let end = head.len() + 33 * 15;
            assert_eq!((cut.end, cut.laid), (Some(end), 33 + lines));
        }
        // A word split after a dropped indent: the count keeps the
        // indent's line, the kept text the indent, laid on the line.
        let doc = parse(" abcdefghijklm界", Form::Plain);
        let cut = counted(&self::styled(&doc, false, 64), 128.0, 2);
        assert_eq!((cut.end, cut.laid, cut.lines), (Some(14), 1, 2));
        let doc = parse(" abcdefghijklm", Form::Plain);
        let kept = counted(&self::styled(&doc, false, 64), 128.0, usize::MAX);
        assert_eq!((kept.laid, kept.lines), (1, 1));
        // Cut within "cdefg" "h ij" after its first laid line, or before
        // it when that passes.
        let doc = parse("ab\ncdefgh ij\nk", Form::Plain);
        let styled = styled(&doc, false, 64);
        let cut = counted(&styled, 56.0, 3);
        assert_eq!((cut.end, cut.laid, cut.lines), (Some(12), 3, 3));
        let cut = counted(&styled, 56.0, 2);
        assert_eq!((cut.end, cut.laid, cut.lines), (Some(8), 2, 2));
        let cut = counted(&styled, 56.0, 1);
        assert_eq!((cut.end, cut.laid, cut.lines), (Some(2), 1, 1));
    }

    #[test]
    fn carriage_returns_end_lines_and_pair_with_a_following_newline() {
        let text = "ab\rcd\r\nef\n\rgh\r\r\nij";
        let doc = parse(text, Form::Escaped);
        let styled = styled(&doc, false, 64);
        assert_eq!(node_text(&styled), text);
        // ab, cd, ef, an empty line, gh, an empty line, ij.
        let whole = counted(&styled, 1280.0, usize::MAX);
        assert_eq!((whole.laid, laid(&styled, 1280.0)), (7, 7));
        for (cap, end) in [(1, 2), (2, 5), (3, 9), (4, 10), (5, 13), (6, 14)] {
            let cut = counted(&styled, 1280.0, cap);
            assert_eq!((cut.end, cut.laid), (Some(end), cap), "{cap}");
        }
    }

    #[test]
    fn a_long_line_is_laid_in_chunks_that_do_not_split_a_scalar() {
        // Its 16 KiB chunks end inside a three-byte scalar.
        let line = format!("xy{}", "界".repeat(9000));
        assert!(!line.is_char_boundary(16 * 1024));
        let doc = parse(&format!("{line}\nz"), Form::Plain);
        let styled = styled(&doc, false, 65_536);
        let whole = counted(&styled, 1280.0, usize::MAX);
        assert_eq!(whole.laid, laid(&styled, 1280.0));
        // 79 wide scalars a 1,264 px line, kept for 100 lines.
        let cut = counted(&styled, 1280.0, 100);
        assert_eq!(cut.laid, 100);
        let end = cut.end.unwrap();
        let doc = parse(&line[..end], Form::Plain);
        assert_eq!(laid(&self::styled(&doc, false, 65_536), 1280.0), 100);
    }

    #[test]
    fn counting_is_charged_cancelled_and_refuses_what_it_cannot_count() {
        let doc = parse(&"abc def\n".repeat(10_000), Form::Plain);
        let page = styled(&doc, false, 64);
        let whole = counted(&page, 1016.0, usize::MAX);
        let out = layout(&page, &mut Metrics, &Request::new(1016.0, 600.0)).unwrap();
        assert_eq!(whole.laid, out.line_count);
        // The breaking is layout's; the count builds no fragment.
        assert!(whole.work <= out.work, "{} {}", whole.work, out.work);
        let stop = AtomicBool::new(true);
        assert_eq!(
            fit(&page, &mut Metrics, 1016.0, 10, Some(&stop)),
            Err(Error::Cancelled)
        );
        let low = Limits {
            work: whole.work - 1,
            ..limits(64)
        };
        let page = StyledDocument::new(&doc, &sheets(false), MAX_INPUT, low).unwrap();
        assert_eq!(
            fit(&page, &mut Metrics, 1016.0, usize::MAX, None),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        );
        assert_eq!(
            fit(&page, &mut Metrics, 1016.0, 0, None),
            Err(Error::Invalid)
        );
        assert_eq!(
            fit(&page, &mut Metrics, f64::NAN, 1, None),
            Err(Error::Limit("MAX_VIEWPORT"))
        );
        // No text lays nothing.
        let doc = parse("", Form::Plain);
        assert_eq!(counted(&styled(&doc, false, 64), 100.0, 1).end, None);
        // What is not a text page, or holds what a line cannot count at
        // one fragment, is refused.
        for html in [
            "<pre>a\nb</pre>",
            "<div style=white-space:pre-wrap>a<br>b</div>",
            "<div style=white-space:pre-wrap>a<img>b</div>",
            "<div style=white-space:pre-wrap>a<span>b</span></div>",
            "<ul><li style=white-space:pre-wrap>a</ul>",
            "<table><td style=white-space:pre-wrap>a</table>",
            "<div style=white-space:pre-wrap>a</div><div>b</div>",
            "<!doctype html><plaintext>\u{1b}[31mred\u{1b}[0m x",
            "<!doctype html><plaintext>a\u{c}b",
        ] {
            let doc = td_html::document::parse(html).unwrap();
            assert_eq!(
                fit(&styled(&doc, false, 64), &mut Metrics, 100.0, 1, None),
                Err(Error::Invalid),
                "{html}"
            );
        }
    }
}
