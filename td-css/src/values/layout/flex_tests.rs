//! The flex and alignment longhands, their shorthands and min sizes' `auto`
//! (DESIGN.md §4).
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use super::super::tests::{child, style};
    use super::super::*;
    /// The twelve flex and alignment longhands with their initial values.
    fn flex_initials() -> [(Property, ComputedValue); 12] {
        let k = ComputedValue::Keyword;
        [
            (FlexDirection, k("row")),
            (FlexWrap, k("nowrap")),
            (FlexGrow, ComputedValue::Number(0.0)),
            (FlexShrink, ComputedValue::Number(1.0)),
            (FlexBasis, ComputedValue::Length(Length::Auto)),
            (JustifyContent, k("normal")),
            (AlignItems, k("normal")),
            (AlignSelf, k("auto")),
            (AlignContent, k("normal")),
            (RowGap, k("normal")),
            (ColumnGap, k("normal")),
            (Order, ComputedValue::Number(0.0)),
        ]
    }
    #[test]
    fn flex_longhands_start_at_their_initials_and_do_not_inherit() {
        let memory = crate::memory().unwrap();
        let initial = child("");
        let new = ComputedStyle::new(&memory).unwrap();
        for (p, want) in flex_initials() {
            assert!(!p.inherited(), "{p:?}");
            assert_eq!(initial.get(p), Some(&want), "{p:?}");
            assert_eq!(new.get(p), Some(&want), "{p:?}");
            assert_eq!(p.initial(&memory).unwrap(), want, "{p:?}");
        }
        let parent = child(
            "flex-direction:column;flex-wrap:wrap;flex-grow:2;flex-shrink:3;flex-basis:10px;\
         justify-content:center;align-items:center;align-self:center;align-content:center;\
         row-gap:1px;column-gap:2px;order:4",
        );
        let s = style("", Some(&parent));
        for (p, want) in flex_initials() {
            assert_ne!(parent.get(p), Some(&want), "{p:?}");
            assert_eq!(s.get(p), Some(&want), "{p:?}");
        }
        let s = style(
            "flex-grow:inherit;order:inherit;align-self:inherit;flex-basis:inherit",
            Some(&parent),
        );
        assert_eq!(s.get(FlexGrow), Some(&ComputedValue::Number(2.0)));
        assert_eq!(s.get(Order), Some(&ComputedValue::Number(4.0)));
        assert_eq!(s.get(AlignSelf), Some(&ComputedValue::Keyword("center")));
        assert_eq!(
            s.get(FlexBasis),
            Some(&ComputedValue::Length(Length::Px(10.0)))
        );
        let s = child("flex-grow:2;flex-grow:initial;order:5;order:unset");
        assert_eq!(s.get(FlexGrow), Some(&ComputedValue::Number(0.0)));
        assert_eq!(s.get(Order), Some(&ComputedValue::Number(0.0)));
    }
    #[test]
    fn flex_longhands_take_their_grammars() {
        use ComputedValue::{Keyword as K, Number as N};
        let px = |n| ComputedValue::Length(Length::Px(n));
        let percent = |n| ComputedValue::Length(Length::Percent(n));
        let max = f64::from(i32::MAX);
        let min = f64::from(i32::MIN);
        for (input, p, want) in [
            (
                "flex-direction:ROW-reverse",
                FlexDirection,
                K("row-reverse"),
            ),
            ("flex-direction:column", FlexDirection, K("column")),
            ("flex-wrap:wrap-reverse", FlexWrap, K("wrap-reverse")),
            ("flex-grow:0.5", FlexGrow, N(0.5)),
            ("flex-grow:1e9", FlexGrow, N(MAX_ABSOLUTE_VALUE)),
            ("flex-shrink:7", FlexShrink, N(7.0)),
            (
                "flex-basis:auto",
                FlexBasis,
                ComputedValue::Length(Length::Auto),
            ),
            ("flex-basis:content", FlexBasis, K("content")),
            ("flex-basis:0", FlexBasis, px(0.0)),
            ("flex-basis:25%", FlexBasis, percent(25.0)),
            (
                "flex-basis:calc(50% - 2px)",
                FlexBasis,
                ComputedValue::Length(Length::Calc {
                    px: -2.0,
                    percent: 50.0,
                    non_negative: true,
                }),
            ),
            ("flex-basis:calc(1px - 2em)", FlexBasis, px(0.0)),
            ("row-gap:normal", RowGap, K("normal")),
            ("row-gap:0", RowGap, px(0.0)),
            ("column-gap:calc(1em + 1px)", ColumnGap, px(17.0)),
            ("order:+12", Order, N(12.0)),
            ("order:99999999999", Order, N(max)),
            ("order:-99999999999", Order, N(min)),
            ("justify-content:Left", JustifyContent, K("left")),
            ("justify-content:stretch", JustifyContent, K("stretch")),
            (
                "justify-content:space-evenly",
                JustifyContent,
                K("space-evenly"),
            ),
            ("align-items:self-end", AlignItems, K("self-end")),
            ("align-items:baseline", AlignItems, K("baseline")),
            ("align-items:first baseline", AlignItems, K("baseline")),
            ("align-self:last baseline", AlignSelf, K("last baseline")),
            ("align-self:normal", AlignSelf, K("normal")),
            ("align-self:center;align-self:auto", AlignSelf, K("auto")),
            (
                "align-content:space-around",
                AlignContent,
                K("space-around"),
            ),
            ("align-content:FIRST  Baseline", AlignContent, K("baseline")),
        ] {
            assert_eq!(child(input).get(p), Some(&want), "{input}");
        }
    }
    #[test]
    fn invalid_flex_values_are_dropped() {
        for (p, keep, bad) in [
            (FlexDirection, "column", "horizontal"),
            (FlexDirection, "column", "row wrap"),
            (FlexWrap, "wrap", "row"),
            (FlexGrow, "2", "-1"),
            (FlexGrow, "2", "1px"),
            (FlexGrow, "2", "auto"),
            (FlexGrow, "2", "10%"),
            (FlexGrow, "2", "1 2"),
            (FlexShrink, "2", "-0.5"),
            (FlexBasis, "1px", "-1px"),
            (FlexBasis, "1px", "none"),
            (FlexBasis, "1px", "1"),
            (FlexBasis, "1px", "auto auto"),
            (JustifyContent, "center", "baseline"),
            (JustifyContent, "center", "self-start"),
            (JustifyContent, "center", "auto"),
            (JustifyContent, "center", "first baseline"),
            (JustifyContent, "center", "last baseline"),
            (AlignItems, "center", "safe left"),
            (AlignContent, "center", "safe left"),
            (JustifyContent, "center", "safe normal"),
            (JustifyContent, "center", "safe space-between"),
            (JustifyContent, "center", "safe safe center"),
            (JustifyContent, "center", "safe center center"),
            (JustifyContent, "center", "center safe"),
            (AlignItems, "center", "auto"),
            (AlignItems, "center", "left"),
            (AlignItems, "center", "space-between"),
            (AlignItems, "center", "safe stretch"),
            (AlignItems, "center", "safe baseline"),
            (AlignItems, "center", "middle baseline"),
            (AlignItems, "center", "last center"),
            (AlignItems, "center", "baseline last"),
            (AlignSelf, "center", "right"),
            (AlignSelf, "center", "safe auto"),
            (AlignContent, "center", "self-start"),
            (AlignContent, "center", "left"),
            (AlignContent, "center", "safe stretch"),
            (AlignContent, "center", "safe self-start"),
            (AlignContent, "center", "first baseline baseline"),
            (RowGap, "1px", "-1px"),
            (RowGap, "1px", "auto"),
            (RowGap, "1px", "1px 2px"),
            (ColumnGap, "1px", "none"),
            (Order, "2", "1.5"),
            (Order, "2", "1e3"),
            (Order, "2", "auto"),
            (MinWidth, "1px", "none"),
            (MinHeight, "1px", "content"),
        ] {
            let name = p.name();
            let kept = child(&format!("{name}:{keep}"));
            assert_ne!(kept.get(p), child("").get(p), "{name}:{keep}");
            let s = child(&format!("{name}:{keep};{name}:{bad}"));
            assert_eq!(s.get(p), kept.get(p), "{name}:{bad}");
        }
    }
    #[test]
    fn overflow_positions_are_accepted_and_dropped() {
        for (input, p, want) in [
            ("justify-content:safe center", JustifyContent, "center"),
            (
                "justify-content:UNSAFE flex-end",
                JustifyContent,
                "flex-end",
            ),
            ("justify-content:safe left", JustifyContent, "left"),
            ("justify-content:unsafe right", JustifyContent, "right"),
            ("align-items:safe self-start", AlignItems, "self-start"),
            ("align-items:unsafe end", AlignItems, "end"),
            ("align-self:safe flex-start", AlignSelf, "flex-start"),
            ("align-content:unsafe start", AlignContent, "start"),
            ("align-content:safe center", AlignContent, "center"),
        ] {
            assert_eq!(
                child(input).get(p),
                Some(&ComputedValue::Keyword(want)),
                "{input}"
            );
        }
    }
    #[test]
    fn flex_shorthand_expands_to_its_three_longhands() {
        use ComputedValue::Number;
        let px = |n| ComputedValue::Length(Length::Px(n));
        let zero = || ComputedValue::Length(Length::Percent(0.0));
        let auto = || ComputedValue::Length(Length::Auto);
        for (input, grow, shrink, basis) in [
            ("none", 0.0, 0.0, auto()),
            ("NONE", 0.0, 0.0, auto()),
            ("auto", 1.0, 1.0, auto()),
            ("initial", 0.0, 1.0, auto()),
            ("2", 2.0, 1.0, zero()),
            ("0", 0.0, 1.0, zero()),
            ("10px", 1.0, 1.0, px(10.0)),
            (
                "30%",
                1.0,
                1.0,
                ComputedValue::Length(Length::Percent(30.0)),
            ),
            ("content", 1.0, 1.0, ComputedValue::Keyword("content")),
            ("calc(1em + 2px)", 1.0, 1.0, px(18.0)),
            ("2 3", 2.0, 3.0, zero()),
            ("2 3 4px", 2.0, 3.0, px(4.0)),
            ("2 4px", 2.0, 1.0, px(4.0)),
            ("4px 2", 2.0, 1.0, px(4.0)),
            ("4px 2 3", 2.0, 3.0, px(4.0)),
            ("2 3 auto", 2.0, 3.0, auto()),
            ("auto 2 3", 2.0, 3.0, auto()),
            ("1 0 auto", 1.0, 0.0, auto()),
            // A unitless zero after two factors is the basis.
            ("0 0 0", 0.0, 0.0, px(0.0)),
            ("1 1 0", 1.0, 1.0, px(0.0)),
            ("0 0", 0.0, 0.0, zero()),
            ("1e9", MAX_ABSOLUTE_VALUE, 1.0, zero()),
        ] {
            let s = child(&format!(
                "flex-grow:5;flex-shrink:5;flex-basis:5px;flex:{input}"
            ));
            assert_eq!(s.get(FlexGrow), Some(&Number(grow)), "{input}");
            assert_eq!(s.get(FlexShrink), Some(&Number(shrink)), "{input}");
            assert_eq!(s.get(FlexBasis), Some(&basis), "{input}");
        }
        for bad in [
            "",
            "-1",
            "1 -1",
            "1 2 3",
            "1 2 3px 4",
            "1px 2px",
            "0 1px 0",
            "1 2px 3",
            "auto auto",
            "none 1",
            "1 none",
            "1 inherit",
            "normal",
            "1 2 10% 0",
            "red",
        ] {
            let s = child(&format!("flex:2 3 4px;flex:{bad}"));
            assert_eq!(s.get(FlexGrow), Some(&Number(2.0)), "{bad}");
            assert_eq!(s.get(FlexShrink), Some(&Number(3.0)), "{bad}");
            assert_eq!(s.get(FlexBasis), Some(&px(4.0)), "{bad}");
        }
        let parent = child("flex:2 3 4px");
        let s = style("flex:inherit", Some(&parent));
        assert_eq!(s.get(FlexShrink), Some(&Number(3.0)));
        let s = child("--f:2 3 4px;flex:var(--f)");
        assert_eq!(s.get(FlexGrow), Some(&Number(2.0)));
        assert_eq!(s.get(FlexShrink), Some(&Number(3.0)));
        assert_eq!(s.get(FlexBasis), Some(&px(4.0)));
        // A later longhand overrides its part.
        let s = child("flex:none;flex-shrink:2");
        assert_eq!(s.get(FlexShrink), Some(&Number(2.0)));
        assert_eq!(s.get(FlexGrow), Some(&Number(0.0)));
    }
    #[test]
    fn flex_flow_and_gap_shorthands_expand() {
        let px = |n| ComputedValue::Length(Length::Px(n));
        let percent = |n| ComputedValue::Length(Length::Percent(n));
        let k = ComputedValue::Keyword;
        for (input, direction, wrap) in [
            ("column", "column", "nowrap"),
            ("wrap", "row", "wrap"),
            ("column wrap", "column", "wrap"),
            ("wrap-reverse row-reverse", "row-reverse", "wrap-reverse"),
            ("initial", "row", "nowrap"),
        ] {
            let s = child(&format!(
                "flex-direction:column-reverse;flex-wrap:wrap-reverse;flex-flow:{input}"
            ));
            assert_eq!(s.get(FlexDirection), Some(&k(direction)), "{input}");
            assert_eq!(s.get(FlexWrap), Some(&k(wrap)), "{input}");
        }
        for bad in [
            "row column",
            "wrap nowrap",
            "row wrap row",
            "",
            "1px",
            "row inherit",
        ] {
            let s = child(&format!("flex-flow:column wrap;flex-flow:{bad}"));
            assert_eq!(s.get(FlexDirection), Some(&k("column")), "{bad}");
            assert_eq!(s.get(FlexWrap), Some(&k("wrap")), "{bad}");
        }
        for (input, row, column) in [
            ("4px", px(4.0), px(4.0)),
            ("4px 1em", px(4.0), px(16.0)),
            ("normal 10%", k("normal"), percent(10.0)),
            ("5%", percent(5.0), percent(5.0)),
            ("0 normal", px(0.0), k("normal")),
            ("initial", k("normal"), k("normal")),
        ] {
            let s = child(&format!("row-gap:9px;column-gap:9px;gap:{input}"));
            assert_eq!(s.get(RowGap), Some(&row), "{input}");
            assert_eq!(s.get(ColumnGap), Some(&column), "{input}");
        }
        for bad in ["-1px", "1px -1px", "1px 2px 3px", "auto", "", "1px inherit"] {
            let s = child(&format!("gap:3px;gap:{bad}"));
            assert_eq!(s.get(RowGap), Some(&px(3.0)), "{bad}");
            assert_eq!(s.get(ColumnGap), Some(&px(3.0)), "{bad}");
        }
    }
    #[test]
    fn min_sizes_take_auto_as_their_initial_value() {
        let auto = ComputedValue::Length(Length::Auto);
        let new = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        for p in [MinWidth, MinHeight] {
            assert_eq!(child("").get(p), Some(&auto), "{p:?}");
            assert_eq!(new.get(p), Some(&auto), "{p:?}");
            let name = p.name();
            for input in [
                format!("{name}:10px;{name}:auto"),
                format!("{name}:10px;{name}:AUTO"),
                format!("{name}:10px;{name}:initial"),
                format!("{name}:10px;{name}:unset"),
            ] {
                assert_eq!(child(&input).get(p), Some(&auto), "{input}");
            }
            let parent = child(&format!("{name}:auto"));
            assert_eq!(parent.get(p), Some(&auto));
            let s = style(&format!("{name}:7px"), Some(&parent));
            assert_eq!(s.get(p), Some(&ComputedValue::Length(Length::Px(7.0))));
            let s = style(&format!("{name}:inherit"), Some(&parent));
            assert_eq!(s.get(p), Some(&auto));
            let parent = child(&format!("{name}:7px"));
            assert_eq!(style("", Some(&parent)).get(p), Some(&auto), "{name}");
        }
    }
    /// A flex container's children are blockified as floats are, and do
    /// not float; its grandchildren, a block's children and the root are
    /// computed as before.
    #[test]
    fn flex_items_are_blockified_and_do_not_float() {
        let display = |s: &ComputedStyle| match s.get(Display) {
            Some(ComputedValue::Keyword(k)) => *k,
            other => panic!("{other:?}"),
        };
        let float = |s: &ComputedStyle| match s.get(Float) {
            Some(ComputedValue::Keyword(k)) => *k,
            other => panic!("{other:?}"),
        };
        for container in ["display:flex", "display:inline-flex"] {
            let parent = child(container);
            for (own, want) in [
                ("inline", "block"),
                ("inline-block", "block"),
                ("inline-table", "table"),
                ("inline-flex", "flex"),
                ("table-row-group", "block"),
                ("table-header-group", "block"),
                ("table-footer-group", "block"),
                ("table-row", "block"),
                ("table-cell", "block"),
                ("table-column-group", "block"),
                ("table-column", "block"),
                ("table-caption", "block"),
                ("block", "block"),
                ("list-item", "list-item"),
                ("table", "table"),
                ("flex", "flex"),
                ("none", "none"),
            ] {
                let s = style(&format!("display:{own}"), Some(&parent));
                assert_eq!(display(&s), want, "{container} {own}");
                assert_eq!(float(&s), "none", "{container} {own}");
                let s = style(&format!("display:{own};float:right"), Some(&parent));
                assert_eq!(display(&s), want, "{container} {own} floated");
                assert_eq!(float(&s), "none", "{container} {own} floated");
                // Its own children are not flex items.
                let grandchild = style("display:inline;float:none", Some(&s));
                let blockified = matches!(want, "flex");
                assert_eq!(
                    display(&grandchild),
                    if blockified { "block" } else { "inline" },
                    "{container} {own}"
                );
            }
        }
        // A block's children, and a float in one, as before.
        let parent = child("display:block");
        let s = style("display:inline", Some(&parent));
        assert_eq!(display(&s), "inline");
        let s = style("display:inline;float:left", Some(&parent));
        assert_eq!((display(&s), float(&s)), ("block", "left"));
        // The root: blockified as a root, floating as it is said to.
        let s = style("display:inline-flex;float:left", None);
        assert_eq!((display(&s), float(&s)), ("flex", "left"));
        let s = style("display:inline", None);
        assert_eq!((display(&s), float(&s)), ("block", "none"));
    }
}
