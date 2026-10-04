//! `$RANDOM`'s generator: SplitMix64 (Steele, Lea and Flood, "Fast
//! Splittable Pseudorandom Number Generators", 2014), whose top 15 bits are
//! bash's 0..32767. `RANDOM=n` SEEDS it, so a seeding script gets the same
//! sequence on every run; the sequence is td's own, not another shell's.

/// The generator's state: a counter that every draw advances by the golden
/// ratio constant, and that the output function then mixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rand {
    state: u64,
}

impl Rand {
    /// An unseeded shell's generator, from the process id and a clock reading,
    /// so two shells started together still draw differently.
    pub fn unseeded(pid: u32, clock: u32) -> Self {
        Self {
            state: u64::from(pid) << 32 | u64::from(clock),
        }
    }

    /// The generator an assignment `RANDOM=v` starts. Every seed is usable,
    /// zero included: the counter only has to differ, not be nonzero.
    pub fn seeded(v: u32) -> Self {
        Self {
            state: u64::from(v),
        }
    }

    pub fn next(&mut self) -> u32 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        u32::try_from(z >> 49).unwrap_or(0)
    }
}

/// C `strtoul` base 10, truncated to the `uint32_t` the state holds -- not
/// `parse()`, which gets three cases wrong: a leading numeric PREFIX counts
/// (`5x` is 5), an out-of-range magnitude saturates instead of failing, and a
/// negative value is negated as UNSIGNED.
pub fn seed_of(text: &str) -> u32 {
    let mut rest = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let neg = match rest.strip_prefix('-') {
        Some(r) => {
            rest = r;
            true
        }
        None => {
            rest = rest.strip_prefix('+').unwrap_or(rest);
            false
        }
    };
    let mut acc: u64 = 0;
    let mut saturated = false;
    for c in rest.chars() {
        let Some(d) = c.to_digit(10).map(u64::from) else {
            break;
        };
        match acc.checked_mul(10).and_then(|a| a.checked_add(d)) {
            Some(a) => acc = a,
            None => {
                acc = u64::MAX;
                saturated = true;
            }
        }
    }
    // A range error is `ULONG_MAX` and is NOT then negated, so an absurd negative
    // seeds all-ones exactly like an absurd positive one. Only a magnitude that
    // FITS gets the unsigned negation.
    if saturated {
        return u32::MAX;
    }
    if neg {
        acc = acc.wrapping_neg();
    }
    (acc & 0xffff_ffff) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// td's sequences, pinned so a change to the generator is a decision and
    /// not an accident: a seeding script asks for exactly these numbers.
    #[test]
    fn a_seeded_sequence_is_pinned() {
        for (seed, want) in [
            (0u32, [28944u32, 14140, 866, 31813, 3484, 10725]),
            (1, [18565, 24437, 31817, 14560, 14557, 24998]),
            (2, [19372, 24548, 19517, 25081, 10210, 11358]),
            (42, [24299, 5239, 9129, 11278, 1246, 28450]),
            (12345, [4360, 6711, 3917, 5771, 16609, 11043]),
            (4_294_967_295, [14808, 12432, 30501, 2426, 23432, 32168]),
        ] {
            let mut r = Rand::seeded(seed);
            let got: Vec<u32> = want.iter().map(|_| r.next()).collect();
            assert_eq!(got, want.to_vec(), "seed {seed}");
        }
    }

    /// Reseeding restarts, and distinct seeds and distinct unseeded inputs
    /// give distinct sequences.
    #[test]
    fn a_seed_names_one_sequence() {
        let draw = |mut r: Rand| -> Vec<u32> { (0..8).map(|_| r.next()).collect() };
        assert_eq!(draw(Rand::seeded(9)), draw(Rand::seeded(9)));
        assert_ne!(draw(Rand::seeded(9)), draw(Rand::seeded(10)));
        assert_ne!(draw(Rand::unseeded(100, 5)), draw(Rand::unseeded(101, 5)));
        assert_ne!(draw(Rand::unseeded(100, 5)), draw(Rand::unseeded(100, 6)));
    }

    /// Every one of the 32768 values is reachable, so no bit of the range is
    /// stuck.
    #[test]
    fn the_whole_range_is_drawn() {
        let mut r = Rand::seeded(7);
        let mut seen = vec![false; 32768];
        for _ in 0..400_000 {
            if let Some(slot) = seen.get_mut(r.next() as usize) {
                *slot = true;
            }
        }
        assert!(seen.iter().all(|s| *s));
    }

    #[test]
    fn every_value_is_in_bashs_range() {
        let mut r = Rand::seeded(7);
        for _ in 0..5000 {
            assert!(r.next() <= 32767);
        }
    }

    /// `strtoul`, not `parse()`: every row is a value a naive parse rejects or
    /// reads differently.
    #[test]
    fn a_seed_is_read_as_strtoul_base_ten() {
        assert_eq!(seed_of("0"), 0);
        assert_eq!(seed_of("1"), 1);
        assert_eq!(seed_of("007"), 7);
        assert_eq!(seed_of("+7"), 7);
        assert_eq!(seed_of(" 5"), 5);
        // A numeric prefix counts; the scan stops at the first non-digit.
        assert_eq!(seed_of("5x"), 5);
        assert_eq!(seed_of("0x10"), 0);
        assert_eq!(seed_of("abc"), 0);
        assert_eq!(seed_of(""), 0);
        // Truncation to 32 bits, and the unsigned negation.
        assert_eq!(seed_of("4294967296"), 0);
        assert_eq!(seed_of("-1"), 4_294_967_295);
        assert_eq!(seed_of("-2"), 4_294_967_294);
        // Saturation at ULONG_MAX, whose low 32 bits are all ones -- which is
        // why an absurd seed matches `-1` rather than `0`.
        assert_eq!(seed_of("18446744073709551616"), 4_294_967_295);
        assert_eq!(seed_of("99999999999999999999999"), 4_294_967_295);
        // The range error is reported as ULONG_MAX and NOT negated, so this is
        // all-ones rather than the 1 a negate-after-saturate would give...
        assert_eq!(seed_of("-18446744073709551616"), 4_294_967_295);
        // ...while a magnitude that still FITS is negated as usual.
        assert_eq!(seed_of("-18446744073709551615"), 1);
    }
}
