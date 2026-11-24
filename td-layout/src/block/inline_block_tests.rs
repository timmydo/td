//! Inline-blocks (DESIGN.md §5, "Inline-blocks"). Each scalar advances
//! 8 px and a 16 px line is 20 px tall with its baseline 15 px down, so
//! widths, tops and baselines are worked by hand.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::*;
    use crate::inline::{FontLine, ReplacedFragment};
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
                .unwrap()
        }
        fn border(&self, id: &str) -> Rect {
            self.geometry(id).border
        }
        /// Each line's top, its first piece's x and its text, an
        /// inline-block's placeholder as `[id]`.
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
        /// The placeholder of the inline-block `id`: its line and its box.
        fn placeholder(&self, id: &str) -> (usize, Rect) {
            for (at, line) in self.out.lines.iter().enumerate() {
                if let Some(r) = line
                    .replaced
                    .iter()
                    .find(|r| r.atomic && self.id(r.node) == Some(id))
                {
                    let rect = Rect {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                    };
                    return (at, rect);
                }
            }
            panic!("no placeholder for {id}");
        }
    }

    fn rect(r: Rect) -> (f64, f64, f64, f64) {
        (r.x, r.y, r.width, r.height)
    }

    const IB: &str = "#b{display:inline-block}";

    /// Shrink-to-fit, placed where its line puts it, a placeholder its
    /// border box there, and its lines right after the line holding it.
    #[test]
    fn an_inline_block_is_laid_whole_in_its_line() {
        let laid = Laid::new("<p>ab <span id=b>cd ef</span> gh", IB, 200.0);
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 40.0, 20.0));
        assert_eq!(laid.placeholder("b"), (0, laid.border("b")));
        assert_eq!(
            laid.lines(),
            vec![(0.0, 0.0, "ab [b] gh".into()), (0.0, 24.0, "cd ef".into()),]
        );
        let holding = &laid.out.lines[0];
        assert_eq!(
            (holding.height, holding.baseline, holding.width),
            (20.0, 15.0, 88.0)
        );
        // Its box, then its block's, in postorder.
        assert_eq!(laid.out.boxes.len(), 4);
        // A placeholder fits the padding a replaced fragment had; an
        // item laid apart is the size DESIGN.md's bounds charge.
        assert_eq!(std::mem::size_of::<ReplacedFragment>(), 48);
        assert_eq!(std::mem::size_of::<crate::block::float::Item>(), 136);
    }

    /// Its baseline is its last line's, padding and all; past a set
    /// height, still its last line's; its bottom margin edge with no line
    /// or where its overflow is not visible (CSS 2.2 §10.8.1).
    #[test]
    fn its_baseline_is_its_last_lines_or_its_bottom_margin_edge() {
        // Two lines from 10 px down: the last's baseline is 45 px down.
        let laid = Laid::new(
            "<p>ab <span id=b>cd<br>ef</span> gh",
            "#b{display:inline-block;padding-top:10px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!(
            (holding.y, holding.height, holding.baseline),
            (0.0, 50.0, 45.0)
        );
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 16.0, 50.0));
        assert_eq!(laid.lines()[2], (30.0, 24.0, "ef".into()));
        // A set height taller than its line.
        let laid = Laid::new(
            "<p>ab <span id=b>cd</span>",
            "#b{display:inline-block;height:50px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (50.0, 15.0));
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 16.0, 50.0));
        // A set height shorter than its lines, which overflow it: still
        // the last line's, 35 px down.
        let laid = Laid::new(
            "<p>ab <span id=b>cd<br>ef</span>",
            "#b{display:inline-block;height:10px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (40.0, 35.0));
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 16.0, 10.0));
        // Overflow hidden: its bottom margin edge.
        let laid = Laid::new(
            "<p>ab <span id=b>cd</span>",
            "#b{display:inline-block;overflow:hidden;margin-bottom:4px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (29.0, 24.0));
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.lines()[1], (0.0, 24.0, "cd".into()));
        // Overflow clip, as visible: its last line's.
        let laid = Laid::new(
            "<p>ab <span id=b>cd</span>",
            "#b{display:inline-block;overflow:clip;margin-bottom:4px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (24.0, 15.0));
        // Empty: its bottom margin edge, on the line's baseline.
        let laid = Laid::new(
            "<p>ab <span id=b></span>",
            "#b{display:inline-block;width:10px;height:6px}",
            200.0,
        );
        assert_eq!(laid.out.lines.len(), 1);
        assert_eq!(laid.out.lines[0].height, 20.0);
        assert_eq!(rect(laid.border("b")), (24.0, 9.0, 10.0, 6.0));
        assert_eq!(laid.placeholder("b"), (0, laid.border("b")));
        // Its own lines alone count: not a paragraph's before it, nor a
        // float's in it, nor a table's in it.
        let laid = Laid::new(
            "<p>x<p>ab <span id=b></span>",
            "#b{display:inline-block;width:10px;height:6px}",
            200.0,
        );
        assert_eq!(rect(laid.border("b")), (24.0, 29.0, 10.0, 6.0));
        let laid = Laid::new(
            "<p>ab <span id=b><span id=f>F</span></span>",
            "#b{display:inline-block}#f{float:left}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (25.0, 20.0));
        let laid = Laid::new(
            "<p>ab <span id=b>cd<table><tr><td>xx</table></span>",
            "#b{display:inline-block}table{border-spacing:0}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (40.0, 15.0));
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 16.0, 40.0));
    }

    /// Its margins take room in the line, negative ones giving it back.
    #[test]
    fn its_margins_take_room_and_give_it_back() {
        let laid = Laid::new(
            "<p>ab <span id=b>cd</span> gh",
            "#b{display:inline-block;margin:3px 4px 5px 6px}",
            200.0,
        );
        // A margin box 26 by 28, its baseline 18 px down.
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (28.0, 18.0));
        assert_eq!(rect(laid.border("b")), (30.0, 3.0, 16.0, 20.0));
        assert_eq!(laid.placeholder("b").1, laid.border("b"));
        assert_eq!(laid.out.lines[0].fragments[1].x, 50.0);
        assert_eq!(laid.lines()[1], (3.0, 30.0, "cd".into()));
        // Negative: a margin box 8 by 15, its baseline 10 px down.
        let laid = Laid::new(
            "<p>ab <span id=b>cd</span> gh",
            "#b{display:inline-block;margin:-5px 0 0 -8px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (20.0, 15.0));
        assert_eq!(rect(laid.border("b")), (16.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.out.lines[0].fragments[1].x, 32.0);
        // A margin box of negative height: its ascent -15 px and its
        // descent 5 px grow no line.
        let laid = Laid::new(
            "<p>ab <span id=b>x</span> cd",
            "#b{display:inline-block;margin-top:-30px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (20.0, 15.0));
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 8.0, 20.0));
        assert_eq!(laid.placeholder("b").1, laid.border("b"));
        let laid = Laid::new(
            "<p>ab <span id=b></span> cd",
            "#b{display:inline-block;margin-bottom:-10px}",
            200.0,
        );
        let holding = &laid.out.lines[0];
        assert_eq!((holding.height, holding.baseline), (20.0, 15.0));
        assert_eq!(rect(laid.border("b")), (24.0, 25.0, 0.0, 0.0));
    }

    /// It moves whole to the next line where it does not fit, stands alone
    /// on one where it is wider, and stays on one that does not wrap.
    #[test]
    fn it_wraps_whole_where_its_parent_wraps() {
        let laid = Laid::new("<p>aaaa bbbb <span id=b>cc dd</span> ee", IB, 100.0);
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa bbbb".into()),
                (20.0, 0.0, "[b] ee".into()),
                (20.0, 0.0, "cc dd".into()),
            ]
        );
        assert_eq!(rect(laid.border("b")), (0.0, 20.0, 40.0, 20.0));
        let laid = Laid::new(
            "<p>aa <span id=b>cc</span> ee",
            "#b{display:inline-block;width:150px}",
            100.0,
        );
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aa".into()),
                (20.0, 0.0, "[b]".into()),
                (20.0, 0.0, "cc".into()),
                (40.0, 0.0, "ee".into()),
            ]
        );
        assert_eq!(rect(laid.border("b")), (0.0, 20.0, 150.0, 20.0));
        let laid = Laid::new(
            "<p style=white-space:nowrap>aaaa bbbb <span id=b>cc dd</span> ee",
            IB,
            100.0,
        );
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa bbbb [b] ee".into()),
                (0.0, 80.0, "cc dd".into()),
            ]
        );
        assert_eq!(laid.out.lines[0].width, 144.0);
        // Its parent's white space, not its own, breaks around it.
        let laid = Laid::new(
            "<p style=white-space:nowrap>aaaa bbbb <span id=b \
             style=white-space:normal>cc dd</span> ee",
            IB,
            100.0,
        );
        assert_eq!(laid.lines()[0], (0.0, 0.0, "aaaa bbbb [b] ee".into()));
    }

    /// Collapsible spaces around it collapse to one each side; inside it
    /// they collapse at its edges as at a block's.
    #[test]
    fn white_space_around_and_inside_it() {
        let laid = Laid::new("<p>ab   <span id=b>  cd  </span>   ef", IB, 200.0);
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 16.0, 20.0));
        assert_eq!(
            laid.lines(),
            vec![(0.0, 0.0, "ab [b] ef".into()), (0.0, 24.0, "cd".into())]
        );
        assert_eq!(laid.out.lines[0].fragments[1].x, 40.0);
    }

    /// text-align moves its line, and it and its lines with it.
    #[test]
    fn text_align_moves_it_with_its_line() {
        for (align, shift) in [("center", 30.0), ("right", 60.0)] {
            let laid = Laid::new(
                &format!("<p style=text-align:{align}>ab <span id=b>cd</span>"),
                IB,
                100.0,
            );
            assert_eq!(rect(laid.border("b")), (shift + 24.0, 0.0, 16.0, 20.0));
            assert_eq!(laid.lines()[1], (0.0, shift + 24.0, "cd".into()));
            assert_eq!(laid.placeholder("b").1, laid.border("b"));
        }
    }

    /// It roots a formatting context, which holds its floats; an outer
    /// float shortens the line holding it.
    #[test]
    fn it_holds_its_floats_and_sits_beside_outer_ones() {
        let laid = Laid::new(
            "<p>ab <span id=b><span id=f></span>cd</span>",
            "#b{display:inline-block}#f{float:left;width:10px;height:30px}",
            200.0,
        );
        assert_eq!(rect(laid.border("b")), (24.0, 0.0, 26.0, 30.0));
        assert_eq!(rect(laid.border("f")), (24.0, 0.0, 10.0, 30.0));
        assert_eq!(
            laid.lines(),
            vec![(0.0, 0.0, "ab [b]".into()), (0.0, 34.0, "cd".into())]
        );
        assert_eq!(laid.out.lines[0].height, 30.0);
        let laid = Laid::new(
            "<p><span id=f></span>ab <span id=b>cd</span>",
            "#b{display:inline-block}#f{float:left;width:30px;height:50px}",
            200.0,
        );
        assert_eq!(rect(laid.border("b")), (54.0, 0.0, 16.0, 20.0));
        assert_eq!(laid.lines()[0], (0.0, 30.0, "ab [b]".into()));
    }

    /// One inside another: each laid apart in its holder's sequence, its
    /// lines after the line holding it.
    #[test]
    fn nested_inline_blocks() {
        let laid = Laid::new(
            "<p>ab <span id=o>cd <span id=i>ef</span> gh</span> ij",
            "#o,#i{display:inline-block}",
            200.0,
        );
        assert_eq!(rect(laid.border("o")), (24.0, 0.0, 64.0, 20.0));
        assert_eq!(rect(laid.border("i")), (48.0, 0.0, 16.0, 20.0));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "ab [o] ij".into()),
                (0.0, 24.0, "cd [i] gh".into()),
                (0.0, 48.0, "ef".into()),
            ]
        );
    }

    /// In a table cell: measured whole, a wrap opportunity each side; the
    /// cell's baseline is the line holding it.
    #[test]
    fn in_a_cell_it_is_measured_and_aligns_the_row() {
        let css = "#b{display:inline-block}table{border-spacing:0}";
        let laid = Laid::new(
            "<table><tr><td id=c>ab <span id=b>cd ef</span></table>",
            css,
            200.0,
        );
        assert_eq!(laid.border("c").width, 64.0);
        let laid = Laid::new(
            "<table style=width:1px><tr><td id=c>ab <span id=b>cd ef</span></table>",
            css,
            200.0,
        );
        assert_eq!(laid.border("c").width, 16.0);
        assert_eq!(rect(laid.border("b")), (0.0, 20.0, 16.0, 40.0));
        // Baseline-aligned beside a cell led by an inline-block of two
        // lines: its baseline is the line holding it, 35 px down.
        let laid = Laid::new(
            "<table><tr><td id=x>x<td><span id=b>cd<br>ef</span> gh</table>",
            "#b{display:inline-block}td{vertical-align:baseline}table{border-spacing:0}",
            200.0,
        );
        let x = laid
            .lines()
            .into_iter()
            .find(|(_, _, text)| text == "x")
            .unwrap();
        assert_eq!(x.0, 20.0);
        assert_eq!(laid.border("b").y, 0.0);
    }

    /// A float's shrink-to-fit width counts it whole, its own min- and
    /// max-content widths; in a run that does not wrap, its least is part
    /// of the word around it.
    #[test]
    fn a_float_shrinks_to_fit_around_it() {
        let css = "#f{float:left}#b{display:inline-block}";
        let html = "<div id=f>ab <span id=b>cd ef</span></div>";
        assert_eq!(Laid::new(html, css, 200.0).border("f").width, 64.0);
        assert_eq!(Laid::new(html, css, 30.0).border("f").width, 30.0);
        assert_eq!(Laid::new(html, css, 10.0).border("f").width, 16.0);
        let html = "<div id=f style=white-space:nowrap>ab <span id=b \
                    style=white-space:normal>cd ef</span> gh</div>";
        assert_eq!(Laid::new(html, css, 200.0).border("f").width, 88.0);
        assert_eq!(Laid::new(html, css, 10.0).border("f").width, 64.0);
        // Where its parent wraps, a wrap opportunity with no space.
        let html = "<div id=f>ab<span id=b>cd ef</span></div>";
        assert_eq!(Laid::new(html, css, 10.0).border("f").width, 16.0);
    }

    /// An outside marker waiting for its item's first line takes the line
    /// holding the inline-block, not the inline-block's own; a marker in
    /// it moves with its line.
    #[test]
    fn outside_markers_take_the_line_holding_it() {
        let laid = Laid::new(
            "<ul><li>ab <span id=b>cd<ul><li>x</ul></span> ef</ul>",
            "#b{display:inline-block}li{list-style-position:outside}",
            200.0,
        );
        assert_eq!(
            laid.lines()
                .into_iter()
                .map(|(_, _, t)| t)
                .collect::<Vec<_>>(),
            ["ab [b] ef", "cd", "x"]
        );
        let markers = &laid.out.markers;
        assert_eq!(markers.len(), 2);
        assert_eq!(markers[0].line, Some(0));
        assert_eq!(markers[1].line, Some(2));
        for marker in markers.iter() {
            let line = &laid.out.lines[marker.line.unwrap() as usize];
            assert_eq!(marker.baseline, line.y + line.baseline);
        }
        assert_eq!(markers[1].x + markers[1].width, 24.0);
    }

    /// visibility:hidden keeps its geometry.
    #[test]
    fn a_hidden_one_keeps_its_geometry() {
        let html = "<p>ab <span id=b>cd ef</span> gh";
        let shown = Laid::new(html, IB, 200.0);
        let hidden = Laid::new(html, "#b{display:inline-block;visibility:hidden}", 200.0);
        let geometry = |laid: &Laid| {
            laid.out
                .boxes
                .iter()
                .map(|g| (g.border, g.content))
                .collect::<Vec<_>>()
        };
        assert_eq!(geometry(&shown), geometry(&hidden));
        assert_eq!(shown.lines(), hidden.lines());
    }

    /// A windowed layout equals the filtered whole one, with inline-blocks
    /// above, below and across the window, some with list markers and
    /// floats in them.
    #[test]
    fn windowed_layouts_equal_filtered_whole_ones() {
        let mut html = String::new();
        for n in 0..40 {
            html.push_str(&format!(
                "<p>word{n} word <span class=b style=height:{h}px>in it<br>and on</span> \
                 more words here to wrap <span class=b>x</span> ok",
                h = 10 + (n * 7) % 90
            ));
            if n % 6 == 0 {
                html.push_str(
                    "<p>a list <span class=b><ol><li>in<li>side</ol>its words</span> after",
                );
            }
            if n % 5 == 0 {
                html.push_str(
                    "<p>with <span class=b><span class=f>float</span>beside it</span> \
                     and <span class=b>nested <span class=b>in<br>it</span> too</span> end",
                );
            }
        }
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(
            ".b{display:inline-block;padding:2px}.f{float:right;width:30px}\
             li{list-style-position:outside}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(200.0, 600.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        let mut last = 0;
        for marker in full.markers.iter() {
            let at = marker.line.unwrap() as usize;
            let line = &full.lines[at];
            assert_eq!(marker.baseline, line.y + line.baseline);
            assert!(at >= last);
            last = at;
        }
        assert!(full.markers.len() >= 10);
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

    /// A line cut at `line_scalars`, or holding that many items, ends
    /// before an inline-block rather than dropping it: its box and lines
    /// are placed.
    #[test]
    fn a_cut_line_ends_before_it_rather_than_dropping_it() {
        let limits = Limits {
            line_scalars: 4,
            ..Limits::default()
        };
        let laid = Laid::with(
            "<p style=white-space:nowrap>abcdefgh <span id=b>cd</span> ij<span id=c>k</span>",
            "#b,#c{display:inline-block}",
            200.0,
            limits,
        );
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "abcd".into()),
                (20.0, 0.0, "[b] ij".into()),
                (20.0, 0.0, "cd".into()),
                (40.0, 0.0, "[c]".into()),
                (40.0, 0.0, "k".into()),
            ]
        );
        assert_eq!(rect(laid.border("b")), (0.0, 20.0, 16.0, 20.0));
        assert_eq!(rect(laid.border("c")), (0.0, 40.0, 8.0, 20.0));
        assert_eq!(laid.out.cuts.lines, 1);
    }

    /// Nesting past the depth limit is refused by name; nested auto-width
    /// inline-blocks each measure their content, work that is charged.
    #[test]
    fn depth_and_work_are_bounded() {
        let nest = |d: usize| {
            format!(
                "<p>{}x{}",
                "<span class=b>a ".repeat(d),
                "</span>".repeat(d)
            )
        };
        let css = sheets(".b{display:inline-block}");
        let work = |d: usize| {
            let html = nest(d);
            let doc = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            layout(&styled, &mut Metrics, &Request::new(400.0, 600.0))
                .unwrap()
                .work
        };
        // Each level measures all below it: more than linear.
        let (ten, twenty) = (work(10), work(20));
        assert!(twenty > 3 * ten, "{ten} {twenty}");
        // Laid no deeper than its boxes, which are refused first.
        let limits = Limits {
            depth: 44,
            ..Limits::default()
        };
        let html = nest(40);
        let doc = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), limits).unwrap();
        assert!(layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).is_ok());
        let html = nest(41);
        let doc = td_html::document::parse(&html).unwrap();
        assert!(matches!(
            StyledDocument::new(&doc, &css, html.len(), limits),
            Err(Error::Limit("MAX_DEPTH"))
        ));
        let limits = Limits {
            work: ten,
            ..Limits::default()
        };
        let html = nest(20);
        let doc = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), limits).unwrap();
        assert!(matches!(
            layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
    }

    /// What is an inline-block: a block box whose display is inline-block,
    /// not an image, not the root, and not a float, which td-css
    /// blockifies.
    #[test]
    fn what_is_an_inline_block() {
        let styled_of = |html: &str, css: &str| {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            (
                styled.has_inline_blocks(),
                styled.has_floats(),
                styled.has_nested_lines(),
            )
        };
        assert_eq!(styled_of("<p>a<span>b</span>", ""), (false, false, false));
        assert_eq!(styled_of("<p>a<span>b</span>", IB), (false, false, false));
        assert_eq!(
            styled_of("<p>a<span id=b>b</span>", IB),
            (true, false, true)
        );
        assert_eq!(
            styled_of("<p>a<img>", "img{display:inline-block}"),
            (false, false, false)
        );
        assert_eq!(
            styled_of("<p>a", "html{display:inline-block}"),
            (false, false, false)
        );
        assert_eq!(
            styled_of(
                "<p>a<span>b</span>",
                "span{display:inline-block;float:left}"
            ),
            (false, true, true)
        );
        // Laid as a float, not in the line.
        let laid = Laid::new(
            "<p>ab <span id=b>cd</span> ef",
            "#b{display:inline-block;float:right}",
            200.0,
        );
        assert_eq!(rect(laid.border("b")), (184.0, 0.0, 16.0, 20.0));
        assert!(laid.out.lines.iter().all(|l| l.replaced.is_empty()));
        // A text page with one is not one `text::fit` counts.
        let html = "<!doctype html><plaintext>a\nb";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("plaintext{display:inline-block;white-space:pre-wrap}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(styled.has_inline_blocks());
        assert!(matches!(
            crate::text::fit(&styled, &mut Metrics, 100.0, 10, None),
            Err(Error::Invalid)
        ));
        let html = "<p>ab <span id=b>cd</span>";
        let laid = Laid::new(html, IB, 200.0);
        let css = sheets(IB);
        let styled = StyledDocument::new(&laid.doc, &css, html.len(), Limits::default()).unwrap();
        let ib = laid.geometry("b");
        assert!(styled.is_inline_block(ib));
        assert!(!styled.is_float(ib));
        let marked = laid.out.boxes.iter().filter(|g| styled.is_inline_block(g));
        assert_eq!(marked.count(), 1);
    }

    /// Placed in flow: a relative offset on its holder moves its box and
    /// lines once, with the line holding it.
    #[test]
    fn a_relative_offset_moves_it_once() {
        let laid = Laid::new(
            "<div style=position:relative;left:3.5px>a <span id=b>x</span></div>",
            IB,
            200.0,
        );
        assert_eq!(rect(laid.border("b")), (19.5, 0.0, 8.0, 20.0));
        assert_eq!(laid.placeholder("b").1, laid.border("b"));
        assert_eq!(laid.lines()[1], (0.0, 19.5, "x".into()));
        // Its own offset moves its box, its lines and its placeholder,
        // which is its border box as drawn.
        let laid = Laid::new(
            "<p>ab <span id=b>x</span> cd",
            "#b{display:inline-block;position:relative;top:50px;left:20px}",
            200.0,
        );
        assert_eq!(rect(laid.border("b")), (44.0, 50.0, 8.0, 20.0));
        assert_eq!(laid.placeholder("b").1, laid.border("b"));
        assert_eq!(laid.lines()[1], (50.0, 44.0, "x".into()));
        assert_eq!(laid.out.lines[0].fragments[1].x, 32.0);
    }
}
