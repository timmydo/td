#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    syntax,
    test_tree::{tree, Handle},
    values::{ComputedValue, Property},
    ComputedStyle, StyleSet,
};
fn parsed(input: &str, percent: bool) -> Result<Option<Linear>, Error> {
    let v = syntax::parse_components(input)?;
    parse(v.value().first().unwrap(), percent)
}
fn style(input: &str) -> ComputedStyle {
    let nodes = tree();
    StyleSet::default()
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3,
            },
            None,
            16.0,
            input,
        )
        .unwrap()
}
fn length(input: &str, property: Property) -> Length {
    match style(input).get(property).unwrap() {
        ComputedValue::Length(n) => *n,
        _ => panic!("expected length"),
    }
}
#[test]
fn arithmetic_precedence_types_and_whitespace_follow_values_3() {
    for (input, want) in [
        ("calc(2px + 3px * 4)", 14.0),
        ("calc((2px + 3px) * 4)", 20.0),
        ("calc(8px / 2 / 2)", 2.0),
        ("calc(3 * calc(2px + 4px))", 18.0),
        ("calc(2pc + 3pt)", 36.0),
        ("calc(0px / 1e-308)", 0.0),
        ("calc(1px + -2px)", -1.0),
    ] {
        assert_eq!(
            parsed(input, true)
                .unwrap()
                .unwrap()
                .compute(16.0, 16.0, None, false),
            Length::Px(want),
            "{input}"
        );
    }
    for input in [
        "calc(0)",
        "calc(0 + 1px)",
        "calc(2px * 2px)",
        "calc(2px / 1px)",
        "calc(2px / (1 - 1))",
        "calc(1px+ 2px)",
        "calc(1px +2px)",
        "calc(1px/**/+/**/2px)",
        "calc(1px +)",
        "calc(1px, 2px)",
        "calc(1px + 2s)",
        "calc(auto)",
        "calc(1e999px)",
        "calc(1e308px * 2)",
        "calc(1px / 1e-999)",
        "calc([1px])",
        "calc(min(1px,2px))",
    ] {
        assert!(parsed(input, true).unwrap().is_none(), "{input}");
    }
    assert!(parsed("calc(10% - 10% + 2px)", false).unwrap().is_none());
}
#[test]
fn computed_percentages_and_font_units_preserve_their_basis() {
    assert_eq!(
        length("font-size:calc(1em + 50% + 2px)", Property::FontSize),
        Length::Px(26.0)
    );
    assert_eq!(
        length("font-size:20px;width:calc(2em + 3rem)", Property::Width),
        Length::Px(100.0)
    );
    let mixed = length("font-size:20px;width:calc(50% - 2em)", Property::Width);
    assert_eq!(
        mixed,
        Length::Calc {
            px: -40.0,
            percent: 50.0,
            non_negative: true
        }
    );
    assert_eq!(mixed.resolve(200.0), Some(60.0));
    assert_eq!(mixed.resolve(20.0), Some(0.0));
    assert_eq!(
        length("width:calc(100% - 100% + 1em)", Property::Width),
        Length::Calc {
            px: 16.0,
            percent: 0.0,
            non_negative: true
        }
    );
    assert_eq!(
        length("margin-left:calc(2px - 10%)", Property::MarginLeft).resolve(100.0),
        Some(-8.0)
    );
    assert_eq!(
        length("font-size:calc(-10px)", Property::FontSize),
        Length::Px(0.0)
    );
    assert_eq!(
        length("width:12px;width:-1px", Property::Width),
        Length::Px(12.0)
    );
    assert_eq!(
        length("width:12px;width:calc(-1px)", Property::Width),
        Length::Px(0.0)
    );
    let Length::Px(large) = length(
        "font-size:1000000px;width:calc(1e308em - 1e308rem + 1px)",
        Property::Width,
    ) else {
        panic!("px");
    };
    assert!((large - 1.0).abs() < 1e-12);
    assert!(Length::Auto.resolve(10.0).is_none());
    assert!(mixed.resolve(f64::NAN).is_none());
    assert!(mixed.resolve(-1.0).is_none());
}
#[test]
fn substitution_shorthands_and_invalid_computed_calc() {
    assert_eq!(
        length(
            "--gap:2px;margin:calc(var(--gap) * 3)",
            Property::MarginBottom
        ),
        Length::Px(6.0)
    );
    assert_eq!(
        length(
            "width:12px;--n:1px;width:calc(var(--n) + 0)",
            Property::Width
        ),
        Length::Auto
    );
    assert_eq!(
        length(
            "border-width:7px;border-width:calc(10% - 10%)",
            Property::BorderTopWidth
        ),
        Length::Px(7.0)
    );
    assert!(crate::conditions::supports_matches("(width:calc(100% - 2em))").unwrap());
    assert!(!crate::conditions::supports_matches("(width:calc(1px * 2px))").unwrap());
}
#[test]
fn arithmetic_admission_counts_ignored_tails_and_exact_depth() {
    let nested = |n: usize| format!("{}1px{}", "calc(".repeat(n), ")".repeat(n));
    assert!(parsed(&nested(MAX_CALC_DEPTH), true).unwrap().is_some());
    assert_eq!(
        parsed(&nested(MAX_CALC_DEPTH + 1), true).unwrap_err(),
        Error::Limit("MAX_CALC_DEPTH")
    );
    // Each term after the first contributes whitespace, +, whitespace, dimension.
    let exact = format!("calc( 1px{} )", " + 1px".repeat(255));
    assert!(parsed(&exact, true).unwrap().is_some());
    let over = format!("calc( 1px{} x)", " + 1px".repeat(255));
    assert_eq!(
        parsed(&over, true).unwrap_err(),
        Error::Limit("MAX_CALC_PARTS")
    );
    let invalid_tail = format!("calc(bad{})", " x".repeat(MAX_CALC_PARTS));
    assert_eq!(
        parsed(&invalid_tail, true).unwrap_err(),
        Error::Limit("MAX_CALC_PARTS")
    );
}
