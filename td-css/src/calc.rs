//! Typed, bounded CSS length arithmetic. Percentages remain explicit.
use crate::syntax::{Component, Token};
use crate::values::{self, Length, Unit, MAX_ABSOLUTE_VALUE};
use crate::Error;

pub const MAX_CALC_PARTS: usize = 1024;
pub const MAX_CALC_DEPTH: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Linear {
    px: f64,
    em: f64,
    rem: f64,
    percent: f64,
    has_percent: bool,
}
impl Linear {
    fn map(self, f: impl Fn(f64) -> f64) -> Option<Self> {
        let result = Self {
            px: f(self.px),
            em: f(self.em),
            rem: f(self.rem),
            percent: f(self.percent),
            has_percent: self.has_percent,
        };
        [result.px, result.em, result.rem, result.percent]
            .iter()
            .all(|n| n.is_finite())
            .then_some(result)
    }
    fn add(self, other: Self, sign: f64) -> Option<Self> {
        Self {
            px: self.px + sign * other.px,
            em: self.em + sign * other.em,
            rem: self.rem + sign * other.rem,
            percent: self.percent + sign * other.percent,
            has_percent: self.has_percent || other.has_percent,
        }
        .map(|n| n)
    }
    pub(crate) fn compute(
        self,
        em: f64,
        rem: f64,
        percentage_basis: Option<f64>,
        non_negative: bool,
    ) -> Length {
        let min = if non_negative {
            0.0
        } else {
            -MAX_ABSOLUTE_VALUE
        };
        if let Some(basis) = percentage_basis {
            return Length::Px(
                sum(&[
                    (self.px, 1.0),
                    (self.em, em),
                    (self.rem, rem),
                    (self.percent, basis / 100.0),
                ])
                .clamp(min, MAX_ABSOLUTE_VALUE),
            );
        }
        let px = sum(&[(self.px, 1.0), (self.em, em), (self.rem, rem)]);
        if self.has_percent {
            Length::Calc {
                px,
                percent: self.percent.clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE),
                non_negative,
            }
        } else {
            Length::Px(px.clamp(min, MAX_ABSOLUTE_VALUE))
        }
    }
}
// Normalize before multiplying so opposite large finite terms cannot
// produce infinity minus infinity. Caller factors are bounded metrics.
fn sum(terms: &[(f64, f64)]) -> f64 {
    let direct: f64 = terms.iter().map(|(n, f)| n * f).sum();
    if direct.is_finite() {
        return direct.clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE);
    }
    let scale = terms.iter().map(|(n, _)| n.abs()).fold(0.0, f64::max);
    if scale == 0.0 {
        return 0.0;
    }
    let mut total: f64 = 0.0;
    let mut correction = 0.0;
    for (n, factor) in terms {
        let n = (n / scale) * factor;
        let next = total + n;
        correction += if total.abs() >= n.abs() {
            (total - next) + n
        } else {
            (n - next) + total
        };
        total = next;
    }
    ((total + correction) * scale).clamp(-MAX_ABSOLUTE_VALUE, MAX_ABSOLUTE_VALUE)
}
#[derive(Clone, Copy)]
enum Expr {
    Number(f64),
    Length(Linear),
}
impl Expr {
    fn add(self, other: Self, sign: f64) -> Option<Self> {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => finite(a + sign * b).map(Self::Number),
            (Self::Length(a), Self::Length(b)) => a.add(b, sign).map(Self::Length),
            _ => None,
        }
    }
    fn multiply(self, other: Self) -> Option<Self> {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => finite(a * b).map(Self::Number),
            (Self::Number(n), Self::Length(v)) | (Self::Length(v), Self::Number(n)) => {
                v.map(|v| v * n).map(Self::Length)
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
            Self::Number(v) => finite(v / n).map(Self::Number),
            Self::Length(v) => v.map(|v| v / n).map(Self::Length),
        }
    }
}
fn finite(n: f64) -> Option<f64> {
    n.is_finite().then_some(n)
}
fn validate(v: &[Component], depth: usize, count: &mut usize) -> Result<(), Error> {
    for c in v {
        if *count == MAX_CALC_PARTS {
            return Err(Error::Limit("MAX_CALC_PARTS"));
        }
        *count += 1;
        if let Component::Function { values, .. } | Component::Block { values, .. } = c {
            if depth == MAX_CALC_DEPTH {
                return Err(Error::Limit("MAX_CALC_DEPTH"));
            }
            validate(values, depth + 1, count)?;
        }
    }
    Ok(())
}
pub(crate) fn parse(value: &Component, allow_percent: bool) -> Result<Option<Linear>, Error> {
    let Component::Function { name, values } = value else {
        return Ok(None);
    };
    if !name.eq_ignore_ascii_case("calc") {
        return Ok(None);
    }
    validate(std::slice::from_ref(value), 0, &mut 0)?;
    Ok(match expression(values) {
        Some(Expr::Length(v)) if allow_percent || !v.has_percent => Some(v),
        _ => None,
    })
}
fn expression(values: &[Component]) -> Option<Expr> {
    let mut parser = Parser { values, pos: 0 };
    let result = parser.sum()?;
    parser.space();
    (parser.pos == values.len()).then_some(result)
}
struct Parser<'a> {
    values: &'a [Component],
    pos: usize,
}
impl Parser<'_> {
    fn space(&mut self) {
        while self.values.get(self.pos).is_some_and(Component::whitespace) {
            self.pos += 1;
        }
    }
    fn sum(&mut self) -> Option<Expr> {
        let mut result = self.product()?;
        loop {
            self.space();
            let sign = match self.values.get(self.pos) {
                Some(v) if v.delim('+') => 1.0,
                Some(v) if v.delim('-') => -1.0,
                _ => return Some(result),
            };
            if !self
                .pos
                .checked_sub(1)
                .and_then(|i| self.values.get(i))
                .is_some_and(Component::whitespace)
                || !self
                    .values
                    .get(self.pos + 1)
                    .is_some_and(Component::whitespace)
            {
                return None;
            }
            self.pos += 1;
            result = result.add(self.product()?, sign)?;
        }
    }
    fn product(&mut self) -> Option<Expr> {
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
    fn value(&mut self) -> Option<Expr> {
        self.space();
        let c = self.values.get(self.pos)?;
        self.pos += 1;
        match c {
            Component::Token(Token::Number(n)) => {
                finite(n.representation.parse().ok()?).map(Expr::Number)
            }
            Component::Block { kind: '(', values } => expression(values),
            Component::Function { name, values } if name.eq_ignore_ascii_case("calc") => {
                expression(values)
            }
            _ => {
                let (n, unit) = values::length(c)?;
                let mut result = Linear::default();
                match unit {
                    Unit::Em => result.em = n,
                    Unit::Rem => result.rem = n,
                    Unit::Percent => {
                        result.percent = n;
                        result.has_percent = true;
                    }
                    Unit::Auto => return None,
                    _ => {
                        let factor = match unit {
                            Unit::Px => 1.0,
                            Unit::Pt => 96.0 / 72.0,
                            Unit::Pc => 16.0,
                            Unit::In => 96.0,
                            Unit::Cm => 96.0 / 2.54,
                            Unit::Mm => 96.0 / 25.4,
                            Unit::Q => 96.0 / 101.6,
                            _ => return None,
                        };
                        result.px = finite(n * factor)?;
                    }
                }
                Some(Expr::Length(result))
            }
        }
    }
}
#[cfg(test)]
mod tests;
