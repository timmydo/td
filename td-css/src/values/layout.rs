use super::*;
use Property::*;
pub(super) const BORDER_STYLES: &[Property] = &[
    BorderTopStyle,
    BorderRightStyle,
    BorderBottomStyle,
    BorderLeftStyle,
];
pub(super) const BORDER_COLORS: &[Property] = &[
    BorderTopColor,
    BorderRightColor,
    BorderBottomColor,
    BorderLeftColor,
];
const BORDER_ALL: &[Property] = &[
    BorderTopWidth,
    BorderRightWidth,
    BorderBottomWidth,
    BorderLeftWidth,
    BorderTopStyle,
    BorderRightStyle,
    BorderBottomStyle,
    BorderLeftStyle,
    BorderTopColor,
    BorderRightColor,
    BorderBottomColor,
    BorderLeftColor,
];
const FONT: &[Property] = &[
    FontStyle,
    FontVariant,
    FontWeight,
    FontSize,
    LineHeight,
    FontFamily,
];
const LIST_STYLE: &[Property] = &[ListStyleType, ListStylePosition];
/// `list-style-type`'s keywords, for the longhand and the shorthand alike;
/// lower-latin and upper-latin are Counter Styles 3's aliases of the
/// alphas, kept as written and mapped by layout.
const LIST_STYLE_TYPES: &[&str] = &[
    "none",
    "disc",
    "circle",
    "square",
    "decimal",
    "decimal-leading-zero",
    "lower-roman",
    "upper-roman",
    "lower-alpha",
    "upper-alpha",
    "lower-latin",
    "upper-latin",
];
/// CSS Images 4's `<image>` functions; `list-style` drops its image, so
/// only the name is checked.
const IMAGE_FUNCTIONS: &[&str] = &[
    "url",
    "src",
    "image",
    "image-set",
    "cross-fade",
    "element",
    "linear-gradient",
    "radial-gradient",
    "conic-gradient",
    "repeating-linear-gradient",
    "repeating-radial-gradient",
    "repeating-conic-gradient",
];
const LIST_STYLE_POSITIONS: &[&str] = &["outside", "inside"];
const STYLES: &[&str] = &[
    "none", "hidden", "dotted", "dashed", "solid", "double", "groove", "ridge", "inset", "outset",
];
const FLEX_DIRECTIONS: &[&str] = &["row", "row-reverse", "column", "column-reverse"];
const FLEX_WRAPS: &[&str] = &["nowrap", "wrap", "wrap-reverse"];
/// The single-keyword values of each alignment longhand, and those of
/// them a `safe` or `unsafe` may precede.
const JUSTIFY_CONTENT: &[&str] = &[
    "normal",
    "space-between",
    "space-around",
    "space-evenly",
    "stretch",
    "center",
    "start",
    "end",
    "flex-start",
    "flex-end",
    "left",
    "right",
];
const JUSTIFY_POSITIONS: &[&str] = &[
    "center",
    "start",
    "end",
    "flex-start",
    "flex-end",
    "left",
    "right",
];
const ALIGN_CONTENT: &[&str] = &[
    "normal",
    "space-between",
    "space-around",
    "space-evenly",
    "stretch",
    "center",
    "start",
    "end",
    "flex-start",
    "flex-end",
    "baseline",
];
const CONTENT_POSITIONS: &[&str] = &["center", "start", "end", "flex-start", "flex-end"];
const ALIGN_ITEMS: &[&str] = &[
    "normal",
    "stretch",
    "baseline",
    "center",
    "start",
    "end",
    "self-start",
    "self-end",
    "flex-start",
    "flex-end",
];
const ALIGN_SELF: &[&str] = &[
    "auto",
    "normal",
    "stretch",
    "baseline",
    "center",
    "start",
    "end",
    "self-start",
    "self-end",
    "flex-start",
    "flex-end",
];
const SELF_POSITIONS: &[&str] = &[
    "center",
    "start",
    "end",
    "self-start",
    "self-end",
    "flex-start",
    "flex-end",
];
const OVERFLOW: &[&str] = &["visible", "hidden", "clip", "scroll", "auto"];
pub(super) fn targets(name: &str) -> Option<&'static [Property]> {
    match name {
        "border" => Some(BORDER_ALL),
        "border-top" => Some(&[BorderTopWidth, BorderTopStyle, BorderTopColor]),
        "border-right" => Some(&[BorderRightWidth, BorderRightStyle, BorderRightColor]),
        "border-bottom" => Some(&[BorderBottomWidth, BorderBottomStyle, BorderBottomColor]),
        "border-left" => Some(&[BorderLeftWidth, BorderLeftStyle, BorderLeftColor]),
        "font" => Some(FONT),
        "overflow" => Some(&[OverflowX, OverflowY]),
        "background" => Some(&[BackgroundColor]),
        "list-style" => Some(LIST_STYLE),
        "flex" => Some(&[FlexGrow, FlexShrink, FlexBasis]),
        "flex-flow" => Some(&[FlexDirection, FlexWrap]),
        "gap" => Some(&[RowGap, ColumnGap]),
        _ => None,
    }
}
pub(super) fn extra(p: Property) -> bool {
    matches!(
        p,
        LineHeight
            | FontVariant
            | BorderTopStyle
            | BorderRightStyle
            | BorderBottomStyle
            | BorderLeftStyle
            | VerticalAlign
            | TextIndent
            | TextTransform
            | Visibility
            | OverflowX
            | OverflowY
            | Float
            | Clear
            | Position
            | BoxSizing
            | MinWidth
            | MaxWidth
            | MinHeight
            | MaxHeight
            | LetterSpacing
            | Opacity
            | Top
            | Right
            | Bottom
            | Left
            | ListStyleType
            | ListStylePosition
            | BorderCollapse
            | BorderSpacing
            | TableLayout
            | FlexDirection
            | FlexWrap
            | FlexGrow
            | FlexShrink
            | FlexBasis
            | JustifyContent
            | AlignItems
            | AlignSelf
            | AlignContent
            | RowGap
            | ColumnGap
            | Order
    )
}
pub(super) fn parse(
    p: Property,
    v: &[Component],
    memory: &Budget,
) -> Result<Option<Specified>, Error> {
    let result = match p {
        BorderTopStyle | BorderRightStyle | BorderBottomStyle | BorderLeftStyle => {
            keyword(v, STYLES)
        }
        FontVariant => keyword(v, &["normal", "small-caps"]),
        TextTransform => keyword(v, &["none", "capitalize", "uppercase", "lowercase"]),
        Visibility => keyword(v, &["visible", "hidden", "collapse"]),
        OverflowX | OverflowY => keyword(v, OVERFLOW),
        Float => keyword(v, &["none", "left", "right"]),
        Clear => keyword(v, &["none", "left", "right", "both"]),
        Position => keyword(v, &["static", "relative"]),
        BoxSizing => keyword(v, &["content-box", "border-box"]),
        ListStyleType => keyword(v, LIST_STYLE_TYPES),
        ListStylePosition => keyword(v, LIST_STYLE_POSITIONS),
        BorderCollapse => keyword(v, &["separate", "collapse"]),
        TableLayout => keyword(v, &["auto", "fixed"]),
        BorderSpacing => border_spacing(v, memory)?,
        FlexDirection => keyword(v, FLEX_DIRECTIONS),
        FlexWrap => keyword(v, FLEX_WRAPS),
        FlexGrow | FlexShrink => match v {
            [Component::Token(Token::Number(n))] => number(&n.representation)
                .filter(|n| *n >= 0.0)
                .map(|n| Specified::Ready(ComputedValue::Number(n.min(MAX_ABSOLUTE_VALUE)))),
            _ => None,
        },
        // An integer, clamped to the range browsers keep it in.
        Order => match v {
            [Component::Token(Token::Number(n))] if n.integer => number(&n.representation)
                .map(|n| n.clamp(f64::from(i32::MIN), f64::from(i32::MAX)))
                .map(|n| Specified::Ready(ComputedValue::Number(n))),
            _ => None,
        },
        FlexBasis => keyword(v, &["content"]).or(parse_length(p, v)?),
        JustifyContent | AlignItems | AlignSelf | AlignContent => alignment(p, v),
        RowGap | ColumnGap => keyword(v, &["normal"]).or(parse_length(p, v)?),
        LineHeight => {
            if let Some(v) = keyword(v, &["normal"]) {
                Some(v)
            } else if let [Component::Token(Token::Number(n))] = v {
                number(&n.representation)
                    .filter(|n| *n >= 0.0)
                    .map(|n| Specified::Ready(ComputedValue::Number(n.min(MAX_ABSOLUTE_VALUE))))
            } else {
                parse_length(p, v)?
            }
        }
        Opacity => {
            let n = match v {
                [Component::Token(Token::Number(n))] => number(&n.representation),
                [Component::Token(Token::Percentage(n))] => {
                    number(&n.representation).map(|n| n / 100.0)
                }
                _ => None,
            };
            n.map(|n| Specified::Ready(ComputedValue::Number(n.clamp(0.0, 1.0))))
        }
        VerticalAlign => keyword(
            v,
            &[
                "baseline",
                "sub",
                "super",
                "top",
                "text-top",
                "middle",
                "bottom",
                "text-bottom",
            ],
        )
        .or(parse_length(p, v)?),
        LetterSpacing => keyword(v, &["normal"]).or(parse_length(p, v)?),
        MaxWidth | MaxHeight => keyword(v, &["none"]).or(parse_length(p, v)?),
        _ => parse_length(p, v)?,
    };
    Ok(result)
}
pub(super) fn shorthand(
    name: &str,
    v: &[Component],
    important: bool,
    memory: &Budget,
) -> Result<BudgetVec<ValueDeclaration>, Error> {
    let mut output = BudgetVec::new(memory)?;
    let Some(targets) = targets(name) else {
        return Ok(output);
    };
    if let Some(global) = global(v) {
        for property in targets {
            output.try_push(ValueDeclaration {
                property: *property,
                value: global.try_clone()?,
                important,
            })?;
        }
        return Ok(output);
    }
    if name == "background" {
        if let Some(value) = color::background(v)? {
            output.try_push(ValueDeclaration {
                property: BackgroundColor,
                value,
                important,
            })?;
        }
        return Ok(output);
    }
    let parsed = match name {
        "font" => font(v, memory)?,
        "overflow" => overflow(v, memory)?,
        "list-style" => list_style(v, memory)?,
        "flex" => flex(v, memory)?,
        "flex-flow" => flex_flow(v, memory)?,
        "gap" => gap(v, memory)?,
        _ => border(v, targets, memory)?,
    };
    if let Some(parsed) = parsed {
        for (property, value) in parsed {
            output.try_push(ValueDeclaration {
                property,
                value,
                important,
            })?;
        }
    }
    Ok(output)
}
fn overflow(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let mut values = v.iter().filter(|v| !v.whitespace());
    let Some(a) = values.next() else {
        return Ok(None);
    };
    let b = values.next().unwrap_or(a);
    if values.next().is_some() {
        return Ok(None);
    }
    let Some(a) = keyword(std::slice::from_ref(a), OVERFLOW) else {
        return Ok(None);
    };
    let Some(b) = keyword(std::slice::from_ref(b), OVERFLOW) else {
        return Ok(None);
    };
    let mut output = td_alloc::BudgetVec::with_capacity(memory, 2)?;
    output.try_push((OverflowX, a))?;
    output.try_push((OverflowY, b))?;
    Ok(Some(output))
}

/// `list-style`: a position, an image and a type, each at most once, in
/// any order. No image is computed, so `none`, a URL or another `<image>`
/// function in its slot is accepted and dropped; a lone `none` sets the type, two set both.
fn list_style(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let mut position = None;
    let mut kind = None;
    let mut image = false;
    let mut nones = 0usize;
    for c in v.iter().filter(|c| !c.whitespace()) {
        let one = std::slice::from_ref(c);
        if c.ident().is_some_and(|n| n.eq_ignore_ascii_case("none")) {
            nones += 1;
        } else if let Some(value) = keyword(one, LIST_STYLE_POSITIONS) {
            if Option::replace(&mut position, value).is_some() {
                return Ok(None);
            }
        } else if let Some(value) = keyword(one, LIST_STYLE_TYPES) {
            if Option::replace(&mut kind, value).is_some() {
                return Ok(None);
            }
        } else if matches!(c, Component::Token(Token::Url(_)))
            || matches!(c, Component::Function { name, .. }
                if IMAGE_FUNCTIONS.iter().any(|f| name.eq_ignore_ascii_case(f)))
        {
            if std::mem::replace(&mut image, true) {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
    // Each none fills an empty image or type slot, the type first.
    let free = usize::from(kind.is_none()) + usize::from(!image);
    if nones > free || (position.is_none() && kind.is_none() && !image && nones == 0) {
        return Ok(None);
    }
    if nones > 0 && kind.is_none() {
        kind = Some(Specified::Ready(ComputedValue::Keyword("none")));
    }
    let mut output = td_alloc::BudgetVec::with_capacity(memory, LIST_STYLE.len())?;
    output.try_push((ListStyleType, kind.unwrap_or(Specified::Initial)))?;
    output.try_push((ListStylePosition, position.unwrap_or(Specified::Initial)))?;
    Ok(Some(output))
}

/// `flex` (Flexbox 1 §7.1): `none` (0 0 auto), `auto` (1 1 auto), or
/// `<grow> <shrink>? || <basis>`, an omitted factor 1 and an omitted
/// basis 0%. A unitless zero is a factor unless two factors precede it.
fn flex(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let factor = |n: f64| Specified::Ready(ComputedValue::Number(n));
    let mut grow = None;
    let mut shrink = None;
    let mut basis = None;
    // Whether the last component was the grow factor, which a shrink
    // factor must follow directly.
    let mut after_grow = false;
    // `auto` needs no case: a lone basis takes factors of 1.
    if single_ident(v).is_some_and(|name| name.eq_ignore_ascii_case("none")) {
        grow = Some(0.0);
        shrink = Some(0.0);
        basis = Some(Specified::Length(0.0, Unit::Auto));
    } else {
        for c in v.iter().filter(|c| !c.whitespace()) {
            let one = std::slice::from_ref(c);
            if let Component::Token(Token::Number(n)) = c {
                let Some(n) = number(&n.representation).filter(|n| *n >= 0.0) else {
                    return Ok(None);
                };
                let n = n.min(MAX_ABSOLUTE_VALUE);
                if grow.is_none() {
                    grow = Some(n);
                    after_grow = true;
                    continue;
                }
                if after_grow && shrink.is_none() {
                    shrink = Some(n);
                } else if n == 0.0 && shrink.is_some() && basis.is_none() {
                    basis = Some(Specified::Length(0.0, Unit::Px));
                } else {
                    return Ok(None);
                }
            } else {
                let Some(value) = parse(FlexBasis, one, memory)? else {
                    return Ok(None);
                };
                if Option::replace(&mut basis, value).is_some() {
                    return Ok(None);
                }
            }
            after_grow = false;
        }
        if grow.is_none() && basis.is_none() {
            return Ok(None);
        }
    }
    let mut output = td_alloc::BudgetVec::with_capacity(memory, 3)?;
    output.try_push((FlexGrow, factor(grow.unwrap_or(1.0))))?;
    output.try_push((FlexShrink, factor(shrink.unwrap_or(1.0))))?;
    output.try_push((
        FlexBasis,
        basis.unwrap_or(Specified::Length(0.0, Unit::Percent)),
    ))?;
    Ok(Some(output))
}

/// `flex-flow`: a direction and a wrap, each at most once, in either
/// order; the one omitted is reset to its initial value.
fn flex_flow(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let mut direction = None;
    let mut wrap = None;
    for c in v.iter().filter(|c| !c.whitespace()) {
        let one = std::slice::from_ref(c);
        let replaced = if let Some(value) = keyword(one, FLEX_DIRECTIONS) {
            Option::replace(&mut direction, value)
        } else if let Some(value) = keyword(one, FLEX_WRAPS) {
            Option::replace(&mut wrap, value)
        } else {
            return Ok(None);
        };
        if replaced.is_some() {
            return Ok(None);
        }
    }
    if direction.is_none() && wrap.is_none() {
        return Ok(None);
    }
    let mut output = td_alloc::BudgetVec::with_capacity(memory, 2)?;
    output.try_push((FlexDirection, direction.unwrap_or(Specified::Initial)))?;
    output.try_push((FlexWrap, wrap.unwrap_or(Specified::Initial)))?;
    Ok(Some(output))
}

/// `gap`: the row gap, then the column gap, which takes the row's when
/// omitted.
fn gap(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let mut values = v.iter().filter(|c| !c.whitespace());
    let Some(first) = values.next() else {
        return Ok(None);
    };
    let second = values.next();
    if values.next().is_some() {
        return Ok(None);
    }
    let Some(row) = parse(RowGap, std::slice::from_ref(first), memory)? else {
        return Ok(None);
    };
    let column = match second {
        Some(c) => match parse(ColumnGap, std::slice::from_ref(c), memory)? {
            Some(column) => column,
            None => return Ok(None),
        },
        None => row.try_clone()?,
    };
    let mut output = td_alloc::BudgetVec::with_capacity(memory, 2)?;
    output.try_push((RowGap, row))?;
    output.try_push((ColumnGap, column))?;
    Ok(Some(output))
}

/// Box Alignment 3's values of the four alignment longhands flex
/// layout reads. `first baseline` computes to `baseline`; a `safe` or
/// `unsafe` overflow position is accepted before a positional value and
/// dropped, td-layout not telling them apart.
fn alignment(p: Property, v: &[Component]) -> Option<Specified> {
    let (words, positions, baselines) = match p {
        JustifyContent => (JUSTIFY_CONTENT, JUSTIFY_POSITIONS, false),
        AlignContent => (ALIGN_CONTENT, CONTENT_POSITIONS, true),
        AlignItems => (ALIGN_ITEMS, SELF_POSITIONS, true),
        _ => (ALIGN_SELF, SELF_POSITIONS, true),
    };
    let mut parts = v.iter().filter(|c| !c.whitespace());
    let first = parts.next()?;
    let Some(second) = parts.next() else {
        return keyword(std::slice::from_ref(first), words);
    };
    if parts.next().is_some() {
        return None;
    }
    let second = std::slice::from_ref(second);
    match syntax::keyword(first.ident()?, &["safe", "unsafe", "first", "last"])? {
        "safe" | "unsafe" => keyword(second, positions),
        "first" if baselines => keyword(second, &["baseline"]),
        "last" if baselines => keyword(second, &["baseline"])
            .map(|_| Specified::Ready(ComputedValue::Keyword("last baseline"))),
        _ => None,
    }
}

/// `border-spacing` (CSS 2.2 §17.6.1), one longhand: one length for both
/// axes, or the horizontal then the vertical; nonnegative, no percentage.
fn border_spacing(v: &[Component], memory: &Budget) -> Result<Option<Specified>, Error> {
    let mut values = v.iter().filter(|c| !c.whitespace());
    let axis = |c: Option<&Component>| -> Result<Option<Axis>, Error> {
        let Some(c) = c else {
            return Ok(None);
        };
        Ok(
            match parse_length(BorderSpacing, std::slice::from_ref(c))? {
                Some(Specified::Length(n, unit)) if !matches!(unit, Unit::Auto) => {
                    Some(Axis::Length(n, unit))
                }
                Some(Specified::Calculated(value)) => Some(Axis::Calculated(value)),
                _ => None,
            },
        )
    };
    let Some(x) = axis(values.next())? else {
        return Ok(None);
    };
    let y = match values.next() {
        Some(c) => match axis(Some(c))? {
            Some(y) => y,
            None => return Ok(None),
        },
        None => x,
    };
    if values.next().is_some() {
        return Ok(None);
    }
    Ok(Some(Specified::Spacing(BudgetShared::new(memory, (x, y))?)))
}

fn border(
    v: &[Component],
    targets: &[Property],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let mut width = None;
    let mut style = None;
    let mut color = None;
    let mut count = 0;
    for c in v.iter().filter(|c| !c.whitespace()) {
        count += 1;
        if count > 3 {
            return Ok(None);
        }
        let one = std::slice::from_ref(c);
        if let Some(v) = keyword(one, STYLES) {
            if Option::replace(&mut style, v).is_some() {
                return Ok(None);
            }
        } else if let Some(v) = parse_color(one)? {
            if Option::replace(&mut color, v).is_some() {
                return Ok(None);
            }
        } else if let Some(v) = parse_length(BorderTopWidth, one)? {
            if Option::replace(&mut width, v).is_some() {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
    if count == 0 {
        return Ok(None);
    }
    let mut output = td_alloc::BudgetVec::with_capacity(memory, targets.len())?;
    for property in targets {
        let value = if property.border() {
            &width
        } else if property.border_color() {
            &color
        } else {
            &style
        };
        output.try_push((
            *property,
            match value {
                Some(value) => value.try_clone()?,
                None => Specified::Initial,
            },
        ))?;
    }
    Ok(Some(output))
}
fn font(
    v: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetVec<(Property, Specified)>>, Error> {
    let mut tokens = v
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.whitespace())
        .peekable();
    let mut normals = 0;
    let mut prefix = td_alloc::BudgetMap::with_capacity(memory, FONT.len())?;
    let size = loop {
        let Some((_, c)) = tokens.next() else {
            return Ok(None);
        };
        let one = std::slice::from_ref(c);
        if let Some(size) = parse_length(FontSize, one)? {
            break size;
        }
        if normals + prefix.len() == 3 {
            return Ok(None);
        }
        if c.ident().is_some_and(|n| n.eq_ignore_ascii_case("normal")) {
            normals += 1;
        } else {
            let found = if let Some(v) = keyword(one, &["italic", "oblique"]) {
                Some((FontStyle, v))
            } else if let Some(v) = keyword(one, &["small-caps"]) {
                Some((FontVariant, v))
            } else {
                weight(one).map(|v| (FontWeight, v))
            };
            let Some((p, value)) = found else {
                return Ok(None);
            };
            if prefix.try_insert(p, value)?.is_some() {
                return Ok(None);
            }
        }
    };
    prefix.try_insert(FontSize, size)?;
    if tokens.peek().is_some_and(|(_, c)| c.delim('/')) {
        let _ = tokens.next();
        let Some((_, c)) = tokens.next() else {
            return Ok(None);
        };
        let Some(height) = parse(LineHeight, std::slice::from_ref(c), memory)? else {
            return Ok(None);
        };
        prefix.try_insert(LineHeight, height)?;
    }
    let Some((start, _)) = tokens.next() else {
        return Ok(None);
    };
    let Some(family) = parse_families(v.get(start..).unwrap_or_default(), memory)? else {
        return Ok(None);
    };
    prefix.try_insert(
        FontFamily,
        Specified::Ready(ComputedValue::FontFamily(family)),
    )?;
    let mut output = td_alloc::BudgetVec::with_capacity(memory, FONT.len())?;
    for property in FONT {
        output.try_push((
            *property,
            prefix.remove(property).unwrap_or(Specified::Initial),
        ))?;
    }
    Ok(Some(output))
}

fn word(style: &ComputedStyle, p: Property) -> Option<&'static str> {
    match style.get(p) {
        Some(ComputedValue::Keyword(v)) => Some(*v),
        _ => None,
    }
}
/// What computing a style ends with: border widths of invisible styles,
/// overflow pairs, and blockification of the root, a float and a flex
/// item, a child of a `flex` or `inline-flex` box (`parent` its computed
/// style), whose `float` computes to `none` (Flexbox 1 §4).
pub(super) fn finish(
    style: &mut ComputedStyle,
    parent: Option<&ComputedStyle>,
) -> Result<(), Error> {
    for (width, kind) in BORDERS.iter().zip(BORDER_STYLES) {
        if matches!(word(style, *kind), Some("none" | "hidden")) {
            style.set(*width, ComputedValue::Length(Length::Px(0.0)))?;
        }
    }
    let x = word(style, OverflowX);
    let y = word(style, OverflowY);
    for (p, own, other) in [(OverflowX, x, y), (OverflowY, y, x)] {
        if !matches!(other, Some("visible" | "clip")) {
            let next = match own {
                Some("visible") => Some("auto"),
                Some("clip") => Some("hidden"),
                _ => None,
            };
            if let Some(next) = next {
                style.set(p, ComputedValue::Keyword(next))?;
            }
        }
    }
    let floated = matches!(word(style, Float), Some("left" | "right"));
    let item = parent.is_some_and(|p| matches!(word(p, Display), Some("flex" | "inline-flex")));
    if item && floated {
        style.set(Float, ComputedValue::Keyword("none"))?;
    }
    if parent.is_none() || floated || item {
        let next = match word(style, Display) {
            Some("inline-table") => Some("table"),
            Some("inline-flex") => Some("flex"),
            Some(
                "inline" | "inline-block" | "table-row-group" | "table-header-group"
                | "table-footer-group" | "table-row" | "table-cell" | "table-column-group"
                | "table-column" | "table-caption",
            ) => Some("block"),
            _ => None,
        };
        if let Some(next) = next {
            style.set(Display, ComputedValue::Keyword(next))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod flex_tests;
#[cfg(test)]
mod tests;
