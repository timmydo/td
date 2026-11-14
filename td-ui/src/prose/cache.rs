//! A bounded open-addressed measurement cache. Refusal disables further
//! allocation attempts; existing storage remains usable and answers stay exact.
use super::{Source, Style, TextFamily, MAX_MEASURES};
use td_alloc::{Budget, BudgetVec};
type Key = (TextFamily, Style, char);
type Value = (Source, u32);
#[derive(Clone, Copy, Debug)]
struct Entry {
    key: Key,
    value: Value,
}
#[derive(Debug, Default)]
pub(super) struct Cache {
    slots: Option<BudgetVec<Option<Entry>>>,
    used: usize,
    refused: bool,
}
impl Clone for Cache {
    fn clone(&self) -> Self {
        // A clone need not duplicate an optional cache's backing storage.
        self.slots.as_ref().map_or_else(
            || Self {
                refused: self.refused,
                ..Self::default()
            },
            |s| Self::new(s.budget()),
        )
    }
}
fn index(key: Key, size: usize) -> usize {
    let (family, style, scalar) = key;
    let hash = (scalar as u32).wrapping_mul(0x9e3779b1)
        ^ (family as u32).wrapping_mul(0x85ebca6b)
        ^ (style as u32).wrapping_mul(0xc2b2ae35);
    (hash as usize) & (size - 1)
}
impl Cache {
    pub(super) fn new(budget: &Budget) -> Self {
        match BudgetVec::new(budget) {
            Ok(slots) => Self {
                slots: Some(slots),
                used: 0,
                refused: false,
            },
            Err(_) => Self {
                refused: true,
                ..Self::default()
            },
        }
    }
    pub(super) fn get(&self, key: Key) -> Option<Value> {
        let slots = self.slots.as_ref()?;
        if slots.is_empty() {
            return None;
        }
        let mut at = index(key, slots.len());
        for _ in 0..slots.len() {
            let entry = slots.get(at)?.as_ref()?;
            if entry.key == key {
                return Some(entry.value);
            }
            at = (at + 1) & (slots.len() - 1);
        }
        None
    }
    pub(super) fn insert(&mut self, key: Key, value: Value) {
        if self.slots.is_none() && !self.refused {
            let limit =
                4 * MAX_MEASURES * std::mem::size_of::<Option<Entry>>() + Budget::metadata_bytes();
            match Budget::new(limit).and_then(|b| BudgetVec::new(&b)) {
                Ok(slots) => self.slots = Some(slots),
                Err(_) => self.refused = true,
            }
        }
        let Some(slots) = &mut self.slots else {
            return;
        };
        if self.used >= slots.len() / 2 {
            if !self.refused && slots.len() < 2 * MAX_MEASURES {
                let size = (slots.len() * 2).max(128);
                match BudgetVec::filled(slots.budget(), size, None) {
                    Ok(mut next) => {
                        for entry in slots.iter().flatten() {
                            put(&mut next, *entry);
                        }
                        *slots = next;
                    }
                    Err(_) => self.refused = true,
                }
            }
            if self.used >= slots.len() / 2 {
                // Forgetting measurements changes speed, never glyph placement.
                slots.fill(None);
                self.used = 0;
            }
        }
        if !slots.is_empty() {
            put(slots, Entry { key, value });
            self.used += 1;
        }
    }
}
fn put(slots: &mut [Option<Entry>], entry: Entry) {
    let mut at = index(entry.key, slots.len());
    for _ in 0..slots.len() {
        if let Some(slot) = slots.get_mut(at) {
            if slot.is_none() {
                *slot = Some(entry);
                return;
            }
        }
        at = (at + 1) & (slots.len() - 1);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn collisions_growth_and_restarts_never_return_another_keys_measurement() {
        let budget = Budget::new(2 << 20).unwrap();
        let base = budget.used();
        let mut cache = Cache::new(&budget);
        for offset in 0..3 {
            for n in 0..(MAX_MEASURES * 2) as u32 {
                let scalar = char::from_u32(32 + n).unwrap_or('x');
                let family = if n % 2 == 0 {
                    TextFamily::Sans
                } else {
                    TextFamily::Serif
                };
                let key = (family, Style::Regular, scalar);
                let value = (Source::Family, n + offset);
                // Insert only on a miss, as Metrics does.
                if let Some(found) = cache.get(key) {
                    assert_eq!(found.1, n + offset);
                } else {
                    cache.insert(key, value);
                }
                assert_eq!(cache.get(key), Some(value));
            }
            cache = Cache::new(&budget);
            assert_eq!(budget.used(), base);
        }
    }
}
