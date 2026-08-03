//! busybox ash's `$RANDOM` generator (`shell/random.c`): an LCG, a Galois LFSR
//! and a 64-bit xorshift combined, then masked to bash's 0..32767. Reproduced
//! exactly rather than approximated because `RANDOM=n` SEEDS it, so a script
//! that seeds is asking for one specific sequence.

/// The three generators' state. `galois` must be SIGNED: the LFSR tap fires on
/// the bit shifted out of the msb, which the C reads as `< 0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rand {
    galois: i32,
    lcg: u32,
    xs_x: u32,
    xs_y: u32,
}

impl Rand {
    /// `INIT_RANDOM_T(rnd, nonzero, v)`: two SEPARATE inputs, one reaching the
    /// LFSR and an xorshift word, the other the LCG and the second xorshift word.
    pub fn init(nonzero: u32, v: u32) -> Self {
        Self {
            galois: nonzero as i32,
            lcg: v,
            xs_x: nonzero,
            xs_y: v,
        }
    }

    /// An assignment has only ONE value, which ash passes as both -- coercing
    /// zero up, since zero is the LFSR's fixed point AND ash's "uninitialised"
    /// marker, and it leaves the xorshift all-zero where `next`'s skip loop
    /// never reaches its exit condition.
    pub fn seeded(v: u32) -> Self {
        Self::init(if v == 0 { 1 } else { v }, v)
    }

    pub fn next(&mut self) -> u32 {
        const MASK: u32 = 0x8000_000b;
        const A: u32 = 2;
        const B: u32 = 7;
        const C: u32 = 3;

        self.lcg = self.lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);

        let mut t = (self.galois as u32) << 1;
        if self.galois < 0 {
            t ^= MASK;
        }
        self.galois = t as i32;

        loop {
            let t = self.xs_x ^ (self.xs_x << A);
            self.xs_x = self.xs_y;
            self.xs_y = self.xs_y ^ (self.xs_y >> C) ^ t ^ (t >> B);
            // Skipping two states drops the xorshift's period from 2^64-1 to
            // 2^64-3, which shares no divisor with the LFSR's 2^32-1; the
            // unskipped period does, shortening the combination's.
            if !(self.xs_y == 0 && self.xs_x <= 2) {
                break;
            }
        }

        let combined = (self.galois as u32)
            .wrapping_sub(self.lcg)
            .wrapping_add(self.xs_y);
        combined & 0x7fff
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

    /// Sequences measured on busybox 1.37.0 ash. An approximation of the
    /// generator passes every "is it a number in range" check and still hands a
    /// seeding script different numbers, which is why these are exact.
    #[test]
    fn a_seeded_sequence_is_ashs() {
        for (seed, want) in [
            (0u32, [3240u32, 22231, 2355, 11491, 7008, 14858]),
            (1, [9882, 31274, 32415, 17757, 4881, 16130]),
            (2, [16531, 7559, 29736, 24043, 2854, 17563]),
            (42, [20351, 9206, 20506, 13396, 18747, 8898]),
            (12345, [2864, 29935, 14187, 3798, 9436, 5897]),
            (4_294_967_295, [29350, 13153, 5018, 5161, 8973, 25390]),
        ] {
            let mut r = Rand::seeded(seed);
            let got: Vec<u32> = want.iter().map(|_| r.next()).collect();
            assert_eq!(got, want.to_vec(), "seed {seed}");
        }
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
