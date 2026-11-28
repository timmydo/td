//! Inline grid containers, laid as inline-blocks are, and the first
//! baseline a grid gives its line, a cell or a flex line (DESIGN.md §5,
//! "Grid", "Inline-blocks"). Each scalar advances 8 px and a 16 px line
//! is 20 px tall with its baseline 15 px down, so widths, tops and
//! baselines are worked by hand.
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
            (Origin::Author, "#t,.t{display:inline-grid}"),
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
        /// Each id'd box's border and content rects, by id.
        fn by_id(&self) -> BTreeMap<String, String> {
            self.out
                .boxes
                .iter()
                .filter_map(|g| {
                    let id = self.id(g.node)?.to_string();
                    Some((id, format!("{:?} {:?}", g.border, g.content)))
                })
                .collect()
        }
    }

    /// An inline grid container of two columns.
    const T: &str = "<span id=t><b id=a>cd</b><b id=b>ef</b></span>";

    /// Laid whole where its line puts it, shrink-to-fit, its items in its
    /// columns, a placeholder its border box, its lines after its line's.
    #[test]
    fn an_inline_grid_is_laid_whole_in_its_line() {
        let css = "#t{grid-template-columns:auto auto}";
        let laid = Laid::new(&format!("<p>ab {T} gh"), css, 200.0);
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
        // A set width shared by `1fr` columns; one column, as wide as its
        // widest item, its items in rows.
        let laid = Laid::new(
            &format!("<p>ab {T} gh"),
            "#t{width:100px;grid-template-columns:1fr 1fr}",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 100.0, 20.0));
        assert_eq!(laid.border("b"), (74.0, 0.0, 50.0, 20.0));
        let laid = Laid::new(&format!("<p>ab {T} gh"), "#a{width:40px}", 200.0);
        assert_eq!(laid.border("t"), (24.0, 0.0, 40.0, 40.0));
        assert_eq!(laid.border("b"), (24.0, 20.0, 40.0, 20.0));
        // Its auto margins are 0, as an inline-block's.
        let laid = Laid::new(
            &format!("<p>ab {T} gh"),
            &format!("{css}#t{{margin:0 auto}}"),
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 32.0, 20.0));
    }

    /// It lays as an inline-block holding the same grid: the same boxes
    /// by id (the inline-block's its own, the grid's within it no other)
    /// and lines, its line's last; only its baseline, and so its line's
    /// height, differ.
    #[test]
    fn it_lays_as_an_inline_block_holding_its_grid() {
        let shapes = [
            (
                "grid-template-columns:auto auto",
                "<b id=a>cd</b><b id=b>ef gh</b>",
            ),
            (
                "grid-template-columns:1fr 2fr;width:90px;column-gap:4px",
                "<b id=a>one two three</b><b id=b>x</b><b id=c style=grid-column:span 2>wide</b>",
            ),
            (
                "grid-template-columns:repeat(auto-fill,minmax(30px,1fr));width:100px",
                "<b id=a>p</b><b id=b>q r</b><b id=c>s</b><b id=d>t</b>",
            ),
            (
                "grid-template-columns:40px auto;grid-auto-rows:1fr;align-items:center",
                "<b id=a style=grid-row:2>low</b><b id=b>a<br>b<br>c</b>tail words",
            ),
            (
                "grid-template-areas:'h h' 'n m';justify-items:center;row-gap:3px",
                "<b id=a style=grid-area:m>m</b><b id=b style=grid-area:h>head</b>\
                 <b id=c style=grid-area:n>nav here</b>",
            ),
            ("", ""),
        ];
        for width in [200.0, 40.0] {
            for (css, items) in shapes {
                let inline = Laid::new(
                    &format!("<p>ab <span id=t>{items}</span>"),
                    &format!("#t{{{css}}}"),
                    width,
                );
                let boxed = Laid::new(
                    &format!("<p>ab <span id=t><span id=g>{items}</span></span>"),
                    &format!("#t{{display:inline-block}}#g{{display:grid;{css}}}"),
                    width,
                );
                let mut boxes = boxed.by_id();
                let grid = boxes.remove("g").unwrap();
                assert_eq!(boxes.get("t"), Some(&grid), "{css} at {width}");
                assert_eq!(inline.by_id(), boxes, "{css} at {width}");
                assert_eq!(inline.lines(), boxed.lines(), "{css} at {width}");
            }
        }
    }

    /// Its baseline is its first item's in grid order (§10.8), placed
    /// first in row-major order whatever its source order: that item's
    /// first line's, else its border box's bottom; with no item, its
    /// bottom margin edge.
    #[test]
    fn its_baseline_is_its_first_items_in_grid_order() {
        let at = |html: &str, css: &str| Laid::new(&format!("<p>ab {html}"), css, 200.0).holding();
        let columns = "#t{grid-template-columns:auto auto}";
        // Its first item padded: its line's, 25 px down.
        assert_eq!(
            at(T, &format!("{columns}#a{{padding-top:10px}}")),
            (0.0, 30.0, 25.0)
        );
        // Its second padded: its first item's, 15 px down.
        assert_eq!(
            at(T, &format!("{columns}#b{{padding-top:10px}}")),
            (0.0, 30.0, 15.0)
        );
        // The second placed first in its row: its line's, 25 px down.
        assert_eq!(
            at(
                T,
                "#t{grid-template-columns:auto auto}#a{grid-column:2}\
                 #b{grid-column:1;grid-row:1;padding-top:10px}"
            ),
            (0.0, 30.0, 25.0)
        );
        // The second placed in the first row, the first in the second:
        // the second's, 25 px down, its row first.
        assert_eq!(
            at(T, "#a{grid-row:2}#b{grid-row:1;padding-top:10px}"),
            (0.0, 50.0, 25.0)
        );
        // Auto-placed after an item placed at the start: that item's.
        assert_eq!(
            at(
                "<span id=t><b id=a>cd</b><b id=b style=grid-area:1/1;padding-top:10px>ef</b></span>",
                "#t{grid-template-columns:auto auto}"
            ),
            (0.0, 30.0, 25.0)
        );
        // Its first item's relative offset moves the item, not its
        // baseline; its own moves it, as an inline-block's.
        let laid = Laid::new(
            &format!("<p>ab {T}"),
            &format!("{columns}#a{{position:relative;top:-500px}}"),
            200.0,
        );
        assert_eq!(
            (laid.holding(), laid.border("a").1),
            ((0.0, 20.0, 15.0), -500.0)
        );
        let laid = Laid::new(
            &format!("<p>ab {T}"),
            &format!("{columns}#t{{position:relative;top:50px}}"),
            200.0,
        );
        assert_eq!(
            (laid.holding(), laid.border("a").1),
            ((0.0, 20.0, 15.0), 50.0)
        );
        // A relative offset within it moves it with its line, as an
        // inline-flex's: 23 px, where browsers keep 15 (a departure).
        let laid = Laid::new(
            "<div>ab <span id=t><b id=a><b style=display:block;position:relative;top:8px>\
             cd</b></b><b>ef</b></span></div>",
            columns,
            200.0,
        );
        assert_eq!(laid.holding().2, 23.0);
        // An item of no line: its border box's bottom, 30 px down.
        assert_eq!(
            at(
                "<span id=t><b id=a style=height:30px></b><b>ef</b></span>",
                columns
            ),
            (0.0, 35.0, 30.0)
        );
        // Centred in a taller row: where it was moved, 20 px down.
        assert_eq!(
            at(
                T,
                &format!("{columns}#a{{align-self:center}}#b{{height:30px}}")
            ),
            (0.0, 30.0, 20.0)
        );
        // Placed below a fixed first row by `align-content`: 75 px down.
        assert_eq!(
            at(T, "#t{height:100px;align-content:end}"),
            (0.0, 100.0, 75.0)
        );
        // Empty: its bottom margin edge, 30 px down with its margin.
        let laid = Laid::new(
            "<p>ab <span id=t></span>",
            "#t{width:20px;height:26px;margin-bottom:4px}",
            200.0,
        );
        assert_eq!(laid.border("t"), (24.0, 0.0, 20.0, 26.0));
        assert_eq!(laid.holding(), (0.0, 35.0, 30.0));
        // An anonymous item's line.
        assert_eq!(
            at(
                "<span id=t>xy<b id=a style=padding-top:10px>cd</b></span>",
                "#t{grid-template-columns:auto auto;align-items:start}"
            ),
            (0.0, 30.0, 15.0)
        );
    }

    /// A cell aligned on its baseline, and a flex item sharing its line's,
    /// that hold a grid take its first item's in grid order, not its first
    /// in tree order (§10.8).
    #[test]
    fn cells_and_flex_lines_take_a_grids_first_baseline() {
        let grid = "<div style=display:grid><b id=a style=grid-row:2>low</b>\
                    <b id=b style=grid-row:1;padding-top:10px>high</b></div>";
        let laid = Laid::new(
            &format!("<table><tr><td>x<td id=d>{grid}</table>"),
            "td{vertical-align:baseline}table{border-spacing:0}",
            200.0,
        );
        let x = laid.lines().into_iter().find(|l| l.2 == "x").unwrap();
        assert_eq!(x.0, 10.0);
        assert_eq!(laid.border("b").1, 0.0);
        let laid = Laid::new(
            &format!(
                "<div style=display:flex;align-items:baseline><p id=x>x</p><div>{grid}</div></div>"
            ),
            "",
            200.0,
        );
        assert_eq!(laid.border("x").1, 10.0);
        // An item of no line: its border box's bottom, 30 px down (§10.8),
        // not what follows the grid.
        let css = "td{vertical-align:baseline}table{border-spacing:0}";
        let laid = Laid::new(
            "<table><tr><td id=c>x<td><div style=display:grid><b style=height:30px></b>\
             <b>ef</b></div></table>",
            css,
            200.0,
        );
        let x = laid.lines().into_iter().find(|l| l.2 == "x").unwrap();
        assert_eq!(x.0, 15.0);
        assert_eq!(laid.border("c").3, 50.0);
        let laid = Laid::new(
            "<table><tr><td>x<td><div style=display:grid><b style=height:30px></b></div>\
             <p style=padding-top:5px>after</table>",
            css,
            200.0,
        );
        let x = laid.lines().into_iter().find(|l| l.2 == "x").unwrap();
        assert_eq!(x.0, 15.0);
        // Kept across a re-lay after it: a grid after it laying an item
        // again restores what the first took.
        let laid = Laid::new(
            "<table><tr><td>x<td><div style=display:grid><b style=height:30px></b></div>\
             <div style=display:grid;grid-auto-rows:1fr><b><i style=display:block;height:100%>y\
             </i></b><b>z<br>z</b></div></table>",
            css,
            200.0,
        );
        let x = laid.lines().into_iter().find(|l| l.2 == "x").unwrap();
        assert_eq!(x.0, 15.0);
        // A grid with no item has none: what follows it.
        let laid = Laid::new(
            "<table><tr><td>x<td><div style=display:grid;height:20px></div>\
             <p style=padding-top:5px>after</table>",
            css,
            200.0,
        );
        let x = laid.lines().into_iter().find(|l| l.2 == "x").unwrap();
        assert_eq!(x.0, 25.0);
        // An inline-grid whose first item is a grid whose own first has no
        // line: that item's border box's bottom, 30 px down.
        let laid = Laid::new(
            "<div>ab <span id=t><div style=display:grid><b style=height:30px></b><b>ef</b></div>\
             <b>gh</b></span></div>",
            "",
            200.0,
        );
        // Its second item in its second row: 70 px tall, 30 above.
        assert_eq!(laid.holding(), (0.0, 70.0, 30.0));
        // And a flex item sharing its line's baseline.
        let laid = Laid::new(
            "<div style=display:flex;align-items:baseline><p id=x>x</p><div>\
             <div style=display:grid><b style=height:30px></b><b>ef</b></div></div></div>",
            "",
            200.0,
        );
        assert_eq!(laid.border("x").1, 15.0);
    }

    /// Measured whole in a cell and a float: its columns' sum, its widest
    /// item for its least where it has one column.
    #[test]
    fn it_is_measured_in_cells_and_floats() {
        let css = "table{border-spacing:0}#t{grid-template-columns:auto auto}";
        let laid = Laid::new(&format!("<table><tr><td id=d>ab {T}</table>"), css, 200.0);
        assert_eq!(laid.border("d").2, 56.0);
        let float = |css: &str, width: f64| {
            let html = format!("<div id=f>ab {T}</div>");
            Laid::new(&html, &format!("#f{{float:left}}{css}"), width)
                .border("f")
                .2
        };
        assert_eq!(float("#t{grid-template-columns:auto auto}", 200.0), 56.0);
        assert_eq!(float("", 200.0), 40.0);
        assert_eq!(float("", 10.0), 16.0);
        assert_eq!(
            float(
                "#t{grid-template-columns:auto auto;margin-left:4px;border:1px solid}",
                200.0
            ),
            62.0
        );
    }

    /// Dense pages of inline grid containers, in one paragraph and in
    /// many, cost linearly in work and in what they charge.
    #[test]
    fn dense_pages_are_linear() {
        let cost = |n: usize, one: &str| {
            let html = format!("<p>{}", one.repeat(n));
            let doc = td_html::document::parse(&html).unwrap();
            let css = sheets(".t{grid-template-columns:auto auto}.u{padding-top:3px}");
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(1000.0, 600.0)).unwrap();
            assert!(out.lines.len() >= n, "{}", out.lines.len());
            (out.work, out.page_bytes)
        };
        for one in [
            "<span class=t><b>a</b><b class=u style=grid-row:1>b</b></span> ",
            "<p>x <span class=t><b>a</b><b>b</b></span> y",
        ] {
            let (small, large) = (cost(2_000, one), cost(20_000, one));
            assert!(large.0 < 11 * small.0, "{one} {small:?} {large:?}");
            // Charges as vectors double: within a doubling of linear.
            assert!(large.1 < 12 * small.1, "{one} {small:?} {large:?}");
        }
    }

    /// A picture whose display is `inline-grid` is an inline picture, in
    /// its line, as an `inline-block` one is.
    #[test]
    fn a_picture_of_inline_grid_is_inline() {
        let laid = Laid::new(
            "<p>ab <img id=i width=10 height=12 style=display:inline-grid> cd",
            "",
            200.0,
        );
        assert_eq!(laid.lines().len(), 1);
        assert_eq!(laid.out.lines[0].replaced[0].x, 24.0);
    }

    /// What is one: a block whose display is inline-grid, not the root; a
    /// floated one is a block-level grid container.
    #[test]
    fn what_is_an_inline_grid() {
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
        let grids: Vec<_> = laid
            .out
            .boxes
            .iter()
            .filter(|g| styled.is_grid_container(g))
            .collect();
        assert_eq!(grids, vec![laid.geometry("t")]);
        let floated = Laid::new(&html, "#t{float:left}", 200.0);
        assert!(!floated.nested);
        assert_eq!(floated.border("t"), (0.0, 0.0, 16.0, 40.0));
    }
}
