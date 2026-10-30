//! Compact retained ID planning shared by typed /get methods.
use super::{
    push::{Children, Item, Name},
    Error, Worker,
};
use crate::wire::LocalId;
use std::ops::Range;
use td_json::scan;
pub(super) struct Requested<T> {
    pub(super) name: Option<Range<u64>>,
    pub(super) id: Option<T>,
    pub(super) found: bool,
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn get_ids<T: LocalId + Eq>(
        &mut self,
        ids: Option<Item>,
        maximum: usize,
    ) -> Result<Vec<Requested<T>>, Error> {
        let mut requested = Vec::<Requested<T>>::new();
        requested
            .try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        let mut local_ids = Vec::<[u8; 16]>::new();
        local_ids
            .try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        if let Some(ids) = ids {
            let mut children = Children::new(ids.range, scan::Kind::Array)?;
            let mut count = 0usize;
            while let Some(item) = children.next(self)? {
                count = count
                    .checked_add(1)
                    .filter(|n| *n <= maximum)
                    .ok_or(Error::RequestTooLarge)?;
                let name = self.push_string(&item)?;
                let id: Option<T> = name.wire_id()?.local();
                self.check()?;
                let duplicate = if let Some(id) = id {
                    match local_ids.binary_search(id.storage_bytes()) {
                        Ok(_) => true,
                        Err(position) => {
                            local_ids.insert(position, *id.storage_bytes());
                            false
                        }
                    }
                } else {
                    let mut duplicate = false;
                    for old in requested.iter().filter(|old| old.id.is_none()) {
                        self.check()?;
                        let range = old.name.clone().ok_or(Error::State)?;
                        if self
                            .push_string(&Item {
                                key: Name::new(),
                                kind: scan::Kind::String,
                                range,
                            })?
                            .bytes()
                            == name.bytes()
                        {
                            duplicate = true;
                            break;
                        }
                    }
                    duplicate
                };
                if !duplicate {
                    requested.push(Requested {
                        name: Some(item.range),
                        id,
                        found: false,
                    });
                }
            }
        }
        Ok(requested)
    }
    pub(super) fn get_not_found<T>(&mut self, requested: &[Requested<T>]) -> Result<(), Error> {
        let mut first = true;
        for item in requested {
            self.check()?;
            if !item.found {
                if !first {
                    self.append(b",")?;
                }
                first = false;
                let range = item.name.clone().ok_or(Error::State)?;
                let name = self.push_string(&Item {
                    key: Name::new(),
                    kind: scan::Kind::String,
                    range,
                })?;
                self.push_id(&name)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_lookup_fits_the_existing_method_object_reservation() {
        assert!(
            std::mem::size_of::<super::Requested<crate::ids::EmailId>>()
                + std::mem::size_of::<[u8; 16]>()
                <= crate::limits::METHOD_OBJECT_BYTES
        );
    }
}
