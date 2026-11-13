//! Document-local interning, including independently shared custom maps.
use crate::syntax::{Component, Token};
use crate::values::{ComputedStyle, ComputedValue, Family, Length};
use crate::variables::CustomProperties;
use crate::Error;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use td_alloc::{Budget, BudgetMap, BudgetVec};

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
}
impl Default for PoolLimits {
    fn default() -> Self {
        Self {
            styles: MAX_STYLES,
            bytes: MAX_POOL_BYTES,
            work: MAX_POOL_WORK,
            comparisons: MAX_STYLE_COMPARISONS,
        }
    }
}
/// Charges include doubled vector/string capacities and conservative map-node
/// overhead. Pointer identities are only keys for allocations this pool owns.
pub struct StylePool {
    styles: BudgetVec<ComputedStyle>,
    buckets: BudgetMap<u64, BudgetVec<StyleId>>,
    custom: BudgetMap<u64, BudgetVec<CustomProperties>>,
    maps: BudgetMap<usize, ()>,
    tokens: BudgetMap<usize, ()>,
    limits: PoolLimits,
    bytes: usize,
    work: usize,
    comparisons: usize,
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
        Ok(Self {
            styles: BudgetVec::new(memory)?,
            buckets: BudgetMap::new(memory)?,
            custom: BudgetMap::new(memory)?,
            maps: BudgetMap::new(memory)?,
            tokens: BudgetMap::new(memory)?,
            limits: PoolLimits {
                styles: limits.styles.min(MAX_STYLES),
                bytes: limits.bytes.min(MAX_POOL_BYTES),
                work: limits.work.min(MAX_POOL_WORK),
                comparisons: limits.comparisons.min(MAX_STYLE_COMPARISONS),
            },
            bytes: 0,
            work: 0,
            comparisons: 0,
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
        let mut hash = DefaultHasher::new();
        // Canonical maps have a stable address while retained by this pool.
        (std::ptr::from_ref(&*style.custom.entries).addr()).hash(&mut hash);
        let value_work = style.values.values().map(value_work).sum::<usize>();
        self.visit(value_work)?;
        for (property, value) in &style.values {
            (*property as u8).hash(&mut hash);
            hash_value(value, &mut hash);
        }
        let key = hash.finish();
        if let Some(bucket) = self.buckets.get(&key) {
            for id in bucket {
                comparison(&mut self.comparisons, self.limits.comparisons)?;
                visit_count(&mut self.work, value_work, self.limits.work)?;
                if self.get(*id).is_some_and(|old| {
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
        if !self.buckets.contains_key(&key) {
            self.visit(self.buckets.len().saturating_add(1))?;
        }
        let bucket = self.buckets.try_get_or_insert_with(key, BudgetVec::new)?;
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
        let identity = std::ptr::from_ref(&*custom.entries).addr();
        if self.maps.contains_key(&identity) {
            return Ok(());
        }
        // Keep the existing conservative admission model alongside exact
        // owned layouts, including both interning indexes.
        let mut charge = 2048;
        let work_before = self.work;
        for (name, value) in custom.entries.iter() {
            self.visit(name.len() + 1)?;
            charge += 160 + 2 * name.heap_capacity();
            if let Some(tokens) = value {
                let token_id = tokens.as_ptr() as usize;
                // Count traversal even for shared tokens: hashing sees them.
                let token_charge = components_charge(tokens, &mut self.work, self.limits.work)?;
                if !self.tokens.contains_key(&token_id) {
                    charge += token_charge + 128;
                }
            }
        }
        let custom_work = self.work - work_before;
        let mut hasher = DefaultHasher::new();
        custom.entries.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(bucket) = self.custom.get(&key) {
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
        if !self.custom.contains_key(&key) {
            self.visit(self.custom.len().saturating_add(1))?;
        }
        // Retain ownership before publishing pointer identities. A later
        // refusal may leave a valid interning entry, never a dangling key.
        self.custom
            .try_get_or_insert_with(key, BudgetVec::new)?
            .try_push(custom.try_clone()?)?;
        self.visit(self.maps.len().saturating_add(1))?;
        self.maps.try_insert(identity, ())?;
        for tokens in custom.entries.values().flatten() {
            let identity = tokens.as_ptr() as usize;
            if !self.tokens.contains_key(&identity) {
                self.visit(self.tokens.len().saturating_add(1))?;
            }
            self.tokens.try_insert(identity, ())?;
        }
        Ok(())
    }
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
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
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
                    .any(|v| std::ptr::from_ref(&*v.entries).addr() == *identity));
            }
            for identity in pool.tokens.keys() {
                assert!(pool.custom.values().flatten().any(|v| v
                    .entries
                    .values()
                    .flatten()
                    .any(|t| t.as_ptr() as usize == *identity)));
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
