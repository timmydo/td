//! Inline tables, laid as inline-blocks are (DESIGN.md §5,
//! "Inline-blocks"). Each scalar advances 8 px and a 16 px line is 20 px
//! tall with its baseline 15 px down, so widths, tops and baselines are
//! worked by hand.
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

    /// `#t` an inline table, `.r` its rows, `.c` its cells, `.p` a
    /// caption, with no spacing.
    const IT: &str = "#t{display:inline-table}.r{display:table-row}\
                      .c{display:table-cell}.p{display:table-caption}";

    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[
            (Origin::UserAgent, HTML_UA_CSS),
            (Origin::Author, "*{margin:0;padding:0;border-width:0}"),
            (Origin::Author, IT),
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
            rect(self.geometry(id).border)
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
        /// The placeholder of `id`: its line and its box.
        fn placeholder(&self, id: &str) -> (usize, (f64, f64, f64, f64)) {
            for (at, line) in self.out.lines.iter().enumerate() {
                if let Some(r) = line
                    .replaced
                    .iter()
                    .find(|r| r.atomic && self.id(r.node) == Some(id))
                {
                    return (at, (r.x, r.y, r.width, r.height));
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

    fn rect(r: Rect) -> (f64, f64, f64, f64) {
        (r.x, r.y, r.width, r.height)
    }

    /// An inline table of one cell holding `text`.
    fn one(text: &str) -> String {
        format!("<span id=t><span class=c id=c>{text}</span></span>")
    }

    /// Laid whole where its line puts it, shrink-to-fit as a table is, a
    /// placeholder its border box there, and its lines right after the
    /// line holding it.
    #[test]
    fn an_inline_table_is_laid_whole_in_its_line() {
        let laid = Laid::new(&format!("<p>ab {} gh", one("cd ef")), "", 200.0);
        assert!(laid.nested);
        assert_eq!(laid.border("t"), (24.0, 0.0, 40.0, 20.0));
        assert_eq!(laid.border("c"), (24.0, 0.0, 40.0, 20.0));
        assert_eq!(laid.placeholder("t"), (0, laid.border("t")));
        assert_eq!(
            laid.lines(),
            vec![(0.0, 0.0, "ab [t] gh".into()), (0.0, 24.0, "cd ef".into())]
        );
        assert_eq!(laid.holding(), (0.0, 20.0, 15.0));
        assert_eq!(laid.out.lines[0].width, 88.0);
        // An HTML table: its spacing and its cell's padding.
        let laid = Laid::new(
            "<p>ab <table id=t><tr><td id=c>cd</table> gh",
            "td{padding:1px}",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 22.0, 26.0));
        assert_eq!(laid.border("c"), (26.0, 2.0, 18.0, 22.0));
        assert_eq!(
            laid.lines(),
            vec![(0.0, 0.0, "ab [t] gh".into()), (3.0, 27.0, "cd".into())]
        );
        // Its cell middle-aligned, as HTML's sheet aligns it: the bottom
        // of the cell's content box, 23 px down, on the line's.
        assert_eq!(laid.holding(), (0.0, 28.0, 23.0));
    }

    /// Its width is its table's, set or shrink-to-fit, with its margins;
    /// its auto margins are 0 (CSS 2.2 §10.3.9), not centring it.
    #[test]
    fn its_width_is_its_tables_and_its_auto_margins_0() {
        let html = format!("<p>ab {} gh", one("cd ef"));
        let laid = Laid::new(&html, "#t{margin:0 auto}", 200.0);
        assert_eq!(laid.border("t"), (24.0, 0.0, 40.0, 20.0));
        assert_eq!(laid.geometry("t").margin.left, 0.0);
        assert_eq!(laid.geometry("t").margin.right, 0.0);
        assert_eq!(laid.lines()[0], (0.0, 0.0, "ab [t] gh".into()));
        let laid = Laid::new(&html, "#t{width:100px;margin:0 7px 0 5px}", 200.0);
        assert_eq!(laid.border("t"), (29.0, 0.0, 100.0, 20.0));
        assert_eq!(laid.out.lines[0].fragments[1].x, 136.0);
        assert_eq!(laid.out.lines[0].width, 160.0);
        // Padding and border count in its box, as a table's do.
        let laid = Laid::new(&html, "#t{padding:3px;border:2px solid}", 200.0);
        assert_eq!(laid.border("t"), (24.0, 0.0, 50.0, 30.0));
        assert_eq!(laid.border("c"), (29.0, 5.0, 40.0, 20.0));
        // Narrower than its content's widest line: as a table, never
        // under its least.
        let laid = Laid::new(&format!("<p>{}", one("cd ef")), "", 30.0);
        assert_eq!(laid.border("t"), (0.0, 0.0, 30.0, 40.0));
        let laid = Laid::new(&format!("<p>{}", one("cd ef")), "", 10.0);
        assert_eq!(laid.border("t"), (0.0, 0.0, 16.0, 40.0));
    }

    /// Its baseline is its first row's (CSS 2.2 §10.8.1, §17.4): that of
    /// the row's baseline-aligned cells, else the bottom content edge of
    /// its lowest cell (§17.5.3); with no row, its bottom margin edge; a
    /// caption's lines are not its rows'.
    #[test]
    fn its_baseline_is_its_first_rows() {
        let rows = "<span id=t><span class=r><span class=c>x</span></span>\
                    <span class=r><span class=c>y</span></span></span>";
        let laid = Laid::new(&format!("<p>ab {rows}"), "#t{padding-top:10px}", 200.0);
        assert_eq!(laid.border("t"), (24.0, 0.0, 8.0, 50.0));
        assert_eq!(laid.holding(), (0.0, 50.0, 25.0));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "ab [t]".into()),
                (10.0, 24.0, "x".into()),
                (30.0, 24.0, "y".into()),
            ]
        );
        // Its first cell laying no line, its baseline its top: the row's
        // is the lowest of its cells', the next one's, 20 px down.
        let laid = Laid::new(
            "<p>ab <span id=t><span class=c></span>\
             <span class=c style=padding-top:5px>x</span></span>",
            "",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 8.0, 25.0));
        assert_eq!(laid.holding(), (0.0, 25.0, 20.0));
        // Empty: its bottom margin edge, 30 px down with its margin.
        let laid = Laid::new(
            "<p>ab <span id=t></span>",
            "#t{width:20px;height:26px;margin-bottom:4px}",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 20.0, 26.0));
        assert_eq!(laid.holding(), (0.0, 35.0, 30.0));
        // Its first row empty, its second not: the first row's cell's
        // bottom content edge, its top, not the second row's line.
        let laid = Laid::new(
            "<p>ab <span id=t><span class=r><span class=c></span></span>\
             <span class=r><span class=c>y</span></span></span>",
            "",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 15.0, 8.0, 20.0));
        assert_eq!(laid.holding(), (0.0, 35.0, 15.0));
        // Its first row's cell holding only a block 30 px tall: no line,
        // its bottom content edge, not the second row's line.
        let laid = Laid::new(
            "<div>ab <span id=t><span class=r><span class=c>\
             <div style=height:30px></div></span></span>\
             <span class=r><span class=c>y</span></span></span> gh</div>",
            "",
            400.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 8.0, 50.0));
        assert_eq!(laid.holding(), (0.0, 50.0, 30.0));
        // A caption above: its row's line, 35 px down, not the caption's.
        let laid = Laid::new(
            "<p>ab <span id=t><span class=p>cap</span>\
             <span class=c>x</span></span>",
            "",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 20.0, 24.0, 20.0));
        assert_eq!(laid.holding(), (0.0, 40.0, 35.0));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "ab [t]".into()),
                (0.0, 24.0, "cap".into()),
                (20.0, 24.0, "x".into()),
            ]
        );
        // A caption below: the row's, 15 px down.
        let laid = Laid::new(
            "<p>ab <span id=t><span class=c>x</span>\
             <span class=p>cap</span></span>",
            "",
            200.0,
        );
        assert_eq!(laid.holding(), (0.0, 40.0, 15.0));
        // Two baseline cells, the second padded: the row's baseline, 21
        // px down, its first cell's line moved to it.
        let laid = Laid::new(
            "<p>ab <span id=t><span class=c id=c>x</span>\
             <span class=c style=vertical-align:baseline;padding-top:6px>y</span></span>",
            ".c{vertical-align:baseline}",
            200.0,
        );
        assert_eq!(laid.border("c"), (24.0, 0.0, 8.0, 26.0));
        assert_eq!(laid.holding(), (0.0, 26.0, 21.0));
        // A middle or bottom cell 100 px tall beside a baseline one: the
        // baseline one's, 15 px down, not the taller cell's line.
        for align in ["middle", "bottom"] {
            let laid = Laid::new(
                &format!(
                    "<div>ab <span id=t><span class=c \
                     style=vertical-align:{align};height:100px>x</span>\
                     <span class=c>y</span></span> gh</div>"
                ),
                "",
                400.0,
            );
            assert_eq!(laid.border("t"), (24.0, 0.0, 16.0, 100.0), "{align}");
            assert_eq!(laid.holding(), (0.0, 100.0, 15.0), "{align}");
        }
        // No baseline cell: the bottom of its lowest cell's content box,
        // 100 px down, not its middle line's nor its bottom margin edge.
        let laid = Laid::new(
            "<div>ab <span id=t><span class=c \
             style=vertical-align:middle;height:100px>x</span></span> gh</div>",
            "#t{padding-bottom:4px}",
            400.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 8.0, 104.0));
        assert_eq!(laid.holding(), (0.0, 105.0, 100.0));
        // An HTML table's middle cells: the taller's content bottom, 60
        // px down, not its first line's.
        let laid = Laid::new(
            "<div>ab <table id=t style=border-spacing:0><tr><td>cd<td>e<br>f<br>g\
             </table> gh</div>",
            "",
            400.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 24.0, 60.0));
        assert_eq!(laid.holding(), (0.0, 65.0, 60.0));
        // Its own relative offset moves it, not its baseline.
        let laid = Laid::new(
            &format!("<p>ab {}", one("x")),
            "#t{position:relative;top:50px}",
            200.0,
        );
        assert_eq!(laid.holding(), (0.0, 20.0, 15.0));
        assert_eq!(laid.border("t"), (24.0, 50.0, 8.0, 20.0));
        assert_eq!(laid.placeholder("t").1, laid.border("t"));
        assert_eq!(laid.lines()[1], (50.0, 24.0, "x".into()));
    }

    /// It moves whole to the next line where it does not fit, stands alone
    /// on one where it is wider, and stays on one that does not wrap.
    #[test]
    fn it_wraps_whole_where_its_parent_wraps() {
        let laid = Laid::new(&format!("<p>aaaa bbbb {} ee", one("cc dd")), "", 100.0);
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa bbbb".into()),
                (20.0, 0.0, "[t] ee".into()),
                (20.0, 0.0, "cc dd".into()),
            ]
        );
        assert_eq!(laid.border("t"), (0.0, 20.0, 40.0, 20.0));
        let laid = Laid::new(&format!("<p>aa {} ee", one("cc")), "#t{width:150px}", 100.0);
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aa".into()),
                (20.0, 0.0, "[t]".into()),
                (20.0, 0.0, "cc".into()),
                (40.0, 0.0, "ee".into()),
            ]
        );
        assert_eq!(laid.border("t"), (0.0, 20.0, 150.0, 20.0));
        let laid = Laid::new(
            &format!("<p style=white-space:nowrap>aaaa bbbb {} ee", one("cc dd")),
            "",
            100.0,
        );
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa bbbb [t] ee".into()),
                (0.0, 80.0, "cc dd".into()),
            ]
        );
        assert_eq!(laid.out.lines[0].width, 144.0);
    }

    /// text-align moves its line, and it and its lines with it.
    #[test]
    fn text_align_moves_it_with_its_line() {
        for (align, shift) in [("center", 30.0), ("right", 60.0)] {
            let laid = Laid::new(
                &format!("<p style=text-align:{align}>ab {}", one("cd")),
                "",
                100.0,
            );
            assert_eq!(laid.border("t"), (shift + 24.0, 0.0, 16.0, 20.0));
            assert_eq!(laid.border("c"), (shift + 24.0, 0.0, 16.0, 20.0));
            assert_eq!(laid.lines()[1], (0.0, shift + 24.0, "cd".into()));
            assert_eq!(laid.placeholder("t").1, laid.border("t"));
        }
    }

    /// In a table cell: measured whole, its table's widths; the cell's
    /// baseline is the line holding it.
    #[test]
    fn in_a_cell_it_is_measured_and_aligns_the_row() {
        let css = "table{border-spacing:0}";
        let html = format!("<table><tr><td id=d>ab {}</table>", one("cd ef"));
        let laid = Laid::new(&html, css, 200.0);
        assert_eq!(laid.border("d").2, 64.0);
        let html = format!(
            "<table style=width:1px><tr><td id=d>ab {}</table>",
            one("cd ef")
        );
        let laid = Laid::new(&html, css, 200.0);
        assert_eq!(laid.border("d").2, 16.0);
        assert_eq!(laid.border("t"), (0.0, 20.0, 16.0, 40.0));
        // Baseline-aligned beside a cell holding a two-row inline table:
        // its baseline is the line holding it, 15 px down.
        let laid = Laid::new(
            "<table><tr><td>x<td id=d><span id=t><span class=r><span class=c>cd</span>\
             </span><span class=r><span class=c>ef</span></span></span> gh</table>",
            "td{vertical-align:baseline}table{border-spacing:0}",
            200.0,
        );
        let x = laid
            .lines()
            .into_iter()
            .find(|(_, _, text)| text == "x")
            .unwrap();
        assert_eq!(x.0, 0.0);
        assert_eq!(laid.border("t"), (8.0, 0.0, 16.0, 40.0));
        assert_eq!(laid.border("d").3, 40.0);
    }

    /// A float's shrink-to-fit width counts it whole, its table's least
    /// and most and its margins.
    #[test]
    fn measured_in_a_float() {
        let css = "#f{float:left}";
        let html = format!("<div id=f>ab {}</div>", one("cd ef"));
        assert_eq!(Laid::new(&html, css, 200.0).border("f").2, 64.0);
        assert_eq!(Laid::new(&html, css, 30.0).border("f").2, 30.0);
        assert_eq!(Laid::new(&html, css, 10.0).border("f").2, 16.0);
        let css = "#f{float:left}#t{margin-left:4px;border:1px solid}";
        assert_eq!(Laid::new(&html, css, 200.0).border("f").2, 70.0);
        assert_eq!(Laid::new(&html, css, 10.0).border("f").2, 22.0);
        // Its parent not wrapping: its least joins the word around it.
        let html = format!(
            "<div id=f style=white-space:nowrap>ab {} gh</div>",
            one("cd ef")
        );
        let css = "#f{float:left}.c{white-space:normal}";
        assert_eq!(Laid::new(&html, css, 200.0).border("f").2, 88.0);
        assert_eq!(Laid::new(&html, css, 10.0).border("f").2, 64.0);
        // A set width, as a table measures one.
        let html = format!("<div id=f>ab {}</div>", one("cd ef"));
        let css = "#f{float:left}#t{width:100px}";
        assert_eq!(Laid::new(&html, css, 400.0).border("f").2, 124.0);
        // Its cells side by side with its spacing: 3 + 16 + 3 + 16 + 3.
        let html = "<div id=f>x <span id=t><span class=c>ab</span>\
                    <span class=c>cd</span></span></div>";
        let css = "#f{float:left}#t{border-spacing:3px}";
        assert_eq!(Laid::new(html, css, 400.0).border("f").2, 57.0);
        assert_eq!(Laid::new(html, css, 10.0).border("f").2, 41.0);
        // A fixed table narrower than its columns is theirs, 40 px, as a
        // table measures it and not a block of its set width.
        let css = "#f{float:left}#t{table-layout:fixed;width:10px}.c{width:20px}";
        assert_eq!(Laid::new(html, css, 400.0).border("f").2, 56.0);
        assert_eq!(Laid::new(html, css, 400.0).border("t").2, 40.0);
    }

    /// A windowed layout equals the filtered whole one, with inline tables
    /// above, below and across the window, some with rows, captions,
    /// list markers and floats in them.
    #[test]
    fn windowed_layouts_equal_filtered_whole_ones() {
        let mut html = String::new();
        for n in 0..40 {
            html.push_str(&format!(
                "<p>word{n} word <span class=t style=height:{h}px><span class=r>\
                 <span class=c>in it<br>and on</span><span class=c>two</span></span>\
                 <span class=r><span class=c>row</span></span></span> \
                 more words here to wrap <span class=t><span class=c>x</span></span> ok",
                h = 10 + (n * 7) % 90
            ));
            if n % 6 == 0 {
                html.push_str(
                    "<p>a list <span class=t><span class=p>cap</span><span class=c>\
                     <ol><li>in<li>side</ol>its words</span></span> after",
                );
            }
            if n % 5 == 0 {
                html.push_str(
                    "<p>with <span class=t><span class=c><span class=f>float</span>\
                     beside it</span></span> and <span class=t><span class=c>nested \
                     <span class=t><span class=c>in<br>it</span></span> too</span></span> end",
                );
            }
        }
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(
            ".t{display:inline-table;padding:2px;border-spacing:1px}\
             .f{float:right;width:30px}li{list-style-position:outside}",
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
                >= 100
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

    /// A floated inline table is a table td-css blockifies, which keeps
    /// its place in flow as every table does (§5, "Floats"): laid as a
    /// block-level table, as before.
    #[test]
    fn a_floated_one_is_a_block_level_table() {
        let html = format!("<p>ab {} gh", one("cd"));
        let floated = Laid::new(&html, "#t{float:left}", 200.0);
        let table = Laid::new(&html, "#t{display:table}", 200.0);
        assert!(!floated.nested);
        assert_eq!(floated.out.boxes, table.out.boxes);
        assert_eq!(floated.out.lines, table.out.lines);
        assert_eq!(
            floated.lines(),
            vec![
                (0.0, 0.0, "ab".into()),
                (20.0, 0.0, "cd".into()),
                (40.0, 0.0, "gh".into()),
            ]
        );
    }

    /// What is one: a block whose display is inline-table, not the root;
    /// a page with none lays its tables as before, with no box laid apart.
    #[test]
    fn what_is_an_inline_table() {
        let styled_of = |html: &str, css: &str| {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            (styled.has_inline_blocks(), styled.has_tables())
        };
        assert_eq!(styled_of("<p>a<table><tr><td>b</table>", ""), (false, true));
        assert_eq!(styled_of(&format!("<p>a{}", one("b")), ""), (true, true));
        assert_eq!(
            styled_of("<p>a", "html{display:inline-table}"),
            (false, true)
        );
        let html = format!("<p>ab {}", one("cd"));
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
        assert!(!styled.is_float(laid.geometry("t")));
        // A page with none: its table is block-level, no line holds a
        // placeholder.
        let laid = Laid::new("<p>ab <table id=k><tr><td>cd</table> ef", "", 200.0);
        assert!(!laid.nested);
        assert_eq!(laid.border("k").1, 20.0);
        assert!(laid.out.lines.iter().all(|l| l.replaced.is_empty()));
    }
}
