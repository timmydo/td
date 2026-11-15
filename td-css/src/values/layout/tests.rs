#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{syntax, values::Color as Rgba};
use crate::{
    test_tree::{tree, Handle},
    Origin, StyleSet,
};
fn style(input: &str, parent: Option<&ComputedStyle>) -> ComputedStyle {
    let nodes = tree();
    StyleSet::new(&[])
        .unwrap()
        .compute(
            &Handle {
                nodes: &nodes,
                index: 3,
            },
            parent,
            16.0,
            input,
        )
        .unwrap()
}
fn child(input: &str) -> ComputedStyle {
    style(
        input,
        Some(&ComputedStyle::new(&crate::memory().unwrap()).unwrap()),
    )
}
#[test]
fn each_added_longhand_has_a_typed_value() {
    let cases = [
        (LineHeight, "1.5", ComputedValue::Number(1.5)),
        (
            FontVariant,
            "small-caps",
            ComputedValue::Keyword("small-caps"),
        ),
        (BorderTopStyle, "solid", ComputedValue::Keyword("solid")),
        (BorderRightStyle, "dashed", ComputedValue::Keyword("dashed")),
        (
            BorderBottomStyle,
            "double",
            ComputedValue::Keyword("double"),
        ),
        (BorderLeftStyle, "hidden", ComputedValue::Keyword("hidden")),
        (
            BorderTopColor,
            "red",
            ComputedValue::Color(Rgba {
                red: 255,
                green: 0,
                blue: 0,
                alpha: 255,
            }),
        ),
        (
            BorderRightColor,
            "currentcolor",
            ComputedValue::Color(Rgba::BLACK),
        ),
        (BorderBottomColor, "#000", ComputedValue::Color(Rgba::BLACK)),
        (
            BorderLeftColor,
            "transparent",
            ComputedValue::Color(Rgba::TRANSPARENT),
        ),
        (
            VerticalAlign,
            "-20%",
            ComputedValue::Length(Length::Percent(-20.0)),
        ),
        (TextIndent, "-2em", ComputedValue::Length(Length::Px(-32.0))),
        (
            TextTransform,
            "uppercase",
            ComputedValue::Keyword("uppercase"),
        ),
        (Visibility, "hidden", ComputedValue::Keyword("hidden")),
        (OverflowX, "scroll", ComputedValue::Keyword("scroll")),
        (OverflowY, "auto", ComputedValue::Keyword("auto")),
        (Float, "left", ComputedValue::Keyword("left")),
        (Clear, "both", ComputedValue::Keyword("both")),
        (Position, "relative", ComputedValue::Keyword("relative")),
        (
            BoxSizing,
            "border-box",
            ComputedValue::Keyword("border-box"),
        ),
        (MinWidth, "2em", ComputedValue::Length(Length::Px(32.0))),
        (
            MaxWidth,
            "50%",
            ComputedValue::Length(Length::Percent(50.0)),
        ),
        (MinHeight, "1px", ComputedValue::Length(Length::Px(1.0))),
        (MaxHeight, "none", ComputedValue::Keyword("none")),
        (
            LetterSpacing,
            "-.5em",
            ComputedValue::Length(Length::Px(-8.0)),
        ),
        (Opacity, "40%", ComputedValue::Number(0.4)),
        (Top, "auto", ComputedValue::Length(Length::Auto)),
        (Right, "-3px", ComputedValue::Length(Length::Px(-3.0))),
        (Bottom, "25%", ComputedValue::Length(Length::Percent(25.0))),
        (Left, "2em", ComputedValue::Length(Length::Px(32.0))),
        (
            ListStylePosition,
            "inside",
            ComputedValue::Keyword("inside"),
        ),
        (
            BorderCollapse,
            "collapse",
            ComputedValue::Keyword("collapse"),
        ),
        (
            BorderSpacing,
            "1em 3pt",
            ComputedValue::Spacing {
                horizontal: 16.0,
                vertical: 4.0,
            },
        ),
        (TableLayout, "fixed", ComputedValue::Keyword("fixed")),
    ];
    assert_eq!(cases.len() + 25, Property::ALL.len());
    assert_eq!(Property::ALL.len(), 59);
    for (p, value, want) in cases {
        assert_eq!(
            child(&format!("{}:{value}", p.name())).get(p),
            Some(&want),
            "{p:?}"
        );
    }
}
#[test]
fn line_height_inherits_numbers_but_computes_lengths_and_percentages() {
    for (source, want) in [
        ("line-height:1.5", ComputedValue::Number(1.5)),
        ("line-height:150%", ComputedValue::Length(Length::Px(30.0))),
        ("line-height:1.5em", ComputedValue::Length(Length::Px(30.0))),
        (
            "line-height:calc(100% + 2px)",
            ComputedValue::Length(Length::Px(22.0)),
        ),
    ] {
        let parent = child(&format!("font-size:20px;{source}"));
        let child = style("font-size:10px", Some(&parent));
        assert_eq!(parent.get(LineHeight), Some(&want));
        assert_eq!(child.get(LineHeight), Some(&want));
    }
    assert_eq!(
        child("line-height:calc(-1px)").get(LineHeight),
        Some(&ComputedValue::Length(Length::Px(0.0)))
    );
    assert_eq!(
        child("line-height:2;line-height:-1").get(LineHeight),
        Some(&ComputedValue::Number(2.0))
    );
}
#[test]
fn borders_reset_components_compute_current_color_and_zero_invisible_widths() {
    let s = child(
        "color:red;border:thick solid currentcolor;border-left:blue;border-right-style:hidden",
    );
    assert_eq!(
        s.get(BorderTopWidth),
        Some(&ComputedValue::Length(Length::Px(5.0)))
    );
    assert_eq!(
        s.get(BorderLeftWidth),
        Some(&ComputedValue::Length(Length::Px(0.0)))
    );
    assert_eq!(
        s.get(BorderRightWidth),
        Some(&ComputedValue::Length(Length::Px(0.0)))
    );
    assert_eq!(s.get(BorderTopColor), s.get(Color));
    assert_eq!(
        s.get(BorderLeftStyle),
        Some(&ComputedValue::Keyword("none"))
    );
    for text in [
        "border:solid dashed",
        "border:1px 2px",
        "border:red blue",
        "border:solid!important red",
    ] {
        assert_eq!(
            child(&format!("border:7px solid;{text}")).get(BorderTopWidth),
            Some(&ComputedValue::Length(Length::Px(7.0))),
            "{text}"
        );
    }
    let s = child(
        "border:solid;border-style:solid dashed dotted double;border-color:red blue green black",
    );
    assert_eq!(
        s.get(BorderLeftStyle),
        Some(&ComputedValue::Keyword("double"))
    );
    assert_eq!(
        s.get(BorderTopWidth),
        Some(&ComputedValue::Length(Length::Px(3.0)))
    );
    assert_eq!(
        s.get(BorderLeftColor),
        Some(&ComputedValue::Color(Rgba::BLACK))
    );
}
#[test]
fn font_shorthand_resets_optional_parts_and_handles_pending_tokens() {
    let s = child("font:italic small-caps bold 20px/150% 'Example Face',sans-serif");
    assert_eq!(s.font_size(), 20.0);
    assert_eq!(
        s.get(LineHeight),
        Some(&ComputedValue::Length(Length::Px(30.0)))
    );
    assert_eq!(s.get(FontWeight), Some(&ComputedValue::Weight(700)));
    assert_eq!(
        s.get(FontVariant),
        Some(&ComputedValue::Keyword("small-caps"))
    );
    assert_eq!(
        s.get(FontFamily),
        Some(&ComputedValue::FontFamily(crate::test_tree::families(
            vec![
                Family::Named(crate::test_tree::owned_text("Example Face")),
                Family::Generic("sans-serif")
            ]
        )))
    );
    let s = child("font:italic bold 20px/2 serif;font:12px monospace");
    assert_eq!(s.get(FontStyle), Some(&ComputedValue::Keyword("normal")));
    assert_eq!(s.get(LineHeight), Some(&ComputedValue::Keyword("normal")));
    assert_eq!(s.get(FontWeight), Some(&ComputedValue::Weight(400)));
    for value in [
        "italic italic 12px serif",
        "normal normal normal normal 12px serif",
        "12px/ serif",
        "12px",
        "caption",
        "bold 12px initial",
    ] {
        assert_eq!(
            child(&format!("font-size:9px;font:{value}")).font_size(),
            9.0,
            "{value}"
        );
    }
    let s = child("--f:italic 12px/1.2 monospace;font:var(--f);font-weight:900");
    assert_eq!(s.font_size(), 12.0);
    assert_eq!(s.get(FontWeight), Some(&ComputedValue::Weight(900)));
    let s = child("font-size:9px;font:var(--bad);font-style:italic");
    assert_eq!(s.font_size(), 16.0);
    assert_eq!(s.get(FontStyle), Some(&ComputedValue::Keyword("italic")));
}
#[test]
fn overflow_pair_computation_and_display_blockification() {
    for (input, x, y) in [
        ("overflow:hidden", "hidden", "hidden"),
        ("overflow:visible scroll", "auto", "scroll"),
        ("overflow:clip auto", "hidden", "auto"),
        ("overflow:visible clip", "visible", "clip"),
        ("overflow:scroll;overflow-x:visible", "auto", "scroll"),
    ] {
        let s = child(input);
        assert_eq!(s.get(OverflowX), Some(&ComputedValue::Keyword(x)));
        assert_eq!(s.get(OverflowY), Some(&ComputedValue::Keyword(y)));
    }
    for (input, display) in [
        ("float:left", "block"),
        ("float:right;display:inline-flex", "flex"),
        ("float:left;display:inline-table", "table"),
        ("float:left;display:none", "none"),
        ("position:relative;display:inline", "inline"),
    ] {
        assert_eq!(
            child(input).get(Display),
            Some(&ComputedValue::Keyword(display))
        );
    }
    assert_eq!(
        style("", None).get(Display),
        Some(&ComputedValue::Keyword("block"))
    );
}
#[test]
fn list_style_shorthand_sets_type_and_position_and_drops_an_image() {
    let parent = child("list-style-type:square;list-style-position:inside");
    let word = |p| ComputedValue::Keyword(p);
    for (input, kind, position) in [
        ("list-style:none", "none", "outside"),
        ("list-style:inside", "disc", "inside"),
        ("list-style:upper-roman inside", "upper-roman", "inside"),
        ("list-style:inside none", "none", "inside"),
        ("list-style:none url(a.png)", "none", "outside"),
        (
            "list-style:url(a.png) lower-alpha",
            "lower-alpha",
            "outside",
        ),
        ("list-style:lower-latin inside", "lower-latin", "inside"),
        ("list-style:UPPER-LATIN", "upper-latin", "outside"),
        (
            "list-style:linear-gradient(red, blue) square",
            "square",
            "outside",
        ),
        (
            "list-style:inside image-set(\"a.png\" 1x)",
            "disc",
            "inside",
        ),
        (
            "list-style:repeating-conic-gradient(red, blue) none",
            "none",
            "outside",
        ),
        ("list-style:none none", "none", "outside"),
        ("list-style:none circle", "circle", "outside"),
        (
            "list-style:inside;list-style-type:decimal",
            "decimal",
            "inside",
        ),
        (
            "--l:inside decimal;list-style:var(--l)",
            "decimal",
            "inside",
        ),
        // Invalid: the declaration is dropped and the initials stay.
        ("list-style:none none disc", "disc", "outside"),
        ("list-style:inside outside", "disc", "outside"),
        ("list-style:square circle", "disc", "outside"),
        ("list-style:url(a) url(b)", "disc", "outside"),
        ("list-style:bogus", "disc", "outside"),
        ("list-style:inside calc(1px)", "disc", "outside"),
        (
            "list-style:url(a) linear-gradient(red, blue)",
            "disc",
            "outside",
        ),
        ("list-style-position:middle", "disc", "outside"),
    ] {
        let s = child(input);
        assert_eq!(s.get(ListStyleType), Some(&word(kind)), "{input}");
        assert_eq!(s.get(ListStylePosition), Some(&word(position)), "{input}");
    }
    // The longhand takes the shorthand's keywords, none included.
    for (input, kind) in [
        ("list-style-type:none", "none"),
        ("list-style-type:lower-latin", "lower-latin"),
        ("list-style-type:upper-latin", "upper-latin"),
        (
            "list-style-type:decimal-leading-zero",
            "decimal-leading-zero",
        ),
    ] {
        assert_eq!(
            child(input).get(ListStyleType),
            Some(&word(kind)),
            "{input}"
        );
    }
    assert_eq!(
        child("list-style-type:linear-gradient(red, blue)").get(ListStyleType),
        Some(&word("disc"))
    );
    // Both inherit.
    let s = style("", Some(&parent));
    assert_eq!(s.get(ListStyleType), Some(&word("square")));
    assert_eq!(s.get(ListStylePosition), Some(&word("inside")));
}
#[test]
fn extra_grammars_reject_unsupported_values_and_clamp_numeric_ranges() {
    for (p, bad) in [
        (Position, "absolute"),
        (Position, "fixed"),
        (MinWidth, "-1px"),
        (MinHeight, "auto"),
        (LetterSpacing, "10%"),
        (LetterSpacing, "calc(10% - 10%)"),
        (Opacity, "NaN"),
        (FontVariant, "all-small-caps"),
        (VerticalAlign, "auto"),
    ] {
        assert!(
            parse_declaration(
                syntax::parse_declarations(&format!("{}:{bad}", p.name()))
                    .unwrap()
                    .value()
                    .first()
                    .unwrap(),
                &crate::memory().unwrap()
            )
            .unwrap()
            .is_empty(),
            "{p:?}:{bad}"
        );
    }
    assert_eq!(
        child("opacity:2").get(Opacity),
        Some(&ComputedValue::Number(1.0))
    );
    assert_eq!(
        child("opacity:-1").get(Opacity),
        Some(&ComputedValue::Number(0.0))
    );
    let s = child("text-indent:calc(10% - 2px);left:calc(-1em);letter-spacing:calc(1px - 1em)");
    assert_eq!(s.get(Left), Some(&ComputedValue::Length(Length::Px(-16.0))));
    assert_eq!(
        s.get(LetterSpacing),
        Some(&ComputedValue::Length(Length::Px(-15.0)))
    );
}
#[test]
fn state_selectors_join_indexed_cascade_without_exposing_history() {
    let mut nodes = tree();
    nodes[3].states = vec![crate::selectors::State::Hover];
    let css =
        "p{opacity:1} p:hover{opacity:.5} :is(p:focus,p:checked){opacity:.2} p:visited{opacity:0}";
    let set = StyleSet::new(&[(Origin::Author, css)]).unwrap();
    let handle = Handle {
        nodes: &nodes,
        index: 3,
    };
    assert_eq!(
        set.compute(&handle, None, 16.0, "").unwrap().get(Opacity),
        Some(&ComputedValue::Number(0.5))
    );
}
#[test]
fn table_properties_inherit_but_the_layout_and_take_no_percentage() {
    let word = |p| ComputedValue::Keyword(p);
    fn spacing(s: &ComputedStyle) -> (f64, f64) {
        match s.get(BorderSpacing) {
            Some(ComputedValue::Spacing {
                horizontal,
                vertical,
            }) => (*horizontal, *vertical),
            other => panic!("{other:?}"),
        }
    }
    let initial = child("");
    assert_eq!(spacing(&initial), (0.0, 0.0));
    assert_eq!(initial.get(BorderCollapse), Some(&word("separate")));
    assert_eq!(initial.get(TableLayout), Some(&word("auto")));
    for (input, x, y) in [
        ("border-spacing:2px", 2.0, 2.0),
        ("border-spacing:2px 1em", 2.0, 16.0),
        ("border-spacing:0 calc(1px + 1em)", 0.0, 17.0),
        ("border-spacing:1em;font-size:10px", 10.0, 10.0),
        ("border-spacing:0.5in 2rem", 48.0, 32.0),
        // One longhand, as CSS 2.2 has it: no per-axis names.
        (
            "border-spacing:2px;-webkit-border-vertical-spacing:5px",
            2.0,
            2.0,
        ),
        ("--s:3px 4px;border-spacing:var(--s)", 3.0, 4.0),
        // Invalid: dropped, the initials stay.
        ("border-spacing:10%", 0.0, 0.0),
        ("border-spacing:-1px", 0.0, 0.0),
        ("border-spacing:1px 2px 3px", 0.0, 0.0),
        ("border-spacing:auto", 0.0, 0.0),
        ("border-spacing:1px red", 0.0, 0.0),
        ("border-spacing:calc(10% + 1px)", 0.0, 0.0),
    ] {
        let s = child(input);
        assert_eq!(spacing(&s), (x, y), "{input}");
    }
    assert_eq!(
        spacing(&child("border-spacing:calc(1px - 5px)")),
        (0.0, 0.0)
    );
    for bad in ["border-collapse:bogus", "table-layout:none"] {
        let s = child(bad);
        assert_eq!(s.get(BorderCollapse), Some(&word("separate")), "{bad}");
        assert_eq!(s.get(TableLayout), Some(&word("auto")), "{bad}");
    }
    let parent = child("border-spacing:3px 4px;border-collapse:collapse;table-layout:fixed");
    let s = style("", Some(&parent));
    assert_eq!(spacing(&s), (3.0, 4.0));
    assert_eq!(s.get(BorderCollapse), Some(&word("collapse")));
    assert_eq!(s.get(TableLayout), Some(&word("auto")));
    let s = style("border-spacing:inherit;table-layout:inherit", Some(&parent));
    assert_eq!(s.get(TableLayout), Some(&word("fixed")));
    assert_eq!(spacing(&s), (3.0, 4.0));
}
