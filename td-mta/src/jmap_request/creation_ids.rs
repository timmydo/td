//! Request-local creation IDs. Reserve storage and response growth before effects.
use super::{children::Name, dispatch::Error};
use crate::wire::{self, LocalId};

type Mapping<'a> = (&'a [u8], &'a [u8]);

struct Entry {
    key: Name,
    value: Name,
}
pub(super) const MAX_CREATED: usize = 64;
pub(super) struct CreationIds {
    entries: Vec<Entry>,
    sorted: [usize; MAX_CREATED],
}
impl Default for CreationIds {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            sorted: [0; MAX_CREATED],
        }
    }
}
impl CreationIds {
    /// A failed creation leaves its reserved slot empty, or its old value intact.
    /// No allocation or lookup is needed when the successful ID arrives.
    pub(super) fn prepare(
        &mut self,
        key: &[u8],
        remaining: &mut u64,
        mut check: impl FnMut() -> Result<(), Error>,
    ) -> Result<usize, Error> {
        let mut name = Name::new();
        name.push(key)?;
        name.id()?;
        let (mut low, mut high) = (0, self.entries.len());
        while low < high {
            check()?;
            let middle = low + (high - low) / 2;
            let slot = *self.sorted.get(middle).ok_or(Error::State)?;
            let entry = self.entries.get(slot).ok_or(Error::State)?;
            match entry.key.bytes().cmp(key) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Ok(slot),
            }
        }
        if self.entries.len() == MAX_CREATED {
            return Err(Error::ResponseCapacity);
        }
        check()?;
        let charge = std::mem::size_of::<Entry>() + key.len() + 255 + 6;
        let after = remaining
            .checked_sub(charge as u64)
            .ok_or(Error::ResponseCapacity)?;
        self.entries
            .try_reserve_exact(1)
            .map_err(|_| Error::ResponseCapacity)?;
        let index = self.entries.len();
        for position in (low..index).rev() {
            let old = *self.sorted.get(position).ok_or(Error::State)?;
            *self.sorted.get_mut(position + 1).ok_or(Error::State)? = old;
        }
        *self.sorted.get_mut(low).ok_or(Error::State)? = index;
        self.entries.push(Entry {
            key: name,
            value: Name::new(),
        });
        *remaining = after;
        Ok(index)
    }
    pub(super) fn set(&mut self, slot: usize, value: &[u8]) -> Result<(), Error> {
        let mut name = Name::new();
        name.push(value)?;
        name.id()?;
        self.entries.get_mut(slot).ok_or(Error::State)?.value = name;
        Ok(())
    }
    pub(super) fn created<T: LocalId>(&mut self, slot: usize, id: T) -> Result<(), Error> {
        let mut bytes = [0; wire::IdKind::WIRE_BYTES];
        let value = wire::encode_id(id, &mut bytes).map_err(|_| Error::State)?;
        self.set(slot, value.as_bytes())
    }
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn entry(&self, index: usize) -> Result<Option<Mapping<'_>>, Error> {
        if index == self.len() {
            return Ok(None);
        }
        let slot = *self.sorted.get(index).ok_or(Error::State)?;
        let entry = self.entries.get(slot).ok_or(Error::State)?;
        Ok(Some((entry.key.bytes(), entry.value.bytes())))
    }
}
/// IDs have already passed the grammar, so this bounded pair needs no escaping.
pub(super) fn pair(
    key: &[u8],
    value: &[u8],
    first: bool,
    output: &mut [u8],
) -> Result<usize, Error> {
    let mut length = 0usize;
    for bytes in [
        if first {
            b"\"".as_slice()
        } else {
            b",\"".as_slice()
        },
        key,
        b"\":\"",
        value,
        b"\"",
    ] {
        let end = length.checked_add(bytes.len()).ok_or(Error::State)?;
        output
            .get_mut(length..end)
            .ok_or(Error::State)?
            .copy_from_slice(bytes);
        length = end;
    }
    Ok(length)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ids::EmailId;
    fn encoded(map: &CreationIds) -> Vec<u8> {
        let mut result = Vec::new();
        for index in 0..map.len() {
            let mut bytes = [0; 520];
            let (key, value) = map.entry(index).unwrap().unwrap();
            if value.is_empty() {
                continue;
            }
            let size = pair(key, value, result.is_empty(), &mut bytes).unwrap();
            result.extend_from_slice(bytes.get(..size).unwrap());
        }
        result
    }
    #[test]
    fn failed_replacements_leave_latest_success_and_empty_slots_stay_private() {
        let mut map = CreationIds::default();
        let mut budget = 10000;
        let first = map.prepare(b"draft", &mut budget, || Ok(())).unwrap();
        assert!(encoded(&map).is_empty());
        map.created(first, EmailId::from_bytes([1; 16])).unwrap();
        let before = encoded(&map);
        let replacement = map.prepare(b"draft", &mut budget, || Ok(())).unwrap();
        assert_eq!(replacement, first);
        map.prepare(b"failed", &mut budget, || Ok(())).unwrap();
        assert_eq!(encoded(&map), before);
        map.created(replacement, EmailId::from_bytes([2; 16]))
            .unwrap();
        assert_eq!(
            encoded(&map),
            br#""draft":"e02020202020202020202020202020202""#
        );
    }
    #[test]
    fn capacity_and_deadline_refuse_before_reserving_and_ids_are_validated() {
        let mut map = CreationIds::default();
        assert_eq!(
            map.prepare(b"draft", &mut 0, || Ok(())),
            Err(Error::ResponseCapacity)
        );
        assert_eq!(map.len(), 0);
        assert_eq!(
            map.prepare(b"draft", &mut 10000, || Err(Error::ExecutionDeadline)),
            Err(Error::ExecutionDeadline)
        );
        assert_eq!(map.len(), 0);
        assert_eq!(
            map.prepare(b"bad\"", &mut 10000, || Ok(())),
            Err(Error::InvalidArguments)
        );
        let key = "a".repeat(255);
        let slot = map.prepare(key.as_bytes(), &mut 10000, || Ok(())).unwrap();
        map.set(slot, "b".repeat(255).as_bytes()).unwrap();
        assert_eq!(encoded(&map).len(), 515);
    }
}

#[cfg(test)]
#[test]
#[allow(clippy::unwrap_used)]
fn aggregate_bound_and_binary_lookup_preserve_prepared_slots() {
    let mut map = CreationIds::default();
    let mut budget = 100000;
    let mut steps = 0;
    for index in (0..MAX_CREATED).rev() {
        let key = format!("key{index:02}");
        let slot = map
            .prepare(key.as_bytes(), &mut budget, || {
                steps += 1;
                Ok(())
            })
            .unwrap();
        map.created(slot, crate::ids::EmailId::from_bytes([index as u8; 16]))
            .unwrap();
    }
    assert!(steps < MAX_CREATED * 8);
    assert_eq!(
        map.prepare(b"extra", &mut budget, || Ok(())),
        Err(Error::ResponseCapacity)
    );
    let before = budget;
    let slot = map.prepare(b"key00", &mut budget, || Ok(())).unwrap();
    assert_eq!(budget, before);
    map.created(slot, crate::ids::EmailId::from_bytes([99; 16]))
        .unwrap();
    assert_eq!(
        map.entry(0).unwrap().unwrap().1,
        b"e63636363636363636363636363636363"
    );
    assert_eq!(map.len(), MAX_CREATED);
}
