//! Replay original selected part metadata in preorder for response composition.
use {
    crate::admission::work::Meter, crate::mime_body_lists, crate::mime_body_lists::Node,
    crate::mime_part_headers::label_json, crate::mime_response::bound::ClassifiedView,
    crate::mime_response::bound::Error, crate::mime_response::bound::PartCursor,
    crate::mime_response::bound::Structure, crate::mime_structure::Status,
    crate::nfc::HeaderBudget, crate::nfc::Scratch, crate::ports::Tick,
};
use {crate::mime_response::lists::Selected, crate::mime_response::lists::View};

/// Exclusive metadata replay consumes selected ownership, never passive views.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::response::Projecting<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::response::Projecting<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0308
/// use {td_mta::mime_response::lists::View, td_mta::mime_response::response::Projecting, td_mta::ports::Tick};
/// fn substitute(view: View<'_, '_>) { let _ = Projecting::new(view, Tick(1)); }
/// ```
pub struct Projecting<'a, 'w, 'n> {
    pub(in crate::mime_response) structure: Structure<'a, 'w>,
    pub(in crate::mime_response) nodes: &'n [Node],
    pub(in crate::mime_response) lists: mime_body_lists::View<'w>,
    pub(in crate::mime_response) next: usize,
}
impl<'a, 'w, 'n> Projecting<'a, 'w, 'n> {
    pub fn new(mut selected: Selected<'a, 'w, 'n>, now: Tick) -> Result<Self, Error> {
        selected.check_deadline(now)?;
        Ok(Self {
            // Transfer the binding originally minted by fresh traversal finish.
            structure: Structure {
                source: selected.binding.source,
                base: selected.binding.base,
                header_limit: selected.binding.header_limit,
                parts: selected.binding.parts,
                work: selected.work,
                budget: selected.budget,
                failure: None,
            },
            nodes: selected.binding.nodes,
            lists: selected.lists,
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
    /// Reuse one separately admitted metadata window and scratch per original part.
    pub fn next<'m>(
        &'m mut self,
        backing: label_json::Backing<'m>,
        scratch: &'m mut Scratch,
        now: Tick,
    ) -> Result<Part<'a, 'm>, Error> {
        self.structure.check_deadline(now)?;
        let ordinal = self.next.checked_add(1).and_then(|n| u16::try_from(n).ok());
        let node = self.nodes.get(self.next).copied();
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
    /// Whole replay completion freshly admits before creating its private owner.
    pub fn finish(mut self, now: Tick) -> Result<Projected<'a, 'w, 'n>, Error> {
        self.structure.check_deadline(now)?;
        if self.next != self.nodes.len() {
            return Err(Error::InvalidState);
        }
        Ok(Projected {
            structure: self.structure,
            nodes: self.nodes,
            lists: self.lists,
        })
    }
}

/// One original metadata replay. Safe forgetting leaves the whole owner retired.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::response::Part<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::response::Part<'_, '_>>();
/// ```
pub struct Part<'a, 'w> {
    pub(in crate::mime_response) child: PartCursor<'a, 'w>,
    pub(in crate::mime_response) node: Node,
    pub(in crate::mime_response) next: &'w mut usize,
    pub(in crate::mime_response) following: u16,
}
impl<'w> Part<'_, 'w> {
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        self.child.poll(now)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        self.child.check_deadline(now)
    }
    pub fn value(&self) -> Option<ClassifiedView<'_>> {
        let view = self.child.value()?;
        Some(ClassifiedView {
            part: view.part,
            metadata: view.metadata,
            node: self.node,
        })
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
        let (view, work, budget, scratch) = self.child.finish(now)?;
        *self.next = usize::from(self.following);
        Ok((
            ClassifiedView {
                part: view.part,
                metadata: view.metadata,
                node: self.node,
            },
            work,
            budget,
            scratch,
        ))
    }
}

/// Complete metadata visitation, provisional through whole-job admission.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_response::response::Projected<'_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_response::response::Projected<'_, '_, '_>>();
/// ```
pub struct Projected<'a, 'w, 'n> {
    pub(in crate::mime_response) structure: Structure<'a, 'w>,
    nodes: &'n [Node],
    pub(in crate::mime_response) lists: mime_body_lists::View<'w>,
}
impl<'w, 'n> Projected<'_, 'w, 'n> {
    pub fn value(&self) -> Option<View<'_, '_>> {
        Some(View {
            structure: crate::mime_response::ordered::View {
                parts: self.structure.parts().ok()?,
                nodes: self.nodes,
            },
            lists: self.lists,
        })
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
                structure: crate::mime_response::ordered::View {
                    parts,
                    nodes: self.nodes,
                },
                lists: self.lists,
            },
            work,
            budget,
        ))
    }
}
const _: () = assert!(
    std::mem::size_of::<Projecting<'_, '_, '_>>()
        + std::mem::size_of::<Part<'_, '_>>()
        + std::mem::size_of::<HeaderBudget>()
        <= 8 * 1024
);
const _: () = assert!(std::mem::size_of::<Projected<'_, '_, '_>>() <= 256);

#[cfg(test)]
#[path = "response/tests.rs"]
pub(in crate::mime_response) mod tests;
