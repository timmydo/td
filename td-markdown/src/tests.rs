#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use crate::{
    parse, BlockKind, Document, Error, Level, Marker, SpanKind, Style, MAX_BYTES, MAX_DEPTH,
};

/// Each block as one line: `^` when a blank line came before it, its
/// containers (`>` a quote, `-`/`1.` an item's first block, `.` its later
/// ones), its kind, and its spans written back in a normal form.
fn show(source: &str) -> Vec<String> {
    let doc = parse(source).unwrap();
    check(source, &doc);
    doc.blocks
        .iter()
        .map(|block| {
            let mut line = String::new();
            if block.blank_before {
                line.push('^');
            }
            for level in doc.levels(block) {
                match level {
                    Level::Quote => line.push('>'),
                    Level::Item { first: false, .. } => line.push('.'),
                    Level::Item {
                        marker: Marker::Bullet(c),
                        ..
                    } => line.push(*c),
                    Level::Item {
                        marker: Marker::Ordered { number, delimiter },
                        ..
                    } => line.push_str(&format!("{number}{delimiter}")),
                }
            }
            match &block.kind {
                BlockKind::Paragraph => line.push_str("P "),
                BlockKind::Heading(level) => line.push_str(&format!("H{level} ")),
                BlockKind::Code { info } => {
                    line.push_str(&format!("C({}) ", crate::text(source, info)))
                }
                BlockKind::Rule => line.push('R'),
                BlockKind::Table => line.push_str("T "),
            }
            for span in doc.spans(block) {
                let text = crate::text(source, &span.range);
                let mut piece = match span.kind {
                    SpanKind::Text => text.to_string(),
                    SpanKind::Code => format!("`{text}`"),
                    SpanKind::SoftBreak => " ".to_string(),
                    SpanKind::HardBreak => "|".to_string(),
                    SpanKind::Image { source: image, .. } => {
                        let link = doc.links.get(image).unwrap();
                        format!("![{text}]({})", crate::text(source, &link.destination))
                    }
                };
                if span.style.has(Style::STRIKE) {
                    piece = format!("~{piece}~");
                }
                if span.style.has(Style::STRONG) {
                    piece = format!("<{piece}>");
                }
                if span.style.has(Style::EMPHASIS) {
                    piece = format!("/{piece}/");
                }
                if let Some(link) = span.link.and_then(|l| doc.links.get(l)) {
                    piece = format!("[{piece}]({})", crate::text(source, &link.destination));
                }
                line.push_str(&piece);
            }
            line
        })
        .collect()
}

/// Every range lies in the source on character boundaries, a block's
/// ranges index the document, and a block's spans lie in its source in
/// order.
fn check(source: &str, doc: &Document) {
    let ok = |range: &std::ops::Range<usize>| {
        range.start <= range.end
            && range.end <= source.len()
            && source.is_char_boundary(range.start)
            && source.is_char_boundary(range.end)
    };
    for block in &doc.blocks {
        assert!(ok(&block.source), "{block:?}");
        assert!(block.spans.end <= doc.spans.len());
        assert!(block.levels.end <= doc.levels.len());
        if let BlockKind::Code { info } = &block.kind {
            assert!(ok(info));
        }
        let mut at = block.source.start;
        for span in doc.spans(block) {
            assert!(
                at <= span.range.start && span.range.end <= block.source.end,
                "{span:?} after {at} in {block:?} of {source:?}"
            );
            at = span.range.end;
        }
    }
    for span in &doc.spans {
        assert!(ok(&span.range), "{span:?} in {source:?}");
        assert!(span.link.is_none_or(|l| l < doc.links.len()));
        if let SpanKind::Image {
            source,
            open,
            close,
        } = span.kind
        {
            assert!(source < doc.links.len());
            assert!(ok(&(open..close)), "{span:?} in {source:?}");
            assert!(open <= span.range.start && span.range.end <= close);
        }
    }
    for link in &doc.links {
        assert!(ok(&link.destination));
        assert!(link.title.as_ref().is_none_or(ok));
    }
}

#[test]
fn paragraphs_headings_and_rules() {
    assert_eq!(
        show("# Title #\n\nSome text\nwrapped.\n\n## Two\nAfter\n***\nSetext\n===\nOther\n---\n"),
        [
            "H1 Title",
            "^P Some text wrapped.",
            "^H2 Two",
            "P After",
            "R",
            "H1 Setext",
            "H2 Other",
        ]
    );
    assert_eq!(
        show("#notheading\n####### seven"),
        ["P #notheading ####### seven"]
    );
    assert_eq!(show("#"), ["H1 "]);
    assert_eq!(show(""), Vec::<String>::new());
    assert_eq!(show("\n\n"), Vec::<String>::new());
    assert_eq!(show("a\r\nb\r\n"), ["P a b"]);
}

#[test]
fn breaks_soft_and_hard() {
    assert_eq!(
        show("one  \ntwo\\\nthree\nfour   "),
        ["P one|two|three four"]
    );
}

#[test]
fn emphasis_strong_and_strike() {
    assert_eq!(show("*a* _b_ **c** __d__"), ["P /a/ /b/ <c> <d>"]);
    assert_eq!(show("***both***"), ["P /<both>/"]);
    assert_eq!(show("*a **b** c*"), ["P /a //<b>// c/"]);
    assert_eq!(
        show("snake_case_word and 2*3*4"),
        ["P snake_case_word and 2/3/4"]
    );
    assert_eq!(show("**unclosed"), ["P **unclosed"]);
    assert_eq!(show("**a*"), ["P */a/"]);
    // The rule of three.
    assert_eq!(show("*foo**bar**baz*"), ["P /foo//<bar>//baz/"]);
    assert_eq!(
        show("~~gone~~ ~one~ ~~~three~~~"),
        ["P ~gone~ ~one~ ~~~three~~~"]
    );
    assert_eq!(show("* not emphasis *"), ["*P not emphasis *"]);
    assert_eq!(show("a * b * c"), ["P a * b * c"]);
}

#[test]
fn code_spans_take_their_text_whole() {
    assert_eq!(show("`code` and ``a ` b``"), ["P `code` and `a ` b`"]);
    assert_eq!(show("`*not em*` *em*"), ["P `*not em*` /em/"]);
    assert_eq!(show("`unmatched and *em*"), ["P `unmatched and /em/"]);
    assert_eq!(show("`` ` ``"), ["P ```"]);
    assert_eq!(show("`a\nb`"), ["P `a` `b`"]);
}

#[test]
fn links_images_and_autolinks() {
    assert_eq!(
        show("[a *b*](http://x \"t\") and ![alt](i.png)"),
        ["P [a ](http://x)[/b/](http://x) and ![alt](i.png)"]
    );
    assert_eq!(show("[a](<b c>) [x]()"), ["P [a](b c) [x]()"]);
    assert_eq!(show("[not a link] [b]"), ["P [not a link] [b]"]);
    assert_eq!(show("[outer [inner](i)](o)"), ["P [outer [inner](i)](o)"]);
    assert_eq!(show("[a](b(c)d)"), ["P [a](b(c)d)"]);
    assert_eq!(
        show("<https://e.x/a?b> <not autolink>"),
        ["P [https://e.x/a?b](https://e.x/a?b) <not autolink>"]
    );
    assert_eq!(
        show("see https://e.x/p_(q). (https://e.x/y) xhttps://no"),
        ["P see [https://e.x/p_(q)](https://e.x/p_(q)). ([https://e.x/y](https://e.x/y)) xhttps://no"]
    );
    assert_eq!(show("[https://a](https://b)"), ["P [https://a](https://b)"]);
    let doc = parse("[a](d 'the title')").unwrap();
    let title = doc.links[0].title.clone().unwrap();
    assert_eq!(crate::text("[a](d 'the title')", &title), "the title");
}

#[test]
fn escapes_drop_their_backslash() {
    assert_eq!(show(r"\*not em\* \a \\"), [r"P *not em* \a \"]);
    assert_eq!(show(r"\# not heading"), ["P # not heading"]);
}

#[test]
fn block_quotes_nest_and_continue_lazily() {
    assert_eq!(
        show("> quoted\nlazy\n> > deeper\n>\n> back"),
        [">P quoted lazy", ">>P deeper", "^>P back"]
    );
    assert_eq!(show(">no space"), [">P no space"]);
}

#[test]
fn lists_bullet_ordered_and_nested() {
    assert_eq!(
        show("- one\n- two\n  more\n\n  second para\n- three\n  - nested\n    1. deep"),
        [
            "-P one",
            "-P two more",
            "^.P second para",
            "-P three",
            ".-P nested",
            "..1.P deep"
        ]
    );
    assert_eq!(show("3. three\n4) four"), ["3.P three", "4)P four"]);
    assert_eq!(show("-\n- b"), ["-P ", "-P b"]);
    assert_eq!(show("- a\n\n- b"), ["-P a", "^-P b"]);
    assert_eq!(
        show("-1. not\n1234567890. long"),
        ["P -1. not 1234567890. long"]
    );
}

#[test]
fn an_ordered_item_interrupts_a_paragraph_only_at_one() {
    assert_eq!(show("para\n2. no\n1. yes"), ["P para 2. no", "1.P yes"]);
    assert_eq!(show("para\n-\nnext"), ["H2 para", "P next"]);
}

#[test]
fn code_blocks_fenced_and_indented() {
    assert_eq!(
        show("```rust\nfn main() {\n\n    *x*\n}\n```\nafter"),
        ["C(rust) fn main() {||    *x*|}", "P after"]
    );
    assert_eq!(show("~~~\na\n~~~~\n"), ["C() a"]);
    assert_eq!(show("```\nunclosed"), ["C() unclosed"]);
    assert_eq!(
        show("    indented\n\n    more\n\n\npara"),
        ["C() indented||more", "^P para"]
    );
    assert_eq!(show("para\n    not code"), ["P para not code"]);
    assert_eq!(show("- item\n\n      code"), ["-P item", "^.C() code"]);
    assert_eq!(show("``` a`b\nnot a fence"), ["P ``` a`b not a fence"]);
    assert_eq!(show("> ```\n> q\n> ```"), [">C() q"]);
}

#[test]
fn pipe_tables_keep_their_lines() {
    assert_eq!(
        show("| a | b |\n|---|:-:|\n| 1 | 2 |\nafter\n\n| x |\n| y |"),
        ["T | a | b |||---|:-:||| 1 | 2 ||after", "^P | x | | y |"]
    );
}

#[test]
fn nesting_stops_at_the_depth() {
    let source = ">".repeat(MAX_DEPTH + 8) + " deep";
    let doc = parse(&source).unwrap();
    let block = doc.blocks.first().unwrap();
    assert_eq!(doc.levels(block).len(), MAX_DEPTH);
    assert!(crate::text(&source, &doc.spans(block)[0].range).starts_with(">>>>"));
}

#[test]
fn a_source_too_long_is_refused() {
    assert_eq!(parse(&"a".repeat(MAX_BYTES + 1)), Err(Error::TooLong));
}

#[test]
fn tables_split_an_intro_line_and_honour_escaped_pipes() {
    assert_eq!(
        show("Here it is:\n| a | b |\n|---|---|\n| 1 | 2 |"),
        ["P Here it is:", "T | a | b |||---|---||| 1 | 2 |"]
    );
    assert_eq!(
        show("| a \\| b | c |\n|---|---|"),
        [r"T | a \| b | c |||---|---|"]
    );
    assert_eq!(show("| a \\|\n|---|---|"), ["P | a | |---|---|"]);
    assert_eq!(show("| a \\\\|\n|---|"), [r"T | a \\|||---|"]);
}

#[test]
fn an_item_in_a_shifting_container_does_not_overflow() {
    assert_eq!(show(">- a\n>\tb"), [">-P a b"]);
    assert_eq!(show("-    - a\n\t\tb"), ["--P a b"]);
    assert_eq!(show("> - a\n   > b"), [">-P a b"]);
}

#[test]
fn links_images_and_urls_keep_their_own() {
    assert_eq!(
        show("[![img](src)](href) ![a *b* [c](d)](e)"),
        ["P [![img](src)](href) ![a *b* [c](d)](e)"]
    );
    assert_eq!(
        show("[see https://x.com/a](https://y.com)"),
        ["P [see https://x.com/a](https://y.com)"]
    );
    assert_eq!(
        show("https://x.com/`a b` [x https://y](z"),
        ["P [https://x.com/](https://x.com/)`a b` [x [https://y](https://y)](z"]
    );
    // A title is one line, and one in parentheses holds none unescaped.
    assert_eq!(show("[a](b \"t\nu\")"), ["P [a](b \"t u\")"]);
    assert_eq!(show("[a](b (t(u)))"), ["P [a](b (t(u)))"]);
    assert_eq!(show(r"[a](b (t\(u))"), ["P [a](b)"]);
    // An escaped line ending is one too.
    assert_eq!(show("> [a](<b\\\n> c>)"), [">P [a](<b|c>)"]);
    assert_eq!(show("> [a](b \"t\\\n> u\")"), [">P [a](b \"t|u\")"]);
}

#[test]
fn breaks_sit_at_their_line_ends() {
    for (source, at) in [
        ("![x](y)\nnext", 7),
        ("x <http://a> \ny", 13),
        ("x\\\ny", 2),
        ("a  \nb", 3),
    ] {
        let doc = parse(source).unwrap();
        let found = doc
            .spans
            .iter()
            .find(|span| matches!(span.kind, SpanKind::SoftBreak | SpanKind::HardBreak))
            .unwrap();
        assert_eq!(found.range, at..at, "{source:?}");
    }
    // Tabs make no hard break.
    assert_eq!(show("a\t\t\nb"), ["P a b"]);
}

#[test]
fn block_sources_cover_their_markup() {
    let source = "```\na\n```\n- hi\n-\n\n# Heading \\#";
    let doc = parse(source).unwrap();
    let sources: Vec<&str> = doc
        .blocks
        .iter()
        .map(|block| crate::text(source, &block.source))
        .collect();
    assert_eq!(sources, ["```\na\n```", "hi", "-", r"# Heading \#"]);
    assert_eq!(show(source).last().unwrap(), "^H1 Heading #");
}

#[test]
fn an_item_opened_blank_closes_at_a_blank_line() {
    assert_eq!(show("-\n\n  foo"), ["-P ", "^P foo"]);
    assert_eq!(show("-\n  foo"), ["-P foo"]);
}

/// Each shape at a size where quadratic work takes seconds even
/// optimised, and linear work milliseconds even unoptimised.
#[test]
fn pathological_runs_stay_fast() {
    for source in [
        "*a ".repeat(50_000),
        "_a _".repeat(50_000),
        "[".repeat(100_000),
        "[a](".repeat(50_000),
        "**a*".repeat(50_000),
        "`".repeat(5_000) + &"``a".repeat(5_000),
        "<a".repeat(50_000),
        "- ".repeat(50_000),
        "> ".repeat(50_000),
        "[".repeat(50_000) + &"[a](b)".repeat(50_000),
        "a\n".to_string() + &"\\\n".repeat(50_000),
        "![](x)\n".repeat(50_000),
        "![".repeat(50_000) + "x" + &"](u)".repeat(50_000),
        "[](x (".repeat(50_000),
        "http://a".to_string() + &")".repeat(50_000),
        "http://a".to_string() + &".)".repeat(50_000),
    ] {
        let start = std::time::Instant::now();
        let doc = parse(&source).unwrap();
        check(&source, &doc);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 1000,
            "{elapsed:?} for {:?}",
            source.get(..20)
        );
    }
}

#[test]
fn any_input_parses_to_ranges_inside_it() {
    let alphabet = [
        "*", "_", "~", "`", "[", "]", "(", ")", "!", "<", ">", "\\", "\n", " ", "  ", "\t", "#",
        "-", "1.", "|", "a", "é", "http://x", ":", "\"", "'", "    ", "```", "=",
    ];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..3000 {
        let mut source = String::new();
        for _ in 0..(seed % 60) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            source.push_str(alphabet[(seed % alphabet.len() as u64) as usize]);
        }
        seed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
        let doc = parse(&source).unwrap();
        check(&source, &doc);
    }
}

#[test]
fn an_images_markup_runs_from_its_bang_past_its_parenthesis() {
    for (source, markup) in [
        ("x ![a](b) y", "![a](b)"),
        ("x ![a](b \"t\" ) y", "![a](b \"t\" )"),
        ("x ![a](b (t)) y", "![a](b (t))"),
        ("x ![a](<b c>) (y)", "![a](<b c>)"),
        ("> x ![a\n> b](c) y", "![a\n> b](c)"),
        ("[![a](b)](c)", "![a](b)"),
    ] {
        let doc = parse(source).unwrap();
        let (open, close) = doc
            .spans
            .iter()
            .find_map(|span| match span.kind {
                SpanKind::Image { open, close, .. } => Some((open, close)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no image in {source:?}"));
        assert_eq!(&source[open..close], markup, "{source:?}");
    }
}
