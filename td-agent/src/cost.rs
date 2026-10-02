//! Money (DESIGN.md §5): OpenRouter's credits, one to the US dollar, held
//! as whole pico-credits (10^-12) in a `u64`. A price list gives prices per
//! token as decimal strings as small as a few hundred-billionths of a
//! credit, and a response's `usage.cost` is a JSON number that may come
//! in exponent form; both are read from their decimal text exactly, with
//! no float in between. 2^64 pico-credits is some eighteen million
//! credits, far past any limit, and every sum saturates there.
//!
//! Limits are kept by reservation: before a request its worst case is
//! reserved against the turn, the conversation and the day, and the
//! request is not sent when a reservation would pass one (`reserve`).

/// Pico-credits in one credit.
pub const ONE: u64 = 1_000_000_000_000;

/// A decimal number's text as pico-credits: `1`, `0.000003`, `1.5e-5`.
/// Digits past the twelfth decimal place round up when `up` holds, as a
/// price does for a reservation, and to the nearest otherwise. A sign,
/// a negative value, a value past the `u64` range or anything that is not
/// a plain JSON number is `None`.
pub fn parse(text: &str, up: bool) -> Option<u64> {
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(at) => {
            let (mantissa, exponent) = text.split_at(at);
            let exponent = exponent.get(1..)?;
            let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
            if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            (mantissa, exponent.parse::<i32>().ok()?)
        }
        None => (text, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (mantissa.contains('.') && fraction.is_empty())
    {
        return None;
    }
    // The digits as one integer, scaled by ten to the `shift`.
    let digits: String = whole
        .chars()
        .chain(fraction.chars())
        .skip_while(|c| *c == '0')
        .collect();
    if digits.is_empty() {
        return Some(0);
    }
    if digits.len() > 36 {
        return None;
    }
    let value: u128 = digits.parse().ok()?;
    let shift = i64::from(exponent) - fraction.len() as i64 + 12;
    let scaled = if shift >= 0 {
        let power = 10u128.checked_pow(u32::try_from(shift).ok()?)?;
        value.checked_mul(power)?
    } else {
        let Some(power) = u32::try_from(-shift)
            .ok()
            .and_then(|s| 10u128.checked_pow(s))
        else {
            // Smaller than any power the type holds: it is a fraction of
            // a pico-credit, which is one when rounding up.
            return Some(u64::from(up));
        };
        let (quotient, remainder) = (value / power, value % power);
        let round = if up {
            remainder > 0
        } else {
            remainder.saturating_mul(2) >= power
        };
        quotient.saturating_add(u128::from(round))
    };
    u64::try_from(scaled).ok()
}

/// `amount` as credits for a person, with a dollar sign: four decimal
/// places, so a cent's fraction a request costs still shows.
pub fn show(amount: u64) -> String {
    // Rounded to the nearest ten-thousandth.
    let tenths = (u128::from(amount) + 50_000_000) / 100_000_000;
    format!("${}.{:04}", tenths / 10_000, tenths % 10_000)
}

/// A model's prices, pico-credits per token and per request, rounded up
/// from the list's decimal strings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pricing {
    pub prompt: u64,
    pub completion: u64,
    pub request: u64,
    /// What reasoning tokens cost, where the list names it.
    pub reasoning: u64,
    /// What writing the prompt cache costs per token, where it is dearer
    /// than the prompt rate.
    pub cache_write: u64,
    pub cache_read: u64,
}

impl Pricing {
    /// The worst a request can cost (DESIGN.md §5): `prompt` tokens at
    /// the highest rate a prompt token could be charged, `max_tokens` at
    /// the highest completion rate, and the per-request fee.
    pub fn reserve(&self, prompt: u64, max_tokens: u64) -> u64 {
        let input = u128::from(prompt) * u128::from(self.prompt.max(self.cache_write));
        let output = u128::from(max_tokens) * u128::from(self.completion.max(self.reasoning));
        let total = input
            .saturating_add(output)
            .saturating_add(u128::from(self.request));
        u64::try_from(total).unwrap_or(u64::MAX)
    }

    /// What a request cost by its token counts, where the response named
    /// no cost: cached prompt tokens at the cache-read rate when the list
    /// gives one, written ones at the cache-write rate, the rest at the
    /// prompt rate; reasoning tokens, which the completion count includes,
    /// at the larger of the completion and reasoning rates (a reasoning
    /// rate of zero means the completion rate), the rest at the
    /// completion rate.
    pub fn charge(&self, usage: &Tokens) -> u64 {
        let cached = usage.cached.min(usage.prompt);
        let written = usage.cache_write.min(usage.prompt - cached);
        let plain = usage.prompt - cached - written;
        let read_rate = if self.cache_read > 0 {
            self.cache_read
        } else {
            self.prompt
        };
        let write_rate = self.prompt.max(self.cache_write);
        let reasoning = usage.reasoning.min(usage.completion);
        let answer = usage.completion - reasoning;
        let reasoning_rate = self.completion.max(self.reasoning);
        let total = [
            (plain, self.prompt),
            (cached, read_rate),
            (written, write_rate),
            (answer, self.completion),
            (reasoning, reasoning_rate),
            (1, self.request),
        ]
        .iter()
        .fold(0u128, |sum, (count, rate)| {
            sum.saturating_add(u128::from(*count) * u128::from(*rate))
        });
        u64::try_from(total).unwrap_or(u64::MAX)
    }
}

/// A response's token counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tokens {
    pub prompt: u64,
    pub completion: u64,
    pub cached: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

/// The three spending limits, each `None` when configured `none`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub turn: Option<u64>,
    pub conversation: Option<u64>,
    pub day: Option<u64>,
}

impl Default for Limits {
    /// The shipped defaults: 1, 10 and 25 credits (DESIGN.md §15).
    fn default() -> Self {
        Self {
            turn: Some(ONE),
            conversation: Some(10 * ONE),
            day: Some(25 * ONE),
        }
    }
}

impl Limits {
    pub fn any(&self) -> bool {
        self.turn.is_some() || self.conversation.is_some() || self.day.is_some()
    }
}

/// Whether `spent` and a reservation of `reserved` stay within `limit`;
/// the refusal names the limit, what is spent and what was asked.
pub fn within(name: &str, limit: Option<u64>, spent: u64, reserved: u64) -> Result<(), String> {
    match limit {
        Some(limit) if spent.saturating_add(reserved) > limit => Err(format!(
            "{name} is {}: {} is spent and this request reserves up to {}",
            show(limit),
            show(spent),
            show(reserved)
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn decimals_are_read_exactly_in_pico_credits() {
        assert_eq!(parse("0", false), Some(0));
        assert_eq!(parse("1", false), Some(ONE));
        assert_eq!(parse("25", false), Some(25 * ONE));
        assert_eq!(parse("0.000003", true), Some(3_000_000));
        assert_eq!(parse("0.0000000375", true), Some(37_500));
        assert_eq!(parse("0.00014", false), Some(140_000_000));
        assert_eq!(parse("1.5e-5", false), Some(15_000_000));
        assert_eq!(parse("1.5E-5", false), Some(15_000_000));
        assert_eq!(parse("2e3", false), Some(2_000 * ONE));
        assert_eq!(parse("2e+0", false), Some(2 * ONE));
        assert_eq!(parse("000.5", false), Some(ONE / 2));
        // Past the twelfth place: up for a price, nearest for a cost.
        assert_eq!(parse("0.0000000000001", true), Some(1));
        assert_eq!(parse("0.0000000000001", false), Some(0));
        assert_eq!(parse("0.0000000000005", false), Some(1));
        assert_eq!(parse("1e-40", true), Some(1));
        assert_eq!(parse("1e-40", false), Some(0));
        for bad in [
            "", "-1", "-0.5", "+1", "1.", ".5", "1e", "1e+", "abc", "0x10", "1e99999", "1 ", "NaN",
        ] {
            assert_eq!(parse(bad, true), None, "{bad:?}");
        }
        // Past the u64 range.
        assert_eq!(parse("1e10", false), None);
        assert_eq!(parse("19000000", false), None);
    }

    #[test]
    fn amounts_show_to_four_places() {
        assert_eq!(show(0), "$0.0000");
        assert_eq!(show(ONE), "$1.0000");
        assert_eq!(show(140_000_000), "$0.0001");
        assert_eq!(show(12_345_600_000), "$0.0123");
        assert_eq!(show(25 * ONE + ONE / 2), "$25.5000");
    }

    #[test]
    fn a_reservation_takes_the_highest_rate_that_could_apply() {
        let pricing = Pricing {
            prompt: 3_000_000,
            completion: 15_000_000,
            request: 1_000,
            reasoning: 0,
            cache_write: 3_750_000,
            cache_read: 300_000,
        };
        // The cache-write rate is dearer than the prompt rate.
        assert_eq!(
            pricing.reserve(1_000, 100),
            1_000 * 3_750_000 + 100 * 15_000_000 + 1_000
        );
        let plain = Pricing {
            cache_write: 0,
            reasoning: 20_000_000,
            ..pricing
        };
        assert_eq!(
            plain.reserve(1_000, 100),
            1_000 * 3_000_000 + 100 * 20_000_000 + 1_000
        );
        assert_eq!(
            Pricing {
                prompt: u64::MAX,
                ..pricing
            }
            .reserve(u64::MAX, 1),
            u64::MAX
        );
        // A charge by tokens bills each kind of prompt token at its rate.
        let usage = Tokens {
            prompt: 1_000,
            completion: 10,
            cached: 600,
            cache_write: 100,
            reasoning: 0,
        };
        assert_eq!(
            pricing.charge(&usage),
            300 * 3_000_000 + 600 * 300_000 + 100 * 3_750_000 + 10 * 15_000_000 + 1_000
        );
        // Reasoning tokens, inside the completion count, at the dearer
        // of the two rates; a zero reasoning rate is the completion rate.
        let thought = Tokens {
            reasoning: 4,
            ..usage
        };
        assert_eq!(pricing.charge(&thought), pricing.charge(&usage));
        assert_eq!(
            plain.charge(&thought),
            1_000 * 3_000_000 - 600 * 3_000_000
                + 600 * 300_000
                + 6 * 15_000_000
                + 4 * 20_000_000
                + 1_000
        );
        // Counts past any real reply saturate rather than wrap.
        let huge = Tokens {
            prompt: u64::MAX,
            completion: u64::MAX,
            cached: 0,
            cache_write: 0,
            reasoning: 0,
        };
        assert_eq!(
            Pricing {
                prompt: u64::MAX,
                completion: u64::MAX,
                ..pricing
            }
            .charge(&huge),
            u64::MAX
        );
    }

    #[test]
    fn a_limit_refuses_what_would_pass_it_by_name() {
        assert!(within("max_cost_per_turn", None, u64::MAX, u64::MAX).is_ok());
        assert!(within("max_cost_per_turn", Some(ONE), ONE / 2, ONE / 2).is_ok());
        let e = within("max_cost_per_turn", Some(ONE), ONE / 2, ONE / 2 + 1).unwrap_err();
        assert!(e.starts_with("max_cost_per_turn is $1.0000"), "{e}");
        assert!(Limits::default().any());
        assert!(!Limits {
            turn: None,
            conversation: None,
            day: None
        }
        .any());
    }
}
