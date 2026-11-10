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
        "(width:calc(1px + 2px))",
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
