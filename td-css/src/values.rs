//! Property grammars and the computed-value model.
use crate::syntax::{self, Text};
use crate::syntax::{Component, Declaration, Token};
use crate::{calc, variables, Error};
use td_alloc::{Budget, BudgetMap, BudgetShared, BudgetString, BudgetVec};
mod color;
mod grid;
mod layout;
use color::parse_color;
pub use grid::{
    Breadth, GridArea, GridAreas, GridLine, Repeat, TrackItem, TrackList, TrackSize,
    MAX_GRID_AREAS, MAX_GRID_LINE, MAX_GRID_TRACKS,
};

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
    Position,
    BoxSizing,
    MinWidth,
    MaxWidth,
    MinHeight,
    MaxHeight,
    LetterSpacing,
    ListStylePosition,
    BorderCollapse,
    BorderSpacing,
    // The rare group (`Rare`): not inherited, rarely set, last.
    Clear,
    Opacity,
    Top,
    Right,
    Bottom,
    Left,
    TableLayout,
    FlexDirection,
    FlexWrap,
    FlexGrow,
    FlexShrink,
    FlexBasis,
    JustifyContent,
    AlignItems,
    AlignSelf,
    AlignContent,
    RowGap,
    ColumnGap,
    Order,
    GridTemplateColumns,
    GridTemplateRows,
    GridTemplateAreas,
    GridAutoColumns,
    GridAutoRows,
    GridAutoFlow,
    GridRowStart,
    GridRowEnd,
    GridColumnStart,
    GridColumnEnd,
    JustifyItems,
    JustifySelf,
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
        Self::Position,
        Self::BoxSizing,
        Self::MinWidth,
        Self::MaxWidth,
        Self::MinHeight,
        Self::MaxHeight,
        Self::LetterSpacing,
        Self::ListStylePosition,
        Self::BorderCollapse,
        Self::BorderSpacing,
        Self::Clear,
        Self::Opacity,
        Self::Top,
        Self::Right,
        Self::Bottom,
        Self::Left,
        Self::TableLayout,
        Self::FlexDirection,
        Self::FlexWrap,
        Self::FlexGrow,
        Self::FlexShrink,
        Self::FlexBasis,
        Self::JustifyContent,
        Self::AlignItems,
        Self::AlignSelf,
        Self::AlignContent,
        Self::RowGap,
        Self::ColumnGap,
        Self::Order,
        Self::GridTemplateColumns,
        Self::GridTemplateRows,
        Self::GridTemplateAreas,
        Self::GridAutoColumns,
        Self::GridAutoRows,
        Self::GridAutoFlow,
        Self::GridRowStart,
        Self::GridRowEnd,
        Self::GridColumnStart,
        Self::GridColumnEnd,
        Self::JustifyItems,
        Self::JustifySelf,
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
            Self::ListStylePosition => "list-style-position",
            Self::BorderCollapse => "border-collapse",
            Self::BorderSpacing => "border-spacing",
            Self::TableLayout => "table-layout",
            Self::FlexDirection => "flex-direction",
            Self::FlexWrap => "flex-wrap",
            Self::FlexGrow => "flex-grow",
            Self::FlexShrink => "flex-shrink",
            Self::FlexBasis => "flex-basis",
            Self::JustifyContent => "justify-content",
            Self::AlignItems => "align-items",
            Self::AlignSelf => "align-self",
            Self::AlignContent => "align-content",
            Self::RowGap => "row-gap",
            Self::ColumnGap => "column-gap",
            Self::Order => "order",
            Self::GridTemplateColumns => "grid-template-columns",
            Self::GridTemplateRows => "grid-template-rows",
            Self::GridTemplateAreas => "grid-template-areas",
            Self::GridAutoColumns => "grid-auto-columns",
            Self::GridAutoRows => "grid-auto-rows",
            Self::GridAutoFlow => "grid-auto-flow",
            Self::GridRowStart => "grid-row-start",
            Self::GridRowEnd => "grid-row-end",
            Self::GridColumnStart => "grid-column-start",
            Self::GridColumnEnd => "grid-column-end",
            Self::JustifyItems => "justify-items",
            Self::JustifySelf => "justify-self",
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
                | Self::ListStylePosition
                | Self::TextAlign
                | Self::LineHeight
                | Self::FontVariant
                | Self::TextIndent
                | Self::TextTransform
                | Self::Visibility
                | Self::LetterSpacing
                | Self::BorderCollapse
                | Self::BorderSpacing
        )
    }

    /// The first longhand of the rare group, which ends the roster.
    pub(crate) const FIRST_RARE: Self = Self::Clear;
    /// Its slot in the rare group; None for a common longhand.
    #[inline(always)]
    pub(crate) fn rare(self) -> Option<usize> {
        (self as usize).checked_sub(Self::FIRST_RARE as usize)
    }
    /// The longhands every style holds itself, in roster order.
    pub(crate) fn common_roster() -> &'static [Self] {
        Self::ALL
            .get(..Self::FIRST_RARE as usize)
            .unwrap_or_default()
    }
    /// The rare group's longhands, in slot order.
    pub(crate) fn rare_roster() -> &'static [Self] {
        Self::ALL
            .get(Self::FIRST_RARE as usize..)
            .unwrap_or_default()
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
    /// Lengths that take no percentage.
    fn no_percentage(self) -> bool {
        self.border() || matches!(self, Self::LetterSpacing | Self::BorderSpacing)
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
        if self == Self::FontFamily {
            let mut list = BudgetVec::new(memory)?;
            list.try_push(Family::Generic("serif"))?;
            return Ok(ComputedValue::FontFamily(BudgetShared::new(memory, list)?));
        }
        self.seed().ok_or(Error::InvalidContext)
    }
    /// The initial value of every property but font-family, which owns
    /// storage. Inlined, with no Result, into the cascade's path for an
    /// unset property, which every element takes for most properties: a
    /// call returning its Result through memory stalled there on store
    /// forwarding.
    #[inline(always)]
    pub(crate) fn seed(self) -> Option<ComputedValue> {
        Some(match self {
            Self::Display => ComputedValue::Keyword("inline"),
            Self::Color => ComputedValue::Color(Color::BLACK),
            Self::BackgroundColor => ComputedValue::Color(Color::TRANSPARENT),
            Self::FontFamily => return None,
            Self::FontSize => ComputedValue::Length(Length::Px(16.0)),
            Self::FontWeight => ComputedValue::Weight(400),
            Self::FontStyle | Self::WhiteSpace => ComputedValue::Keyword("normal"),
            Self::TextDecoration => ComputedValue::Decoration(Decoration::default()),
            Self::Width | Self::Height | Self::MinWidth | Self::MinHeight | Self::FlexBasis => {
                ComputedValue::Length(Length::Auto)
            }
            Self::ListStyleType => ComputedValue::Keyword("disc"),
            Self::ListStylePosition => ComputedValue::Keyword("outside"),
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
            Self::BorderCollapse => ComputedValue::Keyword("separate"),
            Self::BorderSpacing => ComputedValue::Spacing {
                horizontal: 0.0,
                vertical: 0.0,
            },
            Self::TableLayout
            | Self::AlignSelf
            | Self::GridAutoColumns
            | Self::GridAutoRows
            | Self::GridRowStart
            | Self::GridRowEnd
            | Self::GridColumnStart
            | Self::GridColumnEnd
            | Self::JustifySelf => ComputedValue::Keyword("auto"),
            Self::GridTemplateColumns | Self::GridTemplateRows | Self::GridTemplateAreas => {
                ComputedValue::Keyword("none")
            }
            Self::FlexDirection | Self::GridAutoFlow => ComputedValue::Keyword("row"),
            Self::FlexWrap => ComputedValue::Keyword("nowrap"),
            // `justify-items`' initial `legacy` computes to `normal` but
            // under a `legacy` position (`legacy_justify`).
            Self::JustifyContent
            | Self::JustifyItems
            | Self::AlignItems
            | Self::AlignContent
            | Self::RowGap
            | Self::ColumnGap => ComputedValue::Keyword("normal"),
            Self::FlexGrow | Self::Order => ComputedValue::Number(0.0),
            Self::Opacity | Self::FlexShrink => ComputedValue::Number(1.0),
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
    /// `grid-template-*` and `grid-auto-*` other than their keywords.
    GridTracks(BudgetShared<TrackList>),
    /// `grid-template-areas` other than `none`.
    GridAreas(BudgetShared<GridAreas>),
    /// A grid-placement longhand other than `auto`.
    GridLine(BudgetShared<GridLine>),
    Weight(u16),
    Decoration(Decoration),
    /// `border-spacing`: horizontal then vertical, nonnegative px.
    Spacing {
        horizontal: f64,
        vertical: f64,
    },
}

/// The rare group's length: the longhands from `Property::FIRST_RARE` on.
pub(crate) const RARE: usize = Property::ALL.len() - Property::FIRST_RARE as usize;
// `Rare::set` marks a slot a bit.
const _: () = assert!(RARE <= u64::BITS as usize);

/// The rare longhands' computed values, in roster order. None inherits,
/// so a style that sets none shares its style set's initial group, and
/// a declaration that sets one copies it (DESIGN.md, "Computed-style
/// sharing"). Its grid values share their storage with the copies made
/// of it. `set` marks the slots whose value is not the initial one, to
/// the bit, so fingerprints and comparisons visit those alone, and a
/// group's cost does not grow with the roster.
#[derive(Debug)]
pub(crate) struct Rare {
    pub(crate) values: [ComputedValue; RARE],
    pub(crate) set: u64,
}
impl Rare {
    pub(crate) fn initial() -> Result<Self, Error> {
        let mut values = [const { ComputedValue::Number(0.0) }; RARE];
        for (slot, property) in values.iter_mut().zip(Property::rare_roster()) {
            *slot = property.seed().ok_or(Error::InvalidContext)?;
        }
        Ok(Self { values, set: 0 })
    }
    pub(crate) fn try_copy(&self) -> Result<Self, Error> {
        let mut values = [const { ComputedValue::Number(0.0) }; RARE];
        for (slot, value) in values.iter_mut().zip(&self.values) {
            *slot = value.try_clone()?;
        }
        Ok(Self {
            values,
            set: self.set,
        })
    }
    /// Write one slot, marked unless its value is the initial one to the
    /// bit.
    pub(crate) fn write(&mut self, slot: usize, value: ComputedValue) -> Result<(), Error> {
        let initial = Property::rare_roster()
            .get(slot)
            .and_then(|p| p.seed())
            .ok_or(Error::InvalidContext)?;
        let bit = u32::try_from(slot)
            .ok()
            .and_then(|s| 1u64.checked_shl(s))
            .ok_or(Error::InvalidContext)?;
        if value.same(&initial) {
            self.set &= !bit;
        } else {
            self.set |= bit;
        }
        *self.values.get_mut(slot).ok_or(Error::InvalidContext)? = value;
        Ok(())
    }
    /// The marked slots of `set`, in order, with this group's values.
    pub(crate) fn slots(&self, set: u64) -> impl Iterator<Item = (usize, &ComputedValue)> {
        let mut bits = set;
        std::iter::from_fn(move || {
            while bits != 0 {
                let slot = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if let Some(value) = self.values.get(slot) {
                    return Some((slot, value));
                }
            }
            None
        })
    }
    /// Equal to the bit, so a group shared between styles never gives
    /// one of them another's signed zero; unmarked slots are equal.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.set == other.set
            && self
                .slots(self.set)
                .zip(other.slots(other.set))
                .all(|((_, a), (_, b))| a.same(b))
    }
}
/// As styles' `==`: -0 and 0 alike, so two groups apart by a signed zero
/// alone, which mark different slots, are equal.
impl PartialEq for Rare {
    fn eq(&self, other: &Self) -> bool {
        self.slots(self.set | other.set)
            .all(|(slot, value)| other.values.get(slot) == Some(value))
    }
}

#[derive(Debug)]
pub struct ComputedStyle {
    /// The common longhands, those before `Property::FIRST_RARE`.
    pub(crate) values: BudgetMap<Property, ComputedValue>,
    pub(crate) rare: BudgetShared<Rare>,
    pub(crate) custom: variables::CustomProperties,
}
impl PartialEq for ComputedStyle {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
            && (BudgetShared::ptr_eq(&self.rare, &other.rare) || *self.rare == *other.rare)
            && self.custom == other.custom
    }
}

impl ComputedStyle {
    pub fn new(memory: &Budget) -> Result<Self, Error> {
        let common = Property::common_roster();
        let mut values = td_alloc::BudgetMap::with_capacity(memory, common.len())?;
        for property in common {
            let value = if property.border() {
                ComputedValue::Length(Length::Px(0.0))
            } else {
                property.initial(memory)?
            };
            values.try_insert(*property, value)?;
        }
        Ok(Self {
            values,
            rare: BudgetShared::new(memory, Rare::initial()?)?,
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
            rare: self.rare.try_clone()?,
            custom: self.custom.try_clone()?,
        })
    }
    /// Replace one value. A rare one copies the group, which other styles
    /// may share, and writes the copy.
    pub(crate) fn set(&mut self, property: Property, value: ComputedValue) -> Result<(), Error> {
        let Some(slot) = property.rare() else {
            self.values.try_insert(property, value)?;
            return Ok(());
        };
        let mut rare = self.rare.try_copy()?;
        rare.write(slot, value)?;
        self.rare = BudgetShared::new(self.values.budget(), rare)?;
        Ok(())
    }
}
impl ComputedValue {
    /// Equal to the bit: unlike `==`, -0 and 0 differ.
    pub(crate) fn same(&self, other: &Self) -> bool {
        let bits = |a: f64, b: f64| a.to_bits() == b.to_bits();
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => bits(*a, *b),
            (Self::Length(Length::Px(a)), Self::Length(Length::Px(b)))
            | (Self::Length(Length::Percent(a)), Self::Length(Length::Percent(b))) => bits(*a, *b),
            (
                Self::Length(Length::Calc {
                    px: a,
                    percent: p,
                    non_negative: n,
                }),
                Self::Length(Length::Calc {
                    px: b,
                    percent: q,
                    non_negative: m,
                }),
            ) => bits(*a, *b) && bits(*p, *q) && n == m,
            (
                Self::Spacing {
                    horizontal: a,
                    vertical: b,
                },
                Self::Spacing {
                    horizontal: c,
                    vertical: d,
                },
            ) => bits(*a, *c) && bits(*b, *d),
            (Self::GridTracks(a), Self::GridTracks(b)) => BudgetShared::ptr_eq(a, b) || a.same(b),
            _ => self == other,
        }
    }
    /// A copy of any value that owns no storage: all but font-family's.
    #[inline(always)]
    fn plain(&self) -> Option<Self> {
        Some(match self {
            Self::Number(v) => Self::Number(*v),
            Self::Keyword(v) => Self::Keyword(v),
            Self::Color(v) => Self::Color(*v),
            Self::Length(v) => Self::Length(*v),
            Self::FontFamily(_) | Self::GridTracks(_) | Self::GridAreas(_) | Self::GridLine(_) => {
                return None
            }
            Self::Weight(v) => Self::Weight(*v),
            Self::Decoration(v) => Self::Decoration(*v),
            Self::Spacing {
                horizontal,
                vertical,
            } => Self::Spacing {
                horizontal: *horizontal,
                vertical: *vertical,
            },
        })
    }
    /// A plain value copied inline; one owning storage shares it.
    #[inline(always)]
    pub fn try_clone(&self) -> Result<Self, Error> {
        match self.plain() {
            Some(value) => Ok(value),
            None => self.share(),
        }
    }
    #[inline(never)]
    fn share(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::FontFamily(v) => Self::FontFamily(v.try_clone()?),
            Self::GridTracks(v) => Self::GridTracks(v.try_clone()?),
            Self::GridAreas(v) => Self::GridAreas(v.try_clone()?),
            Self::GridLine(v) => Self::GridLine(v.try_clone()?),
            plain => plain.plain().ok_or(Error::InvalidContext)?,
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
            Self::Spacing(axes) => Self::Spacing(axes.try_clone()?),
            Self::Tracks(list) => Self::Tracks(list.try_clone()?),
        })
    }
}

impl ComputedStyle {
    /// The inherited, substituted token value; None is missing or invalid.
    pub fn custom_property(&self, name: &str) -> Option<&[Component]> {
        self.custom.get(name)
    }

    pub fn get(&self, property: Property) -> Option<&ComputedValue> {
        if let Some(slot) = property.rare() {
            return self.rare.values.get(slot);
        }
        // Completed styles hold every common longhand, in roster order.
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
    /// `border-spacing`'s two axes, each a length or calc(), shared so
    /// every other value stays as small as it was.
    Spacing(BudgetShared<(Axis, Axis)>),
    /// A track list whose lengths need the element's font sizes; one
    /// that needs none is computed as parsed, `Ready`.
    Tracks(BudgetShared<TrackList<Axis>>),
}
#[derive(Clone, Copy, Debug)]
pub(crate) enum Axis {
    Length(f64, Unit),
    Calculated(calc::Linear),
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
    pub(crate) fn as_slice(&self) -> &[ValueDeclaration] {
        match self {
            Self::Single(value) => value.as_slice(),
            Self::Many(values) => values,
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
    let lowered = syntax::lower_text(memory, name)?;
    // Grid 1's gap names, aliases (Box Alignment 3 §8.4).
    let name = match lowered.as_str() {
        "grid-gap" => "gap",
        "grid-row-gap" => "row-gap",
        "grid-column-gap" => "column-gap",
        name => name,
    };
    let values = syntax::trim(values);
    let sides = match name {
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
        let targets = sides.or_else(|| layout::targets(name));
        let pending = variables::Pending {
            name: lowered.try_clone()?,
            tokens: BudgetShared::new(memory, syntax::copy_components(values, memory)?)?,
        };
        for property in Property::ALL
            .iter()
            .copied()
            .filter(|p| targets.map_or_else(|| p.name() == name, |sides| sides.contains(p)))
        {
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
    if layout::targets(name).is_some() {
        return layout::shorthand(name, values, important, memory).map(Declarations::Many);
    }
    let Some(property) = Property::ALL.iter().copied().find(|p| p.name() == name) else {
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

/// A pending value substituted and reparsed; None for any other value,
/// which the caller computes as it is, uncopied. `grid_items` counts what
/// an areas grammar reparses, where the substitution counts a string as
/// one component and a 64th of its bytes: each string byte and named
/// cell, and AREA_ITEMS for each area it builds.
pub(crate) fn resolve_pending(
    property: Property,
    value: &Specified,
    custom: &variables::CustomProperties,
    budget: &mut variables::Budget,
    grid_items: &mut usize,
) -> Result<Option<Specified>, Error> {
    let Specified::Pending(pending) = value else {
        return Ok(None);
    };
    let Some(tokens) = variables::substitute(&pending.tokens, custom, budget)? else {
        return Ok(Some(Specified::Unset));
    };
    let areas = matches!(
        pending.name.as_str(),
        "grid-template-areas" | "grid-template" | "grid"
    );
    if areas {
        for c in tokens.iter() {
            if let Component::Token(Token::String(text)) = c {
                let cells = grid::named_cells(text);
                *grid_items = grid_items.saturating_add(text.len()).saturating_add(cells);
            }
        }
    }
    let declarations = parse_named(&pending.name, &tokens, false, &budget.memory)?;
    if areas {
        for d in declarations.as_slice() {
            if let Specified::Ready(ComputedValue::GridAreas(built)) = &d.value {
                let units = built.areas().len().saturating_mul(grid::AREA_ITEMS);
                *grid_items = grid_items.saturating_add(units);
            }
        }
    }
    Ok(Some(
        declarations
            .into_iter()
            .find(|d| d.property == property)
            .map(|d| d.value)
            .unwrap_or(Specified::Unset),
    ))
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
                "grid",
            ],
        ),
        Color | BackgroundColor | BorderTopColor | BorderRightColor | BorderBottomColor
        | BorderLeftColor => parse_color(values)?,
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
        TextDecoration => {
            decoration(values).map(|v| Specified::Ready(ComputedValue::Decoration(v)))
        }
        FontWeight => weight(values),
        p if layout::extra(p) => layout::parse(p, values, memory)?,
        _ => parse_length(property, values)?,
    };
    Ok(result)
}

fn parse_length(property: Property, values: &[Component]) -> Result<Option<Specified>, Error> {
    if let [value @ Component::Function { .. }] = values {
        Ok(calc::parse(value, !property.no_percentage())?.map(Specified::Calculated))
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
                    | Property::MinWidth
                    | Property::MinHeight
                    | Property::FlexBasis
                    | Property::Top
                    | Property::Right
                    | Property::Bottom
                    | Property::Left
            ))
    {
        return None;
    }
    if matches!(unit, Unit::Percent) && property.no_percentage() {
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
    // The common case, a property no rule sets, without a Result between.
    if matches!(value, Specified::Unset) {
        let plain = if property.inherited() {
            parent
                .and_then(|style| style.get(property))
                .and_then(ComputedValue::plain)
        } else if property.border_color() {
            None
        } else {
            property.seed()
        };
        if let Some(value) = plain {
            return Ok(value);
        }
    }
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
        Specified::Spacing(axes) => spacing(axes, font_size, root_font_size),
        Specified::Tracks(list) => tracks(list, font_size, root_font_size, memory)?,
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
                _ => Length::Px(clamp(value * unit_px(*unit, em, root_font_size))),
            };
            ComputedValue::Length(length)
        }
    };
    Ok(result)
}
/// A font-relative track list computed, out of line as `spacing`.
#[inline(never)]
fn tracks(
    list: &TrackList<Axis>,
    em: f64,
    root_font_size: f64,
    memory: &Budget,
) -> Result<ComputedValue, Error> {
    Ok(ComputedValue::GridTracks(BudgetShared::new(
        memory,
        grid::compute(list, em, root_font_size, memory)?,
    )?))
}
/// `border-spacing` computed: out of compute_value's common path, which
/// every property of every element takes.
#[inline(never)]
fn spacing(axes: &(Axis, Axis), em: f64, root_font_size: f64) -> ComputedValue {
    let axis = |axis: &Axis| match axis {
        Axis::Length(value, unit) => {
            (value * unit_px(*unit, em, root_font_size)).clamp(0.0, MAX_ABSOLUTE_VALUE)
        }
        Axis::Calculated(value) => match value.compute(em, root_font_size, Some(0.0), true) {
            Length::Px(px) => px,
            _ => 0.0,
        },
    };
    ComputedValue::Spacing {
        horizontal: axis(&axes.0),
        vertical: axis(&axes.1),
    }
}
/// Px in one `unit`; a percentage is of `em`, as font-size's is.
fn unit_px(unit: Unit, em: f64, root_font_size: f64) -> f64 {
    match unit {
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
    }
}

pub(crate) fn finish(
    style: &mut ComputedStyle,
    parent: Option<&ComputedStyle>,
) -> Result<(), Error> {
    layout::finish(style, parent)
}

/// The parent's `justify-items` when it is a `legacy` with a position,
/// which a child's `legacy` takes (Box Alignment 3 §6.1); None, without
/// a read, when the parent shares the style set's initial group.
#[inline]
pub(crate) fn legacy_justify(
    parent: Option<&ComputedStyle>,
    initial: &BudgetShared<Rare>,
) -> Option<&'static str> {
    let parent = parent.filter(|p| !BudgetShared::ptr_eq(&p.rare, initial))?;
    match parent.rare.values.get(Property::JustifyItems.rare()?) {
        Some(ComputedValue::Keyword(word)) if word.starts_with("legacy ") => Some(word),
        _ => None,
    }
}
/// `justify-items` computed: `legacy` alone, `initial` and `unset` take
/// `legacy`, the parent's legacy position or else `normal`.
pub(crate) fn justify_items(
    specified: &Specified,
    value: ComputedValue,
    legacy: Option<&'static str>,
) -> ComputedValue {
    match (specified, &value) {
        (Specified::Initial | Specified::Unset, _) | (_, ComputedValue::Keyword("legacy")) => {
            ComputedValue::Keyword(legacy.unwrap_or("normal"))
        }
        _ => value,
    }
}

#[cfg(test)]
mod rare_tests;
