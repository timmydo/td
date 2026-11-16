#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::{layout, Request};
    use crate::inline::{wrap_text, FontLine, FontMetrics, Line, LineEnd, TextRun};
    use crate::{BoxKind, Limits, StyledDocument};
    use td_css::{ComputedStyle, Origin, StyleSet, HTML_UA_CSS};
    use td_html::document::Document;
    use LineEnd::{Break, End, Newline, Split, Wrap};

    /// 8 px a scalar, a 20 px line.
    struct Metrics;
    impl FontMetrics for Metrics {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            8 * 64
        }
        fn line(&mut self, _: &ComputedStyle) -> FontLine {
            FontLine {
                height: 20 * 64,
                baseline: 15 * 64,
            }
        }
    }
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[
            (Origin::UserAgent, HTML_UA_CSS),
            (Origin::Author, "*{margin:0;padding:0;border-width:0}"),
            (Origin::Author, css),
        ])
        .unwrap()
    }
    /// The block's id, or its element's name, a line's text, a `#` for each
    /// replaced fragment, and its end.
    fn describe(
        doc: &Document,
        styled: &StyledDocument<'_>,
        line: &Line,
    ) -> (String, String, LineEnd) {
        let block = line.block().map_or(String::from("-"), |id| {
            let b = styled.box_node(id).unwrap();
            assert_eq!(styled.boxes()[id.index()].node, b.node);
            let element = doc.node(b.node).unwrap().element().unwrap();
            element
                .attribute("id")
                .map_or_else(|| element.name.to_string(), str::to_string)
        });
        let text = line
            .fragments
            .iter()
            .map(|f| &doc.node(f.node).unwrap().text().unwrap()[f.range.clone()])
            .chain(line.replaced.iter().map(|_| "#"))
            .collect();
        (block, text, line.end())
    }
    fn ends(html: &str, css: &str, width: f64) -> Vec<(String, String, LineEnd)> {
        ends_at(html, css, width, Limits::default().line_scalars)
    }
    fn ends_at(
        html: &str,
        css: &str,
        width: f64,
        line_scalars: usize,
    ) -> Vec<(String, String, LineEnd)> {
        let doc = td_html::document::parse(html).unwrap();
        let limits = Limits {
            line_scalars,
            ..Limits::default()
        };
        let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits).unwrap();
        let request = Request::new(width, 2000.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        // A window carries them as the whole layout does.
        for window in [0.0..30.0, 15.0..65.0, 40.0..2000.0] {
            let mut windowed = request.clone();
            windowed.window = Some(window.clone());
            let out = layout(&styled, &mut Metrics, &windowed).unwrap();
            let kept: Vec<&Line> = full
                .lines
                .iter()
                .filter(|l| l.y + l.height > window.start && l.y < window.end)
                .collect();
            assert_eq!(out.lines.iter().collect::<Vec<_>>(), kept);
            let tags = |lines: &mut dyn Iterator<Item = &Line>| {
                lines.map(|l| (l.block(), l.end())).collect::<Vec<_>>()
            };
            assert_eq!(tags(&mut out.lines.iter()), tags(&mut kept.iter().copied()));
        }
        full.lines
            .iter()
            .map(|line| describe(&doc, &styled, line))
            .collect()
    }
    fn expect(cases: &[(&str, &str, LineEnd)]) -> Vec<(String, String, LineEnd)> {
        cases
            .iter()
            .map(|(b, t, e)| (b.to_string(), t.to_string(), *e))
            .collect()
    }

    #[test]
    fn paragraphs_wrap_split_and_end() {
        assert_eq!(
            ends("<p id=a>one two three<p id=b>abcdefghij", "", 40.0),
            expect(&[
                ("a", "one", Wrap),
                ("a", "two", Wrap),
                ("a", "three", End),
                ("b", "abcde", Split),
                ("b", "fghij", End),
            ])
        );
    }
    #[test]
    fn breaks_and_preserved_newlines() {
        // A last br leaves an empty line after it, ended with its block.
        assert_eq!(
            ends("<p id=a>one<br>two<br>", "", 400.0),
            expect(&[("a", "one", Break), ("a", "two", Break), ("a", "", End)])
        );
        assert_eq!(
            ends("<pre id=a>one\ntwo\r\nthree\rfour\n</pre>", "", 400.0),
            expect(&[
                ("a", "one", Newline),
                ("a", "two", Newline),
                ("a", "three", Newline),
                ("a", "four", Newline),
                ("a", "", End),
            ])
        );
        // Pre-wrap wraps at spaces and keeps its newlines.
        assert_eq!(
            ends(
                "<div id=a>abc def\nxy\n\nz</div>",
                "div{white-space:pre-wrap}",
                40.0
            ),
            expect(&[
                ("a", "abc", Wrap),
                ("a", "def", Newline),
                ("a", "xy", Newline),
                ("a", "", Newline),
                ("a", "z", End),
            ])
        );
    }
    #[test]
    fn inline_containers_list_items_and_nested_blocks() {
        assert_eq!(
            ends(
                "<p id=a>one <span>two <b>three</b></span> four</p>",
                "",
                40.0
            ),
            expect(&[
                ("a", "one", Wrap),
                ("a", "two", Wrap),
                ("a", "three", Wrap),
                ("a", "four", End),
            ])
        );
        // Outside and inside markers: each item's lines name the item; the
        // inside marker wraps alone before a word it cannot hold.
        assert_eq!(
            ends(
                "<ul><li id=a>one two<li id=b>three</ul>",
                "#b{list-style-position:inside}",
                40.0
            ),
            expect(&[
                ("a", "one", Wrap),
                ("a", "two", End),
                ("b", "", Wrap),
                ("b", "three", End),
            ])
        );
        // Text around a block names its own block, ended where the block
        // interrupts it.
        assert_eq!(
            ends("<div id=o>before<p id=i>inner</p>after</div>", "", 400.0),
            expect(&[
                ("o", "before", End),
                ("i", "inner", End),
                ("o", "after", End)
            ])
        );
        // Empty blocks lay no line; a lone br lays its own and the empty
        // one after it.
        assert_eq!(
            ends(
                "<p id=a></p><div id=b><p></p></div><p id=c><br></p>",
                "",
                400.0
            ),
            expect(&[("c", "", Break), ("c", "", End)])
        );
    }
    #[test]
    fn a_full_line_splits_its_word_and_wraps_at_a_dropped_space() {
        // The dropped space is the opportunity, wide or narrow, in a run
        // that wraps or one that does not.
        for width in [400.0, 24.0] {
            assert_eq!(
                ends_at("<p id=a>abcdef gh", "", width, 3),
                expect(&[("a", "abc", Split), ("a", "def", Wrap), ("a", "gh", End)])
            );
            assert_eq!(
                ends_at(
                    "<p id=a><span style=white-space:nowrap>abcdef</span> gh",
                    "",
                    width,
                    3
                ),
                expect(&[("a", "abc", Wrap), ("a", "gh", End)])
            );
        }
        // A full line breaks at its last opportunity.
        assert_eq!(
            ends_at("<p id=a>ab cdefg", "", 400.0, 4),
            expect(&[("a", "ab", Wrap), ("a", "cdef", Split), ("a", "g", End)])
        );
    }
    #[test]
    fn a_trimmed_space_that_does_not_wrap_is_no_split() {
        for (html, width, lines) in [
            (
                "<p id=a>one <span style=\"white-space:nowrap\">Hello world </span>again</p>",
                80.0,
                &[
                    ("a", "one", Wrap),
                    ("a", "Hello world", Wrap),
                    ("a", "again", End),
                ][..],
            ),
            (
                "<p id=a>Call <span style=\"white-space:nowrap\">+1 555 0100 </span>now</p>",
                100.0,
                &[
                    ("a", "Call", Wrap),
                    ("a", "+1 555 0100", Wrap),
                    ("a", "now", End),
                ],
            ),
            (
                "<p id=a><code style=\"white-space:pre\">let x = 1; </code>then more</p>",
                80.0,
                &[("a", "let x = 1;", Wrap), ("a", "then more", End)],
            ),
            (
                "<p id=a><span style=white-space:nowrap>one </span> two",
                32.0,
                &[("a", "one", Wrap), ("a", "two", End)],
            ),
            (
                "<pre id=a>ab  ab  <b style=\"white-space:normal\">bxbdd</b></pre>",
                56.0,
                &[("a", "ab  ab", Wrap), ("a", "bxbdd", End)],
            ),
        ] {
            assert_eq!(ends(html, "", width), expect(lines), "{html}");
        }
        // A space that does not wrap, dropped past a cut, before text that
        // wraps.
        assert_eq!(
            ends_at(
                "<p id=a><span style=\"white-space:nowrap\">abc </span>def</p>",
                "",
                400.0,
                3
            ),
            expect(&[("a", "abc", Wrap), ("a", "def", End)])
        );
        assert_eq!(
            ends_at(
                "<pre id=a>abcdef    <span style=\"white-space:normal\">gh</span></pre>",
                "",
                400.0,
                6
            ),
            expect(&[("a", "abcdef", Wrap), ("a", "gh", End)])
        );
        // A tail still too wide once moved past its space splits before
        // the letter that continues its word.
        assert_eq!(
            ends(
                "<p id=a>a bbbb<span style=\"letter-spacing:20px\">c</span></p>",
                "",
                50.0
            ),
            expect(&[("a", "a", Wrap), ("a", "bbbb", Split), ("a", "c", End)])
        );
        // A run that does not wrap, wider than the line, moved whole past a
        // space, then split from the soft letter that continues its word.
        assert_eq!(
            ends(
                "<p id=a>a <span style=white-space:nowrap>bcdefgh</span>i",
                "",
                40.0
            ),
            expect(&[("a", "a", Wrap), ("a", "bcdefgh", Split), ("a", "i", End)])
        );
        // A word wider than the line splits after leading spaces it drops.
        assert_eq!(
            ends(
                "<div id=a>   abcdefgh</div>",
                "div{white-space:pre-wrap}",
                40.0
            ),
            expect(&[("a", "abcde", Split), ("a", "fgh", End)])
        );
    }
    #[test]
    fn image_and_marker_edges_wrap() {
        for (html, width, scalars, lines) in [
            (
                "<div id=a><img style=white-space:nowrap width=40>bccex</div>",
                40.0,
                65_536,
                &[("a", "#", Wrap), ("a", "bccex", End)][..],
            ),
            (
                "<p id=a>abc<img width=8>d",
                400.0,
                3,
                &[("a", "abc", Wrap), ("a", "d#", End)],
            ),
            (
                "<p id=a>abc<img width=8>d",
                24.0,
                65_536,
                &[("a", "abc", Wrap), ("a", "d#", End)],
            ),
            (
                "<p id=a>abcd<img width=8><img width=8>",
                400.0,
                65_536,
                &[("a", "abcd##", End)],
            ),
        ] {
            assert_eq!(ends_at(html, "", width, scalars), expect(lines), "{html}");
        }
        // An inside marker alone on its line ends it at its edge.
        assert_eq!(
            ends(
                "<ul><li id=a><span style=white-space:normal>abcde</span></ul>",
                "li{list-style-position:inside;white-space:nowrap}",
                16.0
            ),
            expect(&[
                ("a", "", Wrap),
                ("a", "ab", Split),
                ("a", "cd", Split),
                ("a", "e", End),
            ])
        );
    }
    #[test]
    fn table_cells_name_their_cell() {
        assert_eq!(
            ends(
                "<table><tr><td id=a>one two<td id=b>three</tr><tr><td id=c>four</table>",
                "",
                40.0
            ),
            expect(&[
                ("a", "one", Wrap),
                ("a", "two", End),
                ("b", "three", End),
                ("c", "four", End),
            ])
        );
        // An anonymous cell's lines name the box holding its run.
        assert_eq!(
            ends(
                "<div style=display:table><div id=r style=display:table-row>loose</div></div>",
                "",
                400.0
            ),
            expect(&[("r", "loose", End)])
        );
        // The row's own box, not its run's first.
        let html = "<div style=display:table><div id=r style=display:table-row>loose</div></div>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 100.0)).unwrap();
        let row = (0..styled.boxes().len())
            .find(|&i| {
                doc.node(styled.boxes()[i].node)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("id"))
                    == Some("r")
            })
            .unwrap();
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].block().map(|b| b.index()), Some(row));
        // A caption and a spanning cell name their own boxes.
        assert_eq!(
            ends(
                "<table><caption id=c>cap</caption><tr><td id=a rowspan=2>x<td id=b>y<tr><td id=d>z</table>",
                "",
                400.0
            ),
            expect(&[
                ("c", "cap", End),
                ("a", "x", End),
                ("b", "y", End),
                ("d", "z", End),
            ])
        );
    }
    #[test]
    fn wrapped_text_names_no_block_and_box_ids_index_boxes() {
        let html = "<p>one two";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let text = styled
            .boxes()
            .iter()
            .find(|b| b.kind == BoxKind::Text)
            .unwrap();
        let run = TextRun {
            node: text.node,
            range: 0..7,
            style: text.style,
        };
        let out = wrap_text(&styled, &mut Metrics, &[run], 40, text.style).unwrap();
        let got: Vec<_> = out.lines.iter().map(|l| (l.block(), l.end())).collect();
        assert_eq!(got, [(None, Wrap), (None, End)]);
        let mut id = styled.first_box();
        let mut seen = 0;
        while let Some(at) = id {
            assert!(at.index() < styled.boxes().len());
            seen += 1;
            id = styled.box_node(at).unwrap().first_child;
        }
        assert!(seen > 1);
    }
}
