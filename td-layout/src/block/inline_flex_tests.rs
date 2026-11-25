//! Inline flex containers, laid as inline-blocks are (DESIGN.md §5,
//! "Flex", "Inline-blocks"). Each scalar advances 8 px and a 16 px line
//! is 20 px tall with its baseline 15 px down, so widths, tops and
//! baselines are worked by hand.
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
            (Origin::Author, "#t,.t{display:inline-flex}"),
            (Origin::Author, css),
        ])
        .unwrap()
    }

    struct Laid {
        doc: td_html::document::Document,
        out: Layout,
        nested: bool,
    }

    impl Laid {
        fn new(html: &str, css: &str, width: f64) -> Self {
            let doc = td_html::document::parse(html).unwrap();
            let (out, nested) = {
                let styled =
                    StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
                let out = layout(&styled, &mut Metrics, &Request::new(width, 600.0)).unwrap();
                (out, styled.has_inline_blocks())
            };
            Self { doc, out, nested }
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
                .unwrap()
        }
        fn border(&self, id: &str) -> (f64, f64, f64, f64) {
            let r = self.geometry(id).border;
            (r.x, r.y, r.width, r.height)
        }
        /// Each line's top, its first piece's x and its text, a
        /// placeholder as `[id]`.
        fn lines(&self) -> Vec<(f64, f64, String)> {
            self.out
                .lines
                .iter()
                .map(|line| {
                    let mut pieces: Vec<(f64, String)> = line
                        .fragments
                        .iter()
                        .map(|f| {
                            let node = self.doc.node(f.node).and_then(|n| n.text()).unwrap();
                            (f.x, node[f.range.clone()].to_string())
                        })
                        .collect();
                    for r in line.replaced.iter().filter(|r| r.atomic) {
                        pieces.push((r.x, format!("[{}]", self.id(r.node).unwrap_or(""))));
                    }
                    pieces.sort_by(|a, b| a.0.total_cmp(&b.0));
                    let x = pieces.first().map_or(f64::NAN, |p| p.0);
                    let text = pieces.into_iter().map(|(_, t)| t).collect::<String>();
                    (line.y, x, text)
                })
                .collect()
        }
        /// The placeholder of `id`: its box.
        fn placeholder(&self, id: &str) -> (f64, f64, f64, f64) {
            for line in self.out.lines.iter() {
                if let Some(r) = line
                    .replaced
                    .iter()
                    .find(|r| r.atomic && self.id(r.node) == Some(id))
                {
                    return (r.x, r.y, r.width, r.height);
                }
            }
            panic!("no placeholder for {id}");
        }
        /// The first line's top, height and baseline.
        fn holding(&self) -> (f64, f64, f64) {
            let line = &self.out.lines[0];
            (line.y, line.height, line.baseline)
        }
    }

    /// An inline flex container of two items.
    const T: &str = "<span id=t><b id=a>cd</b><b id=b>ef</b></span>";

    /// Laid whole where its line puts it, shrink-to-fit, its items in a
    /// row, a placeholder its border box, its lines after its line's.
    #[test]
    fn an_inline_flex_is_laid_whole_in_its_line() {
        let laid = Laid::new(&format!("<p>ab {T} gh"), "", 200.0);
        assert!(laid.nested);
        assert_eq!(laid.border("t"), (24.0, 0.0, 32.0, 20.0));
        assert_eq!(laid.border("a"), (24.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.border("b"), (40.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.placeholder("t"), laid.border("t"));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "ab [t] gh".into()),
                (0.0, 24.0, "cd".into()),
                (0.0, 40.0, "ef".into()),
            ]
        );
        assert_eq!(laid.holding(), (0.0, 20.0, 15.0));
        assert_eq!(laid.out.lines[0].width, 80.0);
        // A set width, its items grown into it; a column, as wide as its
        // widest item.
        let laid = Laid::new(&format!("<p>ab {T} gh"), "#t{width:100px}b{flex:1}", 200.0);
        assert_eq!(laid.border("t"), (24.0, 0.0, 100.0, 20.0));
        assert_eq!(laid.border("b"), (74.0, 0.0, 50.0, 20.0));
        let laid = Laid::new(
            &format!("<p>ab {T} gh"),
            "#t{flex-direction:column}#a{width:40px}",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 40.0, 40.0));
        assert_eq!(laid.border("b"), (24.0, 20.0, 40.0, 20.0));
        // Its auto margins are 0, as an inline-block's.
        let laid = Laid::new(&format!("<p>ab {T} gh"), "#t{margin:0 auto}", 200.0);
        assert_eq!(laid.border("t"), (24.0, 0.0, 32.0, 20.0));
    }

    /// Its baseline is its first line's baseline-sharing items', else
    /// its first item's first baseline, a line's or its border box's
    /// bottom; with no item, its bottom margin edge (Flexbox 1 §8.5).
    #[test]
    fn its_baseline_is_its_first_items() {
        let at = |css: &str| {
            let laid = Laid::new(&format!("<p>ab {T}"), css, 200.0);
            (laid.holding(), laid.border("a"), laid.border("b"))
        };
        // Its first item padded: its line's, 25 px down.
        let (holding, a, b) = at("#a{padding-top:10px}");
        assert_eq!(holding, (0.0, 30.0, 25.0));
        assert_eq!((a.3, b.3), (30.0, 30.0));
        // Its second padded: its first item's, 15 px down.
        let (holding, ..) = at("#b{padding-top:10px}");
        assert_eq!(holding, (0.0, 30.0, 15.0));
        // Its second sharing its line's baseline: that, 25 px down.
        let (holding, a, _) = at("#b{padding-top:10px;align-self:baseline}#a{align-self:start}");
        assert_eq!(holding, (0.0, 30.0, 25.0));
        assert_eq!(a.1, 0.0);
        // Both: the first moves down to the second's, 25 px down.
        let (holding, a, b) = at("#t{align-items:baseline}#b{padding-top:10px}");
        assert_eq!(holding, (0.0, 30.0, 25.0));
        assert_eq!((a.1, a.3, b.1, b.3), (10.0, 20.0, 0.0, 30.0));
        // `order` puts the second first: its line's, 15 px down.
        let (holding, ..) = at("#a{padding-top:10px}#b{order:-1}");
        assert_eq!(holding, (0.0, 30.0, 15.0));
        // Its first item's relative offset moves the item, not its
        // baseline; its own moves it, as an inline-block's.
        let (holding, a, _) = at("#a{position:relative;top:-500px}");
        assert_eq!((holding, a.1), ((0.0, 20.0, 15.0), -500.0));
        let (holding, a, _) = at("#a{position:relative;top:-500px}#t{align-items:baseline}");
        assert_eq!((holding, a.1), ((0.0, 20.0, 15.0), -500.0));
        let (holding, a, _) = at("#t{position:relative;top:50px}");
        assert_eq!((holding, a.1), ((0.0, 20.0, 15.0), 50.0));
        let laid = Laid::new(
            &format!("<p>ab {T}"),
            "#t{flex-direction:column}#a{position:relative;top:9px}",
            200.0,
        );
        assert_eq!(laid.holding(), (0.0, 40.0, 15.0));
        // A table item of no line: its border box's bottom, 30 px down,
        // not its margin box's.
        let laid = Laid::new(
            "<p>ab <span id=t><table id=k style=height:30px;width:9px;margin-bottom:6px></table>\
             <b>ef</b></span>",
            "",
            200.0,
        );
        assert_eq!(laid.holding().2, 30.0);
        let laid = Laid::new(
            "<p>ab <span id=t style=flex-direction:column>\
             <table id=k style=height:30px;width:9px;margin-bottom:6px></table><b>ef</b></span>",
            "",
            200.0,
        );
        assert_eq!(laid.holding().2, 30.0);
        // An item of no line: its border box's bottom, 30 px down.
        let laid = Laid::new(
            "<p>ab <span id=t><b id=a style=height:30px></b></span>",
            "",
            200.0,
        );
        assert_eq!(laid.holding(), (0.0, 35.0, 30.0));
        let laid = Laid::new(
            "<p>ab <span id=t><img width=10 height=12></span>",
            "",
            200.0,
        );
        assert_eq!(laid.holding(), (0.0, 20.0, 15.0));
        assert_eq!(laid.border("t"), (24.0, 3.0, 10.0, 12.0));
        // Empty: its bottom margin edge, 30 px down with its margin.
        let laid = Laid::new(
            "<p>ab <span id=t></span>",
            "#t{width:20px;height:26px;margin-bottom:4px}",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 20.0, 26.0));
        assert_eq!(laid.holding(), (0.0, 35.0, 30.0));
        // A column: its first item's, 15 px down, or where
        // `justify-content` moved it.
        let laid = Laid::new(&format!("<p>ab {T}"), "#t{flex-direction:column}", 200.0);
        assert_eq!(laid.holding(), (0.0, 40.0, 15.0));
        let laid = Laid::new(
            &format!("<p>ab {T}"),
            "#t{flex-direction:column;height:100px;justify-content:end}",
            200.0,
        );
        assert_eq!(laid.border("a").1, 60.0);
        assert_eq!(laid.holding(), (0.0, 100.0, 75.0));
        // A wrapping row: its first line's, where `align-content` moved it.
        let laid = Laid::new(
            &format!("<p>ab {T}"),
            "#t{flex-wrap:wrap;width:16px;height:100px;align-content:end}",
            200.0,
        );
        assert_eq!(laid.border("a").1, 60.0);
        assert_eq!(laid.holding(), (0.0, 100.0, 75.0));
        // Its items sharing the line's baseline, 10 px down its padded
        // first: its line's, moved 50 px, 75 px down.
        let laid = Laid::new(
            &format!("<p>ab {T}"),
            "#t{flex-wrap:wrap;width:16px;height:100px;align-content:end;align-items:baseline}\
             #a{padding-top:10px}",
            200.0,
        );
        assert_eq!(laid.border("a").1, 50.0);
        assert_eq!(laid.holding(), (0.0, 100.0, 75.0));
        // An anonymous item's line.
        let laid = Laid::new(
            "<p>ab <span id=t>xy<b id=a style=padding-top:10px>cd</b></span>",
            "#t{align-items:start}",
            200.0,
        );
        assert_eq!(laid.holding(), (0.0, 30.0, 15.0));
    }

    /// It moves whole to the next line where it does not fit, and stays
    /// on one that does not wrap.
    #[test]
    fn it_wraps_whole_where_its_parent_wraps() {
        let laid = Laid::new(&format!("<p>aaaa bbbb {T} ee"), "", 100.0);
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa bbbb".into()),
                (20.0, 0.0, "[t] ee".into()),
                (20.0, 0.0, "cd".into()),
                (20.0, 16.0, "ef".into()),
            ]
        );
        assert_eq!(laid.border("t"), (0.0, 20.0, 32.0, 20.0));
        let laid = Laid::new(
            &format!("<p style=white-space:nowrap>aaaa bbbb {T} ee"),
            "",
            100.0,
        );
        assert_eq!(laid.lines()[0], (0.0, 0.0, "aaaa bbbb [t] ee".into()));
        assert_eq!(laid.out.lines[0].width, 136.0);
    }

    /// text-align moves its line, and it and its items' lines with it.
    #[test]
    fn text_align_moves_it_with_its_line() {
        for (align, shift) in [("center", 22.0), ("right", 44.0)] {
            let laid = Laid::new(&format!("<p style=text-align:{align}>ab {T}"), "", 100.0);
            assert_eq!(laid.border("t"), (shift + 24.0, 0.0, 32.0, 20.0));
            assert_eq!(laid.border("b"), (shift + 40.0, 0.0, 16.0, 20.0));
            assert_eq!(laid.lines()[2], (0.0, shift + 40.0, "ef".into()));
            assert_eq!(laid.placeholder("t"), laid.border("t"));
        }
    }

    /// Measured whole in a cell and a float: a row's sum of its items, a
    /// wrapping row's widest for its least, a column's widest.
    #[test]
    fn it_is_measured_in_cells_and_floats() {
        let css = "table{border-spacing:0}";
        let laid = Laid::new(&format!("<table><tr><td id=d>ab {T}</table>"), css, 200.0);
        assert_eq!(laid.border("d").2, 56.0);
        let laid = Laid::new(
            &format!("<table style=width:1px><tr><td id=d>ab {T}</table>"),
            css,
            200.0,
        );
        assert_eq!(laid.border("d").2, 32.0);
        assert_eq!(laid.border("t"), (0.0, 20.0, 32.0, 20.0));
        let float = |css: &str, width: f64| {
            let html = format!("<div id=f>ab {T}</div>");
            Laid::new(&html, &format!("#f{{float:left}}{css}"), width)
                .border("f")
                .2
        };
        assert_eq!(float("", 200.0), 56.0);
        assert_eq!(float("", 10.0), 32.0);
        assert_eq!(float("#t{flex-wrap:wrap}", 10.0), 16.0);
        assert_eq!(float("#t{flex-direction:column}", 200.0), 40.0);
        assert_eq!(float("#t{margin-left:4px;border:1px solid}", 200.0), 62.0);
        // A cell's baseline is the line holding it.
        let laid = Laid::new(
            &format!("<table><tr><td>x<td id=d>{T} gh</table>"),
            "td{vertical-align:baseline}table{border-spacing:0}#a{padding-top:10px}",
            200.0,
        );
        let x = laid.lines().into_iter().find(|l| l.2 == "x").unwrap();
        assert_eq!(x.0, 10.0);
        assert_eq!(laid.border("t"), (8.0, 0.0, 32.0, 30.0));
    }

    /// A windowed layout equals the filtered whole one, with inline flex
    /// containers above, below and across the window, their items
    /// aligned, wrapped, in columns, holding lists and floats.
    #[test]
    fn windowed_layouts_equal_filtered_whole_ones() {
        let mut html = String::new();
        for n in 0..40 {
            html.push_str(&format!(
                "<p>word{n} word <span class=t style=height:{h}px><b>in it<br>and on</b>\
                 <b class=d>two</b><b>three</b></span> more words here to wrap \
                 <span class=t><b>x</b></span> ok",
                h = 10 + (n * 7) % 90
            ));
            if n % 6 == 0 {
                html.push_str(
                    "<p>a list <span class=t style=flex-direction:column><b>cap</b>\
                     <b><ol><li>in<li>side</ol>its words</b></span> after",
                );
            }
            if n % 5 == 0 {
                html.push_str(
                    "<p>with <span class=t style=flex-wrap:wrap;width:60px><b><i>float</i>\
                     beside it</b><b>and</b> <span class=t><b>nested</b><b>in<br>it</b>\
                     </span> too</span> end",
                );
            }
        }
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(
            ".t{padding:2px;align-items:baseline}.d{align-self:center;padding-top:5px}\
             i{float:right;width:30px}li{list-style-position:outside}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(200.0, 600.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        assert!(full.markers.len() >= 10);
        assert!(
            full.lines
                .iter()
                .flat_map(|l| l.replaced.iter())
                .filter(|r| r.atomic)
                .count()
                >= 90
        );
        let tall = full.height;
        for window in [
            0.0..100.0,
            150.0..400.0,
            380.0..390.0,
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
                "{window:?}"
            );
            assert_eq!(subset.boxes, full.boxes, "{window:?}");
            assert_eq!(subset.height, full.height);
            assert_eq!(subset.overflow, full.overflow);
            assert_eq!(subset.line_count, full.line_count);
        }
    }

    /// Dense pages of inline flex containers, in one paragraph and in
    /// many, cost linearly in work and in what they charge.
    #[test]
    fn dense_pages_are_linear() {
        let cost = |n: usize, one: &str| {
            let html = format!("<p>{}", one.repeat(n));
            let doc = td_html::document::parse(&html).unwrap();
            let css = sheets(".t{align-items:baseline}.u{padding-top:3px}");
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(1000.0, 600.0)).unwrap();
            assert!(out.lines.len() >= n, "{}", out.lines.len());
            (out.work, out.page_bytes)
        };
        for one in [
            "<span class=t><b>a</b><b class=u>b</b></span> ",
            "<p>x <span class=t style=flex-direction:column><b>a</b><b>b</b></span> y",
        ] {
            let (small, large) = (cost(2_000, one), cost(20_000, one));
            assert!(large.0 < 11 * small.0, "{one} {small:?} {large:?}");
            // Charges as vectors double: within a doubling of linear.
            assert!(large.1 < 12 * small.1, "{one} {small:?} {large:?}");
        }
    }

    /// A picture whose display is `inline-flex` or `inline-table` is an
    /// inline picture, in its line, as an `inline-block` one is.
    #[test]
    fn a_picture_of_an_atomic_display_is_inline() {
        for display in ["inline-block", "inline-table", "inline-flex"] {
            let laid = Laid::new(
                &format!("<p>ab <img id=i width=10 height=12 style=display:{display}> cd"),
                "",
                200.0,
            );
            assert_eq!(laid.lines().len(), 1, "{display}");
            assert_eq!(laid.out.lines[0].replaced[0].x, 24.0, "{display}");
        }
    }

    /// What is one: a block whose display is inline-flex, not the root; a
    /// floated one is a block-level flex container.
    #[test]
    fn what_is_an_inline_flex() {
        let html = format!("<p>ab {T} gh");
        let laid = Laid::new(&html, "", 200.0);
        let css = sheets("");
        let styled = StyledDocument::new(&laid.doc, &css, html.len(), Limits::default()).unwrap();
        let marked: Vec<_> = laid
            .out
            .boxes
            .iter()
            .filter(|g| styled.is_inline_block(g))
            .collect();
        assert_eq!(marked, vec![laid.geometry("t")]);
        let floated = Laid::new(&html, "#t{float:left}", 200.0);
        assert!(!floated.nested);
        assert_eq!(floated.border("t"), (0.0, 0.0, 32.0, 20.0));
    }
}
