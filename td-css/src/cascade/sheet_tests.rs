#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
//! Sheets given to a set with media conditions, and the ones it leaves out.
use super::*;
use crate::conditions::{ColorScheme, MediaType};
use crate::test_tree::{tree, Handle};
use crate::values::{Color, ComputedValue as V};
use td_alloc::Budget;

fn rgb(red: u8, blue: u8) -> V {
    V::Color(Color {
        red,
        green: 0,
        blue,
        alpha: 255,
    })
}
fn red() -> V {
    rgb(255, 0)
}
fn blue() -> V {
    rgb(0, 255)
}
fn sheet(origin: Origin, source: &str) -> Sheet<'_> {
    Sheet::new(origin, source)
}
fn compile_at(
    sheets: &[Sheet<'_>],
    environment: &Environment,
) -> Result<(StyleSet, Refused), Error> {
    StyleSet::from_sheets(sheets, environment, &crate::memory().unwrap())
}
fn refusals(refused: &Refused) -> Vec<(usize, Origin, Error)> {
    refused
        .iter()
        .map(|r| (r.index, r.origin, r.error))
        .collect()
}
/// The colour of the first `p.hot`, with no parent style.
fn color_of(set: &StyleSet) -> Option<V> {
    let nodes = tree();
    set.compute(
        &Handle {
            nodes: &nodes,
            index: 3,
        },
        None,
        16.0,
        "",
    )
    .unwrap()
    .get(Property::Color)
    .map(|value| value.try_clone().unwrap())
}
fn color(sheets: &[Sheet<'_>], environment: &Environment) -> Option<V> {
    let (set, refused) = compile_at(sheets, environment).unwrap();
    assert!(refused.is_empty());
    color_of(&set)
}

/// Sheets each meeting one per-sheet bound alone (DESIGN.md §5): the bound,
/// the text and its media lists.
fn per_sheet_refusals() -> Vec<(&'static str, String, Vec<String>)> {
    use crate::{calc, selectors, values, MAX_INPUT_BYTES, MAX_NESTING, MAX_TOKENS};
    let parens = |n: usize, inner: &str| format!("{}{inner}{}", "(".repeat(n), ")".repeat(n));
    let text = |limit, source: String| (limit, source, Vec::new());
    vec![
        text("MAX_INPUT_BYTES", " ".repeat(MAX_INPUT_BYTES + 1)),
        text(
            "MAX_TOKEN_BYTES",
            format!("p{{--x:{}}}", "a".repeat(crate::MAX_TOKEN_BYTES + 1)),
        ),
        text("MAX_TOKENS", "a ".repeat(MAX_TOKENS / 2 + 1)),
        text(
            "MAX_NESTING",
            format!("p{{color:{}}}", "(".repeat(MAX_NESTING + 1)),
        ),
        text("MAX_RULES", "p{}".repeat(crate::MAX_RULES + 1)),
        text(
            "MAX_GROUP_DEPTH",
            format!(
                "{}p{{}}{}",
                "@media all{".repeat(MAX_GROUP_DEPTH + 1),
                "}".repeat(MAX_GROUP_DEPTH + 1)
            ),
        ),
        text(
            "MAX_DECLARATIONS",
            format!("p{{{}}}", "a:;".repeat(MAX_DECLARATIONS + 1)),
        ),
        text(
            "MAX_SELECTORS",
            format!("{}p{{}}", "p,".repeat(selectors::MAX_SELECTORS)),
        ),
        text(
            "MAX_SELECTOR_PARTS",
            format!("{}{{}}", ".a".repeat(selectors::MAX_SELECTOR_PARTS + 1)),
        ),
        text(
            "MAX_SELECTOR_DEPTH",
            format!(
                "{}p{}{{}}",
                ":is(".repeat(selectors::MAX_SELECTOR_DEPTH + 1),
                ")".repeat(selectors::MAX_SELECTOR_DEPTH + 1)
            ),
        ),
        text(
            "MAX_COMPOUNDS",
            format!("{}{{}}", "p ".repeat(selectors::MAX_COMPOUNDS + 1)),
        ),
        text(
            "MAX_NTH_COEFFICIENT",
            format!("p:nth-child({}n+1){{}}", selectors::MAX_NTH_COEFFICIENT + 1),
        ),
        text(
            "MAX_CONDITION_PARTS",
            format!(
                "@media ({}){{}}",
                "x ".repeat(conditions::MAX_CONDITION_PARTS)
            ),
        ),
        text(
            "MAX_CONDITION_DEPTH",
            format!(
                "@media {}{{}}",
                parens(conditions::MAX_CONDITION_DEPTH + 1, "x")
            ),
        ),
        text(
            "MAX_MEDIA_QUERIES",
            format!(
                "@media {}all{{}}",
                "all,".repeat(conditions::MAX_MEDIA_QUERIES)
            ),
        ),
        text(
            "MAX_CALC_PARTS",
            format!(
                "p{{margin:calc({}1px)}}",
                "1px + ".repeat(calc::MAX_CALC_PARTS)
            ),
        ),
        text(
            "MAX_CALC_DEPTH",
            format!(
                "p{{margin:{}1px{}}}",
                "calc(".repeat(calc::MAX_CALC_DEPTH + 1),
                ")".repeat(calc::MAX_CALC_DEPTH + 1)
            ),
        ),
        text(
            "MAX_FAMILIES",
            format!(
                "p{{font-family:{}serif}}",
                "a,".repeat(values::MAX_FAMILIES)
            ),
        ),
        // Media lists count against their sheet's bounds.
        (
            "MAX_INPUT_BYTES",
            " ".repeat(MAX_INPUT_BYTES - 4),
            vec!["print".into()],
        ),
        (
            "MAX_INPUT_BYTES",
            "p{}".into(),
            vec![
                " ".repeat(MAX_INPUT_BYTES / 2),
                " ".repeat(MAX_INPUT_BYTES / 2),
            ],
        ),
        (
            "MAX_TOKENS",
            "p{}".into(),
            vec!["a ".repeat(MAX_TOKENS / 2 + 1)],
        ),
        (
            "MAX_NESTING",
            "p{}".into(),
            vec!["".into(), "(".repeat(MAX_NESTING + 1)],
        ),
        (
            "MAX_CONDITION_PARTS",
            "p{}".into(),
            vec!["x ".repeat(conditions::MAX_CONDITION_PARTS)],
        ),
        (
            "MAX_CONDITION_DEPTH",
            "p{}".into(),
            vec![parens(conditions::MAX_CONDITION_DEPTH + 1, "x")],
        ),
        // A later list's bound holds though an earlier one fails to match.
        (
            "MAX_MEDIA_QUERIES",
            "p{}".into(),
            vec![
                "print".into(),
                format!("{}all", "all,".repeat(conditions::MAX_MEDIA_QUERIES)),
            ],
        ),
    ]
}
fn lists(media: &[String]) -> Vec<&str> {
    media.iter().map(String::as_str).collect()
}

#[test]
fn a_per_sheet_refusal_leaves_its_sheet_out_and_names_it() {
    let environment = Environment::default();
    for (limit, source, media) in per_sheet_refusals() {
        let media = lists(&media);
        let bad = sheet(Origin::Author, &source).with_media(&media);
        let (alone, refused) = compile_at(&[bad], &environment).unwrap();
        assert_eq!(
            refusals(&refused),
            [(0, Origin::Author, Error::Limit(limit))],
            "{limit} alone"
        );
        assert_eq!(alone.rules.len(), 0, "{limit} alone adds nothing");
        for (index, origin) in [
            (1, Origin::User),
            (2, Origin::Author),
            (3, Origin::UserAgent),
        ] {
            let mut sheets = vec![
                sheet(Origin::UserAgent, HTML_UA_CSS),
                sheet(Origin::User, "p{color:red}").with_media(&["screen"]),
                sheet(Origin::Author, ".hot{color:blue}"),
            ];
            let good = sheets.clone();
            sheets.insert(index, Sheet { origin, ..bad });
            let (set, refused) = compile_at(&sheets, &environment).unwrap();
            assert_eq!(
                refusals(&refused),
                [(index, origin, Error::Limit(limit))],
                "{limit} at {index}"
            );
            let (expected, _) = compile_at(&good, &environment).unwrap();
            assert_eq!(format!("{set:?}"), format!("{expected:?}"), "{limit}");
            if media.is_empty() {
                // The tuple constructors keep stopping at the first error.
                let tuples: Vec<_> = sheets.iter().map(|s| (s.origin, s.source)).collect();
                assert_eq!(
                    StyleSet::new(&tuples).map(drop),
                    Err(Error::Limit(limit)),
                    "{limit} by tuple"
                );
            }
        }
    }
}

#[test]
fn a_set_wide_refusal_is_the_error() {
    let environment = Environment::default();
    let empty = sheet(Origin::Author, "");
    assert!(compile_at(&vec![empty; MAX_SHEETS], &environment).is_ok());
    assert_eq!(
        compile_at(&vec![empty; MAX_SHEETS + 1], &environment).map(drop),
        Err(Error::Limit("MAX_SHEETS"))
    );
    // Text and media lists share the set's byte bound.
    let one = " ".repeat(crate::MAX_INPUT_BYTES - 1);
    let mut sheets = vec![sheet(Origin::Author, &one).with_media(&[" "]); 4];
    assert!(compile_at(&sheets, &environment).is_ok());
    sheets.push(sheet(Origin::Author, "").with_media(&[" "]));
    assert_eq!(
        compile_at(&sheets, &environment).map(drop),
        Err(Error::Limit("MAX_STYLE_BYTES"))
    );
    // A sheet past its own bound is refused alone, whatever the others hold.
    let full = " ".repeat(crate::MAX_INPUT_BYTES);
    let over = " ".repeat(crate::MAX_INPUT_BYTES + 1);
    let mut sheets = vec![sheet(Origin::Author, &full); 4];
    sheets.insert(2, sheet(Origin::Author, &over));
    let (_, refused) = compile_at(&sheets, &environment).unwrap();
    assert_eq!(
        refusals(&refused),
        [(2, Origin::Author, Error::Limit("MAX_INPUT_BYTES"))]
    );
    let rules = "p{}".repeat(crate::MAX_RULES);
    let mut sheets = vec![sheet(Origin::Author, &rules); 2];
    assert!(compile_at(&sheets, &environment).is_ok());
    sheets.push(sheet(Origin::Author, "p{}"));
    assert_eq!(
        compile_at(&sheets, &environment).map(drop),
        Err(Error::Limit("MAX_COMPILED_RULES"))
    );
    // Rules a non-matching media list leaves inactive still count.
    let inactive = "@media print{p{}}".repeat(MAX_GROUP_RULES / MAX_SHEETS / 2);
    let mut sheets = vec![sheet(Origin::Author, &inactive).with_media(&["print"]); MAX_SHEETS];
    assert!(compile_at(&sheets, &environment).is_ok());
    let more = format!("{inactive}p{{}}");
    sheets[MAX_SHEETS - 1].source = &more;
    assert_eq!(
        compile_at(&sheets, &environment).map(drop),
        Err(Error::Limit("MAX_GROUP_RULES"))
    );
    let declarations = format!("p{{{}}}", "margin:0;".repeat(MAX_DECLARATIONS / 4));
    assert!(compile_at(&[sheet(Origin::Author, &declarations)], &environment).is_ok());
    assert_eq!(
        compile_at(
            &[
                sheet(Origin::Author, &declarations),
                sheet(Origin::Author, "p{color:red}")
            ],
            &environment
        )
        .map(drop),
        Err(Error::Limit("MAX_DECLARATIONS"))
    );
    let negative = Environment {
        width: -1.0,
        ..environment
    };
    assert_eq!(
        compile_at(&[sheet(Origin::Author, "p{}")], &negative).map(drop),
        Err(Error::InvalidContext)
    );
    // The compile ceiling is the caller's budget, which every sheet spends.
    let tight = Budget::new(256 * 1024).unwrap();
    let wide = ".a{color:red}".repeat(4096);
    assert!(matches!(
        StyleSet::from_sheets(&[sheet(Origin::Author, &wide)], &environment, &tight),
        Err(Error::Memory(_))
    ));
}

/// Compile both lists under fresh budgets and require one set: the same
/// rules and index, and the same charges but for spare array capacity.
fn assert_same_set(with: &[Sheet<'_>], without: &[Sheet<'_>], expected: &[(usize, Error)]) {
    let environment = Environment::default();
    let wide = Budget::new(256 * 1024 * 1024).unwrap();
    let alone = Budget::new(256 * 1024 * 1024).unwrap();
    let (set, refused) = StyleSet::from_sheets(with, &environment, &wide).unwrap();
    let (good, none) = StyleSet::from_sheets(without, &environment, &alone).unwrap();
    assert!(none.is_empty());
    let named: Vec<_> = refused.iter().map(|r| (r.index, r.error)).collect();
    assert_eq!(named, expected);
    assert_eq!(format!("{set:?}"), format!("{good:?}"));
    assert_eq!(
        wide.used() - set.spare_bytes(),
        alone.used() - good.spare_bytes(),
        "a refused sheet leaves no charge behind"
    );
    assert_eq!(color_of(&set), color_of(&good));
    drop((set, good));
    assert_eq!(wide.used(), Budget::metadata_bytes());
    assert_eq!(alone.used(), Budget::metadata_bytes());
}

#[test]
fn one_compilation_leaves_out_every_refused_sheet() {
    let refusals = per_sheet_refusals();
    let media: Vec<Vec<&str>> = refusals.iter().map(|(_, _, m)| lists(m)).collect();
    let good: Vec<String> = (0..refusals.len())
        .map(|n| {
            format!("#g{n}{{color:red}} .g{n} p{{margin:{n}px}} *{{--n:{n}}} .hot{{color:blue}}")
        })
        .collect();
    let mut with = vec![sheet(Origin::UserAgent, HTML_UA_CSS)];
    let mut without = with.clone();
    let mut expected = Vec::new();
    for (n, (limit, source, _)) in refusals.iter().enumerate() {
        expected.push((with.len(), Error::Limit(limit)));
        with.push(sheet(Origin::Author, source).with_media(&media[n]));
        with.push(sheet(Origin::Author, &good[n]));
        without.push(sheet(Origin::Author, &good[n]));
    }
    assert_same_set(&with, &without, &expected);
}

#[test]
fn a_sheet_refused_part_way_is_rolled_back_whole() {
    // Rules, index entries of every kind, custom and ordinary declarations
    // and visited group rules precede each refusal.
    let prefix = "#a{color:red} .b{color:red} p{margin:1px} *{--x:1} :is(.c,d){} \
        @media print{p{}} @media all{.e{color:red}} ";
    let tails = [
        format!(
            "{}p{{}}{}",
            "@media all{".repeat(MAX_GROUP_DEPTH + 1),
            "}".repeat(MAX_GROUP_DEPTH + 1)
        ),
        format!("{}p{{}}", "p,".repeat(crate::selectors::MAX_SELECTORS)),
        format!("p{{margin:{}1px{}}}", "calc(".repeat(17), ")".repeat(17)),
    ];
    let refused: Vec<String> = tails.iter().map(|t| format!("{prefix}{t}")).collect();
    let after = ".hot{color:blue} #z{}";
    let with = [
        sheet(Origin::Author, "p{color:red}"),
        sheet(Origin::Author, &refused[0]),
        sheet(Origin::User, &refused[1]),
        sheet(Origin::Author, &refused[2]),
        sheet(Origin::Author, after),
    ];
    let without = [with[0], with[4]];
    let expected = [
        (1, Error::Limit("MAX_GROUP_DEPTH")),
        (2, Error::Limit("MAX_SELECTORS")),
        (3, Error::Limit("MAX_CALC_DEPTH")),
    ];
    assert_same_set(&with, &without, &expected);

    // The set-wide counters roll back too: what the good sheets fill to
    // the bound still fits after a refused sheet spent some of it.
    let environment = Environment::default();
    let spent = format!(
        "p{{{}}} {}{}",
        "margin:0;".repeat(1000),
        "@media print{p{}}".repeat(1000),
        tails[0]
    );
    let first = [(0, Origin::Author, Error::Limit("MAX_GROUP_DEPTH"))];
    let declarations = format!("p{{{}}}", "margin:0;".repeat(MAX_DECLARATIONS / 4));
    let sheets = [
        sheet(Origin::Author, &spent),
        sheet(Origin::Author, &declarations),
    ];
    assert_eq!(
        refusals(&compile_at(&sheets, &environment).unwrap().1),
        first
    );
    // 62 sheets of 512 visited rules and one of 1,024 fill MAX_GROUP_RULES.
    let walked = "@media print{p{}}".repeat(256);
    let last = walked.repeat(2);
    let mut sheets = vec![sheet(Origin::Author, &spent)];
    sheets.extend(vec![sheet(Origin::Author, &walked); MAX_SHEETS - 2]);
    sheets.push(sheet(Origin::Author, &last));
    assert_eq!(
        refusals(&compile_at(&sheets, &environment).unwrap().1),
        first
    );
    sheets[0] = sheet(Origin::Author, "p{}");
    assert_eq!(
        compile_at(&sheets, &environment).map(drop),
        Err(Error::Limit("MAX_GROUP_RULES")),
        "the good sheets fill the bound exactly"
    );
}

#[test]
fn the_reviewed_page_shape_compiles_once() {
    // Good sheets, then sheets that each nest too deep, as review found
    // (14 of 131 KiB there; scaled down here, measured full size apart).
    let good = ".a{color:red} #b{margin:1px} p .c{padding:2px} ".repeat(100);
    let deep = format!(
        "{}{}p{{}}{}",
        ".x{color:red}".repeat(100),
        "@media all{".repeat(MAX_GROUP_DEPTH + 1),
        "}".repeat(MAX_GROUP_DEPTH + 1)
    );
    let mut with = vec![sheet(Origin::UserAgent, HTML_UA_CSS)];
    with.extend(vec![sheet(Origin::Author, &good); 14]);
    let without = with.clone();
    let mut expected = Vec::new();
    for _ in 0..MAX_SHEETS - with.len() {
        expected.push((with.len(), Error::Limit("MAX_GROUP_DEPTH")));
        with.push(sheet(Origin::Author, &deep));
    }
    assert_same_set(&with, &without, &expected);
}

#[test]
fn a_width_change_can_name_a_sheet_an_inactive_group_hid() {
    let wide_only = format!(
        "@media (min-width: 600px) {{ {}p{{}} }}",
        "p,".repeat(crate::selectors::MAX_SELECTORS)
    );
    let input = [sheet(Origin::Author, &wide_only)];
    let narrow = Environment {
        width: 599.0,
        ..Environment::default()
    };
    assert!(compile_at(&input, &narrow).unwrap().1.is_empty());
    let (_, refused) = compile_at(&input, &Environment::default()).unwrap();
    assert_eq!(
        refusals(&refused),
        [(0, Origin::Author, Error::Limit("MAX_SELECTORS"))]
    );
}

#[test]
fn a_sheet_media_list_matches_as_an_at_media_block_would() {
    let screen = Environment::default();
    let narrow = Environment {
        width: 599.0,
        ..screen
    };
    let edge = Environment {
        width: 600.0,
        ..screen
    };
    let print = Environment {
        media_type: MediaType::Print,
        ..screen
    };
    let dark = Environment {
        color_scheme: ColorScheme::Dark,
        ..narrow
    };
    // HTML's "matches the environment": an empty or whitespace-only list
    // matches; Media Queries 4 §3.2 makes an invalid query `not all`.
    let cases: &[(&str, &[(&Environment, bool)])] = &[
        ("", &[(&screen, true), (&print, true)]),
        (" \t\n", &[(&screen, true), (&print, true)]),
        ("all", &[(&screen, true), (&print, true)]),
        ("screen", &[(&screen, true), (&print, false)]),
        ("SCREEN", &[(&screen, true)]),
        ("only screen", &[(&screen, true), (&print, false)]),
        (
            "print",
            &[(&screen, false), (&narrow, false), (&print, true)],
        ),
        ("not print", &[(&screen, true), (&print, false)]),
        ("not screen", &[(&screen, false), (&print, true)]),
        ("tv", &[(&screen, false)]),
        ("not tv", &[(&screen, true)]),
        (
            "(min-width: 600px)",
            &[(&narrow, false), (&edge, true), (&screen, true)],
        ),
        ("(max-width: 599.5px)", &[(&narrow, true), (&edge, false)]),
        ("(width < 600px)", &[(&narrow, true), (&edge, false)]),
        (
            "(400px <= width < 600px)",
            &[(&narrow, true), (&edge, false)],
        ),
        (
            "screen and (min-width: 37.5em)",
            &[(&narrow, false), (&edge, true)],
        ),
        (
            "print, (min-width: 600px)",
            &[(&narrow, false), (&edge, true), (&print, true)],
        ),
        (
            "(min-width: 600px), (prefers-color-scheme: dark)",
            &[(&narrow, false), (&dark, true)],
        ),
        (
            "not all and (min-width: 600px)",
            &[(&narrow, true), (&edge, false)],
        ),
        // Invalid members become not all; valid ones still answer.
        ("screen and", &[(&screen, false)]),
        ("screen,, print", &[(&screen, true), (&print, true)]),
        ("!!, print", &[(&screen, false), (&print, true)]),
        (
            "(min-width: 600px) and, screen",
            &[(&screen, true), (&print, false)],
        ),
        (
            "screen and (min-width: 600px) or print",
            &[(&screen, false), (&print, false)],
        ),
        ("screen; print", &[(&screen, false), (&print, false)]),
        ("{screen}", &[(&screen, false)]),
        ("(min-width: 600px", &[(&screen, true), (&narrow, false)]),
        // Unknown features stay unknown, so false, even negated.
        ("(hover: hover)", &[(&screen, false)]),
        ("not (hover: hover)", &[(&screen, false)]),
        ("(min-width: 10vw)", &[(&screen, false)]),
        // An attribute is text, not a decoded sheet: U+FEFF is no BOM.
        ("\u{feff}print", &[(&screen, false), (&print, false)]),
        ("\u{feff}", &[(&screen, false), (&print, false)]),
        ("\u{feff}screen", &[(&screen, false)]),
    ];
    let body = ".hot{color:blue}";
    for (media, expected) in cases {
        for &(environment, matches) in *expected {
            let list = [*media];
            let wanted = if matches { blue() } else { red() };
            assert_eq!(
                color(
                    &[
                        sheet(Origin::Author, "p{color:red}"),
                        sheet(Origin::Author, body).with_media(&list),
                    ],
                    environment
                ),
                Some(wanted.try_clone().unwrap()),
                "{media:?} {environment:?}"
            );
            // A prelude cannot hold `{` or `;`, nor end inside a block.
            if !media.contains(['{', ';', '\u{feff}'])
                && media.matches('(').count() == media.matches(')').count()
            {
                let wrapped = format!("p{{color:red}} @media {media} {{{body}}}");
                assert_eq!(
                    color(&[sheet(Origin::Author, &wrapped)], environment),
                    Some(wanted),
                    "@media {media:?} {environment:?}"
                );
            }
        }
    }
}

#[test]
fn every_media_list_of_a_sheet_must_match() {
    let screen = Environment::default();
    let narrow = Environment {
        width: 599.0,
        ..screen
    };
    let print = Environment {
        media_type: MediaType::Print,
        ..screen
    };
    // A `<link media>` list, then the lists of the imports on the way.
    type Case<'a> = (&'a [&'a str], &'a [(&'a Environment, bool)]);
    let cases: &[Case<'_>] = &[
        (&[], &[(&screen, true), (&print, true)]),
        (&["", " "], &[(&screen, true), (&print, true)]),
        (&["print", "screen"], &[(&screen, false), (&print, false)]),
        (
            &["screen", "(min-width: 600px)"],
            &[(&screen, true), (&narrow, false), (&print, false)],
        ),
        (
            &["all", "print, (min-width: 600px)"],
            &[(&screen, true), (&narrow, false), (&print, true)],
        ),
        (
            &["not print", "screen", "(max-width: 700px)"],
            &[(&screen, false), (&narrow, true), (&print, false)],
        ),
        (&["screen", "garbage!"], &[(&screen, false)]),
        (&["screen", "\u{feff}"], &[(&screen, false)]),
    ];
    for (media, expected) in cases {
        for &(environment, matches) in *expected {
            let nested = media
                .iter()
                .rev()
                .fold(".hot{color:blue}".to_owned(), |inner, list| {
                    format!("@media {list} {{{inner}}}")
                });
            let wanted = if matches { blue() } else { red() };
            assert_eq!(
                color(
                    &[
                        sheet(Origin::Author, "p{color:red}"),
                        sheet(Origin::Author, ".hot{color:blue}").with_media(media),
                    ],
                    environment
                ),
                Some(wanted.try_clone().unwrap()),
                "{media:?} {environment:?}"
            );
            if !media.iter().any(|m| m.contains('\u{feff}')) {
                assert_eq!(
                    color(
                        &[sheet(Origin::Author, &format!("p{{color:red}} {nested}"))],
                        environment
                    ),
                    Some(wanted),
                    "nested {media:?} {environment:?}"
                );
            }
        }
    }
}

#[test]
fn media_conditions_keep_cascade_order_and_cannot_be_escaped() {
    let environment = Environment::default();
    let color = |sheets: &[Sheet<'_>]| color(sheets, &environment);
    // A non-matching sheet contributes nothing, not even !important.
    assert_eq!(
        color(&[
            sheet(Origin::Author, "p{color:red}"),
            sheet(Origin::Author, "p{color:blue!important}").with_media(&["print"]),
            sheet(Origin::UserAgent, "p{color:blue!important}").with_media(&["print"]),
        ]),
        Some(red())
    );
    // Matching sheets keep source order and origin precedence.
    assert_eq!(
        color(&[
            sheet(Origin::Author, "p{color:red}").with_media(&["screen"]),
            sheet(Origin::Author, "p{color:blue}").with_media(&["all"]),
        ]),
        Some(blue())
    );
    assert_eq!(
        color(&[
            sheet(Origin::Author, "p{color:blue}").with_media(&["screen"]),
            sheet(Origin::Author, "p{color:red}"),
        ]),
        Some(red())
    );
    assert_eq!(
        color(&[
            sheet(Origin::UserAgent, "p{color:blue!important}").with_media(&["screen"]),
            sheet(Origin::Author, "p{color:red!important}"),
        ]),
        Some(blue())
    );
    // A stray `}` in the sheet cannot close its condition, as it closes an
    // @media block wrapped around the same text.
    let stray = "} p{color:blue}";
    assert_eq!(
        color(&[
            sheet(Origin::Author, "p{color:red}"),
            sheet(Origin::Author, stray).with_media(&["print"]),
        ]),
        Some(red())
    );
    let wrapped = format!("p{{color:red}} @media print {{{stray}}}");
    assert_eq!(color(&[sheet(Origin::Author, &wrapped)]), Some(blue()));
    // Nor can a media list's own text add rules.
    assert_eq!(
        color(&[
            sheet(Origin::Author, "p{color:red}"),
            sheet(Origin::Author, "").with_media(&["print { } p { color: blue } @media all"]),
        ]),
        Some(red())
    );
    // A sheet's conditions are no group level of their own.
    let deepest = format!(
        "{}p{{color:blue}}{}",
        "@media all{".repeat(MAX_GROUP_DEPTH),
        "}".repeat(MAX_GROUP_DEPTH)
    );
    assert_eq!(
        color(&[sheet(Origin::Author, &deepest).with_media(&["screen", "all"])]),
        Some(blue())
    );
    // An inactive sheet is still parsed and admitted whole.
    let many = "p{}".repeat(crate::MAX_RULES + 1);
    let (_, refused) = compile_at(
        &[sheet(Origin::Author, &many).with_media(&["print"])],
        &environment,
    )
    .unwrap();
    assert_eq!(
        refusals(&refused),
        [(0, Origin::Author, Error::Limit("MAX_RULES"))]
    );
}

#[test]
fn media_conditions_release_storage_on_every_refusal() {
    use td_alloc::{with_reservation_limit, BudgetError};
    let memory = Budget::new(16 * 1024 * 1024).unwrap();
    let deep = format!(
        ".x{{color:red}}{}p{{}}{}",
        "@media all{".repeat(MAX_GROUP_DEPTH + 1),
        "}".repeat(MAX_GROUP_DEPTH + 1)
    );
    let sheets = [
        sheet(Origin::UserAgent, "p{color:red}").with_media(&["screen and (min-width: 1px)"]),
        sheet(Origin::Author, &deep),
        sheet(Origin::Author, ".hot{color:blue}").with_media(&["all", "print, (max-width: 1px)"]),
    ];
    let mut complete = false;
    for allowed in 0..1024 {
        let result = with_reservation_limit(allowed, || {
            StyleSet::from_sheets(&sheets, &Environment::default(), &memory)
        });
        match result {
            Ok((set, refused)) => {
                assert_eq!(refused.len(), 1);
                assert_eq!(color_of(&set), Some(red()));
                complete = true;
            }
            Err(error) => assert_eq!(error, Error::Memory(BudgetError::Allocation)),
        }
        assert_eq!(memory.used(), Budget::metadata_bytes());
        if complete {
            break;
        }
    }
    assert!(complete);
}

#[test]
fn a_sheet_tripping_a_total_is_refused_alone_or_the_set_is() {
    let environment = Environment::default();
    let depth = format!(
        "{}p{{}}{}",
        "@media all{".repeat(MAX_GROUP_DEPTH + 1),
        "}".repeat(MAX_GROUP_DEPTH + 1)
    );
    let after = ".hot{color:blue}";
    // Each case: good sheets filling a total but for a little, then what
    // overfills it on its way to a bound of its own, and the same text
    // without that bound.
    let declarations = format!("p{{{}}}", "margin:0;".repeat(MAX_DECLARATIONS / 4 - 3));
    let five_margins = format!("p{{{}}}", "margin:0;".repeat(5));
    let rules = "p{}".repeat(crate::MAX_RULES);
    let fewer = "p{}".repeat(crate::MAX_RULES - 2);
    let five_rules = "p{}".repeat(5);
    let walked = "@media print{p{}}".repeat(256);
    let rest = "@media print{p{}}".repeat(766);
    let cases: [(&str, Vec<&str>, &str); 3] = [
        ("MAX_DECLARATIONS", vec![&declarations], &five_margins),
        ("MAX_COMPILED_RULES", vec![&rules, &fewer], &five_rules),
        (
            "MAX_GROUP_RULES",
            [vec![walked.as_str(); MAX_SHEETS - 3], vec![&rest]].concat(),
            &five_rules,
        ),
    ];
    for (total, good, tail) in cases {
        let refuser = format!("{tail}{depth}");
        let mut with: Vec<_> = good.iter().map(|s| sheet(Origin::Author, s)).collect();
        let index = with.len();
        with.push(sheet(Origin::Author, &refuser));
        with.push(sheet(Origin::Author, after));
        let without: Vec<_> = with
            .iter()
            .enumerate()
            .filter(|&(n, _)| n != index)
            .map(|(_, s)| *s)
            .collect();
        // Alone the refuser is refused for its depth, not for the total.
        let (_, alone) = compile_at(&[with[index]], &environment).unwrap();
        assert_eq!(
            refusals(&alone),
            [(0, Origin::Author, Error::Limit("MAX_GROUP_DEPTH"))]
        );
        assert_same_set(&with, &without, &[(index, Error::Limit("MAX_GROUP_DEPTH"))]);
        // A good sheet that overfills the total is the set's refusal.
        with[index] = sheet(Origin::Author, tail);
        assert_eq!(
            compile_at(&with, &environment).map(drop),
            Err(Error::Limit(total)),
            "{total}"
        );
    }
}

#[test]
fn every_media_list_spends_a_byte_of_its_sheet() {
    let empty = vec![""; crate::MAX_INPUT_BYTES - 2];
    let (_, refused) = compile_at(
        &[sheet(Origin::Author, "p{}").with_media(&empty)],
        &Environment::default(),
    )
    .unwrap();
    assert_eq!(
        refusals(&refused),
        [(0, Origin::Author, Error::Limit("MAX_INPUT_BYTES"))]
    );
    assert_eq!(
        format!("{refused:?}"),
        r#"[Refusal { index: 0, origin: Author, error: Limit("MAX_INPUT_BYTES") }]"#
    );
}
