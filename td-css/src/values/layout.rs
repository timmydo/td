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
    )
}
pub(super) fn parse(p: Property, v: &[Component]) -> Result<Option<Specified>, Error> {
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
) -> Result<Vec<ValueDeclaration>, Error> {
    let Some(targets) = targets(name) else {
        return Ok(Vec::new());
    };
    let values = if let Some(global) = global(v) {
        targets.iter().map(|p| (*p, global.clone())).collect()
    } else {
        let parsed = match name {
            "font" => font(v)?,
            "overflow" => overflow(v),
            _ => border(v, targets)?,
        };
        let Some(parsed) = parsed else {
            return Ok(Vec::new());
        };
        parsed
    };
    Ok(values
        .into_iter()
        .map(|(property, value)| ValueDeclaration {
            property,
            value,
            important,
        })
        .collect())
}
fn overflow(v: &[Component]) -> Option<Vec<(Property, Specified)>> {
    let values: Vec<_> = v.iter().filter(|v| !v.whitespace()).collect();
    let (a, b) = match values.as_slice() {
        [a] => (*a, *a),
        [a, b] => (*a, *b),
        _ => return None,
    };
    Some(vec![
        (OverflowX, keyword(std::slice::from_ref(a), OVERFLOW)?),
        (OverflowY, keyword(std::slice::from_ref(b), OVERFLOW)?),
    ])
}
fn border(
    v: &[Component],
    targets: &[Property],
) -> Result<Option<Vec<(Property, Specified)>>, Error> {
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
            if style.replace(v).is_some() {
                return Ok(None);
            }
        } else if let Some(v) = parse_color(one) {
            if color.replace(v).is_some() {
                return Ok(None);
            }
        } else if let Some(v) = parse_length(BorderTopWidth, one)? {
            if width.replace(v).is_some() {
                return Ok(None);
            }
        } else {
            return Ok(None);
        }
    }
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(
        targets
            .iter()
            .map(|p| {
                let value = if p.border() {
                    &width
                } else if p.border_color() {
                    &color
                } else {
                    &style
                };
                (*p, value.clone().unwrap_or(Specified::Initial))
            })
            .collect(),
    ))
}
fn font(v: &[Component]) -> Result<Option<Vec<(Property, Specified)>>, Error> {
    let tokens: Vec<_> = v
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.whitespace())
        .collect();
    let mut pos = 0;
    let mut normals = 0;
    let mut prefix = BTreeMap::new();
    let size = loop {
        let Some((_, c)) = tokens.get(pos) else {
            return Ok(None);
        };
        let one = std::slice::from_ref(*c);
        if let Some(size) = parse_length(FontSize, one)? {
            pos += 1;
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
            if prefix.insert(p, value).is_some() {
                return Ok(None);
            }
        }
        pos += 1;
    };
    prefix.insert(FontSize, size);
    if tokens.get(pos).is_some_and(|(_, c)| c.delim('/')) {
        pos += 1;
        let Some((_, c)) = tokens.get(pos) else {
            return Ok(None);
        };
        let Some(height) = parse(LineHeight, std::slice::from_ref(*c))? else {
            return Ok(None);
        };
        prefix.insert(LineHeight, height);
        pos += 1;
    }
    let Some((start, _)) = tokens.get(pos) else {
        return Ok(None);
    };
    let Some(family) = parse_families(v.get(*start..).unwrap_or_default())? else {
        return Ok(None);
    };
    prefix.insert(
        FontFamily,
        Specified::Ready(ComputedValue::FontFamily(family)),
    );
    Ok(Some(
        FONT.iter()
            .map(|p| (*p, prefix.remove(p).unwrap_or(Specified::Initial)))
            .collect(),
    ))
}
fn word(style: &ComputedStyle, p: Property) -> Option<&'static str> {
    match style.get(p) {
        Some(ComputedValue::Keyword(v)) => Some(*v),
        _ => None,
    }
}
pub(super) fn finish(style: &mut ComputedStyle, root: bool) {
    for (width, kind) in BORDERS.iter().zip(BORDER_STYLES) {
        if matches!(word(style, *kind), Some("none" | "hidden")) {
            style
                .values
                .insert(*width, ComputedValue::Length(Length::Px(0.0)));
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
                style.values.insert(p, ComputedValue::Keyword(next));
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
            style.values.insert(Display, ComputedValue::Keyword(next));
        }
    }
}

#[cfg(test)]
mod tests;
