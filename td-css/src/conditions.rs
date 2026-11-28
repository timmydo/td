//! Bounded media and supports conditions, evaluated without document state.
use crate::syntax::{self, Component, Token};
use crate::{values, Error};
use td_alloc::{Budget, BudgetVec};

pub const MAX_CONDITION_PARTS: usize = 2048;
pub const MAX_CONDITION_DEPTH: usize = 16;
pub const MAX_MEDIA_QUERIES: usize = 64;
pub const MAX_RESOLUTION_DPPX: f64 = 1000.0;
/// Bytes of a `sizes` attribute; longer ones are refused.
pub const MAX_SIZES_BYTES: usize = 4 * 1024;
/// Comma-separated entries of a `sizes` attribute.
pub const MAX_SIZES_ENTRIES: usize = 64;
/// Bytes of a media attribute given to `media_matches_with_budget`.
pub const MAX_MEDIA_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaType {
    Screen,
    Print,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Environment {
    pub media_type: MediaType,
    pub width: f64,
    pub height: f64,
    pub resolution_dppx: f64,
    pub color_scheme: ColorScheme,
    pub reduced_motion: bool,
}
impl Default for Environment {
    fn default() -> Self {
        Self {
            media_type: MediaType::Screen,
            width: 1280.0,
            height: 720.0,
            resolution_dppx: 1.0,
            color_scheme: ColorScheme::Light,
            reduced_motion: false,
        }
    }
}
impl Environment {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        for n in [self.width, self.height] {
            if !n.is_finite() || !(0.0..=values::MAX_ABSOLUTE_VALUE).contains(&n) {
                return Err(Error::InvalidContext);
            }
        }
        if !self.resolution_dppx.is_finite()
            || !(0.0..=MAX_RESOLUTION_DPPX).contains(&self.resolution_dppx)
        {
            return Err(Error::InvalidContext);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    False,
    True,
    Unknown,
}
impl Truth {
    fn from(value: bool) -> Self {
        if value {
            Self::True
        } else {
            Self::False
        }
    }
    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
        }
    }
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::False, _) | (_, Self::False) => Self::False,
            (Self::True, Self::True) => Self::True,
            _ => Self::Unknown,
        }
    }
    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::True, _) | (_, Self::True) => Self::True,
            (Self::False, Self::False) => Self::False,
            _ => Self::Unknown,
        }
    }
}

/// Parse a complete media query list. Unknown queries do not match.
pub fn media_matches(input: &str, environment: &Environment) -> Result<bool, Error> {
    let memory = crate::memory()?;
    media(
        syntax::parse_components_with_budget(input, &memory)?.value(),
        environment,
        &memory,
    )
}
/// A media query list given as an attribute, such as `<source media>`,
/// charged to `memory`. As for a sheet's media lists, the text is no
/// decoded sheet: a leading U+FEFF is part of an identifier. Text past
/// MAX_MEDIA_BYTES is refused.
pub fn media_matches_with_budget(
    input: &str,
    environment: &Environment,
    memory: &Budget,
) -> Result<bool, Error> {
    if input.len() > MAX_MEDIA_BYTES {
        return Err(Error::Limit("MAX_MEDIA_BYTES"));
    }
    media(
        syntax::components(input, memory, syntax::Bom::Keep)?.value(),
        environment,
        memory,
    )
}

/// HTML's "parse a sizes attribute": the CSS px of the first entry whose
/// media condition matches, or that has none, else 100vw. See DESIGN.md.
pub fn source_size(input: &str, environment: &Environment, memory: &Budget) -> Result<f64, Error> {
    environment.validate()?;
    if input.len() > MAX_SIZES_BYTES {
        return Err(Error::Limit("MAX_SIZES_BYTES"));
    }
    let list = syntax::components(input, memory, syntax::Bom::Keep)?;
    // Every entry is read, so each bound holds whatever an earlier one says.
    let mut count = 0;
    let mut size = None;
    for (i, entry) in list.value().split(|v| v.delim(',')).enumerate() {
        if i == MAX_SIZES_ENTRIES {
            return Err(Error::Limit("MAX_SIZES_ENTRIES"));
        }
        let found = source_size_entry(entry, environment, &mut count, memory)?;
        if size.is_none() {
            size = found;
        }
    }
    Ok(size.unwrap_or(environment.width))
}

/// Query the property/value grammar this crate implements, not layout support.
pub fn supports_matches(input: &str) -> Result<bool, Error> {
    let memory = crate::memory()?;
    supports(
        syntax::parse_components_with_budget(input, &memory)?.value(),
        &memory,
    )
}

fn validate(values: &[Component], depth: usize, count: &mut usize) -> Result<bool, Error> {
    let mut valid = true;
    for v in values {
        if *count == MAX_CONDITION_PARTS {
            return Err(Error::Limit("MAX_CONDITION_PARTS"));
        }
        *count += 1;
        match v {
            Component::Block { values, .. } | Component::Function { values, .. } => {
                if depth == MAX_CONDITION_DEPTH {
                    return Err(Error::Limit("MAX_CONDITION_DEPTH"));
                }
                valid &= validate(values, depth + 1, count)?;
            }
            Component::Token(Token::BadString | Token::BadUrl | Token::Close(_)) => valid = false,
            _ => {}
        }
    }
    Ok(valid)
}
fn compact<'a>(
    values: &'a [Component],
    memory: &Budget,
) -> Result<BudgetVec<&'a Component>, Error> {
    let mut out = td_alloc::BudgetVec::with_capacity(memory, values.len())?;
    for value in values.iter().filter(|v| !v.whitespace()) {
        out.try_push(value)?;
    }
    Ok(out)
}
fn ident(value: Option<&&Component>, name: &str) -> bool {
    value
        .and_then(|v| v.ident())
        .is_some_and(|v| v.eq_ignore_ascii_case(name))
}
fn reserved(name: &str) -> bool {
    ["not", "only", "and", "or", "layer"]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
}

pub(crate) fn media(
    values: &[Component],
    env: &Environment,
    memory: &Budget,
) -> Result<bool, Error> {
    env.validate()?;
    // Validate each comma member separately for recoverable grammar errors,
    // but share admission accounting across all members.
    let mut count = 0;
    if syntax::trim(values).is_empty() {
        return Ok(true);
    }
    let mut answer = false;
    for (i, member) in values.split(|v| v.delim(',')).enumerate() {
        if i == MAX_MEDIA_QUERIES {
            return Err(Error::Limit("MAX_MEDIA_QUERIES"));
        }
        if i > 0 {
            if count == MAX_CONDITION_PARTS {
                return Err(Error::Limit("MAX_CONDITION_PARTS"));
            }
            count += 1;
        }
        if validate(member, 0, &mut count)? {
            answer |= media_query(&compact(member, memory)?, env, memory)? == Some(Truth::True);
        }
    }
    Ok(answer)
}
fn media_query(
    v: &[&Component],
    env: &Environment,
    memory: &Budget,
) -> Result<Option<Truth>, Error> {
    let mut pos = 0;
    let negate = ident(v.first(), "not");
    if negate || ident(v.first(), "only") {
        pos += 1;
    }
    if let Some(name) = v.get(pos).and_then(|v| v.ident()).filter(|n| !reserved(n)) {
        let mut result = Truth::from(
            name.eq_ignore_ascii_case("all")
                || match env.media_type {
                    MediaType::Screen => name.eq_ignore_ascii_case("screen"),
                    MediaType::Print => name.eq_ignore_ascii_case("print"),
                },
        );
        pos += 1;
        if let Some(rest) = v.get(pos..).filter(|v| !v.is_empty()) {
            if !ident(rest.first(), "and") {
                return Ok(None);
            }
            let Some(t) = condition(rest.get(1..).unwrap_or_default(), Some(env), false, memory)?
            else {
                return Ok(None);
            };
            result = result.and(t);
        }
        return Ok(Some(if negate { result.not() } else { result }));
    }
    condition(v, Some(env), true, memory)
}

pub(crate) fn supports(values: &[Component], memory: &Budget) -> Result<bool, Error> {
    if !validate(values, 0, &mut 0)? {
        return Ok(false);
    }
    Ok(condition(&compact(values, memory)?, None, true, memory)? == Some(Truth::True))
}
fn condition(
    v: &[&Component],
    env: Option<&Environment>,
    allow_or: bool,
    memory: &Budget,
) -> Result<Option<Truth>, Error> {
    if ident(v.first(), "not") {
        let [_, value] = v else {
            return Ok(None);
        };
        return Ok(atom(value, env, memory)?.map(Truth::not));
    }
    let Some(first) = v.first() else {
        return Ok(None);
    };
    let Some(mut result) = atom(first, env, memory)? else {
        return Ok(None);
    };
    let mut mode = None;
    let rest = v.get(1..).unwrap_or_default();
    let (pairs, remainder) = rest.as_chunks::<2>();
    for [operator, value] in pairs {
        let or = match operator.ident() {
            Some(n) if n.eq_ignore_ascii_case("and") => false,
            Some(n) if n.eq_ignore_ascii_case("or") && allow_or => true,
            _ => return Ok(None),
        };
        if mode.is_some_and(|old| old != or) {
            return Ok(None);
        }
        mode = Some(or);
        let Some(next) = atom(value, env, memory)? else {
            return Ok(None);
        };
        result = if or {
            result.or(next)
        } else {
            result.and(next)
        };
    }
    if !remainder.is_empty() {
        return Ok(None);
    }
    Ok(Some(result))
}
fn atom(v: &Component, env: Option<&Environment>, memory: &Budget) -> Result<Option<Truth>, Error> {
    let unknown = if env.is_some() {
        Truth::Unknown
    } else {
        Truth::False
    };
    let Component::Block { kind: '(', values } = v else {
        return Ok(if matches!(v, Component::Function { .. }) {
            Some(unknown)
        } else {
            None
        });
    };
    let parts = compact(values, memory)?;
    if let Some(t) = condition(&parts, env, true, memory)? {
        return Ok(Some(t));
    }
    if let Some(env) = env {
        return Ok(Some(feature(values, &parts, env)));
    }
    // A supports declaration is exactly one declaration, not a list.
    if values.iter().any(|v| v.delim(';'))
        || !parts.first().is_some_and(|v| v.ident().is_some())
        || !parts.get(1).is_some_and(|v| v.delim(':'))
    {
        return Ok(Some(Truth::False));
    }
    let parsed = syntax::declarations(values, memory)?;
    let [d] = parsed.value().as_slice() else {
        return Ok(Some(Truth::False));
    };
    Ok(Some(Truth::from(
        crate::variables::parse(d, memory)?.is_some()
            || !values::parse_declaration(d, memory)?.is_empty(),
    )))
}

fn feature(values: &[Component], p: &[&Component], env: &Environment) -> Truth {
    // Whitespace may surround a comparison, never split <= or >=.
    for (i, c) in values.iter().enumerate() {
        if c.delim('<') || c.delim('>') {
            let rest = values.get(i + 1..).unwrap_or_default();
            if rest.first().is_some_and(Component::whitespace)
                && syntax::trim(rest).first().is_some_and(|v| v.delim('='))
            {
                return Truth::Unknown;
            }
        }
    }
    if let [name] = p {
        return match name.ident().and_then(|name| {
            syntax::keyword(
                name,
                &[
                    "width",
                    "height",
                    "resolution",
                    "orientation",
                    "prefers-color-scheme",
                    "prefers-reduced-motion",
                ],
            )
        }) {
            Some("width") => Truth::from(env.width != 0.0),
            Some("height") => Truth::from(env.height != 0.0),
            Some("resolution") => Truth::from(env.resolution_dppx != 0.0),
            Some("orientation" | "prefers-color-scheme") => Truth::True,
            Some("prefers-reduced-motion") => Truth::from(env.reduced_motion),
            _ => Truth::Unknown,
        };
    }
    if let [name, colon, value] = p {
        if colon.delim(':') {
            let Some(name) = name.ident() else {
                return Truth::Unknown;
            };
            let Some(name) = syntax::keyword(
                name,
                &[
                    "width",
                    "height",
                    "resolution",
                    "min-width",
                    "max-width",
                    "min-height",
                    "max-height",
                    "min-resolution",
                    "max-resolution",
                    "orientation",
                    "prefers-color-scheme",
                    "prefers-reduced-motion",
                ],
            ) else {
                return Truth::Unknown;
            };
            let (base, op) = if let Some(n) = name.strip_prefix("min-") {
                (n, Compare::Ge)
            } else if let Some(n) = name.strip_prefix("max-") {
                (n, Compare::Le)
            } else {
                (name, Compare::Eq)
            };
            if let Some(actual) = metric(base, env) {
                return number(value, base)
                    .map(|v| op.apply(actual, v))
                    .unwrap_or(Truth::Unknown);
            }
            if base != name {
                return Truth::Unknown;
            }
            let Some(value) = value.ident() else {
                return Truth::Unknown;
            };
            let value = syntax::keyword(
                value,
                &[
                    "portrait",
                    "landscape",
                    "light",
                    "dark",
                    "reduce",
                    "no-preference",
                ],
            )
            .unwrap_or("");
            return match (base, value) {
                ("orientation", "portrait") => Truth::from(env.height >= env.width),
                ("orientation", "landscape") => Truth::from(env.width > env.height),
                ("prefers-color-scheme", "light") => {
                    Truth::from(env.color_scheme == ColorScheme::Light)
                }
                ("prefers-color-scheme", "dark") => {
                    Truth::from(env.color_scheme == ColorScheme::Dark)
                }
                ("prefers-reduced-motion", "reduce") => Truth::from(env.reduced_motion),
                ("prefers-reduced-motion", "no-preference") => Truth::from(!env.reduced_motion),
                _ => Truth::Unknown,
            };
        }
    }
    range(p, env).unwrap_or(Truth::Unknown)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compare {
    Lt,
    Le,
    Eq,
    Gt,
    Ge,
}
impl Compare {
    fn apply(self, a: f64, b: f64) -> Truth {
        Truth::from(match self {
            Self::Lt => a < b,
            Self::Le => a <= b,
            Self::Eq => a == b,
            Self::Gt => a > b,
            Self::Ge => a >= b,
        })
    }
    fn direction(self) -> i8 {
        match self {
            Self::Lt | Self::Le => -1,
            Self::Eq => 0,
            _ => 1,
        }
    }
}
fn metric(name: &str, env: &Environment) -> Option<f64> {
    match syntax::keyword(name, &["width", "height", "resolution"])? {
        "width" => Some(env.width),
        "height" => Some(env.height),
        "resolution" => Some(env.resolution_dppx),
        _ => None,
    }
}
fn number(value: &Component, name: &str) -> Option<f64> {
    let (n, unit) = match value {
        Component::Token(Token::Dimension(n, u)) => (n, u.as_str()),
        Component::Token(Token::Number(n)) => (n, ""),
        _ => return None,
    };
    let n = n
        .representation
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())?;
    let unit = syntax::keyword(
        unit,
        &[
            "dppx", "x", "dpi", "dpcm", "px", "em", "rem", "pt", "pc", "in", "cm", "mm", "q", "",
        ],
    )?;
    let factor = if name.eq_ignore_ascii_case("resolution") {
        match unit {
            "dppx" | "x" => 1.0,
            "dpi" => 1.0 / 96.0,
            "dpcm" => 2.54 / 96.0,
            _ => return None,
        }
    } else {
        match unit {
            "px" => 1.0,
            "em" | "rem" => 16.0,
            "pt" => 96.0 / 72.0,
            "pc" => 16.0,
            "in" => 96.0,
            "cm" => 96.0 / 2.54,
            "mm" => 96.0 / 25.4,
            "q" => 96.0 / 101.6,
            "" if n == 0.0 => 1.0,
            _ => return None,
        }
    };
    let result = n * factor;
    result.is_finite().then_some(result)
}
fn comparison(p: &[&Component], pos: &mut usize) -> Option<Compare> {
    let token = p.get(*pos)?;
    *pos += 1;
    let eq = p.get(*pos).is_some_and(|c| c.delim('='));
    let op = if token.delim('<') {
        if eq {
            Compare::Le
        } else {
            Compare::Lt
        }
    } else if token.delim('>') {
        if eq {
            Compare::Ge
        } else {
            Compare::Gt
        }
    } else if token.delim('=') {
        return Some(Compare::Eq);
    } else {
        return None;
    };
    if eq {
        *pos += 1;
    }
    Some(op)
}
fn range(p: &[&Component], env: &Environment) -> Option<Truth> {
    let first = p.first()?;
    let mut pos = 1;
    let op = comparison(p, &mut pos)?;
    let second = p.get(pos)?;
    pos += 1;
    if let Some(name) = first.ident() {
        let actual = metric(name, env)?;
        let value = number(second, name)?;
        return (pos == p.len()).then(|| op.apply(actual, value));
    }
    let name = second.ident()?;
    let actual = metric(name, env)?;
    let before = number(first, name)?;
    let mut result = op.apply(before, actual);
    if pos == p.len() {
        return Some(result);
    }
    let other = comparison(p, &mut pos)?;
    if op.direction() == 0 || op.direction() != other.direction() {
        return None;
    }
    let last = p.get(pos)?;
    pos += 1;
    result = result.and(other.apply(actual, number(last, name)?));
    (pos == p.len()).then_some(result)
}

/// `<media-condition>? <source-size-value>`, or None when HTML skips it.
fn source_size_entry(
    entry: &[Component],
    env: &Environment,
    count: &mut usize,
    memory: &Budget,
) -> Result<Option<f64>, Error> {
    let Some((last, rest)) = syntax::trim(entry).split_last() else {
        return Ok(None);
    };
    let rest = syntax::trim(rest);
    // Bound the condition and the value before evaluating either, so a
    // refusal depends on the text alone, never on the environment.
    let parsed = rest.is_empty() || validate(rest, 0, count)?;
    if size_function(last) {
        crate::calc::validate(std::slice::from_ref(last), 0, &mut 0)?;
    }
    // Only an img with loading=lazy allows auto-sizes, and td implements
    // no lazy loading, so HTML skips a lone `auto`; nor is `auto` a
    // <source-size-value> after a condition.
    let Some(size) = source_size_value(last, env) else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Ok(Some(size));
    }
    if !parsed {
        return Ok(None);
    }
    let matched = condition(&compact(rest, memory)?, Some(env), true, memory)? == Some(Truth::True);
    Ok(matched.then_some(size))
}

/// The math functions a `<source-size-value>` may be.
const SIZE_FUNCTIONS: &[&str] = &["calc", "min", "max", "clamp"];

fn size_function(value: &Component) -> bool {
    matches!(value, Component::Function { name, .. }
        if syntax::keyword(name, SIZE_FUNCTIONS).is_some())
}

/// A non-negative `<length>` or math function, never a percentage, in px.
/// A math function's caller has bounded it with calc()'s limits.
fn source_size_value(value: &Component, env: &Environment) -> Option<f64> {
    let px = match value {
        Component::Token(Token::Number(n)) => size_number(&n.representation).filter(|n| *n == 0.0),
        Component::Token(Token::Dimension(n, unit)) => size_number(&n.representation)
            .filter(|n| *n >= 0.0)
            .zip(size_unit(unit, env))
            .map(|(n, factor)| saturate(n * factor)),
        Component::Function { .. } => match size_math(value, env)? {
            Quantity::Length(px) => Some(px),
            Quantity::Number(_) => None,
        },
        _ => None,
    }?;
    // The one range clamp, literal or calculated, into [0,
    // MAX_ABSOLUTE_VALUE]; -0 (and NaN, never produced) become +0.
    Some(if px > 0.0 {
        px.min(values::MAX_ABSOLUTE_VALUE)
    } else {
        0.0
    })
}

/// A number token's value, an overflow saturated as CSS saturates one.
fn size_number(text: &str) -> Option<f64> {
    text.parse::<f64>()
        .ok()
        .filter(|n| !n.is_nan())
        .map(saturate)
}

/// The px in one unit, against no element: font-relative units take the
/// initial 16px font, ex and ch half of it, viewport units the viewport.
fn size_unit(unit: &str, env: &Environment) -> Option<f64> {
    let unit = syntax::keyword(unit, SIZE_UNITS)?;
    Some(match unit {
        "px" => 1.0,
        "em" | "rem" => 16.0,
        "ex" | "ch" => 8.0,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "q" => 96.0 / 101.6,
        "vw" | "svw" | "lvw" | "dvw" => env.width / 100.0,
        "vh" | "svh" | "lvh" | "dvh" => env.height / 100.0,
        "vmin" | "svmin" | "lvmin" | "dvmin" => env.width.min(env.height) / 100.0,
        "vmax" | "svmax" | "lvmax" | "dvmax" => env.width.max(env.height) / 100.0,
        _ => return None,
    })
}
const SIZE_UNITS: &[&str] = &[
    "px", "em", "rem", "ex", "ch", "pt", "pc", "in", "cm", "mm", "q", "vw", "svw", "lvw", "dvw",
    "vh", "svh", "lvh", "dvh", "vmin", "svmin", "lvmin", "dvmin", "vmax", "svmax", "lvmax",
    "dvmax",
];

/// A math function's value, every length resolved to px at once.
#[derive(Clone, Copy)]
enum Quantity {
    Number(f64),
    Length(f64),
}
impl Quantity {
    fn add(self, other: Self, sign: f64) -> Option<Self> {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => Some(Self::Number(saturate(a + sign * b))),
            (Self::Length(a), Self::Length(b)) => Some(Self::Length(saturate(a + sign * b))),
            _ => None,
        }
    }
    fn multiply(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => Some(Self::Number(saturate(a * b))),
            (Self::Number(n), Self::Length(v)) | (Self::Length(v), Self::Number(n)) => {
                Some(Self::Length(saturate(v * n)))
            }
            _ => None,
        }
    }
    fn divide(self, other: Self) -> Option<Self> {
        let Self::Number(n) = other else {
            return None;
        };
        if n == 0.0 {
            return None;
        }
        match self {
            Self::Number(v) => Some(Self::Number(saturate(v / n))),
            Self::Length(v) => Some(Self::Length(saturate(v / n))),
        }
    }
    /// The smaller (or larger) of two values of one type.
    fn pick(self, other: Self, smaller: bool) -> Option<Self> {
        let choose = |a: f64, b: f64| if smaller { a.min(b) } else { a.max(b) };
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => Some(Self::Number(choose(a, b))),
            (Self::Length(a), Self::Length(b)) => Some(Self::Length(choose(a, b))),
            _ => None,
        }
    }
}
/// Finite operands give a finite or infinite result, never NaN; an
/// infinity saturates at f64's range, so a later term can still cancel it.
fn saturate(n: f64) -> f64 {
    n.clamp(-f64::MAX, f64::MAX)
}

/// calc(), min(), max() or clamp(). The caller has bounded its parts and
/// depth with MAX_CALC_PARTS and MAX_CALC_DEPTH, so recursion is bounded.
fn size_math(value: &Component, env: &Environment) -> Option<Quantity> {
    let Component::Function { name, values } = value else {
        return None;
    };
    let name = syntax::keyword(name, SIZE_FUNCTIONS)?;
    if name == "calc" {
        return size_sum(values, env);
    }
    let smaller = name == "min";
    // clamp(MIN, VAL, MAX) is max(MIN, min(VAL, MAX)).
    let mut clamp: [Option<Quantity>; 3] = [None; 3];
    let mut picked: Option<Quantity> = None;
    let mut arguments = 0usize;
    for argument in values.split(|v| v.delim(',')) {
        let q = size_sum(argument, env)?;
        if let Some(slot) = clamp.get_mut(arguments) {
            *slot = Some(q);
        }
        arguments += 1;
        picked = Some(match picked {
            None => q,
            Some(p) => p.pick(q, smaller)?,
        });
    }
    match (name, clamp) {
        ("min" | "max", _) => picked,
        ("clamp", [Some(low), Some(v), Some(high)]) if arguments == 3 => {
            v.pick(high, true)?.pick(low, false)
        }
        _ => None,
    }
}
fn size_sum(values: &[Component], env: &Environment) -> Option<Quantity> {
    let mut parser = SizeParser {
        values,
        pos: 0,
        env,
    };
    let result = parser.sum()?;
    parser.space();
    (parser.pos == values.len()).then_some(result)
}
/// calc.rs's grammar over px: + and - need whitespace on both sides.
struct SizeParser<'a> {
    values: &'a [Component],
    pos: usize,
    env: &'a Environment,
}
impl SizeParser<'_> {
    fn space(&mut self) {
        while self.values.get(self.pos).is_some_and(Component::whitespace) {
            self.pos += 1;
        }
    }
    fn sum(&mut self) -> Option<Quantity> {
        let mut result = self.product()?;
        loop {
            self.space();
            let sign = match self.values.get(self.pos) {
                Some(v) if v.delim('+') => 1.0,
                Some(v) if v.delim('-') => -1.0,
                _ => return Some(result),
            };
            let before = self
                .pos
                .checked_sub(1)
                .and_then(|i| self.values.get(i))
                .is_some_and(Component::whitespace);
            let after = self
                .values
                .get(self.pos + 1)
                .is_some_and(Component::whitespace);
            if !before || !after {
                return None;
            }
            self.pos += 1;
            result = result.add(self.product()?, sign)?;
        }
    }
    fn product(&mut self) -> Option<Quantity> {
        let mut result = self.value()?;
        loop {
            self.space();
            let divide = match self.values.get(self.pos) {
                Some(v) if v.delim('*') => false,
                Some(v) if v.delim('/') => true,
                _ => return Some(result),
            };
            self.pos += 1;
            let right = self.value()?;
            result = if divide {
                result.divide(right)?
            } else {
                result.multiply(right)?
            };
        }
    }
    fn value(&mut self) -> Option<Quantity> {
        self.space();
        let c = self.values.get(self.pos)?;
        self.pos += 1;
        match c {
            Component::Token(Token::Number(n)) => {
                size_number(&n.representation).map(Quantity::Number)
            }
            Component::Token(Token::Dimension(n, unit)) => {
                let n = size_number(&n.representation)?;
                Some(Quantity::Length(saturate(n * size_unit(unit, self.env)?)))
            }
            Component::Block { kind: '(', values } => size_sum(values, self.env),
            Component::Function { .. } => size_math(c, self.env),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
