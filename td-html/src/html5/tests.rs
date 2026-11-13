#![cfg(test)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
fn tokens(s: &str) -> Vec<Token> {
    let mut t = Tokenizer::new(s, ParseLimits::default()).unwrap();
    let mut out = Vec::new();
    while let Some(x) = t.next_token().unwrap() {
        out.push(x);
    }
    out
}
fn owned(s: &str) -> td_alloc::BudgetString {
    let budget = td_alloc::Budget::new(MAX_TREE_BYTES).unwrap();
    td_alloc::BudgetString::copy(&budget, s).unwrap()
}
fn text(s: &str) -> Token {
    Token::Text(owned(s))
}
fn comment(s: &str) -> Token {
    Token::Comment(owned(s))
}
fn tag(name: &str, attrs: &[(&str, &str)], end: bool, close: bool) -> Token {
    let budget = td_alloc::Budget::new(MAX_TREE_BYTES).unwrap();
    let mut attributes = td_alloc::BudgetVec::new(&budget).unwrap();
    for (k, v) in attrs {
        attributes
            .try_push(Attribute {
                name: owned(k),
                value: owned(v),
            })
            .unwrap();
    }
    Token::Tag(Tag {
        name: owned(name),
        attributes,
        end,
        self_closing: close,
    })
}
// Original small fixtures adapted from html5lib-tests/tokenizer/test1.test:
// tag, duplicate attribute, comments and malformed opener cases. WHATWG
// "Tokenization": data/tag open, attribute states, comment states.
#[test]
fn html5lib_data_tag_and_comment_examples() {
    let cases = vec![
        ("<h>", vec![tag("h", &[], false, false)]),
        ("</>", vec![]),
        ("<>", vec![text("<>")]),
        ("<h a='b'>", vec![tag("h", &[("a", "b")], false, false)]),
        ("<h a=b>", vec![tag("h", &[("a", "b")], false, false)]),
        (
            "<h></h a='b'>",
            vec![tag("h", &[], false, false), tag("h", &[], true, false)],
        ),
        (
            "<H A='b' a='d'>",
            vec![tag("h", &[("a", "b")], false, false)],
        ),
        (
            "<h a='b'c='d'>",
            vec![tag("h", &[("a", "b"), ("c", "d")], false, false)],
        ),
        (
            "<p>One<p>Two",
            vec![
                tag("p", &[], false, false),
                text("One"),
                tag("p", &[], false, false),
                text("Two"),
            ],
        ),
        ("<!--comment-->", vec![comment("comment")]),
        ("<!----->", vec![comment("-")]),
        ("<!-- --comment -->", vec![comment(" --comment ")]),
        ("<!--<!-->", vec![comment("<!")]),
        ("<!-- <!--", vec![comment(" <!")]),
        ("<!--comment", vec![comment("comment")]),
        ("<!-", vec![comment("-")]),
        ("<!-->", vec![comment("")]),
        ("<!--->", vec![comment("")]),
        ("<!-- <test-->", vec![comment(" <test")]),
        ("<!DOC>", vec![comment("DOC")]),
        ("<?foo?>", vec![comment("?foo?")]),
        ("</5>", vec![comment("5")]),
    ];
    for (input, expected) in cases {
        assert_eq!(tokens(input), expected, "{input:?}");
    }
}
// html5lib test1's doctype cases, plus EOF/quoted-identifier shapes from
// WHATWG's DOCTYPE public/system and bogus DOCTYPE states.
#[test]
fn doctypes_and_abrupt_identifiers() {
    for (input, name, public, system, quirks) in [
        ("<!DOCTYPE HtMl>", "html", None, None, false),
        ("<!DOCTYPE HtMl", "html", None, None, true),
        ("<!doctype>", "", None, None, true),
        (
            "<!doctype html PUBLIC 'x' 'y'>",
            "html",
            Some("x"),
            Some("y"),
            false,
        ),
        ("<!doctype html SYSTEM 'x'>", "html", None, Some("x"), false),
        ("<!doctype html PUBLIC 'x>", "html", Some("x"), None, true),
        ("<!doctype html SYSTEM 'x>", "html", None, Some("x"), true),
        (
            "<!doctype html SYSTEM 'x' rubbish>",
            "html",
            None,
            Some("x"),
            false,
        ),
        (
            "<!doctype html PUBLIC 'x' rubbish>",
            "html",
            Some("x"),
            None,
            true,
        ),
        ("<!doctype html rubbish>", "html", None, None, true),
    ] {
        assert_eq!(
            tokens(input),
            vec![Token::Doctype(Doctype {
                name: owned(name),
                public_id: public.map(owned),
                system_id: system.map(owned),
                force_quirks: quirks
            })],
            "{input}"
        );
    }
    assert_eq!(
        tokens("<!doctype html PUBLIC 'x><p>"),
        vec![
            Token::Doctype(Doctype {
                name: owned("html"),
                public_id: Some(owned("x")),
                system_id: None,
                force_quirks: true
            }),
            tag("p", &[], false, false)
        ]
    );
}
// WHATWG named/numeric reference states; examples of legacy ambiguity and
// two-scalar entities absent from the frozen resident vocabulary.
#[test]
fn character_references() {
    assert_eq!(
        tokens("&notit; &notin; &amp= &NotEqualTilde; &#x80; &#0; &#x110000; &#xD800; &#x; &#;"),
        vec![text("¬it; ∉ &= ≂\u{338} € � � � &#x; &#;")]
    );
    assert_eq!(
        tokens("<a a='&notit; &amp= &notin; &#65x' b=&amp;>"),
        vec![tag(
            "a",
            &[("a", "&notit; &amp= ∉ Ax"), ("b", "&")],
            false,
            false
        )]
    );
    for &(name, value) in entities::ENTITIES {
        assert_eq!(tokens(&format!("&{name}")), vec![text(value)], "{name}");
    }
    assert_eq!(tokens(&format!("&#{};", "9".repeat(1000))), vec![text("�")]);
}
#[test]
fn raw_text_modes_are_controlled_by_the_builder() {
    for (mode, name, input, expected) in [
        (
            TextMode::Rcdata,
            "textarea",
            "&amp;<b></textareax></TEXTAREA>",
            "&<b></textareax>",
        ),
        (TextMode::Rawtext, "style", "&amp;<b></STYLE>", "&amp;<b>"),
        (
            TextMode::Script,
            "script",
            "<!--<script>x</script>--></script>",
            "<!--<script>x</script>-->",
        ),
        (TextMode::Script, "script", "<!--x</script>", "<!--x"),
        (
            TextMode::Script,
            "script",
            "<script>x</script>",
            "<script>x",
        ),
    ] {
        let source = format!("<{name}>{input}");
        let mut t = Tokenizer::new(&source, ParseLimits::default()).unwrap();
        t.next_token().unwrap();
        t.set_text_mode(mode);
        assert_eq!(t.next_token().unwrap(), Some(text(expected)), "{input}");
        assert_eq!(t.next_token().unwrap(), Some(tag(name, &[], true, false)));
        assert_eq!(t.next_token().unwrap(), None);
    }
    assert_eq!(
        tokens("<script><b>"),
        vec![
            tag("script", &[], false, false),
            tag("b", &[], false, false)
        ]
    );
    let mut t = Tokenizer::new("a\0&amp;<b>", ParseLimits::default()).unwrap();
    t.set_text_mode(TextMode::Plaintext);
    assert_eq!(t.next_token().unwrap(), Some(text("a�&amp;<b>")));
}
#[test]
fn preprocessing_cdata_and_eof() {
    assert_eq!(tokens("\u{feff}a\r\nb\rc\0"), vec![text("a\nb\nc\0")]);
    assert_eq!(tokens("<![CDATA[x]]>"), vec![comment("[CDATA[x]]")]);
    let mut t = Tokenizer::new("<![CDATA[x]]>y", ParseLimits::default()).unwrap();
    t.set_foreign(true);
    assert_eq!(t.next_token().unwrap(), Some(text("x")));
    assert_eq!(t.next_token().unwrap(), Some(text("y")));
    for s in [
        "<a", "<a ", "<a x", "<a x=", "<a x='", "<a x=\"", "<a x=y", "<a x='y'", "<a/",
    ] {
        assert!(tokens(s).is_empty(), "{s}");
    }
    assert_eq!(tokens("</"), vec![text("</")]);
    for s in ["<!--", "<!---", "<!----", "<!----!"] {
        assert_eq!(tokens(s), vec![comment("")], "{s}");
    }
    let mut t = Tokenizer::new("", ParseLimits::default()).unwrap();
    assert_eq!(t.next_token().unwrap(), None);
    assert_eq!(t.next_token().unwrap(), None);
}
fn run(s: &str, limits: ParseLimits) -> Result<(), ParseError> {
    let mut t = Tokenizer::new(s, limits)?;
    while t.next_token()?.is_some() {}
    Ok(())
}
#[test]
fn tokenizer_admission_edges_and_terminal_errors() {
    for n in [2, 3, 4] {
        assert_eq!(
            run(
                "abc",
                ParseLimits {
                    input: n,
                    ..Default::default()
                }
            ),
            if n < 3 {
                Err(ParseError::Input)
            } else {
                Ok(())
            }
        );
        assert_eq!(
            run(
                "<abc>",
                ParseLimits {
                    name_bytes: n,
                    ..Default::default()
                }
            ),
            if n < 3 {
                Err(ParseError::NameBytes)
            } else {
                Ok(())
            }
        );
        assert_eq!(
            run(
                "<a b='abc'>",
                ParseLimits {
                    attribute_bytes: n,
                    ..Default::default()
                }
            ),
            if n < 3 {
                Err(ParseError::AttributeBytes)
            } else {
                Ok(())
            }
        );
        assert_eq!(
            run(
                "<a x y z>",
                ParseLimits {
                    attributes_per_tag: n,
                    ..Default::default()
                }
            ),
            if n < 3 {
                Err(ParseError::AttributesPerTag)
            } else {
                Ok(())
            }
        );
        assert_eq!(
            run(
                "<a x><b y><c z>",
                ParseLimits {
                    attributes: n,
                    ..Default::default()
                }
            ),
            if n < 3 {
                Err(ParseError::Attributes)
            } else {
                Ok(())
            }
        );
    }
    assert_eq!(
        run(
            "<a x x x>",
            ParseLimits {
                attributes_per_tag: 2,
                ..Default::default()
            }
        ),
        Err(ParseError::AttributesPerTag)
    );
    for bytes in [5, 6, 7] {
        assert_eq!(
            run(
                "abc",
                ParseLimits {
                    bytes,
                    ..Default::default()
                }
            ),
            if bytes < 6 {
                Err(ParseError::Bytes)
            } else {
                Ok(())
            }
        );
    }
    for work in [2, 3, 4] {
        assert_eq!(
            run(
                "abc",
                ParseLimits {
                    work,
                    ..Default::default()
                }
            ),
            if work < 3 {
                Err(ParseError::Work)
            } else {
                Ok(())
            }
        );
    }
    let mut t = Tokenizer::new(
        "abcdef",
        ParseLimits {
            work: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(t.next_token(), Err(ParseError::Work));
    assert_eq!(t.next_token(), Err(ParseError::Work));
    assert!(Tokenizer::new(&"x".repeat(MAX_INPUT_BYTES), ParseLimits::default()).is_ok());
    assert!(matches!(
        Tokenizer::new(
            &"x".repeat(MAX_INPUT_BYTES + 1),
            ParseLimits {
                input: usize::MAX,
                ..Default::default()
            }
        ),
        Err(ParseError::Input)
    ));
    assert_eq!(
        run(
            &format!("<{}>", "a".repeat(MAX_NAME_BYTES)),
            ParseLimits::default()
        ),
        Ok(())
    );
    assert_eq!(
        run(
            &format!("<{}>", "a".repeat(MAX_NAME_BYTES + 1)),
            ParseLimits::default()
        ),
        Err(ParseError::NameBytes)
    );
    assert_eq!(
        run(
            &format!("<a b='{}'>", "x".repeat(MAX_ATTRIBUTE_BYTES)),
            ParseLimits::default()
        ),
        Ok(())
    );
    assert_eq!(
        run(
            &format!("<a b='{}'>", "x".repeat(MAX_ATTRIBUTE_BYTES + 1)),
            ParseLimits::default()
        ),
        Err(ParseError::AttributeBytes)
    );
}
#[test]
fn escaped_script_opener_retains_its_dash_state() {
    for opener in ["<!-->", "<!--->"] {
        let source = format!("<script>{opener}<script></script><p>visible</p>");
        let mut t = Tokenizer::new(&source, ParseLimits::default()).unwrap();
        t.next_token().unwrap();
        t.set_text_mode(TextMode::Script);
        assert_eq!(
            t.next_token().unwrap(),
            Some(text(&format!("{opener}<script>")))
        );
        assert_eq!(
            t.next_token().unwrap(),
            Some(tag("script", &[], true, false))
        );
        assert_eq!(t.next_token().unwrap(), Some(tag("p", &[], false, false)));
    }
}
#[test]
fn malformed_recoveries_count_diagnostics() {
    for input in [
        "&#0;",
        "&#xD800;",
        "&#x110000;",
        "&#xFFFF;",
        "&#13;",
        "&#x80;",
        "&doesnotexist;",
        "<a b=f<>",
        "<a\0>",
        "<a x='\0'>",
        "<!--\0-->",
        "<!foo\0>",
        "<!doctype html SYSTEM '\0'>",
    ] {
        let mut t = Tokenizer::new(input, ParseLimits::default()).unwrap();
        while t.next_token().unwrap().is_some() {}
        assert_eq!(
            t.errors(),
            if input.starts_with("<!foo") { 2 } else { 1 },
            "{input:?}"
        );
    }
    let mut t = Tokenizer::new("<!--x", ParseLimits::default()).unwrap();
    t.set_text_mode(TextMode::Script);
    t.next_token().unwrap();
    assert_eq!(t.errors(), 1);
    t.next_token().unwrap();
    assert_eq!(t.errors(), 1);
}

#[test]
fn diagnostics_are_single_counts_at_reconsume_and_eof() {
    for (input, expected) in [
        ("&;", 0),
        ("<!----!\0-->", 1),
        ("<!--\0", 2),
        ("<!doctype", 1),
        ("<!doctype>", 1),
        ("<!doctype html PUBLIC>", 1),
        ("<a/", 1),
        ("</a x/>", 2),
        ("<a x x=", 2),
        ("\u{ffff}", 1),
        ("\x01", 1),
    ] {
        let mut t = Tokenizer::new(input, ParseLimits::default()).unwrap();
        while t.next_token().unwrap().is_some() {}
        assert_eq!(t.errors(), expected, "{input:?}");
    }
    assert_eq!(
        run(
            "<a b='&#x'>",
            ParseLimits {
                attribute_bytes: 2,
                ..Default::default()
            }
        ),
        Err(ParseError::AttributeBytes)
    );
}
