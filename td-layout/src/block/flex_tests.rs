//! Flex layout (DESIGN.md §5, "Flex"). Each scalar advances 8 px and a
//! 16 px line is 20 px tall with its baseline 15 px down, so widths,
//! tops and offsets are worked by hand.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::*;
    use crate::inline::FontLine;
    use crate::Limits;
    use td_css::{Origin, StyleSet, HTML_UA_CSS};

    struct Metrics;
    impl FontMetrics for Metrics {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            8 * 64
        }
        fn line(&mut self, s: &ComputedStyle) -> FontLine {
            FontLine {
                height: (s.font_size() * 80.0) as u32,
                baseline: (s.font_size() * 60.0) as u32,
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

    struct Laid {
        doc: td_html::document::Document,
        out: Layout,
    }

    impl Laid {
        fn new(html: &str, css: &str, width: f64) -> Self {
            Self::with(html, css, width, Limits::default())
        }
        fn with(html: &str, css: &str, width: f64, limits: Limits) -> Self {
            let doc = td_html::document::parse(html).unwrap();
            let out = {
                let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits).unwrap();
                layout(&styled, &mut Metrics, &Request::new(width, 600.0)).unwrap()
            };
            Self { doc, out }
        }
        fn id(&self, node: td_html::document::NodeId) -> Option<&str> {
            self.doc
                .node(node)
                .and_then(|n| n.element())
                .and_then(|e| e.attribute("id"))
        }
        fn geometry(&self, id: &str) -> &BoxGeometry {
            self.out
                .boxes
                .iter()
                .find(|g| self.id(g.node) == Some(id))
                .unwrap_or_else(|| panic!("no box {id}"))
        }
        fn border(&self, id: &str) -> (f64, f64, f64, f64) {
            rect(self.geometry(id).border)
        }
        /// The x and width of each of `ids`' border boxes.
        fn spans(&self, ids: &[&str]) -> Vec<(f64, f64)> {
            ids.iter()
                .map(|id| {
                    let b = self.geometry(id).border;
                    (b.x, b.width)
                })
                .collect()
        }
        /// Each line's top, its first piece's x and its text.
        fn lines(&self) -> Vec<(f64, f64, String)> {
            self.out
                .lines
                .iter()
                .map(|line| {
                    let text = line
                        .fragments
                        .iter()
                        .map(|f| {
                            let node = self.doc.node(f.node).and_then(|n| n.text()).unwrap();
                            node[f.range.clone()].to_string()
                        })
                        .collect::<String>();
                    let x = line.fragments.first().map_or(f64::NAN, |f| f.x);
                    (line.y, x, text)
                })
                .collect()
        }
    }

    fn rect(r: Rect) -> (f64, f64, f64, f64) {
        (r.x, r.y, r.width, r.height)
    }

    const ROW: &str = "#f{display:flex}i{display:block}";

    /// Links in a row: each its max-content wide, the gap between them,
    /// all as tall as the line.
    #[test]
    fn a_navbar_lays_its_links_in_a_row() {
        let laid = Laid::new(
            "<nav id=f>\n <a id=a href=/>ab</a>\n <a id=b href=/>cde</a>\n <a id=c>f</a>\n</nav><p>after",
            "#f{display:flex;column-gap:10px}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 16.0), (26.0, 24.0), (60.0, 8.0)]
        );
        assert_eq!(laid.border("f"), (0.0, 0.0, 400.0, 20.0));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "ab".into()),
                (0.0, 26.0, "cde".into()),
                (0.0, 60.0, "f".into()),
                (20.0, 0.0, "after".into()),
            ]
        );
    }

    /// Grow shares positive free space by flex-grow, from each base size.
    #[test]
    fn grow_shares_free_space_by_its_factors() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
            "#f{display:flex}i{flex:1 1 0}#b{flex-grow:2}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 100.0), (100.0, 200.0), (300.0, 100.0)]
        );
        // From each base size: 40 and 60 of 400 leave 300, half each.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{flex-grow:1}#a{width:40px}#b{flex-basis:60px;width:5px}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 190.0), (190.0, 210.0)]);
        // Factors summing under 1 take that share of the free space.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{flex:0.25 1 0}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 100.0), (100.0, 100.0)]);
    }

    /// Shrink takes negative free space by flex-shrink times base size,
    /// the last flexible item taking what rounding leaves.
    #[test]
    fn shrink_takes_by_factor_times_base_size() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
            "#f{display:flex}i{width:200px}",
            300.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 100.0), (100.0, 100.0), (200.0, 100.0)]
        );
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{width:200px}#b{flex-shrink:2}",
            300.0,
        );
        // 100 px taken a third and two thirds, in 1/64 px: 2,133 and 4,267.
        assert_eq!(
            laid.spans(&["a", "b"]),
            vec![(0.0, 166.671875), (166.671875, 133.328125)]
        );
        // A larger base gives up more: 300 and 100 into 200, 150 and 50.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}#a{width:300px}#b{width:100px}",
            200.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 150.0), (150.0, 50.0)]);
        // flex-shrink:0 overflows.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{width:200px;flex-shrink:0}",
            300.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 200.0), (200.0, 200.0)]);
    }

    /// Items clamped by min and max widths are frozen there and the rest
    /// share again (§9.7's loop).
    #[test]
    fn min_and_max_clamps_freeze_and_the_rest_share_again() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
            "#f{display:flex}i{flex:1 1 0}#a{max-width:50px}",
            300.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 50.0), (50.0, 125.0), (175.0, 125.0)]
        );
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{width:200px}#a{min-width:180px}",
            300.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 180.0), (180.0, 120.0)]);
        // Only the violations of the total's sign freeze: b's minimum
        // (+49 px) outweighs a's maximum (-10 px), so a shares again and
        // ends under its maximum, as c does.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
            "#f{display:flex}i{width:200px}#a{max-width:140px}#b{min-width:199px}",
            450.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 125.5), (125.5, 199.0), (324.5, 125.5)]
        );
        // A minimum over a smaller maximum wins.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{flex:1 1 0}#a{min-width:250px;max-width:100px}",
            300.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 250.0), (250.0, 50.0)]);
        // A base size over the maximum is frozen at it before growing.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{flex-grow:1}#a{width:200px;max-width:80px}",
            300.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 80.0), (80.0, 220.0)]);
    }

    /// An item's automatic minimum is its min-content width (or its
    /// width, if less): a long word does not shrink below it, unless its
    /// min-width is set or it is a scroll container.
    #[test]
    fn the_automatic_minimum_keeps_a_word_whole() {
        let css = "#f{display:flex;width:60px}";
        let laid = Laid::new(
            "<div id=f><p id=a>abcdefghij</p><p id=b>xy</p></div>",
            css,
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 80.0), (80.0, 16.0)]);
        let laid = Laid::new(
            "<div id=f><p id=a style=min-width:0>abcdefghij</p><p id=b>xy</p></div>",
            css,
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 44.0), (44.0, 16.0)]);
        let laid = Laid::new(
            "<div id=f><p id=a style=overflow:hidden>abcdefghij</p><p id=b>xy</p></div>",
            css,
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 44.0), (44.0, 16.0)]);
        // A width under the min-content width is the minimum.
        let laid = Laid::new(
            "<div id=f><p id=a style=width:50px>abcdefghij</p><p id=b>xy</p></div>",
            "#f{display:flex;width:40px}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 50.0), (50.0, 16.0)]);
        // flex:1 items, base 0: each at least its words.
        let laid = Laid::new(
            "<div id=f><p id=a>abcdefghij</p><p id=b>xy</p></div>",
            "#f{display:flex;width:100px}p{flex:1}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 80.0), (80.0, 20.0)]);
    }

    /// Each justify-content value with 280 px free of 400, and with 50 px
    /// too few.
    #[test]
    fn justify_content_places_the_free_space() {
        let xs = |value: &str, width: f64| {
            let laid = Laid::new(
                "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
                &format!("#f{{display:flex;justify-content:{value}}}i{{width:40px;flex-shrink:0}}"),
                width,
            );
            laid.spans(&["a", "b", "c"])
                .into_iter()
                .map(|(x, _)| x)
                .collect::<Vec<_>>()
        };
        for value in [
            "normal",
            "start",
            "flex-start",
            "left",
            "stretch",
            "safe start",
        ] {
            assert_eq!(xs(value, 400.0), vec![0.0, 40.0, 80.0], "{value}");
        }
        for value in ["end", "flex-end", "right", "unsafe end"] {
            assert_eq!(xs(value, 400.0), vec![280.0, 320.0, 360.0], "{value}");
        }
        assert_eq!(xs("center", 400.0), vec![140.0, 180.0, 220.0]);
        assert_eq!(xs("space-between", 400.0), vec![0.0, 180.0, 360.0]);
        // Halves around each: 280 * 1/6, 3/6 and 5/6, floored in 1/64 px.
        assert_eq!(xs("space-around", 400.0), vec![46.65625, 180.0, 313.328125]);
        assert_eq!(xs("space-evenly", 400.0), vec![70.0, 180.0, 290.0]);
        // 120 in 70: the start overflows the end; center both sides; the
        // spaces as start or center.
        for (value, want) in [
            ("start", vec![0.0, 40.0, 80.0]),
            ("end", vec![-50.0, -10.0, 30.0]),
            ("center", vec![-25.0, 15.0, 55.0]),
            ("space-between", vec![0.0, 40.0, 80.0]),
            ("space-around", vec![-25.0, 15.0, 55.0]),
            ("space-evenly", vec![-25.0, 15.0, 55.0]),
        ] {
            assert_eq!(xs(value, 70.0), want, "{value}");
        }
        // One item: space-between at the start, the others centred.
        let one = |value: &str| {
            let laid = Laid::new(
                "<div id=f><i id=a></i></div>",
                &format!("#f{{display:flex;justify-content:{value}}}i{{width:40px}}"),
                100.0,
            );
            laid.border("a").0
        };
        assert_eq!(one("space-between"), 0.0);
        assert_eq!(one("space-around"), 30.0);
        assert_eq!(one("space-evenly"), 30.0);
    }

    /// align-items and align-self: stretch (and normal) to the line's
    /// height, start, end and center within it.
    #[test]
    fn align_items_and_align_self_place_items_across_the_line() {
        let lay = |css: &str| {
            Laid::new(
                "<div id=f><p id=a>ab<br>cd</p><p id=b>ef</p></div><p id=n>next",
                &format!("#f{{display:flex}}{css}"),
                400.0,
            )
        };
        for css in ["", "#f{align-items:normal}", "#f{align-items:stretch}"] {
            let laid = lay(css);
            assert_eq!(laid.border("a"), (0.0, 0.0, 16.0, 40.0), "{css}");
            assert_eq!(laid.border("b"), (16.0, 0.0, 16.0, 40.0), "{css}");
            assert_eq!(laid.geometry("b").content.height, 40.0);
            assert_eq!(laid.border("f").3, 40.0);
            assert_eq!(laid.lines()[2], (0.0, 16.0, "ef".into()));
        }
        for (css, y) in [
            ("#f{align-items:start}", 0.0),
            ("#f{align-items:flex-start}", 0.0),
            ("#f{align-items:baseline}", 0.0),
            ("#f{align-items:end}", 20.0),
            ("#f{align-items:flex-end}", 20.0),
            ("#f{align-items:center}", 10.0),
            ("#b{align-self:center}", 10.0),
            ("#f{align-items:center}#b{align-self:auto}", 10.0),
            ("#f{align-items:center}#b{align-self:stretch}", 0.0),
        ] {
            let laid = lay(css);
            let b = laid.border("b");
            let height = if css.ends_with("stretch}") {
                40.0
            } else {
                20.0
            };
            assert_eq!((b.1, b.3), (y, height), "{css}");
            assert_eq!(laid.lines()[2], (y, 16.0, "ef".into()), "{css}");
            assert_eq!(laid.border("a"), (0.0, 0.0, 16.0, 40.0), "{css}");
            assert_eq!(laid.lines()[3], (40.0, 0.0, "next".into()), "{css}");
        }
        // A set height is not stretched; min- and max-height bound a stretch.
        let laid = lay("#b{height:10px}");
        assert_eq!(laid.border("b").3, 10.0);
        let laid = lay("#b{max-height:30px}");
        assert_eq!(laid.border("b").3, 30.0);
    }

    /// Auto margins take positive free space before justify-content: a
    /// left one pushes its item right; `margin:auto` centres on both axes.
    #[test]
    fn auto_margins_absorb_free_space_first() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
            "#f{display:flex;justify-content:center}i{width:40px}#c{margin-left:auto}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 40.0), (40.0, 40.0), (360.0, 40.0)]
        );
        assert_eq!(laid.geometry("c").margin.left, 280.0);
        // Shared between two auto margins.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{width:40px;margin:0 auto}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(80.0, 40.0), (280.0, 40.0)]);
        let laid = Laid::new(
            "<div id=f><p id=a>ab</p></div>",
            "#f{display:flex;height:100px}#a{margin:auto}",
            400.0,
        );
        assert_eq!(laid.border("a"), (192.0, 40.0, 16.0, 20.0));
        assert_eq!(
            laid.geometry("a").margin,
            Edges {
                top: 40.0,
                right: 192.0,
                bottom: 40.0,
                left: 192.0
            }
        );
        assert_eq!(laid.lines(), vec![(40.0, 192.0, "ab".into())]);
        // No free space: auto margins are 0.
        let laid = Laid::new(
            "<div id=f><i id=a></i></div>",
            "#f{display:flex}i{width:500px;flex-shrink:0;margin-left:auto}",
            400.0,
        );
        assert_eq!(laid.border("a").0, 0.0);
    }

    /// Text among a container's elements is an anonymous item each run;
    /// white space alone between them is none.
    #[test]
    fn text_runs_are_anonymous_items_and_white_space_is_none() {
        let laid = Laid::new(
            "<div id=f>ab <b id=b>cd</b> ef gh</div>",
            "#f{display:flex;column-gap:4px}",
            400.0,
        );
        assert_eq!(laid.spans(&["b"]), vec![(20.0, 16.0)]);
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "ab".into()),
                (0.0, 20.0, "cd".into()),
                (0.0, 40.0, "ef gh".into()),
            ]
        );
        // Each run's lines name the container and end its paragraph.
        let lines = &laid.out.lines;
        assert_eq!(lines[0].block(), lines[2].block());
        assert_ne!(lines[0].block(), lines[1].block());
        assert_eq!(lines[0].end(), crate::inline::LineEnd::End);
        let laid = Laid::new(
            "<div id=f>\n  <b id=a>a</b>\n  <b id=b>b</b>\n</div>",
            "#f{display:flex;column-gap:4px}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 8.0), (12.0, 8.0)]);
        assert_eq!(laid.out.lines.len(), 2);
        // A break is content: its run is an item.
        let laid = Laid::new("<div id=f><br><b id=a>a</b></div>", ROW, 400.0);
        assert_eq!(laid.spans(&["a"]), vec![(0.0, 8.0)]);
        // Its two lines, as `<div><br></div>` lays in block flow.
        let block = Laid::new("<div id=f><br></div>", "", 400.0);
        assert_eq!(laid.border("f").3, block.border("f").3);
        // A run's text wraps within its item when it shrinks.
        let laid = Laid::new("<div id=f>aaaa bbbb cccc <b id=b>dd</b></div>", ROW, 60.0);
        assert_eq!(laid.spans(&["b"]), vec![(44.0, 16.0)]);
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa".into()),
                (20.0, 0.0, "bbbb".into()),
                (40.0, 0.0, "cccc".into()),
                (0.0, 44.0, "dd".into()),
            ]
        );
    }

    /// A flex container in an item, and flex in floats, cells and
    /// inline-blocks, which measure it: the sum of its items' widths.
    #[test]
    fn nested_and_measured_flex_containers() {
        let laid = Laid::new(
            "<div id=f><div id=g><b id=a>ab</b><b id=b>cd</b></div><b id=c>e</b></div>",
            "#f,#g{display:flex}#g{column-gap:6px}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["g", "a", "b", "c"]),
            vec![(0.0, 38.0), (0.0, 16.0), (22.0, 16.0), (38.0, 8.0)]
        );
        // A float shrinks to fit its items and their gaps.
        let css = "#g{display:flex;column-gap:6px}#w{float:left}";
        let laid = Laid::new(
            "<div id=w><div id=g><b>ab</b><b>cd efg</b></div></div>",
            css,
            400.0,
        );
        assert_eq!(laid.border("w"), (0.0, 0.0, 70.0, 20.0));
        // Its min-content width is the sum of its items' too.
        let laid = Laid::new(
            "<div id=w><div id=g><b>ab</b><b>cd efg</b></div></div>",
            css,
            30.0,
        );
        assert_eq!(laid.border("w").2, 46.0);
        // A floated container shrinks to fit its own items.
        let laid = Laid::new(
            "<div id=g class=w><b>ab</b><b>cd efg</b></div><p>after",
            "#g{display:flex;column-gap:6px}.w{float:left}",
            400.0,
        );
        assert_eq!(laid.border("g"), (0.0, 0.0, 70.0, 20.0));
        assert_eq!(laid.lines()[2], (0.0, 70.0, "after".into()));
        let laid = Laid::new(
            "<p>x <span id=w><span id=g><b>ab</b><b>cd</b></span></span> y",
            "#g{display:flex;column-gap:6px}#w{display:inline-block}",
            400.0,
        );
        assert_eq!(laid.border("w"), (16.0, 0.0, 38.0, 20.0));
        let laid = Laid::new(
            "<table><tr><td id=w><div id=g><b>ab</b><b>cd</b></div><td>z</table>",
            "#g{display:flex;column-gap:6px}table{border-spacing:0}",
            400.0,
        );
        assert_eq!(laid.border("w").2, 38.0);
        // A px flex-basis bounds no contribution of an item that may
        // grow past it, and lowers that of one that may not (§9.9.3).
        let laid = Laid::new(
            "<div id=w><div id=g><b style=flex-basis:50px>ab</b><b>cd</b></div></div>",
            css,
            400.0,
        );
        assert_eq!(laid.border("w").2, 38.0);
    }

    /// An item's contribution is clamped by its flex base size only as
    /// §9.9.3 has it, and never below its automatic minimum: a row of
    /// `flex: 1 1 0px` links measures its links' text, in a row, a float,
    /// an inline-block and a cell alike, and lays at what it measured.
    #[test]
    fn contributions_keep_the_automatic_minimum() {
        let links = "<div id=g><a id=a style='flex:1 1 0px'>Home</a>\
                     <a id=b style='flex:1 1 0px'>Abou</a></div>";
        let laid = Laid::new(
            &format!("<div id=f>{links}<div id=r>right</div></div>"),
            "#f,#g{display:flex}",
            400.0,
        );
        assert_eq!(laid.spans(&["g", "a", "b", "r"]), {
            vec![(0.0, 64.0), (0.0, 32.0), (32.0, 32.0), (64.0, 40.0)]
        });
        for (html, css) in [
            (format!("<div id=w>{links}</div>"), "#w{float:left}"),
            (
                format!(
                    "<p>x <span id=w>{}</span> y",
                    links.replace("<div", "<span").replace("</div>", "</span>")
                ),
                "#w{display:inline-block}",
            ),
            (
                format!("<table><tr><td id=w>{links}<td>z</table>"),
                "table{border-spacing:0}",
            ),
        ] {
            let laid = Laid::new(&html, &format!("#g{{display:flex}}{css}"), 400.0);
            assert_eq!(laid.border("w").2, 64.0, "{css}");
            assert_eq!(
                laid.spans(&["a", "b"])[1],
                (laid.border("w").0 + 32.0, 32.0)
            );
        }
        let measured = |item: &str| {
            let laid = Laid::new(
                &format!("<div id=w><div id=g><b id=i style='{item}'>abcdefg</b></div></div>"),
                "#g{display:flex}#w{float:left}",
                400.0,
            );
            (laid.border("w").2, laid.border("i").2)
        };
        // `content` is the base size, and an item that cannot grow
        // contributes no more: its width stands for nothing here.
        assert_eq!(measured("flex-basis:content;width:200px"), (56.0, 56.0));
        // Below its automatic minimum, its min-content or its width.
        assert_eq!(measured("flex:0 0 0px;width:100px"), (56.0, 56.0));
        assert_eq!(measured("flex:0 0 0px;width:20px"), (20.0, 20.0));
        assert_eq!(measured("flex:0 0 0px"), (56.0, 56.0));
        // A scroll container's, or a set min-width, is no such floor.
        assert_eq!(measured("flex:0 0 0px;overflow:hidden"), (0.0, 0.0));
        assert_eq!(measured("flex:0 0 0px;min-width:10px"), (10.0, 10.0));
        // An item that cannot shrink contributes its base size at least.
        assert_eq!(measured("flex:0 0 100px"), (100.0, 100.0));
        assert_eq!(measured("flex:1 0 100px"), (100.0, 100.0));
    }

    fn cols(n: usize) -> String {
        let mut html = String::from("<div id=f>");
        for id in ["a", "b", "c", "d", "e", "f"].iter().take(n) {
            html.push_str(&format!("<div class=col id={id}>{id}</div>"));
        }
        html + "</div>"
    }

    /// A wrapping row breaks its items into lines, each as many as fit it
    /// with the gaps between them, each line its tallest item's height,
    /// the lines `row-gap` apart: Bootstrap's halves lay two by two, and
    /// the inputs a layout of one row or of block flow got wrong lay as
    /// browsers lay them.
    #[test]
    fn wrapping_rows_break_into_lines() {
        let wrap = "#f{display:flex;flex-wrap:wrap}";
        let laid = Laid::new(&cols(4), &format!("{wrap}.col{{flex:0 0 50%}}"), 400.0);
        assert_eq!(
            [
                laid.border("a"),
                laid.border("b"),
                laid.border("c"),
                laid.border("d")
            ],
            [
                (0.0, 0.0, 200.0, 20.0),
                (200.0, 0.0, 200.0, 20.0),
                (0.0, 20.0, 200.0, 20.0),
                (200.0, 20.0, 200.0, 20.0)
            ]
        );
        assert_eq!(laid.border("f"), (0.0, 0.0, 400.0, 40.0));
        // wrap-reverse is laid as wrap.
        let reverse = Laid::new(
            &cols(4),
            "#f{display:flex;flex-wrap:wrap-reverse}.col{flex:0 0 50%}",
            400.0,
        );
        assert_eq!(drawn(&reverse.out), drawn(&laid.out));
        for css in [".col{flex:0 0 100%;max-width:100%}", ".col{width:100%}"] {
            let laid = Laid::new(&cols(3), &format!("{wrap}{css}"), 400.0);
            assert_eq!(
                laid.spans(&["a", "b", "c"]),
                vec![(0.0, 400.0), (0.0, 400.0), (0.0, 400.0)]
            );
            assert_eq!(
                [laid.border("a").1, laid.border("b").1, laid.border("c").1],
                [0.0, 20.0, 40.0]
            );
        }
        // Three of 130 px and two gaps fit 400; the fourth wraps, a
        // row-gap below.
        let laid = Laid::new(
            &cols(4),
            "#f{display:flex;flex-wrap:wrap;gap:10px 4px}.col{flex:0 0 130px}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c", "d"]),
            vec![(0.0, 130.0), (134.0, 130.0), (268.0, 130.0), (0.0, 130.0)]
        );
        assert_eq!(laid.border("d").1, 30.0);
        assert_eq!(laid.border("f").3, 50.0);
        // Four of 97 px and three gaps fill 400 exactly; at 97.25 px the
        // fourth wraps.
        let laid = Laid::new(
            &cols(4),
            "#f{display:flex;flex-wrap:wrap;column-gap:4px}.col{flex:0 0 97px}",
            400.0,
        );
        assert_eq!(laid.border("d"), (303.0, 0.0, 97.0, 20.0));
        let laid = Laid::new(
            &cols(4),
            "#f{display:flex;flex-wrap:wrap;column-gap:4px}.col{flex:0 0 97.25px}",
            400.0,
        );
        assert_eq!(laid.border("c").0, 202.5);
        assert_eq!(laid.border("d"), (0.0, 20.0, 97.25, 20.0));
        // Each line is its tallest item's height, which a stretched item
        // takes; a line's items grow into its free space.
        let laid = Laid::new(
            "<div id=f><div id=a>x<br>y</div><div id=b>b</div><div id=c>c</div></div>",
            "#f{display:flex;flex-wrap:wrap}#a,#b{flex:0 0 150px}#c{flex:1 0 300px}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 150.0, 40.0));
        assert_eq!(laid.border("b"), (150.0, 0.0, 150.0, 40.0));
        assert_eq!(laid.border("c"), (0.0, 40.0, 400.0, 20.0));
        // A wrapping row measures as its widest item at least and its
        // items' sum at most, and lays at what it measured.
        let css = "#g{display:flex;flex-wrap:wrap;column-gap:6px}#w{float:left}";
        let html = "<div id=w><div id=g><b id=a>ab</b><b id=b>cd efg</b></div></div>";
        let laid = Laid::new(html, css, 400.0);
        assert_eq!(laid.border("w").2, 70.0);
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 16.0), (22.0, 48.0)]);
        let laid = Laid::new(html, css, 20.0);
        assert_eq!(laid.border("w").2, 24.0);
        assert_eq!(laid.border("a"), (0.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.border("b"), (0.0, 20.0, 24.0, 40.0));
        // A text run is an item measured alike.
        let laid = Laid::new(
            "<div id=w><div id=g><b id=b>cd efg</b>ab</div></div>",
            css,
            20.0,
        );
        assert_eq!(laid.border("w").2, 24.0);
    }

    /// Lines in a container of definite or least height are placed by
    /// `align-content`, `normal` stretching them; without one they pack
    /// at the start, and they never move up.
    #[test]
    fn align_content_places_the_lines() {
        let tops = |content: &str, height: &str| {
            let laid = Laid::new(
                &cols(4),
                &format!(
                    "#f{{display:flex;flex-wrap:wrap;align-content:{content};{height}}}\
                     .col{{flex:0 0 50%}}"
                ),
                400.0,
            );
            (laid.border("a").1, laid.border("c").1, laid.border("c").3)
        };
        let definite = "height:200px";
        assert_eq!(tops("flex-start", definite), (0.0, 20.0, 20.0));
        assert_eq!(tops("start", definite), (0.0, 20.0, 20.0));
        assert_eq!(tops("end", definite), (160.0, 180.0, 20.0));
        assert_eq!(tops("flex-end", definite), (160.0, 180.0, 20.0));
        assert_eq!(tops("center", definite), (80.0, 100.0, 20.0));
        assert_eq!(tops("space-between", definite), (0.0, 180.0, 20.0));
        assert_eq!(tops("space-around", definite), (40.0, 140.0, 20.0));
        assert_eq!(tops("space-evenly", definite), (53.328125, 126.65625, 20.0));
        assert_eq!(tops("stretch", definite), (0.0, 100.0, 100.0));
        assert_eq!(tops("normal", definite), (0.0, 100.0, 100.0));
        assert_eq!(tops("normal", "min-height:200px"), (0.0, 100.0, 100.0));
        assert_eq!(tops("center", "min-height:200px"), (80.0, 100.0, 20.0));
        // No height: packed at the start; too little: not moved up.
        assert_eq!(tops("center", ""), (0.0, 20.0, 20.0));
        assert_eq!(tops("center", "height:30px"), (0.0, 20.0, 20.0));
        assert_eq!(tops("end", "height:30px"), (0.0, 20.0, 20.0));
        assert_eq!(tops("normal", "height:30px"), (0.0, 20.0, 20.0));
        // Items aligned in a stretched line.
        let laid = Laid::new(
            &cols(4),
            "#f{display:flex;flex-wrap:wrap;height:200px;align-items:center}\
             .col{flex:0 0 50%}",
            400.0,
        );
        assert_eq!((laid.border("a").1, laid.border("c").1), (40.0, 140.0));
    }

    /// A column lays its items down the page, stretched across or at
    /// their shrink-to-fit widths placed by their alignment or auto
    /// margins; grows them into the free space of a definite or least
    /// height, or places them in it by auto margins or
    /// `justify-content`: the sticky footer.
    #[test]
    fn columns_lay_down_the_page() {
        let page = "<body id=f><header id=a>head</header><main id=b style=flex:1>main text</main>\
                    <footer id=c>foot</footer>";
        let laid = Laid::new(
            page,
            "body{min-height:300px}#f{display:flex;flex-direction:column}",
            400.0,
        );
        assert_eq!(
            [laid.border("a"), laid.border("b"), laid.border("c")],
            [
                (0.0, 0.0, 400.0, 20.0),
                (0.0, 20.0, 400.0, 260.0),
                (0.0, 280.0, 400.0, 20.0)
            ]
        );
        assert_eq!(laid.border("f").3, 300.0);
        let items = "<div id=f><i id=a>ab</i><i id=b>abcd</i></div>";
        let at = |css: &str| {
            let laid = Laid::new(
                items,
                &format!("#f{{display:flex;flex-direction:column;{css}}}i{{display:block}}"),
                400.0,
            );
            (laid.border("a"), laid.border("b"))
        };
        assert_eq!(at(""), ((0.0, 0.0, 400.0, 20.0), (0.0, 20.0, 400.0, 20.0)));
        assert_eq!(
            at("align-items:center"),
            ((192.0, 0.0, 16.0, 20.0), (184.0, 20.0, 32.0, 20.0))
        );
        assert_eq!(
            at("align-items:end"),
            ((384.0, 0.0, 16.0, 20.0), (368.0, 20.0, 32.0, 20.0))
        );
        assert_eq!(
            at("align-items:flex-start"),
            ((0.0, 0.0, 16.0, 20.0), (0.0, 20.0, 32.0, 20.0))
        );
        assert_eq!(at("row-gap:10px").1, (0.0, 30.0, 400.0, 20.0));
        // Cross auto margins: centred, or at the end.
        let laid = Laid::new(
            items,
            "#f{display:flex;flex-direction:column}i{display:block}#a{margin:0 auto}\
             #b{margin-left:auto}",
            400.0,
        );
        assert_eq!(
            (laid.border("a"), laid.border("b")),
            ((192.0, 0.0, 16.0, 20.0), (368.0, 20.0, 32.0, 20.0))
        );
        let justified = |css: &str| at(&format!("height:200px;{css}"));
        let tops = |css: &str| {
            let (a, b) = justified(css);
            (a.1, b.1)
        };
        assert_eq!(tops("justify-content:flex-start"), (0.0, 20.0));
        assert_eq!(tops("justify-content:end"), (160.0, 180.0));
        assert_eq!(tops("justify-content:center"), (80.0, 100.0));
        assert_eq!(tops("justify-content:space-between"), (0.0, 180.0));
        assert_eq!(tops("justify-content:space-around"), (40.0, 140.0));
        assert_eq!(tops("justify-content:space-evenly"), (53.328125, 126.65625));
        assert_eq!(tops("justify-content:right"), (0.0, 20.0));
        // Too little room: neither shrunk nor moved up.
        assert_eq!(
            at("height:30px;justify-content:end"),
            ((0.0, 0.0, 400.0, 20.0), (0.0, 20.0, 400.0, 20.0))
        );
        // A main auto margin takes the free space first.
        let laid = Laid::new(
            items,
            "#f{display:flex;flex-direction:column;height:200px;justify-content:center}\
             i{display:block}#b{margin-top:auto}",
            400.0,
        );
        assert_eq!(laid.border("b"), (0.0, 180.0, 400.0, 20.0));
        assert_eq!(laid.geometry("b").margin.top, 160.0);
        // Growing: equally from a basis of 0, by factors, within bounds.
        let (a, b) = justified("}i{flex:1");
        assert_eq!(
            (a, b),
            ((0.0, 0.0, 400.0, 100.0), (0.0, 100.0, 400.0, 100.0))
        );
        let (a, b) = justified("}#a{flex-grow:3}#b{flex-grow:1;max-height:30px");
        assert_eq!((a.3, b.1, b.3), (170.0, 170.0, 30.0));
        // Not below its content's height, its automatic minimum.
        let laid = Laid::new(
            "<div id=f><i id=a>a<br>b<br>c</i><i id=b>d</i></div>",
            "#f{display:flex;flex-direction:column;height:100px}i{display:block;flex:1}",
            400.0,
        );
        assert_eq!(
            (laid.border("a").3, laid.border("b")),
            (60.0, (0.0, 60.0, 400.0, 40.0))
        );
        // A definite basis, a length or a percentage of the height.
        let (a, b) = justified("}#a{flex:0 0 50px}#b{flex:0 0 25%");
        assert_eq!((a.3, b.1, b.3), (50.0, 50.0, 50.0));
        // column-reverse is laid as column, and a column does not wrap.
        for css in [
            "flex-direction:column-reverse",
            "flex-flow:column wrap;height:30px",
        ] {
            let laid = Laid::new(
                items,
                &format!("#f{{display:flex;{css}}}i{{display:block}}"),
                400.0,
            );
            assert_eq!(laid.border("b"), (0.0, 20.0, 400.0, 20.0), "{css}");
        }
        // A column measures as its widest item.
        let laid = Laid::new(
            "<div id=w><div id=g><b id=a>ab</b><b id=b>cd efg</b></div></div>",
            "#g{display:flex;flex-direction:column}#w{float:left}",
            400.0,
        );
        assert_eq!(laid.border("w").2, 48.0);
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 48.0), (0.0, 48.0)]);
    }

    /// A table or picture in a column is placed across by its auto
    /// margins, else its alignment, as rows and pictures in rows are.
    #[test]
    fn tables_and_pictures_in_columns_are_placed_across() {
        let at = |item: &str, css: &str| {
            let html = format!("<div id=f>{item}</div>");
            let css = format!("#f{{display:flex;flex-direction:column}}{css}");
            Laid::new(&html, &css, 400.0).border("t")
        };
        let table = "<table id=t><tr><td>ab</table>";
        assert_eq!(at(table, ""), (0.0, 0.0, 20.0, 24.0));
        assert_eq!(at(table, "#t{margin:0 auto}"), (190.0, 0.0, 20.0, 24.0));
        assert_eq!(
            at(table, "#t{margin-left:auto;width:50%}"),
            (200.0, 0.0, 200.0, 24.0)
        );
        assert_eq!(
            at(table, "#f{height:300px}#t{margin:auto}"),
            (190.0, 138.0, 20.0, 24.0)
        );
        assert_eq!(at(table, "#t{align-self:center}"), (190.0, 0.0, 20.0, 24.0));
        // Its used margins, as any aligned item's: alignment is no margin.
        let margins = |css: &str| {
            let html = format!("<div id=f>{table}</div>");
            let css = format!("#f{{display:flex;flex-direction:column}}{css}");
            let m = Laid::new(&html, &css, 400.0).geometry("t").margin;
            (m.left, m.right)
        };
        assert_eq!(margins("#t{align-self:center}"), (0.0, 0.0));
        assert_eq!(margins("#t{align-self:end;margin-left:5px}"), (5.0, 0.0));
        assert_eq!(margins("#t{margin:0 auto}"), (190.0, 190.0));
        assert_eq!(at(table, "#t{align-self:end}"), (380.0, 0.0, 20.0, 24.0));
        assert_eq!(
            at(table, "#f{align-items:flex-end}#t{margin-left:10px}"),
            (380.0, 0.0, 20.0, 24.0)
        );
        // Grown, it takes the room but its box keeps its rows' height.
        let laid = Laid::new(
            &format!("<div id=f>{table}<p id=a>x</p></div>"),
            "#f{display:flex;flex-direction:column;height:300px}#t{flex:1}",
            400.0,
        );
        assert_eq!(
            (laid.border("t"), laid.border("a").1),
            ((0.0, 0.0, 20.0, 24.0), 280.0)
        );
        let picture = "<img id=t width=40 height=30>";
        assert_eq!(at(picture, ""), (0.0, 0.0, 40.0, 30.0));
        assert_eq!(at(picture, "#t{margin:0 auto}"), (180.0, 0.0, 40.0, 30.0));
        assert_eq!(
            at(picture, "#t{margin-left:auto}"),
            (360.0, 0.0, 40.0, 30.0)
        );
        assert_eq!(
            at(picture, "#t{align-self:center}"),
            (180.0, 0.0, 40.0, 30.0)
        );
        assert_eq!(at(picture, "#t{align-self:end}"), (360.0, 0.0, 40.0, 30.0));
        assert_eq!(
            at(picture, "#f{height:300px}#t{margin:auto}"),
            (180.0, 135.0, 40.0, 30.0)
        );
    }

    /// A column item's automatic minimum is its content's height at an
    /// auto height, or its own where that is less (§4.5); a box that
    /// would scroll, which td does not, keeps its content's height.
    #[test]
    fn column_minimums_and_scroll_containers() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b>y</i></div>",
            "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
             #a{height:50px}#b{flex:1;height:100%}",
            400.0,
        );
        assert_eq!(laid.border("b"), (0.0, 50.0, 400.0, 250.0));
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b>y</i></div>",
            "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
             #a{height:50px}#b{flex:0 0 50px;height:200px}",
            400.0,
        );
        assert_eq!(laid.border("b"), (0.0, 50.0, 400.0, 50.0));
        // Content taller than its height: the content's, as laid.
        let laid = Laid::new(
            "<div id=f><i id=a>a<br>b<br>c</i></div>",
            "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
             #a{flex:0 0 10px;height:20px}",
            400.0,
        );
        assert_eq!(laid.border("a").3, 20.0);
        // A page whose `main` scrolls in a browser: laid whole.
        let page = format!(
            "<body><header id=h>head</header><main id=m>{}</main><footer id=c>foot</footer>",
            "<p>line</p>".repeat(35)
        );
        let sheet = "html,body{height:100%}body{display:flex;flex-direction:column}main{flex:1}";
        let at = |css: &str| {
            let laid = Laid::new(&page, &format!("{sheet}main{{{css}}}"), 400.0);
            (laid.border("m"), laid.border("c").1)
        };
        for css in [
            "overflow-y:auto",
            "overflow:scroll",
            "overflow-y:auto;min-height:0;flex:1 1 0",
            "overflow-y:hidden",
            "overflow:hidden;min-height:0",
        ] {
            assert_eq!(at(css), ((0.0, 20.0, 400.0, 700.0), 720.0), "{css}");
        }
        // One that clips is as the spec has it, and one collapsed to no
        // height hides its content.
        assert_eq!(
            at("overflow-y:clip;min-height:0"),
            ((0.0, 20.0, 400.0, 560.0), 580.0)
        );
        assert_eq!(at("min-height:0"), ((0.0, 20.0, 400.0, 560.0), 580.0));
        assert_eq!(at(""), ((0.0, 20.0, 400.0, 700.0), 720.0));
        assert_eq!(
            at("overflow:hidden;flex:none;height:0"),
            ((0.0, 20.0, 400.0, 0.0), 20.0)
        );
        // A hidden box holding the pane that would scroll, its items in a
        // row or a column.
        let page = format!(
            "<body><header>head</header><div id=m><aside>side</aside><div class=p>{}</div>\
             </div><footer id=c>foot</footer>",
            "<p>line</p>".repeat(35)
        );
        for (lay, height) in [("row", 700.0), ("column", 720.0)] {
            let css = format!(
                "html,body{{height:100%}}body{{display:flex;flex-direction:column}}\
                 #m{{flex:1;overflow:hidden;display:flex;flex-direction:{lay}}}\
                 .p{{flex:1;overflow-y:auto}}"
            );
            let laid = Laid::new(&page, &css, 400.0);
            assert_eq!(
                (laid.border("m"), laid.border("c").1),
                ((0.0, 20.0, 400.0, height), height + 20.0),
                "{lay}"
            );
        }
    }

    /// A flex container's automatic minimum as an item is its own lines'
    /// or items' height, whatever its definite height; a picture of
    /// specified width's the height that width gives.
    #[test]
    fn nested_containers_and_pictures_take_their_contents_minimum() {
        for inner in ["", "flex-direction:column", "flex-wrap:wrap"] {
            let html = format!(
                "<div id=f><i id=a></i><div id=b style=\"display:flex;{inner}\"><i>y</i></div></div>"
            );
            let laid = Laid::new(
                &html,
                "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
                 #a{height:50px}#b{flex:1;height:100%}",
                400.0,
            );
            assert_eq!(laid.border("b"), (0.0, 50.0, 400.0, 250.0), "{inner}");
        }
        let at = |css: &str| {
            let laid = Laid::new(
                "<div id=f><img id=p width=40 height=50></div>",
                &format!("#f{{display:flex;flex-direction:column}}#p{{{css}}}"),
                400.0,
            );
            laid.border("p")
        };
        // A picture whose width its height gives keeps its laid height,
        // so its ratio; one of specified width takes its auto height.
        assert_eq!(at("flex:0 0 10px;height:80px"), (0.0, 0.0, 64.0, 80.0));
        assert_eq!(at("flex:0 0 10px;height:30px"), (0.0, 0.0, 24.0, 30.0));
        assert_eq!(at("flex:0 0 60px;height:80px"), (0.0, 0.0, 64.0, 80.0));
        assert_eq!(
            at("flex:0 0 10px;width:30px;height:80px"),
            (0.0, 0.0, 30.0, 37.5)
        );
    }

    /// A column in a row is measured as its widest item, and a row in a
    /// column is laid across it.
    #[test]
    fn columns_in_rows_and_rows_in_columns() {
        let laid = Laid::new(
            "<div id=f><div id=c><b id=a>ab</b><b id=b>cd efg</b></div><i id=r>r</i></div>",
            "#f{display:flex}#c{display:flex;flex-direction:column}",
            400.0,
        );
        assert_eq!(laid.border("c"), (0.0, 0.0, 48.0, 40.0));
        assert_eq!(laid.border("r"), (48.0, 0.0, 8.0, 40.0));
        let laid = Laid::new(
            "<div id=f><div id=r><b id=a>ab</b><b id=b>cd</b></div><i id=c>c</i></div>",
            "#f{display:flex;flex-direction:column;height:100px}#r{display:flex;flex:1}\
             i{display:block}",
            400.0,
        );
        assert_eq!(laid.border("r"), (0.0, 0.0, 400.0, 80.0));
        // Grown, its items are not laid again: as laid, 20 px tall.
        assert_eq!(laid.border("b"), (16.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.border("c"), (0.0, 80.0, 400.0, 20.0));
    }

    /// A row from the right mirrors its items' places, `flex-start` at
    /// the right, while `start` and `end` keep to the writing mode; a
    /// wrapping one mirrors each line.
    #[test]
    fn row_reverse_mirrors_the_main_axis() {
        let xs = |css: &str| {
            let laid = Laid::new(
                &cols(3),
                &format!(
                    "#f{{display:flex;flex-direction:row-reverse;{css}}}.col{{flex:0 0 40px}}"
                ),
                400.0,
            );
            [laid.border("a").0, laid.border("b").0, laid.border("c").0]
        };
        assert_eq!(xs(""), [360.0, 320.0, 280.0]);
        assert_eq!(xs("justify-content:flex-start"), [360.0, 320.0, 280.0]);
        assert_eq!(xs("justify-content:flex-end"), [80.0, 40.0, 0.0]);
        assert_eq!(xs("justify-content:start"), [80.0, 40.0, 0.0]);
        assert_eq!(xs("justify-content:left"), [80.0, 40.0, 0.0]);
        assert_eq!(xs("justify-content:end"), [360.0, 320.0, 280.0]);
        assert_eq!(xs("justify-content:right"), [360.0, 320.0, 280.0]);
        assert_eq!(xs("justify-content:center"), [220.0, 180.0, 140.0]);
        assert_eq!(xs("justify-content:space-between"), [360.0, 180.0, 0.0]);
        assert_eq!(
            xs("justify-content:space-around"),
            [313.34375, 180.0, 46.671875]
        );
        assert_eq!(xs("column-gap:10px"), [360.0, 310.0, 260.0]);
        let laid = Laid::new(
            &cols(4),
            "#f{display:flex;flex-flow:row-reverse wrap}.col{flex:0 0 150px}",
            400.0,
        );
        assert_eq!(
            [
                laid.border("a"),
                laid.border("b"),
                laid.border("c"),
                laid.border("d")
            ],
            [
                (250.0, 0.0, 150.0, 20.0),
                (100.0, 0.0, 150.0, 20.0),
                (250.0, 20.0, 150.0, 20.0),
                (100.0, 20.0, 150.0, 20.0)
            ]
        );
        // Its lines stay in tree order.
        let laid = Laid::new(
            &cols(3),
            "#f{display:flex;flex-direction:row-reverse}",
            400.0,
        );
        let texts: Vec<String> = laid.lines().into_iter().map(|l| l.2).collect();
        assert_eq!(texts, ["a", "b", "c"]);
    }

    /// A column of stretched items without margins lays as block flow,
    /// and so does a wrapping row whose every item fills its line.
    #[test]
    fn a_column_and_full_lines_lay_as_block_flow() {
        let inner = "<p>some words here to wrap around the line and more of them</p>\
            <ul><li>an item<li>another <b>bold</b> one</ul>\
            <div style=overflow:hidden><div style=float:left;width:30px>fl</div>beside the float, words</div>\
            text between\
            <table><tr><td>a cell<td>another</table><img width=40 height=10>\
            <div style=padding:3px;border:2px>padded and bordered</div>";
        for width in [120.0, 333.0, 800.0] {
            let request = Request::new(width, 600.0);
            let block = styled_layout(
                &format!("<div id=f>{inner}</div><p>after"),
                "#f{display:block}img{display:block}",
                &request,
                Limits::default(),
            );
            for css in [
                "#f{display:flex;flex-direction:column}",
                "#f{display:flex;flex-wrap:wrap}#f>*{flex:0 0 100%;box-sizing:border-box}\
                 #f>img{flex:0 1 auto}",
            ] {
                let flex = styled_layout(
                    &format!("<div id=f>{inner}</div><p>after"),
                    css,
                    &request,
                    Limits::default(),
                );
                assert_eq!(drawn(&flex), drawn(&block), "{width} {css}");
                assert_eq!(flex.height, block.height);
                assert_eq!(flex.markers.len(), block.markers.len());
            }
        }
    }

    /// Percentages summing to 100% fit a wrapping row at every width,
    /// whatever each rounds to in 1/64 px: Bootstrap's thirds and sixths,
    /// with its gutters too, keep one row as the window narrows a pixel
    /// at a time.
    #[test]
    fn percentages_summing_to_all_fit_a_wrapping_row() {
        let one_row = |laid: &Laid, ids: &[&str], width: f64| {
            let spans = laid.spans(ids);
            let top = laid.border(ids[0]).1;
            for (at, id) in ids.iter().enumerate() {
                assert_eq!(laid.border(id).1, top, "{width} {id}");
                assert!(at == 0 || spans[at].0 > spans[at - 1].0, "{width} {id}");
            }
        };
        let ids = ["a", "b", "c", "d", "e", "f"];
        let row = |n: usize| {
            let mut html = String::from("<div class=row id=r>");
            for id in &ids[..n] {
                html.push_str(&format!("<div class=c id={id}>x</div>"));
            }
            html + "</div>"
        };
        for width in 600..=1400 {
            let width = width as f64;
            for (n, css) in [
                (3, ".c{flex:0 0 33.333333%;max-width:33.333333%}"),
                (6, ".c{flex:0 0 16.666667%;max-width:16.666667%}"),
                (
                    3,
                    ".row{margin:0 -15px}.c{box-sizing:border-box;padding:0 15px;\
                     flex:0 0 33.333333%;max-width:33.333333%}",
                ),
            ] {
                let laid = Laid::new(
                    &row(n),
                    &format!(".row{{display:flex;flex-wrap:wrap}}{css}"),
                    width,
                );
                one_row(&laid, &ids[..n], width);
            }
        }
        let laid = Laid::new(
            &row(2),
            ".row{display:flex;flex-wrap:wrap;width:100.015625px}.c{width:50%}",
            400.0,
        );
        one_row(&laid, &["a", "b"], 400.0);
    }

    /// Fractional factors summing to 1 give all the free space, and a sum
    /// under it its share to the nearest 1/64 px.
    #[test]
    fn fractional_factors_share_to_the_nearest_unit() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i><i id=d></i><i id=e></i></div>",
            "#f{display:flex}i{display:block;flex-grow:.2;width:0}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c", "d", "e"]),
            vec![
                (0.0, 80.0),
                (80.0, 80.0),
                (160.0, 80.0),
                (240.0, 80.0),
                (320.0, 80.0)
            ]
        );
        // Wide enough that a sum a unit short of 1 would take a unit less.
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i><i id=d></i><i id=e></i></div>",
            "#f{display:flex}i{display:block;flex-grow:.2;width:0}",
            10000.0,
        );
        assert_eq!(
            laid.spans(&["a", "e"]),
            vec![(0.0, 2000.0), (8000.0, 2000.0)]
        );
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i><i id=c></i></div>",
            "#f{display:flex}i{display:block;flex-grow:.4;width:0}#c{flex-grow:.2;max-width:50px}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c"]),
            vec![(0.0, 160.0), (160.0, 160.0), (320.0, 50.0)]
        );
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{display:block}#a{width:100px}#b{flex-grow:.4;width:0}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 100.0), (100.0, 120.0)]);
    }

    /// The line holds its tallest item, in whole 1/64 px rounded up.
    #[test]
    fn the_line_holds_its_tallest_item() {
        let laid = Laid::new(
            "<div id=f><img id=i width=300 height=50></div>",
            "#f{display:flex}img{max-width:100%}",
            200.0,
        );
        let image = laid.border("i");
        assert_eq!((image.2, image.3), (200.0, 50.0 * 200.0 / 300.0));
        let line = laid.geometry("f").content.height;
        assert!(line >= image.3 && line - image.3 < 1.0 / 64.0, "{line}");
        assert_eq!(line * 64.0, (line * 64.0).round());
    }

    /// An item roots a formatting context: its floats stay in it, and
    /// a float beside the container shortens no item's lines.
    #[test]
    fn floats_stay_in_their_items() {
        let laid = Laid::new(
            "<div id=f><div id=a><span id=s>x</span>ab</div><div id=b>cd</div></div><p id=p>after",
            "#f{display:flex}#s{float:left;width:10px;height:50px}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 26.0, 50.0));
        assert_eq!(laid.border("b"), (26.0, 0.0, 16.0, 50.0));
        assert_eq!(laid.border("p").1, 50.0);
        assert_eq!(laid.lines()[1], (0.0, 10.0, "ab".into()));
        // A float before the container: the container is laid beside it.
        let laid = Laid::new(
            "<div id=s></div><div id=f>ab cd<b>ef</b></div>",
            "#s{float:left;width:30px;height:50px}#f{display:flex}",
            400.0,
        );
        assert_eq!(laid.border("f"), (30.0, 0.0, 370.0, 20.0));
        assert_eq!(laid.lines()[0], (0.0, 30.0, "ab cd".into()));
    }

    /// Pictures are items laid as blocks, at their size, not stretched;
    /// a percentage flex-basis is the container's width's.
    #[test]
    fn pictures_and_percentage_bases() {
        let laid = Laid::new(
            "<div id=f><img id=i width=30 height=10><p id=a>ab<br>cd</p></div>",
            ROW,
            400.0,
        );
        assert_eq!(laid.border("i"), (0.0, 0.0, 30.0, 10.0));
        assert_eq!(laid.border("a"), (30.0, 0.0, 16.0, 40.0));
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}#a{flex-basis:25%}#b{flex:0 0 50%}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 100.0), (100.0, 200.0)]);
    }

    /// box-sizing:border-box makes width, flex-basis and min/max widths
    /// border-box ones.
    #[test]
    fn border_box_sizing() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><i id=b></i></div>",
            "#f{display:flex}i{box-sizing:border-box;padding:0 10px}\
             #a{width:100px}#b{flex:0 0 60px;min-width:90px}",
            400.0,
        );
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 100.0), (100.0, 90.0)]);
        assert_eq!(laid.geometry("a").content.width, 80.0);
        assert_eq!(laid.geometry("b").content.width, 70.0);
    }

    /// One line: the container's min-height is its line's, which its
    /// items stretch to; a definite height is the line's whatever its
    /// items'.
    #[test]
    fn the_containers_height_bounds_its_line() {
        let laid = Laid::new(
            "<div id=f><p id=a>ab</p><p id=b>cd</p></div>",
            "#f{display:flex;min-height:50px}#b{align-self:end}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 16.0, 50.0));
        assert_eq!(laid.border("b"), (16.0, 30.0, 16.0, 20.0));
        assert_eq!(laid.border("f").3, 50.0);
        let laid = Laid::new(
            "<div id=f><p id=a>ab<br>cd<br>ef</p><p id=b>gh</p></div>",
            "#f{display:flex;height:30px}#b{align-self:center}",
            400.0,
        );
        assert_eq!(laid.border("a").3, 30.0);
        assert_eq!(laid.border("b").1, 5.0);
        // An item taller than the line is not moved up (safe alignment).
        let laid = Laid::new(
            "<div id=f><p id=a>ab<br>cd<br>ef</p></div>",
            "#f{display:flex;height:30px;align-items:center}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 16.0, 60.0));
        // Percentage heights in an item resolve against a definite line.
        let laid = Laid::new(
            "<div id=f><div id=a style=height:50%></div></div>",
            "#f{display:flex;height:80px}",
            400.0,
        );
        assert_eq!(laid.border("a").3, 40.0);
    }

    /// What is a flex container, and what blockification does to its
    /// children.
    #[test]
    fn what_is_a_flex_container() {
        let styled_of = |html: &str, css: &str| {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            (
                styled.has_flex(),
                styled.has_floats(),
                styled.has_inline_blocks(),
            )
        };
        assert_eq!(styled_of("<p>a<span>b</span>", ""), (false, false, false));
        assert_eq!(styled_of("<p>a", "p{display:flex}"), (true, false, false));
        assert_eq!(
            styled_of("<p>a", "p{display:inline-flex}"),
            (true, false, false)
        );
        assert_eq!(
            styled_of("<p>a", "html{display:flex}"),
            (true, false, false)
        );
        // A float or an inline-block in a flex container is an item.
        assert_eq!(
            styled_of(
                "<p>a<span>b</span><em>c</em>",
                "p{display:flex}span{float:left}em{display:inline-block}"
            ),
            (true, false, false)
        );
        assert_eq!(
            styled_of("<p>a<img>", "p{display:flex}img{float:left}"),
            (true, false, false)
        );
        // inline-flex is laid as flex, block-level for now.
        let laid = Laid::new(
            "<p>ab <span id=f><b>cd</b><b>ef</b></span> gh",
            "#f{display:inline-flex}",
            400.0,
        );
        assert_eq!(laid.border("f"), (0.0, 20.0, 400.0, 20.0));
    }

    fn styled_layout(html: &str, css: &str, request: &Request<'_>, limits: Limits) -> Layout {
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits).unwrap();
        layout(&styled, &mut Metrics, request).unwrap()
    }

    /// Each box's node, content and border rects, and each line's top,
    /// height, x and fragments' ranges: what two layouts of one tree
    /// share where they lay alike.
    fn drawn(out: &Layout) -> (Vec<String>, Vec<String>) {
        let boxes = out
            .boxes
            .iter()
            .map(|g| format!("{:?} {:?} {:?}", g.node, g.content, g.border))
            .collect();
        let lines = out
            .lines
            .iter()
            .map(|l| {
                let pieces = l
                    .fragments
                    .iter()
                    .map(|f| format!("{:?}{:?}@{}", f.node, f.range, f.x))
                    .collect::<String>();
                format!("{} {} {} {pieces}", l.y, l.height, l.width)
            })
            .collect();
        (boxes, lines)
    }

    /// A row of one `width:100%` item lays as block flow does.
    #[test]
    fn one_full_width_item_lays_as_block_flow() {
        let inner = "<div id=i style='width:100%;overflow:hidden;margin:5px 0 7px'>\
            <p>some words here to wrap around the line and more of them\
            <ul><li>an item<li>another <b>bold</b> one</ul>\
            <div style=float:left;width:30px>fl</div><p>beside the float, words\
            <table><tr><td>a cell<td>another</table></div>";
        for width in [120.0, 333.0, 800.0] {
            let request = Request::new(width, 600.0);
            let flex = styled_layout(
                &format!("<div id=f>{inner}</div><p>after"),
                "#f{display:flex}",
                &request,
                Limits::default(),
            );
            let block = styled_layout(
                &format!("<div id=f>{inner}</div><p>after"),
                "#f{display:block}",
                &request,
                Limits::default(),
            );
            assert_eq!(drawn(&flex), drawn(&block), "{width}");
            assert_eq!(flex.height, block.height);
            assert_eq!(flex.overflow, block.overflow);
            assert_eq!(flex.markers.len(), block.markers.len());
        }
    }

    /// Fixed-width items stretched in a row lay as a fixed table's
    /// top-aligned cells: the same border boxes and lines.
    #[test]
    fn fixed_width_items_lay_as_a_fixed_tables_cells() {
        let cells: &[(u32, &str)] = &[
            (50, "ab cd ef gh"),
            (80, "one<br>two three four"),
            (30, "x"),
            (64, "<p>para</p><p>graphs here</p>"),
        ];
        let total: u32 = cells.iter().map(|c| c.0).sum();
        let mut flex = String::from("<div id=f>");
        let mut table = format!("<table style=width:{total}px><tr>");
        for (at, (width, text)) in cells.iter().enumerate() {
            flex.push_str(&format!("<div id=c{at} style=width:{width}px>{text}</div>"));
            table.push_str(&format!("<td id=c{at} style=width:{width}px>{text}</td>"));
        }
        flex.push_str("</div>");
        table.push_str("</table>");
        let request = Request::new(400.0, 600.0);
        let a = styled_layout(&flex, "#f{display:flex}", &request, Limits::default());
        let b = styled_layout(
            &table,
            "table{table-layout:fixed;border-spacing:0}td{vertical-align:top}",
            &request,
            Limits::default(),
        );
        let rects = |out: &Layout, html: &str| {
            let doc = td_html::document::parse(html).unwrap();
            let id = |g: &BoxGeometry| {
                doc.node(g.node)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("id"))
                    .map(str::to_string)
            };
            let mut cells = out
                .boxes
                .iter()
                .filter_map(|g| Some((id(g)?, g.border)))
                .filter(|(id, _)| id.starts_with('c'))
                .collect::<Vec<_>>();
            cells.sort_by(|x, y| x.0.cmp(&y.0));
            cells
        };
        assert_eq!(rects(&a, &flex), rects(&b, &table));
        let lines = |out: &Layout| {
            out.lines
                .iter()
                .map(|l| (l.y, l.height, l.width, l.fragments.first().map(|f| f.x)))
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(&a), lines(&b));
        assert_eq!(a.height, b.height);
    }

    /// A seeded oracle over empty items of random bases, factors, bounds,
    /// margins and gaps: in order and apart, within their bounds, summing
    /// exactly where a flexible item is unclamped, and those sharing in
    /// proportion to their factors (scaled by base size, shrinking)
    /// within 1/64 px.
    #[test]
    fn resolved_lengths_meet_the_spec_invariants() {
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: u64) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (self.0 >> 33) % n
            }
        }
        let mut checked = (0, 0, 0);
        for seed in 0..400u64 {
            let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let count = 1 + r.below(7) as usize;
            let width = 50 + r.below(600);
            let gap = r.below(4) * 3;
            let mut html = String::from("<div id=f>");
            // basis, grow, shrink, min, max, left and right margins.
            let mut items = vec![];
            for at in 0..count {
                let item = (
                    r.below(200) as f64,
                    [0.0, 0.5, 1.0, 2.0, 3.0][r.below(5) as usize],
                    [0.0, 0.5, 1.0, 2.0, 3.0][r.below(5) as usize],
                    if r.below(3) == 0 {
                        r.below(120) as f64
                    } else {
                        0.0
                    },
                    if r.below(3) == 0 {
                        r.below(250) as f64
                    } else {
                        f64::INFINITY
                    },
                    r.below(3) as f64 * 4.0,
                    r.below(2) as f64 * 5.0,
                );
                let max = if item.4.is_finite() {
                    format!("max-width:{}px;", item.4)
                } else {
                    String::new()
                };
                html.push_str(&format!(
                    "<i id=i{at} style='flex:{} {} {}px;min-width:{}px;{max}margin:0 {}px 0 {}px'></i>",
                    item.1, item.2, item.0, item.3, item.6, item.5
                ));
                items.push(item);
            }
            html.push_str("</div>");
            let laid = Laid::new(
                &html,
                &format!("#f{{display:flex;column-gap:{gap}px}}i{{display:block}}"),
                width as f64,
            );
            let got: Vec<BoxGeometry> = (0..count)
                .map(|at| *laid.geometry(&format!("i{at}")))
                .collect();
            let clamp = |(_, _, _, min, max, ..): (f64, f64, f64, f64, f64, f64, f64), v: f64| {
                v.min(max).max(min)
            };
            let outer: f64 = items
                .iter()
                .map(|i| clamp(*i, i.0) + i.5 + i.6)
                .sum::<f64>()
                + gap as f64 * (count - 1) as f64;
            let growing = outer < width as f64;
            let mut end = f64::NEG_INFINITY;
            let mut sum = gap as f64 * (count - 1) as f64;
            let mut free = vec![];
            for (item, g) in items.iter().zip(&got) {
                let w = g.content.width;
                let start = g.border.x - item.5;
                // In order, each `gap` past the last, none overlapping.
                if end.is_finite() {
                    assert_eq!(start, end + gap as f64, "{seed} {html}");
                } else {
                    assert_eq!(start, 0.0, "{seed} {html}");
                }
                end = g.border.x + w + item.6;
                assert_eq!(w, clamp(*item, w), "{seed} {html}");
                assert_eq!(w * 64.0, (w * 64.0).round(), "{seed}");
                sum += w + item.5 + item.6;
                let factor = if growing { item.1 } else { item.2 };
                let lo = item.3;
                let hi = item.4.max(lo);
                if factor > 0.0 && w > lo && w < hi {
                    free.push((
                        w - item.0,
                        if growing { factor } else { factor * item.0 },
                        factor,
                    ));
                }
            }
            // Factors under 1 in all take that share of the free space.
            if free.iter().map(|f| f.2).sum::<f64>() >= 1.0 {
                // Sums exactly: its free space all given.
                assert_eq!(sum, width as f64, "{seed} {html}");
                checked.0 += 1;
            }
            for (a, wa, _) in &free {
                for (b, wb, _) in &free {
                    // a / wa against b / wb, cross-multiplied, within a
                    // unit of each share.
                    assert!(
                        (a * wb - b * wa).abs() <= (wa + wb) / 64.0 + 1e-9,
                        "{seed} {html} {a} {wa} {b} {wb}"
                    );
                    checked.1 += 1;
                }
            }
            checked.2 += growing as usize;
        }
        // Both ways, many times.
        assert!(checked.0 > 150 && checked.1 > 600, "{checked:?}");
        assert!(checked.2 > 100 && checked.2 < 300, "{checked:?}");
    }

    /// A generated page of flex containers whose items move (center, end,
    /// auto margins) and stay (stretch, start), nested, holding lists,
    /// floats, tables and inline-blocks, and in floats, cells and
    /// inline-blocks, among them columns and wrapping rows, which fit
    /// one row at one width and not the other: windowed layouts equal the
    /// filtered whole one.
    #[test]
    fn windowed_layouts_equal_filtered_whole_ones() {
        let mut html = String::new();
        for n in 0..63 {
            let align = ["center", "end", "start", "stretch"][n % 4];
            let lay = ["", " c", " w", " r", " w h", " c h", " w r", " c m", " w m"][n % 9];
            let content = ["normal", "center", "space-between", "end", "space-around"][n % 5];
            let justify = ["normal", "center", "end", "space-evenly"][n % 4];
            html.push_str(&format!(
                "<div class='f{lay}' style=align-items:{align};align-content:{content};\
                 justify-content:{justify}><p>word{n} more words here to wrap\
                 <br>and on</p><span>item <b>bold</b></span>"
            ));
            match n % 6 {
                0 => html.push_str("<ol><li>one<li>two<li>three</ol>"),
                1 => html.push_str("<div><span class=fl>float</span>beside the float</div>"),
                2 => html.push_str(
                    "<div class=f style=align-items:end><i>in</i><i>ner<br>two</i></div>",
                ),
                3 => html.push_str("<table><tr><td>a<br>b<td>c</table>"),
                4 => html.push_str("<p style=margin:auto>auto<br>margins</p>"),
                _ => html.push_str("<p>x <span class=ib>in<br>block</span> y</p>"),
            }
            html.push_str("text after</div>");
            if n % 5 == 0 {
                html.push_str(
                    "<p>a para <span class=ib><span class=f style=align-items:center>\
                     <b>a</b><b>b<br>c</b></span></span> after</p>\
                     <div class=fl><div class=f style=align-items:end><b>x</b><b>y<br>z</b></div></div>\
                     <table><tr><td><div class=f style=align-items:center><b>p</b><b>q<br>r</b></div>\
                     <td>cell</table><p>clear",
                );
            }
        }
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(
            ".f{display:flex;column-gap:4px;padding:2px}.fl{float:left;width:60px}\
             .ib{display:inline-block}li{list-style-position:outside}             .c{flex-direction:column}.w{flex-wrap:wrap}.w>*{flex-basis:30px}\
             .r{flex-direction:row-reverse}.h{height:150px}.m{min-height:150px}\
             .c>p{flex-grow:1}.c>span{margin-top:auto}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(styled.has_flex() && styled.has_floats() && styled.has_inline_blocks());
        for width in [160.0, 400.0] {
            let request = Request::new(width, 600.0);
            let full = layout(&styled, &mut Metrics, &request).unwrap();
            assert!(full.markers.len() >= 18);
            let tall = full.height;
            for window in [
                0.0..100.0,
                150.0..400.0,
                380.0..390.0,
                tall / 3.0..tall / 3.0 + 41.0,
                tall / 2.0..tall / 2.0 + 37.0,
                tall - 50.0..tall + 50.0,
                tall + 1000.0..tall + 1500.0,
            ] {
                let mut asked = request.clone();
                asked.window = Some(window.clone());
                let subset = layout(&styled, &mut Metrics, &asked).unwrap();
                let kept = |y: f64, h: f64| y + h > window.start && y < window.end;
                assert_eq!(
                    subset.lines.iter().collect::<Vec<_>>(),
                    full.lines
                        .iter()
                        .filter(|l| kept(l.y, l.height))
                        .collect::<Vec<_>>(),
                    "{width} {window:?}"
                );
                assert_eq!(subset.boxes, full.boxes, "{window:?}");
                assert_eq!(subset.height, full.height);
                assert_eq!(subset.overflow, full.overflow);
                assert_eq!(subset.line_count, full.line_count);
                assert_eq!(
                    subset
                        .markers
                        .iter()
                        .map(|m| (m.x, m.y, m.line.map(|l| subset.lines[l as usize].y)))
                        .collect::<Vec<_>>(),
                    full.markers
                        .iter()
                        .filter(|m| kept(m.y, m.height))
                        .map(|m| (m.x, m.y, m.line.map(|l| full.lines[l as usize].y)))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }

    /// A container's moved items' lines and markers move with them, and
    /// lines stay in tree order.
    #[test]
    fn moved_items_take_their_lines_and_markers() {
        let laid = Laid::new(
            "<div id=f><p id=a>ab<br>cd<br>ef</p><ul id=b><li>gh</ul></div>",
            "#f{display:flex;align-items:center}li{list-style-position:outside}",
            400.0,
        );
        assert_eq!(laid.border("b").1, 20.0);
        assert_eq!(laid.lines()[3], (20.0, 16.0, "gh".into()));
        let marker = &laid.out.markers[0];
        assert_eq!((marker.line, marker.baseline), (Some(3), 35.0));
    }

    /// One line of 100,000 items is laid in linear work.
    #[test]
    fn many_items_in_one_line_are_linear() {
        let work = |n: usize| {
            let html = format!("<div id=f>{}</div>", "<i>x</i>".repeat(n));
            let out = styled_layout(
                &html,
                "#f{display:flex}i{display:block;flex:1}",
                &Request::new(1000.0, 600.0),
                Limits::default(),
            );
            assert_eq!(out.lines.len(), n);
            out.work
        };
        let (small, large) = (work(10_000), work(100_000));
        assert!(large < 11 * small, "{small} {large}");
        assert!(large < 100 * 100_000, "{large}");
        // An item's record is the size DESIGN.md's bounds charge.
        assert_eq!(std::mem::size_of::<crate::block::flex::FlexItem>(), 224);
    }

    /// Many lines, each one item, and a long column holding its free
    /// space open, cost what as many blocks do, linearly; a long sticky
    /// footer page laid in windows equals the filtered whole, its items
    /// final once those laid reach its least height.
    #[test]
    fn many_lines_and_long_columns_are_linear() {
        let work = |n: usize, css: &str| {
            let html = format!("<div id=f>{}</div>", "<i>x</i>".repeat(n));
            let out = styled_layout(&html, css, &Request::new(1000.0, 600.0), Limits::default());
            assert_eq!(out.lines.len(), n);
            out.work
        };
        for css in [
            "#f{display:flex;flex-wrap:wrap}i{display:block;flex:0 0 100%}",
            "#f{display:flex;flex-wrap:wrap;height:3000000px;align-content:center}\
             i{display:block;flex:0 0 100%}",
            "#f{display:flex;flex-direction:column;height:3000000px}\
             i{display:block;margin-top:auto}",
        ] {
            let (small, large) = (work(10_000, css), work(100_000, css));
            assert!(large < 11 * small, "{css} {small} {large}");
        }
        let html = format!(
            "<body id=f><header>head</header><main>{}</main><footer>foot</footer>",
            "<p>some words here to wrap around the line</p>".repeat(300)
        );
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("body{display:flex;flex-direction:column;min-height:2000px}main{flex:1}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(200.0, 600.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        for window in [
            0.0..100.0,
            1990.0..2100.0,
            5000.0..5200.0,
            full.height - 50.0..full.height,
        ] {
            let mut asked = request.clone();
            asked.window = Some(window.clone());
            let subset = layout(&styled, &mut Metrics, &asked).unwrap();
            let kept = |y: f64, h: f64| y + h > window.start && y < window.end;
            assert_eq!(
                subset.lines.iter().collect::<Vec<_>>(),
                full.lines
                    .iter()
                    .filter(|l| kept(l.y, l.height))
                    .collect::<Vec<_>>(),
                "{window:?}"
            );
            assert_eq!(subset.boxes, full.boxes);
        }
    }

    /// Lines `align-content` moves after all are laid, and a column's
    /// items moved by growth before them, are kept for a window as the
    /// filtered whole; a sticky footer's `main`, which nothing before it
    /// moves, is laid for a window as block flow is, holding no line
    /// above the window.
    #[test]
    fn windows_over_moved_lines_and_columns() {
        let check = |html: &str, css: &str, windows: &[std::ops::Range<f64>]| {
            let doc = td_html::document::parse(html).unwrap();
            let css = sheets(css);
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let request = Request::new(200.0, 600.0);
            let full = layout(&styled, &mut Metrics, &request).unwrap();
            let mut bytes = vec![];
            for window in windows {
                let mut asked = request.clone();
                asked.window = Some(window.clone());
                let subset = layout(&styled, &mut Metrics, &asked).unwrap();
                let kept = |y: f64, h: f64| y + h > window.start && y < window.end;
                assert_eq!(
                    subset.lines.iter().collect::<Vec<_>>(),
                    full.lines
                        .iter()
                        .filter(|l| kept(l.y, l.height))
                        .collect::<Vec<_>>(),
                    "{window:?}"
                );
                assert_eq!(subset.boxes, full.boxes, "{window:?}");
                bytes.push(subset.page_bytes);
            }
            (full, bytes)
        };
        let item = "<div class=i>one two<br>three</div>";
        for content in ["center", "space-between", "end", "stretch", "space-evenly"] {
            let html = format!("<div id=f>{}</div><p>after", item.repeat(40));
            let css = format!(
                "#f{{display:flex;flex-wrap:wrap;height:3000px;align-content:{content}}}\
                 .i{{flex:0 0 50%}}"
            );
            let (full, _) = check(&html, &css, &[0.0..100.0, 1000.0..1100.0, 2900.0..3100.0]);
            assert!(full.height > 3000.0);
        }
        // Lines past the height, released once they reach it, their
        // items aligned in them.
        let html = format!(
            "<div id=f>{}</div><p>after",
            (item.to_owned() + "<i>x</i>").repeat(40)
        );
        let css = "#f{display:flex;flex-wrap:wrap;height:300px;align-items:center}\
                   .i,i{display:block;flex:0 0 50%}";
        check(&html, css, &[0.0..100.0, 280.0..320.0, 700.0..800.0]);
        // A short page whose footer the growing `main` moves to the foot.
        let html = format!(
            "<body><header>head</header><main>{}</main><footer>foot<br>note</footer>",
            "<p>words to wrap here</p>".repeat(5)
        );
        let css = "body{display:flex;flex-direction:column;min-height:2000px}main{flex:1}";
        let (full, _) = check(&html, css, &[0.0..50.0, 1950.0..2050.0, 1990.0..1995.0]);
        assert_eq!(full.height, 2000.0);
        // A long one: `main` reaches the least height, nothing moves.
        let html = format!(
            "<body><header>head</header><main>{}</main><footer>foot</footer>",
            "<p>words to wrap here</p>".repeat(2000)
        );
        let tall = 2.0 * 20.0 * 2000.0;
        let (_, bytes) = check(&html, css, &[0.0..600.0, tall - 600.0..tall]);
        // The window at the foot holds, laying, no more than either holds
        // once laid: its `main`'s 4,000 lines above it are never kept.
        let doc = td_html::document::parse(&html).unwrap();
        let styled =
            StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
        let mut foot = Request::new(200.0, 600.0);
        foot.window = Some(tall - 600.0..tall);
        foot.bytes = Some(bytes[0].max(bytes[1]) + 16 * 1024);
        assert!(layout(&styled, &mut Metrics, &foot).is_ok());
    }

    /// Lines reaching a wrapping container's height leave no free space
    /// for `align-content`: kept for a window at the foot no more than
    /// `flex-start` keeps; and a column of no definite or least height,
    /// which moves nothing, keeps no more than block flow does.
    #[test]
    fn filled_lines_and_unmoving_columns_are_not_kept() {
        let peak = |html: &str, css: &str, ceiling: Option<usize>| {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            let tall = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0))
                .unwrap()
                .height;
            let mut request = Request::new(400.0, 600.0);
            request.window = Some(tall - 600.0..tall);
            request.bytes = ceiling;
            layout(&styled, &mut Metrics, &request).map(|out| out.page_bytes)
        };
        let html = format!(
            "<div id=f>{}</div>",
            "<div class=i>one two<br>three</div>".repeat(3000)
        );
        let wrap = "#f{display:flex;flex-wrap:wrap}.i{flex:0 0 50%}";
        let least = peak(
            &html,
            &format!("{wrap}#f{{align-content:flex-start}}"),
            None,
        )
        .unwrap();
        for lay in [
            "min-height:1px",
            "height:200px",
            "height:200px;align-content:center",
        ] {
            let css = format!("{wrap}#f{{{lay}}}");
            assert!(peak(&html, &css, Some(least + 16 * 1024)).is_ok(), "{lay}");
        }
        // Those laid before the height is reached are aligned as they lie.
        let laid = Laid::new(
            "<div id=f><i id=a>a<br>b</i><i id=b>c</i><i id=c>d</i><i id=d>e</i></div>",
            "#f{display:flex;flex-wrap:wrap;height:40px;align-items:center}\
             i{display:block;flex:0 0 50%}",
            400.0,
        );
        assert_eq!(
            [laid.border("b").1, laid.border("c").1, laid.border("d").1],
            [10.0, 40.0, 40.0]
        );
        // Lines short of it are kept until they are spread.
        let css = format!("{wrap}#f{{height:200000px;align-content:center}}");
        assert!(peak(&html, &css, Some(least + 16 * 1024)).is_err());
        let html = format!("<div id=f>{}</div>", "<i>x</i>".repeat(20_000));
        let block = peak(&html, "i{display:block}", None).unwrap();
        let css = "#f{display:flex;flex-direction:column}i{display:block}";
        assert!(peak(&html, css, Some(block + 16 * 1024)).is_ok());
    }

    /// Columns nested in rows, each measured by the row and laid by it:
    /// charged, refused by name past the work limit, and bounded by depth.
    #[test]
    fn columns_nested_in_rows_are_bounded() {
        let nest = |d: usize| {
            let mut html = String::new();
            for level in 0..d {
                let class = if level % 2 == 0 { "f" } else { "f c" };
                html.push_str(&format!("<div class='{class}'>a "));
            }
            html + "x" + &"</div>".repeat(d)
        };
        let css = ".f{display:flex}.c{flex-direction:column}";
        let work = |d: usize, limits: Limits| {
            let html = nest(d);
            let doc = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits).unwrap();
            layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).map(|out| out.work)
        };
        let (ten, twenty) = (
            work(10, Limits::default()).unwrap(),
            work(20, Limits::default()).unwrap(),
        );
        assert!(twenty > 2 * ten, "{ten} {twenty}");
        let limits = Limits {
            work: ten,
            ..Limits::default()
        };
        assert!(matches!(
            work(20, limits),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
        let limits = Limits {
            depth: 44,
            ..Limits::default()
        };
        assert!(work(38, limits).is_ok());
    }

    /// A seeded oracle over wrapping rows of random bases, factors,
    /// bounds, margins and gaps: each line as many items as fit it, the
    /// next line's first not fitting; on each line, in order and apart,
    /// within their bounds, summing exactly where a flexible item is
    /// unclamped, sharing in proportion to their factors within 1/64 px.
    #[test]
    fn wrapped_lines_meet_the_spec_invariants() {
        struct Rng(u64);
        impl Rng {
            fn below(&mut self, n: u64) -> u64 {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (self.0 >> 33) % n
            }
        }
        let mut checked = (0, 0);
        for seed in 0..300u64 {
            let mut r = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5eed);
            let count = 2 + r.below(12) as usize;
            let width = 100 + r.below(500);
            let gap = r.below(4) * 3;
            let mut html = String::from("<div id=f>");
            // basis, grow, shrink, min, max, left and right margins.
            let mut items = vec![];
            for at in 0..count {
                let item = (
                    r.below(250) as f64,
                    [0.0, 0.5, 1.0, 2.0][r.below(4) as usize],
                    [0.0, 1.0, 2.0][r.below(3) as usize],
                    if r.below(3) == 0 {
                        r.below(80) as f64
                    } else {
                        0.0
                    },
                    if r.below(3) == 0 {
                        r.below(250) as f64
                    } else {
                        f64::INFINITY
                    },
                    r.below(3) as f64 * 4.0,
                    r.below(2) as f64 * 5.0,
                );
                let max = if item.4.is_finite() {
                    format!("max-width:{}px;", item.4)
                } else {
                    String::new()
                };
                html.push_str(&format!(
                    "<i id=i{at} style='flex:{} {} {}px;min-width:{}px;{max}margin:0 {}px 0 {}px'></i>",
                    item.1, item.2, item.0, item.3, item.6, item.5
                ));
                items.push(item);
            }
            html.push_str("</div>");
            let laid = Laid::new(
                &html,
                &format!(
                    "#f{{display:flex;flex-wrap:wrap;column-gap:{gap}px;row-gap:3px}}\
                     i{{display:block;height:10px}}"
                ),
                width as f64,
            );
            let got: Vec<BoxGeometry> = (0..count)
                .map(|at| *laid.geometry(&format!("i{at}")))
                .collect();
            type Item = (f64, f64, f64, f64, f64, f64, f64);
            let clamp = |i: &Item, v: f64| v.min(i.4).max(i.3);
            let outer = |i: &Item| clamp(i, i.0) + i.5 + i.6;
            // Group by line: each line 13 px below the last.
            let mut lines: Vec<Vec<usize>> = vec![];
            for (at, g) in got.iter().enumerate() {
                let line = (g.border.y / 13.0).round() as usize;
                assert_eq!(g.border.y, line as f64 * 13.0, "{seed} {html}");
                if line == lines.len() {
                    lines.push(vec![]);
                }
                assert_eq!(line + 1, lines.len(), "{seed} in tree order");
                lines[line].push(at);
            }
            let room = width as f64;
            for (n, line) in lines.iter().enumerate() {
                let used: f64 = line.iter().map(|&at| outer(&items[at])).sum::<f64>()
                    + gap as f64 * (line.len() - 1) as f64;
                // Fits, within the rounding's slack, unless alone.
                let slack = (2 * line.len() + 1) as f64 / 64.0;
                assert!(line.len() == 1 || used <= room + slack, "{seed} {html}");
                if let Some(next) = lines.get(n + 1) {
                    let more = used + gap as f64 + outer(&items[next[0]]);
                    assert!(more > room + slack - 2.0 / 64.0, "{seed} {html}");
                }
                let growing = used < room;
                let mut end = f64::NEG_INFINITY;
                let mut sum = gap as f64 * (line.len() - 1) as f64;
                let mut free = vec![];
                for &at in line {
                    let (item, g) = (&items[at], &got[at]);
                    let w = g.content.width;
                    let start = g.border.x - item.5;
                    if end.is_finite() {
                        assert_eq!(start, end + gap as f64, "{seed} {html}");
                    } else {
                        assert_eq!(start, 0.0, "{seed} {html}");
                    }
                    end = g.border.x + w + item.6;
                    assert_eq!(w, clamp(item, w), "{seed} {html}");
                    sum += w + item.5 + item.6;
                    let factor = if growing { item.1 } else { item.2 };
                    if factor > 0.0 && w > item.3 && w < item.4.max(item.3) {
                        free.push((
                            w - item.0,
                            if growing { factor } else { factor * item.0 },
                            factor,
                        ));
                    }
                }
                if free.iter().map(|f| f.2).sum::<f64>() >= 1.0 {
                    assert_eq!(sum, room, "{seed} {html}");
                    checked.0 += 1;
                }
                for (a, wa, _) in &free {
                    for (b, wb, _) in &free {
                        assert!(
                            (a * wb - b * wa).abs() <= (wa + wb) / 64.0 + 1e-9,
                            "{seed} {html}"
                        );
                    }
                }
            }
            checked.1 += (lines.len() > 1) as usize;
        }
        assert!(checked.0 > 150 && checked.1 > 150, "{checked:?}");
    }

    /// A cascade of freezes, each pass freezing one item as the free
    /// space it leaves takes the next below its minimum, visits every
    /// item each pass: charged, and refused by name past the limit.
    #[test]
    fn a_freeze_cascade_is_charged_per_pass() {
        // Factors halving, each item's share about half what is left, and
        // its minimum the share it had a pass before, which the last
        // pass's minimum violation lowered: so each pass freezes one at
        // its minimum (§9.7 step 4e). One more item, of a small factor,
        // takes what is left. The gaps halve too, so 1/64 px allows
        // some twenty passes, not a pass an item (DESIGN.md §5, "Flex").
        let chain = 16;
        let width = 100_000.0f64;
        let factors: Vec<f64> = (0..chain).map(|j| (1u64 << (chain - j)) as f64).collect();
        let mut mins = vec![];
        let mut room = width;
        let mut total = factors.iter().sum::<f64>() + 1.0;
        let mut last: Option<f64> = None;
        for (j, factor) in factors.iter().enumerate() {
            let ratio = room / total;
            let min = match last {
                None => ratio * factor + 1000.0,
                Some(last) => last * factor,
            };
            let min = (min * 64.0).round() / 64.0;
            mins.push(min);
            last = Some(ratio);
            room -= min;
            total -= factors[j];
        }
        let page = |fillers: usize, cascade: bool| {
            let mut html = String::from("<div id=f>");
            for (j, factor) in factors.iter().enumerate() {
                let min = if cascade { mins[j] } else { 0.0 };
                html.push_str(&format!(
                    "<i style='flex:{factor} 1 0px;min-width:{min}px'></i>"
                ));
            }
            html.push_str("<i style='flex:1 1 0px'></i>");
            html.push_str(&"<i style=flex:none></i>".repeat(fillers));
            html.push_str("</div>");
            html
        };
        let css = "#f{display:flex;width:100000px}i{display:block}";
        let request = Request::new(width, 600.0);
        let work = |fillers: usize, cascade: bool| {
            styled_layout(&page(fillers, cascade), css, &request, Limits::default()).work
        };
        let (plain, cascade) = (work(20_000, false), work(20_000, true));
        let passes = (cascade - plain) / (3 * (chain + 1 + 20_000));
        assert!(passes + 2 >= chain, "{plain} {cascade} {passes}");
        let limits = Limits {
            work: plain + (cascade - plain) / 2,
            ..Limits::default()
        };
        let doc = td_html::document::parse(&page(20_000, true)).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(css), 0, limits).unwrap();
        assert!(matches!(
            layout(&styled, &mut Metrics, &request),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
    }

    /// Nested auto-width containers each measure their content: work that
    /// is charged and refused by name past the limit; nesting is bounded
    /// by the depth limit.
    #[test]
    fn nested_containers_are_bounded() {
        let nest = |d: usize| {
            format!(
                "<div class=f>{}x{}</div>",
                "<div class=f>a ".repeat(d),
                "</div>".repeat(d)
            )
        };
        let css = ".f{display:flex}";
        let work = |d: usize, limits: Limits| {
            let html = nest(d);
            let doc = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits).unwrap();
            layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).map(|out| out.work)
        };
        let (ten, twenty) = (
            work(10, Limits::default()).unwrap(),
            work(20, Limits::default()).unwrap(),
        );
        assert!(twenty > 3 * ten, "{ten} {twenty}");
        let limits = Limits {
            work: ten,
            ..Limits::default()
        };
        assert!(matches!(
            work(20, limits),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
        let limits = Limits {
            depth: 44,
            ..Limits::default()
        };
        assert!(work(38, limits).is_ok());
        let html = nest(43);
        let doc = td_html::document::parse(&html).unwrap();
        assert!(matches!(
            StyledDocument::new(&doc, &sheets(css), html.len(), limits),
            Err(Error::Limit("MAX_DEPTH"))
        ));
    }

    /// A table item sizes itself in the room its main size leaves, as an
    /// inline table does, and is not stretched.
    #[test]
    fn a_table_item_sizes_itself() {
        let laid = Laid::new(
            "<div id=f><table id=t><tr><td>ab<td>cd</table><p id=a>ab<br>cd<br>ef</p></div>",
            "#f{display:flex}table{border-spacing:0}",
            400.0,
        );
        assert_eq!(laid.border("t"), (0.0, 0.0, 32.0, 20.0));
        assert_eq!(laid.border("a"), (32.0, 0.0, 16.0, 60.0));
        // A flexible one: its basis its columns' width, grown no wider
        // than they lay, its margins and auto margins around it.
        let laid = Laid::new(
            "<div id=f><table id=t><tr><td>ab<td>cd</table><i id=a></i></div>",
            "#f{display:flex}table{border-spacing:0;margin-left:auto;margin-right:10px}\
             i{display:block;width:40px}",
            400.0,
        );
        assert_eq!(laid.border("t"), (318.0, 0.0, 32.0, 20.0));
        assert_eq!(laid.border("a"), (360.0, 0.0, 40.0, 20.0));
    }

    /// A container's lines are not an inline-block's last in flow, as a
    /// table's are not: an inline-block holding only one has its baseline
    /// at its bottom margin edge (DESIGN.md §5, "Inline-blocks").
    #[test]
    fn a_containers_lines_give_no_inline_block_its_baseline() {
        let laid = Laid::new(
            "<p>x <span id=b><span id=f><b>ab</b><b>cd<br>ef</b></span></span> y",
            "#f{display:flex}#b{display:inline-block}",
            400.0,
        );
        // 40 tall, its bottom on the line's baseline, 15 down from a
        // 20 px line's top: the line is 40 + 5 tall.
        assert_eq!(laid.border("b"), (16.0, 0.0, 32.0, 40.0));
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (45.0, 40.0));
    }

    /// A flex page is no text page: text::fit lays none of it.
    #[test]
    fn a_flex_page_is_no_text_page() {
        let html = "<body>some words to fit";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("body{display:flex;white-space:pre-wrap}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(matches!(
            crate::text::fit(&styled, &mut Metrics, 400.0, 10, None),
            Err(Error::Invalid)
        ));
        let css = sheets("body{white-space:pre-wrap}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(crate::text::fit(&styled, &mut Metrics, 400.0, 10, None).is_ok());
    }

    /// Hostile factors and sizes stay in range: content wider than
    /// MAX_COORDINATE shrunk by the largest factor is refused by name or
    /// laid, and no share overflows.
    #[test]
    fn hostile_factors_and_sizes_are_bounded() {
        let word = "a".repeat(2000);
        let html = format!(
            "<div id=f>{}{}</div>",
            format!("<p>{word}</p>").repeat(100),
            format!("<p style=min-width:0>{word}</p>").repeat(100)
        );
        let css = sheets("#f{display:flex}p{flex-shrink:1000000;letter-spacing:1000000px}");
        let doc = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        match layout(&styled, &mut Metrics, &Request::new(1000.0, 600.0)) {
            Ok(_) | Err(Error::Limit(_)) => {}
            Err(e) => panic!("{e:?}"),
        }
        // And growing by the largest factor into the widest room.
        let html = format!("<div id=f>{}</div>", "<i></i>".repeat(1000));
        let css = sheets("#f{display:flex}i{display:block;flex:1000000 1000000 0}");
        let doc = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(MAX_VIEWPORT, 600.0)).unwrap();
        assert_eq!(
            out.boxes
                .iter()
                .filter(|g| g.border.width == 1000.0)
                .count(),
            1000
        );
    }
}
