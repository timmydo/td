//! Document-local interning, including independently shared custom maps.
use crate::syntax::{Component, Token};
use crate::values::{ComputedStyle, ComputedValue, Family, Length};
use crate::variables::CustomProperties;
use crate::Error;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use td_alloc::{Budget, BudgetError, BudgetVec};

pub const MAX_STYLES: usize = 16_384;
pub const MAX_POOL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_POOL_WORK: usize = 67_108_864;
pub const MAX_STYLE_COMPARISONS: usize = 1_048_576;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyleId(usize);
#[derive(Clone, Copy, Debug)]
pub struct PoolLimits {
    pub styles: usize,
    pub bytes: usize,
    pub work: usize,
    pub comparisons: usize,
    /// The fingerprint key; None takes one from addresses ASLR places,
    /// with no system call.
    pub seed: Option<u64>,
}
impl Default for PoolLimits {
    fn default() -> Self {
        Self {
            styles: MAX_STYLES,
            bytes: MAX_POOL_BYTES,
            work: MAX_POOL_WORK,
            comparisons: MAX_STYLE_COMPARISONS,
            seed: None,
        }
    }
}
/// Charges include doubled vector/string capacities and a conservative
/// allowance for each index entry. Pointer identities are only keys for
/// allocations this pool owns.
pub struct StylePool {
    styles: BudgetVec<ComputedStyle>,
    buckets: Table<BudgetVec<StyleId>>,
    custom: Table<BudgetVec<CustomProperties>>,
    /// Canonical custom maps and their token arrays, by address.
    maps: Table<()>,
    tokens: Table<()>,
    limits: PoolLimits,
    bytes: usize,
    work: usize,
    comparisons: usize,
    /// This pool's fingerprint key, so a page cannot be built offline to
    /// collide in it.
    seed: u64,
}
impl StylePool {
    pub fn new(limits: PoolLimits) -> Result<Self, Error> {
        let memory = Budget::new(
            limits
                .bytes
                .min(MAX_POOL_BYTES)
                .saturating_add(Budget::metadata_bytes()),
        )?;
        Self::with_budget(limits, &memory)
    }
    pub fn with_budget(limits: PoolLimits, memory: &Budget) -> Result<Self, Error> {
        let seed = limits.seed.unwrap_or_else(|| address_key(memory));
        Ok(Self {
            styles: BudgetVec::new(memory)?,
            buckets: Table::new(memory, seed)?,
            custom: Table::new(memory, seed)?,
            maps: Table::new(memory, seed)?,
            tokens: Table::new(memory, seed)?,
            limits: PoolLimits {
                styles: limits.styles.min(MAX_STYLES),
                bytes: limits.bytes.min(MAX_POOL_BYTES),
                work: limits.work.min(MAX_POOL_WORK),
                comparisons: limits.comparisons.min(MAX_STYLE_COMPARISONS),
                seed: limits.seed,
            },
            bytes: 0,
            work: 0,
            comparisons: 0,
            seed,
        })
    }
    pub fn get(&self, id: StyleId) -> Option<&ComputedStyle> {
        self.styles.get(id.0)
    }
    pub fn len(&self) -> usize {
        self.styles.len()
    }
    pub fn is_empty(&self) -> bool {
        self.styles.is_empty()
    }
    pub fn charged_bytes(&self) -> usize {
        self.bytes
    }
    pub fn work(&self) -> usize {
        self.work
    }
    pub fn intern(&mut self, mut style: ComputedStyle) -> Result<StyleId, Error> {
        self.share_custom(&mut style.custom)?;
        let mut hash = Fingerprint::new(self.seed);
        // Canonical maps have a stable address while retained by this pool.
        (std::ptr::from_ref(&*style.custom.entries).addr()).hash(&mut hash);
        let value_work = style.values.values().map(value_work).sum::<usize>();
        self.visit(value_work)?;
        for (property, value) in &style.values {
            (*property as u8).hash(&mut hash);
            hash_value(value, &mut hash);
        }
        let key = hash.finish();
        if let Some(bucket) = self.buckets.get(key, &mut self.work, self.limits.work)? {
            for id in bucket {
                comparison(&mut self.comparisons, self.limits.comparisons)?;
                visit_count(&mut self.work, value_work, self.limits.work)?;
                if self.styles.get(id.0).is_some_and(|old| {
                    std::ptr::eq(&*old.custom.entries, &*style.custom.entries)
                        && old.values == style.values
                }) {
                    return Ok(*id);
                }
            }
        }
        if self.styles.len() >= self.limits.styles {
            return Err(Error::Limit("MAX_STYLES"));
        }
        let mut charge = 256 + 2 * size_of::<ComputedStyle>();
        for value in style.values.values() {
            charge += 128 + 2 * size_of::<ComputedValue>();
            if let ComputedValue::FontFamily(families) = value {
                charge += 2 * families.capacity() * size_of::<Family>();
                for family in families.iter() {
                    if let Family::Named(name) = family {
                        charge += 2 * name.heap_capacity() + 32;
                    }
                }
            }
        }
        self.charge(charge)?;
        let id = StyleId(self.styles.len());
        let bucket = self.buckets.get_or_insert_with(
            key,
            &mut self.work,
            self.limits.work,
            BudgetVec::new,
        )?;
        BudgetVec::try_reserve(bucket, 1)?;
        self.styles.try_push(style)?;
        bucket
            .push_within_capacity(id)
            .map_err(|_| Error::InvalidContext)?;
        Ok(id)
    }
    fn visit(&mut self, count: usize) -> Result<(), Error> {
        self.work = self
            .work
            .checked_add(count)
            .filter(|n| *n <= self.limits.work)
            .ok_or(Error::Limit("MAX_POOL_WORK"))?;
        Ok(())
    }
    fn charge(&mut self, count: usize) -> Result<(), Error> {
        self.bytes = self
            .bytes
            .checked_add(count)
            .filter(|n| *n <= self.limits.bytes)
            .ok_or(Error::Limit("MAX_POOL_BYTES"))?;
        Ok(())
    }
    fn share_custom(&mut self, custom: &mut CustomProperties) -> Result<(), Error> {
        let identity = std::ptr::from_ref(&*custom.entries).addr() as u64;
        if self
            .maps
            .get(identity, &mut self.work, self.limits.work)?
            .is_some()
        {
            return Ok(());
        }
        // Keep the existing conservative admission model alongside exact
        // owned layouts, including both interning indexes.
        let mut charge = 2048;
        let work_before = self.work;
        for (name, entry) in custom.entries.iter() {
            self.visit(name.len() + 1)?;
            charge += 160 + 2 * name.heap_capacity();
            if let Some(tokens) = &entry.value {
                let token_id = tokens.as_ptr() as usize as u64;
                // Count traversal even for shared tokens: hashing sees them.
                let token_charge = components_charge(tokens, &mut self.work, self.limits.work)?;
                if self
                    .tokens
                    .get(token_id, &mut self.work, self.limits.work)?
                    .is_none()
                {
                    charge += token_charge + 128;
                }
            }
        }
        let custom_work = self.work - work_before;
        let key = map_key(self.seed, custom);
        if let Some(bucket) = self.custom.get(key, &mut self.work, self.limits.work)? {
            for old in bucket {
                comparison(&mut self.comparisons, self.limits.comparisons)?;
                visit_count(&mut self.work, custom_work, self.limits.work)?;
                if old == custom {
                    *custom = old.try_clone()?;
                    return Ok(());
                }
            }
        }
        // A novel custom map cannot equal any retained complete style.
        if self.styles.len() >= self.limits.styles {
            return Err(Error::Limit("MAX_STYLES"));
        }
        self.charge(charge)?;
        // Retain ownership before publishing pointer identities. A later
        // refusal may leave a valid interning entry, never a dangling key.
        self.custom
            .get_or_insert_with(key, &mut self.work, self.limits.work, BudgetVec::new)?
            .try_push(custom.try_clone()?)?;
        self.maps
            .get_or_insert_with(identity, &mut self.work, self.limits.work, |_| Ok(()))?;
        for tokens in custom.entries.values().filter_map(|e| e.value.as_ref()) {
            let identity = tokens.as_ptr() as usize as u64;
            self.tokens
                .get_or_insert_with(identity, &mut self.work, self.limits.work, |_| Ok(()))?;
        }
        Ok(())
    }
}
/// Visits a lookup or an insertion is charged, whatever it probes: about
/// what one probes on average in a table at most half full, so the pool's
/// work is a function of the page, not of the key or the addresses it
/// indexes.
const OPERATION: usize = 2;
/// The probes a table may take: this many an operation, and PROBE_SLACK
/// more, past which it refuses `Limit("MAX_POOL_WORK")` before reading.
const PROBE_CAP: usize = 8;
const PROBE_SLACK: usize = 4096;
/// An open-addressed table keyed by u64, at most half full, its slots
/// chosen by the key mixed twice with the pool's seed. A lookup or an
/// insertion is charged OPERATION visits before it probes; its probes,
/// counted apart, are capped at PROBE_CAP an operation plus PROBE_SLACK,
/// refused before the probe that would pass it, so the bound is real and
/// the charge deterministic. Growth doubles the table, charged its new
/// size before anything is allocated or moved, so a refusal leaves it
/// whole; the move's probes count against the cap too, after it, so the
/// next probe refuses if they passed it. Iteration order is the slots',
/// on which no result depends.
struct Table<V> {
    slots: BudgetVec<Option<(u64, V)>>,
    len: usize,
    seed: u64,
    operations: usize,
    probes: usize,
}
impl<V> Table<V> {
    fn new(memory: &Budget, seed: u64) -> Result<Self, Error> {
        Ok(Self {
            slots: BudgetVec::new(memory)?,
            len: 0,
            seed,
            operations: 0,
            probes: 0,
        })
    }
    /// Two rounds of the fingerprint's mixing, the seed in each: with one,
    /// a slot would be a fixed function of the key XOR the seed.
    fn home(&self, key: u64) -> usize {
        let mut mixed = Fingerprint::new(self.seed);
        mixed.add(key);
        mixed.add(0);
        mixed.finish() as usize
    }
    /// One operation, charged and counted.
    fn operation(&mut self, work: &mut usize, max: usize) -> Result<(), Error> {
        visit_count(work, OPERATION, max)?;
        self.operations = self.operations.saturating_add(1);
        Ok(())
    }
    /// The slot holding `key`, else the empty one it would take; each
    /// probe counted against the cap before it is read.
    fn probe(&mut self, key: u64) -> Result<Result<usize, usize>, Error> {
        if self.slots.is_empty() {
            return Ok(Err(0));
        }
        let mask = self.slots.len() - 1;
        let cap = self
            .operations
            .saturating_mul(PROBE_CAP)
            .saturating_add(PROBE_SLACK);
        let mut at = self.home(key) & mask;
        loop {
            if self.probes >= cap {
                return Err(Error::Limit("MAX_POOL_WORK"));
            }
            self.probes += 1;
            match self.slots.get(at) {
                Some(Some((stored, _))) if *stored == key => return Ok(Ok(at)),
                Some(Some(_)) => at = (at + 1) & mask,
                Some(None) => return Ok(Err(at)),
                None => return Err(Error::InvalidContext),
            }
        }
    }
    fn get(&mut self, key: u64, work: &mut usize, max: usize) -> Result<Option<&V>, Error> {
        self.operation(work, max)?;
        Ok(match self.probe(key)? {
            Ok(at) => self.slots.get(at).and_then(Option::as_ref).map(|(_, v)| v),
            Err(_) => None,
        })
    }
    fn get_or_insert_with(
        &mut self,
        key: u64,
        work: &mut usize,
        max: usize,
        make: impl FnOnce(&Budget) -> Result<V, BudgetError>,
    ) -> Result<&mut V, Error> {
        self.operation(work, max)?;
        let at = match self.probe(key)? {
            Ok(at) => at,
            Err(at) => {
                self.operation(work, max)?;
                let at = if self.reserve_one(work, max)? {
                    match self.probe(key)? {
                        Err(at) => at,
                        Ok(_) => return Err(Error::InvalidContext),
                    }
                } else {
                    at
                };
                let value = make(self.slots.budget())?;
                let slot = self.slots.get_mut(at).ok_or(Error::InvalidContext)?;
                *slot = Some((key, value));
                self.len += 1;
                at
            }
        };
        self.slots
            .get_mut(at)
            .and_then(Option::as_mut)
            .map(|(_, value)| value)
            .ok_or(Error::InvalidContext)
    }
    /// Room for one more entry with the table at most half full; whether
    /// it grew. The charge and the allocation precede the move.
    fn reserve_one(&mut self, work: &mut usize, max: usize) -> Result<bool, Error> {
        if self.len.saturating_add(1).saturating_mul(2) <= self.slots.len() {
            return Ok(false);
        }
        let capacity = self.slots.len().saturating_mul(2).max(16);
        visit_count(work, capacity, max)?;
        // td-alloc's constructor, written in full as the allocation guard
        // admits it.
        let mut fresh = td_alloc::BudgetVec::with_capacity(self.slots.budget(), capacity)?;
        for _ in 0..capacity {
            fresh
                .push_within_capacity(None)
                .map_err(|_| Error::InvalidContext)?;
        }
        let mask = capacity - 1;
        let old = std::mem::replace(&mut self.slots, fresh);
        // Counted, not refused: a move stopped halfway would lose entries.
        let mut probes = 0usize;
        for (key, value) in old.into_iter().flatten() {
            let mut at = self.home(key) & mask;
            probes = probes.saturating_add(1);
            while self.slots.get(at).is_some_and(Option::is_some) {
                at = (at + 1) & mask;
                probes = probes.saturating_add(1);
            }
            if let Some(slot) = self.slots.get_mut(at) {
                *slot = Some((key, value));
            }
        }
        self.probes = self.probes.saturating_add(probes);
        Ok(true)
    }
}
/// A custom map's bucket key, keyed as styles are: maps that collide
/// would have to be built against this pool's seed.
fn map_key(seed: u64, custom: &CustomProperties) -> u64 {
    let mut hasher = Fingerprint::new(seed);
    custom.entries.hash(&mut hasher);
    hasher.finish()
}
fn visit_count(work: &mut usize, amount: usize, max: usize) -> Result<(), Error> {
    *work = work
        .checked_add(amount)
        .filter(|n| *n <= max)
        .ok_or(Error::Limit("MAX_POOL_WORK"))?;
    Ok(())
}
fn value_work(value: &ComputedValue) -> usize {
    1 + match value {
        ComputedValue::FontFamily(families) => families
            .iter()
            .map(|family| match family {
                Family::Generic(s) => s.len(),
                Family::Named(s) => s.len(),
            })
            .sum(),
        _ => 0,
    }
}
fn comparison(count: &mut usize, max: usize) -> Result<(), Error> {
    *count = count
        .checked_add(1)
        .filter(|n| *n <= max)
        .ok_or(Error::Limit("MAX_STYLE_COMPARISONS"))?;
    Ok(())
}
fn components_charge(values: &[Component], work: &mut usize, max: usize) -> Result<usize, Error> {
    let mut bytes = 2 * size_of_val(values);
    for value in values {
        *work = work
            .checked_add(1)
            .filter(|n| *n <= max)
            .ok_or(Error::Limit("MAX_POOL_WORK"))?;
        let string = |s: &crate::syntax::Text| 2 * s.heap_capacity() + 32;
        let payload = match value {
            Component::Function { name, .. } => name.len(),
            Component::Token(
                Token::Ident(s)
                | Token::Function(s)
                | Token::AtKeyword(s)
                | Token::String(s)
                | Token::Url(s)
                | Token::Hash { value: s, .. },
            ) => s.len(),
            Component::Token(Token::Number(n) | Token::Percentage(n)) => n.representation.len(),
            Component::Token(Token::Dimension(n, unit)) => n.representation.len() + unit.len(),
            _ => 0,
        };
        visit_count(work, payload, max)?;
        bytes += match value {
            Component::Function { name, values } => {
                string(name)
                    + components_charge(values, work, max)?
                    + 2 * (values.capacity() - values.len()) * size_of::<Component>()
            }
            Component::Block { values, .. } => {
                components_charge(values, work, max)?
                    + 2 * (values.capacity() - values.len()) * size_of::<Component>()
            }
            Component::Token(token) => match token {
                Token::Ident(s)
                | Token::Function(s)
                | Token::AtKeyword(s)
                | Token::String(s)
                | Token::Url(s)
                | Token::Hash { value: s, .. } => string(s),
                Token::Number(n) | Token::Percentage(n) => string(&n.representation),
                Token::Dimension(n, unit) => string(&n.representation) + string(unit),
                _ => 0,
            },
        };
    }
    Ok(bytes)
}
/// A key from where ASLR placed the pool's budget, this call's frame and
/// a static, mixed as fingerprints are: as hard to guess as those
/// addresses, and needing no system call, which RandomState would make
/// and std would panic on failing. A caller with entropy passes its own.
fn address_key(memory: &Budget) -> u64 {
    static ANCHOR: u8 = 0;
    let local = 0u8;
    let mut key = Fingerprint::new(0x243f_6a88_85a3_08d3);
    key.add(std::ptr::from_ref(memory) as usize as u64);
    key.add(std::ptr::from_ref(&local) as usize as u64);
    key.add(std::ptr::from_ref(&ANCHOR) as usize as u64);
    key.finish()
}
/// A style's bucket key: each word, with the state and the pool's key,
/// multiplied out to 128 bits and its halves folded (as wyhash's mum
/// step), far cheaper than SipHash over a style's 230 or so small writes.
/// The fold has no input difference that survives with certainty, and
/// the key is unknown to a page, so colliding styles must be found by
/// chance; a collided bucket is still bounded by MAX_STYLE_COMPARISONS and
/// MAX_POOL_WORK, and equality, not the key, decides sharing.
struct Fingerprint {
    state: u64,
    seed: u64,
}
impl Fingerprint {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    /// The key enters with every word, the first included.
    fn new(seed: u64) -> Self {
        Self { state: 0, seed }
    }
    fn add(&mut self, word: u64) {
        let product = u128::from(self.state ^ word ^ self.seed) * u128::from(Self::K);
        self.state = (product >> 64) as u64 ^ product as u64;
    }
}
impl Hasher for Fingerprint {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            if let Some(head) = word.get_mut(..rest.len()) {
                head.copy_from_slice(rest);
            }
            self.add(u64::from_le_bytes(word));
        }
        // The length, so a zero-padded tail is no full word's equal.
        self.add(bytes.len() as u64);
    }
    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }
    fn write_u16(&mut self, n: u16) {
        self.add(u64::from(n));
    }
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }
    fn finish(&self) -> u64 {
        self.state
    }
}
fn hash_number(value: f64, hash: &mut impl Hasher) {
    // PartialEq identifies signed zero; its fingerprints must agree too.
    (if value == 0.0 { 0 } else { value.to_bits() }).hash(hash);
}
fn hash_value(value: &ComputedValue, hash: &mut impl Hasher) {
    std::mem::discriminant(value).hash(hash);
    match value {
        ComputedValue::Number(n) => hash_number(*n, hash),
        ComputedValue::Keyword(s) => s.hash(hash),
        ComputedValue::Color(c) => (c.red, c.green, c.blue, c.alpha).hash(hash),
        ComputedValue::Length(length) => {
            std::mem::discriminant(length).hash(hash);
            match length {
                Length::Px(n) | Length::Percent(n) => hash_number(*n, hash),
                Length::Calc {
                    px,
                    percent,
                    non_negative,
                } => {
                    hash_number(*px, hash);
                    hash_number(*percent, hash);
                    non_negative.hash(hash);
                }
                Length::Auto => {}
            }
        }
        ComputedValue::FontFamily(families) => {
            for family in families.iter() {
                std::mem::discriminant(family).hash(hash);
                match family {
                    Family::Generic(s) => s.hash(hash),
                    Family::Named(s) => s.hash(hash),
                }
            }
        }
        ComputedValue::Weight(w) => w.hash(hash),
        ComputedValue::Decoration(d) => (d.underline, d.overline, d.line_through).hash(hash),
        ComputedValue::Spacing {
            horizontal,
            vertical,
        } => {
            hash_number(*horizontal, hash);
            hash_number(*vertical, hash);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn fingerprints_tell_apart_lengths_orders_and_values() {
        let key = |f: &dyn Fn(&mut Fingerprint)| {
            let mut h = Fingerprint::new(0x1234);
            f(&mut h);
            h.finish()
        };
        let keys = [
            key(&|h| h.write(b"a")),
            key(&|h| h.write(b"a\0")),
            key(&|h| h.write(b"abcdefgh")),
            key(&|h| h.write(b"abcdefgh\0")),
            key(&|h| "auto".hash(h)),
            key(&|h| "none".hash(h)),
            key(&|h| (1u8, 2u8).hash(h)),
            key(&|h| (2u8, 1u8).hash(h)),
            key(&|h| hash_number(0.0, h)),
            key(&|h| hash_number(1.0, h)),
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in keys.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        // Equal values, equal keys: -0 and 0 compare equal.
        assert_eq!(
            key(&|h| hash_number(-0.0, h)),
            key(&|h| hash_number(0.0, h))
        );
    }
    /// Styles of the test tree's element 3 under each inline declaration.
    fn styles(declarations: &[String]) -> Vec<ComputedStyle> {
        let source = crate::StyleSet::new(&[]).unwrap();
        let nodes = crate::test_tree::tree();
        let element = crate::test_tree::Handle {
            nodes: &nodes,
            index: 3,
        };
        declarations
            .iter()
            .map(|d| source.compute(&element, None, 16.0, d).unwrap())
            .collect()
    }
    /// Pool work and comparisons interning `declarations`' styles.
    fn interned(declarations: &[String]) -> (usize, usize) {
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        for style in styles(declarations) {
            pool.intern(style).unwrap();
        }
        (pool.work, pool.comparisons)
    }
    impl<V> Table<V> {
        fn is_empty(&self) -> bool {
            self.len == 0
        }
        fn keys(&self) -> impl Iterator<Item = u64> + '_ {
            self.slots.iter().flatten().map(|(key, _)| *key)
        }
        fn values(&self) -> impl Iterator<Item = &V> {
            self.slots.iter().flatten().map(|(_, value)| value)
        }
        fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
            self.slots.iter_mut().flatten().map(|(_, value)| value)
        }
        /// The most occupied slots in a row, wrapping.
        fn longest_run(&self) -> usize {
            let (mut run, mut longest) = (0, 0);
            for slot in self.slots.iter().chain(self.slots.iter()) {
                run = if slot.is_some() { run + 1 } else { 0 };
                longest = longest.max(run);
            }
            longest.min(self.slots.len())
        }
        /// Keys `seed` sends to slot 0 of every table up to 65,536 slots.
        fn colliding(seed: u64, count: usize) -> Vec<u64> {
            let memory = Budget::new(1 << 10).unwrap();
            let table = Table::<()>::new(&memory, seed).unwrap();
            (0..u64::MAX)
                .filter(|k| table.home(*k) & 0xffff == 0)
                .take(count)
                .collect()
        }
        /// Each key's slot, in key order.
        fn places(&self, keys: &[u64]) -> Vec<usize> {
            keys.iter()
                .map(|k| {
                    self.slots
                        .iter()
                        .position(|s| s.as_ref().is_some_and(|(key, _)| key == k))
                        .unwrap()
                })
                .collect()
        }
    }
    #[test]
    fn distinct_custom_maps_intern_in_linear_work() {
        // Each map new, each with its own token array: an insertion costs a
        // fixed charge, not one for every entry before it (32 million
        // visits for these).
        let declarations: Vec<String> = (0..4000).map(|i| format!("--a:{i}")).collect();
        let (work, comparisons) = interned(&declarations);
        assert!(work < 4000 * 128, "{work}");
        assert!(comparisons <= 4000, "{comparisons}");
    }
    #[test]
    fn a_table_keeps_every_key_and_charges_a_function_of_its_operations() {
        let memory = Budget::new(16 << 20).unwrap();
        let max = usize::MAX;
        let keys: Vec<u64> = (0..10_000u64)
            .map(|i| i * 16)
            .chain([u64::MAX, 1, 7])
            .collect();
        let mut works = Vec::new();
        for seed in [99, 100] {
            let mut table = Table::new(&memory, seed).unwrap();
            let mut work = 0;
            for (n, key) in keys.iter().enumerate() {
                *table
                    .get_or_insert_with(*key, &mut work, max, |_| Ok(0usize))
                    .unwrap() = n;
            }
            for (n, key) in keys.iter().enumerate() {
                assert_eq!(table.get(*key, &mut work, max).unwrap(), Some(&n));
            }
            assert_eq!(table.get(8, &mut work, max).unwrap(), None);
            assert_eq!(table.keys().count(), keys.len());
            works.push(work);
        }
        // Two operations a key inserted, one a key looked up and one for
        // the absent key, two visits each, and growth to 16, 32, ...,
        // 32,768 slots, its sizes summed.
        let n = keys.len();
        let growth: usize = (4..=15).map(|b| 1 << b).sum();
        assert_eq!(works, [4 * n + 2 * n + 2 + growth; 2]);
    }
    #[test]
    fn growth_is_charged_its_size_before_anything_moves() {
        let memory = Budget::new(1 << 20).unwrap();
        let max = usize::MAX;
        let mut table = Table::new(&memory, 5).unwrap();
        let mut work = 0;
        // Nine keys: growth to 16 at the first, to 32 at the ninth.
        for key in 0..9u64 {
            table
                .get_or_insert_with(key * 1000, &mut work, max, |_| Ok(()))
                .unwrap();
        }
        assert_eq!(work, 9 * 4 + 16 + 32);
        assert_eq!(table.slots.len(), 32);
        // At 16 slots and eight keys, the ninth's growth refused one visit
        // short leaves the slots and every entry as they were.
        let mut small = Table::new(&memory, 5).unwrap();
        let mut spent = 0;
        let eight: Vec<u64> = (0..8u64).map(|k| k * 1000).collect();
        for key in &eight {
            small
                .get_or_insert_with(*key, &mut spent, max, |_| Ok(()))
                .unwrap();
        }
        let places = small.places(&eight);
        let mut edge = spent;
        assert_eq!(
            small.get_or_insert_with(9000, &mut edge, spent + 4 + 31, |_| Ok(())),
            Err(Error::Limit("MAX_POOL_WORK"))
        );
        assert_eq!((small.slots.len(), small.len), (16, 8));
        assert_eq!(small.places(&eight), places);
        // One visit more and it grows, charged the 32 slots exactly.
        let mut edge = spent;
        small
            .get_or_insert_with(9000, &mut edge, spent + 4 + 32, |_| Ok(()))
            .unwrap();
        assert_eq!((edge - spent, small.slots.len(), small.len), (36, 32, 9));
    }
    #[test]
    fn the_seed_places_keys_and_probes_are_capped() {
        let memory = Budget::new(1 << 20).unwrap();
        let max = usize::MAX;
        let keys: Vec<u64> = (0..8u64).map(|k| k * 4096).collect();
        let placed = |seed| {
            let mut table = Table::new(&memory, seed).unwrap();
            let mut work = 0;
            for key in &keys {
                table
                    .get_or_insert_with(*key, &mut work, max, |_| Ok(()))
                    .unwrap();
            }
            table.places(&keys)
        };
        assert_ne!(placed(1), placed(2));
        // Keys a seed sends to one home: each insertion probes past all
        // before it, until the cap, eight an operation and 4,096, refuses
        // with the table whole.
        let mut table = Table::new(&memory, 3).unwrap();
        let colliding: Vec<u64> = (0..u64::MAX)
            .filter(|k| table.home(*k) & 255 == 0)
            .take(120)
            .collect();
        let mut work = 0;
        let mut refused = None;
        for (n, key) in colliding.iter().enumerate() {
            let len = table.len;
            match table.get_or_insert_with(*key, &mut work, max, |_| Ok(())) {
                Ok(_) => {}
                Err(error) => {
                    assert_eq!(table.len, len);
                    refused = Some((n, error));
                    break;
                }
            }
        }
        let (n, error) = refused.unwrap();
        assert_eq!(error, Error::Limit("MAX_POOL_WORK"));
        assert!((60..120).contains(&n), "{n}");
        assert_eq!(table.keys().count(), n);
        assert!(table.probes >= table.operations * 8 + 4096);
        // Work charged was the operations', whatever they probed.
        assert_eq!(
            work,
            table.operations * 2 + (4..=8).map(|b| 1 << b).sum::<usize>()
        );
    }
    #[test]
    fn the_probe_cap_is_eight_an_operation_and_4096() {
        let memory = Budget::new(1 << 20).unwrap();
        let mut table = Table::new(&memory, 1).unwrap();
        let (mut work, max) = (0, usize::MAX);
        table
            .get_or_insert_with(1, &mut work, max, |_| Ok(()))
            .unwrap();
        // A lookup of the one key probes one slot: admitted with exactly
        // one left under the cap its operation brings, refused with none.
        let cap = (table.operations + 1) * 8 + 4096;
        table.probes = cap - 1;
        assert_eq!(table.get(1, &mut work, max), Ok(Some(&())));
        assert_eq!(table.probes, cap);
        let cap = (table.operations + 1) * 8 + 4096;
        table.probes = cap;
        assert_eq!(
            table.get(1, &mut work, max),
            Err(Error::Limit("MAX_POOL_WORK"))
        );
        assert_eq!(table.probes, cap);
    }
    #[test]
    fn a_growth_counts_its_move_and_the_next_probe_past_the_cap_refuses() {
        let memory = Budget::new(1 << 20).unwrap();
        let max = usize::MAX;
        let keys = Table::<()>::colliding(7, 9);
        let (ninth, eight) = keys.split_last().unwrap();
        let filled = || {
            let mut table = Table::new(&memory, 7).unwrap();
            let mut work = 0;
            for key in eight {
                table
                    .get_or_insert_with(*key, &mut work, max, |_| Ok(()))
                    .unwrap();
            }
            table
        };
        // The ninth key probes past the eight before it, nine slots, then
        // grows: each moved key probes past those moved before it, 1 to
        // 8, and the ninth probes nine slots again.
        let mut table = filled();
        let (before, mut work) = (table.probes, 0);
        table
            .get_or_insert_with(*ninth, &mut work, max, |_| Ok(()))
            .unwrap();
        assert_eq!(table.probes - before, 9 + 36 + 9);
        // A move that passes the cap, here by 16, completes; the probe
        // after it refuses with every entry kept, as does the next lookup,
        // whose operation raises the cap by 8.
        let mut table = filled();
        table.probes = (table.operations + 2) * 8 + 4096 - 9 - 20;
        assert_eq!(
            table.get_or_insert_with(*ninth, &mut work, max, |_| Ok(())),
            Err(Error::Limit("MAX_POOL_WORK"))
        );
        assert_eq!((table.slots.len(), table.len), (32, 8));
        let mut kept: Vec<u64> = table.keys().collect();
        kept.sort_unstable();
        let mut eight = eight.to_vec();
        eight.sort_unstable();
        assert_eq!(kept, eight);
        assert_eq!(
            table.get(*ninth ^ 1, &mut work, max),
            Err(Error::Limit("MAX_POOL_WORK"))
        );
    }
    #[test]
    fn a_slot_is_no_function_of_the_key_xor_the_seed() {
        let memory = Budget::new(1 << 10).unwrap();
        let (a, b) = (0x1234_5678_9abc_def0u64, 0x0fed_cba9_8765_4321u64);
        let (one, two) = (
            Table::<()>::new(&memory, a).unwrap(),
            Table::<()>::new(&memory, b).unwrap(),
        );
        let same = (0..1000u64)
            .filter(|k| one.home(*k) == two.home(k ^ a ^ b))
            .count();
        assert!(same < 10, "{same}");
    }
    #[test]
    fn addresses_at_a_stride_spread_over_the_slots() {
        let memory = Budget::new(4 << 20).unwrap();
        for stride in [8u64, 16, 64, 4096] {
            let mut table = Table::new(&memory, 11).unwrap();
            let mut work = 0;
            for k in 0..14_000u64 {
                table
                    .get_or_insert_with(
                        0x7f00_0000_0000 + k * stride,
                        &mut work,
                        usize::MAX,
                        |_| Ok(()),
                    )
                    .unwrap();
            }
            assert!(
                table.longest_run() < 64,
                "{stride}: {}",
                table.longest_run()
            );
        }
    }
    #[test]
    fn fingerprints_tell_a_short_tail_from_a_word_ending_in_its_length() {
        for seed in 0..4 {
            let key = |bytes: &[u8]| {
                let mut hash = Fingerprint::new(seed);
                hash.write(bytes);
                hash.finish()
            };
            assert_ne!(key(b"abcdefg"), key(b"abcdefg\x07"));
            assert_ne!(key(b""), key(b"\0"));
        }
    }
    /// 2,048 lists of eleven items, each `name` with a letter, `tail` added
    /// where the index's bit is set, written by `item`.
    fn variants(
        name: &str,
        tail: &str,
        item: impl Fn(String) -> String,
        join: &str,
    ) -> Vec<String> {
        (0..1usize << 11)
            .map(|bits| {
                let items: Vec<String> = (0..11u8)
                    .map(|i| {
                        let more = if bits >> i & 1 == 1 { tail } else { "" };
                        item(format!("{name}{}{more}", char::from(b'a' + i)))
                    })
                    .collect();
                items.join(join)
            })
            .collect()
    }
    #[test]
    fn custom_names_differing_by_a_tail_and_its_length_do_not_share_a_bucket() {
        // Without the length, "--bcdea" and "--bcdea\x07" make one word.
        let maps = |tail| variants("--bcde", tail, |name| format!("{name}: 1"), ";");
        let (work, comparisons) = interned(&maps("\\7 "));
        assert_eq!((work, comparisons), interned(&maps("h")));
        assert!(comparisons <= 2048 + 16, "{comparisons}");
    }
    #[test]
    fn font_families_differing_by_a_tail_and_its_length_do_not_share_a_bucket() {
        let lists = |tail| {
            variants("abcdef", tail, |name| format!("\"{name}\""), ",")
                .into_iter()
                .map(|list| format!("font-family: {list}"))
                .collect::<Vec<_>>()
        };
        let (work, comparisons) = interned(&lists("\\7 "));
        assert_eq!((work, comparisons), interned(&lists("h")));
        assert!(comparisons <= 2048 + 16, "{comparisons}");
    }
    #[test]
    fn custom_maps_and_their_tokens_are_keyed_and_indexed() {
        let [style] = styles(&["--x: red".to_string()]).try_into().unwrap();
        assert_ne!(map_key(1, &style.custom), map_key(2, &style.custom));
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        pool.intern(style).unwrap();
        assert_eq!((pool.maps.len, pool.tokens.len, pool.custom.len), (1, 1, 1));
    }
    #[test]
    fn styles_differing_by_a_sign_and_an_ulp_do_not_collide() {
        // A multiply-rotate fold passed bit 63 through its multiply and
        // cancelled it with the next word's bit 4: these two always shared
        // a key, and k such properties gave 2^k styles one bucket.
        let pair = [
            "width: calc(3px + 50%)".to_string(),
            "width: calc(-3px + 50.000000000000114%)".to_string(),
        ];
        let control = [
            "width: calc(3px + 50%)".to_string(),
            "width: calc(4px + 50%)".to_string(),
        ];
        let [a, b] = styles(&pair).try_into().unwrap();
        let pool = StylePool::new(PoolLimits::default()).unwrap();
        let key = |style: &ComputedStyle| {
            let mut hash = Fingerprint::new(pool.seed);
            for (property, value) in &style.values {
                (*property as u8).hash(&mut hash);
                hash_value(value, &mut hash);
            }
            hash.finish()
        };
        assert_ne!(a, b);
        assert_ne!(key(&a), key(&b));
        // The one comparison each is the shared empty custom map's.
        assert_eq!(interned(&pair), interned(&control));
    }
    #[test]
    fn a_caller_key_is_used_and_none_needs_no_system_call() {
        let keyed = |seed| {
            StylePool::new(PoolLimits {
                seed,
                ..PoolLimits::default()
            })
            .unwrap()
            .seed
        };
        assert_eq!(keyed(Some(7)), 7);
        assert_eq!(keyed(Some(7)), keyed(Some(7)));
        // From addresses: nonzero, and a fingerprint's first word is keyed.
        assert_ne!(keyed(None), 0);
        let first = |seed| {
            let mut hash = Fingerprint::new(seed);
            hash.write_u8(1);
            hash.finish()
        };
        assert_ne!(first(1), first(2));
    }
    #[test]
    fn two_thousand_crafted_styles_intern_without_scanning_each_other() {
        // Eleven properties, each one of the pair above: 2,048 styles a
        // keyless fold put in one bucket, refused MAX_STYLE_COMPARISONS.
        const PROPERTIES: &[&str] = &[
            "width",
            "height",
            "text-indent",
            "margin-top",
            "margin-right",
            "margin-bottom",
            "margin-left",
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ];
        let declarations: Vec<String> = (0..1usize << PROPERTIES.len())
            .map(|bits| {
                PROPERTIES
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let (px, percent) = if bits >> i & 1 == 1 {
                            ("-3px", "50.000000000000114%")
                        } else {
                            ("3px", "50%")
                        };
                        format!("{p}: calc({px} + {percent});")
                    })
                    .collect()
            })
            .collect();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        for style in styles(&declarations) {
            pool.intern(style).unwrap();
        }
        assert_eq!(pool.len(), 2048);
        // One custom-map probe each, and no style scanned another's bucket.
        assert!(pool.comparisons < 2048 + 16, "{}", pool.comparisons);
    }
    #[test]
    fn partial_interning_refusal_keeps_all_published_identities_owned() {
        use td_alloc::{with_reservation_limit, BudgetError};
        let source = crate::StyleSet::new(&[]).unwrap();
        let nodes = crate::test_tree::tree();
        let element = crate::test_tree::Handle {
            nodes: &nodes,
            index: 3,
        };
        let style = source
            .compute(&element, None, 16.0, "--x: red; color: var(--x)")
            .unwrap();
        let memory = Budget::new(1024 * 1024).unwrap();
        let mut completed = false;
        for allowed in 0..128 {
            let mut pool = StylePool::with_budget(PoolLimits::default(), &memory).unwrap();
            let copy = style.try_clone().unwrap();
            let result = with_reservation_limit(allowed, || pool.intern(copy));
            for identity in pool.maps.keys() {
                assert!(pool
                    .custom
                    .values()
                    .flatten()
                    .any(|v| std::ptr::from_ref(&*v.entries).addr() as u64 == identity));
            }
            for identity in pool.tokens.keys() {
                assert!(pool.custom.values().flatten().any(|v| v
                    .entries
                    .values()
                    .filter_map(|e| e.value.as_ref())
                    .any(|t| t.as_ptr() as usize as u64 == identity)));
            }
            for id in pool.buckets.values().flatten() {
                assert!(pool.get(*id).is_some());
            }
            match result {
                Ok(_) => completed = true,
                Err(error) => assert_eq!(error, Error::Memory(BudgetError::Allocation)),
            }
            let id = pool.intern(style.try_clone().unwrap()).unwrap();
            assert_eq!(pool.get(id).unwrap(), &style);
            assert_eq!(pool.intern(style.try_clone().unwrap()).unwrap(), id);
            drop(pool);
            assert_eq!(memory.used(), Budget::metadata_bytes());
            if completed {
                break;
            }
        }
        assert!(completed);
    }

    #[test]
    fn identical_values_share_but_custom_values_distinguish() {
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let a = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        let first = pool.intern(a.try_clone().unwrap()).unwrap();
        assert_eq!(first, pool.intern(a).unwrap());
        let mut b = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        b.values
            .try_insert(crate::Property::Opacity, ComputedValue::Number(0.5))
            .unwrap();
        assert_ne!(first, pool.intern(b).unwrap());
        assert_eq!(pool.len(), 2);
    }
    #[test]
    fn custom_maps_share_independently_of_ordinary_values() {
        let nodes = crate::test_tree::tree();
        let element = crate::test_tree::Handle {
            nodes: &nodes,
            index: 3,
        };
        let sheets = crate::StyleSet::new(&[]).unwrap();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let first = sheets
            .compute(&element, None, 16.0, "--x: red; color: red")
            .unwrap();
        let second = sheets
            .compute(&element, None, 16.0, "--x: red; color: blue")
            .unwrap();
        let different = sheets
            .compute(&element, None, 16.0, "--x: blue; color: red")
            .unwrap();
        let a = pool.intern(first).unwrap();
        let b = pool.intern(second).unwrap();
        let c = pool.intern(different).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(td_alloc::BudgetShared::ptr_eq(
            &pool.get(a).unwrap().custom.entries,
            &pool.get(b).unwrap().custom.entries
        ));
        assert!(!td_alloc::BudgetShared::ptr_eq(
            &pool.get(a).unwrap().custom.entries,
            &pool.get(c).unwrap().custom.entries
        ));
        let again = sheets
            .compute(&element, None, 16.0, "--x: red; color: red")
            .unwrap();
        assert_eq!(a, pool.intern(again).unwrap());
    }
    #[test]
    fn repeated_style_capacity_refusals_retain_no_custom_maps() {
        let nodes = crate::test_tree::tree();
        let element = crate::test_tree::Handle {
            nodes: &nodes,
            index: 3,
        };
        let sheets = crate::StyleSet::new(&[]).unwrap();
        let mut pool = StylePool::new(PoolLimits {
            styles: 0,
            ..PoolLimits::default()
        })
        .unwrap();
        for index in 0..1000 {
            let style = sheets
                .compute(&element, None, 16.0, &format!("--v{index}:initial"))
                .unwrap();
            assert_eq!(pool.intern(style), Err(Error::Limit("MAX_STYLES")));
        }
        assert_eq!(pool.charged_bytes(), 0);
        assert!(pool.custom.is_empty());
        assert!(pool.maps.is_empty());
        assert!(pool.tokens.is_empty());
    }
    #[test]
    fn redundant_local_custom_declaration_reuses_inherited_map() {
        let nodes = crate::test_tree::tree();
        let element = crate::test_tree::Handle {
            nodes: &nodes,
            index: 3,
        };
        let sheets = crate::StyleSet::new(&[]).unwrap();
        let parent = sheets
            .compute(&element, None, 16.0, "--x: red; --unused: blue")
            .unwrap();
        let child = sheets
            .compute(&element, Some(&parent), 16.0, "--x: red")
            .unwrap();
        assert!(td_alloc::BudgetShared::ptr_eq(
            &parent.custom.entries,
            &child.custom.entries
        ));
        let changed = sheets
            .compute(&element, Some(&parent), 16.0, "--x: blue")
            .unwrap();
        assert!(!td_alloc::BudgetShared::ptr_eq(
            &parent.custom.entries,
            &changed.custom.entries
        ));
    }
    #[test]
    fn signed_zero_and_collisions_use_equality() {
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let mut a = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        a.values
            .try_insert(crate::Property::Opacity, ComputedValue::Number(-0.0))
            .unwrap();
        let id = pool.intern(a.try_clone().unwrap()).unwrap();
        a.values
            .try_insert(crate::Property::Opacity, ComputedValue::Number(0.0))
            .unwrap();
        assert_eq!(id, pool.intern(a.try_clone().unwrap()).unwrap());
        let mut other = a.try_clone().unwrap();
        other
            .values
            .try_insert(crate::Property::Opacity, ComputedValue::Number(1.0))
            .unwrap();
        let other_id = pool.intern(other).unwrap();
        // Force an unequal candidate into every bucket, including a's.
        for bucket in pool.buckets.values_mut() {
            bucket.try_push(other_id).unwrap();
            bucket.rotate_right(1);
        }
        assert_eq!(id, pool.intern(a).unwrap());
    }
    #[test]
    fn work_and_comparison_limits_include_exact_edge() {
        let mut measure = StylePool::new(PoolLimits::default()).unwrap();
        let a = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        measure.intern(a.try_clone().unwrap()).unwrap();
        measure.intern(a.try_clone().unwrap()).unwrap();
        let work = measure.work();
        for (limit, ok) in [(work, true), (work - 1, false)] {
            let mut pool = StylePool::new(PoolLimits {
                work: limit,
                ..PoolLimits::default()
            })
            .unwrap();
            pool.intern(a.try_clone().unwrap()).unwrap();
            assert_eq!(pool.intern(a.try_clone().unwrap()).is_ok(), ok);
        }
        let mut pool = StylePool::new(PoolLimits {
            comparisons: 1,
            ..PoolLimits::default()
        })
        .unwrap();
        pool.intern(a.try_clone().unwrap()).unwrap();
        pool.intern(a.try_clone().unwrap()).unwrap();
        assert_eq!(pool.intern(a), Err(Error::Limit("MAX_STYLE_COMPARISONS")));
    }
    #[test]
    fn budgets_admit_exact_charge_and_refuse_one_less() {
        let mut measure = StylePool::new(PoolLimits::default()).unwrap();
        measure
            .intern(ComputedStyle::new(&crate::memory().unwrap()).unwrap())
            .unwrap();
        let charge = measure.charged_bytes();
        for (bytes, accepted) in [(charge, true), (charge - 1, false)] {
            let mut pool = StylePool::new(PoolLimits {
                bytes,
                ..PoolLimits::default()
            })
            .unwrap();
            assert_eq!(
                pool.intern(ComputedStyle::new(&crate::memory().unwrap()).unwrap())
                    .is_ok(),
                accepted
            );
        }
        let mut pool = StylePool::new(PoolLimits {
            styles: 0,
            ..PoolLimits::default()
        })
        .unwrap();
        assert_eq!(
            pool.intern(ComputedStyle::new(&crate::memory().unwrap()).unwrap()),
            Err(Error::Limit("MAX_STYLES"))
        );
        let mut pool = StylePool::new(PoolLimits {
            comparisons: 0,
            ..PoolLimits::default()
        })
        .unwrap();
        let a = ComputedStyle::new(&crate::memory().unwrap()).unwrap();
        pool.intern(a.try_clone().unwrap()).unwrap();
        assert_eq!(pool.intern(a), Err(Error::Limit("MAX_STYLE_COMPARISONS")));
    }
}
