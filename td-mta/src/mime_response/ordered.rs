//! Complete ordinal-ordered classification under one immutable source binding.
use crate::{
    admission::work::Meter,
    mime_body_lists::Node,
    mime_part_headers::label_json,
    nfc::{HeaderBudget, Scratch},
    ports::Tick,
};
use {
    crate::mime_response::bound::ClassifiedView, crate::mime_response::bound::Error,
    crate::mime_response::bound::PartCursor, crate::mime_response::bound::Structure,
};
/// Exclusive complete-preorder construction; caller slots remain provisional.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::ordered::Classifying<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::ordered::Classifying<'_, '_, '_>>();
/// ```
pub struct Classifying<'a, 'w, 'n> {
    structure: Structure<'a, 'w>,
    nodes: &'n mut [Node],
    next: usize,
}
impl<'a, 'w, 'n> Classifying<'a, 'w, 'n> {
    /// Caller node backing must already be admitted beside the original parts.
    pub fn new(structure: Structure<'a, 'w>, nodes: &'n mut [Node]) -> Result<Self, Error> {
        let count = structure.parts()?.len();
        let nodes = nodes.get_mut(..count).ok_or(Error::NodeCapacity)?;
        Ok(Self {
            structure,
            nodes,
            next: 0,
        })
    }
    pub fn total(&self) -> Result<usize, Error> {
        Ok(self.structure.parts()?.len())
    }
    pub fn completed(&self) -> Result<usize, Error> {
        self.structure.parts()?;
        Ok(self.next)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.structure.check_deadline(now)
    }
    /// Project exactly the next original ordinal; no caller ordinal can skip a cell.
    pub fn next<'m>(
        &'m mut self,
        backing: label_json::Backing<'m>,
        scratch: &'m mut Scratch,
    ) -> Result<Part<'a, 'm>, Error> {
        self.structure.parts()?;
        let ordinal = self
            .next
            .checked_add(1)
            .and_then(|value| u16::try_from(value).ok());
        let node = self.nodes.get_mut(self.next);
        let (Some(ordinal), Some(node)) = (ordinal, node) else {
            self.structure.failure = Some(Error::InvalidState);
            return Err(Error::InvalidState);
        };
        let child = self.structure.metadata(ordinal, backing, scratch)?;
        Ok(Part {
            child,
            node,
            next: &mut self.next,
            following: ordinal,
        })
    }
    /// Fresh admission precedes the all-parts decision; only complete nodes escape.
    pub fn finish(mut self, now: Tick) -> Result<Classified<'a, 'w, 'n>, Error> {
        self.structure.check_deadline(now)?;
        if self.next != self.nodes.len() {
            return Err(Error::InvalidState);
        }
        Ok(Classified {
            structure: self.structure,
            nodes: self.nodes,
        })
    }
}
/// Exclusive one-part projection. Safe forgetting leaves the original binding retired.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::ordered::Part<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::ordered::Part<'_, '_>>();
/// ```
pub struct Part<'a, 'w> {
    child: PartCursor<'a, 'w>,
    node: &'w mut Node,
    next: &'w mut usize,
    following: u16,
}
impl<'w> Part<'_, 'w> {
    pub fn poll(&mut self, now: Tick) -> Result<crate::mime_structure::Status, Error> {
        self.child.poll(now)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.child.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<
        (
            ClassifiedView<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        let (view, work, budget, scratch) = self.child.finish_classified(now)?;
        *self.node = view.node;
        *self.next = usize::from(self.following);
        Ok((view, work, budget, scratch))
    }
}
/// Passive complete cells; copied slices grant no source/publication authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct View<'w, 'n> {
    pub parts: &'w [crate::mime_structure::Part],
    pub nodes: &'n [Node],
}
/// Complete classified preorder, still bound to original source and budgets.
/// No passive copies confer source, locator or response publication authority.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::ordered::Classified<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::ordered::Classified<'_, '_, '_>>();
/// ```
pub struct Classified<'a, 'w, 'n> {
    pub(in crate::mime_response) structure: Structure<'a, 'w>,
    pub(in crate::mime_response) nodes: &'n [Node],
}
impl<'w, 'n> Classified<'_, 'w, 'n> {
    pub fn parts(&self) -> Result<&[crate::mime_structure::Part], Error> {
        self.structure.parts()
    }
    pub fn nodes(&self) -> Result<&[Node], Error> {
        self.structure.parts()?;
        Ok(self.nodes)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.structure.check_deadline(now)
    }
    pub fn finish(
        self,
        now: Tick,
    ) -> Result<(View<'w, 'n>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        let (parts, work, budget) = self.structure.finish(now)?;
        Ok((
            View {
                parts,
                nodes: self.nodes,
            },
            work,
            budget,
        ))
    }
}
const _: () = assert!(
    std::mem::size_of::<Classifying<'_, '_, '_>>()
        + std::mem::size_of::<Part<'_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 8 * 1024
);
const _: () = assert!(std::mem::size_of::<Classified<'_, '_, '_>>() <= 128);
#[cfg(test)]
#[path = "ordered/tests.rs"]
pub(in crate::mime_response) mod tests;
