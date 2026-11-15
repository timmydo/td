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
pub(super) fn finish(style: &mut ComputedStyle, root: bool) -> Result<(), Error> {
    for (width, kind) in BORDERS.iter().zip(BORDER_STYLES) {
        if matches!(word(style, *kind), Some("none" | "hidden")) {
            style
                .values
                .try_insert(*width, ComputedValue::Length(Length::Px(0.0)))?;
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
                style.values.try_insert(p, ComputedValue::Keyword(next))?;
            }
        }
    }
    if root || matches!(word(style, Float), Some("left" | "right")) {
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
            style
                .values
                .try_insert(Display, ComputedValue::Keyword(next))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
