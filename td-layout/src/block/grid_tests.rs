//! Grid layout (DESIGN.md §5, "Grid"). Each scalar advances 8 px and a
//! 16 px line is 20 px tall with its baseline 15 px down, as flex's
//! tests have it, so sizes are worked by hand.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::*;
    use crate::inline::FontLine;
    use crate::Limits;
    use std::collections::BTreeMap;
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
            Self::with(html, css, &Request::new(width, 600.0), Limits::default()).unwrap()
        }
        fn with(html: &str, css: &str, request: &Request, limits: Limits) -> Result<Self, Error> {
            let doc = td_html::document::parse(html).unwrap();
            let out = {
                let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits)?;
                layout(&styled, &mut Metrics, request)?
            };
            Ok(Self { doc, out })
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
        /// Each line's text, and its top, height, width and first x, by
        /// text, which the tests keep unique.
        fn lines(&self) -> BTreeMap<String, (f64, f64, f64, f64)> {
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
                    (text, (line.y, line.height, line.width, x))
                })
                .collect()
        }
        fn texts(&self) -> Vec<String> {
            self.out
                .lines
                .iter()
                .map(|line| {
                    line.fragments
                        .iter()
                        .map(|f| {
                            let node = self.doc.node(f.node).and_then(|n| n.text()).unwrap();
                            node[f.range.clone()].to_string()
                        })
                        .collect::<String>()
                })
                .collect()
        }
    }

    fn rect(r: Rect) -> (f64, f64, f64, f64) {
        (r.x, r.y, r.width, r.height)
    }

    /// Fixed columns, items auto-placed in rows, each as tall as its row.
    #[test]
    fn fixed_columns_place_items_in_rows() {
        let laid = Laid::new(
            "<div id=g><p id=a>ab<p id=b>cd ef<p id=c>gh<p id=d>ij</div><p id=after>x",
            "#g{display:grid;grid-template-columns:40px 64px;column-gap:4px;row-gap:6px}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 40.0, 20.0));
        assert_eq!(laid.border("b"), (44.0, 0.0, 64.0, 20.0));
        assert_eq!(laid.border("c"), (0.0, 26.0, 40.0, 20.0));
        assert_eq!(laid.border("d"), (44.0, 26.0, 64.0, 20.0));
        assert_eq!(laid.border("g"), (0.0, 0.0, 400.0, 46.0));
        assert_eq!(laid.border("after").1, 46.0);
    }

    /// `fr` columns share what fixed ones leave; `auto` takes its
    /// content's max-content width before them; a span spreads.
    #[test]
    fn fr_auto_and_spans_size_columns() {
        let laid = Laid::new(
            "<div id=g><p id=a>abcd<p id=b>x<p id=c>y<p id=d style='grid-column:span 2'>zz</div>",
            "#g{display:grid;grid-template-columns:auto 1fr 2fr;column-gap:10px}",
            400.0,
        );
        // auto 32, then 400 - 32 - 20 = 348 over 3fr: 116, 232.
        assert_eq!(laid.border("a"), (0.0, 0.0, 32.0, 20.0));
        assert_eq!(laid.border("b"), (42.0, 0.0, 116.0, 20.0));
        assert_eq!(laid.border("c"), (168.0, 0.0, 232.0, 20.0));
        assert_eq!(laid.border("d"), (0.0, 20.0, 158.0, 20.0));
    }

    /// Explicit lines, negative lines, names and spans place items;
    /// later items fill the holes they leave, in row order.
    #[test]
    fn lines_names_and_spans_place_items() {
        let laid = Laid::new(
            "<div id=g><p id=a style='grid-row:2;grid-column:-2'>a\
             <p id=b style='grid-column:main-start/main-end'>b\
             <p id=c>c<p id=d style='grid-row:1/span 2;grid-column:1'>d<p id=e>e</div>",
            "#g{display:grid;grid-template-columns:[left] 10px [main-start] 20px 30px [main-end] 40px;\
             grid-auto-rows:20px}",
            400.0,
        );
        assert_eq!(laid.border("a"), (60.0, 20.0, 40.0, 20.0));
        assert_eq!(laid.border("d"), (0.0, 0.0, 10.0, 40.0));
        assert_eq!(laid.border("b"), (10.0, 0.0, 50.0, 20.0));
        assert_eq!(laid.border("c"), (60.0, 0.0, 40.0, 20.0));
        assert_eq!(laid.border("e"), (10.0, 20.0, 20.0, 20.0));
    }

    /// Areas name lines; an item placed in one takes its rectangle.
    #[test]
    fn areas_place_items() {
        let laid = Laid::new(
            "<div id=g><p id=f style=grid-area:foot>f<p id=h style=grid-area:head>h\
             <p id=s style=grid-area:side>s<p id=m style=grid-area:main>m</div>",
            "#g{display:grid;grid-template-areas:'head head' 'side main' 'foot foot';\
             grid-template-columns:100px 1fr;grid-template-rows:30px auto 10px}",
            400.0,
        );
        assert_eq!(laid.border("h"), (0.0, 0.0, 400.0, 30.0));
        assert_eq!(laid.border("s"), (0.0, 30.0, 100.0, 20.0));
        assert_eq!(laid.border("m"), (100.0, 30.0, 300.0, 20.0));
        assert_eq!(laid.border("f"), (0.0, 50.0, 400.0, 10.0));
        // Source order is kept for lines.
        assert_eq!(laid.texts(), ["f", "h", "s", "m"]);
    }

    /// `auto-fill` repeats as many tracks as fit; `auto-fit` collapses
    /// the empty ones, so `fr` tracks share the whole width.
    #[test]
    fn auto_fill_and_auto_fit() {
        let html = "<div id=g><p id=a>a<p id=b>b<p id=c>c</div>";
        let fill = Laid::new(
            html,
            "#g{display:grid;grid-template-columns:repeat(auto-fill,minmax(100px,1fr));\
             column-gap:10px}",
            450.0,
        );
        // 4 tracks fit 450: 4 * 100 + 3 * 10 = 430; each 105.
        assert_eq!(fill.border("a"), (0.0, 0.0, 105.0, 20.0));
        assert_eq!(fill.border("c"), (230.0, 0.0, 105.0, 20.0));
        let fit = Laid::new(
            html,
            "#g{display:grid;grid-template-columns:repeat(auto-fit,minmax(100px,1fr));\
             column-gap:10px}",
            450.0,
        );
        // 3 used of 4: (450 - 20) / 3 each.
        assert_eq!(fit.border("a").2, (430.0 * 64.0 / 3.0f64).floor() / 64.0);
        assert_eq!(fit.border("c").0 + fit.border("c").2, 450.0);
    }

    /// A column flow fills columns first.
    #[test]
    fn column_flow() {
        let laid = Laid::new(
            "<div id=g><p id=a>a<p id=b>b<p id=c>c</div>",
            "#g{display:grid;grid-auto-flow:column;grid-template-rows:20px 20px;\
             grid-auto-columns:50px}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 50.0, 20.0));
        assert_eq!(laid.border("b"), (0.0, 20.0, 50.0, 20.0));
        assert_eq!(laid.border("c"), (50.0, 0.0, 50.0, 20.0));
    }

    /// Items stretch, start, centre and end in their areas; out of order
    /// placement moves later rows' items down to their rows.
    #[test]
    fn alignment_and_out_of_order_rows() {
        let laid = Laid::new(
            "<div id=g><p id=low style=grid-row:2>low<p id=tall style=grid-row:1>t<br>a<br>l\
             <p id=mid style='grid-row:1;align-self:center'>m\
             <p id=end style='grid-row:1;align-self:end;justify-self:end'>e</div>",
            "#g{display:grid;grid-template-columns:50px 50px 50px}",
            400.0,
        );
        assert_eq!(laid.border("tall"), (0.0, 0.0, 50.0, 60.0));
        assert_eq!(laid.border("mid"), (50.0, 20.0, 50.0, 20.0));
        assert_eq!(laid.border("end"), (142.0, 40.0, 8.0, 20.0));
        assert_eq!(laid.border("low"), (0.0, 60.0, 50.0, 20.0));
        assert_eq!(laid.lines()["low"].0, 60.0);
        assert_eq!(laid.border("g").3, 80.0);
    }

    /// `1fr` rows at an auto height are as tall as the tallest item
    /// needs; at a definite height they share it.
    #[test]
    fn flexible_rows() {
        let html = "<div id=g><p id=a>a<p id=b>b<br>c<br>d</div>";
        let auto = Laid::new(html, "#g{display:grid;grid-auto-rows:1fr}", 400.0);
        assert_eq!(auto.border("a"), (0.0, 0.0, 400.0, 60.0));
        assert_eq!(auto.border("b"), (0.0, 60.0, 400.0, 60.0));
        let tall = Laid::new(
            html,
            "#g{display:grid;grid-auto-rows:1fr;height:300px}",
            400.0,
        );
        assert_eq!(tall.border("a"), (0.0, 0.0, 400.0, 150.0));
        assert_eq!(tall.border("b"), (0.0, 150.0, 400.0, 150.0));
        let fixed = Laid::new(
            html,
            "#g{display:grid;grid-template-rows:minmax(0,1fr) 3fr;height:200px}",
            400.0,
        );
        assert_eq!(fixed.border("a"), (0.0, 0.0, 400.0, 50.0));
        assert_eq!(fixed.border("b"), (0.0, 50.0, 400.0, 150.0));
    }

    /// A grid floated is measured by its columns.
    #[test]
    fn measured_grids() {
        let html = "<div id=f><div id=g><p>abc<p>de fgh</div></div>";
        let css = "#f{float:left}#g{display:grid;grid-template-columns:auto auto;column-gap:4px}";
        // max-content: 24 + 4 + 48; min-content: 24 + 4 + 24.
        assert_eq!(Laid::new(html, css, 400.0).border("f").2, 76.0);
        assert_eq!(Laid::new(html, css, 50.0).border("f").2, 52.0);
    }

    fn styled_layout(html: &str, css: &str, request: &Request<'_>) -> Layout {
        let doc = td_html::document::parse(html).unwrap();
        let styled =
            StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
        layout(&styled, &mut Metrics, request).unwrap()
    }

    /// Each box's node, content and border rects, and each line's top,
    /// height, width and fragments: what two layouts of one tree share.
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

    /// Fixed columns, items auto-placed and stretched, lay as a fixed
    /// table's top-aligned cells: the same cell boxes and lines.
    #[test]
    fn fixed_columns_lay_as_a_fixed_tables_cells() {
        let cells: &[(u32, &str)] = &[
            (50, "ab cd ef gh"),
            (80, "one<br>two three four"),
            (30, "x"),
            (64, "<p>para</p><p>graphs here</p>"),
            (50, "second row"),
            (80, "<ul><li>item</ul>"),
            (30, "y z"),
            (64, "last"),
        ];
        let widths = cells[..4]
            .iter()
            .map(|c| format!("{}px", c.0))
            .collect::<Vec<_>>();
        let total: u32 = cells[..4].iter().map(|c| c.0).sum();
        let mut grid = String::from("<div id=g>");
        let mut table = format!("<table style=width:{total}px><tr>");
        for (at, (width, text)) in cells.iter().enumerate() {
            grid.push_str(&format!("<div id=c{at}>{text}</div>"));
            if at == 4 {
                table.push_str("<tr>");
            }
            table.push_str(&format!("<td id=c{at} style=width:{width}px>{text}</td>"));
        }
        grid.push_str("</div>");
        table.push_str("</table>");
        let request = Request::new(400.0, 600.0);
        let css = format!(
            "#g{{display:grid;grid-template-columns:{}}}",
            widths.join(" ")
        );
        let a = styled_layout(&grid, &css, &request);
        let b = styled_layout(
            &table,
            "table{table-layout:fixed;border-spacing:0}td{vertical-align:top}",
            &request,
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
                .filter_map(|g| Some((id(g)?, g.border, g.content)))
                .filter(|(id, ..)| id.starts_with('c'))
                .collect::<Vec<_>>();
            cells.sort_by(|x, y| x.0.cmp(&y.0));
            cells
        };
        assert_eq!(rects(&a, &grid), rects(&b, &table));
        let lines = |out: &Layout| {
            out.lines
                .iter()
                .map(|l| (l.y, l.height, l.width, l.fragments.first().map(|f| f.x)))
                .collect::<Vec<_>>()
        };
        assert_eq!(lines(&a), lines(&b));
        assert_eq!(a.height, b.height);
        assert_eq!(a.markers.len(), b.markers.len());
    }

    /// `repeat(n, 1fr)` in one row lays as a flex row of `flex: 1 1 0px`
    /// items with the same gap: equal shares by cumulative floors, a
    /// track holding its content's minimum.
    #[test]
    fn equal_fractions_lay_as_a_flex_row() {
        for (n, width, gap) in [(3, 400.0, 0), (4, 333.0, 7), (7, 1000.0, 3), (3, 100.0, 5)] {
            let mut inner = String::new();
            for at in 0..n {
                inner.push_str(&format!(
                    "<div id=i{at}>item {at} {}<p>more</div>",
                    "w ".repeat(at)
                ));
            }
            let html = format!("<div id=c>{inner}</div><p>after");
            let request = Request::new(width, 600.0);
            let grid = styled_layout(
                &html,
                &format!(
                    "#c{{display:grid;grid-template-columns:repeat({n},1fr);column-gap:{gap}px}}"
                ),
                &request,
            );
            let flex = styled_layout(
                &html,
                &format!("#c{{display:flex;column-gap:{gap}px}}#c>*{{flex:1 1 0px}}"),
                &request,
            );
            assert_eq!(drawn(&grid), drawn(&flex), "{n} {width} {gap}");
            assert_eq!(grid.height, flex.height);
        }
    }

    /// One column of full-width items in auto rows lays as block flow,
    /// the gaps as margins between them, each item a context's root.
    #[test]
    fn one_column_lays_as_block_flow() {
        let items = [
            "<p>some words here to wrap around the line and more of them",
            "<ul><li>an item<li>another <b>bold</b> one</ul>",
            "<div style=float:left;width:30px>fl</div><p>beside the float, words",
            "<table><tr><td>a cell<td>another</table>",
            "text alone <i>inline</i>",
        ];
        for width in [120.0, 333.0, 800.0] {
            let mut grid = String::from("<div id=c>");
            let mut block = String::from("<div id=c>");
            for (at, inner) in items.iter().enumerate() {
                grid.push_str(&format!("<div id=i{at}>{inner}</div>"));
                let margin = if at + 1 < items.len() { 6 } else { 0 };
                block.push_str(&format!(
                    "<div id=i{at} style=margin-bottom:{margin}px>{inner}</div>"
                ));
            }
            grid.push_str("</div><p>after");
            block.push_str("</div><p>after");
            let request = Request::new(width, 600.0);
            let a = styled_layout(&grid, "#c{display:grid;row-gap:6px;padding:1px}", &request);
            let b = styled_layout(&block, "#c{padding:1px}#c>div{overflow:hidden}", &request);
            assert_eq!(drawn(&a), drawn(&b), "{width}");
            assert_eq!(a.height, b.height);
            assert_eq!(a.overflow, b.overflow);
            assert_eq!(a.markers.len(), b.markers.len());
        }
    }

    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n.max(1)
        }
        fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
            of[self.below(of.len() as u64) as usize]
        }
    }

    /// Sparse auto-placement of `spans` (rows, columns) in `cols`
    /// columns, row by row (§8.5): each item's row and column.
    fn reference_placement(cols: usize, spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let mut taken = std::collections::HashSet::new();
        let (mut row, mut col) = (0, 0);
        let mut out = Vec::new();
        for &(rs, cs) in spans {
            loop {
                if col + cs > cols {
                    row += 1;
                    col = 0;
                    continue;
                }
                let free =
                    (row..row + rs).all(|r| (col..col + cs).all(|c| !taken.contains(&(r, c))));
                if free {
                    break;
                }
                col += 1;
            }
            for r in row..row + rs {
                for c in col..col + cs {
                    taken.insert((r, c));
                }
            }
            out.push((row, col));
            col += cs;
        }
        out
    }

    /// A random grid: its container's rule, and its items' inner HTML,
    /// spans and own styles.
    type Items = Vec<(String, (usize, usize), String)>;

    fn random_grid(r: &mut Rng) -> (String, usize, Items) {
        let cols = 1 + r.below(4) as usize;
        let sizes = (0..cols)
            .map(|_| {
                r.pick(&[
                    "40px",
                    "1fr",
                    "auto",
                    "minmax(30px,1fr)",
                    "2fr",
                    "max-content",
                    "fit-content(60px)",
                ])
                .to_string()
            })
            .collect::<Vec<_>>();
        let rows = r.pick(&[
            "auto",
            "30px",
            "minmax(25px,auto)",
            "auto 40px",
            "min-content",
        ]);
        let gaps = r.pick(&["", "gap:3px;", "column-gap:5px;row-gap:2px;"]);
        let align = r.pick(&[
            "",
            "align-items:start;",
            "justify-items:center;",
            "align-items:end;",
        ]);
        let css = format!(
            "display:grid;grid-template-columns:{};grid-auto-rows:{rows};{gaps}{align}",
            sizes.join(" ")
        );
        let n = 1 + r.below(12) as usize;
        let items = (0..n)
            .map(|at| {
                let cs = 1 + r.below(cols.min(3) as u64) as usize;
                let rs = if r.below(4) == 0 { 2 } else { 1 };
                let words = 1 + r.below(6) as usize;
                let text = (0..words)
                    .map(|w| format!("t{at}w{w}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let inner = match r.below(5) {
                    0 => format!("<p>{text}</p><p>p{at}</p>"),
                    1 => format!("{text}<br>b{at}"),
                    2 => format!("<ul><li>{text}</ul>"),
                    _ => text,
                };
                let own = r.pick(&[
                    "",
                    "",
                    "align-self:center;",
                    "align-self:end;",
                    "justify-self:start;",
                    "height:33px;",
                    "padding:2px;",
                    "margin:1px 3px;",
                    "min-height:45px;",
                ]);
                (inner, (rs, cs), own.to_string())
            })
            .collect();
        (css, cols, items)
    }

    /// Each id'd box's border and content rects, by id, and each line by
    /// its text: layouts equal by geometry, whatever their tree order.
    fn by_node(html: &str, out: &Layout) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let doc = td_html::document::parse(html).unwrap();
        let boxes = out
            .boxes
            .iter()
            .filter_map(|g| {
                let id = doc.node(g.node)?.element()?.attribute("id")?.to_string();
                Some((id, format!("{:?} {:?}", g.border, g.content)))
            })
            .collect();
        let lines = out
            .lines
            .iter()
            .map(|l| {
                // Keyed by its first piece's whole text node and offset,
                // which the items keep unique.
                let key = l.fragments.first().map_or(String::new(), |f| {
                    let text = doc.node(f.node).and_then(|n| n.text()).unwrap();
                    format!("{text}@{}", f.range.start)
                });
                let xs = l
                    .fragments
                    .iter()
                    .map(|f| f.x.to_string())
                    .collect::<Vec<_>>();
                (key, format!("{} {} {} {:?}", l.y, l.height, l.width, xs))
            })
            .collect();
        (boxes, lines)
    }

    /// The most telling oracle: a grid auto-placed from row-major source
    /// equals the same grid placed by explicit lines (from an independent
    /// sparse placement), in that order and shuffled: boxes and lines by
    /// node, whatever order they were laid in.
    #[test]
    fn placement_is_independent_of_source_order() {
        let mut r = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..300 {
            let (css, cols, items) = random_grid(&mut r);
            let spans: Vec<_> = items.iter().map(|i| i.1).collect();
            let placed = reference_placement(cols, &spans);
            let mut order: Vec<usize> = (0..items.len()).collect();
            for at in (1..order.len()).rev() {
                order.swap(at, r.below(at as u64 + 1) as usize);
            }
            let page = |which: &[usize], explicit: bool| {
                let mut html = String::from("<div id=g>");
                for &k in which {
                    let (inner, (rs, cs), own) = &items[k];
                    let (row, col) = placed[k];
                    let place = if explicit {
                        format!(
                            "grid-row:{}/span {rs};grid-column:{}/span {cs};",
                            row + 1,
                            col + 1
                        )
                    } else {
                        format!("grid-row:span {rs};grid-column:span {cs};")
                    };
                    html.push_str(&format!("<div id=i{k} style='{place}{own}'>{inner}</div>"));
                }
                html.push_str("</div><p id=after>after");
                html
            };
            let rows: Vec<usize> = (0..items.len()).collect();
            let auto = page(&rows, false);
            let explicit = page(&rows, true);
            let shuffled = page(&order, true);
            for width in [400.0, 150.0] {
                let request = Request::new(width, 600.0);
                let sheet = format!("#g{{{css}}}");
                let a = styled_layout(&auto, &sheet, &request);
                let b = styled_layout(&explicit, &sheet, &request);
                let c = styled_layout(&shuffled, &sheet, &request);
                let context = format!("{case} {width} {css} {shuffled}");
                assert_eq!(drawn(&a), drawn(&b), "{context}");
                assert_eq!(by_node(&auto, &a), by_node(&shuffled, &c), "{context}");
                assert_eq!(a.height, c.height, "{context}");
                assert_eq!(a.overflow, c.overflow, "{context}");
            }
        }
    }

    /// Areas lay as the same placement written as numeric lines; a named
    /// line places as its number.
    #[test]
    fn areas_equal_their_lines() {
        let html = |places: [&str; 4]| {
            format!(
                "<div id=g><p id=f style='{}'>foot words<p id=h style='{}'>head\
                 <p id=s style='{}'>side words here<p id=m style='{}'>main<br>text</div>",
                places[0], places[1], places[2], places[3]
            )
        };
        let areas = html([
            "grid-area:foot",
            "grid-area:head",
            "grid-area:side",
            "grid-area:main",
        ]);
        let lines = html([
            "grid-area:3/1/4/3",
            "grid-row:1;grid-column:1/-1",
            "grid-row:2;grid-column:1",
            "grid-area:2/2",
        ]);
        let named = html([
            "grid-row:foot;grid-column:a/c",
            "grid-row:r1;grid-column:a/span 2",
            "grid-row:2;grid-column:a",
            "grid-row:2/r3;grid-column:b",
        ]);
        let shape = "grid-template-columns:auto 1fr;grid-template-rows:auto auto 10px";
        let request = Request::new(300.0, 600.0);
        let a = styled_layout(
            &areas,
            &format!("#g{{display:grid;grid-template-areas:'head head' 'side main' 'foot foot';{shape}}}"),
            &request,
        );
        let b = styled_layout(&lines, &format!("#g{{display:grid;{shape}}}"), &request);
        let c = styled_layout(
            &named,
            "#g{display:grid;grid-template-columns:[a] auto [b] 1fr [c];\
             grid-template-rows:[r1] auto auto [r3 foot-start] 10px [foot-end]}",
            &request,
        );
        assert_eq!(drawn(&a), drawn(&b));
        assert_eq!(drawn(&a), drawn(&c));
    }

    /// `auto-fill` at a width lays as the repeat it comes to.
    #[test]
    fn auto_fill_equals_its_count() {
        for (width, track, gap) in [
            (450.0, "minmax(100px,1fr)", 10),
            (1000.0, "120px", 0),
            (90.0, "minmax(100px,1fr)", 4),
        ] {
            let content = (0..7).map(|k| format!("<p>card {k}")).collect::<String>();
            let html = format!("<div id=g>{content}</div>");
            let fill = styled_layout(
                &html,
                &format!("#g{{display:grid;grid-template-columns:repeat(auto-fill,{track});gap:{gap}px}}"),
                &Request::new(width, 600.0),
            );
            let size = if track == "120px" { 120.0 } else { 100.0 };
            let n = (((width + gap as f64) / (size + gap as f64)).floor() as usize).max(1);
            let fixed = styled_layout(
                &html,
                &format!(
                    "#g{{display:grid;grid-template-columns:repeat({n},{track});gap:{gap}px}}"
                ),
                &Request::new(width, 600.0),
            );
            assert_eq!(drawn(&fill), drawn(&fixed), "{width} {track}");
        }
    }

    /// Seeded columns against a reference §12.7.1 in floating point: in
    /// order a gap apart, each at least its minimum, flexible ones within
    /// 1/64 px of their exact share, those held at their minimum the
    /// minimum, and all summing exactly to the room they fill.
    #[test]
    fn columns_meet_the_spec_invariants() {
        let mut r = Rng(0x2545_f491_4f6c_dd1d);
        for case in 0..600 {
            let n = 1 + r.below(6) as usize;
            // (minimum px, factor) per track; factor 0 is a fixed track.
            let mut tracks = Vec::new();
            for _ in 0..n {
                tracks.push(match r.below(4) {
                    0 => ((5 + r.below(60)) as f64, 0.0),
                    1 => (0.0, (1 + r.below(30)) as f64 / 8.0),
                    2 => (r.below(90) as f64, (1 + r.below(8)) as f64),
                    _ => (r.below(20) as f64, 0.0),
                });
            }
            let css = tracks
                .iter()
                .map(|&(min, f)| {
                    if f == 0.0 {
                        format!("{min}px")
                    } else {
                        format!("minmax({min}px,{f}fr)")
                    }
                })
                .collect::<Vec<_>>();
            let width = 50.0 + r.below(900) as f64 + r.below(64) as f64 / 64.0;
            let gap = r.below(9) as f64;
            let html = (0..n).map(|k| format!("<p id=i{k}>")).collect::<String>();
            let laid = Laid::new(
                &format!("<div id=g>{html}</div>"),
                &format!(
                    "#g{{display:grid;grid-template-columns:{};column-gap:{gap}px}}",
                    css.join(" ")
                ),
                width,
            );
            let spans: Vec<_> = (0..n).map(|k| laid.border(&format!("i{k}"))).collect();
            let room = (width * 64.0).floor() / 64.0;
            let gaps = gap * (n - 1) as f64;
            // The reference: flexible tracks whose minimum exceeds their
            // share are inflexible, until none does.
            let mut flexible: Vec<bool> = tracks.iter().map(|t| t.1 > 0.0).collect();
            let mut fr: f64;
            loop {
                let fixed: f64 = (0..n).filter(|&k| !flexible[k]).map(|k| tracks[k].0).sum();
                let leftover = room - gaps - fixed;
                let factors: f64 = (0..n).filter(|&k| flexible[k]).map(|k| tracks[k].1).sum();
                if leftover <= 0.0 || factors == 0.0 {
                    // No space: every track at its minimum.
                    fr = 0.0;
                    flexible.fill(false);
                    break;
                }
                fr = leftover / factors.max(1.0);
                let held: Vec<usize> = (0..n)
                    .filter(|&k| flexible[k] && tracks[k].0 > fr * tracks[k].1)
                    .collect();
                if held.is_empty() {
                    break;
                }
                for k in held {
                    flexible[k] = false;
                }
            }
            let context = format!("{case} {css:?} {width} {gap}");
            for k in 0..n {
                if k > 0 {
                    assert_eq!(
                        spans[k - 1].0 + spans[k - 1].2 + gap,
                        spans[k].0,
                        "{context}"
                    );
                }
                let size = spans[k].2;
                assert!(size >= tracks[k].0, "{context}");
                if flexible[k] {
                    let exact = fr * tracks[k].1;
                    assert!(
                        ((size - exact) * 64.0).abs() <= 1.0,
                        "{context} {k} {size} {exact}"
                    );
                } else {
                    assert_eq!(size, tracks[k].0, "{context} {k}");
                }
            }
            let factors: f64 = (0..n).filter(|&k| flexible[k]).map(|k| tracks[k].1).sum();
            if factors >= 1.0 && fr > 0.0 {
                let used: f64 = spans.iter().map(|s| s.2).sum::<f64>() + gaps;
                assert_eq!(used, room, "{context}");
            }
        }
    }

    /// A stretched item whose content holds a percentage of its height,
    /// or one of percentage height, is laid again at its area's height
    /// once its row is sized.
    #[test]
    fn items_are_laid_again_at_their_areas_height() {
        let laid = Laid::new(
            "<div id=g><p id=tall>a<br>b<br>c<br>d\
             <div id=s><div id=half style=height:50%>h</div></div>\
             <div id=p style=height:100%>p</div></div>",
            "#g{display:grid;grid-template-columns:40px 40px 40px}",
            400.0,
        );
        assert_eq!(laid.border("tall").3, 80.0);
        assert_eq!(laid.border("s"), (40.0, 0.0, 40.0, 80.0));
        assert_eq!(laid.border("half"), (40.0, 0.0, 40.0, 40.0));
        assert_eq!(laid.border("p"), (80.0, 0.0, 40.0, 80.0));
        // At a known row height, each is laid at it once.
        let known = Laid::new(
            "<div id=g><div id=s><div id=half style=height:50%>h</div></div>\
             <div id=p style=height:100%>p</div></div>",
            "#g{display:grid;grid-template-columns:40px 40px;grid-template-rows:90px}",
            400.0,
        );
        assert_eq!(known.border("half"), (0.0, 0.0, 40.0, 45.0));
        assert_eq!(known.border("p"), (40.0, 0.0, 40.0, 90.0));
    }

    /// A page of grids of every placement and sizing, nested in and
    /// holding flex containers, tables, floats, inline-blocks and lists:
    /// each windowed layout equals the filtered whole one.
    #[test]
    fn windowed_layouts_equal_filtered_whole_ones() {
        let mut html = String::new();
        for n in 0..48 {
            let shape = [
                "grid-template-columns:repeat(3,1fr)",
                "grid-template-columns:repeat(auto-fill,minmax(60px,1fr));gap:4px",
                "grid-template-columns:auto 1fr;grid-auto-rows:1fr",
                "grid-template-columns:50px 50px;grid-auto-flow:column;grid-template-rows:auto auto",
                "grid-template-columns:1fr 2fr;height:200px",
                "grid-template-columns:1fr 1fr;grid-template-areas:'a b' 'c c'",
                "grid-template-columns:40px 1fr 40px;align-items:center",
                "grid-template-columns:1fr 1fr;min-height:150px;align-content:end",
            ][n % 8];
            html.push_str(&format!("<div class=g style='{shape}'>"));
            for k in 0..(3 + n % 5) {
                let place = match (n + k) % 7 {
                    0 if n % 3 == 0 => format!("grid-row:{};grid-column:1", 4 - k % 3),
                    1 => "grid-column:span 2".to_string(),
                    2 => "grid-row:span 2".to_string(),
                    3 if n % 8 == 5 => "grid-area:c".to_string(),
                    4 => "align-self:end".to_string(),
                    _ => String::new(),
                };
                let inner = match (n + k) % 6 {
                    0 => format!("<ol><li>one {n} {k}<li>two</ol>"),
                    1 => "<span class=fl>float</span>beside the float".to_string(),
                    2 => "<div class=f><i>in</i><i>ner<br>two</i></div>".to_string(),
                    3 => "<table><tr><td>a<br>b<td>c</table>".to_string(),
                    4 => "<div style=height:50%>half</div>more".to_string(),
                    _ => "x <span class=ib>in<br>block</span> y".to_string(),
                };
                html.push_str(&format!(
                    "<div style='{place}'>word{n} item{k} more words<br>{inner}</div>"
                ));
            }
            html.push_str("text after</div>");
            if n % 4 == 0 {
                html.push_str(
                    "<div class=f><div class=g style='grid-template-columns:1fr 1fr'>\
                     <p>in a flex<p>item<br>two</div><p>beside</div>\
                     <table><tr><td><div class=g style='grid-template-columns:auto auto'>\
                     <b>p</b><b>q<br>r</b></div><td>cell</table>\
                     <div class=fl><div class=g><b>x</b><b>y<br>z</b></div></div>\
                     <p>a para <span class=ib><span class=g style='grid-template-columns:1fr 1fr'>\
                     <b>a</b><b>b<br>c</b></span></span> after<p>clear",
                );
            }
        }
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(
            ".g{display:grid;column-gap:4px;padding:2px}.f{display:flex;align-items:center}\
             .fl{float:left;width:60px}.ib{display:inline-block}li{list-style-position:outside}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(styled.has_grid() && styled.has_flex() && styled.has_floats());
        for width in [160.0, 400.0] {
            let request = Request::new(width, 600.0);
            let full = layout(&styled, &mut Metrics, &request).unwrap();
            assert!(full.markers.len() >= 12);
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

    fn work(html: &str, css: &str, request: &Request<'_>, limits: Limits) -> Result<Layout, Error> {
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits)?;
        layout(&styled, &mut Metrics, request)
    }

    /// Lines at the clamp lay; a grid past MAX_GRID_CELLS is refused by
    /// name, at its edge and beyond.
    #[test]
    fn hostile_lines_and_cells_are_bounded() {
        let request = Request::new(400.0, 600.0);
        let grid = "#g{display:grid;grid-auto-rows:1px;grid-auto-columns:1px}";
        // td-css clamps the integers to 10,000; a row past them lands there.
        let laid = Laid::with(
            "<div id=g><p id=a style='grid-row:99999;grid-column:1'>a\
             <p id=b style='grid-row:-99999;grid-column:1'>b\
             <p id=c style='grid-row:span 3/9999'>c</div>",
            grid,
            &request,
            Limits::default(),
        )
        .unwrap();
        // b starts 9,999 rows before the explicit grid's first line, a
        // 19,998 rows below it.
        assert_eq!(laid.border("a"), (0.0, 19_998.0, 1.0, 1.0));
        assert_eq!(laid.border("b"), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(laid.border("c"), (0.0, 19_994.0, 1.0, 3.0));
        // A span to the clamp: 10,000 columns back from line 2.
        let wide = Laid::with(
            "<div id=g><p id=w style='grid-column:span 99999/2'>w<p id=x>x</div>",
            grid,
            &request,
            Limits::default(),
        )
        .unwrap();
        assert_eq!(wide.border("w"), (0.0, 0.0, 10_000.0, 1.0));
        assert_eq!(wide.border("x"), (0.0, 1.0, 1.0, 1.0));
        // 20,000 rows by 10,000 columns is past the bound.
        let both = "<div id=g><p style='grid-row:99999;grid-column:99999'>a\
                    <p style='grid-row:-99999;grid-column:-99999'>b</div>";
        assert!(matches!(
            work(both, grid, &request, Limits::default()),
            Err(Error::Limit("MAX_GRID_CELLS"))
        ));
        // At the bound: 1,024 columns by 4,096 rows, and one row more.
        let cells = |rows: usize| {
            format!("<div id=g><p style='grid-column:1/1025;grid-row:{rows}'>x<p>y</div>")
        };
        assert!(work(&cells(4096), grid, &request, Limits::default()).is_ok());
        assert!(matches!(
            work(&cells(4097), grid, &request, Limits::default()),
            Err(Error::Limit("MAX_GRID_CELLS"))
        ));
        // Auto-placement grows the cells it probes within the bound too.
        let auto = format!(
            "<div id=g>{}</div>",
            "<p style='grid-column:span 1024'>z".repeat(4097)
        );
        assert!(matches!(
            work(&auto, grid, &request, Limits::default()),
            Err(Error::Limit("MAX_GRID_CELLS"))
        ));
        // An automatic repeat of 1 px tracks stops at MAX_GRID_LINE.
        let fill = Laid::with(
            "<div id=g><p id=a style=grid-column:-2>a</div>",
            "#g{display:grid;grid-template-columns:repeat(auto-fill,1px)}",
            &Request::new(50_000.0, 600.0),
            Limits::default(),
        )
        .unwrap();
        assert_eq!(fill.border("a").0, 9_999.0);
    }

    /// Placement, sizing and laying cost what the items do, linearly:
    /// row by row, out of order, spanning and with named lines.
    #[test]
    fn cost_is_linear_in_items() {
        let request = Request::new(1000.0, 600.0);
        for (css, item) in [
            (
                "#g{display:grid;grid-template-columns:repeat(4,1fr)}",
                "<p>x y</p>",
            ),
            (
                "#g{display:grid;grid-template-columns:repeat(auto-fill,minmax(100px,1fr))}",
                "<p>x</p>",
            ),
            (
                "#g{display:grid;grid-auto-rows:1fr;grid-template-columns:1fr 1fr}",
                "<p>a b c</p>",
            ),
            (
                "#g{display:grid;grid-template-columns:[a] 1fr [b] auto [c] 1fr [d]}\
                 p{grid-column:b/d}i{grid-column:span 2}",
                "<p>x</p><i>y</i>",
            ),
            (
                "#g{display:grid;grid-auto-flow:column;grid-template-rows:repeat(3,auto)}",
                "<p>x</p>",
            ),
        ] {
            let cost = |n: usize| {
                let first = "<b style=grid-row:9000>last</b>";
                let html = format!("<div id=g>{first}{}</div>", item.repeat(n));
                work(&html, css, &request, Limits::default()).unwrap().work
            };
            let (small, large) = (cost(2_000), cost(20_000));
            assert!(large < 12 * small, "{css} {small} {large}");
        }
        // The records DESIGN.md's bounds charge.
        assert_eq!(std::mem::size_of::<crate::block::grid::GridItem>(), 192);
        assert_eq!(std::mem::size_of::<crate::block::grid::Track>(), 88);
        assert_eq!(std::mem::size_of::<crate::block::grid::Measured>(), 40);
    }

    /// Nested grids of auto columns measure each item once a layout, a
    /// level's measure taking its descendants' as kept: linear in depth,
    /// charged, and refused by name past the work and depth limits.
    #[test]
    fn nested_grids_are_bounded() {
        let nest = |d: usize| {
            format!(
                "<div class=g>{}x{}</div>",
                "<div class=g>a ".repeat(d),
                "</div>".repeat(d)
            )
        };
        let css = ".g{display:grid;grid-template-columns:auto 1fr}";
        let request = Request::new(400.0, 600.0);
        let cost = |d: usize, limits: Limits| work(&nest(d), css, &request, limits).map(|o| o.work);
        let (ten, twenty) = (
            cost(10, Limits::default()).unwrap(),
            cost(20, Limits::default()).unwrap(),
        );
        // Each item is measured once, whatever measures it: linear.
        assert!(twenty < 3 * ten, "{ten} {twenty}");
        let limits = Limits {
            work: ten,
            ..Limits::default()
        };
        assert!(matches!(
            cost(20, limits),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
        let limits = Limits {
            depth: 44,
            ..Limits::default()
        };
        assert!(cost(38, limits).is_ok());
    }

    /// A grid that is one band holds its lines above a window until it
    /// ends, charged: a byte limit the row-by-row grid's window fits
    /// refuses it, `MAX_PAGE_BYTES` by name. A line laid at or below the
    /// window's end is dropped as it is laid, since lines only move down.
    #[test]
    fn a_whole_grid_band_holds_its_lines_until_it_ends() {
        let items = "<p>some words in a card</p>".repeat(4_000);
        let html = format!("<div id=g>{items}</div>");
        let rows = "#g{display:grid;grid-template-columns:1fr 1fr}";
        let coupled = "#g{display:grid;grid-template-columns:1fr 1fr;grid-auto-rows:1fr}";
        let whole = work(&html, rows, &Request::new(400.0, 600.0), Limits::default()).unwrap();
        let mut request = Request::new(400.0, 600.0);
        request.window = Some(whole.height - 100.0..whole.height);
        let banded = work(&html, rows, &request, Limits::default()).unwrap();
        let held = work(&html, coupled, &request, Limits::default()).unwrap();
        assert!(banded.lines.len() <= 10, "{}", banded.lines.len());
        assert_eq!(held.lines.len(), banded.lines.len());
        // The coupled grid held every line at once above its window, as
        // many as the whole layout's.
        let slack = (whole.output_bytes - banded.output_bytes) / 2;
        request.bytes = Some(banded.page_bytes + slack);
        assert!(work(&html, rows, &request, Limits::default()).is_ok());
        assert!(matches!(
            work(&html, coupled, &request, Limits::default()),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
    }

    /// A page with a grid container is no text page, as one with flex is.
    #[test]
    fn a_grid_page_is_no_text_page() {
        let html = "<body>some words to fit";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("body{display:grid;white-space:pre-wrap}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(styled.has_grid());
        assert!(matches!(
            crate::text::fit(&styled, &mut Metrics, 400.0, 10, None),
            Err(Error::Invalid)
        ));
        let css = sheets("body{white-space:pre-wrap}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(!styled.has_grid());
        assert!(crate::text::fit(&styled, &mut Metrics, 400.0, 10, None).is_ok());
    }

    /// A grid container and each item root a formatting context: an
    /// item's height holds its floats, and none passes into its
    /// neighbours or past the grid.
    #[test]
    fn items_hold_their_floats() {
        let laid = Laid::new(
            "<div id=g style='display:grid;grid-template-columns:100px 100px'>\
             <div id=a><div style='float:left;width:10px;height:50px'></div>x</div>\
             <div id=b>y</div></div><p id=after>after",
            "",
            400.0,
        );
        assert_eq!(laid.border("a").3, 50.0);
        assert_eq!(laid.border("b").3, 50.0);
        assert_eq!(laid.border("g").3, 50.0);
        // The paragraph after starts at the left edge below the grid.
        let after = laid.border("after");
        assert!(after.1 >= 50.0, "{after:?}");
    }

    /// Line names are indexed once a grid, each name a unit of work, and
    /// an item naming lines costs a few binary searches whatever the lists
    /// hold, charged a unit a probe; a placement is kept for each of the
    /// last eight styles met, so items alternating between two place by
    /// a comparison, and nine in turn each search again.
    #[test]
    fn named_lines_are_indexed_and_charged() {
        let name = |k: usize| format!("nnnnnnnnnnnnnnnnnnnnnnnnn{k:03}");
        let list: Vec<String> = (0..1000).map(name).collect();
        let absent = |k: usize| format!("zzzzzzzzzzzzzzzzzzzzzzzzzz{k:02}");
        let mut css = format!(
            "#g{{display:grid;grid-template-columns:[{l}] 1px;grid-template-rows:[{l}] 1px}}\
             .c{{grid-area:{}}}",
            name(500),
            l = list.join(" "),
        );
        for k in 0..9 {
            let (a, b, c, d) = (absent(k), absent(k + 10), absent(k + 20), absent(k + 30));
            css.push_str(&format!(".s{k}{{grid-area:{a}/{b}/{c}/{d}}}"));
        }
        let request = Request::new(400.0, 600.0);
        let cost = |n: usize, item: &str| {
            let html = format!("<div id=g>{}</div>", item.repeat(n));
            work(&html, &css, &request, Limits::default()).unwrap().work
        };
        let each = |item: &str, per: usize| (cost(300, item) - cost(100, item)) / (200 * per);
        let plain = each("<p></p>", 1);
        let pair = each("<p class=s0></p><p class=s1></p>", 2);
        let nine: String = (0..9).map(|k| format!("<p class=s{k}></p>")).collect();
        let cycle = each(&nine, 9);
        // Two styles: kept, a comparison each.
        assert!(pair < plain + 20, "{pair} {plain}");
        // Nine in turn: eight lookups an item, each some twenty probes of
        // 28-byte names, not a walk of the 2,000 names.
        assert!(cycle > plain + 100, "{cycle} {plain}");
        assert!(cycle < plain + 400, "{cycle} {plain}");
        // One named item: the index of both axes' 2,000 names is charged.
        let one = cost(1, "<p class=c></p>");
        let none = cost(1, "<p></p>");
        // Each axis: a unit a name, the sort's bound before it (eleven a
        // name), a unit a name's place, and its lines' sort and check.
        assert!(one >= none + 50_000, "{one} {none}");
        // And it finds the name, which every list puts on its first line.
        let laid = Laid::new("<div id=g><p id=c class=c>c</p></div>", &css, 400.0);
        assert_eq!(laid.border("c"), (0.0, 0.0, 1.0, 1.0));
    }

    /// Grids of percentage height nested in each other lay their content
    /// again at each level's height, as nested flex columns do, but
    /// measure each item once a layout: a lone leaf 30 deep costs under
    /// 200,000 units, and doubling the depth some eight times that.
    #[test]
    fn nested_percentage_grids_measure_once() {
        let nest = |d: usize| {
            format!(
                "<body>{}<p>leaf</p>{}",
                "<div style='display:grid;height:100%'>".repeat(d),
                "</div>".repeat(d)
            )
        };
        let request = Request::new(400.0, 600.0);
        let cost = |d: usize| {
            work(&nest(d), "", &request, Limits::default())
                .unwrap()
                .work
        };
        let (thirty, sixty) = (cost(30), cost(60));
        assert!(thirty < 200_000, "{thirty}");
        assert!(sixty < 9 * thirty, "{thirty} {sixty}");
    }

    /// A least height that the content's `fr` size leaves unfilled is
    /// filled by the flexible rows, the fr size worked again as at that
    /// height (§12.7.1), and a greatest height they pass caps them so;
    /// rows at their growth limits are capped by it too (§12.6).
    #[test]
    fn least_and_greatest_heights_redo_flexible_rows() {
        let shell = "#g{display:grid;grid-template-areas:'h h' 'n m' 'f f';\
                     grid-template-rows:auto 1fr auto;grid-template-columns:150px 1fr}\
                     #h{grid-area:h}#n{grid-area:n}#m{grid-area:m}#f{grid-area:f}";
        let html =
            "<div id=g><div id=h>h</div><div id=n>n</div><div id=m>m</div><div id=f>f</div></div>";
        let laid = Laid::new(html, &format!("{shell}#g{{min-height:600px}}"), 400.0);
        assert_eq!(laid.border("h"), (0.0, 0.0, 400.0, 20.0));
        assert_eq!(laid.border("n"), (0.0, 20.0, 150.0, 560.0));
        assert_eq!(laid.border("m"), (150.0, 20.0, 250.0, 560.0));
        assert_eq!(laid.border("f"), (0.0, 580.0, 400.0, 20.0));
        assert_eq!(laid.border("g").3, 600.0);
        // A percentage of a definite parent's height.
        let html = format!("<div style=height:300px>{html}</div>");
        let laid = Laid::new(&html, &format!("{shell}#g{{min-height:100%}}"), 400.0);
        assert_eq!(laid.border("m"), (150.0, 20.0, 250.0, 260.0));
        assert_eq!(laid.border("f").1, 280.0);
        // Equal fractions share a least height.
        let two = "<div id=g><p id=a>a<p id=b>b</div>";
        let laid = Laid::new(
            two,
            "#g{display:grid;grid-template-rows:1fr 1fr;min-height:100px}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 400.0, 50.0));
        assert_eq!(laid.border("b"), (0.0, 50.0, 400.0, 50.0));
        // And a greatest height caps them, but for an automatic minimum.
        let rows =
            "#g{display:grid;grid-template-rows:minmax(0,1fr) minmax(0,1fr);max-height:30px}";
        let laid = Laid::new(two, rows, 400.0);
        assert_eq!(laid.border("b").1, 15.0);
        let laid = Laid::new(
            two,
            "#g{display:grid;grid-template-rows:minmax(10px,100px) minmax(10px,100px);max-height:50px}",
            400.0,
        );
        assert_eq!(laid.border("b").1, 25.0);
    }

    /// An item stretched to a row shorter than it laid overflows with its
    /// stretched box and what it holds, not the box it first laid: the
    /// page's height and overflow are a grid's of fixed rows.
    #[test]
    fn a_shrunk_item_overflows_with_its_stretched_box() {
        let html = "<div id=g><div id=a style='padding-bottom:20px;border-bottom:200px solid'>\
                    a<br>b<br>c</div><div>b</div></div>";
        let request = Request::new(400.0, 600.0);
        let laid = |css: &str| work(html, css, &request, Limits::default()).unwrap();
        let auto = laid("#g{display:grid;grid-template-rows:30px;grid-auto-rows:auto}");
        let fixed = laid("#g{display:grid;grid-template-rows:30px 20px}");
        assert_eq!(auto.height, 220.0);
        assert_eq!(auto.overflow, fixed.overflow);
        assert_eq!(auto.height, fixed.height);
        assert_eq!(auto.overflow.height, 220.0);
    }

    /// Rows locked by lines at both clamps, then items each placed past
    /// the last in one row, widen the bitset past its bound: refused by
    /// name as it is restrided, not by the page's bytes.
    #[test]
    fn a_widening_bitset_is_refused_by_cells() {
        let html = format!(
            "<div id=g><p class=t>t<p class=u>u{}</div>",
            "<p class=a>a".repeat(3_000)
        );
        let css = "#g{display:grid}.t{grid-row:-10000}.u{grid-row:10000}.a{grid-row:1}";
        let laid = work(&html, css, &Request::new(400.0, 600.0), Limits::default());
        assert!(
            matches!(laid, Err(Error::Limit("MAX_GRID_CELLS"))),
            "{:?}",
            laid.err()
        );
    }

    /// An automatic repeat counts its tracks at their maxima floored by
    /// their minima (§7.2.3.2): `minmax(300px, 25%)` in 800 px repeats
    /// twice, 300 px each.
    #[test]
    fn auto_repeats_floor_their_maxima_by_their_minima() {
        let laid = Laid::new(
            "<div id=g><p id=a>a<p id=b>b<p id=c>c<p id=d>d</div>",
            "#g{display:grid;grid-template-columns:repeat(auto-fill,minmax(300px,25%))}",
            800.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 300.0, 20.0));
        assert_eq!(laid.border("b"), (300.0, 0.0, 300.0, 20.0));
        assert_eq!(laid.border("c"), (0.0, 20.0, 300.0, 20.0));
        assert_eq!(laid.border("d"), (300.0, 20.0, 300.0, 20.0));
    }

    /// Names in repeats are indexed as the repeat is written, their lines
    /// counted by arithmetic: items place alike against a repeat and
    /// against the same list written out, with names before, in and after
    /// it, at a repetition's start and end and next to its edges, placed
    /// by number, from the end and by span.
    #[test]
    fn repeated_names_place_as_written_out() {
        const NAMES: &[&str] = &["a", "b", "c"];
        let mut r = Rng(0x5eed_1a3e);
        let pick = |r: &mut Rng| -> Vec<&'static str> {
            NAMES.iter().copied().filter(|_| r.below(3) == 0).collect()
        };
        let names = |set: &[&str]| {
            if set.is_empty() {
                String::new()
            } else {
                format!("[{}]", set.join(" "))
            }
        };
        let lines = |sets: &[Vec<&str>]| {
            sets.iter()
                .map(|set| names(set))
                .collect::<Vec<_>>()
                .join(" 10px ")
        };
        for case in 0..600 {
            let before: Vec<Vec<&str>> = (0..1 + r.below(2)).map(|_| pick(&mut r)).collect();
            let period = 1 + r.below(3) as usize;
            let body: Vec<Vec<&str>> = (0..=period).map(|_| pick(&mut r)).collect();
            let after: Vec<Vec<&str>> = (0..1 + r.below(2)).map(|_| pick(&mut r)).collect();
            let times = 1 + r.below(4) as usize;
            let auto = r.below(2) == 0;
            // A track between the repeat and what is around it, or none.
            let (open, close) = (r.below(2) == 0, r.below(2) == 0);
            let how = if auto {
                "auto-fill".to_string()
            } else {
                times.to_string()
            };
            let repeated = format!(
                "{}{}repeat({how},{}){}{}",
                lines(&before),
                if open { " 10px " } else { " " },
                lines(&body),
                if close { " 10px " } else { " " },
                lines(&after),
            );
            // Written out: each line's names, a repetition's end and the
            // next one's start one line, and an edge without a track its
            // neighbour's line.
            let mut out: Vec<Vec<&str>> = before.clone();
            let mut first = open;
            for rep in 0..times {
                for (i, set) in body.iter().enumerate() {
                    let joins = (i == 0 && rep > 0) || (i == 0 && rep == 0 && !first);
                    match out.last_mut() {
                        Some(line) if joins => line.extend(set.iter().copied()),
                        _ => out.push(set.clone()),
                    }
                }
                first = true;
            }
            for (i, set) in after.iter().enumerate() {
                match out.last_mut() {
                    Some(line) if i == 0 && !close => line.extend(set.iter().copied()),
                    _ => out.push(set.clone()),
                }
            }
            for line in out.iter_mut() {
                line.sort_unstable();
                line.dedup();
            }
            let written = lines(&out);
            // A width that repeats an `auto-fill` body `times` times.
            let outside = out.len() - 1 - times * period;
            let width = (outside + times * period) * 10 + 5;
            let mut items = String::new();
            for k in 0..6 {
                let name = NAMES[r.below(3) as usize];
                let n = 1 + r.below(4);
                let place = match r.below(5) {
                    0 => format!("{name} {n}"),
                    1 => format!("{name} -{n}"),
                    2 => format!(
                        "{name} {n} / span {} {}",
                        NAMES[r.below(3) as usize],
                        1 + r.below(3)
                    ),
                    3 => format!("span {name} {n} / {}", 2 + r.below(6)),
                    _ => name.to_string(),
                };
                items.push_str(&format!("<p id=i{k} style='grid-column:{place}'>x</p>"));
            }
            let html = format!("<div id=g>{items}</div>");
            let lay = |columns: &str| {
                let css = format!(
                    "#g{{display:grid;width:{width}px;grid-template-columns:{columns};\
                     grid-auto-columns:7px}}"
                );
                let laid = Laid::new(&html, &css, 2000.0);
                (0..6)
                    .map(|k| laid.border(&format!("i{k}")))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                lay(&repeated),
                lay(&written),
                "case {case}: {repeated} against {written}; {items}"
            );
        }
    }

    /// The name index costs the names a list spells out, not its repeats
    /// expanded: 400 names of 2,503 bytes in an `auto-fill` repeat of
    /// 10,000 tracks, and 1,000 short names in one at 2,000 px, are each
    /// indexed and looked up in a few hundred thousand units.
    #[test]
    fn repeated_names_cost_what_is_written() {
        let request = Request::new(10_000.0, 600.0);
        let prefix = "p".repeat(2_500);
        let long: Vec<String> = (0..400).map(|k| format!("{prefix}{k:03}")).collect();
        let css = format!(
            "#g{{display:grid;width:10000px;grid-template-columns:repeat(auto-fill,[{}] 1px)}}\
             .a{{grid-column:zz}}",
            long.join(" ")
        );
        let html = "<div id=g><p id=a class=a>a</p></div>";
        let laid = work(html, &css, &request, Limits::default()).unwrap();
        assert!(laid.work < 400_000, "{}", laid.work);
        let short: Vec<String> = (0..1000).map(|k| format!("n{k}")).collect();
        let css = format!(
            "#g{{display:grid;grid-template-columns:repeat(auto-fill,[{}] 1px)}}\
             .a{{grid-column:n500 3/span n7 2}}",
            short.join(" ")
        );
        let laid =
            Laid::with(html, &css, &Request::new(2_000.0, 600.0), Limits::default()).unwrap();
        assert!(laid.out.work < 200_000, "{}", laid.out.work);
        // n500's third line is the third repetition's start, its span to
        // n7's second line after it one repetition on.
        let a = laid.border("a");
        assert_eq!((a.0, a.2), (2.0, 2.0));
    }

    /// Contributions kept from a grid's earlier measure lay what measuring
    /// it again would: seeded pages of nested grids of intrinsic and `fr`
    /// tracks, percentage heights laying them again, and grids measured
    /// in floats, inline-blocks, cells and flex items, laid with and
    /// without what is kept, give the same boxes, lines and markers.
    #[test]
    fn kept_contributions_lay_as_measured_again() {
        fn grid(r: &mut Rng, depth: usize, out: &mut String) {
            let columns = [
                "auto 1fr",
                "1fr 1fr",
                "repeat(auto-fill,minmax(40px,1fr))",
                "min-content auto",
                "max-content 1fr",
                "fit-content(80px) 1fr",
                "auto auto auto",
            ][r.below(7) as usize];
            let rows = [
                "",
                "grid-auto-rows:1fr;",
                "height:100%;",
                "min-height:60px;",
            ][r.below(4) as usize];
            out.push_str(&format!(
                "<div style='display:grid;grid-template-columns:{columns};{rows}'>"
            ));
            for k in 0..1 + r.below(5) {
                let own = [
                    "",
                    "height:100%;",
                    "grid-column:span 2;",
                    "align-self:center;",
                    "min-width:30px;",
                ][r.below(5) as usize];
                out.push_str(&format!("<div style='{own}'>w{k} "));
                match r.below(6) {
                    0 if depth < 3 => grid(r, depth + 1, out),
                    1 if depth < 3 => {
                        out.push_str("<span style=float:left>");
                        grid(r, depth + 1, out);
                        out.push_str("</span>");
                    }
                    2 if depth < 3 => {
                        out.push_str("<span style=display:inline-block>");
                        grid(r, depth + 1, out);
                        out.push_str("</span>");
                    }
                    3 if depth < 3 => {
                        out.push_str("<table><tr><td>");
                        grid(r, depth + 1, out);
                        out.push_str("<td>cell</table>");
                    }
                    4 if depth < 3 => {
                        out.push_str("<div style=display:flex><div style=flex:1>");
                        grid(r, depth + 1, out);
                        out.push_str("</div><b>side</b></div>");
                    }
                    _ => out.push_str("some longer words here"),
                }
                out.push_str("</div>");
            }
            out.push_str("</div>");
        }
        let mut r = Rng(0x6e30_9e1d);
        for case in 0..60 {
            let mut html = String::from("<div style=height:400px>");
            for _ in 0..1 + r.below(3) {
                grid(&mut r, 0, &mut html);
            }
            html.push_str("</div>");
            for width in [600.0, 250.0] {
                let request = Request::new(width, 600.0);
                let lay = |off: bool| {
                    crate::block::grid::MEMO_OFF.with(|m| m.set(off));
                    let laid = work(&html, "", &request, Limits::default());
                    crate::block::grid::MEMO_OFF.with(|m| m.set(false));
                    let laid = laid.unwrap();
                    format!(
                        "{:?}{:?}{:?}",
                        &laid.boxes[..],
                        &laid.lines[..],
                        &laid.markers[..]
                    )
                };
                assert!(lay(false) == lay(true), "case {case} at {width}: {html}");
            }
        }
    }

    /// A grid laid once keeps none of its contributions, so a page of
    /// many items lays in what it did before they were kept: 132,000
    /// empty items in one column, and 180,000 in fixed columns.
    #[test]
    fn a_grid_laid_once_keeps_nothing() {
        let request = Request::new(400.0, 600.0);
        let column = format!("<div id=g>{}</div>", "<p>".repeat(132_000));
        assert!(work(&column, "#g{display:grid}", &request, Limits::default()).is_ok());
        let cards = format!("<div id=g>{}</div>", "<div></div>".repeat(180_000));
        let css = "#g{display:grid;grid-template-columns:repeat(4,100px)}";
        assert!(work(&cards, css, &request, Limits::default()).is_ok());
    }

    /// A least height above a greatest one wins as rows are maximized
    /// (§12.6) and as flexible rows are worked again (§12.7.1, CSS 2.2
    /// §10.7).
    #[test]
    fn a_least_height_above_the_greatest_wins() {
        for rows in ["minmax(0,500px)", "1fr", "minmax(0,500px) 1fr"] {
            let laid = Laid::new(
                "<div id=g><p id=a>a</div>",
                &format!("#g{{display:grid;grid-template-rows:{rows};min-height:300px;max-height:100px}}"),
                400.0,
            );
            assert_eq!(laid.border("a").3, 300.0, "{rows}");
            assert_eq!(laid.border("g").3, 300.0, "{rows}");
        }
    }

    /// A name is ordered against a base and a suffix as the two written
    /// together, as byte slices: prefixes, equal stems, longer and
    /// shorter names.
    #[test]
    fn names_order_against_base_and_suffix() {
        const PARTS: &[&str] = &[
            "", "a", "ab", "abc", "b", "a-start", "a-", "-start", "-end", "ba",
        ];
        for n in PARTS {
            for base in PARTS {
                for suffix in ["", "-start", "-end", "c"] {
                    let whole = format!("{base}{suffix}");
                    assert_eq!(
                        crate::block::grid::name_order(n, (base, suffix)),
                        (*n).cmp(whole.as_str()),
                        "{n:?} against {base:?} {suffix:?}"
                    );
                }
            }
        }
    }

    /// The line name indexes are the engine's scratch, emptied for each
    /// grid: page bytes grow by the largest index, not by the grids
    /// built, so 1,500 grids of one 1,000-name list lay, a 120-deep nest
    /// of named `height: 100%` grids built again as it re-lays lays, and
    /// grids naming lines cost what grids numbering them do but once.
    #[test]
    fn line_name_indexes_are_charged_once() {
        let request = Request::new(800.0, 600.0);
        let names: Vec<String> = (0..1000).map(|k| format!("n{k}")).collect();
        let css = format!(
            ".g{{display:grid;grid-template-columns:[{}] 1px}}.a{{grid-column:n999}}",
            names.join(" ")
        );
        let many = "<div class=g><p class=a>a</p></div>".repeat(1_500);
        assert!(work(&many, &css, &request, Limits::default()).is_ok());
        let nest = format!(
            "{}<p class=n>leaf</p>{}",
            "<div class=n>".repeat(120),
            "</div>".repeat(120)
        );
        let css = ".n{display:grid;height:100%;grid-template-rows:[a] auto [b c] auto [d];\
                   grid-row:c}";
        // A debug build's frames want more than a test thread's stack.
        let deep = std::thread::Builder::new()
            .stack_size(16 << 20)
            .spawn(move || {
                work(&nest, css, &Request::new(800.0, 600.0), Limits::default()).map(|l| l.work)
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(deep.is_ok(), "{deep:?}");
        // Page bytes over the same grids placing an item by name and by
        // number: the difference is one index however many grids build.
        let names: Vec<String> = (0..100).map(|k| format!("n{k}")).collect();
        let bytes = |count: usize, place: &str| {
            let css = format!(
                ".g{{display:grid;grid-template-columns:repeat(2,[{}] 1px)}}.a{{grid-column:{place}}}",
                names.join(" ")
            );
            let html = "<div class=g><p class=a>a</p></div>".repeat(count);
            work(&html, &css, &request, Limits::default())
                .unwrap()
                .page_bytes
        };
        let extra = |count| bytes(count, "n50 2") as i64 - bytes(count, "2") as i64;
        let (few, more) = (extra(10), extra(1_000));
        assert!(few > 0, "{few}");
        assert!(more <= few + 1_024, "{few} {more}");
    }

    /// Each grid indexes its own names: one built after another finds
    /// its names where its own list has them, and a list with none puts
    /// a name in an implicit column, whatever the grid before held.
    #[test]
    fn each_grid_indexes_its_own_names() {
        let p = ".p{display:grid;grid-template-columns:[a] 10px [b] 10px [c] 10px}";
        let laid = Laid::new(
            "<div class=p><p style=grid-column:a>x</div><div class=q><p id=i style=grid-column:a>x</div>",
            &format!("{p}.q{{display:grid;grid-template-columns:[c] 10px [b] 10px [a] 10px}}"),
            800.0,
        );
        assert_eq!(laid.border("i").0, 20.0);
        // Laid as with no grid before it: in an implicit column past the
        // three it lists.
        let r = ".r{display:grid;grid-template-columns:10px 10px 10px}";
        let item = "<div class=r><p id=i style=grid-column:c>x</div>";
        let alone = Laid::new(item, r, 800.0).border("i").0;
        let laid = Laid::new(
            &format!("<div class=p><p style=grid-column:a>x</div>{item}"),
            &format!("{p}{r}"),
            800.0,
        );
        assert!(alone > 30.0, "{alone}");
        assert_eq!(laid.border("i").0, alone);
        // Rows are indexed apart from columns, each grid its own.
        let s = ".s{display:grid;grid-template-rows:[a] 10px [b] 10px [c] 10px}";
        let laid = Laid::new(
            "<div class=s><p style=grid-row:a>x</div><div class=t id=t><p id=i style=grid-row:a>x</div>",
            &format!("{s}.t{{display:grid;grid-template-rows:[c] 10px [b] 10px [a] 10px}}"),
            800.0,
        );
        assert_eq!(laid.border("i").1 - laid.border("t").1, 20.0);
    }
}
