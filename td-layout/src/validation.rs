//! Synthetic corpus shapes; no downloaded page bytes or installed fonts.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use crate::block::{layout, Request};
    use crate::inline::{replay, DisplayText, FontLine, FontMetrics};
    use crate::{
        BoxKind, Error, Limits, StyledDocument, MAX_INPUT, MAX_PAGE_BYTES, MAX_PAGE_INPUT,
        WORKING_BYTES,
    };
    use td_css::{ComputedStyle, Origin, StyleSet, HTML_UA_CSS};

    struct Metrics;
    impl FontMetrics for Metrics {
        fn advance(&mut self, style: &ComputedStyle, c: char) -> u32 {
            (style.font_size() * if u32::from(c) > 255 { 64.0 } else { 32.0 }).round() as u32
        }
        fn line(&mut self, style: &ComputedStyle) -> FontLine {
            FontLine {
                height: (style.font_size() * 80.0).round() as u32,
                baseline: (style.font_size() * 60.0).round() as u32,
            }
        }
    }
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[(Origin::UserAgent, HTML_UA_CSS), (Origin::Author, css)]).unwrap()
    }

    #[test]
    fn two_mib_prose_and_unbroken_text_resize_with_source_places() {
        for word in ["words here ", "x"] {
            let mut html = String::from("<p>");
            let remaining = MAX_PAGE_INPUT - html.len();
            html.push_str(&word.repeat(remaining / word.len()));
            html.push_str(&"x".repeat(MAX_PAGE_INPUT - html.len()));
            assert_eq!(html.len(), MAX_PAGE_INPUT);
            let doc = td_html::document::parse(&html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
            assert!(styled.style_count() <= 4);
            for width in [1280.0, 832.0] {
                let mut request = Request::new(width, 720.0);
                let full = layout(&styled, &mut Metrics, &request).unwrap();
                assert!(full.page_bytes <= MAX_PAGE_BYTES);
                assert!(full.page_bytes + WORKING_BYTES <= 288 * 1024 * 1024);
                assert!(full.line_count > 10_000);
                let mut last = None;
                for line in &full.lines {
                    for f in &line.fragments {
                        let source = doc.node(f.node).unwrap().text().unwrap();
                        assert!(source.get(f.range.clone()).is_some());
                        if let Some((node, end)) = last {
                            if node == f.node {
                                assert!(f.range.start >= end);
                            }
                        }
                        if f.display == DisplayText::Source {
                            assert!(!source[f.range.clone()].ends_with(' '));
                        }
                        last = Some((f.node, f.range.end));
                    }
                }
                request.window = Some(1000.0..1800.0);
                request.retained_output_bytes = full.output_bytes;
                let window = layout(&styled, &mut Metrics, &request).unwrap();
                let expected: Vec<_> = full
                    .lines
                    .iter()
                    .filter(|l| l.y < 1800.0 && l.y + l.height > 1000.0)
                    .collect();
                assert_eq!(window.lines.iter().collect::<Vec<_>>(), expected);
                assert_eq!(window.height, full.height);
                assert_eq!(window.overflow, full.overflow);
                assert_eq!(window.line_count, full.line_count);
                assert!(window.output_bytes * 4 < full.output_bytes);
            }
        }
    }

    #[test]
    fn two_hundred_thousand_list_items_lay_out_within_the_limits() {
        // td-html's MAX_NODES leaves no room for a text node each, so the
        // items are empty: each marker stands alone at its item's top.
        let count = 200_000;
        let html = format!("<ol start=-3>{}", "<li>".repeat(count));
        let doc = td_html::document::parse(&html).unwrap();
        for (css, laid) in [
            ("", true),
            // Inside, each marker lays a line, and the lines' 68 MiB step
            // passes the page's bytes.
            ("li{list-style-position:inside}", false),
        ] {
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            assert_eq!(styled.list_item_count(), count);
            assert_eq!(styled.marker_count(), count);
            let result = layout(&styled, &mut Metrics, &Request::new(1280.0, 720.0));
            if !laid {
                assert!(matches!(result, Err(Error::Limit("MAX_PAGE_BYTES"))));
                continue;
            }
            let out = result.unwrap();
            assert_eq!(out.markers.len(), count);
            assert!(out.lines.is_empty());
            assert!(out.page_bytes <= MAX_PAGE_BYTES);
            assert!(out.work < crate::MAX_LAYOUT_WORK / 16);
            let last = out.markers.last().unwrap();
            assert_eq!(last.ordinal, count as i32 - 4);
            assert_eq!(last.line, None);
            assert_eq!(last.text().as_str(), "199996. ");
            // In order: each marker below the last.
            assert!(out.markers.windows(2).all(|w| w[0].y < w[1].y));
        }
    }

    #[test]
    fn four_mib_text_page_lays_out_under_a_raised_input_limit() {
        let mut html = String::from("<!doctype html><plaintext>");
        let line = "a line of plain text, as a text page holds it\n";
        html.push_str(&line.repeat((MAX_INPUT - html.len()) / line.len()));
        html.push_str(&"x".repeat(MAX_INPUT - html.len()));
        assert_eq!(html.len(), MAX_INPUT);
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("plaintext{display:block;white-space:pre-wrap;font-family:monospace}");
        assert!(matches!(
            StyledDocument::new(&doc, &css, html.len(), Limits::default()),
            Err(Error::Limit("MAX_PAGE_INPUT"))
        ));
        let limits = Limits {
            input: MAX_INPUT,
            ..Limits::default()
        };
        assert!(matches!(
            StyledDocument::new(&doc, &css, html.len() + 1, limits),
            Err(Error::Limit("MAX_PAGE_INPUT"))
        ));
        let styled = StyledDocument::new(&doc, &css, html.len(), limits).unwrap();
        let text: Vec<_> = styled
            .boxes()
            .iter()
            .filter(|b| b.kind == BoxKind::Text)
            .collect();
        assert_eq!(text.len(), 1);
        let source = doc.node(text[0].node).unwrap().text().unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(1280.0, 720.0)).unwrap();
        assert_eq!(full.line_count, source.matches('\n').count() + 1);
        assert_eq!(full.lines.len(), full.line_count);
        assert!(full.page_bytes <= MAX_PAGE_BYTES);
        let last = full.lines.last().unwrap().fragments.last().unwrap();
        assert_eq!(last.range.end, source.len());
    }

    fn text_page(lines: &[&str], bytes: usize) -> String {
        let mut html = String::from("<!doctype html><plaintext>");
        let mut at = 0;
        while html.len() + lines[at % lines.len()].len() <= bytes {
            html.push_str(lines[at % lines.len()]);
            at += 1;
        }
        html
    }
    const TEXT_CSS: &str = "plaintext{display:block;white-space:pre-wrap;font-family:monospace}";

    #[test]
    fn four_mib_of_tab_separated_lines_lays_out_a_fragment_a_line() {
        let html = text_page(
            &[
                "name\tvalue\tthird\tx\ty\n",
                "a\tb\tc\td\te\tf\tg\th\ti\tj\n",
            ],
            MAX_INPUT,
        );
        assert!(html.len() > MAX_INPUT - 32);
        let doc = td_html::document::parse(&html).unwrap();
        let limits = Limits {
            input: MAX_INPUT,
            ..Limits::default()
        };
        let styled = StyledDocument::new(&doc, &sheets(TEXT_CSS), html.len(), limits).unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(1280.0, 720.0)).unwrap();
        let source = doc
            .node(full.lines[0].fragments[0].node)
            .unwrap()
            .text()
            .unwrap();
        assert_eq!(full.line_count, source.matches('\n').count() + 1);
        assert!(full.line_count > 200_000);
        // Every line one fragment: the tabs cost none.
        assert!(full.lines.iter().all(|l| l.fragments.len() <= 1));
        // Within the 68 MiB capacity step of the line array, not past it.
        assert!(
            full.output_bytes < 72 * 1024 * 1024,
            "{}",
            full.output_bytes
        );
        // The 8 px space of 16 px monospace: stops every 64 px.
        let second = &full.lines[1].fragments[0];
        assert_eq!(second.width, 9.0 * 64.0 + 8.0);
        let first = &full.lines[0].fragments[0];
        assert_eq!(first.width, 4.0 * 64.0 + 8.0);
    }

    #[test]
    fn tab_stops_and_cuts_through_block_layout_and_windows() {
        let html = "<pre>界\tx\n<span>ab</span>\tc</pre><p>aaaaaaa<img>b</p><p>cc";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("p{white-space:nowrap}pre{text-align:center}");
        let limits = Limits {
            line_scalars: 6,
            ..Limits::default()
        };
        let styled = StyledDocument::new(&doc, &css, html.len(), limits).unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(400.0, 720.0)).unwrap();
        // The wide glyph is 16 px; the tab runs to 64 px from the line's
        // start, before the line is centred.
        let line = &full.lines[0];
        assert_eq!(line.fragments.len(), 1);
        assert_eq!(line.width, 72.0);
        let centred = 8.0 + (384.0 - 72.0) / 2.0;
        assert_eq!(line.fragments[0].x, centred);
        // A run's tab meets the stop the line sets, not the run's start.
        let line = &full.lines[1];
        assert_eq!(line.fragments.len(), 2);
        assert_eq!(line.fragments[1].width, 48.0 + 8.0);
        // The nowrap paragraph is cut at six scalars: the image and what
        // follows on its line are not laid.
        assert_eq!(full.cuts.lines, 1);
        let cut = full.cuts.first.unwrap();
        let text = doc.node(cut.node).unwrap().text().unwrap();
        assert_eq!((text, cut.offset), ("aaaaaaa", 6));
        assert!(full.lines.iter().all(|l| l.replaced.is_empty()));
        assert_eq!(full.lines.len(), 4);
        let mut request = Request::new(400.0, 720.0);
        request.window = Some(full.lines[0].y..full.lines[0].y + 1.0);
        let window = layout(&styled, &mut Metrics, &request).unwrap();
        assert_eq!(window.cuts, full.cuts);
        assert_eq!(window.lines.len(), 1);
        // An image is a place of its own when it is the first not laid.
        let html = "<p>aaaaaa<img>b";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), limits).unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(400.0, 720.0)).unwrap();
        let cut = full.cuts.first.unwrap();
        assert_eq!(cut.offset, 0);
        assert_eq!(
            doc.node(cut.node)
                .unwrap()
                .element()
                .map(|e| e.name.as_str()),
            Some("img")
        );
        // Where it wraps, an image at the bound breaks the line instead.
        let html = "<p>aaaa<img width=1 height=1>b";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(
            &doc,
            &sheets(""),
            html.len(),
            Limits {
                line_scalars: 4,
                ..limits
            },
        )
        .unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(400.0, 720.0)).unwrap();
        assert_eq!(full.cuts, Default::default());
        assert_eq!(full.lines.len(), 2);
        assert_eq!(full.lines[1].replaced.len(), 1);
        assert_eq!(full.lines[1].fragments.len(), 1);
    }

    #[test]
    fn a_smaller_run_meets_the_stops_its_block_sets() {
        let html = "<pre>a\tb\n<small>a\tb</small></pre>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let full = layout(&styled, &mut Metrics, &Request::new(400.0, 720.0)).unwrap();
        let pre = styled
            .boxes()
            .iter()
            .find(|b| doc.node(b.node).unwrap().element().map(|e| e.name.as_str()) == Some("pre"))
            .unwrap()
            .style;
        // The pre's 8 px space sets stops every 64 px for both lines.
        let mut starts = Vec::new();
        for line in &full.lines {
            let f = line.fragments.last().unwrap();
            replay(&styled, &mut Metrics, f, pre, |s| {
                if s.shown == 'b' {
                    starts.push(s.pen as f64 / 64.0 + f.x - line.fragments[0].x);
                }
            })
            .unwrap();
        }
        assert_eq!(starts, [64.0, 64.0]);
    }

    #[test]
    fn dense_blocks_release_old_output_when_overlap_exceeds_page_budget() {
        let html = format!("<body>{}", "<p>abcdefghijklmnop</p>".repeat(256));
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let first = Request::new(1280.0, 720.0);
        let second = Request::new(832.0, 720.0);
        let full = layout(&styled, &mut Metrics, &first).unwrap();
        let resized = layout(&styled, &mut Metrics, &second).unwrap();
        let budget = full.page_bytes.max(resized.page_bytes);
        drop((styled, full, resized));
        let limited = StyledDocument::new(
            &doc,
            &css,
            html.len(),
            Limits {
                bytes: budget,
                ..Limits::default()
            },
        )
        .unwrap();
        let old = layout(&limited, &mut Metrics, &first).unwrap();
        let mut overlap = second.clone();
        overlap.retained_output_bytes = old.output_bytes;
        assert!(matches!(
            layout(&limited, &mut Metrics, &overlap),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
        drop(old);
        let resized = layout(&limited, &mut Metrics, &second).unwrap();
        assert_eq!(resized.line_count, 256);
        assert!(resized.page_bytes <= budget);
    }

    #[test]
    fn repeated_framework_variables_share_across_different_visible_styles() {
        let declarations: String = (0..128).map(|i| format!("--v{i}:8px;")).collect();
        let css = sheets(&format!(
            "*{{{declarations}}}p{{padding:var(--v0)}}.accent{{color:red}}"
        ));
        let html = format!("<body>{}", "<p>one<p class=accent>two".repeat(256));
        let doc = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let paragraphs: Vec<_> = styled
            .boxes()
            .iter()
            .filter(|b| {
                doc.node(b.node)
                    .and_then(|n| n.element())
                    .is_some_and(|e| e.name == "p")
            })
            .collect();
        assert_eq!(paragraphs.len(), 512);
        assert_eq!(paragraphs[0].style, paragraphs[2].style);
        assert_eq!(paragraphs[1].style, paragraphs[3].style);
        assert_ne!(paragraphs[0].style, paragraphs[1].style);
        assert!(styled.style_count() <= 5);
        let output = layout(&styled, &mut Metrics, &Request::new(390.0, 844.0)).unwrap();
        assert_eq!(output.line_count, 512);
        assert!(output.page_bytes < 2 * 1024 * 1024);
    }

    #[test]
    fn every_style_and_layout_allocation_refusal_returns_its_charge() {
        use td_alloc::{with_reservation_limit, Budget};
        let html = "<body><div style='color:red'>alpha <b>beta gamma</b><img width=8 height=9><img width=7 height=6><i>tail</i></div><p>delta epsilon";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("body{--space:2px}div{padding:var(--space)}");
        let memory = Budget::new(8 * 1024 * 1024).unwrap();
        let base = memory.used();
        let mut constructed = false;
        for allowed in 0..4096 {
            let result = with_reservation_limit(allowed, || {
                StyledDocument::with_budget(&doc, &css, html.len(), Limits::default(), &memory)
            });
            constructed = result.is_ok();
            drop(result);
            assert_eq!(memory.used(), base, "style allocation {allowed}");
            if constructed {
                break;
            }
        }
        assert!(constructed);
        let styled =
            StyledDocument::with_budget(&doc, &css, html.len(), Limits::default(), &memory)
                .unwrap();
        let retained = memory.used();
        for width in [40.0, 400.0] {
            let mut laid_out = false;
            for allowed in 0..4096 {
                let result = with_reservation_limit(allowed, || {
                    layout(&styled, &mut Metrics, &Request::new(width, 100.0))
                });
                laid_out = result.is_ok();
                drop(result);
                assert_eq!(memory.used(), retained, "layout allocation {allowed}");
                if laid_out {
                    break;
                }
            }
            assert!(laid_out);
        }
        drop(styled);
        assert_eq!(memory.used(), base);
    }

    #[test]
    fn shared_document_and_overlapping_outputs_retain_the_page_budget() {
        use td_alloc::{Budget, BudgetShared};
        let root = Budget::new(8 * 1024 * 1024).unwrap();
        let base = root.used();
        let page = root.child(8 * 1024 * 1024 - base).unwrap();
        let html = "<p>alpha beta gamma delta epsilon zeta eta theta";
        let doc = td_html::document::parse_with_budget(html, Default::default(), &page).unwrap();
        let shared = BudgetShared::new(&page, doc).unwrap();
        let observer = shared.try_clone().unwrap();
        let css = sheets("");
        let styled =
            StyledDocument::shared(shared, &css, html.len(), Limits::default(), &page).unwrap();
        let first = page.child(1024 * 1024).unwrap();
        let old = crate::block::layout_with_budget(
            &styled,
            &mut Metrics,
            &Request::new(40.0, 100.0),
            &first,
        )
        .unwrap();
        let first_bytes = first.used();
        assert!(first_bytes > Budget::metadata_bytes());
        drop(first);
        let kept = root.used();
        // The caller leaves retained_output_bytes at zero. Actual owners still
        // enforce the root cap independently of that conservative admission hint.
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        assert!(matches!(
            layout(&styled, &mut Metrics, &Request::new(30.0, 100.0)),
            Err(Error::Memory(td_alloc::BudgetError::Limit))
        ));
        drop(blocker);
        assert_eq!(root.used(), kept);
        let new = layout(&styled, &mut Metrics, &Request::new(30.0, 100.0)).unwrap();
        let both = root.used();
        drop(old);
        assert_eq!(root.used(), both - first_bytes);
        // A completed worker owns the document; dropping its launcher's handles
        // does not return its storage while the result is still alive.
        drop(page);
        let styled = std::thread::spawn(move || styled).join().unwrap();
        assert_eq!(styled.document().len(), observer.len());
        drop((styled, observer, new));
        assert_eq!(root.used(), base);
    }
}
