//! Document-local interning, including independently shared custom maps.
use crate::syntax::{Component, Token};
use crate::values::{
    Breadth, ComputedStyle, ComputedValue, Family, GridArea, GridAreas, GridLine, Length, Rare,
    Repeat, TrackItem, TrackList, TrackSize,
};
use crate::variables::CustomProperties;
use crate::Error;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetVec};

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
    /// Canonical rare groups by bitwise fingerprint, each with the one
    /// a style's folds.
    rare: Table<BudgetVec<(BudgetShared<Rare>, u64)>>,
    /// The last group to arrive already canonical, or to become so, and
    /// its style fingerprint: a style set's initial group, which every
    /// style that sets no rare longhand shares, is hashed once a run of them.
    known: Option<(BudgetShared<Rare>, u64)>,
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
            rare: Table::new(memory, seed)?,
            known: None,
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
        let rare = self.share_rare(&mut style.rare)?;
        let mut hash = Fingerprint::new(self.seed);
        // Canonical maps have a stable address while retained by this pool.
        (std::ptr::from_ref(&*style.custom.entries).addr()).hash(&mut hash);
        hash.add(rare);
        let value_work = style.values.values().map(value_work).sum::<usize>();
        self.visit(value_work)?;
        for (property, value) in &style.values {
            (*property as u8).hash(&mut hash);
            hash_value(value, Zero::Equal, &mut hash);
        }
        let key = hash.finish();
        if let Some(bucket) = self.buckets.get(key, &mut self.work, self.limits.work)? {
            for id in bucket {
                comparison(&mut self.comparisons, self.limits.comparisons)?;
                visit_count(&mut self.work, value_work, self.limits.work)?;
                let Some(old) = self.styles.get(id.0) else {
                    continue;
                };
                if !std::ptr::eq(&*old.custom.entries, &*style.custom.entries)
                    || old.values != style.values
                {
                    continue;
                }
                // Canonical groups differ only by a signed zero (`same` is
                // bitwise, `==` is not): the styles are equal, as when a
                // style held every longhand.
                if BudgetShared::ptr_eq(&old.rare, &style.rare) {
                    return Ok(*id);
                }
                // `==` reads the slots either group marks.
                let group_work = 1 + style
                    .rare
                    .slots(old.rare.set | style.rare.set)
                    .map(|(_, value)| self::value_work(value))
                    .sum::<usize>();
                visit_count(&mut self.work, group_work, self.limits.work)?;
                if *old.rare == *style.rare {
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
    /// Replace `group` with this pool's canonical equal, pooling it if
    /// new, and return its fingerprint for a style's, which hashes signed
    /// zeros alike as styles' `==` does. No rare longhand inherits, so the
    /// one group that reaches the pool already canonical is a style set's
    /// initial one: the group held in `known`, the same allocation, is
    /// taken at once; any other is hashed by contents and compared.
    fn share_rare(&mut self, group: &mut BudgetShared<Rare>) -> Result<u64, Error> {
        if let Some((known, loose)) = &self.known {
            if BudgetShared::ptr_eq(known, group) {
                return Ok(*loose);
            }
        }
        let group_work = group_work(group);
        self.visit(group_work)?;
        // Groups are told apart to the bit, so their bucket key is too:
        // keyed by the loose fingerprint, a group's 2^11 sign variants
        // would share one bucket a page could fill without the key. Both
        // visit the marked slots alone, the others being initial; the
        // loose one passes over a slot `==` its initial value, a signed
        // zero's other sign, as an unmarked slot.
        let mut exact = Fingerprint::new(self.seed);
        let mut loose = Fingerprint::new(self.seed);
        exact.add(group.set);
        for (slot, value) in group.slots(group.set) {
            exact.add(slot as u64);
            hash_value(value, Zero::Bits, &mut exact);
            let initial = crate::Property::rare_roster()
                .get(slot)
                .and_then(|p| p.seed());
            if initial.as_ref() != Some(value) {
                loose.add(slot as u64);
                hash_value(value, Zero::Equal, &mut loose);
            }
        }
        let (key, loose) = (exact.finish(), loose.finish());
        if let Some(bucket) = self.rare.get(key, &mut self.work, self.limits.work)? {
            for (old, old_loose) in bucket {
                comparison(&mut self.comparisons, self.limits.comparisons)?;
                visit_count(&mut self.work, group_work, self.limits.work)?;
                if old.same(group) {
                    if BudgetShared::ptr_eq(old, group) {
                        self.known = Some((old.try_clone()?, *old_loose));
                    }
                    *group = old.try_clone()?;
                    return Ok(*old_loose);
                }
            }
        }
        // A novel group cannot equal any retained complete style.
        if self.styles.len() >= self.limits.styles {
            return Err(Error::Limit("MAX_STYLES"));
        }
        // Its index entry, as a style's, its doubled storage, and its
        // grid values' own.
        let owned = group.values.iter().map(owned_charge).sum::<usize>();
        self.charge(128 + 2 * size_of::<Rare>() + owned)?;
        self.rare
            .get_or_insert_with(key, &mut self.work, self.limits.work, BudgetVec::new)?
            .try_push((group.try_clone()?, loose))?;
        self.known = Some((group.try_clone()?, loose));
        Ok(loose)
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
/// The slots one operation may read. No run of occupied slots reaches it,
/// so a probe for any key, present or absent, reads at most this many,
/// however often it is made.
const PROBE_LIMIT: usize = 64;
/// How far past its half-full size a table may grow to keep its runs
/// short: at most this many times the slots its entries would need.
const SPREAD: usize = 4;
/// An open-addressed table keyed by u64, at most half full, its slots
/// chosen by the key mixed twice with the pool's seed. A lookup or an
/// insertion is charged OPERATION visits before it probes. No run of
/// occupied slots reaches PROBE_LIMIT: an insertion that would make one
/// grows the table first, as does a growth whose move made one, up to
/// SPREAD times its half-full size, and past that the insertion is
/// refused `Limit("MAX_POOL_WORK")` with the table whole. A probe is
/// refused before it would read a slot past PROBE_LIMIT, which only a
/// table at that size can need. A growth doubles the table, charged its
/// new size before anything is allocated or moved; its move, which
/// cannot stop halfway, is counted in `probes`. Iteration order is the
/// slots', on which no result depends.
struct Table<V> {
    slots: BudgetVec<Option<(u64, V)>>,
    len: usize,
    seed: u64,
    /// Slots read, by operations and moves alike.
    probes: usize,
}
impl<V> Table<V> {
    fn new(memory: &Budget, seed: u64) -> Result<Self, Error> {
        Ok(Self {
            slots: BudgetVec::new(memory)?,
            len: 0,
            seed,
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
    /// The slot holding `key`, else the empty one it would take; refused
    /// before a read past PROBE_LIMIT.
    fn probe(&mut self, key: u64) -> Result<Result<usize, usize>, Error> {
        if self.slots.is_empty() {
            return Ok(Err(0));
        }
        let mask = self.slots.len() - 1;
        let mut at = self.home(key) & mask;
        for _ in 0..PROBE_LIMIT {
            self.probes = self.probes.saturating_add(1);
            match self.slots.get(at) {
                Some(Some((stored, _))) if *stored == key => return Ok(Ok(at)),
                Some(Some(_)) => at = (at + 1) & mask,
                Some(None) => return Ok(Err(at)),
                None => return Err(Error::InvalidContext),
            }
        }
        Err(Error::Limit("MAX_POOL_WORK"))
    }
    fn get(&mut self, key: u64, work: &mut usize, max: usize) -> Result<Option<&V>, Error> {
        visit_count(work, OPERATION, max)?;
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
        visit_count(work, OPERATION, max)?;
        let at = match self.probe(key)? {
            Ok(at) => at,
            Err(mut at) => {
                visit_count(work, OPERATION, max)?;
                if self.len.saturating_add(1).saturating_mul(2) > self.slots.len() {
                    self.grow(work, max)?;
                    at = self.vacancy(key)?;
                }
                // The run the entry would join, or bridge, stays short.
                while self.run_through(at) >= PROBE_LIMIT {
                    self.grow(work, max)?;
                    at = self.vacancy(key)?;
                }
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
    /// The empty slot an absent `key` would take.
    fn vacancy(&mut self, key: u64) -> Result<usize, Error> {
        match self.probe(key)? {
            Err(at) => Ok(at),
            Ok(_) => Err(Error::InvalidContext),
        }
    }
    /// The occupied slots on either side of the empty slot `at`, and it:
    /// the run an entry there would be part of, counted to PROBE_LIMIT.
    fn run_through(&mut self, at: usize) -> usize {
        let mask = self.slots.len().wrapping_sub(1);
        let occupied = |slots: &BudgetVec<Option<(u64, V)>>, i: usize| {
            slots.get(i & mask).is_some_and(Option::is_some)
        };
        let mut run = 1;
        while run < PROBE_LIMIT && occupied(&self.slots, at.wrapping_add(run)) {
            run += 1;
        }
        let mut back = 1;
        while run < PROBE_LIMIT && occupied(&self.slots, at.wrapping_sub(back)) {
            run += 1;
            back += 1;
        }
        self.probes = self.probes.saturating_add(run);
        run
    }
    /// The longest run of occupied slots, wrapping, counted to PROBE_LIMIT.
    fn longest_run(&self) -> usize {
        let (mut run, mut longest) = (0usize, 0usize);
        for slot in self.slots.iter().chain(self.slots.iter().take(PROBE_LIMIT)) {
            run = if slot.is_some() { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        longest.min(self.slots.len())
    }
    /// Double the table, then again while its move left a run at
    /// PROBE_LIMIT, up to SPREAD times the half-full size of one entry
    /// more; each size charged before it is allocated or moved into.
    fn grow(&mut self, work: &mut usize, max: usize) -> Result<(), Error> {
        let needed = self
            .len
            .saturating_add(1)
            .saturating_mul(2)
            .checked_next_power_of_two()
            .unwrap_or(usize::MAX)
            .max(16);
        loop {
            let capacity = self.slots.len().saturating_mul(2).max(16);
            if capacity > needed.saturating_mul(SPREAD) {
                return Err(Error::Limit("MAX_POOL_WORK"));
            }
            visit_count(work, capacity, max)?;
            // td-alloc's constructor, written in full as the allocation
            // guard admits it.
            let mut fresh = td_alloc::BudgetVec::with_capacity(self.slots.budget(), capacity)?;
            for _ in 0..capacity {
                fresh
                    .push_within_capacity(None)
                    .map_err(|_| Error::InvalidContext)?;
            }
            let mask = capacity - 1;
            let old = std::mem::replace(&mut self.slots, fresh);
            // Counted, not refused: a move stopped halfway would lose
            // entries.
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
            if self.longest_run() < PROBE_LIMIT {
                return Ok(());
            }
        }
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
#[inline(always)]
fn value_work(value: &ComputedValue) -> usize {
    1 + match value {
        ComputedValue::FontFamily(_)
        | ComputedValue::GridTracks(_)
        | ComputedValue::GridAreas(_)
        | ComputedValue::GridLine(_) => owned_work(value),
        _ => 0,
    }
}
/// A group's visits: one, and its marked slots', which alone its
/// fingerprints and comparisons read.
fn group_work(group: &Rare) -> usize {
    1 + group
        .slots(group.set)
        .map(|(_, value)| value_work(value))
        .sum::<usize>()
}
/// A family list's name bytes, or a grid value's items and name bytes:
/// out of line, as few values own storage.
#[inline(never)]
fn owned_work(value: &ComputedValue) -> usize {
    match value {
        ComputedValue::FontFamily(families) => families
            .iter()
            .map(|family| match family {
                Family::Generic(s) => s.len(),
                Family::Named(s) => s.len(),
            })
            .sum(),
        ComputedValue::GridTracks(list) => {
            list.items().len() + list.names().iter().map(|n| n.len() + 1).sum::<usize>()
        }
        ComputedValue::GridAreas(areas) => areas.areas().iter().map(|a| a.name.len() + 1).sum(),
        ComputedValue::GridLine(line) => line.name.as_ref().map_or(0, |n| n.len()),
        _ => 0,
    }
}
/// What a rare value's own storage is charged: doubled, as a style's
/// font families, each name as a family's.
fn owned_charge(value: &ComputedValue) -> usize {
    let name = |text: &crate::syntax::Text| 2 * text.heap_capacity() + 32;
    match value {
        ComputedValue::GridTracks(list) => {
            2 * size_of::<TrackList>()
                + 2 * list.items.capacity() * size_of::<TrackItem>()
                + 2 * list.names.capacity() * size_of::<crate::syntax::Text>()
                + list.names().iter().map(name).sum::<usize>()
        }
        ComputedValue::GridAreas(areas) => {
            2 * size_of::<GridAreas>()
                + 2 * areas.areas.capacity() * size_of::<GridArea>()
                + areas.areas().iter().map(|a| name(&a.name)).sum::<usize>()
        }
        ComputedValue::GridLine(line) => {
            2 * size_of::<GridLine>() + line.name.as_ref().map_or(0, name)
        }
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
/// the key is unknown to a page, so values a table's equality tells apart
/// share a bucket only by chance: each table hashes zeros as its equality
/// compares them (styles' `==` alike, rare groups' `same` by sign). A
/// collided bucket is still bounded by MAX_STYLE_COMPARISONS and
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
/// How a fingerprint hashes a signed zero: as the equality it keys.
#[derive(Clone, Copy)]
enum Zero {
    /// PartialEq's: -0 and 0 alike.
    Equal,
    /// `same`'s: to the bit.
    Bits,
}
fn hash_number(value: f64, zero: Zero, hash: &mut impl Hasher) {
    let alike = matches!(zero, Zero::Equal) && value == 0.0;
    (if alike { 0 } else { value.to_bits() }).hash(hash);
}
#[inline(always)]
fn hash_length(length: &Length, zero: Zero, hash: &mut impl Hasher) {
    std::mem::discriminant(length).hash(hash);
    match length {
        Length::Px(n) | Length::Percent(n) => hash_number(*n, zero, hash),
        Length::Calc {
            px,
            percent,
            non_negative,
        } => {
            hash_number(*px, zero, hash);
            hash_number(*percent, zero, hash);
            non_negative.hash(hash);
        }
        Length::Auto => {}
    }
}
fn hash_breadth(breadth: &Breadth, zero: Zero, hash: &mut impl Hasher) {
    std::mem::discriminant(breadth).hash(hash);
    match breadth {
        Breadth::Length(length) => hash_length(length, zero, hash),
        Breadth::Flex(n) => hash_number(*n, zero, hash),
        Breadth::Auto | Breadth::MinContent | Breadth::MaxContent => {}
    }
}
/// A grid value's contents: out of line, as no common value is one.
#[inline(never)]
fn hash_grid(value: &ComputedValue, zero: Zero, hash: &mut impl Hasher) {
    match value {
        ComputedValue::GridTracks(list) => hash_tracks(list, zero, hash),
        ComputedValue::GridAreas(areas) => {
            (areas.rows(), areas.columns(), areas.areas().len()).hash(hash);
            for area in areas.areas() {
                area.name.hash(hash);
                (area.row_start, area.row_end).hash(hash);
                (area.column_start, area.column_end).hash(hash);
            }
        }
        ComputedValue::GridLine(line) => {
            (line.span, line.integer).hash(hash);
            line.name.hash(hash);
        }
        _ => {}
    }
}
fn hash_tracks(list: &TrackList, zero: Zero, hash: &mut impl Hasher) {
    list.items().len().hash(hash);
    for item in list.items() {
        std::mem::discriminant(item).hash(hash);
        match item {
            TrackItem::Names { start, len } => (start, len).hash(hash),
            TrackItem::Repeat { repeat, len } => {
                std::mem::discriminant(repeat).hash(hash);
                if let Repeat::Count(n) = repeat {
                    n.hash(hash);
                }
                len.hash(hash);
            }
            TrackItem::Size(size) => {
                std::mem::discriminant(size).hash(hash);
                match size {
                    TrackSize::Breadth(b) => hash_breadth(b, zero, hash),
                    TrackSize::MinMax(a, b) => {
                        hash_breadth(a, zero, hash);
                        hash_breadth(b, zero, hash);
                    }
                    TrackSize::FitContent(length) => hash_length(length, zero, hash),
                }
            }
        }
    }
    list.names().len().hash(hash);
    for name in list.names() {
        name.hash(hash);
    }
}
fn hash_value(value: &ComputedValue, zero: Zero, hash: &mut impl Hasher) {
    std::mem::discriminant(value).hash(hash);
    match value {
        ComputedValue::Number(n) => hash_number(*n, zero, hash),
        ComputedValue::Keyword(s) => s.hash(hash),
        ComputedValue::Color(c) => (c.red, c.green, c.blue, c.alpha).hash(hash),
        ComputedValue::Length(length) => hash_length(length, zero, hash),
        ComputedValue::GridTracks(_) | ComputedValue::GridAreas(_) | ComputedValue::GridLine(_) => {
            hash_grid(value, zero, hash)
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
            hash_number(*horizontal, zero, hash);
            hash_number(*vertical, zero, hash);
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
            key(&|h| hash_number(0.0, Zero::Equal, h)),
            key(&|h| hash_number(1.0, Zero::Equal, h)),
        ];
        for (i, a) in keys.iter().enumerate() {
            for b in keys.iter().skip(i + 1) {
                assert_ne!(a, b);
            }
        }
        // Equal values, equal keys: -0 and 0 compare equal.
        assert_eq!(
            key(&|h| hash_number(-0.0, Zero::Equal, h)),
            key(&|h| hash_number(0.0, Zero::Equal, h))
        );
        // A rare group's are its bits.
        assert_ne!(
            key(&|h| hash_number(-0.0, Zero::Bits, h)),
            key(&|h| hash_number(0.0, Zero::Bits, h))
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
        /// Keys `seed` sends to slot 0 of every table up to 65,536 slots.
        fn colliding(seed: u64, count: usize) -> Vec<u64> {
            let memory = Budget::new(1 << 10).unwrap();
            let table = Table::<()>::new(&memory, seed).unwrap();
            (0..u64::MAX)
                .filter(|k| table.home(*k) & 0xffff == 0)
                .take(count)
                .collect()
        }
        /// The slots a probe for `key` reads: its run from home, and the
        /// slot that ends it.
        fn reach(&self, key: u64) -> usize {
            let mask = self.slots.len() - 1;
            let mut at = self.home(key) & mask;
            let mut read = 1;
            while self.slots.get(at).is_some_and(Option::is_some) {
                at = (at + 1) & mask;
                read += 1;
            }
            read
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
    fn one_absent_key_looked_up_again_and_again_stays_within_its_allowance() {
        // A freed group's address, reused by the next element's, is one
        // absent key looked up once an element: its run is fixed, so an
        // allowance averaged over operations ran out on every long one.
        let memory = Budget::new(1 << 20).unwrap();
        let max = usize::MAX;
        let mut table = Table::new(&memory, 0x5eed).unwrap();
        let mut work = 0;
        for k in 0..127u64 {
            table
                .get_or_insert_with(0x7f00_0000_0000 + k * 64, &mut work, max, |_| Ok(()))
                .unwrap();
        }
        assert_eq!((table.len, table.slots.len()), (127, 256));
        let absent = (0..100_000u64).max_by_key(|k| table.reach(*k)).unwrap();
        assert!(table.reach(absent) >= 12, "{}", table.reach(absent));
        for _ in 0..5000 {
            assert_eq!(table.get(absent, &mut work, max), Ok(None));
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
    fn the_seed_places_keys_and_no_run_reaches_the_probe_limit() {
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
        // Keys the seed sends to one home in every table up to 65,536
        // slots: each joins one run, which growth cannot part. The 64th
        // would make it PROBE_LIMIT long; the table grows twice past its
        // half-full 128 slots, to SPREAD times them, and then refuses it
        // with the table whole.
        let colliding = Table::<()>::colliding(3, 80);
        let mut table = Table::new(&memory, 3).unwrap();
        let mut work = 0;
        let mut refused = None;
        for (n, key) in colliding.iter().enumerate() {
            let len = table.len;
            if let Err(error) = table.get_or_insert_with(*key, &mut work, max, |_| Ok(())) {
                assert_eq!(table.len, len);
                refused = Some((n, error));
                break;
            }
        }
        assert_eq!(refused, Some((63, Error::Limit("MAX_POOL_WORK"))));
        assert_eq!((table.len, table.slots.len()), (63, 512));
        assert_eq!(table.longest_run(), 63);
        // Each operation, two visits, and each size grown to, charged.
        let operations = 63 * 2 + 2;
        let growth: usize = (4..=9).map(|b| 1 << b).sum();
        assert_eq!(work, operations * 2 + growth);
        // Every key is found, and a lookup reads no more than the limit:
        // an absent key of that home reads the run and the slot after it.
        for key in colliding.iter().take(63) {
            let before = table.probes;
            assert_eq!(table.get(*key, &mut work, max), Ok(Some(&())));
            assert!(table.probes - before <= PROBE_LIMIT);
        }
        let absent = *colliding.get(70).unwrap();
        let before = table.probes;
        assert_eq!(table.get(absent, &mut work, max), Ok(None));
        assert_eq!(table.probes - before, PROBE_LIMIT);
    }
    #[test]
    fn a_probe_is_refused_before_it_reads_past_the_limit() {
        let memory = Budget::new(1 << 20).unwrap();
        let (mut work, max) = (0, usize::MAX);
        let mut table = Table::new(&memory, 1).unwrap();
        for key in 0..200u64 {
            table
                .get_or_insert_with(key, &mut work, max, |_| Ok(()))
                .unwrap();
        }
        for slot in table.slots.iter_mut() {
            *slot = None;
        }
        // A run no insertion can make: PROBE_LIMIT slots from a key's home.
        let mask = table.slots.len() - 1;
        let home = table.home(2) & mask;
        let filler = |i: usize| u64::MAX - i as u64;
        for i in 0..PROBE_LIMIT - 1 {
            *table.slots.get_mut((home + i) & mask).unwrap() = Some((filler(i), ()));
        }
        let before = table.probes;
        assert_eq!(table.get(2, &mut work, max), Ok(None));
        assert_eq!(table.probes - before, PROBE_LIMIT);
        *table
            .slots
            .get_mut((home + PROBE_LIMIT - 1) & mask)
            .unwrap() = Some((filler(PROBE_LIMIT), ()));
        let before = table.probes;
        assert_eq!(
            table.get(2, &mut work, max),
            Err(Error::Limit("MAX_POOL_WORK"))
        );
        assert_eq!(table.probes - before, PROBE_LIMIT);
    }
    #[test]
    fn a_move_that_leaves_a_long_run_grows_again_up_to_the_spread() {
        let memory = Budget::new(1 << 20).unwrap();
        let mut table = Table::new(&memory, 9).unwrap();
        let mut work = 0;
        for key in 0..100u64 {
            table
                .get_or_insert_with(key, &mut work, usize::MAX, |_| Ok(()))
                .unwrap();
        }
        // Sixty-four keys of one home in every size, set in place as no
        // insertion could: each move keeps their run whole.
        for slot in table.slots.iter_mut() {
            *slot = None;
        }
        for (at, key) in Table::<()>::colliding(9, 64).into_iter().enumerate() {
            *table.slots.get_mut(at).unwrap() = Some((key, ()));
        }
        table.len = 64;
        assert_eq!(table.slots.len(), 256);
        let before = work;
        assert_eq!(
            table.grow(&mut work, usize::MAX),
            Err(Error::Limit("MAX_POOL_WORK"))
        );
        // To 512, and again to SPREAD times the 256 its entries need.
        assert_eq!((table.slots.len(), table.len), (1024, 64));
        assert_eq!(work - before, 512 + 1024);
        assert_eq!(table.longest_run(), 64);
    }
    #[test]
    fn a_growth_counts_its_move_and_keeps_every_entry() {
        let memory = Budget::new(1 << 20).unwrap();
        let max = usize::MAX;
        let keys = Table::<()>::colliding(7, 9);
        let (ninth, eight) = keys.split_last().unwrap();
        let mut table = Table::new(&memory, 7).unwrap();
        let mut work = 0;
        for key in eight {
            table
                .get_or_insert_with(*key, &mut work, max, |_| Ok(()))
                .unwrap();
        }
        // The ninth key probes past the eight before it, nine slots, then
        // grows: each moved key probes past those moved before it, 1 to
        // 8; the ninth probes nine slots again, and its run is nine.
        let before = table.probes;
        table
            .get_or_insert_with(*ninth, &mut work, max, |_| Ok(()))
            .unwrap();
        assert_eq!(table.probes - before, 9 + 36 + 9 + 9);
        assert_eq!((table.slots.len(), table.len), (32, 9));
        let mut kept: Vec<u64> = table.keys().collect();
        kept.sort_unstable();
        let mut all = keys.clone();
        all.sort_unstable();
        assert_eq!(kept, all);
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
                hash_value(value, Zero::Equal, &mut hash);
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
            .compute(&element, None, 16.0, "--x: red; color: var(--x); order: 2")
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
            if let Some((known, _)) = &pool.known {
                assert!(pool
                    .rare
                    .values()
                    .flatten()
                    .any(|(g, _)| BudgetShared::ptr_eq(g, known)));
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
        b.set(crate::Property::Opacity, ComputedValue::Number(0.5))
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
        a.set(crate::Property::LineHeight, ComputedValue::Number(-0.0))
            .unwrap();
        let id = pool.intern(a.try_clone().unwrap()).unwrap();
        a.set(crate::Property::LineHeight, ComputedValue::Number(0.0))
            .unwrap();
        assert_eq!(id, pool.intern(a.try_clone().unwrap()).unwrap());
        let mut other = a.try_clone().unwrap();
        other
            .set(crate::Property::LineHeight, ComputedValue::Number(1.0))
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
    #[test]
    fn equal_rare_groups_built_apart_fingerprint_alike_and_pool_as_one() {
        let [a, b, c, d] = styles(&[
            "order:4".to_string(),
            "order:4".to_string(),
            "order:4;width:1px".to_string(),
            "order:5".to_string(),
        ])
        .try_into()
        .unwrap();
        // Separate copies, equal by contents: the styles compare equal.
        assert!(!BudgetShared::ptr_eq(&a.rare, &b.rare));
        assert_eq!(a, b);
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let (mut ga, mut gb, mut gd) = (
            a.rare.try_clone().unwrap(),
            b.rare.try_clone().unwrap(),
            d.rare.try_clone().unwrap(),
        );
        let key = pool.share_rare(&mut ga).unwrap();
        assert_eq!(pool.share_rare(&mut gb).unwrap(), key);
        assert!(BudgetShared::ptr_eq(&ga, &gb));
        assert_ne!(pool.share_rare(&mut gd).unwrap(), key);
        // A second pool, keyed apart, keys the group apart too.
        let mut other = StylePool::new(PoolLimits {
            seed: Some(pool.seed ^ 1),
            ..PoolLimits::default()
        })
        .unwrap();
        assert_ne!(other.share_rare(&mut a.rare.try_clone().unwrap()), Ok(key));
        let first = pool.intern(a).unwrap();
        assert_eq!(pool.intern(b).unwrap(), first);
        let third = pool.intern(c).unwrap();
        assert_ne!(third, first);
        let canonical = &pool.get(first).unwrap().rare;
        assert!(BudgetShared::ptr_eq(
            canonical,
            &pool.get(third).unwrap().rare
        ));
        assert!(BudgetShared::ptr_eq(canonical, &ga));
        assert_eq!(pool.rare.len, 2);
    }
    #[test]
    fn a_rare_group_is_charged_once_and_the_initial_one_hashed_once() {
        let declarations: Vec<String> = [
            "width:1px;order:4",
            "width:2px;order:4",
            "width:3px;order:5",
            "width:4px",
            "width:5px",
        ]
        .map(String::from)
        .to_vec();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let mut charges = Vec::new();
        let mut works = Vec::new();
        for style in styles(&declarations) {
            let (bytes, work) = (pool.charged_bytes(), pool.work());
            pool.intern(style).unwrap();
            charges.push(pool.charged_bytes() - bytes);
            works.push(pool.work() - work);
        }
        let group = 128 + 2 * size_of::<Rare>();
        // 1,040 with 19 longhands: 48 bytes a rare longhand, a value staying
        // 24, and 16 for the doubled mask of marked slots.
        assert_eq!(group, 1_632);
        assert_eq!(size_of::<ComputedValue>(), 24);
        // The second shares the first's group: a style's charge alone.
        let [_, style, two, three, four]: [usize; 5] = charges.try_into().unwrap();
        let [_, shared, _, new, found]: [usize; 5] = works.try_into().unwrap();
        assert_eq!(two, style + group);
        // The initial group, first seen with the fourth, then known.
        assert_eq!(three, style + group);
        assert_eq!(four, style);
        // The 71-longhand style was charged 13,056: 19 values fewer, a handle more.
        assert_eq!(
            style,
            13_056 - 19 * 176 + 2 * size_of::<BudgetShared<Rare>>()
        );
        assert_eq!(style, 9_728);
        // The known group costs nothing; another is hashed (a visit and
        // one a marked slot, not one each of the 31 values) and looked
        // up, then inserted (two operations) or compared once with its
        // equal (as many as hashing). The initial group marks none; the
        // second's marks `order`.
        assert_eq!(new - found, 1 + 2 + 4);
        assert_eq!(crate::values::RARE, 31);
        assert_eq!(shared - found, 2 + 2 + 2);
        assert_eq!(pool.rare.len, 3);
    }
    #[test]
    fn interning_hashes_a_style_sets_initial_group_once() {
        let declarations: Vec<String> = (0..100).map(|i| format!("width:{i}px")).collect();
        let source = styles(&declarations);
        let default = source.first().unwrap().rare.try_clone().unwrap();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        for style in source {
            assert!(BudgetShared::ptr_eq(&style.rare, &default));
            pool.intern(style).unwrap();
        }
        assert_eq!((pool.len(), pool.rare.len), (100, 1));
        assert!(BudgetShared::ptr_eq(&pool.known.unwrap().0, &default));
    }
    #[test]
    fn styles_differing_only_in_a_rare_value_do_not_share_a_bucket() {
        // Their common values and custom maps are equal: the group's
        // fingerprint alone keeps them apart.
        let declarations: Vec<String> = (0..300).map(|i| format!("order:{i}")).collect();
        let (_, comparisons) = interned(&declarations);
        assert!(comparisons < 300 + 16, "{comparisons}");
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        for style in styles(&declarations) {
            pool.intern(style).unwrap();
        }
        assert_eq!((pool.len(), pool.rare.len), (300, 300));
    }
    #[test]
    fn a_pooled_group_never_gives_a_style_another_ones_signed_zero() {
        let [a, b] = styles(&["top:-0px".to_string(), "top:0px;width:1px".to_string()])
            .try_into()
            .unwrap();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let first = pool.intern(a).unwrap();
        let second = pool.intern(b).unwrap();
        let top = |id| {
            match pool.get(id).unwrap().get(crate::Property::Top) {
                Some(ComputedValue::Length(Length::Px(px))) => Some(*px),
                _ => None,
            }
            .unwrap()
        };
        assert!(top(first).is_sign_negative());
        assert!(top(second).is_sign_positive());
        // Buckets of their own, each one group.
        assert_eq!(pool.rare.len, 2);
        assert_eq!(pool.rare.values().flatten().count(), 2);
        // Styles that differ by that sign alone are one style, the first,
        // as when a style held every longhand.
        let [c] = styles(&["top:0px".to_string()]).try_into().unwrap();
        assert_eq!(pool.intern(c).unwrap(), first);
    }
    #[test]
    fn a_groups_sign_variants_do_not_share_a_bucket() {
        // Eleven rare longhands may be a zero of either sign: 2,048
        // groups `==` one another, all apart to the bit. Hashed as `==`
        // compares, they shared a bucket a page builds without the key.
        const ZEROS: &[&str] = &[
            "opacity:",
            "top:",
            "right:",
            "bottom:",
            "left:",
            "flex-grow:",
            "flex-shrink:",
            "flex-basis:",
            "row-gap:",
            "column-gap:",
            "order:",
        ];
        let variant = |signs: usize| {
            let mut declaration = String::new();
            for (bit, name) in ZEROS.iter().enumerate() {
                declaration.push_str(name);
                declaration.push_str(if signs >> bit & 1 == 1 { "-0" } else { "0" });
                if !matches!(*name, "opacity:" | "flex-grow:" | "flex-shrink:" | "order:") {
                    declaration.push_str("px");
                }
                declaration.push(';');
            }
            declaration
        };
        for (elements, variants) in [(1_500, 2_048), (10_000, 256)] {
            let declarations: Vec<String> = (0..elements).map(|i| variant(i % variants)).collect();
            let mut pool = StylePool::new(PoolLimits::default()).unwrap();
            for style in styles(&declarations) {
                pool.intern(style).unwrap();
            }
            assert_eq!(pool.len(), 1);
            assert_eq!(pool.rare.len, elements.min(variants));
            // At most a custom map's, a group's and a style's, each element.
            assert!(pool.comparisons <= 3 * elements, "{}", pool.comparisons);
        }
    }
    #[test]
    #[allow(clippy::panic, clippy::indexing_slicing)]
    fn equal_grid_values_built_apart_share_a_style_and_unequal_ones_do_not() {
        let declarations: Vec<String> = [
            "grid-template-columns:[a] 1px repeat(2, 1fr);grid-area:x;grid-template-areas:'x y'",
            "grid-template-columns:[a] 1px repeat(2, 1fr);grid-area:x;grid-template-areas:'x y'",
            "grid-template-columns:[a] 1px repeat(2, 2fr);grid-area:x;grid-template-areas:'x y'",
            "grid-template-columns:[b] 1px repeat(2, 1fr);grid-area:x;grid-template-areas:'x y'",
            "grid-template-columns:[a] 1px repeat(3, 1fr);grid-area:x;grid-template-areas:'x y'",
            "grid-template-columns:[a] 1px repeat(2, 1fr);grid-area:z;grid-template-areas:'x y'",
            "grid-template-columns:[a] 1px repeat(2, 1fr);grid-area:x;grid-template-areas:'x x'",
            "grid-template-columns:[a] 1px repeat(2, 1fr);grid-area:x;grid-template-areas:'x y'",
            // Font-relative lists computed per element, equal at 16px.
            "grid-auto-rows:1em",
            "grid-auto-rows:16px",
            "grid-auto-rows:1em",
        ]
        .map(String::from)
        .to_vec();
        let source = styles(&declarations);
        let tracks = |s: &ComputedStyle| match s.get(crate::Property::GridTemplateColumns) {
            Some(ComputedValue::GridTracks(l)) => l.try_clone().unwrap(),
            other => panic!("{other:?}"),
        };
        assert!(!BudgetShared::ptr_eq(
            &tracks(&source[0]),
            &tracks(&source[1])
        ));
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let ids: Vec<StyleId> = source
            .into_iter()
            .map(|s| pool.intern(s).unwrap())
            .collect();
        assert_eq!(ids[1], ids[0]);
        assert_eq!(ids[7], ids[0]);
        for (i, id) in ids.iter().enumerate().take(7).skip(2) {
            assert!(ids.iter().take(i).all(|other| other != id), "{i}");
        }
        assert_eq!((ids[9], ids[10]), (ids[8], ids[8]));
        // One canonical group, so one canonical list.
        assert!(BudgetShared::ptr_eq(
            &tracks(pool.get(ids[0]).unwrap()),
            &tracks(pool.get(ids[7]).unwrap())
        ));
        assert_eq!((pool.len(), pool.rare.len), (7, 7));
    }
    #[test]
    fn grid_values_differing_only_in_contents_do_not_share_a_bucket() {
        let declarations: Vec<String> = (0..300)
            .map(|i| match i % 3 {
                0 => format!("grid-template-columns:[a] 1px {i}fr"),
                1 => format!("grid-template-areas:'a{i} b'"),
                _ => format!("grid-row-start:{i} name"),
            })
            .collect();
        let (_, comparisons) = interned(&declarations);
        assert!(comparisons < 300 + 16, "{comparisons}");
    }
    #[test]
    #[allow(clippy::panic, clippy::indexing_slicing)]
    fn grid_track_lists_keep_a_signed_zero_to_the_bit() {
        let [a, b, c] = styles(&[
            "grid-template-rows:-0px minmax(-0px, 1fr) 0fr".to_string(),
            "grid-template-rows:0px minmax(0px, 1fr) -0fr;width:1px".to_string(),
            "grid-template-rows:0px minmax(0px, 1fr) -0fr".to_string(),
        ])
        .try_into()
        .unwrap();
        let first = |s: &ComputedStyle| match s.get(crate::Property::GridTemplateRows) {
            Some(ComputedValue::GridTracks(l)) => match l.items().first() {
                Some(TrackItem::Size(TrackSize::Breadth(Breadth::Length(Length::Px(px))))) => *px,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        };
        assert!(first(&a).is_sign_negative());
        assert!(first(&b).is_sign_positive());
        assert_eq!(a, c);
        let (Some(x), Some(y)) = (
            a.get(crate::Property::GridTemplateRows),
            c.get(crate::Property::GridTemplateRows),
        ) else {
            panic!()
        };
        assert!(!x.same(y));
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let ids = [pool.intern(a).unwrap(), pool.intern(b).unwrap()];
        assert!(first(pool.get(ids[0]).unwrap()).is_sign_negative());
        assert!(first(pool.get(ids[1]).unwrap()).is_sign_positive());
        assert_eq!(pool.rare.len, 2);
        // Apart by those signs alone, styles are one, the first.
        assert_eq!(pool.intern(c).unwrap(), ids[0]);
        // Its group is the second's, to the bit.
        assert_eq!(pool.rare.len, 2);
        // A flex factor's sign alone tells groups apart too.
        let [p, n] = styles(&[
            "grid-template-rows:0fr".to_string(),
            "grid-template-rows:-0fr".to_string(),
        ])
        .try_into()
        .unwrap();
        let (Some(x), Some(y)) = (
            p.get(crate::Property::GridTemplateRows),
            n.get(crate::Property::GridTemplateRows),
        ) else {
            panic!()
        };
        assert!(!x.same(y));
        assert_eq!(x, y);
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let (first, second) = (pool.intern(p).unwrap(), pool.intern(n).unwrap());
        assert_eq!(first, second);
        assert_eq!(pool.rare.len, 2);
    }
    #[test]
    #[allow(clippy::panic, clippy::indexing_slicing)]
    fn a_group_holding_grid_values_is_charged_by_their_contents() {
        let list = |n: usize| {
            let tracks: Vec<String> = (0..n).map(|i| format!("[l{i}] {i}px")).collect();
            format!("grid-template-columns:{}", tracks.join(" "))
        };
        let declarations: Vec<String> = vec![
            "width:1px".into(),
            "order:1".into(),
            list(1),
            list(100),
            "grid-row:span 2 name-of-a-line-past-sixty-two-bytes-0123456789abcdefghijklmnopqrstuvwxyz"
                .into(),
            "grid-template-areas:'a b' 'c d'".into(),
        ];
        let source = styles(&declarations);
        let owned: Vec<usize> = source
            .iter()
            .map(|s| s.rare.values.iter().map(owned_charge).sum())
            .collect();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let mut charges = Vec::new();
        for style in source {
            let bytes = pool.charged_bytes();
            pool.intern(style).unwrap();
            charges.push(pool.charged_bytes() - bytes);
        }
        let group = 128 + 2 * size_of::<Rare>();
        // The first also interns the custom map and the initial group.
        for (charge, owned) in charges.iter().zip(&owned).skip(1) {
            assert_eq!(*charge, 9_728 + group + owned);
        }
        let item = size_of::<TrackItem>();
        let text = size_of::<crate::syntax::Text>();
        assert_eq!(owned[1], 0);
        // A name and a size, the name inline: their doubled storage with
        // the capacity td-alloc reserved, and 32 a name.
        assert!(owned[2] >= 2 * size_of::<TrackList>() + 2 * 2 * item + 2 * text + 32);
        assert!(owned[3] >= owned[2] + 2 * 198 * item + 2 * 99 * text + 99 * 32);
        // A name past 62 bytes has storage of its own, doubled.
        assert!(owned[4] >= 2 * size_of::<GridLine>() + 2 * 72 + 32);
        assert!(owned[5] >= 2 * size_of::<GridAreas>() + 2 * 4 * size_of::<GridArea>() + 4 * 32);
    }
    #[test]
    fn huge_distinct_track_lists_are_refused_by_their_charge() {
        // A thousand tracks an element, each list distinct: about 0.1 MB
        // a group, so MAX_POOL_BYTES binds after some 500 elements.
        let tracks = vec!["1px"; crate::values::MAX_GRID_TRACKS - 1].join(" ");
        let declarations: Vec<String> = (0..700)
            .map(|i| format!("grid-template-columns:{i}px {tracks}"))
            .collect();
        let mut pool = StylePool::new(PoolLimits::default()).unwrap();
        let mut refused = None;
        for (i, style) in styles(&declarations).into_iter().enumerate() {
            if let Err(error) = pool.intern(style) {
                refused = Some((i, error));
                break;
            }
        }
        let (at, error) = refused.unwrap();
        assert_eq!(error, Error::Limit("MAX_POOL_BYTES"));
        assert!((400..700).contains(&at), "{at}");
        assert!(pool.charged_bytes() <= MAX_POOL_BYTES);
        assert_eq!(pool.len(), at);
    }
}
