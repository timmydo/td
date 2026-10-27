//! A QR code of the recovery key's 48 digits, so a phone can keep the key
//! without anyone typing it (td-install/ENCRYPTION.md "Device-bound
//! default"). One symbol only: ISO/IEC 18004 version 2 (25 by 25 modules),
//! error correction level Q, numeric mode, which holds exactly 48 digits in
//! its 22 data codewords. The scanned text is the digits alone, the keyslot
//! passphrase stock cryptsetup takes.
//!
//! Std-only and fixed to that one symbol, so there is no version, mode or
//! block-interleaving choice to get wrong. The mask is chosen by the
//! standard's penalty score. Every buffer that holds the digits' bits is
//! sized once and zeroed when done, as the key's own buffers are.

/// Modules per side.
pub const SIZE: usize = 25;
/// The most digits the symbol holds.
pub const CAPACITY: usize = 48;
/// Light modules a reader needs around the symbol, per side.
pub const QUIET_ZONE: usize = 4;

const DATA_CODEWORDS: usize = 22;
const ECC_CODEWORDS: usize = 22;
/// Level Q's two-bit indicator in the format information.
const LEVEL_Q: u32 = 0b11;
/// Version 2's one alignment pattern, centred here.
const ALIGNMENT: usize = 18;

/// A finished symbol: which modules are dark, zeroed on drop.
pub struct Code {
    dark: Vec<bool>,
}

impl Drop for Code {
    fn drop(&mut self) {
        wipe(&mut self.dark);
    }
}

fn wipe<T: Copy + Default>(values: &mut [T]) {
    values.fill(T::default());
    std::hint::black_box(values);
}

impl Code {
    /// Whether the module at column `x`, row `y` is dark; outside the
    /// symbol is light, as the quiet zone is.
    pub fn dark(&self, x: usize, y: usize) -> bool {
        x < SIZE && y < SIZE && self.dark.get(y * SIZE + x).copied().unwrap_or(false)
    }
}

/// The symbol for 1 to 48 ASCII digits, or `None` for anything else.
pub fn numeric(digits: &[u8]) -> Option<Code> {
    let grid = encode(digits)?;
    let best = (0..8u8).min_by_key(|&mask| {
        let mut trial = grid.clone();
        trial.mask(mask);
        trial.penalty()
    })?;
    Some(finish(grid, best))
}

/// The symbol with a given mask, for comparison with another encoder.
#[cfg(test)]
fn with_mask(digits: &[u8], mask: u8) -> Option<Code> {
    Some(finish(encode(digits)?, mask))
}

fn finish(mut grid: Grid, mask: u8) -> Code {
    grid.mask(mask);
    Code {
        dark: std::mem::take(&mut grid.dark),
    }
}

/// The unmasked symbol: function patterns and data, format area reserved.
fn encode(digits: &[u8]) -> Option<Grid> {
    if digits.is_empty() || digits.len() > CAPACITY || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut data = data_codewords(digits)?;
    let mut ecc = remainder(&data, &generator(ECC_CODEWORDS));
    let mut grid = Grid::new();
    grid.function_patterns();
    grid.place(data.iter().chain(&ecc));
    wipe(&mut data);
    wipe(&mut ecc);
    Some(grid)
}

/// Mode, count, digit groups, terminator and padding, as 22 bytes.
fn data_codewords(digits: &[u8]) -> Option<Vec<u8>> {
    let mut bits = Bits {
        bytes: Vec::with_capacity(DATA_CODEWORDS),
        len: 0,
    };
    bits.push(0b0001, 4);
    bits.push(u32::try_from(digits.len()).ok()?, 10);
    for group in digits.chunks(3) {
        let value = group.iter().fold(0u32, |n, d| n * 10 + u32::from(d - b'0'));
        let width = match group.len() {
            3 => 10,
            2 => 7,
            _ => 4,
        };
        bits.push(value, width);
    }
    let capacity = DATA_CODEWORDS * 8;
    let terminator = capacity.saturating_sub(bits.len).min(4);
    bits.push(0, terminator);
    bits.push(0, (8 - bits.len % 8) % 8);
    for pad in [0xec, 0x11].iter().cycle() {
        if bits.len >= capacity {
            break;
        }
        bits.push(*pad, 8);
    }
    let complete = bits.len == capacity;
    complete.then(|| std::mem::take(&mut bits.bytes))
}

/// Allocated for exactly the data codewords, so pushing never leaves a
/// copy behind; zeroed on drop.
struct Bits {
    bytes: Vec<u8>,
    len: usize,
}

impl Drop for Bits {
    fn drop(&mut self) {
        wipe(&mut self.bytes);
    }
}

impl Bits {
    fn push(&mut self, value: u32, width: usize) {
        for shift in (0..width).rev() {
            if self.len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> shift) & 1 != 0 {
                if let Some(byte) = self.bytes.last_mut() {
                    *byte |= 0x80 >> (self.len % 8);
                }
            }
            self.len += 1;
        }
    }
}

/// Multiplication in GF(2^8) modulo x^8 + x^4 + x^3 + x^2 + 1 (0x11d).
fn gf_multiply(x: u8, y: u8) -> u8 {
    let mut z: u16 = 0;
    for shift in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11d);
        if (y >> shift) & 1 != 0 {
            z ^= u16::from(x);
        }
    }
    (z & 0xff) as u8
}

/// The Reed-Solomon generator of `degree`, the product of (x - 2^i) for i
/// below it, highest term implied, coefficients from the next one down.
fn generator(degree: usize) -> Vec<u8> {
    let mut divisor = vec![0u8; degree];
    if let Some(last) = divisor.last_mut() {
        *last = 1;
    }
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            let next = divisor.get(j + 1).copied().unwrap_or(0);
            if let Some(coefficient) = divisor.get_mut(j) {
                *coefficient = gf_multiply(*coefficient, root) ^ next;
            }
        }
        root = gf_multiply(root, 0x02);
    }
    divisor
}

/// The error correction codewords: `data` times x^degree modulo the
/// generator.
fn remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for byte in data {
        let factor = byte ^ result.first().copied().unwrap_or(0);
        result.rotate_left(1);
        if let Some(last) = result.last_mut() {
            *last = 0;
        }
        for (r, coefficient) in result.iter_mut().zip(divisor) {
            *r ^= gf_multiply(*coefficient, factor);
        }
    }
    result
}

/// Zeroed on drop, every mask trial's copy included.
#[derive(Clone)]
struct Grid {
    dark: Vec<bool>,
    /// Finder, separator, timing, alignment and format modules, which
    /// carry no data and are never masked.
    function: Vec<bool>,
}

impl Drop for Grid {
    fn drop(&mut self) {
        wipe(&mut self.dark);
    }
}

impl Grid {
    fn new() -> Self {
        Self {
            dark: vec![false; SIZE * SIZE],
            function: vec![false; SIZE * SIZE],
        }
    }

    fn get(&self, x: usize, y: usize) -> bool {
        x < SIZE && y < SIZE && self.dark.get(y * SIZE + x).copied().unwrap_or(false)
    }

    fn is_function(&self, x: usize, y: usize) -> bool {
        x < SIZE && y < SIZE && self.function.get(y * SIZE + x).copied().unwrap_or(false)
    }

    /// A function module; outside the symbol is ignored, as a finder's
    /// separator past the edge is.
    fn set_function(&mut self, x: isize, y: isize, dark: bool) {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        if x >= SIZE || y >= SIZE {
            return;
        }
        let at = y * SIZE + x;
        if let (Some(module), Some(function)) = (self.dark.get_mut(at), self.function.get_mut(at)) {
            *module = dark;
            *function = true;
        }
    }

    fn function_patterns(&mut self) {
        let last = SIZE as isize - 1;
        for i in 0..SIZE as isize {
            self.set_function(6, i, i % 2 == 0);
            self.set_function(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (last - 3, 3), (3, last - 3)] {
            for dy in -4..=4isize {
                for dx in -4..=4isize {
                    let distance = dx.abs().max(dy.abs());
                    self.set_function(cx + dx, cy + dy, distance != 2 && distance != 4);
                }
            }
        }
        let centre = ALIGNMENT as isize;
        for dy in -2..=2isize {
            for dx in -2..=2isize {
                self.set_function(centre + dx, centre + dy, dx.abs().max(dy.abs()) != 1);
            }
        }
        // Reserved now, written per mask.
        self.format(0);
    }

    /// Level Q's format information for `mask`, both copies, and the dark
    /// module beside the lower one.
    fn format(&mut self, mask: u8) {
        let data = (LEVEL_Q << 3) | u32::from(mask);
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = ((data << 10) | rem) ^ 0x5412;
        let bit = |i: usize| (bits >> i) & 1 != 0;
        let size = SIZE as isize;
        for i in 0..6 {
            self.set_function(8, i, bit(i as usize));
        }
        self.set_function(8, 7, bit(6));
        self.set_function(8, 8, bit(7));
        self.set_function(7, 8, bit(8));
        for i in 9..15 {
            self.set_function(14 - i, 8, bit(i as usize));
        }
        for i in 0..8 {
            self.set_function(size - 1 - i, 8, bit(i as usize));
        }
        for i in 8..15 {
            self.set_function(8, size - 15 + i, bit(i as usize));
        }
        self.set_function(8, size - 8, true);
    }

    /// The codewords' bits in the standard's two-column zigzag from the
    /// lower right, skipping the vertical timing column and every function
    /// module. Version 2's seven remainder modules stay light.
    fn place<'a>(&mut self, codewords: impl Iterator<Item = &'a u8>) {
        let mut next =
            codewords.flat_map(|byte| (0..8).rev().map(move |shift| (byte >> shift) & 1 != 0));
        let mut right = SIZE - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            let upward = (right + 1) & 2 == 0;
            for vertical in 0..SIZE {
                let y = if upward {
                    SIZE - 1 - vertical
                } else {
                    vertical
                };
                for x in [right, right - 1] {
                    if self.is_function(x, y) {
                        continue;
                    }
                    let Some(bit) = next.next() else { return };
                    if let Some(module) = self.dark.get_mut(y * SIZE + x) {
                        *module = bit;
                    }
                }
            }
            if right < 2 {
                break;
            }
            right -= 2;
        }
    }

    /// XOR `mask`'s pattern over the data modules and write its format
    /// information; applying the same mask twice undoes the data part.
    fn mask(&mut self, mask: u8) {
        for y in 0..SIZE {
            for x in 0..SIZE {
                let flip = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                if flip && !self.is_function(x, y) {
                    if let Some(module) = self.dark.get_mut(y * SIZE + x) {
                        *module = !*module;
                    }
                }
            }
        }
        self.format(mask);
    }

    /// The standard's penalty: runs of five or more, 2x2 blocks,
    /// finder-like 1:1:3:1:1 patterns with four light modules beside them
    /// (the quiet zone counting as light), and imbalance of dark modules.
    fn penalty(&self) -> u32 {
        let mut score = 0;
        for line in 0..SIZE {
            score += self.line_penalty(|i| self.get(i, line));
            score += self.line_penalty(|i| self.get(line, i));
        }
        for y in 0..SIZE - 1 {
            for x in 0..SIZE - 1 {
                let colour = self.get(x, y);
                if colour == self.get(x + 1, y)
                    && colour == self.get(x, y + 1)
                    && colour == self.get(x + 1, y + 1)
                {
                    score += 3;
                }
            }
        }
        let dark = self.dark.iter().filter(|d| **d).count();
        let total = SIZE * SIZE;
        let k = (dark * 20)
            .abs_diff(total * 10)
            .div_ceil(total)
            .saturating_sub(1);
        score + u32::try_from(k * 10).unwrap_or(u32::MAX)
    }

    fn line_penalty(&self, module: impl Fn(usize) -> bool) -> u32 {
        let mut score = 0;
        let mut colour = false;
        let mut run = 0usize;
        let mut history = RunHistory::default();
        for i in 0..SIZE {
            if module(i) == colour {
                run += 1;
                if run == 5 {
                    score += 3;
                } else if run > 5 {
                    score += 1;
                }
            } else {
                history.add(run);
                if !colour {
                    score += history.finder_like() * 40;
                }
                colour = module(i);
                run = 1;
            }
        }
        if colour {
            history.add(run);
            run = 0;
        }
        history.add(run + SIZE);
        score + history.finder_like() * 40
    }
}

/// The last seven run lengths along a line, newest first; the light border
/// before the first run counts as the symbol's width.
#[derive(Default)]
struct RunHistory {
    runs: [usize; 7],
    started: bool,
}

impl RunHistory {
    fn add(&mut self, mut run: usize) {
        if !self.started {
            run += SIZE;
            self.started = true;
        }
        self.runs.rotate_right(1);
        if let Some(first) = self.runs.first_mut() {
            *first = run;
        }
    }

    /// 1:1:3:1:1 dark-light-dark-light-dark with four light on one side.
    fn finder_like(&self) -> u32 {
        let [r0, n, r2, r3, r4, r5, r6] = self.runs;
        let core = n > 0 && r2 == n && r3 == n * 3 && r4 == n && r5 == n;
        u32::from(core && r0 >= n * 4 && r6 >= n) + u32::from(core && r6 >= n * 4 && r0 >= n)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// qrencode 4.1.1's `qrencode -l Q -v 2 -m 0 -t ASCII` of this key, one
    /// character per module (it prints two), rows from the top, `#` dark;
    /// its mask, read from its format bits, was 3. Generated off the build
    /// by a reference encoder td does not carry, so the symbol is checked
    /// against an implementation that is not this one.
    const KEY: &[u8] = b"123456789012345678901234567890123456789012345678";
    const REFERENCE_MASK: u8 = 3;
    const REFERENCE: &[&str] = &[
        "#######  ###  #   #######",
        "#     # # ##  #   #     #",
        "# ### # ##  ### # # ### #",
        "# ### #  ##   ### # ### #",
        "# ### #     ##  # # ### #",
        "#     #  # # #    #     #",
        "####### # # # # # #######",
        "          ####           ",
        " ### ##    #          ## ",
        " ##### ### # #   #   #   ",
        "#     #  #### #  ####### ",
        "  # #  # ##   ### #   ###",
        "#    ##    #   ##      ##",
        " ### #      #######  #  #",
        " ##   # # #  ##     ### #",
        "#    # #   ####  #  ## # ",
        "  ### ###   # #######    ",
        "        ## ### ##   # #  ",
        "#######   ###   # # # # #",
        "#     # ##  ## ##   #  # ",
        "# ### #  ##  ## #########",
        "# ### # ##    ## ###    #",
        "# ### # # #  #  # #  # # ",
        "#     # #   ##   ####  # ",
        "#######  ###### ### #   #",
    ];

    fn rows(code: &Code) -> Vec<String> {
        (0..SIZE)
            .map(|y| {
                (0..SIZE)
                    .map(|x| if code.dark(x, y) { '#' } else { ' ' })
                    .collect()
            })
            .collect()
    }

    /// Every mask's format word, both copies, against ISO/IEC 18004's
    /// table of level Q's eight (bit 14 first): the reference symbol
    /// pins only mask 3.
    #[test]
    fn each_mask_writes_the_standards_format_word_twice() {
        const LEVEL_Q_WORDS: &[u16] = &[
            0x355F, 0x3068, 0x3F31, 0x3A06, 0x24B4, 0x2183, 0x2EDA, 0x2BED,
        ];
        for (mask, word) in (0..8u8).zip(LEVEL_Q_WORDS) {
            let code = with_mask(KEY, mask).unwrap();
            let bit = |i: usize| (word >> i) & 1 != 0;
            // Around the upper-left finder: bits 0 to 7 down column 8,
            // skipping the timing row, and 8 to 14 leftwards along row 8,
            // skipping the timing column.
            let first = [
                (8, 0),
                (8, 1),
                (8, 2),
                (8, 3),
                (8, 4),
                (8, 5),
                (8, 7),
                (8, 8),
                (7, 8),
                (5, 8),
                (4, 8),
                (3, 8),
                (2, 8),
                (1, 8),
                (0, 8),
            ];
            for (i, (x, y)) in first.into_iter().enumerate() {
                assert_eq!(code.dark(x, y), bit(i), "mask {mask} first copy bit {i}");
            }
            // Bits 0 to 7 leftwards from the right edge on row 8, then 8 to
            // 14 down column 8 from under the lower-left finder.
            for i in 0..8 {
                assert_eq!(code.dark(SIZE - 1 - i, 8), bit(i), "mask {mask} bit {i}");
            }
            for i in 8..15 {
                assert_eq!(code.dark(8, SIZE - 15 + i), bit(i), "mask {mask} bit {i}");
            }
            // The dark module beside the lower-left finder.
            assert!(code.dark(8, SIZE - 8));
        }
    }

    #[test]
    fn the_symbol_matches_a_reference_encoder() {
        let code = with_mask(KEY, REFERENCE_MASK).unwrap();
        assert_eq!(rows(&code), REFERENCE);
    }

    #[test]
    fn reed_solomon_matches_the_standards_worked_example() {
        // ISO/IEC 18004 Annex I's 1-M "01234567": 16 data codewords and 10
        // error correction codewords.
        let data = [
            0x10, 0x20, 0x0c, 0x56, 0x61, 0x80, 0xec, 0x11, 0xec, 0x11, 0xec, 0x11, 0xec, 0x11,
            0xec, 0x11,
        ];
        let ecc = remainder(&data, &generator(10));
        assert_eq!(
            ecc,
            [0xa5, 0x24, 0xd4, 0xc1, 0xed, 0x36, 0xc7, 0x87, 0x2c, 0x55]
        );
    }

    #[test]
    fn forty_eight_digits_fill_the_data_codewords_exactly() {
        let data = data_codewords(KEY).unwrap();
        assert_eq!(data.len(), DATA_CODEWORDS);
        // 4 + 10 + 16 * 10 bits, a two-bit terminator, no padding.
        assert_eq!(data[0], 0b0001_0000);
        // The count's last six bits, then 123's first two.
        assert_eq!(data[1], 0b1100_0000);
        assert_eq!(data[21] & 0b11, 0);
        // Fewer digits pad with 0xec, 0x11.
        let short = data_codewords(b"1").unwrap();
        assert_eq!(short.len(), DATA_CODEWORDS);
        assert_eq!(&short[3..5], &[0xec, 0x11]);
    }

    #[test]
    fn only_one_to_forty_eight_digits_encode() {
        assert!(numeric(b"").is_none());
        assert!(numeric(&[b'1'; CAPACITY + 1]).is_none());
        assert!(numeric(b"12-34").is_none());
        assert!(numeric(&[b'9'; CAPACITY]).is_some());
    }

    #[test]
    fn the_chosen_mask_has_the_least_penalty_and_keeps_the_function_patterns() {
        let grid = encode(KEY).unwrap();
        let scores: Vec<u32> = (0..8)
            .map(|mask| {
                let mut trial = grid.clone();
                trial.mask(mask);
                trial.penalty()
            })
            .collect();
        let chosen = numeric(KEY).unwrap();
        let least = scores.iter().min().unwrap();
        let mask = (0..8u8).find(|&m| scores[m as usize] == *least).unwrap();
        assert_eq!(rows(&chosen), rows(&with_mask(KEY, mask).unwrap()));
        // Finders, the timing rows and the dark module are mask-free.
        for corner in [(0, 0), (SIZE - 7, 0), (0, SIZE - 7)] {
            assert!(chosen.dark(corner.0, corner.1));
            assert!(!chosen.dark(corner.0 + 1, corner.1 + 1));
            assert!(chosen.dark(corner.0 + 3, corner.1 + 3));
        }
        assert!(chosen.dark(8, SIZE - 8));
        for i in 8..SIZE - 8 {
            assert_eq!(chosen.dark(i, 6), i % 2 == 0);
            assert_eq!(chosen.dark(6, i), i % 2 == 0);
        }
        assert!(!chosen.dark(SIZE, 0) && !chosen.dark(0, SIZE));
    }
}
