//! Property grammars and the computed-value model.
use crate::syntax::{self, Text};
use crate::syntax::{Component, Declaration, Token};
use crate::{calc, variables, Error};
use td_alloc::{Budget, BudgetMap, BudgetShared, BudgetString, BudgetVec};
mod layout;

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
    LineHeight,
    FontVariant,
    BorderTopStyle,
    BorderRightStyle,
    BorderBottomStyle,
    BorderLeftStyle,
    BorderTopColor,
    BorderRightColor,
    BorderBottomColor,
    BorderLeftColor,
    VerticalAlign,
    TextIndent,
    TextTransform,
    Visibility,
    OverflowX,
    OverflowY,
    Float,
    Clear,
    Position,
    BoxSizing,
    MinWidth,
    MaxWidth,
    MinHeight,
    MaxHeight,
    LetterSpacing,
    Opacity,
    Top,
    Right,
    Bottom,
    Left,
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
        Self::LineHeight,
        Self::FontVariant,
        Self::BorderTopStyle,
        Self::BorderRightStyle,
        Self::BorderBottomStyle,
        Self::BorderLeftStyle,
        Self::BorderTopColor,
        Self::BorderRightColor,
        Self::BorderBottomColor,
        Self::BorderLeftColor,
        Self::VerticalAlign,
        Self::TextIndent,
        Self::TextTransform,
        Self::Visibility,
        Self::OverflowX,
        Self::OverflowY,
        Self::Float,
        Self::Clear,
        Self::Position,
        Self::BoxSizing,
        Self::MinWidth,
        Self::MaxWidth,
        Self::MinHeight,
        Self::MaxHeight,
        Self::LetterSpacing,
        Self::Opacity,
        Self::Top,
        Self::Right,
        Self::Bottom,
        Self::Left,
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
            Self::LineHeight => "line-height",
            Self::FontVariant => "font-variant",
            Self::BorderTopStyle => "border-top-style",
            Self::BorderRightStyle => "border-right-style",
            Self::BorderBottomStyle => "border-bottom-style",
            Self::BorderLeftStyle => "border-left-style",
            Self::BorderTopColor => "border-top-color",
            Self::BorderRightColor => "border-right-color",
            Self::BorderBottomColor => "border-bottom-color",
            Self::BorderLeftColor => "border-left-color",
            Self::VerticalAlign => "vertical-align",
            Self::TextIndent => "text-indent",
            Self::TextTransform => "text-transform",
            Self::Visibility => "visibility",
            Self::OverflowX => "overflow-x",
            Self::OverflowY => "overflow-y",
            Self::Float => "float",
            Self::Clear => "clear",
            Self::Position => "position",
            Self::BoxSizing => "box-sizing",
            Self::MinWidth => "min-width",
            Self::MaxWidth => "max-width",
            Self::MinHeight => "min-height",
            Self::MaxHeight => "max-height",
            Self::LetterSpacing => "letter-spacing",
            Self::Opacity => "opacity",
            Self::Top => "top",
            Self::Right => "right",
            Self::Bottom => "bottom",
            Self::Left => "left",
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
                | Self::LineHeight
                | Self::FontVariant
                | Self::TextIndent
                | Self::TextTransform
                | Self::Visibility
                | Self::LetterSpacing
        )
    }

    fn margin(self) -> bool {
        matches!(
            self,
            Self::MarginTop | Self::MarginRight | Self::MarginBottom | Self::MarginLeft
        )
    }

    fn signed_length(self) -> bool {
        self.margin()
            || matches!(
                self,
                Self::Top
                    | Self::Right
                    | Self::Bottom
                    | Self::Left
                    | Self::VerticalAlign
                    | Self::TextIndent
                    | Self::LetterSpacing
            )
    }
    fn border_color(self) -> bool {
        matches!(
            self,
            Self::BorderTopColor
                | Self::BorderRightColor
                | Self::BorderBottomColor
                | Self::BorderLeftColor
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

    /// Context-free seed; complete computation also applies dependent properties.
    pub fn initial(self, memory: &Budget) -> Result<ComputedValue, Error> {
        Ok(match self {
            Self::Display => ComputedValue::Keyword("inline"),
            Self::Color => ComputedValue::Color(Color::BLACK),
            Self::BackgroundColor => ComputedValue::Color(Color::TRANSPARENT),
            Self::FontFamily => ComputedValue::FontFamily({
                let mut list = BudgetVec::new(memory)?;
                list.try_push(Family::Generic("serif"))?;
                BudgetShared::new(memory, list)?
            }),
            Self::FontSize => ComputedValue::Length(Length::Px(16.0)),
            Self::FontWeight => ComputedValue::Weight(400),
            Self::FontStyle | Self::WhiteSpace => ComputedValue::Keyword("normal"),
            Self::TextDecoration => ComputedValue::Decoration(Decoration::default()),
            Self::Width | Self::Height => ComputedValue::Length(Length::Auto),
            Self::ListStyleType => ComputedValue::Keyword("disc"),
            Self::TextAlign => ComputedValue::Keyword("start"),
            Self::LineHeight | Self::FontVariant | Self::LetterSpacing => {
                ComputedValue::Keyword("normal")
            }
            Self::TextTransform
            | Self::Float
            | Self::Clear
            | Self::MaxWidth
            | Self::MaxHeight
            | Self::BorderTopStyle
            | Self::BorderRightStyle
            | Self::BorderBottomStyle
            | Self::BorderLeftStyle => ComputedValue::Keyword("none"),
            Self::Visibility | Self::OverflowX | Self::OverflowY => {
                ComputedValue::Keyword("visible")
            }
            Self::Position => ComputedValue::Keyword("static"),
            Self::BoxSizing => ComputedValue::Keyword("content-box"),
            Self::VerticalAlign => ComputedValue::Keyword("baseline"),
            Self::Opacity => ComputedValue::Number(1.0),
            Self::Top | Self::Right | Self::Bottom | Self::Left => {
                ComputedValue::Length(Length::Auto)
            }
            p if p.border_color() => ComputedValue::Color(Color::BLACK),
            property if property.border() => ComputedValue::Length(Length::Px(3.0)),
            _ => ComputedValue::Length(Length::Px(0.0)),
        })
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
    /// Unresolved percentage-bearing sum; the caller supplies its basis.
    Calc {
        px: f64,
        percent: f64,
        non_negative: bool,
    },
    Auto,
}

impl Length {
    /// Resolve against a caller-selected percentage basis, without layout.
    pub fn resolve(self, basis: f64) -> Option<f64> {
        if !basis.is_finite() || !(0.0..=MAX_ABSOLUTE_VALUE).contains(&basis) {
            return None;
        }
        let (n, non_negative) = match self {
            Self::Px(n) if n.is_finite() => (n, false),
            Self::Percent(p) if p.is_finite() => (p / 100.0 * basis, false),
            Self::Calc {
                px,
                percent,
                non_negative,
            } if px.is_finite() && percent.is_finite() => {
                (px + percent / 100.0 * basis, non_negative)
            }
            _ => return None,
        };
        if n.is_nan() {
            return None;
        }
        Some(n.clamp(
            if non_negative {
                0.0
            } else {
                -MAX_ABSOLUTE_VALUE
            },
            MAX_ABSOLUTE_VALUE,
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Family {
    Generic(&'static str),
    Named(Text),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decoration {
    pub underline: bool,
    pub overline: bool,
    pub line_through: bool,
}

#[derive(Debug, PartialEq)]
pub enum ComputedValue {
    Number(f64),
    Keyword(&'static str),
    Color(Color),
    Length(Length),
    FontFamily(BudgetShared<BudgetVec<Family>>),
    Weight(u16),
    Decoration(Decoration),
}

#[derive(Debug, PartialEq)]
pub struct ComputedStyle {
    pub(crate) values: BudgetMap<Property, ComputedValue>,
    pub(crate) custom: variables::CustomProperties,
}

impl ComputedStyle {
    pub fn new(memory: &Budget) -> Result<Self, Error> {
        let mut values = td_alloc::BudgetMap::with_capacity(memory, Property::ALL.len())?;
        for property in Property::ALL {
            let value = if property.border() {
                ComputedValue::Length(Length::Px(0.0))
            } else {
                property.initial(memory)?
            };
            values.try_insert(*property, value)?;
        }
        Ok(Self {
            values,
            custom: variables::CustomProperties::new(memory)?,
        })
    }
    pub fn try_clone(&self) -> Result<Self, Error> {
        let mut values =
            td_alloc::BudgetMap::with_capacity(self.values.budget(), self.values.len())?;
        for (key, value) in &self.values {
            values.try_insert(*key, value.try_clone()?)?;
        }
        Ok(Self {
            values,
            custom: self.custom.try_clone()?,
        })
    }
}
impl ComputedValue {
    pub fn try_clone(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::Number(v) => Self::Number(*v),
            Self::Keyword(v) => Self::Keyword(v),
            Self::Color(v) => Self::Color(*v),
            Self::Length(v) => Self::Length(*v),
            Self::FontFamily(v) => Self::FontFamily(v.try_clone()?),
            Self::Weight(v) => Self::Weight(*v),
            Self::Decoration(v) => Self::Decoration(*v),
        })
    }
}
impl Specified {
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::Pending(v) => Self::Pending(v.try_clone()?),
            Self::Initial => Self::Initial,
            Self::Inherit => Self::Inherit,
            Self::Unset => Self::Unset,
            Self::Ready(v) => Self::Ready(v.try_clone()?),
            Self::CurrentColor => Self::CurrentColor,
            Self::Length(n, u) => Self::Length(*n, *u),
            Self::Calculated(v) => Self::Calculated(*v),
            Self::RelativeWeight(v) => Self::RelativeWeight(*v),
        })
    }
}

impl ComputedStyle {
    /// The inherited, substituted token value; None is missing or invalid.
    pub fn custom_property(&self, name: &str) -> Option<&[Component]> {
        self.custom.get(name)
    }

    pub fn get(&self, property: Property) -> Option<&ComputedValue> {
        // Completed styles contain the entire ordered property roster.
        self.values
            .get_index(property as usize)
            .filter(|(key, _)| **key == property)
            .map(|(_, value)| value)
            .or_else(|| self.values.get(&property))
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

#[derive(Debug)]
pub(crate) enum Specified {
    Pending(variables::Pending),
    Initial,
    Inherit,
    Unset,
    Ready(ComputedValue),
    CurrentColor,
    Length(f64, Unit),
    Calculated(calc::Linear),
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

#[derive(Debug)]
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
    match syntax::keyword(single_ident(values)?, &["initial", "inherit", "unset"])? {
        "initial" => Some(Specified::Initial),
        "inherit" => Some(Specified::Inherit),
        "unset" => Some(Specified::Unset),
        _ => None,
    }
}

pub(crate) enum Declarations {
    Single(Option<ValueDeclaration>),
    Many(BudgetVec<ValueDeclaration>),
}
impl Declarations {
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Single(value) => value.is_none(),
            Self::Many(values) => values.is_empty(),
        }
    }
}
pub(crate) enum DeclarationIter {
    Single(std::option::IntoIter<ValueDeclaration>),
    Many(td_alloc::BudgetIntoIter<ValueDeclaration>),
}
impl Iterator for DeclarationIter {
    type Item = ValueDeclaration;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Single(value) => value.next(),
            Self::Many(values) => values.next(),
        }
    }
}
impl IntoIterator for Declarations {
    type Item = ValueDeclaration;
    type IntoIter = DeclarationIter;
    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Single(value) => DeclarationIter::Single(value.into_iter()),
            Self::Many(values) => DeclarationIter::Many(values.into_iter()),
        }
    }
}

pub(crate) fn parse_declaration(
    declaration: &Declaration,
    memory: &Budget,
) -> Result<Declarations, Error> {
    parse_named(
        declaration.name(),
        declaration.value(),
        declaration.important(),
        memory,
    )
}
fn parse_named(
    name: &str,
    values: &[Component],
    important: bool,
    memory: &Budget,
) -> Result<Declarations, Error> {
    let name = syntax::lower_text(memory, name)?;
    let values = syntax::trim(values);
    let sides = match name.as_str() {
        "margin" => Some(MARGINS),
        "padding" => Some(PADDINGS),
        "border-width" => Some(BORDERS),
        "border-style" => Some(layout::BORDER_STYLES),
        "border-color" => Some(layout::BORDER_COLORS),
        _ => None,
    };
    let Some(has_var) = variables::inspect(values) else {
        return Ok(Declarations::Single(None));
    };
    if has_var {
        let mut out = BudgetVec::new(memory)?;
        let targets = sides.or_else(|| layout::targets(&name));
        let pending = variables::Pending {
            name: name.try_clone()?,
            tokens: BudgetShared::new(memory, syntax::copy_components(values, memory)?)?,
        };
        for property in Property::ALL.iter().copied().filter(|p| {
            targets.map_or_else(|| p.name() == name.as_str(), |sides| sides.contains(p))
        }) {
            out.try_push(ValueDeclaration {
                property,
                value: Specified::Pending(pending.try_clone()?),
                important,
            })?;
        }
        return Ok(Declarations::Many(out));
    }
    if let Some(sides) = sides {
        let mut out = BudgetVec::new(memory)?;
        let mut parsed = [None, None, None, None];
        let mut count = 0usize;
        if let Some(value) = global(values) {
            *parsed.get_mut(count).ok_or(Error::InvalidContext)? = Some(value);
            count += 1;
        } else {
            for component in values.iter().filter(|v| !v.whitespace()) {
                if count == 4 || global(std::slice::from_ref(component)).is_some() {
                    return Ok(Declarations::Single(None));
                }
                let Some(property) = sides.first() else {
                    return Ok(Declarations::Single(None));
                };
                let Some(value) = parse_value(*property, std::slice::from_ref(component), memory)?
                else {
                    return Ok(Declarations::Single(None));
                };
                *parsed.get_mut(count).ok_or(Error::InvalidContext)? = Some(value);
                count += 1;
            }
        }
        let indexes = match count {
            1 => [0, 0, 0, 0],
            2 => [0, 1, 0, 1],
            3 => [0, 1, 2, 1],
            4 => [0, 1, 2, 3],
            _ => return Ok(Declarations::Single(None)),
        };
        for (property, index) in sides.iter().copied().zip(indexes) {
            out.try_push(ValueDeclaration {
                property,
                value: parsed
                    .get(index)
                    .and_then(Option::as_ref)
                    .ok_or(Error::InvalidContext)?
                    .try_clone()?,
                important,
            })?;
        }
        return Ok(Declarations::Many(out));
    }
    if layout::targets(&name).is_some() {
        return layout::shorthand(&name, values, important, memory).map(Declarations::Many);
    }
    let Some(property) = Property::ALL
        .iter()
        .copied()
        .find(|p| p.name() == name.as_str())
    else {
        return Ok(Declarations::Single(None));
    };
    Ok(Declarations::Single(
        parse_value(property, values, memory)?.map(|value| ValueDeclaration {
            property,
            value,
            important,
        }),
    ))
}

pub(crate) fn resolve_pending(
    property: Property,
    value: &Specified,
    custom: &variables::CustomProperties,
    budget: &mut variables::Budget,
) -> Result<Specified, Error> {
    let Specified::Pending(pending) = value else {
        return value.try_clone();
    };
    let Some(tokens) = variables::substitute(&pending.tokens, custom, budget)? else {
        return Ok(Specified::Unset);
    };
    Ok(parse_named(&pending.name, &tokens, false, &budget.memory)?
        .into_iter()
        .find(|d| d.property == property)
        .map(|d| d.value)
        .unwrap_or(Specified::Unset))
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

pub(crate) fn length(value: &Component) -> Option<(f64, Unit)> {
    match value {
        Component::Token(Token::Number(n)) if number(&n.representation)? == 0.0 => {
            Some((0.0, Unit::Px))
        }
        Component::Token(Token::Percentage(n)) => Some((number(&n.representation)?, Unit::Percent)),
        Component::Token(Token::Dimension(n, unit)) => {
            let unit = match syntax::keyword(
                unit,
                &["px", "pt", "pc", "in", "cm", "mm", "q", "em", "rem"],
            )? {
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

fn parse_value(
    property: Property,
    values: &[Component],
    memory: &Budget,
) -> Result<Option<Specified>, Error> {
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
        Color | BackgroundColor | BorderTopColor | BorderRightColor | BorderBottomColor
        | BorderLeftColor => parse_color(values),
        FontFamily => {
            parse_families(values, memory)?.map(|v| Specified::Ready(ComputedValue::FontFamily(v)))
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
        p if layout::extra(p) => layout::parse(p, values)?,
        _ => parse_length(property, values)?,
    };
    Ok(result)
}

fn parse_length(property: Property, values: &[Component]) -> Result<Option<Specified>, Error> {
    if let [value @ Component::Function { .. }] = values {
        Ok(calc::parse(
            value,
            !property.border() && property != Property::LetterSpacing,
        )?
        .map(Specified::Calculated))
    } else {
        Ok(parse_length_property(property, values))
    }
}

fn parse_length_property(property: Property, values: &[Component]) -> Option<Specified> {
    let [value] = values else {
        return None;
    };
    if let Some(name) = value.ident() {
        if property == Property::FontSize {
            let (size, unit) = match syntax::keyword(
                name,
                &[
                    "xx-small", "x-small", "small", "medium", "large", "x-large", "xx-large",
                    "smaller", "larger",
                ],
            )? {
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
            let width = match syntax::keyword(name, &["thin", "medium", "thick"])? {
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
    if number < 0.0 && !property.signed_length() {
        return None;
    }
    if matches!(unit, Unit::Auto)
        && !(property.margin()
            || matches!(
                property,
                Property::Width
                    | Property::Height
                    | Property::Top
                    | Property::Right
                    | Property::Bottom
                    | Property::Left
            ))
    {
        return None;
    }
    if matches!(unit, Unit::Percent) && (property.border() || property == Property::LetterSpacing) {
        return None;
    }
    Some(Specified::Length(number, unit))
}

fn weight(values: &[Component]) -> Option<Specified> {
    if let Some(name) = single_ident(values) {
        return Some(
            match syntax::keyword(name, &["normal", "bold", "bolder", "lighter"])? {
                "normal" => Specified::Ready(ComputedValue::Weight(400)),
                "bold" => Specified::Ready(ComputedValue::Weight(700)),
                "bolder" => Specified::RelativeWeight(true),
                "lighter" => Specified::RelativeWeight(false),
                _ => return None,
            },
        );
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
        let target =
            match syntax::keyword(value.ident()?, &["underline", "overline", "line-through"])? {
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

fn parse_families(
    values: &[Component],
    memory: &Budget,
) -> Result<Option<BudgetShared<BudgetVec<Family>>>, Error> {
    let mut families = BudgetVec::new(memory)?;
    for part in values.split(|v| v.delim(',')) {
        if families.len() == MAX_FAMILIES {
            return Err(Error::Limit("MAX_FAMILIES"));
        }
        let part = syntax::trim(part);
        if let [Component::Token(Token::String(name))] = part {
            families.try_push(Family::Named(name.try_clone()?))?;
            continue;
        }
        if let [value] = part {
            if let Some(generic) = value
                .ident()
                .and_then(|name| syntax::keyword(name, FAMILIES))
            {
                families.try_push(Family::Generic(generic))?;
                continue;
            }
        }
        if let [Component::Token(Token::Ident(name))] = part {
            if syntax::keyword(
                name,
                &[
                    "initial",
                    "inherit",
                    "unset",
                    "revert",
                    "revert-layer",
                    "default",
                ],
            )
            .is_some()
            {
                return Ok(None);
            }
            families.try_push(Family::Named(name.try_clone()?))?;
            continue;
        }
        let mut joined = BudgetString::new(memory)?;
        for value in part.iter().filter(|v| !v.whitespace()) {
            let Some(name) = value.ident() else {
                return Ok(None);
            };
            if syntax::keyword(
                name,
                &[
                    "initial",
                    "inherit",
                    "unset",
                    "revert",
                    "revert-layer",
                    "default",
                ],
            )
            .is_some()
                || syntax::keyword(name, FAMILIES).is_some()
            {
                return Ok(None);
            }
            if !joined.is_empty() {
                joined.try_push(' ')?;
            }
            joined.try_push_str(name)?;
        }
        if joined.is_empty() {
            return Ok(None);
        }
        families.try_push(Family::Named(Text::from_string(memory, joined)?))?;
    }
    Ok(Some(BudgetShared::new(memory, families)?))
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
            let (red, green, blue, alpha) = match syntax::keyword(
                name,
                &[
                    "black",
                    "silver",
                    "gray",
                    "white",
                    "maroon",
                    "red",
                    "purple",
                    "fuchsia",
                    "green",
                    "lime",
                    "olive",
                    "yellow",
                    "navy",
                    "blue",
                    "teal",
                    "aqua",
                    "orange",
                    "rebeccapurple",
                    "transparent",
                ],
            )? {
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
            let channels = match value.len() {
                3 | 4 => value.len(),
                6 | 8 => value.len() / 2,
                _ => return None,
            };
            let mut bytes = [0u8, 0, 0, 255];
            for (index, byte) in bytes.iter_mut().take(channels).enumerate() {
                *byte = if value.len() <= 4 {
                    u8::from_str_radix(value.get(index..index + 1)?, 16).ok()? * 17
                } else {
                    u8::from_str_radix(value.get(index * 2..index * 2 + 2)?, 16).ok()?
                };
            }
            let [red, green, blue, alpha] = bytes;
            Color {
                red,
                green,
                blue,
                alpha,
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
    memory: &Budget,
) -> Result<ComputedValue, Error> {
    let initial = || {
        if property.border_color() {
            Ok(ComputedValue::Color(color))
        } else {
            property.initial(memory)
        }
    };
    let inherited = || match parent.and_then(|style| style.get(property)) {
        Some(value) => value.try_clone(),
        None => initial(),
    };
    let result = match value {
        Specified::Pending(_) => return Err(Error::InvalidContext),
        Specified::Initial => initial()?,
        Specified::Inherit => inherited()?,
        Specified::Unset => {
            if property.inherited() {
                inherited()?
            } else {
                initial()?
            }
        }
        Specified::Ready(value) => value.try_clone()?,
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
        Specified::Calculated(value) => {
            let em = if property == Property::FontSize {
                parent.map(ComputedStyle::font_size).unwrap_or(16.0)
            } else {
                font_size
            };
            ComputedValue::Length(value.compute(
                em,
                root_font_size,
                (matches!(property, Property::FontSize | Property::LineHeight)).then_some(em),
                !property.signed_length(),
            ))
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
                Unit::Percent if !matches!(property, Property::FontSize | Property::LineHeight) => {
                    Length::Percent(clamp(*value))
                }
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
    };
    Ok(result)
}

pub(crate) fn finish(style: &mut ComputedStyle, root: bool) -> Result<(), Error> {
    layout::finish(style, root)
}
