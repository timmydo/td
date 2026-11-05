//! Streaming Gear boundaries over stored image bytes, not decoded file contents.
pub const MIN: usize = 16 * 1024;
pub const TARGET: usize = 64 * 1024;
pub const MAX: usize = 256 * 1024;

pub fn gear(byte: u8) -> u64 {
    let mut n = u64::from(byte).wrapping_add(0x9e3779b97f4a7c15);
    n = (n ^ (n >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    n = (n ^ (n >> 27)).wrapping_mul(0x94d049bb133111eb);
    n ^ (n >> 31)
}

#[derive(Default)]
pub struct Chunker {
    rolling: u64,
    length: usize,
}

impl Chunker {
    /// True includes this byte in the completed chunk and resets the state.
    pub fn push(&mut self, byte: u8) -> bool {
        self.rolling = self.rolling.wrapping_shl(1).wrapping_add(gear(byte));
        self.length += 1;
        if self.length == MAX || (self.length >= MIN && self.rolling & (TARGET as u64 - 1) == 0) {
            self.rolling = 0;
            self.length = 0;
            true
        } else {
            false
        }
    }
}
