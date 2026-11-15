//! List items: HTML's ordinal values and CSS Counter Styles 3's marker
//! text for the predefined styles td-css computes. See DESIGN.md §5,
//! "List markers".
use crate::{BoxId, Error, StyleId};
use td_alloc::{Budget, BudgetVec, SmallText};
use td_html::document::{Element, NodeId};

/// The longest marker text: "MMMDCCCLXXXVIII. ", 3888 in upper-roman.
/// Decimal's longest is "-2147483648. " (13 bytes), alphabetic's seven
/// letters and the suffix, a bullet's three bytes and a space.
pub const MAX_MARKER_BYTES: usize = 17;
/// Roman numerals' range (CSS Counter Styles 3 §6); past it, decimal.
const ROMAN_MAX: i32 = 3999;
const ROMAN: &[(i32, &str, &str)] = &[
    (1000, "m", "M"),
    (900, "cm", "CM"),
    (500, "d", "D"),
    (400, "cd", "CD"),
    (100, "c", "C"),
    (90, "xc", "XC"),
    (50, "l", "L"),
    (40, "xl", "XL"),
    (10, "x", "X"),
    (9, "ix", "IX"),
    (5, "v", "V"),
    (4, "iv", "IV"),
    (1, "i", "I"),
];

/// A list-style-type td-css computes, but `none`, which makes no marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    Disc,
    Circle,
    Square,
    Decimal,
    DecimalLeadingZero,
    LowerRoman,
    UpperRoman,
    LowerAlpha,
    UpperAlpha,
}
impl MarkerKind {
    /// The computed list-style-type; None for `none` or an unknown word.
    pub fn from_keyword(word: &str) -> Option<Self> {
        Some(match word {
            "disc" => Self::Disc,
            "circle" => Self::Circle,
            "square" => Self::Square,
            "decimal" => Self::Decimal,
            "decimal-leading-zero" => Self::DecimalLeadingZero,
            "lower-roman" => Self::LowerRoman,
            "upper-roman" => Self::UpperRoman,
            "lower-alpha" | "lower-latin" => Self::LowerAlpha,
            "upper-alpha" | "upper-latin" => Self::UpperAlpha,
            _ => return None,
        })
    }
    /// A bullet's text is the same whatever the ordinal.
    pub fn bullet(self) -> bool {
        matches!(self, Self::Disc | Self::Circle | Self::Square)
    }
    /// The marker string (CSS Lists 3 §3.2): the counter representation
    /// and its suffix, ". " for counters and " " after a bullet. A value
    /// outside a style's range takes decimal, its fallback.
    pub fn text(self, ordinal: i32) -> SmallText<MAX_MARKER_BYTES> {
        let mut out = SmallText::new();
        if self.write(ordinal, &mut out).is_none() {
            // Unreachable within MAX_MARKER_BYTES, which tests hold at
            // every style's extremes; an empty marker rather than a panic.
            return SmallText::new();
        }
        out
    }
    fn write(self, ordinal: i32, out: &mut SmallText<MAX_MARKER_BYTES>) -> Option<()> {
        let push = |out: &mut SmallText<MAX_MARKER_BYTES>, s: &str| out.try_push_str(s).ok();
        let bullet = match self {
            Self::Disc => Some("\u{2022} "),
            Self::Circle => Some("\u{25e6} "),
            Self::Square => Some("\u{25aa} "),
            _ => None,
        };
        if let Some(bullet) = bullet {
            return push(out, bullet);
        }
        let value = i64::from(ordinal);
        match self {
            Self::LowerRoman | Self::UpperRoman if (1..=ROMAN_MAX).contains(&ordinal) => {
                let mut rest = ordinal;
                for (step, lower, upper) in ROMAN {
                    while rest >= *step {
                        push(
                            out,
                            if self == Self::LowerRoman {
                                lower
                            } else {
                                upper
                            },
                        )?;
                        rest -= step;
                    }
                }
            }
            Self::LowerAlpha | Self::UpperAlpha if value >= 1 => {
                // Bijective base 26: at most seven letters for i32::MAX.
                let mut letters = [0u8; 7];
                let mut at = letters.len();
                let mut rest = value;
                let base = if self == Self::LowerAlpha { b'a' } else { b'A' };
                while rest > 0 {
                    rest -= 1;
                    at = at.checked_sub(1)?;
                    *letters.get_mut(at)? = base + u8::try_from(rest % 26).ok()?;
                    rest /= 26;
                }
                push(out, std::str::from_utf8(letters.get(at..)?).ok()?)?;
            }
            _ => {
                // decimal; decimal-leading-zero pads to two symbols, the
                // negative sign counting (Counter Styles 3, "pad").
                let mut digits = [0u8; 10];
                let mut at = digits.len();
                let mut rest = value.unsigned_abs();
                loop {
                    at = at.checked_sub(1)?;
                    *digits.get_mut(at)? = b'0' + u8::try_from(rest % 10).ok()?;
                    rest /= 10;
                    if rest == 0 {
                        break;
                    }
                }
                if value < 0 {
                    push(out, "-")?;
                }
                let len = digits.len() - at + usize::from(value < 0);
                if self == Self::DecimalLeadingZero && len < 2 {
                    push(out, "0")?;
                }
                push(out, std::str::from_utf8(digits.get(at..)?).ok()?)?;
            }
        }
        push(out, ". ")
    }
}

/// HTML's "rules for parsing integers": leading ASCII whitespace, a sign,
/// then digits up to the first non-digit; none is an error. The value is
/// clamped to i32, the range td keeps a counter in: CSS Lists 3 §4
/// clamps a counter at its UA's range rather than overflowing it.
pub(crate) fn parse_integer(text: &str) -> Option<i32> {
    let text = text.trim_start_matches(['\t', '\n', '\x0c', '\r', ' ']);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, text.get(1..)?),
        Some(b'+') => (false, text.get(1..)?),
        _ => (false, text),
    };
    let mut value: i64 = 0;
    let mut any = false;
    for byte in digits.bytes() {
        if !byte.is_ascii_digit() {
            break;
        }
        any = true;
        // Past i32 already: further digits change nothing once clamped.
        if value <= i64::from(u32::MAX) {
            value = value * 10 + i64::from(byte - b'0');
        }
    }
    any.then(|| clamp(if negative { -value } else { value }))
}
fn clamp(value: i64) -> i32 {
    // Within i32 after the clamp, so the conversion cannot fail.
    i32::try_from(value.clamp(i64::from(i32::MIN), i64::from(i32::MAX))).unwrap_or(0)
}

/// One list item's ordinal value, by box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListItem {
    pub id: BoxId,
    pub ordinal: i32,
}

#[derive(Clone, Copy)]
enum Next {
    At(i32),
    /// A reversed `ol` without a start: its count of owned `li` less this.
    FromCount(i32),
}
struct Owner {
    node: NodeId,
    /// An `ol`, `ul` or `menu`; else the parent of items with none above.
    explicit: bool,
    reversed: bool,
    next: Next,
    li: i32,
    /// Where this owner's items awaiting its count start in `deferred`.
    mark: usize,
}

/// HTML's list owners and ordinal values, kept during box generation in
/// tree order: an owner opens when its element's box is made and closes
/// when its subtree is done.
pub(crate) struct Numbering {
    owners: BudgetVec<Owner>,
    /// (index in items, items before it): a reversed `ol` without a start
    /// numbers from its count of owned `li`, known once it closes.
    deferred: BudgetVec<(usize, i32)>,
    items: BudgetVec<ListItem>,
}
impl Numbering {
    pub fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            owners: BudgetVec::new(memory)?,
            deferred: BudgetVec::new(memory)?,
            items: BudgetVec::new(memory)?,
        })
    }
    /// A list item's box `id`, in tree order: assign its ordinal (HTML
    /// §4.4.8, "ordinal value"). `charge` admits the record's growth.
    #[inline(never)]
    pub fn item(
        &mut self,
        id: BoxId,
        parent: Option<NodeId>,
        element: &Element,
        charge: &mut dyn FnMut(usize) -> Result<(), Error>,
    ) -> Result<(), Error> {
        // The list owner: the closest ol, ul or menu ancestor, else the
        // parent. Explicit owners lie above every implicit one.
        let explicit = self.owners.last().is_some_and(|o| o.explicit);
        if !explicit && !self.owners.last().is_some_and(|o| Some(o.node) == parent) {
            if let Some(node) = parent {
                self.owners.try_push(Owner {
                    node,
                    explicit: false,
                    reversed: false,
                    next: Next::At(1),
                    li: 0,
                    mark: self.deferred.len(),
                })?;
            }
        }
        let li = element.name.as_str() == "li";
        let value = if li {
            element.attribute("value").and_then(parse_integer)
        } else {
            None
        };
        let index = self.items.len();
        let ordinal = match self.owners.last_mut() {
            Some(owner) => {
                if let Some(value) = value {
                    owner.next = Next::At(value);
                }
                let step: i32 = if owner.reversed { -1 } else { 1 };
                let ordinal = match owner.next {
                    Next::At(n) => {
                        owner.next = Next::At(n.saturating_add(step));
                        n
                    }
                    Next::FromCount(k) => {
                        owner.next = Next::FromCount(k.saturating_add(1));
                        self.deferred.try_push((index, k))?;
                        0
                    }
                };
                owner.li = owner.li.saturating_add(i32::from(li));
                ordinal
            }
            None => 1,
        };
        if self.items.len() == self.items.capacity() {
            let added = self.items.capacity().max(4);
            charge(
                added
                    .checked_mul(2 * std::mem::size_of::<ListItem>())
                    .and_then(|n| n.checked_add(32))
                    .ok_or(Error::Limit("MAX_PAGE_BYTES"))?,
            )?;
            BudgetVec::try_reserve_exact(&mut self.items, added)?;
        }
        self.items.try_push(ListItem { id, ordinal })?;
        Ok(())
    }
    /// An element's box was made: an ol, ul or menu opens an owner.
    #[inline]
    pub fn enter(&mut self, node: NodeId, element: &Element) -> Result<(), Error> {
        match element.name.as_str() {
            "ol" | "ul" | "menu" => self.open(node, element),
            _ => Ok(()),
        }
    }
    #[inline(never)]
    fn open(&mut self, node: NodeId, element: &Element) -> Result<(), Error> {
        let ol = element.name.as_str() == "ol";
        let reversed = ol && element.attribute("reversed").is_some();
        // The ol's starting value (HTML §4.4.5).
        let next = match ol.then(|| element.attribute("start").and_then(parse_integer)) {
            Some(Some(start)) => Next::At(start),
            _ if reversed => Next::FromCount(0),
            _ => Next::At(1),
        };
        self.owners.try_push(Owner {
            node,
            explicit: true,
            reversed,
            next,
            li: 0,
            mark: self.deferred.len(),
        })?;
        Ok(())
    }
    /// `node`'s subtree is done: close the owners it opened.
    #[inline]
    pub fn leave(&mut self, node: NodeId) -> Result<(), Error> {
        while self.owners.last().is_some_and(|o| o.node == node) {
            self.close()?;
        }
        Ok(())
    }
    /// Close every owner still open; the walk ended.
    pub fn finish(mut self) -> Result<BudgetVec<ListItem>, Error> {
        while !self.owners.is_empty() {
            self.close()?;
        }
        Ok(self.items)
    }
    #[inline(never)]
    fn close(&mut self) -> Result<(), Error> {
        let owner = self.owners.pop().ok_or(Error::Invalid)?;
        let pending = self.deferred.get(owner.mark..).ok_or(Error::Invalid)?;
        for (index, before) in pending {
            let item = self.items.get_mut(*index).ok_or(Error::Invalid)?;
            item.ordinal = clamp(i64::from(owner.li) - i64::from(*before));
        }
        self.deferred.truncate(owner.mark);
        Ok(())
    }
}

/// A list item's marker, generated text with no DOM bytes: it names its
/// item, not a text node, and no selection, find or byte range maps to it.
/// `x`, `y` and `baseline` are absolute CSS px, like a line's fragments.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MarkerFragment {
    /// The list item element.
    pub item: NodeId,
    pub ordinal: i32,
    pub kind: MarkerKind,
    /// Laid inline at the start of the item's first line, rather than
    /// hanging outside it.
    pub inside: bool,
    /// The item's style, which the marker is drawn in.
    pub style: StyleId,
    /// Its line's index in Layout::lines; None when the item laid no line,
    /// the marker then standing at the top of the item's content box.
    pub line: Option<u32>,
    pub x: f64,
    /// The top of the marker's own line box, its font's height.
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Absolute, as Line::y plus Line::baseline is.
    pub baseline: f64,
}
impl MarkerFragment {
    /// What it shows: kind.text(ordinal).
    pub fn text(&self) -> SmallText<MAX_MARKER_BYTES> {
        self.kind.text(self.ordinal)
    }
}

#[cfg(test)]
mod tests;
