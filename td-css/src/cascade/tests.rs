#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::test_tree::{tree, Handle};
use crate::values::{Color, ComputedValue as V, Decoration, Family, Length};

fn compute(css: &str, inline: &str) -> ComputedStyle {
    let nodes = tree();
    StyleSet::new(&[(Origin::Author, css)])
        .unwrap()
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3,
            },
            None,
            16.0,
            inline,
        )
        .unwrap()
}

fn red() -> V {
    V::Color(Color {
        red: 255,
        green: 0,
        blue: 0,
        alpha: 255,
    })
}
fn blue() -> V {
    V::Color(Color {
        red: 0,
        green: 0,
        blue: 255,
        alpha: 255,
    })
}

#[test]
fn every_origin_and_importance_pair_has_the_specified_precedence() {
    let ranks = [
        (Origin::UserAgent, false),
        (Origin::User, false),
        (Origin::Author, false),
        (Origin::Author, true),
        (Origin::User, true),
        (Origin::UserAgent, true),
    ];
    let nodes = tree();
    let element = Handle {
        nodes: &nodes,
        index: 3,
    };
    for (low_index, &(low_origin, low_important)) in ranks.iter().enumerate() {
        for &(high_origin, high_important) in ranks.iter().skip(low_index + 1) {
            let low = format!(
                "#container > p.first{{color:red{}}}",
                if low_important { "!important" } else { "" }
            );
            let high = format!(
                "p{{color:blue{}}}",
                if high_important { "!important" } else { "" }
            );
            for sheets in [
                vec![(low_origin, low.as_str()), (high_origin, high.as_str())],
                vec![(high_origin, high.as_str()), (low_origin, low.as_str())],
            ] {
                let result = StyleSet::new(&sheets)
                    .unwrap()
                    .compute(&element, None, 16.0, "")
                    .unwrap();
                assert_eq!(result.get(Property::Color), Some(&blue()), "{sheets:?}");
            }
        }
    }
}

#[test]
fn inline_specificity_and_source_order_are_independent_tiers() {
    assert_eq!(
        compute("#container p.first {color:blue}", "color:red").get(Property::Color),
        Some(&red())
    );
    assert_eq!(
        compute("p {color:blue!important}", "color:red").get(Property::Color),
        Some(&blue())
    );
    assert_eq!(
        compute("#container p {color:blue!important}", "color:red!important").get(Property::Color),
        Some(&red())
    );
    let nodes = tree();
    let set = StyleSet::new(&[(Origin::User, "p{color:blue!important}")]).unwrap();
    assert_eq!(
        set.compute(
            &Handle {
                nodes: &nodes,
                index: 3
            },
            None,
            16.0,
            "color:red!important"
        )
        .unwrap()
        .get(Property::Color),
        Some(&blue())
    );
    assert_eq!(
        compute(".first{color:red} p{color:blue}", "").get(Property::Color),
        Some(&red())
    );
    assert_eq!(
        compute("p{color:red;color:blue}", "").get(Property::Color),
        Some(&blue())
    );
    assert_eq!(
        compute("p{color:red}p{color:blue}", "").get(Property::Color),
        Some(&blue())
    );
    assert_eq!(
        compute(":where(#container p){color:blue}p{color:red}", "").get(Property::Color),
        Some(&red())
    );
    assert_eq!(
        compute("p:is(.first,#absent){color:red}.first{color:blue}", "").get(Property::Color),
        Some(&red())
    );
    let sheets = StyleSet::new(&[
        (Origin::Author, "p{color:red}"),
        (Origin::Author, "p{color:blue}"),
    ])
    .unwrap();
    assert_eq!(
        sheets
            .compute(
                &Handle {
                    nodes: &nodes,
                    index: 3
                },
                None,
                16.0,
                ""
            )
            .unwrap()
            .get(Property::Color),
        Some(&blue())
    );
}

#[test]
fn inherited_values_are_computed_once_and_font_dependencies_are_ordered() {
    let nodes = tree();
    let set = StyleSet::default();
    let parent = set.compute(&Handle { nodes:&nodes,index:2 }, None,16.0,
        "font-size:20px; margin:2em; color:blue; background-color:currentcolor; font-weight:500; width:50%").unwrap();
    let child = set.compute(&Handle { nodes:&nodes,index:3 },Some(&parent),20.0,
        "font-size:2em; margin:inherit; padding:1em 2rem; color:red; background-color:inherit; font-weight:bolder").unwrap();
    assert_eq!(child.font_size(), 40.0);
    assert_eq!(
        child.get(Property::MarginLeft),
        Some(&V::Length(Length::Px(40.0)))
    );
    assert_eq!(
        child.get(Property::PaddingTop),
        Some(&V::Length(Length::Px(40.0)))
    );
    assert_eq!(
        child.get(Property::PaddingRight),
        Some(&V::Length(Length::Px(40.0)))
    );
    assert_eq!(child.get(Property::BackgroundColor), Some(&blue()));
    assert_eq!(child.get(Property::FontWeight), Some(&V::Weight(700)));
    assert_eq!(child.get(Property::Width), Some(&V::Length(Length::Auto)));
    let inherited = set
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3,
            },
            Some(&parent),
            20.0,
            "width:inherit",
        )
        .unwrap();
    assert_eq!(inherited.font_size(), 20.0);
    assert_eq!(
        inherited.get(Property::Width),
        Some(&V::Length(Length::Percent(50.0)))
    );
    assert_eq!(
        inherited.get(Property::MarginLeft),
        Some(&V::Length(Length::Px(0.0)))
    );
    let current = set
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3,
            },
            Some(&parent),
            20.0,
            "color:currentcolor;background-color:currentcolor",
        )
        .unwrap();
    assert_eq!(current.get(Property::Color), Some(&blue()));
    assert_eq!(current.get(Property::BackgroundColor), Some(&blue()));
}

#[test]
fn root_rem_uses_initial_size_only_for_its_font_size() {
    // CSS Values 3 §5.1.1: rem on the root's font-size breaks the cycle
    // with the initial size; other properties use its computed size.
    let nodes = tree();
    let root = Handle {
        nodes: &nodes,
        index: 0,
    };
    let set = StyleSet::default();
    for (source, size) in [
        ("font-size:2rem;margin:1rem", 32.0),
        ("font-size:20px;margin:1rem", 20.0),
        ("font-size:0;margin:1rem", 0.0),
    ] {
        let style = set.compute(&root, None, 16.0, source).unwrap();
        assert_eq!(style.font_size(), size);
        assert_eq!(
            style.get(Property::MarginTop),
            Some(&V::Length(Length::Px(size))),
            "{source}"
        );
    }
}

#[test]
fn every_property_has_initial_inherit_and_unset() {
    let nodes = tree();
    let set = StyleSet::default();
    let parent = compute("", "display:block;color:red;background-color:blue;font-family:monospace;font-size:30px;font-weight:700;font-style:italic;text-decoration:underline;white-space:pre;margin:5px;padding:6px;border-width:7px;width:25%;height:2em;list-style-type:square;text-align:right");
    for property in Property::ALL {
        assert!(parent.get(*property).is_some(), "{property:?}");
        for keyword in ["initial", "inherit", "unset"] {
            let style = set
                .compute(
                    &Handle {
                        nodes: &nodes,
                        index: 3,
                    },
                    Some(&parent),
                    30.0,
                    &format!("{}:{keyword}", property.name()),
                )
                .unwrap();
            let expected = if keyword == "inherit" || (keyword == "unset" && property.inherited()) {
                parent.get(*property).unwrap().clone()
            } else {
                property.initial()
            };
            assert_eq!(
                style.get(*property),
                Some(&expected),
                "{property:?}:{keyword}"
            );
        }
    }
}

#[test]
fn all_first_properties_compute_typed_values() {
    let cases = [
        (
            "display",
            "table-cell",
            Property::Display,
            V::Keyword("table-cell"),
        ),
        (
            "color",
            "#1234",
            Property::Color,
            V::Color(Color {
                red: 17,
                green: 34,
                blue: 51,
                alpha: 68,
            }),
        ),
        (
            "background-color",
            "#abcdef80",
            Property::BackgroundColor,
            V::Color(Color {
                red: 171,
                green: 205,
                blue: 239,
                alpha: 128,
            }),
        ),
        (
            "font-family",
            "\"serif\", Times New Roman, sans-serif",
            Property::FontFamily,
            V::FontFamily(vec![
                Family::Named("serif".into()),
                Family::Named("Times New Roman".into()),
                Family::Generic("sans-serif"),
            ]),
        ),
        (
            "font-size",
            "150%",
            Property::FontSize,
            V::Length(Length::Px(24.0)),
        ),
        ("font-weight", "900", Property::FontWeight, V::Weight(900)),
        (
            "font-style",
            "oblique",
            Property::FontStyle,
            V::Keyword("oblique"),
        ),
        (
            "text-decoration",
            "underline overline line-through",
            Property::TextDecoration,
            V::Decoration(Decoration {
                underline: true,
                overline: true,
                line_through: true,
            }),
        ),
        (
            "white-space",
            "break-spaces",
            Property::WhiteSpace,
            V::Keyword("break-spaces"),
        ),
        (
            "margin-top",
            "-2em",
            Property::MarginTop,
            V::Length(Length::Px(-32.0)),
        ),
        (
            "margin-right",
            "auto",
            Property::MarginRight,
            V::Length(Length::Auto),
        ),
        (
            "margin-bottom",
            "25%",
            Property::MarginBottom,
            V::Length(Length::Percent(25.0)),
        ),
        (
            "margin-left",
            "2rem",
            Property::MarginLeft,
            V::Length(Length::Px(32.0)),
        ),
        (
            "padding-top",
            "1px",
            Property::PaddingTop,
            V::Length(Length::Px(1.0)),
        ),
        (
            "padding-right",
            "2px",
            Property::PaddingRight,
            V::Length(Length::Px(2.0)),
        ),
        (
            "padding-bottom",
            "3px",
            Property::PaddingBottom,
            V::Length(Length::Px(3.0)),
        ),
        (
            "padding-left",
            "4px",
            Property::PaddingLeft,
            V::Length(Length::Px(4.0)),
        ),
        (
            "border-top-width",
            "thin",
            Property::BorderTopWidth,
            V::Length(Length::Px(1.0)),
        ),
        (
            "border-right-width",
            "medium",
            Property::BorderRightWidth,
            V::Length(Length::Px(3.0)),
        ),
        (
            "border-bottom-width",
            "thick",
            Property::BorderBottomWidth,
            V::Length(Length::Px(5.0)),
        ),
        (
            "border-left-width",
            "0",
            Property::BorderLeftWidth,
            V::Length(Length::Px(0.0)),
        ),
        (
            "width",
            "80%",
            Property::Width,
            V::Length(Length::Percent(80.0)),
        ),
        ("height", "auto", Property::Height, V::Length(Length::Auto)),
        (
            "list-style-type",
            "lower-roman",
            Property::ListStyleType,
            V::Keyword("lower-roman"),
        ),
        (
            "text-align",
            "center",
            Property::TextAlign,
            V::Keyword("center"),
        ),
    ];
    assert_eq!(cases.len(), Property::ALL.len());
    for (name, value, property, expected) in cases {
        assert_eq!(
            compute("", &format!("{name}:{value}")).get(property),
            Some(&expected),
            "{name}:{value}"
        );
    }
}

#[test]
fn shorthand_expansion_is_atomic_and_has_longhand_precedence() {
    for (input, expected) in [
        ("1px", [1.0, 1.0, 1.0, 1.0]),
        ("1px 2px", [1.0, 2.0, 1.0, 2.0]),
        ("1px 2px 3px", [1.0, 2.0, 3.0, 2.0]),
        ("1px 2px 3px 4px", [1.0, 2.0, 3.0, 4.0]),
    ] {
        let style = compute("", &format!("margin:{input}"));
        for (property, value) in [
            Property::MarginTop,
            Property::MarginRight,
            Property::MarginBottom,
            Property::MarginLeft,
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(style.get(property), Some(&V::Length(Length::Px(value))));
        }
    }
    let style = compute("", "margin:1px 2px!important;margin-left:9px; padding:4px;padding:1px -2px; border-width:thin thick");
    assert_eq!(
        style.get(Property::MarginLeft),
        Some(&V::Length(Length::Px(2.0)))
    );
    assert_eq!(
        style.get(Property::PaddingTop),
        Some(&V::Length(Length::Px(4.0)))
    );
    assert_eq!(
        style.get(Property::BorderRightWidth),
        Some(&V::Length(Length::Px(5.0)))
    );
    for invalid in [
        "inherit 1px",
        "initial initial",
        "1px unset",
        "1px 2px 3px 4px 5px",
        "1px,2px",
    ] {
        assert_eq!(
            compute("", &format!("margin:7px;margin:{invalid}")).get(Property::MarginTop),
            Some(&V::Length(Length::Px(7.0))),
            "{invalid}"
        );
    }
}

#[test]
fn unsupported_or_invalid_values_do_not_erase_valid_ones() {
    for invalid in [
        "nonsense",
        "rgb(1,2,3)",
        "var(--x)",
        "#12",
        "red blue",
        "revert",
        "revert-layer",
    ] {
        assert_eq!(
            compute("", &format!("color:red;color:{invalid}!important")).get(Property::Color),
            Some(&red()),
            "{invalid}"
        );
    }
    for invalid in [
        "-1px",
        "1vw",
        "1",
        "calc(1px + 2px)",
        "1e999px",
        "NaN",
        "auto junk",
    ] {
        assert_eq!(
            compute("", &format!("width:9px;width:{invalid}")).get(Property::Width),
            Some(&V::Length(Length::Px(9.0))),
            "{invalid}"
        );
    }
    for invalid in ["350", "1000", "1e2", "100.0", "bolder lighter"] {
        assert_eq!(
            compute("", &format!("font-weight:bold;font-weight:{invalid}"))
                .get(Property::FontWeight),
            Some(&V::Weight(700))
        );
    }
    assert_eq!(
        compute("", "font-family:monospace;font-family:serif foo").get(Property::FontFamily),
        Some(&V::FontFamily(vec![Family::Generic("monospace")]))
    );
    assert_eq!(
        compute(
            "",
            "text-decoration:underline;text-decoration:underline underline"
        )
        .get(Property::TextDecoration),
        Some(&V::Decoration(Decoration {
            underline: true,
            ..Decoration::default()
        }))
    );
    assert_eq!(
        compute("", "border-width:thin;border-width:10%").get(Property::BorderTopWidth),
        Some(&V::Length(Length::Px(1.0)))
    );
    assert_eq!(
        compute("", "font-size:20px;font-size:-1px").font_size(),
        20.0
    );
}

#[test]
fn escaped_non_hex_characters_do_not_become_color_digits() {
    for color in [
        r"#\+f0000",
        r"#00\+f00",
        r"#0000\+f",
        r"#000000\+f",
        r"#\-00000",
    ] {
        assert_eq!(
            compute("", &format!("color:red;color:{color}")).get(Property::Color),
            Some(&red()),
            "{color}"
        );
    }
}

#[test]
fn lengths_resolve_absolute_units_and_clamp_finite_overflow() {
    for value in ["1in", "72pt", "6pc", "2.54cm", "25.4mm", "101.6q", "96px"] {
        let style = compute("", &format!("width:{value}"));
        let Some(V::Length(Length::Px(px))) = style.get(Property::Width) else {
            panic!("missing px")
        };
        assert!((px - 96.0).abs() < 0.000001, "{value}: {px}");
    }
    let style = compute(
        "",
        "font-size:1e308em;width:1e308em;margin:-1e308em;padding:1e308%;height:1e999px",
    );
    assert_eq!(style.font_size(), MAX_ABSOLUTE_VALUE);
    assert_eq!(
        style.get(Property::Width),
        Some(&V::Length(Length::Px(MAX_ABSOLUTE_VALUE)))
    );
    assert_eq!(
        style.get(Property::MarginTop),
        Some(&V::Length(Length::Px(-MAX_ABSOLUTE_VALUE)))
    );
    assert_eq!(
        style.get(Property::PaddingTop),
        Some(&V::Length(Length::Percent(MAX_ABSOLUTE_VALUE)))
    );
    assert_eq!(style.get(Property::Height), Some(&V::Length(Length::Auto)));
    let zero = compute("", "font-size:0;width:1e308em");
    assert_eq!(zero.get(Property::Width), Some(&V::Length(Length::Px(0.0))));
}

#[test]
fn relative_font_weights_and_keyword_sizes_follow_ua_tables() {
    let nodes = tree();
    let set = StyleSet::default();
    for (weight, bolder, lighter) in [
        (100, 400, 100),
        (200, 400, 100),
        (300, 400, 100),
        (400, 700, 100),
        (500, 700, 100),
        (600, 900, 400),
        (700, 900, 400),
        (800, 900, 700),
        (900, 900, 700),
    ] {
        let parent = compute("", &format!("font-weight:{weight}"));
        for (keyword, expected) in [("bolder", bolder), ("lighter", lighter)] {
            let child = set
                .compute(
                    &Handle {
                        nodes: &nodes,
                        index: 3,
                    },
                    Some(&parent),
                    16.0,
                    &format!("font-weight:{keyword}"),
                )
                .unwrap();
            assert_eq!(child.get(Property::FontWeight), Some(&V::Weight(expected)));
        }
    }
    for (keyword, px) in [
        ("xx-small", 9.0),
        ("x-small", 10.0),
        ("small", 13.0),
        ("medium", 16.0),
        ("large", 18.0),
        ("x-large", 24.0),
        ("xx-large", 32.0),
        ("larger", 19.2),
        ("smaller", 16.0 / 1.2),
    ] {
        assert!((compute("", &format!("font-size:{keyword}")).font_size() - px).abs() < 0.000001);
    }
}

#[test]
fn at_rules_and_invalid_selector_lists_never_leak_styles() {
    let css = "@import 'x'; @media screen{p{color:blue!important}} @supports(display:block){p{color:blue}}
        @layer{p{color:blue}} @font-face{color:blue} p,:unknown{color:blue} p{color:red}";
    assert_eq!(compute(css, "").get(Property::Color), Some(&red()));
    assert_eq!(
        compute("p{color:red}p:is(:unknown,*){color:blue}", "").get(Property::Color),
        Some(&blue())
    );
    assert_eq!(
        compute("p{color:red; &{color:blue}}", "").get(Property::Color),
        Some(&red())
    );
}

#[test]
fn ua_sheet_uses_the_same_pipeline_and_has_no_hidden_injection() {
    let mut nodes = tree();
    let body = Handle {
        nodes: &nodes,
        index: 1,
    };
    assert_eq!(
        StyleSet::default()
            .compute(&body, None, 16.0, "")
            .unwrap()
            .get(Property::Display),
        Some(&V::Keyword("inline"))
    );
    let set = StyleSet::new(&[(Origin::UserAgent, HTML_UA_CSS)]).unwrap();
    let body_style = set.compute(&body, None, 16.0, "").unwrap();
    assert_eq!(
        body_style.get(Property::Display),
        Some(&V::Keyword("block"))
    );
    assert_eq!(
        body_style.get(Property::MarginTop),
        Some(&V::Length(Length::Px(8.0)))
    );
    for (tag, display) in [
        ("script", "none"),
        ("table", "table"),
        ("tr", "table-row"),
        ("td", "table-cell"),
        ("li", "list-item"),
    ] {
        nodes[3].tag = tag;
        assert_eq!(
            set.compute(
                &Handle {
                    nodes: &nodes,
                    index: 3
                },
                None,
                16.0,
                ""
            )
            .unwrap()
            .get(Property::Display),
            Some(&V::Keyword(display))
        );
    }
    nodes[3].tag = "h1";
    assert_eq!(
        set.compute(
            &Handle {
                nodes: &nodes,
                index: 3
            },
            None,
            16.0,
            ""
        )
        .unwrap()
        .font_size(),
        32.0
    );
    nodes[3].tag = "pre";
    let pre = set
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3,
            },
            None,
            16.0,
            "",
        )
        .unwrap();
    assert_eq!(pre.get(Property::WhiteSpace), Some(&V::Keyword("pre")));
    let link = set
        .compute(
            &Handle {
                nodes: &nodes,
                index: 6,
            },
            None,
            16.0,
            "",
        )
        .unwrap();
    assert_eq!(link.get(Property::Color), Some(&blue()));
    let overridden = StyleSet::new(&[
        (Origin::UserAgent, HTML_UA_CSS),
        (Origin::Author, "a{color:red}"),
    ])
    .unwrap();
    assert_eq!(
        overridden
            .compute(
                &Handle {
                    nodes: &nodes,
                    index: 6
                },
                None,
                16.0,
                ""
            )
            .unwrap()
            .get(Property::Color),
        Some(&red())
    );
}

#[test]
fn aggregate_admission_limits_and_context_are_enforced() {
    assert!(StyleSet::new(&vec![(Origin::Author, ""); MAX_SHEETS]).is_ok());
    assert!(matches!(
        StyleSet::new(&vec![(Origin::Author, ""); MAX_SHEETS + 1]),
        Err(Error::Limit("MAX_SHEETS"))
    ));
    let one = " ".repeat(crate::MAX_INPUT_BYTES);
    let sheets = vec![(Origin::Author, one.as_str()); 4];
    assert!(StyleSet::new(&sheets).is_ok());
    assert!(matches!(
        StyleSet::new(&[(Origin::Author, one.as_str()); 5]),
        Err(Error::Limit("MAX_STYLE_BYTES"))
    ));
    let many_rules = "p{}".repeat(MAX_RULES);
    assert!(StyleSet::new(&[(Origin::Author, &many_rules)]).is_ok());
    assert!(matches!(
        StyleSet::new(&[(Origin::Author, &many_rules), (Origin::Author, "p{}")]),
        Err(Error::Limit("MAX_RULES"))
    ));
    let many_declarations = format!("p{{{}}}", "margin:0;".repeat(MAX_DECLARATIONS / 4));
    assert!(StyleSet::new(&[(Origin::Author, &many_declarations)]).is_ok());
    assert!(matches!(
        StyleSet::new(&[
            (Origin::Author, &many_declarations),
            (Origin::Author, "p{color:red}")
        ]),
        Err(Error::Limit("MAX_DECLARATIONS"))
    ));
    let nodes = tree();
    for bad in [f64::NAN, f64::INFINITY, -1.0, MAX_ABSOLUTE_VALUE + 1.0] {
        assert_eq!(
            StyleSet::default().compute(
                &Handle {
                    nodes: &nodes,
                    index: 3
                },
                None,
                bad,
                ""
            ),
            Err(Error::InvalidContext)
        );
    }
    let families = vec!["Arial"; values::MAX_FAMILIES].join(",");
    assert!(StyleSet::default()
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3
            },
            None,
            16.0,
            &format!("font-family:{families}")
        )
        .is_ok());
    assert!(matches!(
        StyleSet::default().compute(
            &Handle {
                nodes: &nodes,
                index: 3
            },
            None,
            16.0,
            &format!("font-family:{families},Arial")
        ),
        Err(Error::Limit("MAX_FAMILIES"))
    ));
    let selector = ".first".repeat(crate::selectors::MAX_SELECTOR_PARTS);
    let sheet = format!("{selector}{{color:red}}");
    let set = StyleSet::new(&vec![(Origin::Author, sheet.as_str()); 16]).unwrap();
    assert_eq!(
        set.compute(
            &Handle {
                nodes: &nodes,
                index: 3
            },
            None,
            16.0,
            ""
        ),
        Err(Error::Limit("MAX_MATCH_STEPS"))
    );
}

#[test]
fn realistic_class_rule_fanout_exposes_unindexed_work() {
    // Original synthetic shape: thousands of unrelated utility classes,
    // and a modest multi-class element. No downloaded stylesheet bytes.
    let css: String = (0..5000)
        .map(|i| format!(".utility-{i}{{color:red}}"))
        .collect();
    let mut nodes = tree();
    nodes[3].attrs = vec![("class", "article body primary wide visible".into())];
    let set = StyleSet::new(&[(Origin::Author, &css)]).unwrap();
    assert_eq!(
        set.compute(
            &Handle {
                nodes: &nodes,
                index: 3
            },
            None,
            16.0,
            ""
        ),
        Err(Error::Limit("MAX_MATCH_STEPS"))
    );
}

#[test]
fn conditional_variables_do_not_masquerade_as_supported_coverage() {
    let css = ":root { --ink:blue } @media screen { p { color:blue } } \
               p { color:red; color:var(--ink); width:calc(100% - 2px) } \
               p:hover { color:blue } p::before { color:blue }";
    let set = StyleSet::new(&[(Origin::Author, css)]).unwrap();
    assert_eq!(set.rules.len(), 2);
    assert_eq!(compute(css, "").get(Property::Color), Some(&red()));
    assert_eq!(
        compute(css, "").get(Property::Width),
        Some(&V::Length(Length::Auto))
    );
}
