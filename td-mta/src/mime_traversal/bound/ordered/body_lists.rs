//! Select body lists only from the complete original classified preorder.
use super::{Classified, Error};
use crate::{
    admission::work::Meter,
    limits::Limits,
    mime_body_lists::{self, Node, Status},
    mime_traversal::Part,
    nfc::HeaderBudget,
    ports::Tick,
};

struct Binding<'a, 'w, 'n> {
    // Keep the original immutable source pinned for later response composition.
    _source: &'a [u8],
    _base: u64,
    _header_limit: u64,
    parts: &'w [Part],
    nodes: &'n [Node],
}

/// Passive lists and original cells, provisional through fresh whole-job publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct View<'w, 'n> {
    pub structure: super::View<'w, 'n>,
    pub lists: mime_body_lists::View<'w>,
}

/// Exclusive selection consumes the private completed classifier, never passive cells.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::Selecting<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::Selecting<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use td_mta::{limits::Limits, mime_body_lists::Backing, ports::Tick};
/// use td_mta::mime_traversal::bound::ordered::{self, body_lists::Selecting};
/// fn substitute<'w, 'n>(view: ordered::View<'w, 'n>, output: Backing<'w>) {
///     let _ = Selecting::new(view, &Limits::default(), output, Tick(1));
/// }
/// ```
pub struct Selecting<'a, 'w, 'n> {
    binding: Binding<'a, 'w, 'n>,
    child: mime_body_lists::Cursor<'n, 'w>,
}
impl<'a, 'w, 'n> Selecting<'a, 'w, 'n> {
    /// Output windows must already be admitted beside original descriptor/node backing.
    /// Limits only constrain selection of the already bounded original nodes.
    pub fn new(
        mut classified: Classified<'a, 'w, 'n>,
        limits: &Limits,
        output: mime_body_lists::Backing<'w>,
        now: Tick,
    ) -> Result<Self, Error> {
        classified.check_deadline(now)?;
        let structure = classified.structure;
        let child = mime_body_lists::Cursor::new(
            classified.nodes,
            limits,
            output,
            structure.work,
            structure.budget,
        )
        .map_err(Error::BodyLists)?;
        Ok(Self {
            binding: Binding {
                _source: structure.source,
                _base: structure.base,
                _header_limit: structure.header_limit,
                parts: structure.parts,
                nodes: classified.nodes,
            },
            child,
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.child.poll(now).map_err(Error::BodyLists)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.child.check_deadline(now).map_err(Error::BodyLists)
    }
    pub fn value(&self) -> Option<View<'_, '_>> {
        Some(View {
            structure: super::View {
                parts: self.binding.parts,
                nodes: self.binding.nodes,
            },
            lists: self.child.value()?,
        })
    }
    pub fn finish(self, now: Tick) -> Result<Selected<'a, 'w, 'n>, Error> {
        let (lists, work, budget) = self.child.finish(now).map_err(Error::BodyLists)?;
        Ok(Selected {
            binding: self.binding,
            lists,
            work,
            budget,
            failure: None,
        })
    }
}

/// Complete selection retains the original binding and original admission owners.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_traversal::bound::ordered::body_lists::Selected<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_traversal::bound::ordered::body_lists::Selected<'_, '_, '_>>();
/// ```
pub struct Selected<'a, 'w, 'n> {
    binding: Binding<'a, 'w, 'n>,
    lists: mime_body_lists::View<'w>,
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    failure: Option<Error>,
}
impl<'w, 'n> Selected<'_, 'w, 'n> {
    pub fn value(&self) -> Option<View<'_, '_>> {
        if self.failure.is_some() {
            return None;
        }
        Some(View {
            structure: super::View {
                parts: self.binding.parts,
                nodes: self.binding.nodes,
            },
            lists: self.lists,
        })
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge(self.work, now, 0, 0, &mut 0)
            .map_err(Error::Admission);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(View<'w, 'n>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        Ok((
            View {
                structure: super::View {
                    parts: self.binding.parts,
                    nodes: self.binding.nodes,
                },
                lists: self.lists,
            },
            self.work,
            self.budget,
        ))
    }
}
const _: () = assert!(
    std::mem::size_of::<Selecting<'_, '_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 16 * 1024
);
const _: () = assert!(std::mem::size_of::<Selected<'_, '_, '_>>() <= 256);

#[cfg(test)]
mod tests;
