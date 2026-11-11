//! Synthetic corpus shapes; no downloaded page bytes or installed fonts.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
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
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
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
                .cloned()
                .collect();
            assert_eq!(window.lines, expected);
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
