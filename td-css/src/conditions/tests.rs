#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
#[test]
fn media_types_logic_features_and_range_examples() {
    let env = Environment {
        width: 800.0,
        height: 600.0,
        resolution_dppx: 2.0,
        color_scheme: ColorScheme::Dark,
        reduced_motion: true,
        ..Environment::default()
    };
    for q in [
        "",
        "all",
        "screen",
        "only SCREEN",
        "not print",
        "not unknown",
        "screen and (min-width:40em)",
        "(400px <= width <= 800px)",
        "(800px >= width > 400px)",
        "(width:800px)",
        "(height >= 600px)",
        "(80Q < width)",
        "(width)",
        "(resolution)",
        "(min-resolution:192dpi)",
        "(resolution:2dppx)",
        "(resolution:2x)",
        "(prefers-color-scheme:dark)",
        "(prefers-reduced-motion:reduce)",
        "(orientation:landscape)",
        "(unknown:thing) or (width:800px)",
        "bad syntax, screen",
        "not (width:1px)",
        "((width:800px) and (height:600px))",
        "screen and not (width:1px)",
        "(width > -1px)",
        "(prefers-color-scheme)",
    ] {
        assert_eq!(media_matches(q, &env), Ok(true), "{q}");
    }
    for q in [
        "print",
        "not screen",
        "only (width)",
        "screen or (width)",
        "not (unknown:thing)",
        "(unknown:thing)",
        "not unknown(function)",
        "(width > 800px)",
        "(800px < width)",
        "(400px < width > 200px)",
        "(width < = 900px)",
        "(width:1em)",
        "(min-orientation:portrait)",
        "(min-prefers-color-scheme:dark)",
        "(width:NaN)",
        "(width:1e999px)",
        "(width:2vw)",
        "(width) and (height) or (resolution)",
        "not not (width)",
        "layer",
        ",",
        "screen and (width) or (height)",
        "screen and",
        "(prefers-color-scheme:unknown)",
    ] {
        assert_eq!(media_matches(q, &env), Ok(false), "{q}");
    }
}
#[test]
fn environment_preferences_boolean_and_print() {
    let env = Environment {
        media_type: MediaType::Print,
        width: 0.0,
        height: 0.0,
        resolution_dppx: 0.0,
        ..Environment::default()
    };
    for q in [
        "print",
        "(prefers-color-scheme:light)",
        "(prefers-reduced-motion:no-preference)",
        "(orientation:portrait)",
    ] {
        assert_eq!(media_matches(q, &env), Ok(true), "{q}");
    }
    for q in [
        "screen",
        "(width)",
        "(height)",
        "(resolution)",
        "(prefers-reduced-motion)",
    ] {
        assert_eq!(media_matches(q, &env), Ok(false), "{q}");
    }
    for n in [
        f64::NAN,
        f64::INFINITY,
        -1.0,
        values::MAX_ABSOLUTE_VALUE + 1.0,
    ] {
        assert_eq!(
            media_matches(
                "all",
                &Environment {
                    width: n,
                    ..Environment::default()
                }
            ),
            Err(Error::InvalidContext)
        );
    }
    assert_eq!(
        media_matches(
            "all",
            &Environment {
                resolution_dppx: MAX_RESOLUTION_DPPX + 1.0,
                ..Environment::default()
            }
        ),
        Err(Error::InvalidContext)
    );
}
#[test]
fn supports_uses_property_grammar_and_boolean_not_unknown() {
    for q in [
        "(width:calc(1px + 2px))",
        "(color:red)",
        "(color:var(--x))",
        "(--x:red)",
        "(margin:1px 2em)",
        "(color:red!important)",
        "not (future:value)",
        "(color:red) and (display:block)",
        "(color:invalid) or (padding:0)",
        "not future()",
        "((color:red))",
    ] {
        assert_eq!(supports_matches(q), Ok(true), "{q}");
    }
    for q in [
        "(color:invalid)",
        "(color:red; color:blue)",
        "(color:red) and (display:block) or (padding:0)",
        "not not (color:red)",
        "color:red",
        "(color:red) and",
        "",
        "(color:red ])",
    ] {
        assert_eq!(supports_matches(q), Ok(false), "{q}");
    }
}
#[test]
fn query_bounds_apply_to_false_and_unknown_branches() {
    let env = Environment::default();
    assert!(media_matches(&vec!["screen"; MAX_MEDIA_QUERIES].join(","), &env).unwrap());
    assert_eq!(
        media_matches(&vec!["print"; MAX_MEDIA_QUERIES + 1].join(","), &env),
        Err(Error::Limit("MAX_MEDIA_QUERIES"))
    );
    let nested = format!(
        "{}width{}",
        "(".repeat(MAX_CONDITION_DEPTH),
        ")".repeat(MAX_CONDITION_DEPTH)
    );
    assert!(media_matches(&nested, &env).is_ok());
    assert_eq!(
        media_matches(&format!("({nested})"), &env),
        Err(Error::Limit("MAX_CONDITION_DEPTH"))
    );
    let wide = format!("future({})", "x ".repeat(MAX_CONDITION_PARTS));
    assert_eq!(
        media_matches(&wide, &env),
        Err(Error::Limit("MAX_CONDITION_PARTS"))
    );
    assert_eq!(
        supports_matches(&wide),
        Err(Error::Limit("MAX_CONDITION_PARTS"))
    );
}

#[test]
fn supports_does_not_recover_a_declaration_after_an_at_rule() {
    assert_eq!(supports_matches("(@unknown {} color:red)"), Ok(false));
    assert_eq!(supports_matches("not (@unknown {} color:red)"), Ok(true));
}

#[test]
fn media_commas_count_toward_component_admission() {
    let env = Environment::default();
    let exact = format!("{}a,", "a ".repeat(1023));
    assert_eq!(media_matches(&exact, &env), Ok(false));
    assert_eq!(
        media_matches(&format!("{exact}b"), &env),
        Err(Error::Limit("MAX_CONDITION_PARTS"))
    );
}

// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod sizes {
    use super::*;

    fn viewport(width: f64, height: f64) -> Environment {
        Environment {
            width,
            height,
            ..Environment::default()
        }
    }
    fn size(input: &str, env: &Environment) -> f64 {
        source_size(input, env, &crate::memory().unwrap()).unwrap()
    }
    fn close(actual: f64, expected: f64, input: &str) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "{input}: {actual} != {expected}"
        );
    }

    // HTML §4.8.4.2.2, "Sizes attributes": the example's three entries.
    #[test]
    fn sizes_spec_example_picks_the_first_matching_entry() {
        let sizes = "(max-width: 30em) 100vw, (max-width: 50em) 50vw, calc(33vw - 100px)";
        close(size(sizes, &viewport(320.0, 600.0)), 320.0, sizes);
        close(size(sizes, &viewport(700.0, 600.0)), 350.0, sizes);
        close(size(sizes, &viewport(1280.0, 720.0)), 322.4, sizes);
        // The first matching entry wins, and one without a condition matches.
        close(
            size("10px, 20px", &viewport(1280.0, 720.0)),
            10.0,
            "10px, 20px",
        );
        close(
            size("(min-width: 1px) 30px, 20px", &viewport(1280.0, 720.0)),
            30.0,
            "condition",
        );
    }

    #[test]
    fn sizes_skip_negative_percentage_and_invalid_entries() {
        let env = viewport(1000.0, 500.0);
        for (input, expected) in [
            ("-1px, 20px", 20.0),
            ("50%, 20px", 20.0),
            ("calc(50% + 1px), 20px", 20.0),
            ("foo, 20px", 20.0),
            ("1, 20px", 20.0),
            ("0, 20px", 0.0),
            ("-0px, 20px", 0.0),
            ("10vw 20px, 30px", 30.0),
            ("(min-width: 1px) 1px 2px, 30px", 30.0),
            ("screen 10px, 30px", 30.0),
            ("screen and (min-width: 1px) 10px, 30px", 30.0),
            ("(unknown: 1) 10px, 30px", 30.0),
            ("(width > 1px) or (unknown) 10px, 30px", 10.0),
            ("not (max-width: 10px) 10px, 30px", 10.0),
            ("(max-width: 10px) 10px, 30px", 30.0),
            ("10px 20px, 30px", 30.0),
            ("attr(x), 30px", 30.0),
            ("calc(1px), 30px", 1.0),
            ("calc(1), 30px", 30.0),
            ("calc(1px / 0), 30px", 30.0),
            ("calc(1px * 1px), 30px", 30.0),
            ("calc(1px+1px), 30px", 30.0),
            ("calc(1px+ 1px), 30px", 30.0),
            ("calc(1px +(1px)), 30px", 30.0),
            ("calc(1px + (1px)), 30px", 2.0),
            ("calc(10px - 20px), 30px", 0.0),
            ("10px, , 30px", 10.0),
            (", 30px", 30.0),
            ("\"x\" 10px, 30px", 30.0),
            ("(min-width:1px)10px", 10.0),
            ("1e999px, 30px", values::MAX_ABSOLUTE_VALUE),
            ("-1e999px, 30px", 30.0),
            ("calc(1e999px), 30px", values::MAX_ABSOLUTE_VALUE),
            ("1e300px", values::MAX_ABSOLUTE_VALUE),
        ] {
            close(size(input, &env), expected, input);
        }
    }

    #[test]
    fn sizes_auto_empty_and_no_match_give_the_viewport_width() {
        let env = viewport(900.0, 400.0);
        for input in [
            "",
            "   ",
            "\t\n",
            "auto",
            "AUTO",
            "(max-width: 10px) 10px",
            "(max-width: 10px) 10px, auto",
            "-5px",
            "(min-width: 1px) auto",
            "\u{feff}10px",
        ] {
            close(size(input, &env), 900.0, input);
        }
        // td allows no auto-sizes, so HTML skips `auto` wherever it stands.
        close(size("auto, 50vw", &env), 450.0, "auto, 50vw");
    }

    #[test]
    fn sizes_units_resolve_against_the_initial_font_and_viewport() {
        let landscape = viewport(1000.0, 500.0);
        let portrait = viewport(400.0, 800.0);
        for (input, expected) in [
            ("10px", 10.0),
            ("2em", 32.0),
            ("2REM", 32.0),
            ("2ex", 16.0),
            ("2ch", 16.0),
            ("72pt", 96.0),
            ("1pc", 16.0),
            ("1in", 96.0),
            ("2.54cm", 96.0),
            ("25.4mm", 96.0),
            ("101.6q", 96.0),
        ] {
            close(size(input, &landscape), expected, input);
            close(size(input, &portrait), expected, input);
        }
        for prefix in ["", "s", "l", "d"] {
            for (unit, wide, tall) in [
                ("vw", 100.0, 40.0),
                ("vh", 50.0, 80.0),
                ("vmin", 50.0, 40.0),
                ("vmax", 100.0, 80.0),
            ] {
                let input = format!("10{prefix}{unit}");
                close(size(&input, &landscape), wide, &input);
                close(size(&input, &portrait), tall, &input);
            }
        }
        // Units td does not resolve here leave the entry invalid.
        for input in ["10vi", "10cqw", "10lh", "10deg"] {
            close(size(&format!("{input}, 7px"), &landscape), 7.0, input);
        }
    }

    #[test]
    fn sizes_math_functions_nest() {
        let env = viewport(1000.0, 500.0);
        for (input, expected) in [
            ("calc(10px + 2 * 5px)", 20.0),
            ("calc((10px + 2px) * 2)", 24.0),
            ("calc(calc(10vw / 2) - 1em)", 34.0),
            ("calc(100px / 4 * 2)", 50.0),
            ("calc(2 * 3 * 1px)", 6.0),
            ("min(10vw, 50px)", 50.0),
            ("max(10vw, 50px)", 100.0),
            ("min(1px)", 1.0),
            ("min(calc(10px + 5px), 20px, 3em)", 15.0),
            ("clamp(10px, 50vw, 300px)", 300.0),
            ("clamp(10px, 1vw, 300px)", 10.0),
            ("clamp(10px, 20px, 30px)", 20.0),
            ("clamp(40px, 20px, 30px)", 40.0),
            ("calc(max(1px, 2px) + min(3px, 4px))", 5.0),
            ("CALC( 1px + 1px )", 2.0),
        ] {
            close(size(input, &env), expected, input);
        }
        for input in [
            "min()",
            "min(1px, 2)",
            "clamp(1px, 2px)",
            "clamp(1px, 2px, 3px, 4px)",
            "max(1px,)",
            "calc(1px, 2px)",
            "calc([1px])",
            "calc(abs(1px))",
        ] {
            close(size(&format!("{input}, 7px"), &env), 7.0, input);
        }
    }

    #[test]
    fn sizes_math_is_bounded_by_the_calc_limits() {
        let env = Environment::default();
        let memory = crate::memory().unwrap();
        // Root plus MAX_CALC_DEPTH - 1 nested calc()s.
        let depth = crate::calc::MAX_CALC_DEPTH;
        let deep = |levels: usize| format!("{}1px{}", "calc(".repeat(levels), ")".repeat(levels));
        close(size(&deep(depth), &env), 1.0, "deep");
        assert_eq!(
            source_size(&deep(depth + 1), &env, &memory),
            Err(Error::Limit("MAX_CALC_DEPTH"))
        );
        // Behind a condition, whether or not it matches.
        for condition in ["(min-width: 0)", "(max-width: 0)"] {
            assert_eq!(
                source_size(&format!("{condition} {}", deep(depth + 1)), &env, &memory),
                Err(Error::Limit("MAX_CALC_DEPTH")),
                "{condition}"
            );
        }
        // The root, two spaces and the first term, then " + 1px" as four
        // parts each; a block around the first term adds one more.
        let parts = crate::calc::MAX_CALC_PARTS;
        let terms = (parts - 4) / 4;
        assert_eq!(4 + 4 * terms, parts);
        let wide = |first: &str| format!("calc( {first}{} )", " + 1px".repeat(terms));
        close(size(&wide("1px"), &env), (terms + 1) as f64, "wide");
        assert_eq!(
            source_size(&wide("(1px)"), &env, &memory),
            Err(Error::Limit("MAX_CALC_PARTS"))
        );
        // A function that is no math function is not walked as one.
        let other = format!(
            "{}1px{}, 3px",
            "x(".repeat(depth + 1),
            ")".repeat(depth + 1)
        );
        close(size(&other, &env), 3.0, "other");
    }

    #[test]
    fn sizes_input_and_entry_bounds() {
        let env = Environment::default();
        let memory = crate::memory().unwrap();
        let long = format!("{}1px", " ".repeat(MAX_SIZES_BYTES - 3));
        assert_eq!(long.len(), MAX_SIZES_BYTES);
        close(size(&long, &env), 1.0, "long");
        assert_eq!(
            source_size(&format!(" {long}"), &env, &memory),
            Err(Error::Limit("MAX_SIZES_BYTES"))
        );
        let entries = |n: usize| vec!["1px"; n].join(",");
        close(size(&entries(MAX_SIZES_ENTRIES), &env), 1.0, "entries");
        assert_eq!(
            source_size(&entries(MAX_SIZES_ENTRIES + 1), &env, &memory),
            Err(Error::Limit("MAX_SIZES_ENTRIES"))
        );
        // Later entries are read after a match: the refusal does not depend
        // on which entry the environment picks.
        let deep = format!(
            "1px, {}width{} 2px",
            "(".repeat(MAX_CONDITION_DEPTH + 1),
            ")".repeat(MAX_CONDITION_DEPTH + 1)
        );
        assert_eq!(
            source_size(&deep, &env, &memory),
            Err(Error::Limit("MAX_CONDITION_DEPTH"))
        );
        // Condition parts are shared across the entries.
        let half = format!("(x{}) 1px", " x".repeat(MAX_CONDITION_PARTS / 4));
        let shared = [half.as_str(); 2].join(",");
        assert!(shared.len() <= MAX_SIZES_BYTES);
        close(size(&half, &env), env.width, "half");
        assert_eq!(
            source_size(&shared, &env, &memory),
            Err(Error::Limit("MAX_CONDITION_PARTS"))
        );
        assert_eq!(
            source_size(
                "1px",
                &Environment {
                    width: f64::NAN,
                    ..Environment::default()
                },
                &memory
            ),
            Err(Error::InvalidContext)
        );
    }

    #[test]
    fn sizes_and_media_charge_the_given_budget() {
        let env = Environment::default();
        let tiny = td_alloc::Budget::new(td_alloc::Budget::metadata_bytes() + 8).unwrap();
        assert!(matches!(
            source_size("(min-width: 1px) calc(1px + 1px)", &env, &tiny),
            Err(Error::Memory(_))
        ));
        assert!(matches!(
            media_matches_with_budget("screen and (min-width: 1px)", &env, &tiny),
            Err(Error::Memory(_))
        ));
        let ample = td_alloc::Budget::new(td_alloc::Budget::metadata_bytes() + 64 * 1024).unwrap();
        let before = ample.used();
        assert_eq!(
            source_size("(min-width: 1px) calc(1px + 1px)", &env, &ample),
            Ok(2.0)
        );
        assert_eq!(
            media_matches_with_budget("screen and (min-width: 1px)", &env, &ample),
            Ok(true)
        );
        assert_eq!(ample.used(), before);
    }

    #[test]
    fn media_with_budget_agrees_with_media_matches() {
        let memory = crate::memory().unwrap();
        let queries = [
            "",
            " ",
            "all",
            "print",
            "screen and (min-width: 600px)",
            "(min-resolution: 2dppx)",
            "(resolution: 1x)",
            "(max-resolution: 96dpi)",
            "(400px <= width <= 800px)",
            "(width > 800px)",
            "not screen",
            "not (min-width: 600px)",
            "(max-width: 100px) or (min-height: 500px)",
            "(max-width: 100px), print, (orientation: portrait)",
            "bad syntax, screen",
            "(width:2vw)",
            "only screen and (prefers-color-scheme: dark)",
        ];
        for env in [
            viewport(320.0, 640.0),
            viewport(1280.0, 720.0),
            Environment {
                resolution_dppx: 2.0,
                color_scheme: ColorScheme::Dark,
                ..viewport(700.0, 400.0)
            },
        ] {
            for q in queries {
                assert_eq!(
                    media_matches_with_budget(q, &env, &memory),
                    media_matches(q, &env),
                    "{q}"
                );
            }
        }
        // An attribute's U+FEFF is text, where media_matches decodes a sheet.
        let env = Environment::default();
        assert_eq!(media_matches("\u{feff}screen", &env), Ok(true));
        assert_eq!(
            media_matches_with_budget("\u{feff}screen", &env, &memory),
            Ok(false)
        );
        let long = format!("{}screen", " ".repeat(MAX_MEDIA_BYTES - 6));
        assert_eq!(media_matches_with_budget(&long, &env, &memory), Ok(true));
        assert_eq!(
            media_matches_with_budget(&format!(" {long}"), &env, &memory),
            Err(Error::Limit("MAX_MEDIA_BYTES"))
        );
    }

    #[test]
    fn sizes_negative_zero_is_positive_zero() {
        let env = Environment::default();
        for input in [
            "-0",
            "-0px",
            "-0.0px",
            "-0vw",
            "-0e-0px",
            "calc(-0px)",
            "calc(-0 * 1px)",
            "min(-0px, 1px)",
            "calc(1px - 2px)",
        ] {
            let size = size(input, &env);
            assert!(size == 0.0 && size.is_sign_positive(), "{input}: {size}");
        }
    }

    #[test]
    fn sizes_refusal_and_overflow_do_not_depend_on_the_viewport() {
        let memory = crate::memory().unwrap();
        let wide = viewport(1000.0, 500.0);
        let narrow = viewport(100.0, 500.0);
        let deep = format!(
            "{}x{}",
            "(".repeat(MAX_CONDITION_DEPTH + 1),
            ")".repeat(MAX_CONDITION_DEPTH + 1)
        );
        for input in [
            format!("{deep} calc(1e308vw), 5px"),
            format!("{deep} 1%, 5px"),
            format!("{deep} 1px, 5px"),
        ] {
            for env in [wide, narrow] {
                assert_eq!(
                    source_size(&input, &env, &memory),
                    Err(Error::Limit("MAX_CONDITION_DEPTH")),
                    "{input}"
                );
            }
        }
        // Condition parts are counted whether or not the size is valid.
        let half = format!("(x{}) ", " x".repeat(MAX_CONDITION_PARTS / 4));
        let shared = format!("{half}1px, {half}1%");
        for env in [wide, narrow] {
            assert_eq!(
                source_size(&shared, &env, &memory),
                Err(Error::Limit("MAX_CONDITION_PARTS"))
            );
        }
        for input in [
            "1e308vw",
            "calc(1e308vw)",
            "calc(1e308vw * 1e308)",
            "calc(1e308vw + 1e308vw)",
            "max(1e308vw, 1px)",
            "calc(1e308vw / 1e-308)",
        ] {
            for env in [wide, narrow] {
                assert_eq!(
                    source_size(input, &env, &memory),
                    Ok(values::MAX_ABSOLUTE_VALUE),
                    "{input}"
                );
            }
        }
        // A saturated term can still be cancelled.
        for env in [wide, narrow] {
            assert_eq!(
                source_size("calc(1e308vw - 1e308vw + 5px)", &env, &memory),
                Ok(5.0)
            );
            assert_eq!(
                source_size("calc(1e999px - 1e999px + 5px)", &env, &memory),
                Ok(5.0)
            );
            // Intermediates saturate at f64's range, not the result's
            // bound: a term past MAX_ABSOLUTE_VALUE still cancels exactly.
            assert_eq!(
                source_size("calc(2e6px - 1.5e6px)", &env, &memory),
                Ok(500_000.0)
            );
        }
    }
}
