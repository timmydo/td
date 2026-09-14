//! Fixed-capacity display text; formatting cannot grow model memory.
use std::fmt::{self, Write};
#[derive(Clone, Copy, Debug)]
pub struct Text<const N: usize> {
    bytes: [u8; N],
    length: usize,
}
impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self {
            bytes: [0; N],
            length: 0,
        }
    }
}
impl<const N: usize> Text<N> {
    pub fn new(text: &str) -> Self {
        let mut this = Self::default();
        let _ = this.write_str(text);
        this
    }
    pub fn as_str(&self) -> &str {
        self.bytes
            .get(..self.length)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or("")
    }
    pub fn clear(&mut self) {
        self.length = 0;
    }
    pub fn truncated(text: &str) -> Self {
        if text.len() <= N {
            return Self::new(text);
        }
        let mut this = Self::default();
        if N < 3 {
            return this;
        }
        for ch in text.chars() {
            if this.length + ch.len_utf8() > N - 3 {
                break;
            }
            let _ = this.write_char(ch);
        }
        let _ = this.write_str("...");
        this
    }
    pub fn number(value: Option<u64>) -> Self {
        let mut this = Self::default();
        match value {
            Some(v) => {
                let _ = write!(this, "{v}");
            }
            None => {
                let _ = this.write_str("Unavailable");
            }
        }
        this
    }
    pub fn percent(value: Option<u64>, partial: bool) -> Self {
        let mut this = Self::default();
        match value {
            Some(v) => {
                let _ = write!(
                    this,
                    "{}.{:02}%{}",
                    v / 100,
                    v % 100,
                    if partial { "*" } else { "" }
                );
            }
            None => {
                let _ = this.write_str("Unavailable");
            }
        }
        this
    }
    pub fn bytes(value: Option<u64>, partial: bool) -> Self {
        let mut this = Self::default();
        if let Some(v) = value {
            let (unit, divisor) = if v >= 1 << 40 {
                ("TiB", 1u64 << 40)
            } else if v >= 1 << 30 {
                ("GiB", 1 << 30)
            } else if v >= 1 << 20 {
                ("MiB", 1 << 20)
            } else if v >= 1 << 10 {
                ("KiB", 1 << 10)
            } else {
                ("B", 1)
            };
            let _ = write!(
                this,
                "{}.{:01} {unit}{}",
                v / divisor,
                u128::from(v % divisor) * 10 / u128::from(divisor),
                if partial { "*" } else { "" }
            );
        } else {
            let _ = this.write_str("Unavailable");
        }
        this
    }
}
impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self
            .length
            .checked_add(text.len())
            .filter(|end| *end <= N)
            .ok_or(fmt::Error)?;
        self.bytes
            .get_mut(self.length..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}
