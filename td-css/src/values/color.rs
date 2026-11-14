//! sRGB colours from CSS Color 4 and the colour of the CSS Backgrounds 3
//! `background` shorthand. See DESIGN.md §4.
use super::{number, Color, ComputedValue, Specified, MAX_ABSOLUTE_VALUE};
use crate::calc::{self, Scalar};
use crate::syntax::{self, Component, Token};
use crate::Error;

/// CSS Color 4 §6.1, sorted by name for binary search.
const NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];
const LONGEST_NAME: usize = 20;

fn named(name: &str) -> Option<Color> {
    if name.len() > LONGEST_NAME {
        return None;
    }
    let index = NAMED
        .binary_search_by(|(entry, _)| {
            let lower = name.bytes().map(|b| b.to_ascii_lowercase());
            entry.bytes().cmp(lower)
        })
        .ok()?;
    let (_, value) = NAMED.get(index)?;
    let [_, red, green, blue] = value.to_be_bytes();
    Some(Color {
        red,
        green,
        blue,
        alpha: 255,
    })
}

/// One color value; None is any other value.
pub(super) fn parse_color(values: &[Component]) -> Result<Option<Specified>, Error> {
    let [value] = values else {
        return Ok(None);
    };
    let color = match value {
        Component::Token(Token::Ident(name)) if name.eq_ignore_ascii_case("currentcolor") => {
            return Ok(Some(Specified::CurrentColor));
        }
        Component::Token(Token::Ident(name)) if name.eq_ignore_ascii_case("transparent") => {
            Some(Color::TRANSPARENT)
        }
        Component::Token(Token::Ident(name)) => named(name),
        Component::Token(Token::Hash { value, .. }) => hex(value),
        Component::Function { name, values } => function(name, values)?,
        _ => None,
    };
    Ok(color.map(|color| Specified::Ready(ComputedValue::Color(color))))
}

fn hex(value: &str) -> Option<Color> {
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
    Some(Color {
        red,
        green,
        blue,
        alpha,
    })
}

/// One argument of rgb()/hsl(). An angle is already in degrees.
#[derive(Clone, Copy)]
enum Part {
    Number(f64),
    Percent(f64),
    Angle(f64),
    None,
}

fn part(value: &Component) -> Result<Option<Part>, Error> {
    Ok(match value {
        Component::Token(Token::Number(n)) => number(&n.representation).map(Part::Number),
        Component::Token(Token::Percentage(n)) => number(&n.representation).map(Part::Percent),
        Component::Token(Token::Dimension(n, unit)) => {
            let Some(n) = number(&n.representation) else {
                return Ok(None);
            };
            // Reduce whole turns first so a large finite angle stays finite.
            let degrees = match syntax::keyword(unit, &["deg", "grad", "rad", "turn"]) {
                Some("deg") => n % 360.0,
                Some("grad") => n % 400.0 * 0.9,
                Some("rad") => (n % std::f64::consts::TAU).to_degrees(),
                Some("turn") => n % 1.0 * 360.0,
                _ => return Ok(None),
            };
            Some(Part::Angle(degrees))
        }
        Component::Token(Token::Ident(name)) if name.eq_ignore_ascii_case("none") => {
            Some(Part::None)
        }
        Component::Function { .. } => match calc::scalar(value)? {
            Some(Scalar::Number(n)) => Some(Part::Number(n)),
            Some(Scalar::Percent(n)) => Some(Part::Percent(n)),
            None => None,
        },
        _ => None,
    })
}

/// Three channels, separators and alpha; a value with more significant
/// components is invalid without the rest being inspected.
const MAX_COLOR_ARGUMENTS: usize = 7;

/// CSS Color 4 §5.1 rgb()/rgba() and §7 hsl()/hsla(), in the legacy comma
/// syntax (§4.1.2) or the modern space syntax; None is an invalid value.
fn function(name: &str, values: &[Component]) -> Result<Option<Color>, Error> {
    let hsl = match syntax::keyword(name, &["rgb", "rgba", "hsl", "hsla"]) {
        Some("rgb" | "rgba") => false,
        Some(_) => true,
        None => return Ok(None),
    };
    let mut slots: [Option<&Component>; MAX_COLOR_ARGUMENTS] = [None; MAX_COLOR_ARGUMENTS];
    let mut count = 0;
    for value in values.iter().filter(|v| !v.whitespace()) {
        let Some(slot) = slots.get_mut(count) else {
            return Ok(None);
        };
        *slot = Some(value);
        count += 1;
    }
    let legacy = slots.iter().flatten().any(|v| v.delim(','));
    let (channels_at, separators_at, alpha_at, separator) = if legacy {
        ([0, 2, 4], [1, 3, 5], 6, ',')
    } else {
        ([0, 1, 2], [3, 3, 3], 4, '/')
    };
    let has_alpha = count == alpha_at + 1;
    if !has_alpha && count != alpha_at - 1 {
        return Ok(None);
    }
    let at = |index: usize| slots.get(index).copied().flatten();
    for index in separators_at.into_iter().filter(|index| *index < count) {
        if !at(index).is_some_and(|v| v.delim(separator)) {
            return Ok(None);
        }
    }
    let mut parts = [Part::None; 3];
    for (out, index) in parts.iter_mut().zip(channels_at) {
        let Some(value) = at(index) else {
            return Ok(None);
        };
        let Some(found) = part(value)? else {
            return Ok(None);
        };
        *out = found;
    }
    let alpha = if has_alpha {
        let Some(value) = at(alpha_at) else {
            return Ok(None);
        };
        match part(value)? {
            Some(Part::Number(n)) => n,
            Some(Part::Percent(n)) => n / 100.0,
            Some(Part::None) if !legacy => 0.0,
            _ => return Ok(None),
        }
    } else {
        1.0
    };
    let [first, second, third] = parts;
    let rgb = if hsl {
        let hue = match first {
            Part::Number(n) | Part::Angle(n) => n,
            Part::None if !legacy => 0.0,
            _ => return Ok(None),
        };
        let mut sl = [0.0; 2];
        for (out, value) in sl.iter_mut().zip([second, third]) {
            *out = match value {
                Part::Percent(n) => n,
                Part::Number(n) if !legacy => n,
                Part::None if !legacy => 0.0,
                _ => return Ok(None),
            };
        }
        let [saturation, lightness] = sl;
        hsl_to_rgb(hue, saturation, lightness).map(|n| n * 255.0)
    } else {
        // Legacy channels are all numbers or all percentages (§4.1.2).
        if legacy
            && !(parts.iter().all(|p| matches!(p, Part::Number(_)))
                || parts.iter().all(|p| matches!(p, Part::Percent(_))))
        {
            return Ok(None);
        }
        let mut rgb = [0.0; 3];
        for (out, value) in rgb.iter_mut().zip(parts) {
            *out = match value {
                Part::Number(n) => n,
                Part::Percent(n) => n * 255.0 / 100.0,
                Part::None if !legacy => 0.0,
                _ => return Ok(None),
            };
        }
        rgb
    };
    let [red, green, blue] = rgb.map(byte);
    Ok(Some(Color {
        red,
        green,
        blue,
        alpha: byte(alpha * 255.0),
    }))
}

/// Clamps a channel to [0, 255] and rounds it to the nearest integer,
/// halves toward +∞ (CSS Color 4 §5.1).
fn byte(n: f64) -> u8 {
    // number() and calc::scalar() admit only finite input, so n is never
    // NaN (an infinite channel product clamps); at most 255.5 floors to
    // 255, which the cast keeps.
    (n.clamp(0.0, 255.0) + 0.5).floor() as u8
}

/// CSS Color 4 §7.1. Negative saturation clamps to zero (§7); other
/// magnitudes clamp to MAX_ABSOLUTE_VALUE so the arithmetic stays finite,
/// and channels outside sRGB's gamut clamp in `byte`. Channels are [0, 1].
fn hsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> [f64; 3] {
    let mut hue = hue % 360.0;
    if hue < 0.0 {
        hue += 360.0;
    }
    let saturation = saturation.clamp(0.0, MAX_ABSOLUTE_VALUE) / 100.0;
    let lightness = lightness.clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE) / 100.0;
    let a = saturation * lightness.min(1.0 - lightness);
    let f = |n: f64| {
        let k = (n + hue / 30.0) % 12.0;
        lightness - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    [f(0.0), f(8.0), f(4.0)]
}

/// The image functions a background layer steps over, fetching nothing.
/// Gradients and image-set() are unsupported, so invalid: a page's earlier
/// fallback colour then survives the cascade.
const IMAGE_FUNCTIONS: &[&str] = &["url", "src"];
/// CSS Values 4 §6 and CSS Containment 3's container length units,
/// accepted in skipped position/size slots.
const LENGTH_UNITS: &[&str] = &[
    "px", "cm", "mm", "q", "in", "pt", "pc", "em", "rem", "ex", "rex", "cap", "rcap", "ch", "rch",
    "ic", "ric", "lh", "rlh", "vw", "svw", "lvw", "dvw", "vh", "svh", "lvh", "dvh", "vi", "svi",
    "lvi", "dvi", "vb", "svb", "lvb", "dvb", "vmin", "svmin", "lvmin", "dvmin", "vmax", "svmax",
    "lvmax", "dvmax", "cqw", "cqh", "cqi", "cqb", "cqmin", "cqmax",
];
/// Math functions a skipped slot steps over whole; the syntax layer already
/// balanced and bounded their contents.
const MATH_FUNCTIONS: &[&str] = &["calc", "min", "max", "clamp"];
const REPEATS: &[&str] = &["repeat", "space", "round", "no-repeat"];

fn image(value: &Component) -> bool {
    match value {
        Component::Token(Token::Url(_)) => true,
        Component::Function { name, .. } => syntax::keyword(name, IMAGE_FUNCTIONS).is_some(),
        _ => false,
    }
}

/// A <length-percentage> in a slot nothing stores: any CSS Values 4 length
/// unit, a percentage, bare zero, or a math function stepped over whole.
/// `nonnegative` refuses a negative token.
fn offset(value: &Component, nonnegative: bool) -> bool {
    let n = match value {
        Component::Token(Token::Number(n)) => {
            return number(&n.representation).is_some_and(|n| n == 0.0);
        }
        Component::Token(Token::Percentage(n)) => &n.representation,
        Component::Token(Token::Dimension(n, unit))
            if syntax::keyword(unit, LENGTH_UNITS).is_some() =>
        {
            &n.representation
        }
        Component::Function { name, .. } => {
            return syntax::keyword(name, MATH_FUNCTIONS).is_some();
        }
        _ => return false,
    };
    number(n).is_some_and(|n| !(nonnegative && n < 0.0))
}

#[derive(Clone, Copy, PartialEq)]
enum Place {
    Horizontal,
    Vertical,
    Center,
    Offset,
}

fn place(value: &Component) -> Option<Place> {
    if let Some(name) = value.ident() {
        return match syntax::keyword(name, &["left", "right", "top", "bottom", "center"]) {
            Some("left" | "right") => Some(Place::Horizontal),
            Some("top" | "bottom") => Some(Place::Vertical),
            Some(_) => Some(Place::Center),
            None => None,
        };
    }
    offset(value, false).then_some(Place::Offset)
}

/// CSS Backgrounds 3 §2.6 <bg-position> over one to four places.
fn position(places: &[Place]) -> bool {
    use Place::*;
    let x = |p: Place| matches!(p, Horizontal | Center);
    let y = |p: Place| matches!(p, Vertical | Center);
    match places {
        [_] => true,
        [a, b] => {
            (matches!(a, Horizontal | Center | Offset) && matches!(b, Vertical | Center | Offset))
                || (y(*a) && x(*b))
        }
        _ => {
            // [center | [left|right] <offset>?] && [center | [top|bottom] <offset>?]
            let mut groups = [Center; 2];
            let mut count = 0;
            let mut rest = places;
            while let Some((first, tail)) = rest.split_first() {
                if *first == Offset {
                    return false;
                }
                let Some(group) = groups.get_mut(count) else {
                    return false;
                };
                *group = *first;
                count += 1;
                rest = tail;
                if *first != Center {
                    if let Some((Offset, tail)) = rest.split_first() {
                        rest = tail;
                    }
                }
            }
            let [a, b] = groups;
            count == 2 && ((x(a) && y(b)) || (y(a) && x(b)))
        }
    }
}

/// One layer of CSS Backgrounds 3 §2.10 `background`. The outer None is
/// an invalid layer; the inner value is its colour, which only the final
/// layer may carry.
fn layer(values: &[Component], last: bool) -> Result<Option<Option<Specified>>, Error> {
    let mut items = values.iter().filter(|v| !v.whitespace()).peekable();
    let (mut image_seen, mut position_seen, mut repeat_seen, mut attachment_seen) =
        (false, false, false, false);
    let mut boxes = 0;
    let mut color = None;
    let mut empty = true;
    while let Some(value) = items.next() {
        empty = false;
        let word = value.ident().and_then(|name| {
            syntax::keyword(
                name,
                &[
                    "none",
                    "repeat-x",
                    "repeat-y",
                    "repeat",
                    "space",
                    "round",
                    "no-repeat",
                    "scroll",
                    "fixed",
                    "local",
                    "border-box",
                    "padding-box",
                    "content-box",
                ],
            )
        });
        let seen = match word {
            Some("none") => &mut image_seen,
            Some("repeat-x" | "repeat-y") => &mut repeat_seen,
            Some(word) if REPEATS.contains(&word) => {
                if items
                    .peek()
                    .and_then(|v| v.ident())
                    .is_some_and(|name| syntax::keyword(name, REPEATS).is_some())
                {
                    items.next();
                }
                &mut repeat_seen
            }
            Some("scroll" | "fixed" | "local") => &mut attachment_seen,
            Some(_) => {
                boxes += 1;
                if boxes > 2 {
                    return Ok(None);
                }
                continue;
            }
            None if image(value) => &mut image_seen,
            None => {
                if let Some(first) = place(value) {
                    let mut places = [first; 4];
                    let mut count = 1;
                    while let Some(next) = items.peek() {
                        let Some(next) = place(next) else {
                            break;
                        };
                        let Some(slot) = places.get_mut(count) else {
                            return Ok(None);
                        };
                        *slot = next;
                        count += 1;
                        items.next();
                    }
                    if !places.get(..count).is_some_and(position) {
                        return Ok(None);
                    }
                    if items.peek().is_some_and(|v| v.delim('/')) {
                        items.next();
                        if !size(&mut items) {
                            return Ok(None);
                        }
                    }
                    &mut position_seen
                } else {
                    let Some(found) = parse_color(std::slice::from_ref(value))? else {
                        return Ok(None);
                    };
                    if !last || color.is_some() {
                        return Ok(None);
                    }
                    color = Some(found);
                    continue;
                }
            }
        };
        if std::mem::replace(seen, true) {
            return Ok(None);
        }
    }
    Ok((!empty).then_some(color))
}

/// CSS Backgrounds 3 §2.9 <bg-size> after a position's slash.
fn size<'a>(items: &mut std::iter::Peekable<impl Iterator<Item = &'a Component>>) -> bool {
    let size_value = |value: &Component| {
        value
            .ident()
            .is_some_and(|n| n.eq_ignore_ascii_case("auto"))
            || offset(value, true)
    };
    let Some(first) = items.next() else {
        return false;
    };
    if first
        .ident()
        .is_some_and(|n| syntax::keyword(n, &["cover", "contain"]).is_some())
    {
        return true;
    }
    if !size_value(first) {
        return false;
    }
    if items.peek().is_some_and(|next| size_value(next)) {
        items.next();
    }
    true
}

/// The `background` shorthand's background-color: the final layer's
/// colour, else initial. None is an invalid declaration.
pub(super) fn background(values: &[Component]) -> Result<Option<Specified>, Error> {
    let mut layers = values.split(|v| v.delim(',')).peekable();
    let mut color = None;
    while let Some(values) = layers.next() {
        let last = layers.peek().is_none();
        let Some(found) = layer(values, last)? else {
            return Ok(None);
        };
        color = found;
    }
    Ok(Some(color.unwrap_or(Specified::Initial)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use crate::conditions::supports_matches;
    use crate::test_tree::{tree, Handle};
    use crate::values::{ComputedStyle, Property};
    use crate::{Origin, StyleSet};

    /// CSS Color 4 §6.1's table (CRD 2026-11-12), name and decimal columns,
    /// transcribed from the specification's markup rather than from `NAMED`.
    const SPEC_NAMED: &[(&str, u8, u8, u8)] = &[
        ("aliceblue", 240, 248, 255),
        ("antiquewhite", 250, 235, 215),
        ("aqua", 0, 255, 255),
        ("aquamarine", 127, 255, 212),
        ("azure", 240, 255, 255),
        ("beige", 245, 245, 220),
        ("bisque", 255, 228, 196),
        ("black", 0, 0, 0),
        ("blanchedalmond", 255, 235, 205),
        ("blue", 0, 0, 255),
        ("blueviolet", 138, 43, 226),
        ("brown", 165, 42, 42),
        ("burlywood", 222, 184, 135),
        ("cadetblue", 95, 158, 160),
        ("chartreuse", 127, 255, 0),
        ("chocolate", 210, 105, 30),
        ("coral", 255, 127, 80),
        ("cornflowerblue", 100, 149, 237),
        ("cornsilk", 255, 248, 220),
        ("crimson", 220, 20, 60),
        ("cyan", 0, 255, 255),
        ("darkblue", 0, 0, 139),
        ("darkcyan", 0, 139, 139),
        ("darkgoldenrod", 184, 134, 11),
        ("darkgray", 169, 169, 169),
        ("darkgreen", 0, 100, 0),
        ("darkgrey", 169, 169, 169),
        ("darkkhaki", 189, 183, 107),
        ("darkmagenta", 139, 0, 139),
        ("darkolivegreen", 85, 107, 47),
        ("darkorange", 255, 140, 0),
        ("darkorchid", 153, 50, 204),
        ("darkred", 139, 0, 0),
        ("darksalmon", 233, 150, 122),
        ("darkseagreen", 143, 188, 143),
        ("darkslateblue", 72, 61, 139),
        ("darkslategray", 47, 79, 79),
        ("darkslategrey", 47, 79, 79),
        ("darkturquoise", 0, 206, 209),
        ("darkviolet", 148, 0, 211),
        ("deeppink", 255, 20, 147),
        ("deepskyblue", 0, 191, 255),
        ("dimgray", 105, 105, 105),
        ("dimgrey", 105, 105, 105),
        ("dodgerblue", 30, 144, 255),
        ("firebrick", 178, 34, 34),
        ("floralwhite", 255, 250, 240),
        ("forestgreen", 34, 139, 34),
        ("fuchsia", 255, 0, 255),
        ("gainsboro", 220, 220, 220),
        ("ghostwhite", 248, 248, 255),
        ("gold", 255, 215, 0),
        ("goldenrod", 218, 165, 32),
        ("gray", 128, 128, 128),
        ("green", 0, 128, 0),
        ("greenyellow", 173, 255, 47),
        ("grey", 128, 128, 128),
        ("honeydew", 240, 255, 240),
        ("hotpink", 255, 105, 180),
        ("indianred", 205, 92, 92),
        ("indigo", 75, 0, 130),
        ("ivory", 255, 255, 240),
        ("khaki", 240, 230, 140),
        ("lavender", 230, 230, 250),
        ("lavenderblush", 255, 240, 245),
        ("lawngreen", 124, 252, 0),
        ("lemonchiffon", 255, 250, 205),
        ("lightblue", 173, 216, 230),
        ("lightcoral", 240, 128, 128),
        ("lightcyan", 224, 255, 255),
        ("lightgoldenrodyellow", 250, 250, 210),
        ("lightgray", 211, 211, 211),
        ("lightgreen", 144, 238, 144),
        ("lightgrey", 211, 211, 211),
        ("lightpink", 255, 182, 193),
        ("lightsalmon", 255, 160, 122),
        ("lightseagreen", 32, 178, 170),
        ("lightskyblue", 135, 206, 250),
        ("lightslategray", 119, 136, 153),
        ("lightslategrey", 119, 136, 153),
        ("lightsteelblue", 176, 196, 222),
        ("lightyellow", 255, 255, 224),
        ("lime", 0, 255, 0),
        ("limegreen", 50, 205, 50),
        ("linen", 250, 240, 230),
        ("magenta", 255, 0, 255),
        ("maroon", 128, 0, 0),
        ("mediumaquamarine", 102, 205, 170),
        ("mediumblue", 0, 0, 205),
        ("mediumorchid", 186, 85, 211),
        ("mediumpurple", 147, 112, 219),
        ("mediumseagreen", 60, 179, 113),
        ("mediumslateblue", 123, 104, 238),
        ("mediumspringgreen", 0, 250, 154),
        ("mediumturquoise", 72, 209, 204),
        ("mediumvioletred", 199, 21, 133),
        ("midnightblue", 25, 25, 112),
        ("mintcream", 245, 255, 250),
        ("mistyrose", 255, 228, 225),
        ("moccasin", 255, 228, 181),
        ("navajowhite", 255, 222, 173),
        ("navy", 0, 0, 128),
        ("oldlace", 253, 245, 230),
        ("olive", 128, 128, 0),
        ("olivedrab", 107, 142, 35),
        ("orange", 255, 165, 0),
        ("orangered", 255, 69, 0),
        ("orchid", 218, 112, 214),
        ("palegoldenrod", 238, 232, 170),
        ("palegreen", 152, 251, 152),
        ("paleturquoise", 175, 238, 238),
        ("palevioletred", 219, 112, 147),
        ("papayawhip", 255, 239, 213),
        ("peachpuff", 255, 218, 185),
        ("peru", 205, 133, 63),
        ("pink", 255, 192, 203),
        ("plum", 221, 160, 221),
        ("powderblue", 176, 224, 230),
        ("purple", 128, 0, 128),
        ("rebeccapurple", 102, 51, 153),
        ("red", 255, 0, 0),
        ("rosybrown", 188, 143, 143),
        ("royalblue", 65, 105, 225),
        ("saddlebrown", 139, 69, 19),
        ("salmon", 250, 128, 114),
        ("sandybrown", 244, 164, 96),
        ("seagreen", 46, 139, 87),
        ("seashell", 255, 245, 238),
        ("sienna", 160, 82, 45),
        ("silver", 192, 192, 192),
        ("skyblue", 135, 206, 235),
        ("slateblue", 106, 90, 205),
        ("slategray", 112, 128, 144),
        ("slategrey", 112, 128, 144),
        ("snow", 255, 250, 250),
        ("springgreen", 0, 255, 127),
        ("steelblue", 70, 130, 180),
        ("tan", 210, 180, 140),
        ("teal", 0, 128, 128),
        ("thistle", 216, 191, 216),
        ("tomato", 255, 99, 71),
        ("turquoise", 64, 224, 208),
        ("violet", 238, 130, 238),
        ("wheat", 245, 222, 179),
        ("white", 255, 255, 255),
        ("whitesmoke", 245, 245, 245),
        ("yellow", 255, 255, 0),
        ("yellowgreen", 154, 205, 50),
    ];

    fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> Color {
        Color {
            red,
            green,
            blue,
            alpha,
        }
    }

    fn style(css: &str, inline: &str, parent: Option<&ComputedStyle>) -> ComputedStyle {
        let nodes = tree();
        let initial = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        StyleSet::new(&[(Origin::Author, css)])
            .unwrap()
            .compute(
                &Handle {
                    nodes: &nodes,
                    index: 3,
                },
                Some(parent.unwrap_or(&initial)),
                16.0,
                inline,
            )
            .unwrap()
    }

    fn get(style: &ComputedStyle, property: Property) -> Color {
        match style.get(property) {
            Some(ComputedValue::Color(color)) => *color,
            other => panic!("{property:?} is {other:?}"),
        }
    }

    // An earlier valid declaration that an invalid one must not erase.
    const KEPT: Color = Color {
        red: 1,
        green: 2,
        blue: 3,
        alpha: 4,
    };

    /// `color` after `color:#01020304; color:VALUE`: KEPT when VALUE is invalid.
    fn color(value: &str) -> Color {
        get(
            &style("", &format!("color:#01020304;color:{value}"), None),
            Property::Color,
        )
    }

    /// `background-color` after a KEPT background-color and `background:VALUE`.
    fn background(value: &str) -> Color {
        get(
            &style(
                "",
                &format!("background-color:#01020304;background:{value}"),
                None,
            ),
            Property::BackgroundColor,
        )
    }

    #[test]
    fn every_named_color_matches_the_specification_table() {
        assert_eq!(NAMED.len(), 148);
        assert_eq!(SPEC_NAMED.len(), NAMED.len());
        assert!(
            NAMED.windows(2).all(|w| w[0].0 < w[1].0),
            "binary search order"
        );
        assert!(NAMED.iter().all(|(name, _)| name.len() <= LONGEST_NAME));
        for (name, red, green, blue) in SPEC_NAMED {
            let want = rgba(*red, *green, *blue, 255);
            assert_eq!(color(name), want, "{name}");
            assert_eq!(color(&name.to_ascii_uppercase()), want, "{name}");
        }
    }

    #[test]
    fn named_colors_spot_checks_and_case() {
        // CSS Color 4 §6.1: rebeccapurple is #663399; grey spellings alias gray.
        assert_eq!(color("rebeccapurple"), rgba(102, 51, 153, 255));
        assert_eq!(color("RebeccaPurple"), rgba(102, 51, 153, 255));
        assert_eq!(color("lightGoldenRodYellow"), rgba(250, 250, 210, 255));
        assert_eq!(color("darkslategrey"), color("darkslategray"));
        assert_eq!(color("green"), rgba(0, 128, 0, 255));
        assert_eq!(color("lime"), rgba(0, 255, 0, 255));
        for unknown in [
            "notacolor",
            "lightgoldenrodyellowx",
            "Canvas",
            "CanvasText",
            "ButtonFace",
            "greyish",
            "\"red\"",
            "a",
        ] {
            assert_eq!(color(unknown), KEPT, "{unknown}");
        }
    }

    #[test]
    fn transparent_and_currentcolor() {
        assert_eq!(color("transparent"), Color::TRANSPARENT);
        assert_eq!(color("TRANSPARENT"), Color::TRANSPARENT);
        let parent = style("", "color:rgb(0 0 255)", None);
        let child = style(
            "",
            "color:currentColor;background-color:currentcolor;border-top-color:currentcolor",
            Some(&parent),
        );
        // color: currentcolor is color: inherit (CSS Color 4 §6.4).
        assert_eq!(child.color(), rgba(0, 0, 255, 255));
        assert_eq!(get(&child, Property::BackgroundColor), rgba(0, 0, 255, 255));
        let own = style(
            "",
            "color:hsl(0 100% 50% / 50%);background:currentcolor",
            None,
        );
        assert_eq!(get(&own, Property::BackgroundColor), rgba(255, 0, 0, 128));
    }

    #[test]
    fn rgb_functions_in_legacy_and_modern_syntax() {
        for (input, want) in [
            ("rgb(255, 0, 0)", rgba(255, 0, 0, 255)),
            ("rgb(255,0,0)", rgba(255, 0, 0, 255)),
            ("RGBA(255 , 0 , 0 , 0.5)", rgba(255, 0, 0, 128)),
            ("rgba(1, 2, 3)", rgba(1, 2, 3, 255)),
            ("rgb(1, 2, 3, 40%)", rgba(1, 2, 3, 102)),
            ("rgb(100%, 50%, 0%)", rgba(255, 128, 0, 255)),
            ("rgba(0%, 0%, 100%, 1)", rgba(0, 0, 255, 255)),
            ("rgb(1 2 3)", rgba(1, 2, 3, 255)),
            ("rgb(1 2 3 / 50%)", rgba(1, 2, 3, 128)),
            ("rgb(1 2 3/.25)", rgba(1, 2, 3, 64)),
            ("rgba(1 2 3)", rgba(1, 2, 3, 255)),
            // Modern syntax mixes numbers and percentages, and none is zero.
            ("rgb(10% 20 none)", rgba(26, 20, 0, 255)),
            ("rgb(none none none / none)", Color::TRANSPARENT),
            ("rgb(1 2 3 / none)", rgba(1, 2, 3, 0)),
            // Rounding is to the nearest integer, halves toward +∞.
            ("rgb(0.5 1.49 254.5)", rgba(1, 1, 255, 255)),
            ("rgb(0.49 0 0 / 0.5)", rgba(0, 0, 0, 128)),
            ("rgb(0 0 0 / 0.501961)", rgba(0, 0, 0, 128)),
            // Out-of-range channels and alpha clamp (§5.1, §4.2).
            ("rgb(300 -20 127.5)", rgba(255, 0, 128, 255)),
            ("rgb(150%, -5%, 0%)", rgba(255, 0, 0, 255)),
            ("rgb(1 2 3 / 2)", rgba(1, 2, 3, 255)),
            ("rgb(1 2 3 / -1)", rgba(1, 2, 3, 0)),
            ("rgb(1 2 3 / 150%)", rgba(1, 2, 3, 255)),
            ("rgb(1e300 0 0)", rgba(255, 0, 0, 255)),
            // calc() resolving to a number or a percentage.
            (
                "rgb(calc(100 + 155) 0 calc(50% * 2))",
                rgba(255, 0, 255, 255),
            ),
            ("rgba(0, 0, 0, calc(1 / 2))", rgba(0, 0, 0, 128)),
            ("rgb(calc(50%), 0%, 0%)", rgba(128, 0, 0, 255)),
            ("rgb(calc(1000) 0 0 / calc(-1))", rgba(255, 0, 0, 0)),
        ] {
            assert_eq!(color(input), want, "{input}");
        }
    }

    #[test]
    fn hsl_functions_hues_and_clamping() {
        for (input, want) in [
            ("hsl(0, 100%, 50%)", rgba(255, 0, 0, 255)),
            ("hsla(120, 100%, 50%, 0.25)", rgba(0, 255, 0, 64)),
            ("hsl(120deg 100% 25%)", rgba(0, 128, 0, 255)),
            ("HSL(240 100 50)", rgba(0, 0, 255, 255)),
            ("hsl(240 100% 50 / 50%)", rgba(0, 0, 255, 128)),
            ("hsla(240 100% 50%)", rgba(0, 0, 255, 255)),
            // Hue units: deg, grad, rad and turn; numbers are degrees.
            ("hsl(0.5turn 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(200grad 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(3.141592653589793rad 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(180DEG, 100%, 50%)", rgba(0, 255, 255, 255)),
            // Normalized into [0, 360).
            ("hsl(-120deg 100% 50%)", rgba(0, 0, 255, 255)),
            ("hsl(480 100% 50%)", rgba(0, 255, 0, 255)),
            ("hsl(-540 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(1e300deg 0% 50%)", rgba(128, 128, 128, 255)),
            ("hsl(1e300turn 0% 50%)", rgba(128, 128, 128, 255)),
            ("hsl(1e300rad 100% 0%)", rgba(0, 0, 0, 255)),
            // none is zero in modern syntax.
            ("hsl(none 0% 100% / none)", rgba(255, 255, 255, 0)),
            ("hsl(none 100% 50%)", rgba(255, 0, 0, 255)),
            // Negative saturation clamps to 0% (§7); out-of-gamut channels clamp.
            ("hsl(0 -50% 40%)", rgba(102, 102, 102, 255)),
            ("hsl(0 100% 150%)", rgba(255, 255, 255, 255)),
            ("hsl(0 100% -20%)", rgba(0, 0, 0, 255)),
            ("hsl(0 1e300% 50%)", rgba(255, 0, 0, 255)),
            ("hsl(0 100% 50% / 2)", rgba(255, 0, 0, 255)),
            (
                "hsl(calc(60 * 2) calc(50% + 50%) 25%)",
                rgba(0, 128, 0, 255),
            ),
        ] {
            assert_eq!(color(input), want, "{input}");
        }
    }

    #[test]
    fn invalid_color_functions_keep_the_cascaded_value() {
        for invalid in [
            // Legacy syntax: no mixing, no none, commas throughout.
            "rgb(1, 2%, 3)",
            "rgb(1%, 2, 3%)",
            "rgb(1, 2, none)",
            "rgba(1, 2, 3, none)",
            "rgb(1,2 3)",
            "rgb(1 2 3, 0.5)",
            "rgb(1, 2, 3 / 0.5)",
            "rgb(1,2,3,)",
            "rgb(,1,2,3)",
            "rgb(1,,2,3)",
            "hsl(0, 100, 50%)",
            "hsl(0, 100%, 50)",
            "hsl(none, 100%, 50%)",
            "hsl(0, 100%, 50%, none)",
            // Missing and extra arguments.
            "rgb()",
            "rgb(1 2)",
            "rgb(1, 2)",
            "rgb(1 2 3 4)",
            "rgb(1, 2, 3, 4, 5)",
            "rgb(1 2 3 /)",
            "rgb(1 2 3 / 0.5 / 1)",
            "rgb(1 2 / 3)",
            "rgb(/ 1 2 3)",
            "hsl(0deg, 50%)",
            // Wrong types.
            "rgb(10deg 0 0)",
            "rgb(1px 0 0)",
            "rgb(1 2 3 / 1deg)",
            "hsl(10px 50% 50%)",
            "hsl(0 50deg 50%)",
            "rgb(red 0 0)",
            "rgb(\"1\" 0 0)",
            "rgb(1e999 0 0)",
            // calc() only as a number or percentage; never a length or angle.
            "rgb(calc(10px) 0 0)",
            "rgb(calc(10% + 1px) 0 0)",
            "rgb(calc(10% + 0px) 0 0)",
            "rgb(calc(1 + 10%) 0 0)",
            "hsl(calc(10deg) 50% 50%)",
            "rgb(min(1, 2) 0 0)",
            // Unimplemented colour functions and syntaxes.
            "hwb(0 0% 0%)",
            "lab(50% 0 0)",
            "oklch(0.5 0.1 120)",
            "color(srgb 1 0 0)",
            "color-mix(in srgb, red, blue)",
            "rgb(from red r g b)",
            "rgb(1 2 3) red",
            "-webkit-rgb(1 2 3)",
        ] {
            assert_eq!(color(invalid), KEPT, "{invalid}");
        }
    }

    #[test]
    fn background_shorthand_sets_the_final_layer_color() {
        for (input, want) in [
            ("#fff", rgba(255, 255, 255, 255)),
            ("rgb(0 0 255 / 50%)", rgba(0, 0, 255, 128)),
            (
                "url(x) no-repeat center / cover #fff",
                rgba(255, 255, 255, 255),
            ),
            ("url(\"x.png\") red", rgba(255, 0, 0, 255)),
            ("red url(x)", rgba(255, 0, 0, 255)),
            (
                "url(a.png), src(\"b.png\") 0 0 / 10px auto, hsl(240 100% 50%)",
                rgba(0, 0, 255, 255),
            ),
            (
                "url(a.png) repeat-y, navy url(b.png) fixed",
                rgba(0, 0, 128, 255),
            ),
            (
                "url(x) 0 0 / 50% repeat-y scroll padding-box content-box red",
                rgba(255, 0, 0, 255),
            ),
            // CSS Backgrounds 3 §2.10's example.
            (
                "url(\"chess.png\") 40% / 10em gray round fixed border-box",
                rgba(128, 128, 128, 255),
            ),
            ("currentcolor", Color::BLACK),
            ("transparent", Color::TRANSPARENT),
            // A shorthand without a colour resets background-color.
            ("none", Color::TRANSPARENT),
            ("url(x)", Color::TRANSPARENT),
            ("url(x), url(y) no-repeat", Color::TRANSPARENT),
            ("0 0", Color::TRANSPARENT),
            ("left top", Color::TRANSPARENT),
            ("top left", Color::TRANSPARENT),
            ("center", Color::TRANSPARENT),
            ("10px top", Color::TRANSPARENT),
            ("right 10px bottom 20px", Color::TRANSPARENT),
            ("left 10px top", Color::TRANSPARENT),
            ("center bottom 5%", Color::TRANSPARENT),
            ("calc(10% + 2px) 0 / calc(5px * 2)", Color::TRANSPARENT),
            ("center / contain", Color::TRANSPARENT),
            ("repeat no-repeat", Color::TRANSPARENT),
            ("border-box padding-box", Color::TRANSPARENT),
            // A url layer keeps its colour.
            (
                "#333 url(hero.jpg) center/cover no-repeat",
                rgba(0x33, 0x33, 0x33, 255),
            ),
            // Skipped slots take any CSS Values 4 length unit, percentages
            // and calc()/min()/max()/clamp() stepped over whole.
            (
                "#333 url(hero.jpg) center / 100vw auto",
                rgba(0x33, 0x33, 0x33, 255),
            ),
            ("url(x) 0 1vh red", rgba(255, 0, 0, 255)),
            ("red url(x) 1ch 1ex", rgba(255, 0, 0, 255)),
            ("min(1px, 2px) red", rgba(255, 0, 0, 255)),
            ("clamp(1px, 2vw, 3px) 0 red", rgba(255, 0, 0, 255)),
            ("MAX(1em, 10%) 0 / CALC(1px) auto red", rgba(255, 0, 0, 255)),
            ("1DVH 2cqmin / 3rlh 4svmax red", rgba(255, 0, 0, 255)),
            ("left 1lh top 2ic / 1cap red", rgba(255, 0, 0, 255)),
            ("-5vw -1% red", rgba(255, 0, 0, 255)),
            ("initial", Color::TRANSPARENT),
            ("unset", Color::TRANSPARENT),
        ] {
            assert_eq!(background(input), want, "{input}");
        }
    }

    #[test]
    fn invalid_background_shorthands_keep_the_cascaded_value() {
        for invalid in [
            // A colour only in the final layer, at most once.
            "red, url(x)",
            "url(x) red, url(y)",
            "red blue",
            "red,",
            ", red",
            "",
            // Each component at most once.
            "url(x) url(y)",
            "none none",
            "none url(x)",
            "0 0 left",
            "no-repeat repeat-x",
            "repeat no-repeat repeat",
            "fixed scroll",
            "border-box padding-box content-box",
            // <bg-position> and <bg-size> grammar.
            "left right",
            "top bottom",
            "top 10px",
            "0 0 0 0 0",
            "center 10px top",
            "left 10px right 5px",
            "/ cover",
            "0 0 /",
            "0 0 / -5px",
            "0 0 / cover contain",
            "0 0 / red",
            "auto",
            "0 0 / -1vw red",
            "0 0 / -1% red",
            // Units and functions outside the length subset.
            "1deg 0 red",
            "1s red",
            "1x red",
            "1foo red",
            "1e999px red",
            "abs(1px) red",
            "env(safe-area-inset-top) red",
            "0 0 / attr(x) red",
            // Gradients and image-set() are unsupported.
            "linear-gradient(red, blue)",
            "linear-gradient(red, blue) red",
            "url(x), linear-gradient(red, blue) red",
            "radial-gradient(red, blue) red",
            "conic-gradient(garbage) red",
            "repeating-linear-gradient(red, blue 10%) red",
            "-webkit-linear-gradient(top, red, blue) red",
            "-webkit-gradient(linear, left top, left bottom, from(red), to(blue)) red",
            "image-set(url(x) 1x) red",
            // Anything else.
            "url(x) frobnicate",
            "url(x) 5",
            "element(#x)",
            "cross-fade(url(a), url(b))",
            "red inherit",
            "inherit red",
            "rgb(1, 2%, 3)",
        ] {
            assert_eq!(background(invalid), KEPT, "{invalid:?}");
        }
    }

    #[test]
    fn background_sets_only_background_color_and_fetches_nothing() {
        // td-css has no fetch path: a url() is only a token stepped over, and
        // the shorthand changes exactly what its colour longhand would.
        assert_eq!(
            style(
                "",
                "background:url(https://example.invalid/x.png) #fff",
                None
            ),
            style("", "background-color:#fff", None)
        );
        assert_eq!(
            style("", "background-color:red;background:url(x)", None),
            style("", "", None)
        );
        let parent = style("", "background-color:red", None);
        let child = style(
            "",
            "background-color:blue;background:inherit",
            Some(&parent),
        );
        assert_eq!(get(&child, Property::BackgroundColor), rgba(255, 0, 0, 255));
        let important = style("p{background:red!important}", "background:blue", None);
        assert_eq!(
            get(&important, Property::BackgroundColor),
            rgba(255, 0, 0, 255)
        );
        let border = style("", "border:1px solid rgb(0 0 255 / 50%)", None);
        assert_eq!(
            get(&border, Property::BorderLeftColor),
            rgba(0, 0, 255, 128)
        );
    }

    #[test]
    fn unsupported_gradient_keeps_the_author_fallback_colour() {
        // The usual fallback pattern: a gradient td-css cannot paint is an
        // invalid declaration, so the colour before it survives.
        let fallback = style(
            "p{background:#1e5799;background:linear-gradient(to bottom,#1e5799 0%,#7db9e8 100%)}",
            "",
            None,
        );
        assert_eq!(
            get(&fallback, Property::BackgroundColor),
            rgba(0x1e, 0x57, 0x99, 255)
        );
        // Unsupported even in the same declaration as its colour.
        assert_eq!(background("linear-gradient(red, blue) #fff"), KEPT);
        let url = style(
            "p{background:#333 url(hero.jpg) center/cover no-repeat}",
            "",
            None,
        );
        assert_eq!(
            get(&url, Property::BackgroundColor),
            rgba(0x33, 0x33, 0x33, 255)
        );
    }

    #[test]
    fn background_and_color_functions_take_var() {
        let vars = style(
            "",
            "--c:0 0 255;--bg:url(x) rgb(var(--c));color:rgb(var(--c) / 50%);background:var(--bg)",
            None,
        );
        assert_eq!(vars.color(), rgba(0, 0, 255, 128));
        assert_eq!(get(&vars, Property::BackgroundColor), rgba(0, 0, 255, 255));
        // Invalid after substitution: unset, not the earlier declaration.
        let invalid = style(
            "",
            "--c:red;background-color:blue;background:var(--c), url(x)",
            None,
        );
        assert_eq!(get(&invalid, Property::BackgroundColor), Color::TRANSPARENT);
    }

    #[test]
    fn supports_queries_the_color_and_background_grammar() {
        for (query, want) in [
            ("(color: rgb(1 2 3 / 50%))", true),
            ("(color: hsl(120deg 50% 50%))", true),
            ("(color: papayawhip)", true),
            ("(background: url(x) no-repeat red)", true),
            ("(color: rgb(1, 2%, 3))", false),
            ("(background: red, url(x))", false),
            ("(color: hwb(0 0% 0%))", false),
            ("(background: url(x) 1vw 2cqi / clamp(1px, 2px, 3px))", true),
            ("(background: conic-gradient(garbage))", false),
            ("(background: linear-gradient(red, blue))", false),
            ("(background: image-set(url(x) 1x))", false),
        ] {
            assert_eq!(supports_matches(query).unwrap(), want, "{query}");
        }
    }

    #[test]
    fn hostile_color_values_are_bounded() {
        // calc() nesting inside a colour obeys MAX_CALC_DEPTH.
        let depth = crate::calc::MAX_CALC_DEPTH + 1;
        let deep = format!(
            "p{{color:rgb({}1{} 0 0)}}",
            "calc(".repeat(depth),
            ")".repeat(depth)
        );
        assert_eq!(
            StyleSet::new(&[(Origin::Author, deep.as_str())]).err(),
            Some(Error::Limit("MAX_CALC_DEPTH"))
        );
        let shallow = format!(
            "rgb({}1{} 0 0)",
            "calc(".repeat(depth - 1),
            ")".repeat(depth - 1)
        );
        assert_eq!(color(&shallow), rgba(1, 0, 0, 255));
        // Function nesting past MAX_NESTING is refused by syntax.
        let nested = format!(
            "p{{color:{}1{}}}",
            "rgb(".repeat(crate::MAX_NESTING + 1),
            ")".repeat(crate::MAX_NESTING + 1)
        );
        assert!(matches!(
            StyleSet::new(&[(Origin::Author, nested.as_str())]),
            Err(Error::Limit(_))
        ));
        let rgb_nest = format!("{}1{}", "rgb(".repeat(20), ")".repeat(20));
        assert_eq!(color(&rgb_nest), KEPT);
        // Huge argument lists are invalid, inspected only up to their eighth
        // component; huge layer lists are linear and need no storage.
        let arguments = format!("rgb({}1)", "1,".repeat(40_000));
        assert_eq!(color(&arguments), KEPT);
        let calcs = format!("rgb({}calc(1))", "calc(1) ".repeat(20_000));
        assert_eq!(color(&calcs), KEPT);
        let layers = format!("{}red", "url(x),".repeat(20_000));
        assert_eq!(background(&layers), rgba(255, 0, 0, 255));
        let misplaced = format!("{}red,url(x)", "url(x),".repeat(20_000));
        assert_eq!(background(&misplaced), KEPT);
        // Skipped math functions are stepped over whole, without the
        // calc() depth check: their depth is MAX_NESTING's.
        let math = format!(
            "{}1px{} 0 blue",
            "calc(".repeat(crate::calc::MAX_CALC_DEPTH + 4),
            ")".repeat(crate::calc::MAX_CALC_DEPTH + 4)
        );
        assert_eq!(background(&math), rgba(0, 0, 255, 255));
        let image = format!(
            "linear-gradient({}red{}) blue",
            "linear-gradient(".repeat(20),
            ")".repeat(20)
        );
        assert_eq!(background(&image), KEPT);
    }
}
