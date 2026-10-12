//! Exact integer conversion after bounded JSON lexical validation.
// The retained scanner already checked JSON number syntax. Keep exact decimal
// significance, not a floating conversion that could round a fraction to UInt.
#[derive(Default)]
pub(crate) struct Unsigned {
    pub(crate) negative: bool,
    fraction: bool,
    exponent: bool,
    exponent_negative: bool,
    exponent_value: i64,
    decimals: i64,
    significant: i64,
    zeros: i64,
    prefix: u64,
}
impl Unsigned {
    /// Feed only validated JSON number bytes from a source capped at 4 MiB.
    pub(crate) fn byte(&mut self, byte: u8) {
        match byte {
            b'-' if self.exponent => self.exponent_negative = true,
            b'-' => self.negative = true,
            b'.' => self.fraction = true,
            b'e' | b'E' => self.exponent = true,
            b'0'..=b'9' if self.exponent => {
                // Coefficient lengths stay below u32::MAX - 16 (the JSON cap is 4 MiB).
                self.exponent_value =
                    (self.exponent_value * 10 + i64::from(byte - b'0')).min(i64::from(u32::MAX));
            }
            b'0'..=b'9' => {
                if self.fraction {
                    self.decimals += 1;
                }
                if self.significant != 0 || byte != b'0' {
                    self.significant += 1;
                    if self.significant <= 16 {
                        self.prefix = self.prefix * 10 + u64::from(byte - b'0');
                    }
                    self.zeros = if byte == b'0' { self.zeros + 1 } else { 0 };
                }
            }
            _ => (),
        }
    }
    pub(crate) fn finish(self) -> Option<u64> {
        if self.significant == 0 {
            return Some(0);
        }
        if self.negative {
            return None;
        }
        let exponent = if self.exponent_negative {
            -self.exponent_value
        } else {
            self.exponent_value
        };
        let shift = exponent - self.decimals + self.zeros;
        let digits = self.significant - self.zeros;
        if shift < 0 || digits + shift > 16 {
            return None;
        }
        let mut value = self.prefix;
        for _ in digits..self.significant.min(16) {
            value /= 10;
        }
        for _ in 0..shift {
            value *= 10;
        }
        (value < (1u64 << 53)).then_some(value)
    }
}
