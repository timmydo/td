//! The first property grammar and computed-value model.
use crate::syntax::{Component, Declaration, Token};
use crate::Error;
use std::collections::BTreeMap;

pub const MAX_FAMILIES: usize = 32;
pub const MAX_ABSOLUTE_VALUE: f64 = 1_000_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Property {
    Display,
    Color,
    BackgroundColor,
    FontFamily,
    FontSize,
    FontWeight,
    FontStyle,
    TextDecoration,
    WhiteSpace,
    MarginTop,
    MarginRight,
    MarginBottom,
    MarginLeft,
    PaddingTop,
    PaddingRight,
    PaddingBottom,
    PaddingLeft,
    BorderTopWidth,
    BorderRightWidth,
    BorderBottomWidth,
    BorderLeftWidth,
    Width,
    Height,
    ListStyleType,
    TextAlign,
}

impl Property {
    pub const ALL: &'static [Self] = &[
        Self::Display,
        Self::Color,
        Self::BackgroundColor,
        Self::FontFamily,
        Self::FontSize,
        Self::FontWeight,
        Self::FontStyle,
        Self::TextDecoration,
        Self::WhiteSpace,
        Self::MarginTop,
        Self::MarginRight,
        Self::MarginBottom,
        Self::MarginLeft,
        Self::PaddingTop,
        Self::PaddingRight,
        Self::PaddingBottom,
        Self::PaddingLeft,
        Self::BorderTopWidth,
        Self::BorderRightWidth,
        Self::BorderBottomWidth,
        Self::BorderLeftWidth,
        Self::Width,
        Self::Height,
        Self::ListStyleType,
        Self::TextAlign,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Color => "color",
            Self::BackgroundColor => "background-color",
            Self::FontFamily => "font-family",
            Self::FontSize => "font-size",
            Self::FontWeight => "font-weight",
            Self::FontStyle => "font-style",
            Self::TextDecoration => "text-decoration",
            Self::WhiteSpace => "white-space",
            Self::MarginTop => "margin-top",
            Self::MarginRight => "margin-right",
            Self::MarginBottom => "margin-bottom",
            Self::MarginLeft => "margin-left",
            Self::PaddingTop => "padding-top",
            Self::PaddingRight => "padding-right",
            Self::PaddingBottom => "padding-bottom",
            Self::PaddingLeft => "padding-left",
            Self::BorderTopWidth => "border-top-width",
            Self::BorderRightWidth => "border-right-width",
            Self::BorderBottomWidth => "border-bottom-width",
            Self::BorderLeftWidth => "border-left-width",
            Self::Width => "width",
            Self::Height => "height",
            Self::ListStyleType => "list-style-type",
            Self::TextAlign => "text-align",
        }
    }

    pub fn inherited(self) -> bool {
        matches!(
            self,
            Self::Color
                | Self::FontFamily
                | Self::FontSize
                | Self::FontWeight
                | Self::FontStyle
                | Self::WhiteSpace
                | Self::ListStyleType
                | Self::TextAlign
        )
    }

    fn margin(self) -> bool {
        matches!(
            self,
            Self::MarginTop | Self::MarginRight | Self::MarginBottom | Self::MarginLeft
        )
    }

    fn border(self) -> bool {
        matches!(
            self,
            Self::BorderTopWidth
                | Self::BorderRightWidth
                | Self::BorderBottomWidth
                | Self::BorderLeftWidth
        )
    }

    pub fn initial(self) -> ComputedValue {
        match self {
            Self::Display => ComputedValue::Keyword("inline"),
            Self::Color => ComputedValue::Color(Color::BLACK),
            Self::BackgroundColor => ComputedValue::Color(Color::TRANSPARENT),
            Self::FontFamily => ComputedValue::FontFamily(vec![Family::Generic("serif")]),
            Self::FontSize => ComputedValue::Length(Length::Px(16.0)),
            Self::FontWeight => ComputedValue::Weight(400),
            Self::FontStyle | Self::WhiteSpace => ComputedValue::Keyword("normal"),
            Self::TextDecoration => ComputedValue::Decoration(Decoration::default()),
            Self::Width | Self::Height => ComputedValue::Length(Length::Auto),
            Self::ListStyleType => ComputedValue::Keyword("disc"),
            Self::TextAlign => ComputedValue::Keyword("start"),
            property if property.border() => ComputedValue::Length(Length::Px(3.0)),
            _ => ComputedValue::Length(Length::Px(0.0)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Color {
    pub const BLACK: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 255,
    };
    pub const TRANSPARENT: Self = Self {
        red: 0,
        green: 0,
        blue: 0,
        alpha: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    Px(f64),
    Percent(f64),
    Auto,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Family {
    Generic(&'static str),
    Named(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decoration {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ComputedValue {
    Keyword(&'static str),
    Color(Color),
    Length(Length),
    FontFamily(Vec<Family>),
    Weight(u16),
    Decoration(Decoration),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ComputedStyle {
    pub(crate) values: BTreeMap<Property, ComputedValue>,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            values: Property::ALL.iter().map(|p| (*p, p.initial())).collect(),
        }
    }
}

impl ComputedStyle {
    pub fn get(&self, property: Property) -> Option<&ComputedValue> {
        self.values.get(&property)
    }

    pub fn font_size(&self) -> f64 {
        match self.get(Property::FontSize) {
            Some(ComputedValue::Length(Length::Px(value))) => *value,
            _ => 16.0,
        }
    }

    pub fn color(&self) -> Color {
        match self.get(Property::Color) {
            Some(ComputedValue::Color(color)) => *color,
            _ => Color::BLACK,
        }
    }

    fn weight(&self) -> u16 {
        match self.get(Property::FontWeight) {
            Some(ComputedValue::Weight(value)) => *value,
            _ => 400,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Specified {
    Initial,
    Inherit,
    Unset,
    Ready(ComputedValue),
    CurrentColor,
    Length(f64, Unit),
    RelativeWeight(bool),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Unit {
    Px,
    Pt,
    Pc,
    In,
    Cm,
    Mm,
    Q,
    Em,
    Rem,
    Percent,
    Auto,
}

#[derive(Clone, Debug)]
pub(crate) struct ValueDeclaration {
    pub property: Property,
    pub value: Specified,
    pub important: bool,
}

const MARGINS: &[Property] = &[
    Property::MarginTop,
    Property::MarginRight,
    Property::MarginBottom,
    Property::MarginLeft,
];
const PADDINGS: &[Property] = &[
    Property::PaddingTop,
    Property::PaddingRight,
    Property::PaddingBottom,
    Property::PaddingLeft,
];
const BORDERS: &[Property] = &[
    Property::BorderTopWidth,
    Property::BorderRightWidth,
    Property::BorderBottomWidth,
    Property::BorderLeftWidth,
];
const FAMILIES: &[&str] = &[
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-serif",
    "ui-sans-serif",
    "ui-monospace",
    "ui-rounded",
];

fn single_ident(values: &[Component]) -> Option<&str> {
    match values {
        [value] => value.ident(),
        _ => None,
    }
}

fn global(values: &[Component]) -> Option<Specified> {
    match single_ident(values)?.to_ascii_lowercase().as_str() {
        "initial" => Some(Specified::Initial),
        "inherit" => Some(Specified::Inherit),
        "unset" => Some(Specified::Unset),
        _ => None,
    }
}

pub(crate) fn parse_declaration(declaration: &Declaration) -> Result<Vec<ValueDeclaration>, Error> {
    let name = declaration.name().to_ascii_lowercase();
    let values = declaration.value();
    let sides = match name.as_str() {
        "margin" => Some(MARGINS),
        "padding" => Some(PADDINGS),
        "border-width" => Some(BORDERS),
        _ => None,
    };
    if let Some(sides) = sides {
        let parsed = if let Some(value) = global(values) {
            vec![value]
        } else {
            let mut parsed = Vec::new();
            for component in values.iter().filter(|v| !v.whitespace()) {
                if parsed.len() == 4 {
                    return Ok(Vec::new());
                }
                let Some(&property) = sides.first() else {
                    return Ok(Vec::new());
                };
                if global(std::slice::from_ref(component)).is_some() {
                    return Ok(Vec::new());
                }
                let Some(value) = parse_value(property, std::slice::from_ref(component))? else {
                    return Ok(Vec::new());
                };
                parsed.push(value);
            }
            parsed
        };
        let expanded = match parsed.as_slice() {
            [a] => vec![a.clone(), a.clone(), a.clone(), a.clone()],
            [a, b] => vec![a.clone(), b.clone(), a.clone(), b.clone()],
            [a, b, c] => vec![a.clone(), b.clone(), c.clone(), b.clone()],
            [a, b, c, d] => vec![a.clone(), b.clone(), c.clone(), d.clone()],
            _ => return Ok(Vec::new()),
        };
        return Ok(sides
            .iter()
            .copied()
            .zip(expanded)
            .map(|(property, value)| ValueDeclaration {
                property,
                value,
                important: declaration.important(),
            })
            .collect());
    }
    let Some(property) = Property::ALL.iter().copied().find(|p| p.name() == name) else {
        return Ok(Vec::new());
    };
    Ok(parse_value(property, values)?
        .map(|value| ValueDeclaration {
            property,
            value,
            important: declaration.important(),
        })
        .into_iter()
        .collect())
}

fn keyword(values: &[Component], options: &'static [&'static str]) -> Option<Specified> {
    let name = single_ident(values)?;
    options
        .iter()
        .find(|option| name.eq_ignore_ascii_case(option))
        .map(|value| Specified::Ready(ComputedValue::Keyword(value)))
}

fn number(text: &str) -> Option<f64> {
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

fn length(value: &Component) -> Option<(f64, Unit)> {
    match value {
        Component::Token(Token::Number(n)) if number(&n.representation)? == 0.0 => {
            Some((0.0, Unit::Px))
        }
        Component::Token(Token::Percentage(n)) => Some((number(&n.representation)?, Unit::Percent)),
        Component::Token(Token::Dimension(n, unit)) => {
            let unit = match unit.to_ascii_lowercase().as_str() {
                "px" => Unit::Px,
                "pt" => Unit::Pt,
                "pc" => Unit::Pc,
                "in" => Unit::In,
                "cm" => Unit::Cm,
                "mm" => Unit::Mm,
                "q" => Unit::Q,
                "em" => Unit::Em,
                "rem" => Unit::Rem,
                _ => return None,
            };
            Some((number(&n.representation)?, unit))
        }
        Component::Token(Token::Ident(name)) if name.eq_ignore_ascii_case("auto") => {
            Some((0.0, Unit::Auto))
        }
        _ => None,
    }
}

fn parse_value(property: Property, values: &[Component]) -> Result<Option<Specified>, Error> {
    if let Some(value) = global(values) {
        return Ok(Some(value));
    }
    use Property::*;
    let result = match property {
        Display => keyword(
            values,
            &[
                "none",
                "inline",
                "block",
                "inline-block",
                "list-item",
                "table",
                "inline-table",
                "table-row-group",
                "table-header-group",
                "table-footer-group",
                "table-row",
                "table-cell",
                "table-column-group",
                "table-column",
                "table-caption",
                "flex",
                "inline-flex",
            ],
        ),
        Color | BackgroundColor => parse_color(values),
        FontFamily => {
            parse_families(values)?.map(|v| Specified::Ready(ComputedValue::FontFamily(v)))
        }
        FontStyle => keyword(values, &["normal", "italic", "oblique"]),
        WhiteSpace => keyword(
            values,
            &[
                "normal",
                "pre",
                "nowrap",
                "pre-wrap",
                "pre-line",
                "break-spaces",
            ],
        ),
        TextAlign => keyword(
            values,
            &["start", "end", "left", "right", "center", "justify"],
        ),
        ListStyleType => keyword(
            values,
            &[
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
            ],
        ),
        TextDecoration => {
            decoration(values).map(|v| Specified::Ready(ComputedValue::Decoration(v)))
        }
        FontWeight => weight(values),
        _ => parse_length_property(property, values),
    };
    Ok(result)
}

fn parse_length_property(property: Property, values: &[Component]) -> Option<Specified> {
    let [value] = values else {
        return None;
    };
    if let Some(name) = value.ident() {
        if property == Property::FontSize {
            let (size, unit) = match name.to_ascii_lowercase().as_str() {
                "xx-small" => (9.0, Unit::Px),
                "x-small" => (10.0, Unit::Px),
                "small" => (13.0, Unit::Px),
                "medium" => (16.0, Unit::Px),
                "large" => (18.0, Unit::Px),
                "x-large" => (24.0, Unit::Px),
                "xx-large" => (32.0, Unit::Px),
                "smaller" => (1.0 / 1.2, Unit::Em),
                "larger" => (1.2, Unit::Em),
                _ => return length_property(property, value),
            };
            return Some(Specified::Length(size, unit));
        }
        if property.border() {
            let width = match name.to_ascii_lowercase().as_str() {
                "thin" => 1.0,
                "medium" => 3.0,
                "thick" => 5.0,
                _ => return length_property(property, value),
            };
            return Some(Specified::Length(width, Unit::Px));
        }
    }
    length_property(property, value)
}

fn length_property(property: Property, value: &Component) -> Option<Specified> {
    let (number, unit) = length(value)?;
    if number < 0.0 && !property.margin() {
        return None;
    }
    if matches!(unit, Unit::Auto)
        && !(property.margin() || matches!(property, Property::Width | Property::Height))
    {
        return None;
    }
    if matches!(unit, Unit::Percent) && property.border() {
        return None;
    }
    Some(Specified::Length(number, unit))
}

fn weight(values: &[Component]) -> Option<Specified> {
    if let Some(name) = single_ident(values) {
        return Some(match name.to_ascii_lowercase().as_str() {
            "normal" => Specified::Ready(ComputedValue::Weight(400)),
            "bold" => Specified::Ready(ComputedValue::Weight(700)),
            "bolder" => Specified::RelativeWeight(true),
            "lighter" => Specified::RelativeWeight(false),
            _ => return None,
        });
    }
    let [Component::Token(Token::Number(number))] = values else {
        return None;
    };
    if !number.integer {
        return None;
    }
    let value = number.representation.parse::<u16>().ok()?;
    ((100..=900).contains(&value) && value % 100 == 0)
        .then_some(Specified::Ready(ComputedValue::Weight(value)))
}

fn decoration(values: &[Component]) -> Option<Decoration> {
    if single_ident(values).is_some_and(|v| v.eq_ignore_ascii_case("none")) {
        return Some(Decoration::default());
    }
    let mut result = Decoration::default();
    let mut any = false;
    for value in values.iter().filter(|v| !v.whitespace()) {
        let target = match value.ident()?.to_ascii_lowercase().as_str() {
            "underline" => &mut result.underline,
            "overline" => &mut result.overline,
            "line-through" => &mut result.line_through,
            _ => return None,
        };
        if *target {
            return None;
        }
        *target = true;
        any = true;
    }
    any.then_some(result)
}

fn parse_families(values: &[Component]) -> Result<Option<Vec<Family>>, Error> {
    let mut families = Vec::new();
    for part in values.split(|v| v.delim(',')) {
        if families.len() == MAX_FAMILIES {
            return Err(Error::Limit("MAX_FAMILIES"));
        }
        let part = crate::syntax::trim(part);
        if let [Component::Token(Token::String(name))] = part {
            families.push(Family::Named(name.clone()));
            continue;
        }
        let mut words = Vec::new();
        for value in part.iter().filter(|v| !v.whitespace()) {
            let Some(name) = value.ident() else {
                return Ok(None);
            };
            if [
                "initial",
                "inherit",
                "unset",
                "revert",
                "revert-layer",
                "default",
            ]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n))
            {
                return Ok(None);
            }
            words.push(name);
        }
        if words.is_empty() {
            return Ok(None);
        }
        if let [name] = words.as_slice() {
            if let Some(generic) = FAMILIES
                .iter()
                .find(|family| name.eq_ignore_ascii_case(family))
            {
                families.push(Family::Generic(generic));
                continue;
            }
        }
        if words.iter().any(|name| {
            FAMILIES
                .iter()
                .any(|family| name.eq_ignore_ascii_case(family))
        }) {
            return Ok(None);
        }
        families.push(Family::Named(words.join(" ")));
    }
    Ok(Some(families))
}

fn parse_color(values: &[Component]) -> Option<Specified> {
    let [value] = values else {
        return None;
    };
    let color = match value {
        Component::Token(Token::Ident(name)) => {
            if name.eq_ignore_ascii_case("currentcolor") {
                return Some(Specified::CurrentColor);
            }
            let (red, green, blue, alpha) = match name.to_ascii_lowercase().as_str() {
                "black" => (0, 0, 0, 255),
                "silver" => (192, 192, 192, 255),
                "gray" => (128, 128, 128, 255),
                "white" => (255, 255, 255, 255),
                "maroon" => (128, 0, 0, 255),
                "red" => (255, 0, 0, 255),
                "purple" => (128, 0, 128, 255),
                "fuchsia" => (255, 0, 255, 255),
                "green" => (0, 128, 0, 255),
                "lime" => (0, 255, 0, 255),
                "olive" => (128, 128, 0, 255),
                "yellow" => (255, 255, 0, 255),
                "navy" => (0, 0, 128, 255),
                "blue" => (0, 0, 255, 255),
                "teal" => (0, 128, 128, 255),
                "aqua" => (0, 255, 255, 255),
                "orange" => (255, 165, 0, 255),
                "rebeccapurple" => (102, 51, 153, 255),
                "transparent" => (0, 0, 0, 0),
                _ => return None,
            };
            Color {
                red,
                green,
                blue,
                alpha,
            }
        }
        Component::Token(Token::Hash { value, .. }) => {
            if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            let mut bytes = Vec::new();
            match value.len() {
                3 | 4 => {
                    for ch in value.chars() {
                        bytes.push(u8::try_from(ch.to_digit(16)?).ok()? * 17);
                    }
                }
                6 | 8 => {
                    for pair in value.as_bytes().as_chunks::<2>().0 {
                        let text = std::str::from_utf8(pair).ok()?;
                        bytes.push(u8::from_str_radix(text, 16).ok()?);
                    }
                }
                _ => return None,
            }
            match bytes.as_slice() {
                [red, green, blue] => Color {
                    red: *red,
                    green: *green,
                    blue: *blue,
                    alpha: 255,
                },
                [red, green, blue, alpha] => Color {
                    red: *red,
                    green: *green,
                    blue: *blue,
                    alpha: *alpha,
                },
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(Specified::Ready(ComputedValue::Color(color)))
}

pub(crate) fn compute_value(
    property: Property,
    value: &Specified,
    parent: Option<&ComputedStyle>,
    font_size: f64,
    color: Color,
    root_font_size: f64,
) -> ComputedValue {
    let inherited = || {
        parent
            .and_then(|style| style.get(property))
            .cloned()
            .unwrap_or_else(|| property.initial())
    };
    match value {
        Specified::Initial => property.initial(),
        Specified::Inherit => inherited(),
        Specified::Unset => {
            if property.inherited() {
                inherited()
            } else {
                property.initial()
            }
        }
        Specified::Ready(value) => value.clone(),
        Specified::CurrentColor => ComputedValue::Color(if property == Property::Color {
            parent.map(ComputedStyle::color).unwrap_or(Color::BLACK)
        } else {
            color
        }),
        Specified::RelativeWeight(bolder) => {
            let parent = parent.map(ComputedStyle::weight).unwrap_or(400);
            let weight = if *bolder {
                match parent {
                    0..=300 => 400,
                    301..=500 => 700,
                    _ => 900,
                }
            } else {
                match parent {
                    0..=500 => 100,
                    501..=700 => 400,
                    _ => 700,
                }
            };
            ComputedValue::Weight(weight)
        }
        Specified::Length(value, unit) => {
            let em = if property == Property::FontSize {
                parent.map(ComputedStyle::font_size).unwrap_or(16.0)
            } else {
                font_size
            };
            let clamp = |value: f64| value.clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE);
            let length = match unit {
                Unit::Auto => Length::Auto,
                Unit::Percent if property != Property::FontSize => Length::Percent(clamp(*value)),
                _ => {
                    let factor = match unit {
                        Unit::Px => 1.0,
                        Unit::Pt => 96.0 / 72.0,
                        Unit::Pc => 16.0,
                        Unit::In => 96.0,
                        Unit::Cm => 96.0 / 2.54,
                        Unit::Mm => 96.0 / 25.4,
                        Unit::Q => 96.0 / 101.6,
                        Unit::Em => em,
                        Unit::Rem => root_font_size,
                        Unit::Percent => em / 100.0,
                        Unit::Auto => 0.0,
                    };
                    Length::Px(clamp(value * factor))
                }
            };
            ComputedValue::Length(length)
        }
    }
}
