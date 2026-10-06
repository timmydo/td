//! Caller-supplied monotonic ticks; no ambient clock or runtime scheduling.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Tick(pub u64);
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline(Tick);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Overflow;
impl std::fmt::Display for Overflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("monotonic deadline overflow")
    }
}
impl std::error::Error for Overflow {}
impl Deadline {
    pub fn after(now: Tick, milliseconds: u64) -> Result<Self, Overflow> {
        now.0
            .checked_add(milliseconds)
            .map(|v| Self(Tick(v)))
            .ok_or(Overflow)
    }
    pub const fn at(tick: Tick) -> Self {
        Self(tick)
    }
    pub const fn tick(self) -> Tick {
        self.0
    }
    pub fn expired(self, now: Tick) -> bool {
        now >= self.0
    }
}
