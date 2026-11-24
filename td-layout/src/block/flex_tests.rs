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

    /// A column lays its children as block flow, and so does a wrapping
    /// row whose items' hypothetical sizes do not fit it unwrapped, as a
    /// layout without flex did; one whose items fit lays one row.
    #[test]
    fn columns_and_wrapping_rows_that_do_not_fit_lay_as_block_flow() {
        let stacked = |laid: &Laid, ids: &[&str], width: f64| {
            for (at, id) in ids.iter().enumerate() {
                let (x, y, w, _) = laid.border(id);
                assert_eq!((x, w), (0.0, width), "{id}");
                assert!(at == 0 || y >= laid.border(ids[at - 1]).1 + 20.0, "{id}");
            }
        };
        let page = "<body id=f><header id=a>head</header><main id=b style=flex:1>main text</main>\
                    <footer id=c>foot</footer>";
        let css = "body{min-height:300px}#f{display:flex;flex-direction:column}";
        let laid = Laid::new(page, css, 400.0);
        stacked(&laid, &["a", "b", "c"], 400.0);
        assert_eq!(laid.border("f").3, 300.0);
        // Laid as the same page without flex.
        let request = Request::new(400.0, 600.0);
        assert_eq!(
            drawn(&styled_layout(page, css, &request, Limits::default())),
            drawn(&styled_layout(
                page,
                "body{min-height:300px}",
                &request,
                Limits::default()
            ))
        );
        let words = "one two three four five six seven eight nine ten";
        let laid = Laid::new(
            &format!("<div id=f><p id=a>{words}<p id=b>{words}<p id=c>{words}</div>"),
            "#f{display:flex;flex-direction:column-reverse}",
            200.0,
        );
        stacked(&laid, &["a", "b", "c"], 200.0);
        let cols = "<div id=f><div id=a>a</div><div id=b>b</div><div id=c>c</div>\
                    <div id=d>d</div></div>";
        for css in [
            ".col{flex:0 0 100%;max-width:100%}",
            ".col{width:100%}",
            ".col{flex:0 0 50%}",
            ".col{flex:0 0 130px}",
        ] {
            let laid = Laid::new(
                &cols
                    .replace("<div id=", "<div class=col id=")
                    .replacen("class=col ", "", 1),
                &format!("#f{{display:flex;flex-wrap:wrap;column-gap:4px}}{css}"),
                400.0,
            );
            stacked(&laid, &["a", "b", "c", "d"], 400.0);
            let laid = Laid::new(
                &cols
                    .replace("<div id=", "<div class=col id=")
                    .replacen("class=col ", "", 1),
                &format!("#f{{display:flex;flex-wrap:wrap-reverse;column-gap:4px}}{css}"),
                400.0,
            );
            stacked(&laid, &["a", "b", "c", "d"], 400.0);
        }
        // Items that fit, the gaps with them, lay one row.
        let laid = Laid::new(
            &cols
                .replace("<div id=", "<div class=col id=")
                .replacen("class=col ", "", 1),
            "#f{display:flex;flex-wrap:wrap;column-gap:4px}.col{flex:0 0 97px}",
            400.0,
        );
        assert_eq!(
            laid.spans(&["a", "b", "c", "d"]),
            vec![(0.0, 97.0), (101.0, 97.0), (202.0, 97.0), (303.0, 97.0)]
        );
        // A pixel over is no rounding: stacked.
        let laid = Laid::new(
            &cols
                .replace("<div id=", "<div class=col id=")
                .replacen("class=col ", "", 1),
            "#f{display:flex;flex-wrap:wrap;column-gap:4px}.col{flex:0 0 97.25px}",
            400.0,
        );
        stacked(&laid, &["a", "b", "c", "d"], 400.0);
        let laid = Laid::new(
            &cols
                .replace("<div id=", "<div class=col id=")
                .replacen("class=col ", "", 1),
            "#f{display:flex;flex-wrap:wrap}.col{flex:1 1 25%}",
            400.0,
        );
        assert_eq!(laid.border("d"), (300.0, 0.0, 100.0, 20.0));
        // A wrapping row measures as its widest item at least and its
        // items' sum at most, and lays at what it measured.
        let css = "#g{display:flex;flex-wrap:wrap;column-gap:6px}#w{float:left}";
        let html = "<div id=w><div id=g><b id=a>ab</b><b id=b>cd efg</b></div></div>";
        let laid = Laid::new(html, css, 400.0);
        assert_eq!(laid.border("w").2, 70.0);
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 16.0), (22.0, 48.0)]);
        let laid = Laid::new(html, css, 20.0);
        assert_eq!(laid.border("w").2, 24.0);
        assert_eq!(laid.spans(&["a", "b"]), vec![(0.0, 24.0), (0.0, 24.0)]);
        // A text run is an item measured alike.
        let laid = Laid::new(
            "<div id=w><div id=g><b id=b>cd efg</b>ab</div></div>",
            css,
            20.0,
        );
        assert_eq!(laid.border("w").2, 24.0);
        // A column measures as block flow.
        let laid = Laid::new(
            html,
            "#g{display:flex;flex-direction:column}#w{float:left}",
            400.0,
        );
        assert_eq!(laid.border("w").2, 48.0);
        stacked(&laid, &["a", "b"], 48.0);
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
        for n in 0..54 {
            let align = ["center", "end", "start", "stretch"][n % 4];
            let lay = ["", " c", " w"][n % 3];
            html.push_str(&format!(
                "<div class='f{lay}' style=align-items:{align}><p>word{n} more words here to wrap\
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
             .ib{display:inline-block}li{list-style-position:outside}             .c{flex-direction:column}.w{flex-wrap:wrap}.w>*{flex-basis:30px}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(styled.has_flex() && styled.has_floats() && styled.has_inline_blocks());
        // How many wrapping containers lay their items in a row.
        let rows = |out: &Layout| {
            let mut seen = std::collections::BTreeMap::new();
            for g in &out.boxes {
                let Some(parent) = doc.node(g.node).and_then(|n| n.parent()) else {
                    continue;
                };
                let class = doc
                    .node(parent)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("class"));
                if class == Some("f w") {
                    seen.entry(parent).or_insert_with(Vec::new).push(g.border.x);
                }
            }
            seen.values()
                .filter(|xs| xs.iter().any(|x| *x != xs[0]))
                .count()
        };
        for width in [160.0, 400.0] {
            let request = Request::new(width, 600.0);
            let full = layout(&styled, &mut Metrics, &request).unwrap();
            assert!(full.markers.len() >= 18);
            let rows = rows(&full);
            assert!(
                if width > 200.0 { rows > 4 } else { rows < 4 },
                "{width} {rows}"
            );
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
        assert_eq!(std::mem::size_of::<crate::block::flex::FlexItem>(), 216);
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
