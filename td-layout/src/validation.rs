//! Synthetic corpus shapes; no downloaded page bytes or installed fonts.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use crate::block::{layout, Request};
    use crate::inline::{DisplayText, FontLine, FontMetrics};
    use crate::{Error, Limits, StyledDocument, MAX_PAGE_BYTES, MAX_PAGE_INPUT, WORKING_BYTES};
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
                assert!(full.charged_bytes <= MAX_PAGE_BYTES);
                assert!(full.charged_bytes + WORKING_BYTES <= 288 * 1024 * 1024);
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
    fn dense_blocks_release_old_output_when_overlap_exceeds_page_budget() {
        let html = format!("<body>{}", "<p>abcdefghijklmnop</p>".repeat(256));
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let first = Request::new(1280.0, 720.0);
        let second = Request::new(832.0, 720.0);
        let full = layout(&styled, &mut Metrics, &first).unwrap();
        let resized = layout(&styled, &mut Metrics, &second).unwrap();
        let budget = full.charged_bytes.max(resized.charged_bytes);
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
        assert!(resized.charged_bytes <= budget);
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
        assert!(output.charged_bytes < 2 * 1024 * 1024);
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
