#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
fn extract(source: &str, final_scalar: bool, gaps: bool) -> (String, Diagnostics) {
    let chars: Vec<_> = source.chars().collect();
    let mut cursor = Extractor::new();
    let mut offset = 0;
    let mut output = String::new();
    for _ in 0..chars.len() * 40 + 200 {
        if gaps {
            let progress = cursor.poll(None, false).unwrap();
            assert!(!progress.consumed);
            if let Some(c) = progress.scalar {
                output.push(c);
            }
            if progress.status == Status::Complete {
                assert_eq!(offset, chars.len());
                return (output, cursor.diagnostics());
            }
        }
        let last = offset == chars.len() || (final_scalar && offset + 1 == chars.len());
        let progress = cursor.poll(chars.get(offset).copied(), last).unwrap();
        if progress.consumed {
            offset += 1;
        }
        if let Some(c) = progress.scalar {
            output.push(c);
        }
        if progress.status == Status::Complete {
            assert_eq!(offset, chars.len());
            assert_eq!(
                cursor.poll(Some('!'), true).unwrap(),
                Progress {
                    consumed: false,
                    scalar: None,
                    status: Status::Complete,
                }
            );
            return (output, cursor.diagnostics());
        }
    }
    panic!("extraction stalled");
}
fn check(source: &str, expected: &str, malformed: bool) {
    for last in [false, true] {
        for gaps in [false, true] {
            assert_eq!(
                extract(source, last, gaps),
                (
                    expected.to_owned(),
                    Diagnostics {
                        malformed_markup: malformed,
                    }
                ),
                "source {source:?}, last={last}, gaps={gaps}"
            );
        }
    }
}
#[test]
fn text_blocks_inline_tags_and_references_have_literal_projection() {
    for (source, expected) in [
        ("", ""),
        ("plain 😀 text", "plain 😀 text"),
        ("<span>Hel</span>lo", "Hello"),
        ("a<div>b</div>c<br>d", "a b c d"),
        ("a<unknown>b</unknown>c", "abc"),
        (
            "A&nbsp;B\u{a0}C &amp; &lt; &gt; &quot; &apos;",
            "A B C & < > \" '",
        ),
        (
            "&#128; &#0; &#xD800; &#x110000; &copy; &wat;",
            "€ � � � &copy; &wat;",
        ),
        ("&ampx; &#x; &amp", "&x; &#x; &"),
        (
            "&lt;script&gt;literal&lt;/script&gt;",
            "<script>literal</script>",
        ),
    ] {
        check(source, expected, false);
    }
    for tag in [
        "address",
        "article",
        "aside",
        "blockquote",
        "br",
        "div",
        "dl",
        "dt",
        "dd",
        "fieldset",
        "figcaption",
        "figure",
        "footer",
        "form",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "header",
        "hr",
        "li",
        "main",
        "nav",
        "ol",
        "p",
        "pre",
        "section",
        "table",
        "td",
        "th",
        "tr",
        "ul",
    ] {
        check(
            &format!("x<{}>y</{tag}>z", tag.to_ascii_uppercase()),
            "x y z",
            false,
        );
    }
}
#[test]
fn attributes_quotes_and_reference_boundaries_are_streamed() {
    for (source, expected) in [
        (
            "a<img alt='hello &amp; bye' title=ok src=secret>b",
            "a hello & bye  ok b",
        ),
        (
            "a<a href='secret' data-title='hidden' TITLE=seen>b</a>c",
            "a seen bc",
        ),
        ("a<i alt='' title>b", "a  b"),
        ("<img alt='x>y' title=one/two>", " x>y  one/two "),
        (
            "<img alt='&ampx &amp=1 &amp; &quot;'>",
            " &ampx &amp=1 & \" ",
        ),
        ("<img alt=&amp;>", " & "),
        ("x</span title=hidden>y", "xy"),
        ("<div alt='a'>b</div>", " a  b "),
        ("<img alt='<' title='&lt;'>", " <  < "),
    ] {
        check(source, expected, false);
    }
}
#[test]
fn ignored_contexts_do_not_leak_text_attributes_or_fake_closes() {
    for (source, expected) in [
        ("a<!-- <img alt=secret> &amp; -->b", "ab"),
        ("a<!DOCTYPE html PUBLIC 'x>y'>b<?hidden?>c", "ay'>bc"),
        (
            "a<head title=secret>hidden<img alt=secret><style>x</style></head>b",
            "ab",
        ),
        (
            "a<script alt=secret>hidden </scriptx> still <img alt=secret></ScRiPt >b",
            "ab",
        ),
        ("a<style>hidden </style!> still << /style> </STYLE>b", "ab"),
        (
            "<head><script>fake </head> </script> hidden</head>visible",
            "visible",
        ),
        ("a<script/>hidden</script>b", "ab"),
        ("a<style> <!-- </style>b", "ab"),
        ("a<!---x--->b", "ab"),
    ] {
        check(source, expected, false);
    }
}
#[test]
fn truncated_markup_reports_diagnostics_without_unbounded_retention() {
    for (source, expected) in [
        ("a<", "a<"),
        ("a< ", "a< "),
        ("a</", "a"),
        ("a<div", "a"),
        ("a<!--x", "a"),
        ("a<!DOCTYPE x", "a"),
        ("a<head>x", "a"),
        ("a<script>x", "a"),
        ("a<style>x</styl", "a"),
        ("a<img alt='visible", "a visible "),
        ("a<img title=&amp", "a & "),
        ("a<img alt=>b", "ab"),
        ("a</!>b", "ab"),
        ("a< /x>b", "a< /x>b"),
    ] {
        check(source, expected, true);
    }
    assert!(std::mem::size_of::<Extractor>() <= 256);
    check(
        &format!("a<{} title=visible>b", "x".repeat(4096)),
        "a visible b",
        false,
    );
    check(
        &format!("a<img {}=hidden alt=shown>b", "x".repeat(4096)),
        "a shown b",
        false,
    );
    check(
        &format!("a<script>{}</script>b", "<scriptx> &amp;".repeat(4096)),
        "ab",
        false,
    );
    check(&format!("a<!--{}-->b", "-x".repeat(4096)), "ab", false);
}

#[test]
fn malformed_attributes_and_all_truncation_points_finish_with_bounded_progress() {
    for (source, expected) in [
        ("<img alt=a=b>", " a=b "),
        ("a<div=x>b", "ab"),
        ("<img alt='a'title='b'>", " a  b "),
        ("<img alt=a`b>", " a`b "),
        ("<img alt=a<b>", " a<b "),
        ("<img =bogus alt=x>", " x "),
        ("<img / alt=x>", " x "),
    ] {
        check(source, expected, true);
    }
    for source in [
        "<head><script>x</script></head>visible",
        "x<div alt='&amp;' title=&#x1f600;>text</div>",
        "x<!--- comment --->y<!DOCTYPE html 'q>r'>z",
        "<style>x</stylex>ignored</style>tail",
    ] {
        for end in 0..=source.len() {
            if let Some(prefix) = source.get(..end) {
                let baseline = extract(prefix, false, false);
                for last in [false, true] {
                    for gaps in [false, true] {
                        assert_eq!(extract(prefix, last, gaps), baseline, "{prefix:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn literal_angles_implicit_body_comment_ends_and_nul_have_literal_oracles() {
    check(
        "Price < 5. Don't miss <b>this</b>!",
        "Price < 5. Don't miss this!",
        true,
    );
    check("I <3 you <b>x</b>", "I <3 you x", true);
    check(
        "<html><head><meta charset=utf-8><body><p>Hello</p>",
        " Hello ",
        false,
    );
    check(
        "<head><title></head>Only in title</title></head>shown",
        "shown",
        false,
    );
    check("a<!-->b<!-- c -->d", "abd", true);
    check("a<!--->b", "ab", true);
    check("a<!-- x --!>b", "ab", true);
    check("a\0b<img alt='c\0d'>", "a�b c�d ", true);
    let mut cursor = Extractor::new();
    assert_eq!(cursor.poll(None, false).unwrap().status, Status::NeedInput);
    assert_eq!(
        cursor.poll(Some('x'), true).unwrap(),
        Progress {
            consumed: true,
            scalar: Some('x'),
            status: Status::Yield
        }
    );
    assert_eq!(cursor.poll(None, false).unwrap().status, Status::Complete);
    let mut invalid = Extractor::new();
    invalid.phase = Phase::Name;
    invalid.name_len = usize::MAX;
    assert_eq!(invalid.poll(Some('x'), false), Err(Error::State));
    assert_eq!(invalid.poll(None, true), Err(Error::State));
}
