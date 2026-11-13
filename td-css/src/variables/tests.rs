#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    test_tree::{tree, Handle},
    values::{Color, ComputedValue, Length, Property},
    ComputeStats, ComputedStyle, Origin, StyleSet,
};
fn style(inline: &str) -> ComputedStyle {
    let nodes = tree();
    StyleSet::new(&[])
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
fn color(value: &str) -> Color {
    style(&format!("color:{value}")).color()
}
#[test]
fn substitutions_are_tokens_with_fallback_and_case_sensitive_names() {
    for (input, expected) in [
        ("--ink:red;color:var(--ink)", "red"),
        ("color:var(--missing,blue)", "blue"),
        ("--A:red;--a:blue;color:VAR(--A)", "red"),
        ("--ink:initial;color:var(--ink,blue)", "blue"),
        ("--ink:;color:var(--ink,blue)", "black"),
        ("color:red;color:var(--missing)", "black"),
        ("--ink:red;--ink:var(not-custom);color:var(--ink)", "red"),
        ("--ink:red;--ink:!;color:var(--ink)", "red"),
        ("color:red;color:var(bad)", "red"),
        (
            "color:var(--missing,var(--also-missing,rebeccapurple))",
            "rebeccapurple",
        ),
        ("--r:red;color:var(--r,) ", "red"),
    ] {
        assert_eq!(style(input).color(), color(expected), "{input}");
    }
    let s = style("--n:12;width:var(--n)px");
    assert_eq!(
        s.get(Property::Width),
        Some(&ComputedValue::Length(Length::Auto))
    );
    let s = style("--empty:; font-family:var(--missing,Arial,sans-serif)");
    assert_eq!(s.custom_property("--empty"), Some([].as_slice()));
    assert!(matches!(s.get(Property::FontFamily),Some(ComputedValue::FontFamily(v)) if v.len()==2));
}
#[test]
fn every_cycle_member_is_invalid_including_unused_fallback_edges() {
    for input in [
        "--a:var(--a);color:var(--a,blue)",
        "--a:var(--b);--b:var(--a);color:var(--a,blue)",
        "--ok:red;--a:var(--ok,var(--a));color:var(--a,blue)",
        "--a:var(--b) var(--c);--b:var(--a);--c:var(--b,red);color:var(--c,blue)",
    ] {
        assert_eq!(style(input).color(), color("blue"), "{input}");
    }
    assert_eq!(
        style("--cycle:var(--cycle);--valid:var(--cycle,red);color:var(--valid)").color(),
        color("red")
    );
}
#[test]
fn inherited_custom_values_are_substituted_once_but_lengths_use_current_font() {
    let nodes = tree();
    let set = StyleSet::new(&[]).unwrap();
    let e = Handle {
        nodes: &nodes,
        index: 3,
    };
    let parent = set
        .compute(
            &e,
            None,
            16.0,
            "--a:var(--b);--b:red;--space:2em;font-size:20px;color:blue;margin:2em",
        )
        .unwrap();
    let child = set
        .compute(
            &e,
            Some(&parent),
            20.0,
            "--b:blue;color:var(--a);font-size:10px;margin:var(--space)",
        )
        .unwrap();
    assert_eq!(child.color(), color("red"));
    assert_eq!(
        child.get(Property::MarginTop),
        Some(&ComputedValue::Length(Length::Px(20.0)))
    );
    let unchanged = set.compute(&e, Some(&parent), 20.0, "").unwrap();
    assert!(td_alloc::BudgetShared::ptr_eq(
        &parent.custom.entries,
        &unchanged.custom.entries
    ));
    for value in ["inherit", "unset"] {
        let child = set
            .compute(
                &e,
                Some(&parent),
                20.0,
                &format!("--b:{value};color:var(--b)"),
            )
            .unwrap();
        assert_eq!(child.color(), color("red"));
    }
    let child = set
        .compute(
            &e,
            Some(&parent),
            20.0,
            "--b:initial;color:var(--b,inherit)",
        )
        .unwrap();
    assert_eq!(child.color(), color("blue"));
}
#[test]
fn pending_shorthands_compete_before_substitution() {
    let s = style("--m:1px 2px 3px 4px;margin:var(--m);margin-left:9px");
    for (p, n) in [
        (Property::MarginTop, 1.0),
        (Property::MarginRight, 2.0),
        (Property::MarginBottom, 3.0),
        (Property::MarginLeft, 9.0),
    ] {
        assert_eq!(s.get(p), Some(&ComputedValue::Length(Length::Px(n))));
    }
    let s = style("margin:5px;margin:var(--missing);margin-left:2px");
    assert_eq!(
        s.get(Property::MarginTop),
        Some(&ComputedValue::Length(Length::Px(0.0)))
    );
    assert_eq!(
        s.get(Property::MarginLeft),
        Some(&ComputedValue::Length(Length::Px(2.0)))
    );
    let s = style("--m:1px nonsense;margin:5px;margin:var(--m)");
    assert_eq!(
        s.get(Property::MarginTop),
        Some(&ComputedValue::Length(Length::Px(0.0)))
    );
    let s = style("--m:2px;margin:var(--m)!important;margin-left:9px");
    assert_eq!(
        s.get(Property::MarginLeft),
        Some(&ComputedValue::Length(Length::Px(2.0)))
    );
}
#[test]
fn custom_cascade_obeys_origins_importance_specificity_and_inline() {
    let nodes = tree();
    let e = Handle {
        nodes: &nodes,
        index: 3,
    };
    let set = StyleSet::new(&[
        (Origin::UserAgent, "p{--a:red!important}"),
        (Origin::User, "p{--a:blue!important}"),
        (
            Origin::Author,
            "#container p{--a:rebeccapurple!important}p{color:var(--a)}",
        ),
    ])
    .unwrap();
    assert_eq!(
        set.compute(&e, None, 16.0, "--a:orange!important")
            .unwrap()
            .color(),
        color("red")
    );
    let set = StyleSet::new(&[(
        Origin::Author,
        "p{--a:red}#container p{--a:blue}p{color:var(--a)}",
    )])
    .unwrap();
    assert_eq!(
        set.compute(&e, None, 16.0, "").unwrap().color(),
        color("blue")
    );
    assert_eq!(
        set.compute(&e, None, 16.0, "--a:orange").unwrap().color(),
        color("orange")
    );
}
#[test]
fn custom_count_and_storage_have_exact_edges() {
    let nodes = tree();
    let e = Handle {
        nodes: &nodes,
        index: 3,
    };
    let definitions: String = (0..MAX_CUSTOM_PROPERTIES)
        .map(|i| format!("--x{i}:0;"))
        .collect();
    assert!(StyleSet::new(&[])
        .unwrap()
        .compute(&e, None, 16.0, &definitions)
        .is_ok());
    assert_eq!(
        StyleSet::new(&[])
            .unwrap()
            .compute(&e, None, 16.0, &format!("{definitions}--extra:0")),
        Err(Error::Limit("MAX_CUSTOM_PROPERTIES"))
    );
    let payload = "x".repeat(MAX_CUSTOM_BYTES / 64 - 7);
    let sheets: Vec<String> = (0..2)
        .map(|half| {
            format!(
                "p{{{}}}",
                (half * 32..(half + 1) * 32)
                    .map(|i| format!("--p{i:03}:\"{payload}\";"))
                    .collect::<String>()
            )
        })
        .collect();
    let refs: Vec<_> = sheets
        .iter()
        .map(|s| (Origin::Author, s.as_str()))
        .collect();
    let set = StyleSet::new(&refs).unwrap();
    let result = set.compute(&e, None, 16.0, "").unwrap();
    assert_eq!(result.custom.bytes, MAX_CUSTOM_BYTES);
    assert_eq!(
        set.compute(&e, None, 16.0, "--extra:x"),
        Err(Error::Limit("MAX_CUSTOM_BYTES"))
    );
}
#[test]
fn dependency_depth_and_expansion_work_refuse_before_unbounded_growth() {
    let nodes = tree();
    let e = Handle {
        nodes: &nodes,
        index: 3,
    };
    let set = StyleSet::new(&[]).unwrap();
    for count in [MAX_VARIABLE_DEPTH, MAX_VARIABLE_DEPTH + 1] {
        let text: String = (0..count)
            .map(|i| {
                if i + 1 == count {
                    format!("--v{i:03}:red;")
                } else {
                    format!("--v{i:03}:var(--v{:03});", i + 1)
                }
            })
            .collect();
        let result = set.compute(&e, None, 16.0, &text);
        if count == MAX_VARIABLE_DEPTH {
            assert!(result.is_ok());
        } else {
            assert_eq!(result, Err(Error::Limit("MAX_VARIABLE_DEPTH")));
        }
    }
    let mut explosive = "--v0:x;".to_owned();
    for i in 1..19 {
        explosive.push_str(&format!("--v{i}:var(--v{})var(--v{});", i - 1, i - 1));
    }
    assert_eq!(
        set.compute(&e, None, 16.0, &explosive),
        Err(Error::Limit("MAX_SUBSTITUTION_COMPONENTS"))
    );
    let mut huge = format!("--big:\"{}\";", "x".repeat(16000));
    for property in Property::ALL {
        huge.push_str(&format!(
            "{}:{};",
            property.name(),
            "var(--big)".repeat(128)
        ));
    }
    let mut stats = ComputeStats::default();
    assert_eq!(
        set.compute_with_stats(&e, None, 16.0, &huge, &mut stats),
        Err(Error::Limit("MAX_SUBSTITUTION_BYTES"))
    );
    assert!(stats.substitution_bytes > MAX_SUBSTITUTION_BYTES - 20000);
    let mut nested = "--v00:x;".to_owned();
    for i in 1..34 {
        nested.push_str(&format!("--v{i:02}:f(var(--v{:02}));", i - 1));
    }
    assert_eq!(
        set.compute(&e, None, 16.0, &nested),
        Err(Error::Limit("MAX_NESTING"))
    );
}

#[test]
fn fallback_is_a_declaration_value_even_when_unused() {
    for fallback in ["!", ";", "red !important", "var(--missing,;)"] {
        let value = format!("--a:red;color:blue;color:var(--a,{fallback})");
        assert_eq!(style(&value).color(), color("blue"), "{value}");
        assert!(
            !crate::conditions::supports_matches(&format!("(color:var(--a,{fallback}))")).unwrap()
        );
    }
    for fallback in ["(!)", "[;]", "f(!;)"] {
        assert_eq!(
            style(&format!("--a:red;color:var(--a,{fallback})")).color(),
            color("red")
        );
    }
}

#[test]
fn custom_values_apply_css_wide_defaulting_after_substitution() {
    let nodes = tree();
    let e = Handle {
        nodes: &nodes,
        index: 3,
    };
    let set = StyleSet::new(&[]).unwrap();
    let parent = style("--a:red;--b:blue");
    for (wide, want) in [("initial", "orange"), ("inherit", "red"), ("unset", "red")] {
        let source = format!("--a:var(--missing,{wide});color:var(--a,orange)");
        let s = set.compute(&e, Some(&parent), 16.0, &source).unwrap();
        assert_eq!(s.color(), color(want), "{wide}");
        assert_eq!(
            set.compute(&e, None, 16.0, &source).unwrap().color(),
            color("orange"),
            "root {wide}"
        );
    }
    assert_eq!(
        style("--a:var(--missing,initial);--b:var(--a,blue);color:var(--b)").color(),
        color("blue")
    );
}
