//! Floats (DESIGN.md §5, "Floats"): each open block formatting
//! context's placed floats, the room they leave a line or a box at a y,
//! and where a float of a size goes; and the items an inline sequence
//! lays apart to place. Coordinates are flow coordinates, before relative
//! offsets, as a block's container is.
use crate::inline::Counters;
use crate::{Error, Side};
use td_alloc::{Budget, BudgetVec};

/// A placed float's margin box, and the lowest bottom of its context's
/// floats queued up to it.
#[derive(Clone, Copy)]
struct Placed {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    side: Side,
    reach: f64,
}

/// A length in px as whole 1/64 px, the unit widths are compared in, so
/// a width measured to fit a room that is a difference of two x's is not
/// refused for an ulp.
pub(crate) fn units(px: f64) -> f64 {
    (px * 64.0).round()
}

/// Who asks for room: a context's lines (and its roots in flow, laid
/// where its lines are), or its floats being placed. Each mostly asks at
/// a y no higher than the last, and forgets the floats above it; one
/// asking higher (past a negative margin or a block's set height) takes
/// back those reaching below where it asks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Lines,
    Places,
}

impl View {
    fn index(self) -> usize {
        match self {
            Self::Lines => 0,
            Self::Places => 1,
        }
    }
}

/// An open formatting context: where its floats start in `queued` and in
/// each view's `started`, the next of `queued` each view has yet to
/// reach and the y it last asked at, the lowest top a later float may
/// take, and its floats' lowest bottoms.
#[derive(Clone, Copy)]
struct Context {
    queued: usize,
    next: [usize; 2],
    started: [usize; 2],
    asked: [f64; 2],
    top: f64,
    left_bottom: f64,
    right_bottom: f64,
}

/// What `clear` asks: below the floats on which sides.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Clear {
    pub left: bool,
    pub right: bool,
}

impl Clear {
    pub fn of(keyword: &str) -> Self {
        match keyword {
            "left" => Self {
                left: true,
                right: false,
            },
            "right" => Self {
                left: false,
                right: true,
            },
            "both" => Self {
                left: true,
                right: true,
            },
            _ => Self::default(),
        }
    }
}

/// The room between a box's content edges and the floats beside it over
/// a height, and the nearest bottom of those that take any.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Band {
    pub left: f64,
    pub right: f64,
    pub below: Option<f64>,
}

impl Band {
    pub fn room(self) -> f64 {
        (self.right - self.left).max(0.0)
    }
}

/// What a nested item is, and what placing it reads.
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    /// A float: the side it goes to and the floats it clears.
    Float { side: Side, clear: Clear },
}

/// A box laid apart where its inline sequence starts (a float, of its
/// `kind`): its margin box `width` (negative where its margins are) by
/// `height` with its top left at `laid`, to be placed and its output,
/// the ranges here, moved there, its lines after the `anchor`th line of
/// its sequence.
#[derive(Clone, Copy)]
pub(crate) struct Item {
    pub width: f64,
    pub height: f64,
    pub kind: Kind,
    pub laid: (f64, f64),
    pub placed: Option<(f64, f64)>,
    pub anchor: usize,
    pub lines: (usize, usize),
    pub boxes: (usize, usize),
    pub markers: (usize, usize),
    /// Its own overflow where it was laid.
    pub bounds: Option<super::Rect>,
}

/// A context's floats are placed in order of their tops, none higher than
/// an earlier one's, and queued so. Each view takes a float from the queue
/// when it asks at or past its top, and lets it go when it asks past its
/// bottom: it visits the floats it has reached and not passed, not every
/// float below it, however far the other view has gone.
pub(crate) struct Floats {
    queued: BudgetVec<Placed>,
    started: [BudgetVec<Placed>; 2],
    contexts: BudgetVec<Context>,
}

impl Floats {
    pub fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            queued: BudgetVec::new(memory)?,
            started: [BudgetVec::new(memory)?, BudgetVec::new(memory)?],
            contexts: BudgetVec::new(memory)?,
        })
    }
    /// A formatting context opens: its floats, until it closes, are the
    /// only ones its lines and boxes see.
    pub fn open(&mut self, counters: &mut Counters) -> Result<(), Error> {
        counters.admit_growth(&mut self.contexts)?;
        let queued = self.queued.len();
        let [lines, places] = &self.started;
        self.contexts.try_push(Context {
            queued,
            next: [queued, queued],
            started: [lines.len(), places.len()],
            asked: [f64::NEG_INFINITY; 2],
            top: f64::NEG_INFINITY,
            left_bottom: f64::NEG_INFINITY,
            right_bottom: f64::NEG_INFINITY,
        })?;
        Ok(())
    }
    /// The innermost context closes: the lowest bottom of its floats, if
    /// it had any, which its root's auto height reaches.
    pub fn close(&mut self) -> Result<Option<f64>, Error> {
        let context = self.contexts.pop().ok_or(Error::Invalid)?;
        self.queued.truncate(context.queued);
        for (started, base) in self.started.iter_mut().zip(context.started) {
            started.truncate(base);
        }
        let bottom = context.left_bottom.max(context.right_bottom);
        Ok(bottom.is_finite().then_some(bottom))
    }
    /// Whether a context is open and holds a float: content asking for
    /// room above where its lines reached may meet one they passed.
    pub fn any(&self) -> bool {
        self.contexts
            .last()
            .is_some_and(|c| c.queued < self.queued.len())
    }
    /// The bottom a box clearing `clear` lies below, if any float it
    /// clears was placed in the innermost context.
    pub fn clearance(&self, clear: Clear) -> Option<f64> {
        let context = self.contexts.last()?;
        let mut bottom = f64::NEG_INFINITY;
        if clear.left {
            bottom = bottom.max(context.left_bottom);
        }
        if clear.right {
            bottom = bottom.max(context.right_bottom);
        }
        bottom.is_finite().then_some(bottom)
    }
    /// The band between `x0` and `x1` that the innermost context's floats
    /// overlapping `y` to `y + height` leave, a float overlapping a zero
    /// height where it holds `y`, as `view` asks: it takes the floats
    /// whose tops it reaches and lets go those wholly above `y`, and
    /// asking above where it last did, takes back those reaching below
    /// `y` it let go.
    pub fn band(
        &mut self,
        (x0, x1): (f64, f64),
        y: f64,
        height: f64,
        view: View,
        counters: &mut Counters,
    ) -> Result<Band, Error> {
        let mut band = Band {
            left: x0,
            right: x1,
            below: None,
        };
        let Some(context) = self.contexts.last_mut() else {
            return Ok(band);
        };
        let which = view.index();
        let next = context.next.get_mut(which).ok_or(Error::Invalid)?;
        let base = *context.started.get(which).ok_or(Error::Invalid)?;
        let started = self.started.get_mut(which).ok_or(Error::Invalid)?;
        let asked = context.asked.get_mut(which).ok_or(Error::Invalid)?;
        if y < *asked {
            counters.step(1)?;
            let queue = self.queued.get(context.queued..).ok_or(Error::Invalid)?;
            *next = context.queued + queue.partition_point(|f| f.reach <= y);
            started.truncate(base);
        }
        *asked = y;
        while let Some(&f) = self.queued.get(*next) {
            if f.top > y && f.top >= y + height {
                break;
            }
            counters.step(1)?;
            *next += 1;
            counters.admit_growth(started)?;
            started.try_push(f)?;
        }
        let mut at = base;
        while let Some(&f) = started.get(at) {
            counters.step(1)?;
            if f.bottom <= y {
                let last = started.len() - 1;
                started.swap(at, last);
                started.pop();
                continue;
            }
            at += 1;
            let overlaps = f.top <= y || f.top < y + height;
            if !overlaps || f.left >= x1 || f.right <= x0 {
                continue;
            }
            match f.side {
                Side::Left => band.left = band.left.max(f.right),
                Side::Right => band.right = band.right.min(f.left),
            }
            band.below = Some(band.below.map_or(f.bottom, |b: f64| b.min(f.bottom)));
        }
        Ok(band)
    }
    /// Where a float `width` by `height` on `side`, clearing `clear`, goes
    /// in its containing block's content edges `x0` to `x1`, met at
    /// `start` (CSS 2.2 §9.5.1): no higher than `start`, an earlier
    /// float's top or what it clears, and from there the first y its
    /// width fits beside the floats it overlaps, or where none does. Its
    /// margin box's top left, and the room at that y.
    pub fn place(
        &mut self,
        (width, height): (f64, f64),
        side: Side,
        clear: Clear,
        (x0, x1): (f64, f64),
        start: f64,
        counters: &mut Counters,
    ) -> Result<(f64, f64, f64), Error> {
        let mut y = start;
        if let Some(context) = self.contexts.last() {
            y = y.max(context.top);
        }
        if let Some(below) = self.clearance(clear) {
            y = y.max(below);
        }
        loop {
            let band = self.band((x0, x1), y, height, View::Places, counters)?;
            match band.below {
                Some(below) if units(width.max(0.0)) > units(band.room()) => y = below,
                _ => {
                    let x = match side {
                        Side::Left => band.left,
                        Side::Right => band.right - width,
                    };
                    return Ok((x, y, band.room()));
                }
            }
        }
    }
    /// A float placed: its margin box at `(x, y)`, `width` by `height`,
    /// none where its width is negative.
    pub fn add(
        &mut self,
        (x, y): (f64, f64),
        (width, height): (f64, f64),
        side: Side,
        counters: &mut Counters,
    ) -> Result<(), Error> {
        let bottom = y + height;
        let context = self.contexts.last_mut().ok_or(Error::Invalid)?;
        let reach = match self.queued.last() {
            Some(last) if self.queued.len() > context.queued => last.reach.max(bottom),
            _ => bottom,
        };
        counters.admit_growth(&mut self.queued)?;
        self.queued.try_push(Placed {
            left: x,
            top: y,
            right: x + width.max(0.0),
            bottom,
            side,
            reach,
        })?;
        context.top = context.top.max(y);
        match side {
            Side::Left => context.left_bottom = context.left_bottom.max(bottom),
            Side::Right => context.right_bottom = context.right_bottom.max(bottom),
        }
        Ok(())
    }
}
