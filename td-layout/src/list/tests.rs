#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::{layout, BoxGeometry, Layout, Request};
    use crate::inline::{replay, DisplayText, FontLine, FontMetrics, Line};
    use crate::list::*;
    use crate::{BoxKind, Limits, StyledDocument};
    use td_css::{ComputedStyle, Origin, StyleSet, HTML_UA_CSS};
    use td_html::document::Document;

    /// 8 px a scalar, a 20 px line with its baseline at 15.
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
    const BASE: &str = "body,ul,ol,p{margin:0}ul,ol{padding-left:40px}";
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[
            (Origin::UserAgent, HTML_UA_CSS),
            (Origin::Author, BASE),
            (Origin::Author, css),
        ])
        .unwrap()
    }
    fn styled<'d>(doc: &'d Document, html: &str, css: &str) -> StyledDocument<'d> {
        StyledDocument::new(doc, &sheets(css), html.len(), Limits::default()).unwrap()
    }
    fn lay(html: &str, css: &str, width: f64) -> (Document, Layout) {
        let doc = td_html::document::parse(html).unwrap();
        let out = layout(
            &styled(&doc, html, css),
            &mut Metrics,
            &Request::new(width, 600.0),
        )
        .unwrap();
        (doc, out)
    }
    /// With HTML's sheet alone: body's 8 px margin, a list's 16 px and 40 px.
    fn lay_ua(html: &str) -> (Document, Layout) {
        let doc = td_html::document::parse(html).unwrap();
        let sheets = StyleSet::new(&[(Origin::UserAgent, HTML_UA_CSS)]).unwrap();
        let styled = StyledDocument::new(&doc, &sheets, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        (doc, out)
    }
    /// Each list-item box's ordinal, in tree order.
    fn ordinals(html: &str, css: &str) -> Vec<i32> {
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, css);
        let ids: Vec<_> = (0..styled.boxes().len())
            .map(crate::BoxId)
            .filter(|id| styled.ordinal(*id).is_some())
            .collect();
        assert_eq!(ids.len(), styled.list_item_count());
        ids.into_iter()
            .map(|id| styled.ordinal(id).unwrap())
            .collect()
    }
    fn texts(out: &Layout) -> Vec<String> {
        out.markers
            .iter()
            .map(|m| m.text().as_str().to_string())
            .collect()
    }
    fn shown(doc: &Document, line: &Line) -> String {
        line.fragments
            .iter()
            .map(|f| match f.display {
                DisplayText::Source => &doc.node(f.node).unwrap().text().unwrap()[f.range.clone()],
                DisplayText::Space => " ",
                DisplayText::Replacement => "\u{fffd}",
            })
            .collect()
    }
    fn geometry<'a>(doc: &Document, out: &'a Layout, id: &str) -> &'a BoxGeometry {
        out.boxes
            .iter()
            .find(|g| {
                doc.node(g.node)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("id"))
                    == Some(id)
            })
            .unwrap()
    }

    #[test]
    fn counter_styles_and_their_ranges() {
        use MarkerKind::*;
        for (kind, value, want) in [
            (Disc, 7, "\u{2022} "),
            (Circle, -7, "\u{25e6} "),
            (Square, 0, "\u{25aa} "),
            (Decimal, 1, "1. "),
            (Decimal, 0, "0. "),
            (Decimal, -12, "-12. "),
            (Decimal, i32::MIN, "-2147483648. "),
            (Decimal, i32::MAX, "2147483647. "),
            (DecimalLeadingZero, 5, "05. "),
            (DecimalLeadingZero, 0, "00. "),
            (DecimalLeadingZero, 10, "10. "),
            // The negative sign counts toward the pad.
            (DecimalLeadingZero, -5, "-5. "),
            (LowerRoman, 1, "i. "),
            (LowerRoman, 4, "iv. "),
            (LowerRoman, 14, "xiv. "),
            (UpperRoman, 1994, "MCMXCIV. "),
            (UpperRoman, 3999, "MMMCMXCIX. "),
            (UpperRoman, 3888, "MMMDCCCLXXXVIII. "),
            // Out of 1..=3999, roman falls back to decimal.
            (LowerRoman, 4000, "4000. "),
            (UpperRoman, 0, "0. "),
            (UpperRoman, -3, "-3. "),
            (LowerAlpha, 1, "a. "),
            (LowerAlpha, 26, "z. "),
            (LowerAlpha, 27, "aa. "),
            (UpperAlpha, 702, "ZZ. "),
            (UpperAlpha, 703, "AAA. "),
            (LowerAlpha, i32::MAX, "fxshrxw. "),
            // Alphabetic's range starts at 1.
            (LowerAlpha, 0, "0. "),
            (UpperAlpha, -1, "-1. "),
        ] {
            assert_eq!(kind.text(value).as_str(), want, "{kind:?} {value}");
        }
        // No marker text passes the bound, which the longest meets exactly.
        let mut longest = 0;
        for kind in [
            Disc,
            Circle,
            Square,
            Decimal,
            DecimalLeadingZero,
            LowerRoman,
            UpperRoman,
            LowerAlpha,
            UpperAlpha,
        ] {
            for value in [i32::MIN, -1, 0, 1, 3888, 3999, 4000, i32::MAX]
                .into_iter()
                .chain((1..=4000).step_by(7))
            {
                let text = kind.text(value);
                assert!(!text.as_str().is_empty(), "{kind:?} {value}");
                longest = longest.max(text.as_str().len());
            }
        }
        assert_eq!(longest, MAX_MARKER_BYTES);
        assert_eq!(MarkerKind::from_keyword("none"), None);
        // Counter Styles 3's latin aliases.
        assert_eq!(MarkerKind::from_keyword("lower-latin"), Some(LowerAlpha));
        assert_eq!(MarkerKind::from_keyword("upper-latin"), Some(UpperAlpha));
        let (_, out) = lay(
            "<ol style='list-style:upper-latin inside' start=28><li>a</ol>",
            "",
            400.0,
        );
        assert_eq!(texts(&out), ["AB. "]);
        assert!(out.markers[0].inside);
    }

    #[test]
    fn integers_parse_as_html_says_and_clamp() {
        for (text, want) in [
            ("7", Some(7)),
            (" \t+7x", Some(7)),
            ("-3", Some(-3)),
            ("007", Some(7)),
            ("", None),
            ("-", None),
            ("x1", None),
            ("\u{a0}1", None),
            ("99999999999", Some(i32::MAX)),
            ("-99999999999", Some(i32::MIN)),
            (&"9".repeat(10_000), Some(i32::MAX)),
        ] {
            assert_eq!(parse_integer(text), want, "{text:?}");
        }
    }

    #[test]
    fn ordinals_follow_start_reversed_and_value() {
        for (html, want) in [
            ("<ol><li>a<li>b<li value=10>c<li>d</ol>", vec![1, 2, 10, 11]),
            ("<ol start=5><li>a<li>b</ol>", vec![5, 6]),
            ("<ol start=x><li>a<li>b</ol>", vec![1, 2]),
            ("<ol start=-2><li><li><li><li></ol>", vec![-2, -1, 0, 1]),
            ("<ol reversed><li>a<li>b<li>c</ol>", vec![3, 2, 1]),
            ("<ol reversed start=10><li>a<li>b</ol>", vec![10, 9]),
            ("<ol reversed><li>a<li value=7>b<li>c</ol>", vec![3, 7, 6]),
            // Only li elements count toward a reversed list's start, but every
            // list item takes a number.
            (
                "<ol reversed><li>a<div style=display:list-item>b</div><li>c</ol>",
                vec![2, 1, 0],
            ),
            // A ul's items count up from 1, and value applies there too.
            ("<ul reversed><li>a<li value=4>b<li>c</ul>", vec![1, 4, 5]),
            ("<ul start=5><li>a<li>b</ul>", vec![1, 2]),
            // value is an li's: another list item ignores it.
            (
                "<ol><li>a<div style=display:list-item value=9>b</div><li>c</ol>",
                vec![1, 2, 3],
            ),
            // display:none items are not owned; list-style-type none ones are.
            (
                "<ol><li>a<li style=display:none>b<li style=list-style-type:none>c<li>d</ol>",
                vec![1, 2, 3],
            ),
        ] {
            assert_eq!(ordinals(html, ""), want, "{html}");
        }
    }

    #[test]
    fn nested_lists_and_owners_number_independently() {
        for (html, want) in [
            ("<ol><li>a<ol><li>x<li>y</ol><li>b</ol>", vec![1, 1, 2, 2]),
            (
                "<ol reversed><li>a<li>b<ol reversed><li>x<li>y<li>z</ol><li>c</ol>",
                vec![3, 2, 3, 2, 1, 1],
            ),
            ("<ol start=3><li>a<ul><li>x</ul><li>b</ol>", vec![3, 1, 4]),
            // The owner is the closest ol, ul or menu ancestor, however deep.
            (
                "<ol><li>a<div><span style=display:list-item>x</span></div><li>b</ol>",
                vec![1, 2, 3],
            ),
            // A's parent owns a and c, b's parent b, though CSS's counter
            // would run 1, 2, 3 through a's following sibling.
            (
                "<div><li>a</li><section><li>b</section><li>c</div>",
                vec![1, 1, 2],
            ),
            // dir is not an owner, though HTML's sheet styles it as a list.
            ("<ol><li>a<dir><li>x</dir><li>b</ol>", vec![1, 2, 3]),
            // Without one, it is the parent.
            (
                "<div><li>a<li>b</div><div><li>c</div><menu><li>d<li>e</menu>",
                vec![1, 2, 1, 1, 2],
            ),
            // An inline list still owns its items.
            (
                "<ol style=display:inline start=4><li>a<li>b</ol><ol><li>c</ol>",
                vec![4, 5, 1],
            ),
        ] {
            assert_eq!(ordinals(html, ""), want, "{html}");
        }
    }

    #[test]
    fn counters_clamp_at_i32() {
        let max = i32::MAX;
        let min = i32::MIN;
        for (html, want) in [
            ("<ol start=99999999999><li><li></ol>", vec![max, max]),
            (
                "<ol start=2147483646><li><li><li></ol>",
                vec![max - 1, max, max],
            ),
            (
                "<ol reversed start=-99999999999><li><li></ol>",
                vec![min, min],
            ),
            (
                "<ol><li value=2147483647><li><li value=-2147483649><li></ol>",
                vec![max, max, min, min + 1],
            ),
        ] {
            assert_eq!(ordinals(html, ""), want, "{html}");
        }
        let (_, out) = lay(
            "<ol style=list-style-type:upper-roman start=99999999999><li>a<li>b</ol>",
            "",
            400.0,
        );
        assert_eq!(texts(&out), ["2147483647. ", "2147483647. "]);
        assert_eq!(out.markers[0].width, 12.0 * 8.0);
    }

    #[test]
    fn bullets_hang_outside_without_taking_line_space() {
        let html = "<ul><li id=a>one two<li id=b>three</ul>";
        let (doc, out) = lay(html, "", 400.0);
        assert_eq!(texts(&out), ["\u{2022} ", "\u{2022} "]);
        let a = geometry(&doc, &out, "a");
        for (marker, line) in out.markers.iter().zip(&out.lines) {
            assert!(!marker.inside);
            assert_eq!(marker.style, a.style);
            // Its end abuts the item's border edge; the line starts there.
            assert_eq!(marker.x + marker.width, a.border.x);
            assert_eq!(marker.width, 16.0);
            assert_eq!(line.fragments[0].x, 40.0);
            assert_eq!(marker.baseline, line.y + line.baseline);
            assert_eq!((marker.y, marker.height), (line.y, 20.0));
        }
        assert_eq!(out.markers[0].line, Some(0));
        assert_eq!(out.markers[1].line, Some(1));
        // The border edge, not the padding or content edge, and after the
        // item's margin.
        let (doc, out) = lay(
            "<ul><li id=a style='margin-left:3px;border-left:5px solid;padding-left:10px'>x</ul>",
            "",
            400.0,
        );
        let a = geometry(&doc, &out, "a");
        assert_eq!(a.border.x, 43.0);
        assert_eq!(out.markers[0].x + out.markers[0].width, 43.0);
        assert_eq!(out.lines[0].fragments[0].x, 58.0);
        // A marker hanging past the page's start is in its overflow.
        let (_, flush) = lay(html, "ul{padding:0}", 400.0);
        assert_eq!(flush.markers[0].x, -16.0);
        assert_eq!(flush.overflow.x, -16.0);
        // The lines are those of the same list without markers, wrapped too.
        for width in [400.0, 60.0] {
            let (_, plain) = lay(html, "li{list-style-type:none}", width);
            let (_, marked) = lay(html, "", width);
            assert!(plain.markers.is_empty());
            assert_eq!(plain.lines.len(), marked.lines.len());
            assert!(plain.lines.iter().eq(marked.lines.iter()));
            assert_eq!(plain.boxes, marked.boxes);
        }
        // Nested lists: circle, then square.
        let (_, out) = lay("<ul><li>a<ul><li>b<ul><li>c</ul></ul></ul>", "", 400.0);
        assert_eq!(texts(&out), ["\u{2022} ", "\u{25e6} ", "\u{25aa} "]);
    }

    #[test]
    fn nested_lists_take_html_s_bullets() {
        // HTML §15.3.7: a ul, menu or dir in any list is circle, in two
        // square; an ol keeps decimal at any depth.
        let (disc, circle, square) = ("\u{2022} ", "\u{25e6} ", "\u{25aa} ");
        for (html, want) in [
            ("<ol><li>a<ul><li>b</ul></ol>", vec!["1. ", circle]),
            (
                "<ol><li>a<ol><li>b<ul><li>c</ul></ol></ol>",
                vec!["1. ", "1. ", square],
            ),
            (
                "<ul><li>a<ol><li>b<ul><li>c</ul></ol></ul>",
                vec![disc, "1. ", square],
            ),
            ("<menu><li>a<menu><li>b</menu></menu>", vec![disc, circle]),
            ("<ul><li>a<menu><li>b</menu></ul>", vec![disc, circle]),
            (
                "<ul><li>a<ul><li>b<ul><li>c<ul><li>d</ul></ul></ul></ul>",
                vec![disc, circle, square, square],
            ),
            // Only a list ancestor counts: a div between changes nothing.
            ("<ul><li><div><ul><li>a</ul></div></ul>", vec![disc, circle]),
            // The type attribute is a presentational hint, not mapped.
            (
                "<ol type=a><li>a</ol><ul type=square><li>b</ul>",
                vec!["1. ", disc],
            ),
        ] {
            let (_, out) = lay(html, "", 400.0);
            assert_eq!(texts(&out), want, "{html}");
        }
    }

    #[test]
    fn html_s_sheet_lays_menu_and_dir_as_lists() {
        // Block, 16 px above and below, 40 px of padding and a disc.
        for html in ["<menu><li id=a>one</menu>", "<dir><li id=a>one</dir>"] {
            let (doc, out) = lay_ua(html);
            let a = geometry(&doc, &out, "a");
            assert_eq!((a.border.x, a.border.y), (48.0, 24.0), "{html}");
            assert_eq!(texts(&out), ["\u{2022} "], "{html}");
            assert_eq!((out.markers[0].x, out.markers[0].y), (32.0, 24.0));
        }
        // Each sets disc rather than inheriting a type.
        let (_, out) = lay_ua(
            "<div style=list-style-type:decimal><dir><li>a</dir><menu><li>b</menu><ul><li>c</ul></div>",
        );
        assert_eq!(texts(&out), ["\u{2022} "; 3]);
        // A menu in a list item: its circle 40 px in from the disc, not on it.
        let (_, out) = lay_ua("<ul><li><menu><li>b</menu></ul>");
        assert_eq!(texts(&out), ["\u{2022} ", "\u{25e6} "]);
        let at: Vec<_> = out.markers.iter().map(|m| (m.x, m.y)).collect();
        assert_eq!(at, [(32.0, 24.0), (72.0, 24.0)]);
    }

    #[test]
    fn html_s_sheet_drops_a_nested_list_s_margins() {
        // The outer list's 16 px stand; the nested one adds none.
        let (doc, out) = lay_ua("<ul><li>a<ul id=n><li>b</ul><li id=c>c</ul><p id=p>d");
        let n = geometry(&doc, &out, "n");
        assert_eq!((n.border.y, n.border.height), (44.0, 20.0));
        assert_eq!(geometry(&doc, &out, "c").border.y, 64.0);
        assert_eq!(geometry(&doc, &out, "p").border.y, 84.0 + 16.0);
        // dl, menu and dir count on either side.
        for html in [
            "<ol><li>a<dl id=n><dt>t</dl></ol>",
            "<dl><dd>a<menu id=n><li>b</menu></dl>",
            "<dir><li>a<ol id=n><li>b</ol></dir>",
        ] {
            let (doc, out) = lay_ua(html);
            assert_eq!(geometry(&doc, &out, "n").border.y, 44.0, "{html}");
        }
        // Any list ancestor, through a div too; a list in a div keeps them.
        let (doc, out) = lay_ua("<ul><li>a<div><ul id=n><li>b</ul></div></ul>");
        assert_eq!(geometry(&doc, &out, "n").border.y, 44.0);
        let (doc, out) = lay_ua("<div>a<ul id=n><li>b</ul></div>");
        assert_eq!(geometry(&doc, &out, "n").border.y, 28.0 + 16.0);
    }

    #[test]
    fn marker_line_and_pending_sizes_hold() {
        // The per-fragment storage the limits and §6 are sized by.
        use std::mem::size_of;
        assert_eq!(size_of::<MarkerFragment>(), 72);
        assert_eq!(size_of::<Line>(), 136);
        assert_eq!(size_of::<crate::inline::Pending>(), 120);
    }

    #[test]
    fn decimal_markers_take_their_ordinals() {
        let (_, out) = lay("<ol start=9><li>a<li>b<li value=100>c</ol>", "", 400.0);
        assert_eq!(texts(&out), ["9. ", "10. ", "100. "]);
        assert_eq!(out.markers[1].width, 32.0);
        assert_eq!(out.markers[1].x, 40.0 - 32.0);
        let (_, out) = lay(
            "<ol style=list-style-type:lower-alpha><li>a<li>b</ol>\
         <ol style=list-style-type:upper-roman reversed><li>a<li>b<li>c<li>d</ol>",
            "",
            400.0,
        );
        assert_eq!(texts(&out), ["a. ", "b. ", "IV. ", "III. ", "II. ", "I. "]);
    }

    #[test]
    fn none_makes_no_marker_but_keeps_the_count() {
        let (_, out) = lay(
            "<ol><li>a<li style=list-style-type:none>b<li>c</ol>",
            "",
            400.0,
        );
        assert_eq!(texts(&out), ["1. ", "3. "]);
        assert_eq!(out.markers[1].line, Some(2));
        let (_, out) = lay("<ul style='list-style:none'><li>a<li>b</ul>", "", 400.0);
        assert!(out.markers.is_empty());
    }

    #[test]
    fn inside_markers_lead_the_first_line_and_wrap_with_it() {
        let html = "<ol style=list-style-position:inside><li id=a>aa bb cc</ol>";
        let (doc, out) = lay(html, "", 400.0);
        let a = geometry(&doc, &out, "a");
        let marker = out.markers[0];
        assert!(marker.inside);
        assert_eq!(marker.line, Some(0));
        assert_eq!((marker.x, marker.width), (a.content.x, 24.0));
        assert_eq!(out.lines[0].fragments[0].x, a.content.x + 24.0);
        assert_eq!(out.lines[0].width, 24.0 + 64.0);
        assert_eq!(shown(&doc, &out.lines[0]), "aa bb cc");
        // At 40 + 64 px, "aa bb" fits beside the marker and "cc" wraps to the
        // content edge, not under the text.
        let (doc, out) = lay(html, "", 104.0);
        assert_eq!(
            out.lines.iter().map(|l| shown(&doc, l)).collect::<Vec<_>>(),
            ["aa bb", "cc"]
        );
        assert_eq!(out.lines[1].fragments[0].x, 40.0);
        assert_eq!(out.markers[0].line, Some(0));
        // A word that does not fit beside it moves whole to the next line.
        let (doc, out) = lay(
            "<ol style=list-style-position:inside><li>abcdefgh</ol>",
            "",
            104.0,
        );
        assert_eq!(
            out.lines.iter().map(|l| shown(&doc, l)).collect::<Vec<_>>(),
            ["", "abcdefgh"]
        );
        // Text alignment moves an inside marker with its line, and leaves an
        // outside one at the item's edge.
        let html = "<ul><li id=a>ab</ul>";
        for (css, x) in [
            ("li{text-align:center}", 24.0),
            (
                "li{text-align:center;list-style-position:inside}",
                40.0 + (360.0 - 32.0) / 2.0,
            ),
        ] {
            let (_, out) = lay(html, css, 400.0);
            assert_eq!(out.markers[0].x, x, "{css}");
        }
        // Collapsible spaces after it go, as at a line's start.
        let (doc, out) = lay(
            "<ul style=list-style-position:inside><li>  x</ul>",
            "",
            400.0,
        );
        assert_eq!(shown(&doc, &out.lines[0]), "x");
        assert_eq!(out.lines[0].fragments[0].x, 40.0 + 16.0);
    }

    #[test]
    fn a_block_first_child_carries_the_marker_on_its_first_line() {
        let html = "<ul><li id=a><p id=p style=margin-top:10px>text<p>more</ul>";
        let (doc, out) = lay(html, "", 400.0);
        let a = geometry(&doc, &out, "a");
        let p = geometry(&doc, &out, "p");
        let marker = out.markers[0];
        assert_eq!(marker.line, Some(0));
        assert_eq!(out.lines[0].y, p.content.y);
        assert_eq!(marker.baseline, out.lines[0].y + 15.0);
        assert_eq!(marker.x + marker.width, a.border.x);
        // A relative child moves the line, and the marker with its top
        // but not its left: the end stays at the item's border edge.
        let (doc, out) = lay(
            "<ul><li id=a><p style=position:relative;top:10px;left:20px>x</ul>",
            "",
            400.0,
        );
        let a = geometry(&doc, &out, "a");
        assert_eq!(out.lines[0].y, a.content.y + 10.0);
        assert_eq!(out.lines[0].fragments[0].x, a.content.x + 20.0);
        assert_eq!(out.markers[0].baseline, out.lines[0].y + 15.0);
        assert_eq!(out.markers[0].x + out.markers[0].width, a.border.x);
        // Nested items' markers share the first line, outermost first.
        let (_, out) = lay("<ul><li><ul><li>x</ul></ul>", "ul ul{margin:0}", 400.0);
        assert_eq!(out.markers.len(), 2);
        assert_eq!(out.markers[0].line, Some(0));
        assert_eq!(out.markers[1].line, Some(0));
        assert_eq!(out.markers[0].baseline, out.markers[1].baseline);
        assert_eq!(out.markers[1].x - out.markers[0].x, 40.0);
        // Inside, the marker is inline content before the block: a line of
        // its own.
        let (doc, out) = lay(html, "li{list-style-position:inside}", 400.0);
        assert_eq!(out.markers[0].line, Some(0));
        assert!(out.lines[0].fragments.is_empty());
        assert_eq!(shown(&doc, &out.lines[1]), "text");
        assert_eq!(out.lines[1].y, 20.0 + 10.0);
    }

    #[test]
    fn an_empty_item_still_shows_its_marker() {
        let html = "<ol><li id=a></li><li id=b> </li><li id=c>c</ol>";
        let (doc, out) = lay(html, "", 400.0);
        let (a, b, c) = (
            geometry(&doc, &out, "a"),
            geometry(&doc, &out, "b"),
            geometry(&doc, &out, "c"),
        );
        assert_eq!(texts(&out), ["1. ", "2. ", "3. "]);
        for (marker, item) in out.markers.iter().zip([a, b]) {
            // No line: at the content top, which it holds open.
            assert_eq!(marker.line, None);
            assert_eq!(marker.y, item.content.y);
            assert_eq!(marker.baseline, item.content.y + 15.0);
            assert_eq!(item.content.height, 20.0);
        }
        assert_eq!(c.content.y, 40.0);
        assert_eq!(out.markers[2].line, Some(0));
        assert_eq!(out.lines.len(), 1);
        // An item of a block image holds no line either: the marker stands at
        // its top and the image's height stands.
        let (doc, out) = lay(
            "<ul><li id=a><img style=display:block width=10 height=50></ul>",
            "",
            400.0,
        );
        assert_eq!(out.markers[0].line, None);
        assert_eq!(geometry(&doc, &out, "a").content.height, 50.0);
        // Inside, an empty item lays one line holding its marker.
        let (doc, out) = lay(html, "li{list-style-position:inside}", 400.0);
        assert_eq!(out.lines.len(), 3);
        assert_eq!(
            out.markers.iter().map(|m| m.line).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(2)]
        );
        assert_eq!(geometry(&doc, &out, "a").content.height, 20.0);
        assert!(out.lines[0].fragments.is_empty());
        assert_eq!(out.lines[0].width, 24.0);
        // An empty item's marker hanging past the page's start is in its
        // overflow, as a line's is.
        let (_, out) = lay("<ul style=padding:0><li></ul>", "", 400.0);
        assert_eq!(out.markers[0].line, None);
        assert_eq!(out.markers[0].x, -16.0);
        assert_eq!(out.overflow.x, -16.0);
    }

    #[test]
    fn markers_have_no_dom_bytes() {
        let html = "<ol><li>alpha beta<li><p>gamma</ol>\
                <ul style=list-style-position:inside><li>delta<li></ul>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, "");
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        assert_eq!(out.markers.len(), 4);
        let mut bytes = String::new();
        for (index, line) in out.lines.iter().enumerate() {
            for f in &line.fragments {
                // Every fragment is a text node's bytes, and replays to them.
                let text = doc.node(f.node).unwrap().text().unwrap();
                let block = styled
                    .boxes()
                    .iter()
                    .find(|b| b.kind == BoxKind::Block)
                    .unwrap()
                    .style;
                replay(&styled, &mut Metrics, f, block, |r| {
                    bytes.push_str(&text[r.range.clone()]);
                })
                .unwrap();
            }
            // A marker's line holds it apart from its fragments.
            for marker in out.markers.iter().filter(|m| m.line == Some(index as u32)) {
                for f in &line.fragments {
                    assert!(f.x >= marker.x + marker.width || f.x + f.width <= marker.x);
                }
            }
        }
        assert_eq!(
            bytes,
            "alphabetagammadelta".replace("alphabeta", "alpha beta")
        );
        for marker in out.markers.iter() {
            // It names its item, an element, never a text node.
            let node = doc.node(marker.item).unwrap();
            assert_eq!(node.element().unwrap().name.as_str(), "li");
            assert!(node.text().is_none());
        }
        // Inside markers shift the fragments, not their bytes: the same
        // ranges as with no marker.
        let ranges = |css: &str| {
            let (_, out) = lay(html, css, 400.0);
            out.lines
                .iter()
                .flat_map(|l| l.fragments.iter().map(|f| (f.node, f.range.clone())))
                .collect::<Vec<_>>()
        };
        assert_eq!(ranges(""), ranges("li{list-style-type:none}"));
    }

    #[test]
    fn windows_keep_the_markers_of_the_lines_they_keep() {
        let items: String = (0..40).map(|i| format!("<li>item {i}")).collect();
        let html = format!("<ol>{items}<li></ol><ul style=list-style-position:inside>{items}</ul>");
        let doc = td_html::document::parse(&html).unwrap();
        let styled = styled(&doc, &html, "");
        let full = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        assert_eq!(full.markers.len(), 81);
        for window in [300.0..700.0, 790.0..830.0, 0.0..0.0] {
            let mut request = Request::new(400.0, 600.0);
            request.window = Some(window.clone());
            let part = layout(&styled, &mut Metrics, &request).unwrap();
            let wanted = |y: f64, h: f64| {
                window.start < window.end && y + h > window.start && y < window.end
            };
            let kept: Vec<_> = full
                .markers
                .iter()
                .filter(|m| match m.line {
                    Some(line) => {
                        let line = &full.lines[line as usize];
                        wanted(line.y, line.height)
                    }
                    None => wanted(m.y, m.height),
                })
                .map(|m| (m.item, m.ordinal, m.x, m.y))
                .collect();
            let got: Vec<_> = part
                .markers
                .iter()
                .map(|m| (m.item, m.ordinal, m.x, m.y))
                .collect();
            assert_eq!(got, kept, "{window:?}");
            for m in part.markers.iter() {
                if let Some(line) = m.line {
                    let line = &part.lines[line as usize];
                    assert_eq!(m.baseline, line.y + line.baseline);
                }
            }
            assert_eq!(part.overflow, full.overflow);
            assert_eq!(part.work, full.work);
        }
    }

    #[test]
    fn markers_are_charged_work_fragments_and_bytes() {
        let html = format!("<ol>{}</ol>", "<li>x".repeat(100));
        let doc = td_html::document::parse(&html).unwrap();
        let marked = styled(&doc, &html, "");
        let plain = styled(&doc, &html, "li{list-style-type:none}");
        // The ordinals cost the styled document the same either way.
        assert_eq!(marked.charged_bytes(), plain.charged_bytes());
        let with = layout(&marked, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        let without = layout(&plain, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        // Each scalar of "1. " … "100. " measured, and each marker placed.
        let scalars: usize = (1..=100).map(|n: usize| n.to_string().len() + 2).sum();
        assert_eq!(with.work - without.work, scalars + 100);
        assert!(with.output_bytes > without.output_bytes);
        // Markers count as fragments.
        let fragments = |limit| {
            let limits = Limits {
                fragments: limit,
                ..Limits::default()
            };
            let styled = StyledDocument::new(&doc, &sheets(""), html.len(), limits).unwrap();
            layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).map(|l| l.markers.len())
        };
        assert_eq!(fragments(200), Ok(100));
        assert_eq!(fragments(199), Err(Error::Limit("MAX_FRAGMENTS")));
        // The ordinal records are charged to the styled document, by name.
        let none = td_html::document::parse("<p>x").unwrap();
        let base = StyledDocument::new(&none, &sheets(""), 4, Limits::default())
            .unwrap()
            .charged_bytes();
        let list = td_html::document::parse("<p>x<ol><li></ol>").unwrap();
        let one = StyledDocument::new(&list, &sheets(""), 4, Limits::default()).unwrap();
        assert!(one.charged_bytes() > base);
        for (bytes, ok) in [
            (one.charged_bytes(), true),
            (one.charged_bytes() - 1, false),
        ] {
            let result = StyledDocument::new(
                &list,
                &sheets(""),
                4,
                Limits {
                    bytes,
                    ..Limits::default()
                },
            );
            assert_eq!(result.is_ok(), ok);
            if !ok {
                assert!(matches!(result, Err(Error::Limit("MAX_PAGE_BYTES"))));
            }
        }
    }
}
