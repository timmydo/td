//! Selected history coverage and bounded source routing, without file I/O or pin ownership.
use crate::{
    format::{bindings::Selection, Sequence},
    ports::{Error, ViewIdentity},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeSource {
    History { index: usize },
    Active,
}
/// Borrows the selected manifest; every named file still needs its own checks.
pub struct ChangeRoute<'m> {
    selection: Selection<'m>,
    view: ViewIdentity,
}
impl<'m> ChangeRoute<'m> {
    pub fn new(selection: Selection<'m>, view: ViewIdentity) -> Result<Self, Error> {
        super::active::validate_view(selection, view).map_err(|_| Error::Invalid)?;
        if view.history_floor > view.committed_sequence {
            return Err(Error::Invalid);
        }
        if view.history_floor < view.checkpoint {
            let first = selection
                .manifest()
                .history(0)
                .map_err(|_| Error::HistoryLost)?;
            if first.base > view.history_floor {
                return Err(Error::HistoryLost);
            }
        }
        Ok(Self { selection, view })
    }
    pub const fn identity(&self) -> ViewIdentity {
        self.view
    }
    /// At most the manifest's 64 descriptors; no directory scan or I/O.
    pub fn source(&self, view: ViewIdentity, sequence: Sequence) -> Result<ChangeSource, Error> {
        if view != self.view {
            return Err(Error::Conflict);
        }
        if sequence > view.committed_sequence {
            return Err(Error::Invalid);
        }
        if sequence <= view.history_floor {
            return Err(Error::HistoryLost);
        }
        if sequence > view.checkpoint {
            return Ok(ChangeSource::Active);
        }
        let manifest = self.selection.manifest();
        for index in 0..manifest.history_count() {
            let descriptor = manifest.history(index).map_err(|_| Error::Corrupt)?;
            if descriptor.base < sequence && sequence <= descriptor.through {
                return Ok(ChangeSource::History { index });
            }
        }
        Err(Error::HistoryLost)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        format::{container::Current, manifest, Table, TABLE_COUNT},
        ids::AccountId,
        ports::Crypto,
    };
    use td_crypto::{Digest, Provider};
    const ACCOUNT: AccountId = AccountId::from_bytes([0x33; 16]);
    struct Selected {
        format: Vec<u8>,
        current: Vec<u8>,
        manifest: Vec<u8>,
    }
    fn hex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        assert!(rest.is_empty());
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
    fn hash(bytes: &[u8]) -> [u8; 32] {
        let mut hash = Provider.sha256().unwrap();
        hash.update(bytes).unwrap();
        hash.finish().unwrap()
    }
    impl Selected {
        fn selection(&self) -> Selection<'_> {
            Selection::decode(
                &Provider,
                ACCOUNT,
                &self.format,
                &self.current,
                &self.manifest,
            )
            .unwrap()
        }
        fn view(&self, end: u64, floor: u64) -> ViewIdentity {
            let header = self.selection().manifest().header();
            ViewIdentity {
                account: header.account,
                epoch: header.epoch,
                generation: header.generation,
                checkpoint: header.through,
                segment: header.active_segment,
                committed_offset: 96 + 132 * (end - header.through.number()),
                committed_sequence: Sequence::from_u64(end),
                history_floor: Sequence::from_u64(floor),
            }
        }
        fn new(ranges: &[(u64, u64)], checkpoint: u64) -> Self {
            let mut selected = Self {
                format: hex(include_str!("../../tests/fixtures/format-v1/format.hex")),
                current: hex(include_str!(
                    "../../tests/fixtures/format-v1/current-history.hex"
                )),
                manifest: hex(include_str!(
                    "../../tests/fixtures/format-v1/manifest-history.hex"
                )),
            };
            let original = selected.selection();
            let manifest = original.manifest();
            let header = manifest::Header {
                through: Sequence::from_u64(checkpoint),
                active_segment: ranges.len() as u64 + 1,
                ..manifest.header()
            };
            let tables = std::array::from_fn::<_, TABLE_COUNT, _>(|index| {
                manifest
                    .table(Table::from_tag(index as u16 + 1).unwrap())
                    .unwrap()
            });
            let histories: Vec<_> = ranges
                .iter()
                .enumerate()
                .map(|(index, (base, through))| manifest::HistoryDescriptor {
                    segment: index as u64 + 1,
                    base: Sequence::from_u64(*base),
                    through: Sequence::from_u64(*through),
                    file_bytes: 96 + 132 * (through - base),
                    digest: [index as u8; 32],
                })
                .collect();
            let current = original.current();
            let mut bytes = vec![0; crate::format::MAX_MANIFEST_BYTES];
            let size =
                manifest::encode(&Provider, header, &tables, &histories, &mut bytes).unwrap();
            bytes.truncate(size);
            selected.manifest = bytes;
            Current {
                manifest_digest: hash(&selected.manifest),
                ..current
            }
            .encode(&Provider, &mut selected.current)
            .unwrap();
            selected
        }
    }
    #[test]
    fn routes_exact_segment_boundaries_across_maximum_manifest() {
        let ranges: Vec<_> = (0..64).map(|base| (base, base + 1)).collect();
        let selected = Selected::new(&ranges, 64);
        let view = selected.view(66, 0);
        let route = ChangeRoute::new(selected.selection(), view).unwrap();
        assert_eq!(route.identity(), view);
        for sequence in 1..=64 {
            assert_eq!(
                route.source(view, Sequence::from_u64(sequence)),
                Ok(ChangeSource::History {
                    index: sequence as usize - 1
                })
            );
        }
        for sequence in [65, 66] {
            assert_eq!(
                route.source(view, Sequence::from_u64(sequence)),
                Ok(ChangeSource::Active)
            );
        }
        assert_eq!(
            route.source(view, Sequence::default()),
            Err(Error::HistoryLost)
        );
        assert_eq!(
            route.source(view, Sequence::from_u64(67)),
            Err(Error::Invalid)
        );
        assert!(std::mem::size_of::<ChangeRoute<'_>>() <= 512);
    }
    #[test]
    fn missing_coverage_is_history_lost_and_floor_boundary_is_exclusive() {
        let selected = Selected::new(&[(5, 9), (9, 12)], 12);
        assert!(matches!(
            ChangeRoute::new(selected.selection(), selected.view(14, 4)),
            Err(Error::HistoryLost)
        ));
        for floor in [5, 7, 9, 12, 13, 14] {
            let view = selected.view(14, floor);
            let route = ChangeRoute::new(selected.selection(), view).unwrap();
            for sequence in floor + 1..=14 {
                let expected = if sequence <= 9 {
                    ChangeSource::History { index: 0 }
                } else if sequence <= 12 {
                    ChangeSource::History { index: 1 }
                } else {
                    ChangeSource::Active
                };
                assert_eq!(
                    route.source(view, Sequence::from_u64(sequence)),
                    Ok(expected)
                );
            }
            assert_eq!(
                route.source(view, Sequence::from_u64(floor)),
                Err(Error::HistoryLost)
            );
        }
        let empty = Selected::new(&[], 12);
        assert!(matches!(
            ChangeRoute::new(empty.selection(), empty.view(14, 11)),
            Err(Error::HistoryLost)
        ));
        let view = empty.view(14, 12);
        let route = ChangeRoute::new(empty.selection(), view).unwrap();
        assert_eq!(
            route.source(view, Sequence::from_u64(13)),
            Ok(ChangeSource::Active)
        );
        assert_eq!(
            route.source(view, Sequence::from_u64(12)),
            Err(Error::HistoryLost)
        );
    }
    #[test]
    fn invalid_view_binding_and_identity_changes_cannot_select_sources() {
        let selected = Selected::new(&[(0, 1)], 1);
        let view = selected.view(2, 0);
        for mode in 0..9 {
            let mut changed = view;
            match mode {
                0 => changed.account = AccountId::from_bytes([1; 16]),
                1 => changed.epoch = crate::ids::StoreEpoch::from_bytes([1; 16]),
                2 => changed.generation += 1,
                3 => changed.checkpoint = Sequence::default(),
                4 => changed.segment += 1,
                5 => changed.committed_offset = 95,
                6 => changed.committed_sequence = Sequence::default(),
                7 => changed.history_floor = Sequence::from_u64(3),
                _ => {
                    changed.committed_offset = (crate::format::JOURNAL_HEADER_BYTES
                        + crate::format::MAX_JOURNAL_FRAME_BYTES
                        + 1) as u64
                }
            }
            assert!(
                matches!(
                    ChangeRoute::new(selected.selection(), changed),
                    Err(Error::Invalid)
                ),
                "mode {mode}"
            );
            let route = ChangeRoute::new(selected.selection(), view).unwrap();
            assert_eq!(
                route.source(changed, Sequence::from_u64(1)),
                Err(Error::Conflict)
            );
            assert_eq!(
                route.source(view, Sequence::from_u64(1)),
                Ok(ChangeSource::History { index: 0 })
            );
        }
    }
}
