use crate::Encoding;

/// At most four scalars from one input byte (GB18030 error recovery can replay
/// three buffered bytes). Unused slots are private; iteration never exposes them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decoded {
    scalars: [char; 4],
    len: usize,
    pub had_errors: bool,
}
impl Decoded {
    const fn new() -> Self {
        Self {
            scalars: ['\0'; 4],
            len: 0,
            had_errors: false,
        }
    }
    pub fn chars(&self) -> impl Iterator<Item = char> + '_ {
        self.scalars.iter().copied().take(self.len)
    }
    fn emit(&mut self, value: Option<char>) {
        self.had_errors |= value.is_none();
        if let Some(slot) = self.scalars.get_mut(self.len) {
            *slot = value.unwrap_or('\u{fffd}');
            self.len += 1;
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Finished;
impl std::fmt::Display for Finished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("decoder already finished")
    }
}
impl std::error::Error for Finished {}

/// Fixed-state scalar decoder. Feed bytes with push, drain each returned batch,
/// then finish exactly at end of stream. Chunk boundaries have no significance.
/// No normalization, BOM stripping, filtering, I/O or allocation is performed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decoder {
    encoding: Encoding,
    pending: [u8; 4],
    len: usize,
    replacement_seen: bool,
    finished: bool,
}
impl Decoder {
    pub const fn new(encoding: Encoding) -> Self {
        Self {
            encoding,
            pending: [0; 4],
            len: 0,
            replacement_seen: false,
            finished: false,
        }
    }
    pub fn push(&mut self, byte: u8) -> Result<Decoded, Finished> {
        if self.finished {
            return Err(Finished);
        }
        // Every incomplete sequence has at most three bytes.
        if let Some(slot) = self.pending.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        }
        let mut out = Decoded::new();
        while self.len != 0 {
            let Some((consumed, value)) = self.step() else {
                break;
            };
            self.pending.rotate_left(consumed);
            self.len -= consumed;
            if self.encoding != Encoding::Replacement || !self.replacement_seen {
                out.emit(value);
            }
            self.replacement_seen = true;
        }
        Ok(out)
    }
    /// Flush an incomplete sequence as one replacement; subsequent calls are inert.
    pub fn finish(&mut self) -> Decoded {
        let mut out = Decoded::new();
        if self.len != 0 {
            out.emit(None);
            self.len = 0;
        }
        self.finished = true;
        out
    }
    fn step(&self) -> Option<(usize, Option<char>)> {
        let [a, b, c, d] = self.pending;
        if let Some(table) = self.encoding.single_byte() {
            return Some((
                1,
                if a < 128 {
                    Some(char::from(a))
                } else {
                    lookup(table, u32::from(a - 128))
                },
            ));
        }
        match self.encoding {
            Encoding::Utf8 => {
                let bytes = self.pending.get(..self.len)?;
                match std::str::from_utf8(bytes) {
                    Ok(s) => s.chars().next().map(|ch| (ch.len_utf8(), Some(ch))),
                    Err(e) if e.valid_up_to() != 0 => Some((1, Some(char::from(a)))),
                    Err(e) => e.error_len().map(|n| (n, None)),
                }
            }
            Encoding::Utf16Le | Encoding::Utf16Be => {
                if self.len < 2 {
                    return None;
                }
                let unit = if self.encoding == Encoding::Utf16Le {
                    u16::from_le_bytes
                } else {
                    u16::from_be_bytes
                };
                let first = unit([a, b]);
                match first {
                    0xd800..=0xdbff => {
                        if self.len < 4 {
                            return None;
                        }
                        let second = unit([c, d]);
                        if (0xdc00..=0xdfff).contains(&second) {
                            Some((
                                4,
                                char::from_u32(
                                    0x10000
                                        + ((u32::from(first) - 0xd800) << 10)
                                        + u32::from(second)
                                        - 0xdc00,
                                ),
                            ))
                        } else {
                            Some((2, None))
                        }
                    }
                    _ => Some((2, char::from_u32(u32::from(first)))),
                }
            }
            Encoding::Replacement => Some((1, None)),
            Encoding::UserDefined => Some((
                1,
                char::from_u32(if a < 128 {
                    u32::from(a)
                } else {
                    0xf780 + u32::from(a - 128)
                }),
            )),
            Encoding::ShiftJis => {
                if a <= 0x80 {
                    return Some((1, Some(char::from(a))));
                }
                if (0xa1..=0xdf).contains(&a) {
                    return Some((1, char::from_u32(0xff61 + u32::from(a - 0xa1))));
                }
                if !(0x81..=0x9f).contains(&a) && !(0xe0..=0xfc).contains(&a) {
                    return Some((1, None));
                }
                if self.len < 2 {
                    return None;
                }
                let value = if (0x40..=0x7e).contains(&b) || (0x80..=0xfc).contains(&b) {
                    let pointer = u32::from(a - if a < 0xa0 { 0x81 } else { 0xc1 }) * 188
                        + u32::from(b - if b < 0x7f { 0x40 } else { 0x41 });
                    if (8836..=10715).contains(&pointer) {
                        char::from_u32(0xe000 + pointer - 8836)
                    } else {
                        lookup(include_bytes!("../data/jis0208.bin"), pointer)
                    }
                } else {
                    None
                };
                Some((if value.is_none() && b < 128 { 1 } else { 2 }, value))
            }
            Encoding::EucKr | Encoding::Gbk | Encoding::Gb18030 => {
                if a < 128 {
                    return Some((1, Some(char::from(a))));
                }
                let korean = self.encoding == Encoding::EucKr;
                if !korean && a == 0x80 {
                    return Some((1, Some('€')));
                }
                if !(0x81..=0xfe).contains(&a) {
                    return Some((1, None));
                }
                if self.len < 2 {
                    return None;
                }
                if !korean && b.is_ascii_digit() {
                    if self.len < 3 {
                        return None;
                    }
                    if !(0x81..=0xfe).contains(&c) {
                        return Some((1, None));
                    }
                    if self.len < 4 {
                        return None;
                    }
                    if !d.is_ascii_digit() {
                        return Some((1, None));
                    }
                    let pointer = u32::from(a - 0x81) * 12600
                        + u32::from(b - 0x30) * 1260
                        + u32::from(c - 0x81) * 10
                        + u32::from(d - 0x30);
                    return Some((4, gb_range(pointer)));
                }
                let value = if korean {
                    if (0x41..=0xfe).contains(&b) {
                        lookup(
                            include_bytes!("../data/euc-kr.bin"),
                            u32::from(a - 0x81) * 190 + u32::from(b - 0x41),
                        )
                    } else {
                        None
                    }
                } else if (0x40..=0x7e).contains(&b) || (0x80..=0xfe).contains(&b) {
                    lookup(
                        include_bytes!("../data/gb18030.bin"),
                        u32::from(a - 0x81) * 190
                            + u32::from(b - if b < 0x7f { 0x40 } else { 0x41 }),
                    )
                } else {
                    None
                };
                Some((if value.is_none() && b < 128 { 1 } else { 2 }, value))
            }
            // All remaining variants were handled by single_byte above.
            _ => Some((1, None)),
        }
    }
}
fn word(bytes: &[u8], position: usize) -> Option<u32> {
    let start = position.checked_mul(4)?;
    let bytes: [u8; 4] = bytes.get(start..start.checked_add(4)?)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}
fn lookup(bytes: &[u8], pointer: u32) -> Option<char> {
    char::from_u32(word(bytes, usize::try_from(pointer).ok()?)?)
}
fn gb_range(pointer: u32) -> Option<char> {
    if (39420..189000).contains(&pointer) || pointer > 1237575 {
        return None;
    }
    if pointer == 7457 {
        return Some('\u{e7c7}');
    }
    let data = include_bytes!("../data/gb18030-ranges.bin");
    let mut low = 0;
    let mut high = data.len() / 8;
    while low < high {
        let middle = low + (high - low) / 2;
        if word(data, middle * 2)? <= pointer {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let index = low.checked_sub(1)? * 2;
    char::from_u32(word(data, index + 1)? + pointer - word(data, index)?)
}
