//! A page's inline pictures as its layouts lay them (td-web/DESIGN.md
//! §10, "Subresources"): each decoded picture, the copies
//! fitted to the boxes it is laid in, and the set a layout lays, by node.
//! Where the pictures come from, and on what thread, is the caller's.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::memory::Resident;
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetVec};
use td_html::document::NodeId;
use td_ui::picture::Fitted;

/// The most `img` elements a page lays a picture for.
pub const MAX_PICTURES: usize = 128;

/// Told what dropped, on the thread its last drop runs on: a caller that
/// keeps a page's last drops on one thread checks it so. Shared by every
/// value it rides on, and never let go.
pub type Witness = &'static (dyn Fn(&'static str) + Send + Sync);

/// A witness riding on a value, told `what` when the value drops; none by
/// default.
pub struct Witnessed {
    witness: Option<Witness>,
    what: &'static str,
}

impl Witnessed {
    /// No one told.
    pub const fn none(what: &'static str) -> Self {
        Self {
            witness: None,
            what,
        }
    }

    pub fn new(what: &'static str, witness: Witness) -> Self {
        Self {
            witness: Some(witness),
            what,
        }
    }

    /// The same witness, told `what` of another value.
    fn also(&self, what: &'static str) -> Self {
        Self {
            witness: self.witness,
            what,
        }
    }
}

impl Drop for Witnessed {
    fn drop(&mut self) {
        if let Some(witness) = &self.witness {
            witness(self.what);
        }
    }
}

/// A decoded picture: its pixels, premultiplied in the raster's order at
/// its own size, charged to its page's pictures term, shared by the
/// caller's pictures and the layouts that lay it.
pub struct Inline {
    picture: Resident<Fitted>,
    /// Its size as stored, its intrinsic size: a JPEG may be decoded at
    /// a smaller scale.
    natural: (usize, usize),
    /// Every pixel opaque: drawn by copying, not blending.
    opaque: bool,
    witness: Witnessed,
}

impl Inline {
    /// `picture`, of intrinsic size `natural`; told `inline` as it drops,
    /// and its fitted copies `fit`.
    pub fn new(
        picture: Resident<Fitted>,
        natural: (usize, usize),
        witness: Option<Witness>,
    ) -> Self {
        let opaque = picture.bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
        Self {
            picture,
            natural,
            opaque,
            witness: Witnessed {
                witness,
                what: "inline",
            },
        }
    }

    pub fn picture(&self) -> &Fitted {
        &self.picture
    }

    pub fn opaque(&self) -> bool {
        self.opaque
    }

    /// Its intrinsic size, as stored.
    pub fn natural(&self) -> (usize, usize) {
        self.natural
    }

    /// The whole multiple of its size `width` by `height` device pixels
    /// is, if they are one: a box it is drawn in with no copy.
    pub fn factor(&self, width: usize, height: usize) -> Option<usize> {
        let factor = width.checked_div(self.picture.width)?;
        (factor > 0
            && factor.checked_mul(self.picture.width) == Some(width)
            && factor.checked_mul(self.picture.height) == Some(height))
        .then_some(factor)
    }

    /// The picture fitted to `width` by `height` device pixels, charged
    /// to `term`: none when refused.
    pub fn fit(
        &self,
        width: usize,
        height: usize,
        term: &Budget,
        work: &mut u64,
        cancel: &AtomicBool,
    ) -> Option<FitCopy> {
        let cost = self.picture.fit_exact_cost(width, height)?;
        // Its steps spent only once its room is had.
        let left = work.checked_sub(cost.steps)?;
        let bytes = width.checked_mul(height)?.checked_mul(4)?;
        let charge = term.charge(bytes).ok()?;
        let scratch = term.charge(cost.scratch).ok()?;
        *work = left;
        let fitted = self
            .picture
            .fit_exact_until(width, height, &|| cancel.load(Ordering::SeqCst))?;
        drop(scratch);
        let capacity = fitted.bgra.capacity();
        Some(FitCopy {
            fitted: Resident::new(fitted, charge, capacity).ok()?,
            // An average of opaque pixels is opaque.
            opaque: self.opaque,
            _witness: self.witness.also("fit"),
        })
    }
}

/// A picture fitted to the box a layout lays it in, charged to its page's
/// pictures term, shared by the layouts that lay it at that size.
pub struct FitCopy {
    fitted: Resident<Fitted>,
    opaque: bool,
    /// Held for its drop.
    _witness: Witnessed,
}

impl FitCopy {
    pub fn fitted(&self) -> &Fitted {
        &self.fitted
    }

    pub fn opaque(&self) -> bool {
        self.opaque
    }
}

/// The pictures a layout lays, by node index, and the term their fitted
/// copies are charged to.
pub struct Set {
    shown: BudgetVec<(usize, BudgetShared<Inline>)>,
    term: Budget,
    /// The overlap steps a layout's copies may take.
    work: u64,
}

impl Set {
    /// `shown`, each picture by its node's index, ordered here; copies
    /// charged to `term` within `work` overlap steps.
    pub fn new(
        mut shown: BudgetVec<(usize, BudgetShared<Inline>)>,
        term: &Budget,
        work: u64,
    ) -> Result<Self, BudgetError> {
        shown.sort_unstable_by_key(|(node, _)| *node);
        Ok(Self {
            shown,
            term: term.try_clone()?,
            work,
        })
    }

    /// The picture laid for `node`, if it has come.
    pub fn get(&self, node: NodeId) -> Option<&BudgetShared<Inline>> {
        let at = self
            .shown
            .binary_search_by_key(&node.index(), |(at, _)| *at)
            .ok()?;
        self.shown.get(at).map(|(_, inline)| inline)
    }

    pub fn term(&self) -> &Budget {
        &self.term
    }

    pub fn work(&self) -> u64 {
        self.work
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;

    /// A set laying an opaque 3x2 picture for `node`, its copies charged
    /// to `term`.
    pub(crate) fn set_of(node: NodeId, term: &Budget) -> Set {
        let bgra = [1, 2, 3, 255].repeat(6);
        let charge = term.charge(bgra.len()).unwrap();
        let capacity = bgra.capacity();
        let fitted = Fitted {
            width: 3,
            height: 2,
            bgra,
        };
        let inline = Inline::new(
            Resident::new(fitted, charge, capacity).unwrap(),
            (3, 2),
            None,
        );
        assert!(inline.opaque());
        let mut shown = BudgetVec::new(term).unwrap();
        shown
            .try_push((node.index(), BudgetShared::new(term, inline).unwrap()))
            .unwrap();
        Set::new(shown, term, 32 << 20).unwrap()
    }

    /// A set is ordered as it is made, so each picture is found by its
    /// node however the caller gave them.
    #[test]
    fn a_set_finds_each_picture_given_out_of_order() {
        let term = Budget::new(1 << 20).unwrap();
        let inline = |shade: u8| {
            let bgra = [shade, shade, shade, 255].to_vec();
            let charge = term.charge(4).unwrap();
            let fitted = Fitted {
                width: 1,
                height: 1,
                bgra,
            };
            BudgetShared::new(
                &term,
                Inline::new(Resident::new(fitted, charge, 4).unwrap(), (1, 1), None),
            )
            .unwrap()
        };
        let page = Budget::new(64 << 20).unwrap();
        let markup = "<img src=a><p>x<img src=b><p>y<img src=c><img>";
        let tree = crate::tree::Tree::parse(markup, crate::tree::Source::Markup, &page).unwrap();
        let nodes = tree.pictures();
        assert_eq!(nodes.len(), 3);
        let mut shown = BudgetVec::new(&term).unwrap();
        for (at, shade) in [(2, 90), (0, 20), (1, 50)] {
            shown.try_push((nodes[at].index(), inline(shade))).unwrap();
        }
        let set = Set::new(shown, &term, 0).unwrap();
        for (at, shade) in [(0, 20), (1, 50), (2, 90)] {
            let found = set.get(nodes[at]).unwrap();
            assert_eq!(found.picture().bgra[0], shade);
        }
        // A node with no picture laid: the document's own.
        let (bare, _) = tree.document().nodes().next().unwrap();
        assert!(set.get(bare).is_none());
    }

    /// A witness is told what dropped: the picture and each copy fitted
    /// from it; none is told without one.
    #[test]
    fn a_witness_is_told_each_drop() {
        let term = Budget::new(1 << 20).unwrap();
        let (tell, told) = std::sync::mpsc::channel();
        let witness: Witness = Box::leak(Box::new(move |what| {
            let _ = tell.send(what);
        }));
        let bgra = [9, 9, 9, 255].repeat(4);
        let fitted = Fitted {
            width: 2,
            height: 2,
            bgra,
        };
        let charge = term.charge(16).unwrap();
        let inline = Inline::new(
            Resident::new(fitted, charge, 16).unwrap(),
            (2, 2),
            Some(witness),
        );
        let mut work = u64::MAX;
        let copy = inline
            .fit(4, 4, &term, &mut work, &AtomicBool::new(false))
            .unwrap();
        drop(inline);
        assert_eq!(told.try_recv(), Ok("inline"));
        drop(copy);
        assert_eq!(told.try_recv(), Ok("fit"));
        assert!(told.try_recv().is_err());
        drop(Witnessed::none("page"));
    }
}
