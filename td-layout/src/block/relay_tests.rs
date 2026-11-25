//! Flex items laid again at their used sizes (DESIGN.md §5, "Flex",
//! "Laying again"). Each scalar advances 8 px and a 16 px line is 20 px
//! tall with its baseline 15 px down, so sizes and offsets are worked by
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

    fn lay(html: &str, css: &str, request: &Request) -> (td_html::document::Document, Layout) {
        let doc = td_html::document::parse(html).unwrap();
        let out = {
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            layout(&styled, &mut Metrics, request).unwrap()
        };
        (doc, out)
    }

    struct Laid {
        doc: td_html::document::Document,
        out: Layout,
    }

    impl Laid {
        fn new(html: &str, css: &str, width: f64) -> Self {
            let (doc, out) = lay(html, css, &Request::new(width, 600.0));
            assert_eq!(out.line_count, out.lines.len(), "{html}");
            Self { doc, out }
        }
        fn border(&self, id: &str) -> (f64, f64, f64, f64) {
            let b = self
                .out
                .boxes
                .iter()
                .find(|g| {
                    self.doc
                        .node(g.node)
                        .and_then(|n| n.element())
                        .and_then(|e| e.attribute("id"))
                        == Some(id)
                })
                .unwrap_or_else(|| panic!("no box {id}"))
                .border;
            (b.x, b.y, b.width, b.height)
        }
        /// Each line's top and its text.
        fn lines(&self) -> Vec<(f64, String)> {
            self.out
                .lines
                .iter()
                .map(|l| {
                    let text = l
                        .fragments
                        .iter()
                        .map(|f| {
                            let node = self.doc.node(f.node).and_then(|n| n.text()).unwrap();
                            node[f.range.clone()].to_string()
                        })
                        .collect::<String>();
                    (l.y, text)
                })
                .collect()
        }
    }

    /// A card deck's cards are stretched to the tallest, each laid again
    /// at that height, so a growing body puts the footer at the foot.
    #[test]
    fn a_card_decks_footers_sit_at_their_feet() {
        let laid = Laid::new(
            "<div id=d><div id=k1><p class=b>short</p><p id=f1>foot</p></div>\
             <div id=k2><p class=b>a<br>b<br>c<br>d</p><p id=f2>foot</p></div></div>",
            "#d{display:flex}#d>div{display:flex;flex-direction:column}.b{flex:1 1 auto}",
            400.0,
        );
        assert_eq!(laid.border("k1"), (0.0, 0.0, 40.0, 100.0));
        assert_eq!(laid.border("f1"), (0.0, 80.0, 40.0, 20.0));
        assert_eq!(laid.border("f2"), (40.0, 80.0, 32.0, 20.0));
        assert_eq!(
            laid.lines(),
            vec![
                (0.0, "short".into()),
                (80.0, "foot".into()),
                (0.0, "a".into()),
                (20.0, "b".into()),
                (40.0, "c".into()),
                (60.0, "d".into()),
                (80.0, "foot".into()),
            ]
        );
    }

    /// A card centred in a grown `main` is centred in its used height; the
    /// holy grail's grown middle row stretches its columns to it.
    #[test]
    fn grown_rows_align_and_stretch_their_items() {
        let page = "<div id=s><p>head</p><div id=m><p id=c>card</p></div><p>foot</p></div>";
        let column = "#s{display:flex;flex-direction:column;height:300px}#m{flex:1;display:flex}";
        let laid = Laid::new(page, &format!("{column}#m{{align-items:center}}"), 400.0);
        assert_eq!(laid.border("m"), (0.0, 20.0, 400.0, 260.0));
        assert_eq!(laid.border("c").1, 140.0);
        let laid = Laid::new(page, column, 400.0);
        assert_eq!(laid.border("c"), (0.0, 20.0, 32.0, 260.0));
        // A wrapping row grown in a column spreads its lines in it.
        let laid = Laid::new(
            "<div id=s><div id=w><i id=a>a</i><i id=b>b</i></div></div>",
            "#s{display:flex;flex-direction:column;height:300px}\
             #w{flex:1;display:flex;flex-wrap:wrap}i{display:block;flex:0 0 100%}",
            400.0,
        );
        assert_eq!(laid.border("a"), (0.0, 0.0, 400.0, 150.0));
        assert_eq!(laid.border("b"), (0.0, 150.0, 400.0, 150.0));
    }

    /// A percentage height in a grown or stretched item resolves against
    /// its used height.
    #[test]
    fn percentages_resolve_against_used_sizes() {
        let laid = Laid::new(
            "<div id=s><p>head</p><div id=g><p id=h>x</p></div></div>",
            "#s{display:flex;flex-direction:column;height:300px}#g{flex:1}#h{height:100%}",
            400.0,
        );
        assert_eq!(laid.border("h"), (0.0, 20.0, 400.0, 280.0));
        let laid = Laid::new(
            "<div id=r><p>a<br>b<br>c</p><div id=g><p id=h>x</p></div></div>",
            "#r{display:flex}#h{height:50%}",
            400.0,
        );
        assert_eq!(laid.border("g").3, 60.0);
        assert_eq!(laid.border("h").3, 30.0);
    }

    /// A row of definite height lays its stretched items at its height
    /// once: what they hold resolves against it.
    #[test]
    fn definite_rows_lay_stretched_items_at_their_height() {
        let laid = Laid::new(
            "<div id=r><div id=g><p id=h>x</p></div></div>",
            "#r{display:flex;height:90px}#h{height:50%}",
            400.0,
        );
        assert_eq!(laid.border("g").3, 90.0);
        assert_eq!(laid.border("h").3, 45.0);
        // Once: as much work as laying them at their tops.
        let work = |align: &str| {
            let html = format!(
                "<div id=r>{}</div>",
                "<div class=k><i>x</i></div>".repeat(2000)
            );
            let css =
                format!("#r{{display:flex;height:90px;align-items:{align}}}.k{{display:flex}}");
            lay(&html, &css, &Request::new(100_000.0, 600.0)).1.work
        };
        let (stretched, top) = (work("stretch"), work("start"));
        assert!(stretched < top + top / 10, "{stretched} {top}");
    }

    /// A row of definite height laid as a column item takes its items'
    /// heights as laid at an auto height as its content's, its automatic
    /// minimum: it shrinks to its share.
    #[test]
    fn a_definite_rows_content_is_its_items_auto_heights() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><div id=b><i>y</i></div></div>",
            "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
             #a{height:50px}#b{flex:1;height:300px;display:flex}",
            400.0,
        );
        assert_eq!(laid.border("b"), (0.0, 50.0, 400.0, 250.0));
    }

    /// A column laid again from an item before those it keeps for the
    /// window keeps the window from that item: what it laid again above
    /// the window is not kept.
    #[test]
    fn a_column_laid_again_keeps_the_window_from_where_it_dropped() {
        let html = "<div id=s><div id=a><p>a<br>b<br>c<br>d</p><p class=g></p>\
                    <p id=e>end</p></div><p>after</p></div>";
        let css = "#s{display:flex;flex-direction:column;height:400px}\
                   #a{flex:1;display:flex;flex-direction:column}.g{flex:1}";
        let whole = Laid::new(html, css, 400.0);
        assert_eq!(whole.border("e").1, 360.0);
        let doc = td_html::document::parse(html).unwrap();
        let styled =
            StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
        for window in [0.0..100.0, 300.0..500.0] {
            let mut asked = Request::new(400.0, 600.0);
            asked.window = Some(window.clone());
            let subset = layout(&styled, &mut Metrics, &asked).unwrap();
            let kept = |l: &&crate::inline::Line| l.y + l.height > window.start && l.y < window.end;
            assert_eq!(
                subset.lines.iter().collect::<Vec<_>>(),
                whole.out.lines.iter().filter(kept).collect::<Vec<_>>(),
                "{window:?}"
            );
        }
    }

    /// A column laid again keeps its items' resolved auto margins.
    #[test]
    fn a_column_laid_again_keeps_its_auto_margins() {
        let laid = Laid::new(
            "<div id=s><div id=g><i>x</i></div><p id=b>b</p></div>",
            "#s{display:flex;flex-direction:column;height:300px}\
             #g{flex:1;max-height:100px;display:flex}#b{margin-top:auto}",
            400.0,
        );
        assert_eq!(laid.border("g").3, 100.0);
        assert_eq!(laid.border("b").1, 280.0);
        let b = laid.out.boxes.iter().find(|g| g.border.y == 280.0).unwrap();
        assert_eq!(b.margin.top, 180.0);
    }

    /// A column item's content's percentages of its own percentage height
    /// are no content of it: it shrinks to its share, and what it holds
    /// resolves against that.
    #[test]
    fn percentages_of_an_item_are_not_its_content() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><div id=b><i id=c>y</i></div></div>",
            "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
             #a{height:50px}#b{flex:1;height:100%}#c{height:100%}",
            400.0,
        );
        assert_eq!(laid.border("b"), (0.0, 50.0, 400.0, 250.0));
        assert_eq!(laid.border("c"), (0.0, 50.0, 400.0, 250.0));
    }

    /// Re-lays nest `MAX_RELAY` deep: past that an item keeps the content
    /// it was laid with, as at its hypothetical size.
    #[test]
    fn re_lays_nest_two_deep() {
        let nest = |levels: usize| {
            let open = "<div class=g>".repeat(levels);
            let close = "</div>".repeat(levels);
            format!(
                "<div id=r><p>a<br>b<br>c<br>d<br>e<br>f</p><div class=k>{open}\
                 <p class=b>x</p><p id=foot>foot</p>{close}</div></div>"
            )
        };
        let css = "#r{display:flex}.k,.g{display:flex;flex-direction:column}\
                   .g,.b{flex:1}";
        // The card laid again in the row, its grown box in it: two deep.
        let laid = Laid::new(&nest(1), css, 400.0);
        assert_eq!(laid.border("foot").1, 100.0);
        // A third: as laid at its hypothetical size, its foot under its
        // text, its box grown to the card's foot.
        let laid = Laid::new(&nest(2), css, 400.0);
        assert_eq!(laid.border("foot").1, 20.0);
        let laid = Laid::new(&nest(5), css, 400.0);
        assert_eq!(laid.border("foot").1, 20.0);
    }

    /// One pass lays a line or column again, from its first item laid
    /// again: a row or column of 10,000 items each laid again costs less
    /// than `MAX_RELAY + 1` layouts of it, and twice as many items twice
    /// as much.
    #[test]
    fn a_line_or_column_is_laid_again_in_one_pass() {
        let work = |n: usize, css: &str| {
            let html = format!(
                "<div id=f><p>a<br>b</p>{}</div>",
                "<div class=k><i>x</i></div>".repeat(n)
            );
            let (_, out) = lay(&html, css, &Request::new(1000.0, 600.0));
            out.work
        };
        let row = "#f{display:flex}.k{display:flex;width:1px}i{display:block}";
        let column = "#f{display:flex;flex-direction:column;height:400000px}\
                      .k{display:flex;flex:1}i{display:block}";
        for (css, plain) in [
            (row, format!("{row}#f{{align-items:start}}")),
            (column, format!("{column}.k{{flex:none}}")),
        ] {
            let (small, large) = (work(10_000, css), work(20_000, css));
            let alone = work(10_000, &plain);
            assert!(small < 3 * alone, "{css} {small} {alone}");
            assert!(small > alone, "{css} {small} {alone}");
            assert!(large < 21 * small / 10, "{css} {small} {large}");
        }
        let (_, out) = lay(
            &format!(
                "<div id=f><p>a<br>b</p>{}</div>",
                "<div class=k><i>x</i></div>".repeat(3)
            ),
            row,
            &Request::new(1000.0, 600.0),
        );
        assert!(out.boxes.iter().filter(|g| g.border.height == 40.0).count() >= 6);
        // Laid again from the first item laid again: one at the end of a
        // line of items each wanting its height costs about one more.
        let line = |css: &str| {
            let html = format!(
                "<div id=f>{}<div class=k><i>x</i></div></div>",
                "<div class=k><i>x<br>y</i></div>".repeat(10_000)
            );
            lay(&html, css, &Request::new(100_000.0, 600.0)).1.work
        };
        let (again, top) = (line(row), line(&format!("{row}#f{{align-items:start}}")));
        assert!(again < top + top / 50, "{again} {top}");
        // The state a mark keeps, a record DESIGN.md's bounds charge.
        assert_eq!(std::mem::size_of::<crate::block::flex::Mark>(), 104);
    }

    /// What an item laid again holds is laid again whole: its lists'
    /// numbers and markers, inline-blocks and inline flex containers, as
    /// laid once at its size; an outside marker waiting for a flex list
    /// item's first line keeps it from being laid again.
    #[test]
    fn what_is_laid_again_is_laid_whole() {
        let laid = Laid::new(
            "<div id=d><div id=k><ol id=o><li>one<li>two</ol><p class=b>\
             x <span class=ib>in<br>block</span> <span class=if><b>a</b></span></p>\
             <ol start=7><li id=l>seven</ol></div>\
             <p>a<br>b<br>c<br>d<br>e<br>f<br>g<br>h</p></div>",
            "#d{display:flex}#k{display:flex;flex-direction:column}.b{flex:1}\
             .ib{display:inline-block}.if{display:inline-flex}\
             li{list-style-position:outside}",
            400.0,
        );
        assert_eq!(laid.border("l").1, 140.0);
        let markers = laid
            .out
            .markers
            .iter()
            .map(|m| (m.ordinal, laid.out.lines[m.line.unwrap() as usize].y, m.y))
            .collect::<Vec<_>>();
        assert_eq!(
            markers,
            vec![(1, 0.0, 0.0), (2, 20.0, 20.0), (7, 140.0, 140.0)]
        );
        // A list item's marker waits for the first line of the row it
        // holds: the item laying it is not laid again; one after it is.
        let css = "#k{display:flex}#c{display:flex;flex-direction:column}.b{flex:1}\
                   li{list-style-position:outside}";
        let card = "<div id=c><p class=b>x</p><p id=e>end</p></div>";
        let laid = Laid::new(
            &format!("<ul><li><div id=k><p>a<br>b<br>c</p>{card}</div></ul>"),
            css,
            400.0,
        );
        assert_eq!(laid.border("e").1, 40.0);
        let laid = Laid::new(
            &format!("<ul><li><div id=k>{card}<p>a<br>b<br>c</p></div></ul>"),
            css,
            400.0,
        );
        assert_eq!(laid.border("e").1, 20.0);
        assert_eq!(laid.out.markers.len(), 1);
        assert_eq!(laid.out.markers[0].line, Some(0));
    }

    /// Every item laid again equals its content laid as a block of its
    /// used size, moved to it: its boxes, its lines and its markers, in
    /// rows stretching it, columns growing it, rows of definite height
    /// and wrapping rows; whole and windowed.
    #[test]
    fn items_laid_again_equal_blocks_of_their_size() {
        const INNER: &[&str] = &[
            "<div class=col><p class=g>body {n}</p><p>foot</p></div>",
            "<p class=h>half {n}</p><p>after</p>",
            "<ol><li>one {n}<li>two</ol><p class=q>quarter</p>",
            "<p><span class=if><b>a</b><b class=pt>b {n}</b></span> text</p><p class=h>h</p>",
            "<div class=row><i>a</i><i class=cn>b<br>{n}</i></div>",
            "<div class=row style=align-items:end><i>a {n}</i>\
             <i><span class=ib>in<br>block</span></i></div>",
            "<p>plain {n}</p>",
        ];
        const SHEET: &str = ".col{display:flex;flex-direction:column;height:100%}.g{flex:1}\
            .h{height:50%}.q{min-height:25%}.if{display:inline-flex;align-items:baseline}\
            .pt{padding-top:3px}.row{display:flex;height:100%}.cn{align-self:center}\
            .ib{display:inline-block}li{list-style-position:outside}i{display:block}";
        const PAGE: &str =
            ".r,.d,.w{display:flex}.c{display:flex;flex-direction:column;height:200px}\
            .c>.it{flex:1}.d{height:70px}.w{flex-wrap:wrap}.w>*{flex:0 0 45%}";
        let mut html = String::new();
        let mut items = Vec::new();
        for n in 0..28 {
            let item = |k: usize| {
                let inner = INNER[(n + k) % INNER.len()].replace("{n}", &n.to_string());
                format!("<div class=it id=i{n}x{k}>{inner}</div>")
            };
            let (a, b) = (item(0), item(1));
            html.push_str(&match n % 4 {
                0 => format!("<div class=r><p>t<br>t<br>t<br>t<br>t</p>{a}{b}</div>"),
                1 => format!("<div class=c><p>head</p>{a}{b}</div>"),
                2 => format!("<div class=d>{a}{b}</div>"),
                _ => format!("<div class=w><p>t<br>t<br>t<br>t</p>{a}{b}</div>"),
            });
            items.extend([(n, 0), (n, 1)]);
        }
        let css = format!("{SHEET}{PAGE}");
        let whole = Laid::new(&html, &css, 400.0);
        let mut laid_again = 0;
        for (n, k) in items {
            let id = format!("i{n}x{k}");
            let (x, y, width, height) = whole.border(&id);
            let inner = INNER[(n + k) % INNER.len()].replace("{n}", &n.to_string());
            let alone = Laid::new(
                &format!(
                    "<div class=it id={id} style=width:{width}px;height:{height}px>{inner}</div>"
                ),
                SHEET,
                1000.0,
            );
            let held = |laid: &Laid, (dx, dy): (f64, f64)| content(laid, &id, (dx, dy));
            let (a, b) = (held(&whole, (x, y)), held(&alone, (0.0, 0.0)));
            assert_eq!(a, b, "{id} {inner}");
            laid_again += usize::from(inner.contains('%') || inner.contains("class=col"));
        }
        assert!(laid_again > 0);
        // Windowed, it keeps the whole layout's lines in the window.
        let (doc, _) = lay(&html, &css, &Request::new(400.0, 600.0));
        let styled =
            StyledDocument::new(&doc, &sheets(&css), html.len(), Limits::default()).unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(400.0, 600.0)).unwrap();
        for window in [0.0..90.0, 300.0..500.0, full.height - 60.0..full.height] {
            let mut asked = Request::new(400.0, 600.0);
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
            assert_eq!(subset.line_count, full.line_count);
        }
    }

    /// A column item of percentage height measured for its minimum is
    /// laid again at its used height whatever it holds, and is measured
    /// only where it can be: no outside marker waiting, its column's
    /// items' marks kept once they fill it.
    #[test]
    fn measured_items_are_laid_again_at_their_height() {
        let laid = Laid::new(
            "<div class=center>Loading</div>",
            "html,body{height:100%}body{display:flex;flex-direction:column}\
             .center{height:100%;display:flex;align-items:center;justify-content:center}",
            400.0,
        );
        assert_eq!(laid.lines()[0], (290.0, "Loading".into()));
        let column = "#f{display:flex;flex-direction:column;height:300px}";
        for (align, y) in [("center", 140.0), ("flex-end", 280.0)] {
            let laid = Laid::new(
                "<div id=f><div id=b>text</div></div>",
                &format!("{column}#b{{height:100%;display:flex;align-items:{align}}}"),
                400.0,
            );
            assert_eq!(laid.lines()[0].0, y, "{align}");
        }
        let half = format!("{column}#b{{height:50%}}");
        let laid = Laid::new(
            "<div id=f><div id=b><table id=t><tr><td>x</table></div></div>",
            &format!("{half}#t{{height:100%}}"),
            400.0,
        );
        assert_eq!(laid.border("t").3, 150.0);
        let laid = Laid::new(
            "<div id=f><div id=b><img width=10 height=10 style=height:100%></div></div>",
            &half,
            400.0,
        );
        assert_eq!(laid.out.lines[0].height, 155.0);
        let laid = Laid::new(
            "<div id=f><div id=b><p id=t>x</p></div></div>",
            &format!("{half}#t{{position:relative;top:50%}}"),
            400.0,
        );
        assert_eq!(laid.border("t").1, 75.0);
        let css = "#s{display:flex;flex-direction:column;height:300px}#g{flex:1}\
                   #a{height:100%}#c{height:100%}";
        let laid = Laid::new(
            "<div id=s><div id=g>g</div><div id=a><div id=c>c</div></div></div>",
            css,
            400.0,
        );
        assert_eq!(laid.border("c").3, 300.0);
        let laid = Laid::new(
            "<ul><li><div id=s><div id=a><div id=c>c</div></div></div></ul>",
            css,
            400.0,
        );
        assert_eq!(laid.border("c").3, 300.0);
    }

    /// An anonymous item centred or at the end of a line moves with its
    /// height: a row of text so aligned, stretched or grown, is laid again.
    #[test]
    fn centred_text_moves_with_its_used_height() {
        let laid = Laid::new(
            "<div id=r><a id=a>Label</a><p>x<br>y<br>z</p></div>",
            "#r{display:flex}#a{display:flex;align-items:center}",
            400.0,
        );
        assert_eq!(laid.lines()[0], (20.0, "Label".into()));
        let laid = Laid::new(
            "<div id=r><a id=a>Label</a><p>x<br>y<br>z</p></div>",
            "#r{display:flex}#a{display:flex;align-items:flex-end}",
            400.0,
        );
        assert_eq!(laid.lines()[0].0, 40.0);
        let laid = Laid::new(
            "<div id=s><div id=a>text</div></div>",
            "#s{display:flex;flex-direction:column;height:150px}\
             #a{flex:1;display:flex;align-items:center}",
            400.0,
        );
        assert_eq!(laid.lines()[0].0, 65.0);
    }

    /// Percentages resolve once an item's size is definite, changed or
    /// not: in the tallest card of a deck, in a column's items' items, in
    /// an inline picture, a relative offset and a gap between lines.
    #[test]
    fn percentages_resolve_once_definite() {
        let laid = Laid::new(
            "<div class=deck><div class=card><div class=body><p class=half id=h>h</p>\
             <p>a<br>b</p></div></div><div class=card><div class=body>x</div></div></div>",
            ".deck{display:flex}.card{display:flex;flex-direction:column}.body{flex:1}\
             .half{height:50%}",
            400.0,
        );
        assert_eq!(laid.border("h").3, 30.0);
        let laid = Laid::new(
            "<div id=s><div id=k><div><p class=half id=h>x</p></div></div></div>",
            "#s{display:flex;flex-direction:column;height:300px}\
             #k{display:flex;flex-direction:column}.half{height:50%}",
            400.0,
        );
        assert_eq!(laid.border("h").3, 10.0);
        let column = "#s{display:flex;flex-direction:column;height:300px}#g{flex:1}";
        let laid = Laid::new(
            "<div id=s><div id=g><img width=10 height=10 style=height:50%></div></div>",
            column,
            400.0,
        );
        assert_eq!(laid.out.lines[0].replaced[0].height, 150.0);
        let laid = Laid::new(
            "<div id=r><p>a<br>b<br>c</p><div id=g><p id=t>x</p></div></div>",
            "#r{display:flex}#t{position:relative;top:50%}",
            400.0,
        );
        assert_eq!(laid.border("t").1, 30.0);
        let laid = Laid::new(
            "<div id=s><div id=g><i id=a>a</i><i id=b>b</i></div></div>",
            &format!(
                "{column}#g{{display:flex;flex-wrap:wrap;row-gap:10%;align-content:start}}\
                 i{{display:block;flex:0 0 100%}}"
            ),
            400.0,
        );
        assert_eq!(laid.border("b").1, 50.0);
    }

    /// What is laid again does not hang on where the column lies: an
    /// item filling its column holds percentages of its height as one in
    /// a cell or a float does; a column of least height leaves them auto.
    #[test]
    fn laying_again_is_the_same_wherever_it_lies() {
        let page = "<div id=s><div id=g><p id=h>x</p><p id=t>fill</p></div><p>foot</p></div>";
        let css = "#s{display:flex;flex-direction:column;height:100px}#g{flex:1}\
                   #h{height:50%}#t{height:60px}";
        for html in [
            page.to_string(),
            format!("<table><tr><td>{page}</table>"),
            format!("<div style=float:left;width:300px>{page}</div>"),
        ] {
            let laid = Laid::new(&html, css, 400.0);
            assert_eq!(laid.border("h").3, 40.0, "{html}");
        }
        let laid = Laid::new(
            "<div id=s><div id=g><p id=h>x</p></div></div>",
            "#s{display:flex;flex-direction:column;min-height:300px}#g{flex:1}#h{height:50%}",
            400.0,
        );
        assert_eq!((laid.border("g").3, laid.border("h").3), (300.0, 20.0));
        // A flex container grown there is laid at its used height.
        let laid = Laid::new(
            "<div id=s><div id=g><p class=b>x</p><p id=e>end</p></div></div>",
            "#s{display:flex;flex-direction:column;min-height:300px}\
             #g{flex:1;display:flex;flex-direction:column}.b{flex:1}",
            400.0,
        );
        assert_eq!(laid.border("e").1, 280.0);
    }

    /// A row of definite height in a column, its stretched items' content
    /// holding percentages of them, takes their heights as laid at an
    /// auto height as its automatic minimum.
    #[test]
    fn a_definite_rows_minimum_has_no_percentages() {
        let laid = Laid::new(
            "<div id=f><i id=a></i><div id=b><div id=c><i id=d>y</i></div></div></div>",
            "#f{display:flex;flex-direction:column;height:300px}i{display:block}\
             #a{height:50px}#b{flex:1;height:300px;display:flex}#d{height:100%}",
            400.0,
        );
        assert_eq!(laid.border("b").3, 250.0);
        assert_eq!(laid.border("d").3, 250.0);
    }

    /// A row keeps a mark for an item only where it may be the first laid
    /// again: one holding percentages, or shorter than every one marked
    /// before it, until one is shorter than an item before it.
    #[test]
    fn a_row_marks_what_may_be_laid_again_first() {
        // As tall as the item before it, the second is not laid again;
        // the third, shorter, is, at the line's height.
        let laid = Laid::new(
            "<div id=r><p>x</p><div class=c><i>y</i></div><div class=c><i id=i></i></div></div>",
            "#r{display:flex}.c{display:flex;align-items:center}i{display:block}\
             #i{height:10px}",
            400.0,
        );
        assert_eq!(laid.border("i").1, 5.0);
        // As tall as one marked before it, one holding percentages is.
        let laid = Laid::new(
            "<div id=r><div class=c><i>y</i></div><div><p id=h>x</p></div></div>",
            "#r{display:flex}.c{display:flex;align-items:center}i{display:block}\
             #h{height:50%}",
            400.0,
        );
        assert_eq!(laid.border("h").3, 10.0);
        // One its `max-height` holds at its height is not laid again, and
        // one as tall after it is.
        let laid = Laid::new(
            "<div id=r><p>a<br>b<br>c</p><div class=c style=max-height:20px><i>y</i></div>\
             <div class=c><i id=i>z</i></div></div>",
            "#r{display:flex}.c{display:flex;align-items:center}i{display:block}",
            400.0,
        );
        assert_eq!(laid.border("i").1, 20.0);
        // Nor one at its `max-height` before it.
        let laid = Laid::new(
            "<div id=r><div class=c style=max-height:20px><i>y</i></div>\
             <div class=c><i id=i>z</i></div><p>a<br>b<br>c</p></div>",
            "#r{display:flex}.c{display:flex;align-items:center}i{display:block}",
            400.0,
        );
        assert_eq!(laid.border("i").1, 20.0);
        // 120,000 items as tall, with or without a `max-height`, or 4,000
        // each shorter than the last, each laid again, keep the bytes of a
        // row laid once.
        let page = |item: &dyn Fn(usize) -> String, n: usize, card: &str| {
            let items: String = (0..n).rev().map(item).collect();
            let html = format!("<div id=f>{items}</div>");
            let bytes = |align: &str| {
                let css = format!(
                    "#f{{display:flex;align-items:{align}}}\
                     b{{display:flex;align-items:center;{card}}}i{{display:block}}"
                );
                let (_, out) = lay(&html, &css, &Request::new(1000.0, 600.0));
                (out.boxes.len(), out.page_bytes)
            };
            (bytes("stretch"), bytes("start"))
        };
        for card in ["", "max-height:100px"] {
            let ((boxes, stretched), (_, start)) =
                page(&|_| "<b><i></i></b>".into(), 120_000, card);
            assert_eq!(boxes, 240_003);
            assert!(stretched < start + 1024, "{card} {stretched} {start}");
        }
        let shorter = |n: usize| format!("<b style=padding-bottom:{n}px><i></i></b>");
        let ((_, stretched), (_, start)) = page(&shorter, 4_000, "");
        assert!(stretched < start + 1024, "{stretched} {start}");
        // So does one of items each shorter than the last and all taller
        // than the line's `max-height`.
        let items: String = (0..4_000)
            .rev()
            .map(|n| format!("<b><i style=height:{}px></i></b>", n + 100))
            .collect();
        let html = format!("<div id=f>{items}</div>");
        let bytes = |align: &str| {
            let css = format!(
                "#f{{display:flex;max-height:50px;align-items:{align}}}\
                 b{{display:flex;align-items:center}}i{{display:block}}"
            );
            lay(&html, &css, &Request::new(1000.0, 600.0)).1.page_bytes
        };
        let (stretched, start) = (bytes("stretch"), bytes("start"));
        assert!(stretched < start + 1024, "{stretched} {start}");
    }

    /// A line its bounds hold shorter than an item is laid again from the
    /// first item stretched otherwise than it laid: one taller than a
    /// `max-height`, or than the definite height of a row whose items'
    /// heights at an auto height its column item's minimum takes.
    #[test]
    fn a_line_shorter_than_its_items_is_laid_again_from_the_first() {
        let items = "<div class=rc id=j><b id=tj>s</b><i class=h60></i></div>\
                     <div class=rc id=k><b id=tk>s</b><i class=h100></i></div>";
        let css = ".rc{display:flex;align-items:center}b,i{display:block}\
                   .h60{height:60px;width:5px}.h100{height:100px;width:5px}";
        let laid = Laid::new(
            &format!("<div id=r>{items}</div>"),
            &format!("#r{{display:flex;max-height:60px}}{css}"),
            400.0,
        );
        assert_eq!((laid.border("tj").1, laid.border("tk").1), (20.0, 20.0));
        let laid = Laid::new(
            &format!("<div id=f><div id=b>{items}</div></div>"),
            &format!(
                "#f{{display:flex;flex-direction:column;height:300px}}\
                 #b{{flex:1;height:60px;display:flex}}{css}"
            ),
            400.0,
        );
        assert_eq!((laid.border("b").3, laid.border("tk").1), (300.0, 140.0));
        let laid = Laid::new(
            &format!("<div id=f><div id=b>{items}</div></div>"),
            &format!(
                "#f{{display:flex;flex-direction:column;height:300px}}\
                 #b{{flex:0 0 60px;height:60px;display:flex}}{css}"
            ),
            400.0,
        );
        assert_eq!(laid.border("tk").1, 20.0);
        // An item shorter than the tallest is not laid again where the
        // line is held to its own height.
        let laid = Laid::new(
            "<div id=r><p>t<br>t<br>t<br>t<br>t</p><div class=card><p class=g>a<br>b</p>\
             <p>foot</p></div><div class=card><p class=g>a</p><p id=fk>foot</p></div></div>",
            "#r{display:flex;max-height:60px}.card{display:flex;flex-direction:column}.g{flex:1}",
            400.0,
        );
        assert_eq!(laid.border("fk").1, 40.0);
    }

    /// A row of definite height that is a column item laid at its own
    /// height, its base from that height, lays its stretched items at it
    /// once, leaving a re-lay for what they hold.
    #[test]
    fn a_px_height_row_in_a_column_lays_its_items_at_it() {
        for app in ["", "min-height:300px"] {
            let laid = Laid::new(
                "<div class=app><div class=side>s<br>s<br>s<br>s<br>s<br>s<br>s<br>s</div>\
                 <div class=main><div class=bar><nav><a>Home</a><a>About</a></nav></div>\
                 <div class=content>content</div></div></div>",
                &format!(
                    ".app{{display:flex;{app}}}.side{{width:50px}}\
                     .main{{flex:1;display:flex;flex-direction:column}}\
                     .bar{{height:60px;display:flex}}nav{{display:flex}}\
                     nav a{{display:flex;align-items:center}}.content{{flex:1}}"
                ),
                400.0,
            );
            let home = laid.lines().into_iter().find(|l| l.1 == "Home");
            assert_eq!(home.map(|l| l.0), Some(20.0), "{app}");
        }
    }

    /// A flex container grown in a column of least height is laid at its
    /// used height, its own free space shared, percentages of it auto.
    #[test]
    fn a_column_of_least_height_gives_its_items_no_definite_height() {
        let laid = Laid::new(
            "<div id=s><div id=k><p id=h>x</p><p id=g>g</p></div></div>",
            "#s{display:flex;flex-direction:column;min-height:300px}\
             #k{flex:1;display:flex;flex-direction:column}#h{height:50%}#g{flex:1}",
            400.0,
        );
        let (k, h, g) = (laid.border("k"), laid.border("h"), laid.border("g"));
        assert_eq!((k.3, h.3, g.1, g.3), (300.0, 20.0, 20.0, 280.0));
        // A row's stretched item is definite once its line's height is
        // (§9.4 step 11), so percentages in it resolve.
        let laid = Laid::new(
            "<div id=s><div id=k><div><p id=h>x</p></div></div></div>",
            "#s{display:flex;flex-direction:column;min-height:300px}\
             #k{flex:1;display:flex}#h{height:50%}",
            400.0,
        );
        assert_eq!((laid.border("k").3, laid.border("h").3), (300.0, 150.0));
        // Its line's height so fixed, a row there lays its stretched items
        // at it once, a level of re-lays left for what they hold; an item
        // of the row not stretched keeps its percentage height auto.
        let css = "#s{display:flex;flex-direction:column;min-height:300px}\
                   #mid{flex:1;display:flex}#side{display:flex;flex-direction:column}";
        let page = |sp: &str| {
            format!(
                "<div id=s><div id=mid><div id=side><div id=sp>{sp}</div><p>foot</p></div>\
                 <div>main</div><p id=t style=align-self:start;height:50%>t</p></div>\
                 <p>footer</p></div>"
            )
        };
        let laid = Laid::new(
            &page("mid"),
            &format!("{css}#sp{{flex:1;display:flex;align-items:center}}"),
            400.0,
        );
        let mid = laid.lines().into_iter().find(|l| l.1 == "mid");
        assert_eq!(mid.map(|l| l.0), Some(120.0));
        assert_eq!(laid.border("t").3, 20.0);
        let laid = Laid::new(
            &page("<p id=h style=height:50%>x</p>"),
            &format!("{css}#sp{{flex:1}}"),
            400.0,
        );
        assert_eq!(laid.border("h").3, 130.0);
    }

    /// A column keeps a mark for an item only where it may be the first
    /// laid again: a flex container, or any where its height is definite,
    /// until one holding percentages, which is laid again.
    #[test]
    fn a_column_marks_what_may_be_laid_again_first() {
        let html = format!("<div id=c>{}</div>", "<b><i></i></b>".repeat(120_000));
        let bytes = |child: &str| {
            let css = format!(
                "#c{{display:flex;flex-direction:column;height:600px}}b{{flex:1}}\
                 i{{display:block;height:{child}}}"
            );
            let (_, out) = lay(&html, &css, &Request::new(1000.0, 600.0));
            out.page_bytes
        };
        let (percent, fixed) = (bytes("50%"), bytes("5px"));
        assert!(percent < fixed + 1024, "{percent} {fixed}");
        // Of least height, it lays none of them again.
        let bytes = |child: &str| {
            let css = format!(
                "#c{{display:flex;flex-direction:column;min-height:900000px}}b{{flex:1}}\
                 i{{display:block;height:{child}}}"
            );
            let (_, out) = lay(&html, &css, &Request::new(1000.0, 600.0));
            out.page_bytes
        };
        let (percent, fixed) = (bytes("50%"), bytes("5px"));
        assert!(percent < fixed + 1024, "{percent} {fixed}");
    }

    /// The boxes, lines and markers within the element `id`, moved by
    /// `-dx`, `-dy`.
    #[allow(clippy::type_complexity)]
    fn content(
        laid: &Laid,
        id: &str,
        (dx, dy): (f64, f64),
    ) -> (
        Vec<(f64, f64, f64, f64)>,
        Vec<(f64, f64, f64, f64, String)>,
        Vec<(i32, f64, f64)>,
    ) {
        let doc = &laid.doc;
        let within = |node: td_html::document::NodeId| {
            let mut at = doc.node(node).and_then(|n| n.parent());
            while let Some(up) = at {
                let element = doc.node(up).and_then(|n| n.element());
                if element.and_then(|e| e.attribute("id")) == Some(id) {
                    return true;
                }
                at = doc.node(up).and_then(|n| n.parent());
            }
            false
        };
        let boxes = laid
            .out
            .boxes
            .iter()
            .filter(|g| within(g.node))
            .map(|g| {
                (
                    g.border.x - dx,
                    g.border.y - dy,
                    g.border.width,
                    g.border.height,
                )
            })
            .collect();
        let mut held = Vec::new();
        let lines = laid
            .out
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.fragments.first().is_some_and(|f| within(f.node)))
            .map(|(at, l)| {
                held.push(at);
                let first = l.fragments.first().unwrap();
                let text = l
                    .fragments
                    .iter()
                    .map(|f| {
                        let node = doc.node(f.node).and_then(|n| n.text()).unwrap();
                        node[f.range.clone()].to_string()
                    })
                    .collect::<String>();
                (l.y - dy, l.height, l.baseline, first.x - dx, text)
            })
            .collect();
        let markers = laid
            .out
            .markers
            .iter()
            .filter(|m| m.line.is_some_and(|l| held.contains(&(l as usize))))
            .map(|m| (m.ordinal, m.x - dx, m.y - dy))
            .collect();
        (boxes, lines, markers)
    }
}
