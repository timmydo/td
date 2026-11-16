#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use crate::block::{layout, BoxGeometry, Layout, Rect, Request};
    use crate::inline::{FontLine, FontMetrics, Line};
    use crate::{Error, Limits, StyledDocument, TableRole, MAX_TABLE_COLUMNS};
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
    const BASE: &str = "body{margin:0}";
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
    fn by_id<'a>(doc: &Document, out: &'a Layout, id: &str) -> &'a BoxGeometry {
        out.boxes
            .iter()
            .find(|g| {
                doc.node(g.node)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("id"))
                    == Some(id)
            })
            .unwrap_or_else(|| panic!("no box {id}"))
    }
    fn border(doc: &Document, out: &Layout, id: &str) -> (f64, f64, f64, f64) {
        let Rect {
            x,
            y,
            width,
            height,
        } = by_id(doc, out, id).border;
        (x, y, width, height)
    }
    /// Each line's text and its first fragment's x and the line's y.
    fn texts(doc: &Document, out: &Layout) -> Vec<(String, f64, f64)> {
        out.lines.iter().map(|l| text(doc, l)).collect()
    }
    fn text(doc: &Document, line: &Line) -> (String, f64, f64) {
        let mut s = String::new();
        for f in &line.fragments {
            let source = doc.node(f.node).unwrap().text().unwrap();
            s.push_str(&source[f.range.clone()]);
        }
        let x = line.fragments.first().map_or(-1.0, |f| f.x);
        (s, x, line.y)
    }
    fn names(doc: &Document, out: &Layout) -> Vec<String> {
        out.boxes
            .iter()
            .map(|g| {
                doc.node(g.node)
                    .unwrap()
                    .element()
                    .unwrap()
                    .name
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_two_by_two_grid_lays_cells_side_by_side() {
        let html = "<table id=t><tr id=r1><td id=a>ab<td id=b>abcd\
                    <tr id=r2><td id=c>a b c<td id=d>x</table>";
        let (doc, out) = lay(html, "", 400.0);
        // HTML's sheet: 2 px spacing, 1 px padding; columns as wide as
        // their widest cell, 40 and 32 px of text.
        assert_eq!(border(&doc, &out, "a"), (2.0, 2.0, 42.0, 22.0));
        assert_eq!(border(&doc, &out, "b"), (46.0, 2.0, 34.0, 22.0));
        assert_eq!(border(&doc, &out, "c"), (2.0, 26.0, 42.0, 22.0));
        assert_eq!(border(&doc, &out, "d"), (46.0, 26.0, 34.0, 22.0));
        assert_eq!(border(&doc, &out, "r1"), (2.0, 2.0, 78.0, 22.0));
        assert_eq!(border(&doc, &out, "r2"), (2.0, 26.0, 78.0, 22.0));
        assert_eq!(border(&doc, &out, "t"), (0.0, 0.0, 82.0, 50.0));
        assert_eq!(by_id(&doc, &out, "a").content.x, 3.0);
        // Postorder, as every box's geometry is.
        assert_eq!(
            names(&doc, &out),
            ["td", "td", "tr", "td", "td", "tr", "tbody", "table", "body", "html"]
        );
        // Lines row by row, cells left to right: tree order.
        assert_eq!(
            texts(&doc, &out),
            [
                ("ab".into(), 3.0, 3.0),
                ("abcd".into(), 47.0, 3.0),
                ("a b c".into(), 3.0, 27.0),
                ("x".into(), 47.0, 27.0)
            ]
        );
        assert_eq!(out.height, 50.0);
        assert_eq!(out.overflow.width, 400.0);
    }
    #[test]
    fn roles_and_the_page_flag() {
        let html = "<table><caption>c</caption><colgroup><col></colgroup>\
                    <thead><tr><th>h</thead><tbody><tr><td>d</tbody><tfoot></tfoot></table>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, "");
        assert!(styled.has_tables());
        let roles: Vec<_> = (0..styled.boxes().len())
            .filter_map(|i| styled.table_role(crate::BoxId(i)))
            .collect();
        use TableRole::*;
        assert_eq!(
            roles,
            [
                Table,
                Caption,
                ColumnGroup,
                Column,
                RowGroup,
                Row,
                Cell,
                RowGroup,
                Row,
                Cell,
                RowGroup
            ]
        );
        let plain = td_html::document::parse("<p>no table").unwrap();
        assert!(!styled_plain(&plain).has_tables());
    }
    #[test]
    fn an_inline_table_is_laid_as_a_block_table() {
        let html = "<p>before <span id=t style=display:inline-table>\
                    <span style=display:table-cell id=c>x</span></span> after";
        let (doc, out) = lay(html, "p{margin:0}", 400.0);
        assert_eq!(border(&doc, &out, "t"), (0.0, 20.0, 8.0, 20.0));
        assert_eq!(border(&doc, &out, "c"), (0.0, 20.0, 8.0, 20.0));
        assert_eq!(
            texts(&doc, &out),
            [
                ("before".into(), 0.0, 0.0),
                ("x".into(), 0.0, 20.0),
                ("after".into(), 0.0, 40.0)
            ]
        );
    }
    #[test]
    fn hidden_inputs_make_no_box_anywhere() {
        // HTML §15.3.1's rule, in and out of tables, any case, and over an
        // author's display.
        let html = "<p>a<input id=x type=HIDDEN>b<input id=y type=text>\
                    <form><input id=z type=hidden style=display:inline></form>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, "input{display:block}");
        let boxed: Vec<_> = styled
            .boxes()
            .iter()
            .filter_map(|b| doc.node(b.node).and_then(|n| n.element()))
            .filter_map(|e| e.attribute("id"))
            .collect();
        assert_eq!(boxed, ["y"]);
    }
    fn styled_plain(doc: &Document) -> StyledDocument<'_> {
        StyledDocument::new(doc, &sheets(""), 12, Limits::default()).unwrap()
    }
    #[test]
    fn columns_take_their_content_widths_between_min_and_max() {
        let html = "<table id=t><tr><td id=a>aaaa aaaa aaaa<td id=b>bb</table>";
        let css = "table{border-spacing:0}td{padding:0}";
        // Room for both maxima: 112 and 16.
        let (doc, out) = lay(html, css, 400.0);
        assert_eq!(border(&doc, &out, "a").2, 112.0);
        assert_eq!(border(&doc, &out, "b").2, 16.0);
        assert_eq!(out.lines.len(), 2);
        // Less room: the auto table takes it, never below the minima (32,
        // 16), the columns between their minima and maxima (§17.5.2.2).
        let (doc, out) = lay(html, css, 90.0);
        assert_eq!(border(&doc, &out, "t").2, 90.0);
        assert_eq!(border(&doc, &out, "a").2 + border(&doc, &out, "b").2, 90.0);
        assert_eq!(border(&doc, &out, "b").2, 16.0);
        assert_eq!(
            texts(&doc, &out)
                .iter()
                .map(|t| t.0.as_str())
                .collect::<Vec<_>>(),
            ["aaaa aaaa", "aaaa", "bb"]
        );
        // Less than the minima: the table overflows at them.
        let (doc, out) = lay(html, css, 20.0);
        assert_eq!(border(&doc, &out, "t").2, 48.0);
        assert_eq!(border(&doc, &out, "a").2, 32.0);
        assert_eq!(out.overflow.width, 48.0);
        // A long word widens its column whatever the room.
        let (doc, out) = lay(
            "<table><tr><td id=a>a<td id=b>bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</table>",
            css,
            100.0,
        );
        assert_eq!(border(&doc, &out, "b").2, 400.0);
        assert_eq!(border(&doc, &out, "a").2, 8.0);
        assert_eq!(out.lines.len(), 2);
    }
    #[test]
    fn a_set_width_is_kept_and_extra_goes_by_maxima() {
        let html = "<table id=t><tr><td id=a>aaaaaa<td id=b>bb</table>";
        let css = "table{border-spacing:0}td{padding:0}";
        let (doc, out) = lay(html, &format!("{css}table{{width:256px}}"), 400.0);
        assert_eq!(border(&doc, &out, "t").2, 256.0);
        // 48 and 16 share 192 more by 3 to 1.
        assert_eq!(border(&doc, &out, "a").2, 192.0);
        assert_eq!(border(&doc, &out, "b").2, 64.0);
        let (doc, out) = lay(html, &format!("{css}table{{width:50%}}"), 400.0);
        assert_eq!(border(&doc, &out, "t").2, 200.0);
        // Never under the columns' minima.
        let (doc, out) = lay(html, &format!("{css}table{{width:10px}}"), 400.0);
        assert_eq!(border(&doc, &out, "t").2, 64.0);
        // Border-box from HTML's sheet: the border is in the width.
        let (doc, out) = lay(
            html,
            &format!("{css}table{{width:100px;border:5px solid;padding:3px}}"),
            400.0,
        );
        assert_eq!(border(&doc, &out, "t").2, 100.0);
        assert_eq!(border(&doc, &out, "a").0, 8.0);
        assert_eq!(border(&doc, &out, "a").2 + border(&doc, &out, "b").2, 84.0);
        // Auto margins center the table.
        let (doc, out) = lay(html, &format!("{css}table{{margin:0 auto}}"), 400.0);
        assert_eq!(border(&doc, &out, "t"), (168.0, 0.0, 64.0, 20.0));
        // A set left margin and an auto right: flush left after it.
        let (doc, out) = lay(html, &format!("{css}table{{margin:0 auto 0 10px}}"), 400.0);
        assert_eq!(border(&doc, &out, "t"), (10.0, 0.0, 64.0, 20.0));
        let (doc, out) = lay(html, &format!("{css}table{{margin:0 0 0 auto}}"), 400.0);
        assert_eq!(border(&doc, &out, "t").0, 336.0);
        // A cell's width is its column's preferred width: kept where it
        // fits, given up first of what the content needs.
        let (doc, out) = lay(html, &format!("{css}#b{{width:100px}}"), 400.0);
        assert_eq!(border(&doc, &out, "b").2, 100.0);
        assert_eq!(border(&doc, &out, "a").2, 48.0);
        let (doc, out) = lay(html, &format!("{css}#b{{width:100px}}"), 120.0);
        assert_eq!(border(&doc, &out, "b").2, 72.0);
        assert_eq!(border(&doc, &out, "a").2, 48.0);
        // A percentage column takes its share of the table.
        let (doc, out) = lay(
            html,
            &format!("{css}table{{width:400px}}#b{{width:75%}}"),
            400.0,
        );
        assert_eq!(border(&doc, &out, "b").2, 300.0);
        assert_eq!(border(&doc, &out, "a").2, 100.0);
    }
    #[test]
    fn colspan_and_rowspan_place_cells_and_share_widths() {
        let html = "<table id=t>\
            <tr><td id=a rowspan=2>a<td id=b colspan=2>bbbbbbbbbbbbbbbbbbbb\
            <tr><td id=c>c<td id=d>d\
            <tr><td id=e colspan=3>e</table>";
        let css = "table{border-spacing:4px}td{padding:0}";
        let (doc, out) = lay(html, css, 400.0);
        let (a, b, c, d, e) = (
            border(&doc, &out, "a"),
            border(&doc, &out, "b"),
            border(&doc, &out, "c"),
            border(&doc, &out, "d"),
            border(&doc, &out, "e"),
        );
        // c and d sit right of the rowspan, under b, whose 160 px spread
        // over their two columns with the spacing between.
        assert_eq!((c.1, d.1), (28.0, 28.0));
        assert_eq!(c.0, a.0 + a.2 + 4.0);
        assert_eq!(d.0 + d.2, b.0 + b.2);
        assert_eq!(b.2, 160.0);
        assert_eq!(c.2 + 4.0 + d.2, 160.0);
        // a spans both rows and the spacing between.
        assert_eq!(a.3, 44.0);
        assert_eq!(e.0, 4.0);
        assert_eq!(e.2, a.2 + 4.0 + 160.0);
        assert_eq!(e.1, 52.0);
        // HTML's rules for the attributes: 0 or junk is 1 for colspan; a
        // rowspan of 0, or one past its group, ends with the group.
        let html = "<table><tbody><tr><td id=a colspan=0>a<td id=b colspan=x>b\
            <td id=c rowspan=0>c<td id=d rowspan=9>d<tr><td id=e>e</tbody>\
            <tbody><tr><td id=f>f<td id=g>g<td id=h>h<td id=i>i</tbody></table>";
        let (doc, out) = lay(html, css, 400.0);
        assert_eq!(border(&doc, &out, "a").2, 8.0);
        assert_eq!(border(&doc, &out, "c").3, 44.0);
        assert_eq!(border(&doc, &out, "d").3, 44.0);
        assert_eq!(border(&doc, &out, "e").0, 4.0);
        // The next group starts free of them.
        assert_eq!(border(&doc, &out, "i").0, border(&doc, &out, "d").0);
        assert_eq!(border(&doc, &out, "f").1, 52.0);
    }
    #[test]
    fn a_rowspan_taller_than_its_rows_grows_the_last() {
        let html = "<table><tr id=r1><td id=a rowspan=2>a<br>b<br>c<br>d<td id=b>b\
                    <tr id=r2><td id=c>c</table>";
        let (doc, out) = lay(html, "table{border-spacing:0}td{padding:0}", 400.0);
        assert_eq!(border(&doc, &out, "a").3, 80.0);
        assert_eq!(border(&doc, &out, "r1").3, 20.0);
        // A row spans every column.
        assert_eq!(border(&doc, &out, "r2"), (0.0, 20.0, 16.0, 60.0));
        // c is in the middle of its row, as HTML's sheet aligns cells.
        let c = texts(&doc, &out).into_iter().find(|t| t.0 == "c").unwrap();
        assert_eq!(c.2, 40.0);
    }
    #[test]
    fn border_spacing_and_collapse() {
        let html = "<table id=t><tr><td id=a>a<td id=b>b<tr><td id=c>c<td id=d>d</table>";
        let (doc, out) = lay(html, "table{border-spacing:10px 4px}td{padding:0}", 400.0);
        assert_eq!(border(&doc, &out, "a"), (10.0, 4.0, 8.0, 20.0));
        assert_eq!(border(&doc, &out, "b"), (28.0, 4.0, 8.0, 20.0));
        assert_eq!(border(&doc, &out, "d"), (28.0, 28.0, 8.0, 20.0));
        assert_eq!(border(&doc, &out, "t"), (0.0, 0.0, 46.0, 52.0));
        // Collapsed: no spacing, no table padding, each border halved and
        // shared by the boxes either side of it.
        let (doc, out) = lay(
            html,
            "table{border-collapse:collapse;border:4px solid;padding:9px}\
             td{padding:0;border:2px solid}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "a"), (2.0, 2.0, 10.0, 22.0));
        assert_eq!(border(&doc, &out, "b"), (12.0, 2.0, 10.0, 22.0));
        assert_eq!(border(&doc, &out, "c").1, 24.0);
        assert_eq!(by_id(&doc, &out, "a").border_widths.left, 1.0);
        assert_eq!(by_id(&doc, &out, "t").border_widths.left, 2.0);
        assert_eq!(border(&doc, &out, "t"), (0.0, 0.0, 24.0, 48.0));
    }
    #[test]
    fn vertical_align_in_a_row_of_differing_heights() {
        let html = "<table><tr><td id=tall>a<br>b<br>c\
            <td id=top style=vertical-align:top>t\
            <td id=mid style=vertical-align:middle>m\
            <td id=bot style=vertical-align:bottom>o\
            <td id=base style=vertical-align:baseline>s\
            <td style=vertical-align:baseline><span style=font-size:40px;line-height:40px>L</span>\
            </table>";
        let (doc, out) = lay(html, "table{border-spacing:0}td{padding:0}", 400.0);
        let at = |s: &str| texts(&doc, &out).into_iter().find(|t| t.0 == s).unwrap().2;
        // The row is the tallest cell's, 60 px.
        assert_eq!(border(&doc, &out, "tall").3, 60.0);
        assert_eq!(border(&doc, &out, "top").3, 60.0);
        assert_eq!(at("t"), 0.0);
        assert_eq!(at("m"), 20.0);
        assert_eq!(at("o"), 40.0);
        // Baseline cells share the lowest first baseline, the 40 px line's
        // at 25: its font's 15 and half its 20 px leading.
        let line = |s: &str| {
            out.lines
                .iter()
                .find(|l| text(&doc, l).0 == s)
                .map(|l| l.y + l.baseline)
                .unwrap()
        };
        assert_eq!(line("s"), 25.0);
        assert_eq!(line("L"), 25.0);
        // The cells' boxes stay at the row's top, their content moved.
        assert_eq!(border(&doc, &out, "bot").1, 0.0);
        assert_eq!(by_id(&doc, &out, "bot").content.height, 60.0);
    }
    #[test]
    fn captions_lie_above_the_table_its_width() {
        let html = "<table id=t><caption id=c>a caption</caption><tr><td id=a>aaaaaaaaaaaaaaaaaaaaaaaaa</table>";
        let (doc, out) = lay(html, "td{padding:0}table{border-spacing:0}", 400.0);
        let caption = border(&doc, &out, "c");
        let table = border(&doc, &out, "t");
        assert_eq!(caption, (0.0, 0.0, 200.0, 20.0));
        assert_eq!(table, (0.0, 20.0, 200.0, 20.0));
        // Centred, as HTML's sheet has it.
        assert_eq!(texts(&doc, &out)[0], ("a caption".into(), 64.0, 0.0));
        assert_eq!(out.height, 40.0);
        // A caption's minimum, its longest word, widens the table; its
        // maximum does not (§17.4's wrapper).
        let (doc, out) = lay(
            "<table id=t><caption>a long caption here</caption><tr><td>a</table>",
            "td{padding:0}table{border-spacing:0}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "t").2, 56.0);
        // One after the rows lies below them, in tree order.
        let (doc, out) = lay(
            "<table id=t><tr><td>row</td></tr><caption id=c>below</caption></table>",
            "td{padding:0}table{border-spacing:0}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "t").1, 0.0);
        assert_eq!(border(&doc, &out, "c").1, 20.0);
        assert_eq!(
            texts(&doc, &out)
                .iter()
                .map(|t| t.0.as_str())
                .collect::<Vec<_>>(),
            ["row", "below"]
        );
    }
    #[test]
    fn css_tables_get_anonymous_tables_rows_and_cells() {
        let css = "td{padding:0}";
        // Cells with no row or table: one anonymous table around them.
        let (doc, out) = lay(
            "<div id=x style=display:table-cell>aa</div>\n<div id=y style=display:table-cell>b</div><p>after",
            css,
            400.0,
        );
        assert_eq!(border(&doc, &out, "x"), (0.0, 0.0, 16.0, 20.0));
        assert_eq!(border(&doc, &out, "y"), (16.0, 0.0, 8.0, 20.0));
        assert_eq!(texts(&doc, &out)[2].2, 36.0);
        // Rows with no table, and loose content in a table, a row and a
        // row group: anonymous rows and cells around it.
        let html = "<div id=t style=display:table>loose <i>text</i>\
            <div style=display:table-cell id=c1>c</div>\
            <div style=display:table-row id=r><div style=display:table-cell id=c2>d</div>more</div></div>";
        let (doc, out) = lay(html, css, 400.0);
        let t = texts(&doc, &out);
        assert_eq!(t[0], ("loose text".into(), 0.0, 0.0));
        assert_eq!(t[1].0, "c");
        assert_eq!(t[1].2, 0.0);
        let c1 = border(&doc, &out, "c1");
        assert_eq!(c1.1, 0.0);
        assert!(c1.0 >= 80.0);
        // The row: d in the first column, "more" in an anonymous second.
        let c2 = border(&doc, &out, "c2");
        assert_eq!((c2.0, c2.1), (0.0, 20.0));
        let more = t.iter().find(|t| t.0 == "more").unwrap();
        assert_eq!(more.1, c1.0);
        // Whitespace between table boxes makes no cell.
        let (doc, out) = lay(
            "<div style=display:table-row> <div style=display:table-cell id=a>a</div> \
             <div style=display:table-cell id=b>b</div> </div>",
            css,
            400.0,
        );
        assert_eq!(border(&doc, &out, "b").0, 8.0);
        assert_eq!(out.lines.len(), 2);
    }
    #[test]
    fn empty_cells_rows_and_tables() {
        let html =
            "<table id=t><tr id=r1><td id=a><td id=b>b<tr id=r2></tr><tr id=r3><td id=c>c</table>";
        let (doc, out) = lay(html, "td{padding:0}table{border-spacing:0}", 400.0);
        // The empty cell is its column's width, c's, and its row's height.
        assert_eq!(border(&doc, &out, "a"), (0.0, 0.0, 8.0, 20.0));
        assert_eq!(border(&doc, &out, "b").0, 8.0);
        assert_eq!(border(&doc, &out, "r2").3, 0.0);
        assert_eq!(border(&doc, &out, "r3").1, 20.0);
        let (doc, out) = lay("<table id=t></table><p id=p>x", "", 400.0);
        assert_eq!(border(&doc, &out, "t"), (0.0, 0.0, 0.0, 0.0));
        assert_eq!(texts(&doc, &out)[0].2, 16.0);
        // A form or hidden input in a table makes no row (HTML §15.3.9).
        let (doc, out) = lay(
            "<table id=t><form></form><input type=hidden><tr><td>a</table>",
            "td{padding:0}table{border-spacing:0}form,input{padding:5px}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "t").3, 20.0);
    }
    #[test]
    fn nested_tables_measure_and_lay_inside_their_cells() {
        let html = "<table id=outer><tr><td id=o1>x<td id=o2>\
            <table id=inner><tr><td id=i1>aaaa<td id=i2>bb</table></table>";
        let (doc, out) = lay(html, "", 400.0);
        let inner = border(&doc, &out, "inner");
        let o2 = by_id(&doc, &out, "o2").content;
        assert_eq!(inner.0, o2.x);
        assert_eq!(inner.2, 2.0 + 34.0 + 2.0 + 18.0 + 2.0);
        assert_eq!(o2.width, inner.2);
        // x is middle-aligned against the taller cell.
        let t = texts(&doc, &out);
        assert_eq!(t[0].0, "x");
        assert_eq!(t[0].2, by_id(&doc, &out, "o1").content.y + 3.0);
        assert_eq!(t[1].0, "aaaa");
        // Tables nested 60 deep lay out within MAX_DEPTH.
        let depth = 60;
        let html = format!(
            "{}deep{}",
            "<table><tr><td>".repeat(depth),
            "</table>".repeat(depth)
        );
        let doc = td_html::document::parse(&html).unwrap();
        let styled = styled(&doc, &html, "");
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].fragments[0].x, 3.0 * depth as f64);
    }
    #[test]
    fn fixed_layout_reads_the_first_row_and_measures_nothing() {
        let html = "<table id=t><colgroup><col id=k style=width:50px><col></colgroup>\
            <tr><td id=a>aaaaaaaaaaaaaaaa<td id=b style=width:30px>b<td id=c>c\
            <tr><td>x<td>y<td id=z>zzzzzzzzzzzzzzzzzzzzzzzzzzzz</table>";
        let css = "table{table-layout:fixed;width:200px;border-spacing:0}td{padding:0}";
        let (doc, out) = lay(html, css, 400.0);
        assert_eq!(border(&doc, &out, "t").2, 200.0);
        assert_eq!(border(&doc, &out, "a").2, 50.0);
        assert_eq!(border(&doc, &out, "b").2, 30.0);
        assert_eq!(border(&doc, &out, "c").2, 120.0);
        // Content wider than its column wraps or overflows it.
        assert_eq!(border(&doc, &out, "z").2, 120.0);
        // a's word is split at its column's width, three lines; z's two.
        assert_eq!(border(&doc, &out, "k"), (0.0, 0.0, 50.0, 100.0));
        // Without a width the automatic algorithm applies.
        let (doc, out) = lay(
            html,
            "table{table-layout:fixed;border-spacing:0}td{padding:0}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "z").2, 224.0);
    }
    #[test]
    fn effective_columns_merge_what_no_cell_divides() {
        let html = "<table id=t><tr><td id=a colspan=100>wide<tr><td>a<td>b<td id=c>c</table>";
        let (doc, out) = lay(html, "td{padding:0}", 400.0);
        // Three columns, 28 px and the 4 px between them under the wide
        // cell, and four gaps, not a hundred and one.
        assert_eq!(border(&doc, &out, "t").2, 28.0 + 4.0 * 2.0);
        assert_eq!(border(&doc, &out, "a").2, 32.0);
        let (doc, out) = lay(
            "<table id=t><colgroup id=g span=3></colgroup><col id=k style=width:20px>\
             <tr><td>a<td>b<td>c<td>d<td>e</table>",
            "td{padding:0}table{border-spacing:0}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "g"), (0.0, 0.0, 24.0, 20.0));
        assert_eq!(border(&doc, &out, "k"), (24.0, 0.0, 20.0, 20.0));
        assert_eq!(border(&doc, &out, "t").2, 52.0);
    }
    #[test]
    fn a_page_with_more_columns_than_the_bound_is_refused_by_name() {
        let css = "td{padding:0}";
        let cells = |n: usize| {
            format!(
                "<table><tr>{}</table>",
                "<td colspan=1000>x".repeat(n / 1000)
            )
        };
        let html = cells(MAX_TABLE_COLUMNS);
        let doc = td_html::document::parse(&html).unwrap();
        assert!(layout(
            &styled(&doc, &html, css),
            &mut Metrics,
            &Request::new(400.0, 600.0)
        )
        .is_ok());
        let html = cells(MAX_TABLE_COLUMNS + 1000);
        let doc = td_html::document::parse(&html).unwrap();
        assert!(matches!(
            layout(
                &styled(&doc, &html, css),
                &mut Metrics,
                &Request::new(400.0, 600.0)
            ),
            Err(Error::Limit("MAX_TABLE_COLUMNS"))
        ));
    }
    #[test]
    fn windows_equal_the_whole_layout_filtered() {
        let html = "<ul><li><table><tr><td rowspan=3>r<br>o<br>w<br>s<br>p<br>a<td>one\
            <tr><td><table><tr><td>in<br>ner<td style=vertical-align:bottom>b</table>\
            <tr><td><ol><li>item<li>two</ol></table><p>after a table</ul>\
            <div style=display:table-cell>x</div><p>end\
            <table><tr style=vertical-align:baseline><td>a<br>b<br>c\
            <td><span style=font-size:40px;line-height:40px>L</span><br>m<td><ul><li></ul></table>\
            <table><tr><td>1<br>2<br>3<br>4<br>5<br>6<br>7<br>8<br>9\
            <td style=vertical-align:bottom>z<td style=vertical-align:middle>m<br>n</table>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, "");
        let full = layout(&styled, &mut Metrics, &Request::new(300.0, 600.0)).unwrap();
        assert!(full.lines.len() > 20);
        // Two items' and one lineless marker in a cell.
        assert_eq!(full.markers.len(), 4);
        assert!(full.markers.iter().any(|m| m.line.is_none()));
        for start in (0..full.height as u32 + 20).step_by(7) {
            for size in [0.0, 1.0, 13.0, 40.0, 500.0] {
                let start = f64::from(start);
                let window = start..start + size;
                let request = Request {
                    window: Some(window.clone()),
                    ..Request::new(300.0, 600.0)
                };
                let part = layout(&styled, &mut Metrics, &request).unwrap();
                let meets = |y: f64, h: f64| size > 0.0 && y + h > window.start && y < window.end;
                let lines: Vec<_> = full.lines.iter().filter(|l| meets(l.y, l.height)).collect();
                assert_eq!(part.lines.iter().collect::<Vec<_>>(), lines, "{window:?}");
                let kept = |m: &crate::list::MarkerFragment| match m.line {
                    Some(i) => meets(full.lines[i as usize].y, full.lines[i as usize].height),
                    None => meets(m.y, m.height),
                };
                let markers: Vec<_> = full.markers.iter().filter(|m| kept(m)).collect();
                assert_eq!(part.markers.len(), markers.len(), "{window:?}");
                for (p, f) in part.markers.iter().zip(markers) {
                    assert_eq!((p.item, p.y, p.x), (f.item, f.y, f.x));
                    if let Some(i) = p.line {
                        let line = &part.lines[i as usize];
                        assert_eq!(line, &full.lines[f.line.unwrap() as usize]);
                    }
                }
                assert_eq!(
                    part.boxes.iter().collect::<Vec<_>>(),
                    full.boxes.iter().collect::<Vec<_>>()
                );
                assert_eq!(part.height, full.height);
                assert_eq!(part.overflow, full.overflow);
                assert_eq!(part.line_count, full.line_count);
            }
        }
    }
    #[test]
    fn fragments_keep_their_dom_bytes() {
        let html = "<table><tr><td>héllo wörld<td>  spaced\tout  <tr><td colspan=2>a&amp;b</table>";
        let (doc, out) = lay(html, "", 60.0);
        let mut seen = String::new();
        for line in &out.lines {
            for f in &line.fragments {
                let text = doc.node(f.node).unwrap().text().unwrap();
                let bytes = text.get(f.range.clone()).unwrap();
                seen.push_str(bytes);
                seen.push('|');
            }
        }
        assert_eq!(seen, "héllo|wörld|spaced|out|a&b|");
        // A collapsed tab keeps its byte.
        let (doc, out) = lay(html, "", 400.0);
        let tab = out
            .lines
            .iter()
            .flat_map(|l| l.fragments.iter())
            .find(|f| f.display == crate::inline::DisplayText::Space)
            .unwrap();
        let text = doc.node(tab.node).unwrap().text().unwrap();
        assert_eq!(&text[tab.range.clone()], "\t");
    }
    #[test]
    fn images_in_cells_measure_by_their_natural_size() {
        let html = "<table id=t><tr><td id=a><img width=100 height=50><td id=b>\
                    <img style=max-width:100% width=200 height=50></table>";
        let (doc, out) = lay(html, "td{padding:0}table{border-spacing:0}", 1000.0);
        assert_eq!(border(&doc, &out, "a").2, 100.0);
        assert_eq!(border(&doc, &out, "b").2, 200.0);
        // Squeezed, a percentage-limited image gives way first.
        let (doc, out) = lay(html, "td{padding:0}table{border-spacing:0}", 150.0);
        assert_eq!(border(&doc, &out, "a").2, 100.0);
        assert_eq!(border(&doc, &out, "b").2, 50.0);
    }
    #[test]
    fn deep_and_hostile_shapes_are_bounded() {
        // Nested past td-html's 256 levels the page is not parsed; under a
        // lowered MAX_DEPTH, layout refuses by name, not by the stack.
        let nest = |depth: usize| {
            format!(
                "{}deep{}",
                "<table><tr><td>".repeat(depth),
                "</table>".repeat(depth)
            )
        };
        assert!(td_html::document::parse(&nest(70)).is_err());
        let html = nest(30);
        let doc = td_html::document::parse(&html).unwrap();
        let shallow = Limits {
            depth: 100,
            ..Limits::default()
        };
        let styled_deep =
            StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        assert!(layout(&styled_deep, &mut Metrics, &Request::new(400.0, 600.0)).is_ok());
        assert!(matches!(
            StyledDocument::new(&doc, &sheets(""), html.len(), shallow).and_then(|s| layout(
                &s,
                &mut Metrics,
                &Request::new(400.0, 600.0)
            )
            .map(|_| ())),
            Err(Error::Limit("MAX_DEPTH"))
        ));
        // A long text in a cell nested 30 deep is measured once a level
        // and charged each time: refused by the work ceiling when that is
        // lowered under it.
        let text = "word ".repeat(20_000);
        let html = format!(
            "{}{text}{}",
            "<table><tr><td>".repeat(30),
            "</table>".repeat(30)
        );
        let doc = td_html::document::parse(&html).unwrap();
        let styled = styled(&doc, &html, "");
        let out = layout(&styled, &mut Metrics, &Request::new(800.0, 600.0)).unwrap();
        assert!(out.work > 30 * 100_000, "{}", out.work);
        assert!(out.work < 40 * 100_000, "{}", out.work);
        let limited = StyledDocument::new(
            &doc,
            &sheets(""),
            html.len(),
            Limits {
                work: out.work - 1,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(matches!(
            layout(&limited, &mut Metrics, &Request::new(800.0, 600.0)),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
    }
    fn widths(doc: &Document, out: &Layout, ids: &[&str]) -> Vec<f64> {
        ids.iter().map(|id| border(doc, out, id).2).collect()
    }
    fn refusal(html: &str, css: &str, limits: Limits) -> Option<Error> {
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(css), html.len(), limits).unwrap();
        layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).err()
    }
    #[test]
    fn cells_fit_their_measured_width_at_any_table_x() {
        // At a fractional x, a width taken as the difference of two x's
        // fell an ulp short of the columns' and wrapped each cell.
        let html = "<div id=d><table><tr><td>Name of thing<td>Value here\
                    <td>More text here<td>last cell text</table></div>";
        for step in 0..96 {
            let css = format!("#d{{padding:0 {}px}}", f64::from(step) * 0.37);
            let (_, out) = lay(html, &css, 1280.0);
            assert_eq!(out.lines.len(), 4, "{css}");
        }
        let (_, out) = lay(html, "#d{padding:0 0.4em}", 1280.0);
        assert_eq!(out.lines.len(), 4);
    }
    #[test]
    fn a_set_width_is_preferred_and_min_width_a_floor() {
        let css = "table{border-spacing:0}td{padding:0}";
        // Ten words wrap at a cell's 100 px, which the table keeps.
        let words = "aaa ".repeat(10);
        let (doc, out) = lay(
            &format!("<table id=t><tr><td id=a>{words}</table>"),
            &format!("{css}td{{width:100px}}"),
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["t", "a"]), [100.0, 100.0]);
        assert_eq!(out.lines.len(), 4);
        // A narrower table wins over its cell's width.
        let (doc, out) = lay(
            "<table id=t><tr><td id=a>aa</table>",
            &format!("{css}table{{width:100px}}td{{width:300px}}"),
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["t", "a"]), [100.0, 100.0]);
        // A wide table gives its extra to the auto column, not the set one.
        let (doc, out) = lay(
            "<table id=t><tr><td id=a>a<td id=b>b</table>",
            &format!("{css}table{{width:100%}}#a{{width:50px}}"),
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["a", "b"]), [50.0, 350.0]);
        // Bootstrap's media object: a body 10000 px wide beside an image
        // takes what the image leaves.
        let (doc, out) = lay(
            "<div class=m><div class=l id=l><img width=64 height=64></div>\
             <div class=b id=b>text</div></div>",
            ".m{display:table}.l,.b{display:table-cell}.b{width:10000px}",
            800.0,
        );
        assert_eq!(widths(&doc, &out, &["l", "b"]), [64.0, 736.0]);
        // Squeezed, an auto column gives way before a set one.
        let html = "<table id=t><tr><td id=a>aa aa aa aa<td id=b>b<td id=c>c</table>";
        let set = format!("{css}#b{{width:100px}}#c{{width:20%}}");
        let (doc, out) = lay(html, &set, 150.0);
        assert_eq!(widths(&doc, &out, &["a", "b", "c"]), [20.0, 100.0, 30.0]);
        // then the set one; the percentage keeps its share of the table.
        let (doc, out) = lay(html, &set, 60.0);
        assert_eq!(widths(&doc, &out, &["a", "b", "c"]), [16.0, 32.0, 12.0]);
        let (doc, out) = lay(html, &set, 20.0);
        assert_eq!(widths(&doc, &out, &["a", "b", "c"]), [16.0, 8.0, 8.0]);
        // A wider table's extra goes to the auto columns alone, by their
        // maxima, 88 and 8.
        let (doc, out) = lay(
            html,
            &format!("{css}table{{width:300px}}#b{{width:100px}}"),
            400.0,
        );
        assert_eq!(
            widths(&doc, &out, &["a", "b", "c"]),
            [183.328125, 100.0, 16.671875]
        );
        // A cell's width counts its padding and border, as its box-sizing
        // says.
        let (doc, out) = lay(
            "<table><tr><td id=a>a</table>",
            "table{border-spacing:0}td{width:100px;padding:5px;border:1px solid}",
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["a"]), [112.0]);
        let (doc, out) = lay(
            "<table><tr><td id=a>a</table>",
            "table{border-spacing:0}td{width:100px;padding:5px;box-sizing:border-box}",
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["a"]), [100.0]);
        // min-width raises a cell's least width.
        let (doc, out) = lay(
            "<table><tr><td id=a>a<td id=b>b</table>",
            &format!("{css}#a{{min-width:100px}}"),
            50.0,
        );
        assert_eq!(widths(&doc, &out, &["a", "b"]), [100.0, 8.0]);
        // A spanning cell's width and percentage reach its columns.
        let (doc, out) = lay(
            "<table id=t><tr><td colspan=2 style=width:200px>x<tr><td id=a>a<td id=b>b</table>",
            css,
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["t", "a", "b"]), [200.0, 100.0, 100.0]);
        let (doc, out) = lay(
            "<table id=t><tr><td colspan=2 style=width:50%>x<td id=c>c\
             <tr><td id=a>a<td id=b>b</table>",
            &format!("{css}table{{width:400px}}"),
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["a", "b", "c"]), [100.0, 100.0, 200.0]);
        // Percentage columns alone share spare room by percentage.
        let (doc, out) = lay(
            "<table><tr><td id=a style=width:10%>a<td id=b style=width:30%>b</table>",
            &format!("{css}table{{width:400px}}"),
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["a", "b"]), [100.0, 300.0]);
    }
    #[test]
    fn a_percentage_column_asks_the_table_its_share_needs() {
        let css = "table{border-spacing:0}td{padding:0}";
        // 48 px at a quarter: a 192 px table, if there is room.
        let (doc, out) = lay(
            "<table id=t><tr><td id=a style=width:25%>aaaaaa<td id=b>b</table>",
            css,
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["t", "a", "b"]), [192.0, 48.0, 144.0]);
        // The rest at three quarters: 288 px asks 384.
        let (doc, out) = lay(
            &format!(
                "<table id=t><tr><td id=a style=width:25%>a<td id=b>{}</table>",
                "b".repeat(36)
            ),
            css,
            800.0,
        );
        assert_eq!(widths(&doc, &out, &["t", "a", "b"]), [384.0, 96.0, 288.0]);
        // A spanning cell's maximum spreads over its columns.
        let (doc, out) = lay(
            "<table id=t><tr><td colspan=2>aa aa aa aa<tr><td>a<td>b</table>",
            css,
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["t"]), [88.0]);
        // Spanning cells spread narrowest first: the wide one shares what
        // is left by the maxima the narrow one gave, where tree order
        // would give each column a third.
        let (doc, out) = lay(
            "<table id=t><tr><td colspan=3>aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\
             <tr><td colspan=2>aaaaaaaaaa<td id=c>a<tr><td id=a>a<td id=b>a<td>a</table>",
            css,
            400.0,
        );
        let w = widths(&doc, &out, &["t", "a", "b", "c"]);
        assert_eq!((w[0], w[1] + w[2] + w[3]), (240.0, 240.0));
        assert_eq!(w[1], w[2]);
        assert!(w[3] < 40.0, "{w:?}");
    }
    #[test]
    fn columns_fill_the_table_exactly() {
        // A 100% table after a fractional margin ends at its container's
        // edge, not past it.
        let (doc, out) = lay(
            "<div style=margin-left:3.3px><table id=t><tr><td>a<td>b<td>c</table></div>",
            "table{width:100%}",
            400.0,
        );
        let t = border(&doc, &out, "t");
        assert!(t.0 + t.2 <= 400.0, "{t:?}");
        // Shrinking three columns by shares that round: their widths and
        // the spacing make the table's.
        let html = "<table id=t><tr><td id=a>aaa aaa aaa<td id=b>aa aa aa aa aa\
                    <td id=c>a a a a a a a</table>";
        for room in [101.0, 102.5, 137.3, 150.015625] {
            let (doc, out) = lay(html, "td{padding:0}", room);
            let w = widths(&doc, &out, &["t", "a", "b", "c"]);
            assert_eq!(w[1] + w[2] + w[3] + 4.0 * 2.0, w[0], "{room}");
            assert!(w[0] <= room, "{room}");
        }
    }
    #[test]
    fn vertical_align_takes_a_set_height_into_account() {
        let html = "<table><tr><td id=a style=height:100px>x<td>y</table>";
        let css = "table{border-spacing:0}td{padding:0}";
        let at = |css: &str| {
            let (doc, out) = lay(html, css, 400.0);
            let t = texts(&doc, &out);
            (t[0].2, t[1].2)
        };
        // HTML's sheet aligns cells in the middle: y's line and x's alike,
        // x's by its content, not the height it is set.
        assert_eq!(at(css), (40.0, 40.0));
        assert_eq!(at(&format!("{css}#a{{vertical-align:top}}")), (0.0, 40.0));
        assert_eq!(
            at(&format!("{css}#a{{vertical-align:bottom}}")),
            (80.0, 40.0)
        );
        // Row, cell and table heights.
        let (doc, out) = lay(
            "<table id=t><tr id=r style=height:50px><td id=a>a<tr><td id=b style=height:30px>b</table>",
            &format!("{css}td{{vertical-align:top}}"),
            400.0,
        );
        assert_eq!(border(&doc, &out, "r").3, 50.0);
        assert_eq!(border(&doc, &out, "a").3, 50.0);
        assert_eq!(border(&doc, &out, "b"), (0.0, 50.0, 8.0, 30.0));
        assert_eq!(border(&doc, &out, "t").3, 80.0);
        let (doc, out) = lay(
            "<table id=t><tr><td id=a>a</table>",
            &format!("{css}table{{height:200px}}"),
            400.0,
        );
        assert_eq!(border(&doc, &out, "t").3, 200.0);
        // Baseline cells: the row holds the lowest baseline and the
        // deepest descent below it.
        let (doc, out) = lay(
            "<table><tr id=r><td><span style=font-size:40px;line-height:40px>L</span>\
             <td>a<br>b</table>",
            &format!("{css}td{{vertical-align:baseline}}"),
            400.0,
        );
        assert_eq!(border(&doc, &out, "r").3, 50.0);
        // A table moved by position: relative moves its cells and lines.
        let (doc, out) = lay(
            "<table id=t><tr><td id=a>a</table>",
            &format!("{css}table{{position:relative;left:5px;top:10px}}"),
            400.0,
        );
        assert_eq!(border(&doc, &out, "t").0, 5.0);
        assert_eq!(border(&doc, &out, "a"), (5.0, 10.0, 8.0, 20.0));
        assert_eq!(texts(&doc, &out)[0], ("a".into(), 5.0, 10.0));
    }
    #[test]
    fn a_colspan_over_a_held_column_keeps_its_rowspan() {
        // HTML §4.9.12: c overlaps b's rowspan, which still holds row 3.
        let (doc, out) = lay(
            "<table><tr><td id=a>a<td id=b rowspan=3>b<tr><td colspan=2>c\
             <tr><td id=d>d<td id=e>e</table>",
            "td{padding:0}",
            400.0,
        );
        assert!(border(&doc, &out, "e").0 > border(&doc, &out, "b").0);
        assert_eq!(border(&doc, &out, "d").0, border(&doc, &out, "a").0);
    }
    #[test]
    fn span_attributes_clamp_at_their_bounds() {
        let css = "td{padding:0}table{border-spacing:0}";
        // colspan=1000 and 1001 both take 1000 columns, as 999 and one.
        let (doc, out) = lay(
            "<table><tr><td colspan=1000>a<td id=x>x<tr><td colspan=999>b<td>c<td id=y>y\
             <tr><td colspan=1001>d<td id=z>z</table>",
            css,
            400.0,
        );
        let x = border(&doc, &out, "x").0;
        assert_eq!(border(&doc, &out, "y").0, x);
        assert_eq!(border(&doc, &out, "z").0, x);
        // rowspan=65534 holds 65534 rows and 65535 no more.
        let span = |n: u32| {
            format!(
                "<table><tr><td rowspan={n}>a<td>b{}<tr><td id=x>x<tr><td id=y>y</table>",
                "<tr>".repeat(65532)
            )
        };
        for (n, x) in [(65533, 0.0), (65534, 8.0), (65535, 8.0)] {
            let html = span(n);
            let (doc, out) = lay(&html, css, 400.0);
            assert_eq!(border(&doc, &out, "x").0, x, "{n}");
            assert_eq!(border(&doc, &out, "y").0, 0.0, "{n}");
        }
    }
    #[test]
    fn inside_markers_are_measured_with_their_line() {
        let (doc, out) = lay(
            "<table><tr><td id=a><ul><li>ab</ul><td>x</table>",
            "td{padding:0}table{border-spacing:0}ul{margin:0;padding:0;list-style-position:inside}",
            400.0,
        );
        // The marker's two scalars and the item's two on one line.
        assert_eq!(border(&doc, &out, "a").2, 32.0);
        assert_eq!(out.lines.len(), 2);
        // Before a block it is a line of its own.
        let (doc, out) = lay(
            "<table><tr><td id=a><ul><li><p>a</ul><td>x</table>",
            "td{padding:0}table{border-spacing:0}p{margin:0}\
             ul{margin:0;padding:0;list-style-position:inside}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "a").2, 16.0);
    }
    #[test]
    fn the_sheet_borders_tables_with_a_border_attribute() {
        let (doc, out) = lay(
            "<table id=t border=1><tr id=r><td id=a>a</table>\
             <table id=u border=0><tr><td id=b>b</table><table id=v><tr><td id=c>c</table>",
            "",
            400.0,
        );
        let a = by_id(&doc, &out, "a");
        assert_eq!(a.border_widths.left, 1.0);
        assert_eq!(by_id(&doc, &out, "t").border_widths.left, 1.0);
        for id in ["b", "c"] {
            assert_eq!(by_id(&doc, &out, id).border_widths.left, 0.0);
        }
        assert_eq!(by_id(&doc, &out, "u").border_widths.left, 0.0);
        // Groups and rows inherit their table's border colour.
        let html = "<table id=t border=1 style=border-color:red><tr id=r><td>a</table>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, "");
        let red = |id: &str| {
            let b = styled
                .boxes()
                .iter()
                .find(|b| {
                    doc.node(b.node)
                        .and_then(|n| n.element())
                        .and_then(|e| e.attribute("id"))
                        == Some(id)
                })
                .unwrap();
            styled
                .style(b.style)
                .unwrap()
                .get(td_css::Property::BorderTopColor)
                .map(|v| format!("{v:?}"))
        };
        assert_eq!(red("r"), red("t"));
    }
    #[test]
    fn fixed_layout_spreads_by_width_and_measures_its_columns() {
        let css = "table{table-layout:fixed;width:400px;border-spacing:0}td{padding:0}";
        let (doc, out) = lay(
            "<table><tr><td id=a style=width:100px>a<td id=b style=width:50px>b</table>",
            css,
            400.0,
        );
        let (a, b) = (border(&doc, &out, "a").2, border(&doc, &out, "b").2);
        assert_eq!(a + b, 400.0);
        assert!((a - 2.0 * b).abs() < 0.1, "{a} {b}");
        // Only the first row sets widths.
        let (doc, out) = lay(
            "<table><tr><td id=a>a<td id=b>b<tr><td style=width:300px>c<td>d</table>",
            css,
            400.0,
        );
        assert_eq!(widths(&doc, &out, &["a", "b"]), [200.0, 200.0]);
        // A fixed table measures as its columns where they are wider.
        let (doc, out) = lay(
            "<table><tr><td id=o><table style=table-layout:fixed;width:50px>\
             <col style=width:200px><tr><td>x</table><td>y</table>",
            "table{border-spacing:0}td{padding:0}",
            400.0,
        );
        assert_eq!(border(&doc, &out, "o").2, 200.0);
        // Percentage padding is zero, measured and laid alike.
        let (doc, out) = lay(
            "<table><tr><td id=a>aaaa</table>",
            "table{border-spacing:0}td{padding:0 10%}",
            400.0,
        );
        let a = by_id(&doc, &out, "a");
        assert_eq!((a.content.x, a.content.width), (a.border.x, 32.0));
    }
    #[test]
    fn a_window_filters_rows_only_once_no_rowspan_is_open() {
        // Filtering after the first row would drop lines the bottom-aligned
        // rowspan still has to move and leave its record pointing past them.
        let html = "<table><tr><td rowspan=3 style=vertical-align:bottom>s<td>a<br>a<br>a\
                    <tr><td>b<br>b<br>b<tr><td>c<br>c<br>c</table>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = styled(&doc, html, "");
        let request = Request {
            window: Some(100.0..140.0),
            ..Request::new(400.0, 600.0)
        };
        let out = layout(&styled, &mut Metrics, &request).unwrap();
        let seen: Vec<_> = texts(&doc, &out).into_iter().map(|t| (t.0, t.2)).collect();
        assert_eq!(
            seen,
            [("b".into(), 87.0), ("b".into(), 107.0), ("c".into(), 131.0)]
        );
    }
    #[test]
    fn a_window_far_down_one_cell_keeps_no_more_capacity_than_its_lines() {
        let html = format!("<table><tr><td><pre>{}</pre></table>", "a\n".repeat(20_000));
        let doc = td_html::document::parse(&html).unwrap();
        let styled = styled(&doc, &html, "");
        let request = Request {
            window: Some(300_000.0..300_100.0),
            ..Request::new(400.0, 600.0)
        };
        let out = layout(&styled, &mut Metrics, &request).unwrap();
        assert_eq!(out.lines.len(), 6);
        assert!(out.lines.capacity() < 64, "{}", out.lines.capacity());
        // Its 15,000 lines above the window are charged while held and
        // given back, so the output does not bar a next layout.
        assert!(out.output_bytes < 1 << 20, "{}", out.output_bytes);
    }
    #[test]
    fn an_auto_table_never_overflows_its_room_by_rounding() {
        let (_, out) = lay(
            &format!(
                "<div style=margin-left:3.3px><table><tr><td>{}</table></div>",
                "word ".repeat(300)
            ),
            "",
            800.0,
        );
        assert_eq!(out.overflow.width, 800.0);
    }
    #[test]
    fn a_baseline_cell_with_a_set_height_is_that_tall() {
        let (doc, out) = lay(
            "<table><tr id=r><td style=vertical-align:baseline;height:100px>a\
             <td style=vertical-align:baseline>\
             <span style=font-size:40px;line-height:40px>L</span></table>",
            "",
            400.0,
        );
        // 100 px and its padding, as browsers have it, not that plus the
        // 10 px its line moves down to the 40 px line's baseline.
        assert_eq!(border(&doc, &out, "r").3, 102.0);
    }
    #[test]
    fn a_window_holds_few_of_a_tables_lines_at_once() {
        let charged = |html: &str, window: Option<std::ops::Range<f64>>| {
            let doc = td_html::document::parse(html).unwrap();
            let styled = styled(&doc, html, "");
            let request = Request {
                window,
                ..Request::new(400.0, 600.0)
            };
            let out = layout(&styled, &mut Metrics, &request).unwrap();
            (out.page_bytes - styled.charges().held(), out.lines.len())
        };
        // One cell's 20,000 lines: those below the window are dropped as
        // they are laid, since alignment only moves them down.
        let cell = format!("<table><tr><td><pre>{}</pre></table>", "a\n".repeat(20_000));
        let (whole, lines) = charged(&cell, None);
        assert!(lines > 20_000);
        let (part, lines) = charged(&cell, Some(0.0..100.0));
        assert_eq!(lines, 5);
        assert!(part * 20 < whole, "{part} {whole}");
        // 20,000 rows: those above the window are dropped as each row
        // ends, so a window at the end holds no more than one at the top.
        // Each cell's first line is kept as it is laid, so without that a
        // window would hold every line of the table; the lines are a
        // quarter of what the whole layout charges.
        let rows = format!("<table>{}</table>", "<tr><td>a".repeat(20_000));
        let (whole, _) = charged(&rows, None);
        let (top, _) = charged(&rows, Some(0.0..100.0));
        let (end, lines) = charged(&rows, Some(479_000.0..479_100.0));
        assert_eq!(lines, 5);
        assert!(end < top + 64 * 1024, "{top} {end}");
        assert!(end * 5 < whole * 4, "{end} {whole}");
    }
    #[test]
    fn measuring_charges_its_scratch_once_whatever_the_nesting() {
        let page = |depth: usize| {
            format!(
                "{}{}{}",
                "<table><tr><td>".repeat(depth),
                "<div></div>".repeat(2000),
                "</table>".repeat(depth)
            )
        };
        let charged = |depth: usize| {
            let html = page(depth);
            let doc = td_html::document::parse(&html).unwrap();
            let styled = styled(&doc, &html, "");
            let out = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
            (out.page_bytes - styled.charges().held(), out.work)
        };
        let ((one, work), (deep, _)) = (charged(1), charged(20));
        // Each div measured costs eight units and laid one, besides the
        // walks that find it.
        assert!(work > 2000 * 9, "{work}");
        // So does each inline image measured.
        let html = format!(
            "<table><tr><td>{}</table>",
            "<img width=5 height=5>".repeat(2000)
        );
        let doc = td_html::document::parse(&html).unwrap();
        let styled = styled(&doc, &html, "");
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        assert!(out.work > 2000 * 9, "{}", out.work);
        // 2,000 events are 32 KiB a measure; twenty levels charge them
        // once, not twenty times.
        assert!(deep < one + 64 * 1024, "{one} {deep}");
    }
    #[test]
    fn nested_css_tables_spend_four_levels_of_depth_each() {
        // A cell in a cell is an anonymous table and row around each: four
        // of MAX_DEPTH a level, though the page wrote one element.
        let nest = |n: usize| {
            format!(
                "{}x{}",
                "<div style=display:table-cell>".repeat(n),
                "</div>".repeat(n)
            )
        };
        assert_eq!(refusal(&nest(63), "", Limits::default()), None);
        assert!(matches!(
            refusal(&nest(64), "", Limits::default()),
            Some(Error::Limit("MAX_DEPTH"))
        ));
    }
}
