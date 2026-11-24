//! Floats (DESIGN.md §5, "Floats"). Each scalar advances 8 px and a
//! 16 px line is 20 px tall, so text widths and line tops are worked by
//! hand.
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
            let doc = td_html::document::parse(html).unwrap();
            let out = {
                let styled =
                    StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
                layout(&styled, &mut Metrics, &Request::new(width, 600.0)).unwrap()
            };
            Self { doc, out }
        }
        fn border(&self, id: &str) -> Rect {
            self.out
                .boxes
                .iter()
                .find(|g| {
                    self.doc
                        .node(g.node)
                        .and_then(|n| n.element())
                        .and_then(|e| e.attribute("id"))
                        == Some(id)
                })
                .unwrap()
                .border
        }
        /// Each line's top, and its first fragment's x and text.
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

    /// Text beside a left float starts at its edge; below it, at the block's.
    #[test]
    fn lines_beside_a_left_float_start_at_its_edge() {
        let laid = Laid::new(
            "<div id=f></div><p>aaaa bbbb cccc dddd",
            "#f{float:left;width:40px;height:30px}",
            120.0,
        );
        assert_eq!(rect(laid.border("f")), (0.0, 0.0, 40.0, 30.0));
        // 80 px beside it holds two words; the line at 20 still meets it.
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 40.0, "aaaa bbbb".into()),
                (20.0, 40.0, "cccc dddd".into()),
            ]
        );
        let laid = Laid::new(
            "<div id=f></div><p>aaaa bbbb cccc dddd eeee ffff",
            "#f{float:left;width:40px;height:30px}",
            120.0,
        );
        assert_eq!(laid.lines()[2], (40.0, 0.0, "eeee ffff".into()));
    }

    /// A right float takes its room from the line's end, and stands at the
    /// container's right edge.
    #[test]
    fn a_right_float_shortens_lines_at_their_end() {
        let laid = Laid::new(
            "<div id=f></div><p>aaaa bbbb cccc",
            "#f{float:right;width:40px;height:20px}",
            120.0,
        );
        assert_eq!(rect(laid.border("f")), (80.0, 0.0, 40.0, 20.0));
        assert_eq!(
            laid.lines(),
            vec![(0.0, 0.0, "aaaa bbbb".into()), (20.0, 0.0, "cccc".into())]
        );
    }

    /// Met within a line, a float goes on it where it fits beside the
    /// line's content, which then moves past it; else at the next line's top.
    #[test]
    fn a_float_met_in_a_line_goes_on_it_or_the_next() {
        let laid = Laid::new(
            "<p>aa <img id=f width=30 height=20> bb cc",
            "#f{float:left}",
            200.0,
        );
        assert_eq!(rect(laid.border("f")), (0.0, 0.0, 30.0, 20.0));
        assert_eq!(laid.lines(), vec![(0.0, 30.0, "aa bb cc".into())]);
        // `aaaaaaaaaa ` (88 px) leaves 32 px of 120 on its line: the float of
        // 40 waits for the next line's top, and `bbbb` still takes this line.
        let laid = Laid::new(
            "<p>aaaaaaaaaa <img id=f width=40 height=40>bbbb cccc dddd eeee",
            "#f{float:left}",
            120.0,
        );
        assert_eq!(rect(laid.border("f")), (0.0, 20.0, 40.0, 40.0));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaaaaaaaa bbbb".into()),
                (20.0, 40.0, "cccc dddd".into()),
                (40.0, 40.0, "eeee".into()),
            ]
        );
    }

    /// Floats on one side stack beside each other; one that does not fit
    /// beside them goes down to where it does (CSS 2.2 §9.5.1).
    #[test]
    fn floats_stack_and_move_down_where_they_do_not_fit() {
        let laid = Laid::new(
            "<div id=a></div><div id=b></div><div id=c></div><div id=d></div>",
            "div{float:left;height:20px}#a{width:50px;height:40px}#b{width:40px}\
             #c{width:40px}#d{float:right;width:20px}",
            100.0,
        );
        assert_eq!(rect(laid.border("a")), (0.0, 0.0, 50.0, 40.0));
        assert_eq!(rect(laid.border("b")), (50.0, 0.0, 40.0, 20.0));
        // 10 px is left beside a and b: c goes below b, beside a.
        assert_eq!(rect(laid.border("c")), (50.0, 20.0, 40.0, 20.0));
        // No higher than c's top, where 10 px is left beside a and c: below
        // them both.
        assert_eq!(rect(laid.border("d")), (80.0, 40.0, 20.0, 20.0));
        // Room beside an earlier float at an earlier float's top.
        let laid = Laid::new(
            "<div id=a></div><div id=b></div><div id=d></div>",
            "div{height:20px}#a{float:left;width:50px;height:40px}         #b{float:left;clear:left;width:10px}#d{float:right;width:20px}",
            100.0,
        );
        assert_eq!(rect(laid.border("b")), (0.0, 40.0, 10.0, 20.0));
        assert_eq!(rect(laid.border("d")), (80.0, 40.0, 20.0, 20.0));
    }

    /// clear moves a block's border box below the floats on its sides, and
    /// a float's own clear below those before it.
    #[test]
    fn clear_moves_below_the_floats_on_its_sides() {
        let css = "#l{float:left;width:20px;height:50px}#r{float:right;width:20px;height:30px}\
                   .b{height:10px}";
        let laid = Laid::new(
            "<div id=l></div><div id=r></div><div id=n class=b></div>\
             <div id=cr class=b style=clear:right></div><div id=cb class=b style=clear:both></div>",
            css,
            100.0,
        );
        assert_eq!(laid.border("n").y, 0.0);
        assert_eq!(laid.border("cr").y, 30.0);
        assert_eq!(laid.border("cb").y, 50.0);
        let laid = Laid::new(
            "<div id=l></div><div id=f style=float:left;clear:left;width:10px;height:5px></div>",
            css,
            100.0,
        );
        assert_eq!(rect(laid.border("f")), (0.0, 50.0, 10.0, 5.0));
    }

    /// An auto-width float shrinks to fit its content, within the room.
    #[test]
    fn an_auto_width_float_shrinks_to_fit() {
        let laid = Laid::new(
            "<div id=s>abc</div><div id=w>aaaa bbbb cccc dddd</div>",
            "div{float:left;clear:left}",
            100.0,
        );
        assert_eq!(rect(laid.border("s")), (0.0, 0.0, 24.0, 20.0));
        // Wider than the room: the room, and its words wrapped in it.
        assert_eq!(rect(laid.border("w")), (0.0, 20.0, 100.0, 40.0));
    }

    /// A formatting context's root holds its floats; an ordinary block lets
    /// them hang out of it, and the next line wraps beside them still.
    #[test]
    fn a_context_root_holds_its_floats() {
        let css = ".f{float:left;width:20px;height:50px}";
        let laid = Laid::new(
            "<div id=o style=overflow:hidden><div class=f></div>x</div>",
            css,
            100.0,
        );
        assert_eq!(laid.border("o").height, 50.0);
        let laid = Laid::new("<div id=o><div class=f></div>x</div><p>after", css, 100.0);
        assert_eq!(laid.border("o").height, 20.0);
        assert_eq!(laid.lines()[1], (20.0, 20.0, "after".into()));
    }

    /// A formatting context's root beside a float takes the room it leaves.
    #[test]
    fn a_context_root_beside_a_float_takes_its_room() {
        let laid = Laid::new(
            "<div id=f></div><div id=o>abc</div>",
            "#f{float:left;width:30px;height:40px}#o{overflow:hidden}",
            100.0,
        );
        assert_eq!(rect(laid.border("o")), (30.0, 0.0, 70.0, 20.0));
        assert_eq!(laid.lines()[0], (0.0, 30.0, "abc".into()));
        // A set width that does not fit beside it goes below.
        let laid = Laid::new(
            "<div id=f></div><div id=o>abc</div>",
            "#f{float:left;width:30px;height:40px}#o{overflow:hidden;width:80px}",
            100.0,
        );
        assert_eq!(rect(laid.border("o")), (0.0, 40.0, 80.0, 20.0));
    }

    /// A word that does not fit beside a float moves down past it rather
    /// than splitting there.
    #[test]
    fn a_word_too_wide_beside_a_float_moves_below_it() {
        let laid = Laid::new(
            "<div id=f></div><p>aaaaaaaaaa",
            "#f{float:left;width:50px;height:30px}",
            100.0,
        );
        assert_eq!(laid.lines(), vec![(30.0, 0.0, "aaaaaaaaaa".into())]);
    }

    /// A table cell holds its floats, and measures them beside its text.
    #[test]
    fn a_cell_holds_and_measures_its_floats() {
        let laid = Laid::new(
            "<table><tr><td id=c><img id=f width=40 height=60>ab</table>",
            "#f{float:left}",
            300.0,
        );
        let cell = laid.border("c");
        // The float beside its text on one line: 40 + 16.
        assert_eq!(cell.width, 56.0);
        assert_eq!(cell.height, 60.0);
        assert_eq!(rect(laid.border("f")), (cell.x, cell.y, 40.0, 60.0));
        assert_eq!(laid.lines()[0].1, cell.x + 40.0);
    }

    /// Lines and markers in a float move with it; its list item's marker
    /// hangs beside its own first line.
    #[test]
    fn a_floats_lines_and_markers_move_with_it() {
        let laid = Laid::new(
            "<div>aaaaaaaaaa <div id=f><ul><li>x</ul></div>b</div>",
            "#f{float:right;width:40px}ul{padding-left:20px}",
            120.0,
        );
        let float = laid.border("f");
        assert_eq!((float.x, float.y), (80.0, 20.0));
        let x = laid.lines().into_iter().find(|(_, _, t)| t == "x").unwrap();
        assert_eq!((x.0, x.1), (20.0, 100.0));
        let marker = laid.out.markers.first().unwrap();
        assert_eq!(marker.baseline, 20.0 + 15.0);
        assert!(marker.x + marker.width <= 100.0);
    }

    /// A windowed layout equals the filtered whole one, floats moved down
    /// by their placement included.
    #[test]
    fn windowed_layouts_with_floats_equal_filtered_whole_ones() {
        let mut html = String::new();
        for n in 0..40 {
            html.push_str(&format!(
                "<p>word{n} word word word <img class=f width=30 height={h}> more text here ok",
                h = 10 + n % 50
            ));
            if n % 7 == 0 {
                html.push_str("<div class=r><ol><li>in<li>side</ol>words in a float</div>");
            }
            if n % 5 == 0 {
                html.push_str(
                    "<ul><li>met mid line <span class=r><span class=li>in</span>\
                     <span class=li>side</span>words</span> after it</ul>",
                );
            }
        }
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(
            ".f{float:left}.r{float:right;width:60px}li{list-style-position:outside}\
             .li{display:list-item;list-style-type:disc}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(200.0, 600.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        // Each marker names its line, in line order.
        let mut last = 0;
        for marker in full.markers.iter() {
            let at = marker.line.unwrap() as usize;
            let line = &full.lines[at];
            assert_eq!(marker.baseline, line.y + line.baseline);
            assert!(at >= last);
            last = at;
        }
        assert!(full.markers.len() >= 20);
        for window in [0.0..100.0, 150.0..400.0, 380.0..390.0, 1000.0..1500.0] {
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
            assert_eq!(
                subset
                    .markers
                    .iter()
                    .map(|m| (m.x, m.y))
                    .collect::<Vec<_>>(),
                full.markers
                    .iter()
                    .filter(|m| kept(m.y, m.height))
                    .map(|m| (m.x, m.y))
                    .collect::<Vec<_>>(),
            );
        }
    }

    /// A float met within a line puts its lines after that line, so lines
    /// lie in the tree order of their starts; one met before any of its
    /// sequence, before it.
    #[test]
    fn a_floats_lines_follow_the_line_it_was_met_in() {
        let laid = Laid::new(
            "<p>aaaa bbbb <span class=f>xx</span> cccc dddd eeee ffff gggg",
            ".f{float:right;width:40px}",
            200.0,
        );
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaaa bbbb cccc dddd".into()),
                (0.0, 160.0, "xx".into()),
                (20.0, 0.0, "eeee ffff gggg".into()),
            ]
        );
        let laid = Laid::new(
            "<p><span class=f>xx</span>aaaa bbbb",
            ".f{float:right;width:40px}",
            200.0,
        );
        assert_eq!(
            laid.lines(),
            vec![(0.0, 160.0, "xx".into()), (0.0, 0.0, "aaaa bbbb".into())]
        );
        // Met after only an inside marker, which holds no place: before
        // the line.
        let laid = Laid::new(
            "<ul><li style=list-style-position:inside><span class=f>FF</span>aaa bbb",
            ".f{float:right;width:16px}",
            200.0,
        );
        let texts: Vec<String> = laid.lines().into_iter().map(|(_, _, t)| t).collect();
        assert_eq!(texts, ["FF", "aaa bbb"]);
        // Met within a word that then wraps: after the line the word
        // goes to.
        let laid = Laid::new(
            "<p>aaa bbb<span class=f>FF</span>bbbbbb ccc",
            ".f{float:right;width:16px}",
            100.0,
        );
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, 0.0, "aaa".into()),
                (20.0, 0.0, "bbbbbbbbb".into()),
                (0.0, 84.0, "FF".into()),
                (40.0, 0.0, "ccc".into()),
            ]
        );
    }

    /// A cell's baseline is its first in-flow line's, not that of a float
    /// laid before it, and a window keeps the row's height.
    #[test]
    fn a_cells_baseline_skips_its_floats() {
        let html = "<div style=height:1000px></div><div class=t><div class=c>\
                    <div class=f>F</div>text</div><div class=c><div id=in>o<br>o<br>o</div>\
                    </div></div><p id=after>z";
        let css = sheets(".t{display:table}.c{display:table-cell}.f{float:left;padding-top:40px}");
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(400.0, 600.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        let laid = Laid {
            doc: td_html::document::parse(html).unwrap(),
            out: layout(&styled, &mut Metrics, &request).unwrap(),
        };
        assert_eq!(laid.border("in").y, 1000.0);
        assert_eq!(laid.border("after").y, 1060.0);
        let mut asked = request.clone();
        asked.window = Some(0.0..100.0);
        let subset = layout(&styled, &mut Metrics, &asked).unwrap();
        assert_eq!(subset.boxes, full.boxes);
        assert_eq!(subset.height, full.height);
    }

    /// Widths that sum to the room fit, however their x's round: floats in
    /// percent columns at fractional offsets stay on one row, and text in a
    /// cell measured to fit beside a float stays on one line.
    #[test]
    fn exact_fits_beside_floats_are_not_refused_for_an_ulp() {
        for (a, b) in [
            ("33.33333333%", "66.66666667%"),
            ("16.66666667%", "83.33333333%"),
            ("41.66666667%", "58.33333333%"),
        ] {
            for width in (200..520).step_by(7) {
                for margin in ["0", "0.3px", "1.7%"] {
                    let laid = Laid::new(
                        "<div id=w><div id=a class=f></div><div id=b class=f></div></div>",
                        &format!(
                            ".f{{float:left;height:10px}}#a{{width:{a}}}#b{{width:{b}}}\
                             #w{{margin-left:{margin}}}"
                        ),
                        f64::from(width),
                    );
                    assert_eq!(laid.border("b").y, 0.0, "{a} {b} {width} {margin}");
                }
            }
        }
        let laid = Laid::new(
            "<table style=margin-left:0.014px><tr><td><img class=f width=40 height=60>ab cd</table>",
            ".f{float:left}",
            400.0,
        );
        assert_eq!(laid.lines().len(), 1, "{:?}", laid.lines());
    }

    /// Floats in measured content: no wrap opportunity beside one, and a
    /// space after one kept.
    #[test]
    fn a_measured_float_keeps_its_lines_words_and_spaces() {
        let laid = Laid::new(
            "<table><tr><td id=c>ab<img class=r width=40 height=10> cd</table>",
            ".r{float:right}",
            400.0,
        );
        assert_eq!(laid.border("c").width, 80.0);
        assert_eq!(laid.lines().len(), 1, "{:?}", laid.lines());
        let laid = Laid::new(
            "<table style=width:1px><tr><td id=c>ab<img class=f width=8 height=8>cd</table>",
            ".f{float:left}",
            400.0,
        );
        assert_eq!(laid.border("c").width, 32.0);
        assert_eq!(laid.lines().len(), 1, "{:?}", laid.lines());
        // Text, a space and a float: the float stays beside the text.
        let laid = Laid::new(
            "<table><tr><td>Name <span class=v>42</span><tr><td id=c>Longer name \
             <span class=v id=v>7</span></table>",
            ".v{float:right}",
            400.0,
        );
        assert_eq!(laid.border("c").height, 20.0);
        assert_eq!(laid.border("v").y, laid.border("c").y);
        let laid = Laid::new(
            "<div id=f>Label <img id=i width=40 height=10></div>",
            "#f{float:left}#i{float:right}",
            400.0,
        );
        assert_eq!(rect(laid.border("f")), (0.0, 0.0, 88.0, 20.0));
        assert_eq!(laid.border("i").y, 0.0);
    }

    /// Lines past a block's set height do not lose the floats beside the
    /// content after it.
    #[test]
    fn content_above_where_lines_reached_still_meets_floats() {
        let laid = Laid::new(
            "<div id=f></div><div id=h>a<br>b<br>c<br>d<br>e<br>f<br>g</div><p>after",
            "#f{float:left;width:40px;height:100px}#h{height:10px}",
            200.0,
        );
        assert_eq!(laid.lines().last().unwrap(), &(10.0, 40.0, "after".into()));
    }

    /// A float that would fit after one waiting for the next line waits
    /// too, so it is no higher.
    #[test]
    fn a_float_after_a_waiting_one_waits() {
        let laid = Laid::new(
            "<p>aaaaaaaaaa <img id=a width=60 height=20><img id=b width=10 height=10>bb cc",
            "img{float:left}",
            120.0,
        );
        assert_eq!(rect(laid.border("a")), (0.0, 20.0, 60.0, 20.0));
        assert_eq!(rect(laid.border("b")), (60.0, 20.0, 10.0, 10.0));
    }

    /// A right float whose margin box is negative keeps its right margin
    /// edge at the container's.
    #[test]
    fn a_right_float_with_a_negative_margin_box() {
        let laid = Laid::new(
            "<div id=f></div>",
            "#f{float:right;width:20px;height:10px;margin-left:-50px}",
            100.0,
        );
        assert_eq!(rect(laid.border("f")), (80.0, 0.0, 20.0, 10.0));
    }

    /// overflow clip, like visible, opens no formatting context: the
    /// block's lines meet the float, the block does not move.
    #[test]
    fn overflow_clip_opens_no_context() {
        let laid = Laid::new(
            "<div id=f></div><div id=c>aaaa</div>",
            "#f{float:left;width:40px;height:40px}#c{overflow-x:visible;overflow-y:clip}",
            120.0,
        );
        assert_eq!(laid.border("c").x, 0.0);
        assert_eq!(laid.lines(), vec![(0.0, 40.0, "aaaa".into())]);
    }

    /// A page without floats takes no float path; a floated table keeps its
    /// place in flow; the root never floats; a text page with a float is not
    /// one `text::fit` counts.
    #[test]
    fn what_floats_and_what_does_not() {
        let styled_of = |html: &str, css: &str, doc: &mut Option<td_html::document::Document>| {
            *doc = Some(td_html::document::parse(html).unwrap());
            let doc = doc.as_ref().unwrap();
            StyledDocument::new(doc, &sheets(css), html.len(), Limits::default())
                .unwrap()
                .has_floats()
        };
        let mut doc = None;
        assert!(!styled_of("<p>a<span>b</span>", "", &mut doc));
        assert!(!styled_of("<p>a", "html{float:left}", &mut doc));
        assert!(!styled_of(
            "<table><tr><td>a</table>",
            "table{float:left}",
            &mut doc
        ));
        assert!(styled_of(
            "<p>a<span>b</span>",
            "span{float:right}",
            &mut doc
        ));
        assert!(styled_of("<p>a<img>", "img{float:left}", &mut doc));
        let laid = Laid::new(
            "<table id=t><tr><td>a</table><p>b",
            "table{float:left;width:50px}",
            100.0,
        );
        let table = laid.border("t");
        assert_eq!(table.y, 0.0);
        // Below it, in flow, not beside it.
        assert_eq!(laid.lines()[1], (table.height, 0.0, "b".into()));
        let html = "<!doctype html><plaintext>a\nb";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("plaintext{display:block;white-space:pre-wrap}plaintext{float:left}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(styled.has_floats());
        assert!(matches!(
            crate::text::fit(&styled, &mut Metrics, 100.0, 10, None),
            Err(Error::Invalid)
        ));
    }

    /// Floats side by side, each placed past all those before it, cost their
    /// pairs: enough of them are refused by the work bound, by name.
    #[test]
    fn floats_side_by_side_are_bounded_by_the_work_limit() {
        let html = "<div class=f></div>".repeat(20_000);
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(".f{float:left;width:0;height:10px}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        assert!(matches!(
            layout(&styled, &mut Metrics, &Request::new(100.0, 600.0)),
            Err(Error::Limit("MAX_LAYOUT_WORK"))
        ));
        // Each below the last, retired as lines pass them: linear.
        let html = "<div class=f></div><p>x".repeat(20_000);
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets(".f{float:left;width:10px;height:10px}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 600.0)).unwrap();
        assert_eq!(out.line_count, 20_000);
        assert!(out.work < 2_000_000, "{}", out.work);
        // Floats taller than their paragraphs pile up below the lines: a
        // line visits only those its top has reached.
        let html = "<div class=f></div><p>x".repeat(20_000);
        let css = sheets(".f{float:left;clear:left;width:10px;height:40px}");
        let doc = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 600.0)).unwrap();
        assert_eq!(out.height, 800_000.0);
        assert!(out.work < 2_000_000, "{}", out.work);
    }
}
