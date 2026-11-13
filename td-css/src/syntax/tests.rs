#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use super::*;

fn tokens(input: &str) -> Vec<Token> {
    tokenize(input).unwrap().into_inner().into_iter().collect()
}

fn ident(input: &str) -> Token {
    Token::Ident(crate::test_tree::owned_text(input))
}

#[test]
fn syntax_section_two_rule_and_at_rule_examples() {
    // Syntax §2: the basic rule and the three shapes of at-rule.
    let parsed = parse_stylesheet(
        "p > a { color: blue; text-decoration: underline; }
         @import \"my-styles.css\";
         @page :left { margin-left: 4cm; margin-right: 3cm; }
         @media print { body { font-size: 10pt } }",
    )
    .unwrap();
    assert_eq!(parsed.errors(), 0);
    let rules = parsed.value();
    assert_eq!(rules.len(), 4);
    assert_eq!(rules[0].at_name(), None);
    let declarations = rules[0].declarations().unwrap();
    assert_eq!(declarations.value().len(), 2);
    assert_eq!(declarations.value()[0].name(), "color");
    assert_eq!(rules[1].at_name(), Some("import"));
    assert!(rules[1].block().is_none());
    assert_eq!(rules[2].at_name(), Some("page"));
    assert_eq!(rules[3].at_name(), Some("media"));
    assert!(matches!(
        rules[3].block().unwrap().last(),
        Some(Component::Token(Token::Whitespace))
    ));
}

#[test]
fn escapes_names_hash_types_and_preprocessing() {
    // Syntax §2.1 and §4.3.7: escape terminators and replacement code points.
    assert_eq!(
        tokens(r"\26 B \000026B \26B"),
        vec![
            ident("&B"),
            Token::Whitespace,
            ident("&B"),
            Token::Whitespace,
            ident("ɫ")
        ]
    );
    assert_eq!(
        tokens("\u{feff}a\r\nb\rc\x0cd\0"),
        vec![
            ident("a"),
            Token::Whitespace,
            ident("b"),
            Token::Whitespace,
            ident("c"),
            Token::Whitespace,
            ident("d�")
        ]
    );
    assert_eq!(tokens(r"\0 \d800 \110000 "), vec![ident("���")]);
    assert_eq!(
        tokens("#a #123 #\\31 23"),
        vec![
            Token::Hash {
                value: crate::test_tree::owned_text("a"),
                id: true
            },
            Token::Whitespace,
            Token::Hash {
                value: crate::test_tree::owned_text("123"),
                id: false
            },
            Token::Whitespace,
            Token::Hash {
                value: crate::test_tree::owned_text("123"),
                id: true
            },
        ]
    );
    assert_eq!(
        tokens("--x -é _x \\"),
        vec![
            ident("--x"),
            Token::Whitespace,
            ident("-é"),
            Token::Whitespace,
            ident("_x"),
            Token::Whitespace,
            ident("�")
        ]
    );
}

#[test]
fn comments_do_not_merge_tokens_or_introduce_whitespace() {
    assert_eq!(tokens("ab/**/cd"), vec![ident("ab"), ident("cd")]);
    assert_eq!(
        tokens("a/*x*/ /*y*/b"),
        vec![ident("a"), Token::Whitespace, ident("b")]
    );
    let parsed = tokenize("a/*unterminated").unwrap();
    assert_eq!(parsed.value().as_slice(), &vec![ident("a")]);
    assert_eq!(parsed.errors(), 1);
}

#[test]
fn numbers_dimensions_and_exponents_keep_integer_type() {
    let cases = [
        ("+12", "+12", true, "number"),
        ("-.5em", "-.5", false, "em"),
        ("1e-2%", "1e-2", false, "%"),
        ("1E+2px", "1E+2", false, "px"),
        ("10e", "10", true, "e"),
        ("1e9999", "1e9999", false, "number"),
    ];
    for (input, representation, integer, unit) in cases {
        let number = Number {
            representation: crate::test_tree::owned_text(representation),
            integer,
        };
        let expected = match unit {
            "number" => Token::Number(number),
            "%" => Token::Percentage(number),
            _ => Token::Dimension(number, crate::test_tree::owned_text(unit)),
        };
        assert_eq!(tokens(input), vec![expected], "{input}");
    }
    assert_eq!(
        tokens("1."),
        vec![
            Token::Number(Number {
                representation: crate::test_tree::owned_text("1"),
                integer: true
            }),
            Token::Delim('.')
        ]
    );
}

#[test]
fn strings_newlines_and_eof_recover() {
    assert_eq!(
        tokens("'ab\\\ncd'"),
        vec![Token::String(crate::test_tree::owned_text("abcd"))]
    );
    assert_eq!(
        tokens("'a\nb"),
        vec![Token::BadString, Token::Whitespace, ident("b")]
    );
    let parsed = tokenize("\"abc\\").unwrap();
    assert_eq!(
        parsed.value().as_slice(),
        &vec![Token::String(crate::test_tree::owned_text("abc"))]
    );
    assert_eq!(parsed.errors(), 1);
    assert_eq!(tokens("\\\n"), vec![Token::Delim('\\'), Token::Whitespace]);
}

#[test]
fn urls_quoted_unquoted_bad_and_eof() {
    assert_eq!(
        tokens("URL(  a\\ b )"),
        vec![Token::Url(crate::test_tree::owned_text("a b"))]
    );
    assert_eq!(
        tokens("url( 'a')"),
        vec![
            Token::Function(crate::test_tree::owned_text("url")),
            Token::Whitespace,
            Token::String(crate::test_tree::owned_text("a")),
            Token::Close(')')
        ]
    );
    assert_eq!(tokens("url(a b)z"), vec![Token::BadUrl, ident("z")]);
    assert_eq!(tokens("url(a'\\)bad)z"), vec![Token::BadUrl, ident("z")]);
    assert_eq!(tokens("url(a\x7f)z"), vec![Token::BadUrl, ident("z")]);
    assert_eq!(tokens("url(a\\\n)z"), vec![Token::BadUrl, ident("z")]);
    let parsed = tokenize("url(a").unwrap();
    assert_eq!(
        parsed.value().as_slice(),
        &vec![Token::Url(crate::test_tree::owned_text("a"))]
    );
    assert_eq!(parsed.errors(), 1);
}

#[test]
fn cdo_cdc_only_ignored_at_stylesheet_level() {
    assert_eq!(
        tokens("<!-- -->"),
        vec![Token::Cdo, Token::Whitespace, Token::Cdc]
    );
    let rules = parse_stylesheet("<!-- p{} --> q{} ").unwrap().into_inner();
    assert_eq!(rules.len(), 2);
    let declarations = parse_declarations("<!-- color:red; color:blue").unwrap();
    assert_eq!(declarations.value().len(), 1);
    assert_eq!(
        declarations.value()[0].value(),
        &[Component::Token(ident("blue"))]
    );
}

#[test]
fn balanced_components_hide_separators_and_recover_at_eof() {
    let values = parse_components("f(a; [b; {c}]) tail").unwrap();
    assert_eq!(values.errors(), 0);
    assert_eq!(values.value().len(), 3);
    let values = parse_components("f([{").unwrap();
    assert_eq!(values.errors(), 3);
    assert!(matches!(&values.value()[0], Component::Function { values, .. } if values.len() == 1));
    assert_eq!(parse_stylesheet("p {x:1").unwrap().value().len(), 1);
    assert!(parse_stylesheet("p > a").unwrap().value().is_empty());
    assert!(parse_stylesheet("@unknown a").unwrap().value()[0]
        .block()
        .is_none());
    assert_eq!(
        parse_components(")").unwrap().value().as_slice(),
        &vec![Component::Token(Token::Close(')'))]
    );
}

#[test]
fn declaration_recovery_and_importance_are_at_top_level() {
    let parsed = parse_declarations(
        "bad value; 12: red; color: red ! ImPoRtAnT ;
         @unknown { a:b; } width: f(!important;); --x:;
         color: blue!important extra; padding: 0 !/**/important;",
    )
    .unwrap();
    assert_eq!(parsed.errors(), 2);
    let decls = parsed.value();
    assert_eq!(decls.len(), 5);
    assert!(decls[0].important());
    assert_eq!(decls[0].value(), &[Component::Token(ident("red"))]);
    assert!(!decls[1].important());
    assert_eq!(decls[2].name(), "--x");
    assert!(decls[2].value().is_empty());
    assert!(!decls[3].important());
    assert!(decls[4].important());
    let decls = parse_declarations("broken {a;b}; good:yes; @x y; last:ok").unwrap();
    assert_eq!(
        decls
            .value()
            .iter()
            .map(Declaration::name)
            .collect::<Vec<_>>(),
        ["good", "last"]
    );
}

#[test]
fn eof_at_rules_recover_with_a_diagnostic() {
    // Syntax §5.4.2: EOF returns the at-rule, but is a parse error.
    for input in ["@unknown", "@unknown a", "@unknown f(x)"] {
        let sheet = parse_stylesheet(input).unwrap();
        assert_eq!(sheet.value().len(), 1);
        assert_eq!(sheet.errors(), 1, "{input}");
        let inline = parse_declarations(input).unwrap();
        assert!(inline.value().is_empty());
        assert_eq!(inline.errors(), 1, "{input}");
    }
    for input in ["@unknown;", "@unknown a;", "@unknown{}"] {
        assert_eq!(parse_stylesheet(input).unwrap().errors(), 0);
        assert_eq!(parse_declarations(input).unwrap().errors(), 0);
    }
    assert_eq!(parse_stylesheet("@unknown f(").unwrap().errors(), 2);
    assert_eq!(parse_declarations("@unknown f(").unwrap().errors(), 2);
}

#[test]
fn unmatched_closers_are_preserved_parse_errors() {
    for input in [")", "]", "}"] {
        let parsed = parse_components(input).unwrap();
        assert_eq!(parsed.errors(), 1);
        assert_eq!(parsed.value().len(), 1);
    }
    assert_eq!(parse_components("([}])").unwrap().errors(), 1);
    assert_eq!(parse_components("([])").unwrap().errors(), 0);
    assert_eq!(parse_declarations("color:red)").unwrap().errors(), 1);
    assert_eq!(parse_components("'a\n").unwrap().errors(), 1);
    assert_eq!(parse_components("url(a b)").unwrap().errors(), 1);
}

#[test]
fn input_and_token_string_limits_include_exact_edge() {
    let exact = format!("/*{}*/", "x".repeat(MAX_INPUT_BYTES - 4));
    assert!(tokenize(&exact).is_ok());
    assert_eq!(
        tokenize(&(exact + " ")),
        Err(Error::Limit("MAX_INPUT_BYTES"))
    );
    assert!(tokenize(&"x".repeat(MAX_TOKEN_BYTES)).is_ok());
    for input in [
        "x".repeat(MAX_TOKEN_BYTES + 1),
        format!("'{}'", "x".repeat(MAX_TOKEN_BYTES + 1)),
        "1".repeat(MAX_TOKEN_BYTES + 1),
        format!("url({})", "x".repeat(MAX_TOKEN_BYTES + 1)),
    ] {
        assert_eq!(tokenize(&input), Err(Error::Limit("MAX_TOKEN_BYTES")));
    }
    assert!(tokenize(&"é".repeat(MAX_TOKEN_BYTES / 2)).is_ok());
    assert_eq!(
        tokenize(&"é".repeat(MAX_TOKEN_BYTES / 2 + 1)),
        Err(Error::Limit("MAX_TOKEN_BYTES"))
    );
}

#[test]
fn token_depth_rule_and_declaration_limits() {
    assert!(tokenize(&":".repeat(MAX_TOKENS)).is_ok());
    assert_eq!(
        tokenize(&":".repeat(MAX_TOKENS + 1)),
        Err(Error::Limit("MAX_TOKENS"))
    );
    assert!(parse_components(&"(".repeat(MAX_NESTING)).is_ok());
    assert_eq!(
        parse_components(&"(".repeat(MAX_NESTING + 1)),
        Err(Error::Limit("MAX_NESTING"))
    );
    assert_eq!(
        parse_stylesheet(&format!("@skip{{{}}}", "f(".repeat(MAX_NESTING))),
        Err(Error::Limit("MAX_NESTING"))
    );
    assert_eq!(
        parse_stylesheet(&"a{}".repeat(MAX_RULES))
            .unwrap()
            .value()
            .len(),
        MAX_RULES
    );
    assert_eq!(
        parse_stylesheet(&"a{}".repeat(MAX_RULES + 1)),
        Err(Error::Limit("MAX_RULES"))
    );
    assert_eq!(
        parse_declarations(&"x:;".repeat(MAX_DECLARATIONS))
            .unwrap()
            .value()
            .len(),
        MAX_DECLARATIONS
    );
    assert_eq!(
        parse_declarations(&"x:;".repeat(MAX_DECLARATIONS + 1)),
        Err(Error::Limit("MAX_DECLARATIONS"))
    );
}

#[test]
fn deterministic_hostile_strings_terminate_without_panics() {
    let alphabet = [
        'a', '0', '\0', '\r', '\n', '\\', '"', '\'', '(', ')', '[', ']', '{', '}', '/', '*', ' ',
        ';', ':', '!', '#', '@', '-', '+', '.', '%', 'é',
    ];
    let mut state = 0x12345678u32;
    for _ in 0..1000 {
        let mut input = String::new();
        for _ in 0..128 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            input.push(alphabet[state as usize % alphabet.len()]);
        }
        let _ = parse_stylesheet(&input);
        let _ = parse_declarations(&input);
    }
}

#[test]
fn inline_and_shared_text_keep_utf8_order_and_hash_across_the_boundary() {
    use std::hash::{Hash, Hasher};
    let memory = crate::memory().unwrap();
    for value in [
        "",
        "ABCDEFGHIJKLMN",
        "ABCDEFGHIJKLMNO",
        "ééééééé",
        "éééééééé",
        "語🌍aé",
        "é語🌍abcdef",
    ] {
        let text = super::text(&memory, value).unwrap();
        assert_eq!(text.as_str(), value);
        let copy = td_alloc::with_reservation_limit(0, || text.try_clone()).unwrap();
        assert_eq!(copy, text);
        let heap = td_alloc::BudgetString::copy(&memory, value).unwrap();
        let shared = super::Text(super::TextStorage::Shared(
            td_alloc::BudgetShared::new(&memory, heap).unwrap(),
        ));
        assert_eq!(text, shared);
        assert_eq!(text.cmp(&shared), std::cmp::Ordering::Equal);
        let mut a = std::collections::hash_map::DefaultHasher::new();
        let mut b = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut a);
        shared.hash(&mut b);
        assert_eq!(a.finish(), b.finish());
        let folded = super::lower_text(&memory, value).unwrap();
        assert_eq!(folded.as_str(), value.to_ascii_lowercase());
        let mut builder = super::TextBuilder::new(&memory);
        for ch in value.chars() {
            builder.try_push(ch).unwrap();
        }
        assert_eq!(builder.finish().unwrap(), text);
    }
    for len in [61, 62, 63, 64, 128] {
        let value = format!("{}é", "x".repeat(len - 2));
        let text = super::text(&memory, &value).unwrap();
        let mut builder = super::TextBuilder::new(&memory);
        for ch in value.chars() {
            builder.try_push(ch).unwrap();
        }
        assert_eq!(text, builder.finish().unwrap());
        assert_eq!(text.as_str(), value);
        let copy = td_alloc::with_reservation_limit(0, || text.try_clone()).unwrap();
        assert_eq!(text, copy);
    }
    assert_eq!(memory.used(), td_alloc::Budget::metadata_bytes());
}

#[test]
fn borrowed_ascii_and_normalized_character_storage_produce_identical_trees() {
    for source in ["", "p { color: red; font-family: LongNamedFamilyForThePage; --a: 1px }", "@media (min-width:1px) { [data-x*=abc i] { content: 'escaped\\1f30d'; padding:calc(1px + 2em) } }"] {
        let normalized = format!("\u{feff}{source}");
        assert_eq!(tokenize(source).unwrap(), tokenize(&normalized).unwrap());
        assert_eq!(parse_components(source).unwrap(), parse_components(&normalized).unwrap());
        assert_eq!(parse_stylesheet(source).unwrap(), parse_stylesheet(&normalized).unwrap());
    }
}
