//! The glyph atlas: one 8-bit coverage page of fixed extent, shelf-packed,
//! with its entries keyed by style and scalar. A glyph is covered into the
//! page once and blended from it after; when the page or its key budget is
//! full it resets whole under a new epoch. The page reports the band of
//! rows written since it was last taken, the sub-image a GPU backend
//! uploads. Nothing here reads the environment, a clock, a descriptor or
//! the filesystem.

use std::collections::BTreeMap;

use crate::coverage::Mask;

pub const PAGE_WIDTH: usize = 1024;
pub const PAGE_HEIGHT: usize = 1024;
/// Keys held at once, placed and missing together: missing scalars take
/// no page space, so they need a bound of their own.
pub const MAX_KEYS: usize = 8192;
/// Blank pixels kept between neighbours, so a GPU sampler's filtering
/// never reads into the next glyph.
const GUTTER: usize = 1;

/// Which of a face's styles a glyph is drawn from.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Style {
    Regular,
    Bold,
}

/// A glyph's rectangle on the page and its bearing: its left column
/// `left` pixels right of the pen and its top row `top` above the
/// baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub left: i32,
    pub top: i32,
}

/// What the atlas holds for a key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Slot {
    /// Coverage on the page.
    Placed(Entry),
    /// The face has the scalar but its glyph covers nothing (a space).
    Blank,
    /// The face lacks the scalar; the caller falls back.
    Missing,
}

#[derive(Clone, Copy, Debug)]
struct Shelf {
    y: usize,
    height: usize,
    next: usize,
}

#[derive(Clone, Debug)]
pub struct Atlas {
    page: Vec<u8>,
    shelves: Vec<Shelf>,
    slots: BTreeMap<(Style, char), Slot>,
    epoch: u64,
    dirty: Option<(usize, usize)>,
}

impl Default for Atlas {
    fn default() -> Self {
        Self::new()
    }
}

impl Atlas {
    pub fn new() -> Self {
        Self {
            page: vec![0; PAGE_WIDTH * PAGE_HEIGHT],
            shelves: Vec::new(),
            slots: BTreeMap::new(),
            epoch: 0,
            dirty: None,
        }
    }

    /// The page, `PAGE_WIDTH` bytes a row.
    pub fn page(&self) -> &[u8] {
        &self.page
    }

    /// Advances on every reset; an entry is valid only in the epoch that
    /// placed it.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn get(&self, style: Style, scalar: char) -> Option<Slot> {
        self.slots.get(&(style, scalar)).copied()
    }

    /// The rows written since the band was last taken, as a start and an
    /// end row, and none when nothing was.
    pub fn take_dirty(&mut self) -> Option<(usize, usize)> {
        self.dirty.take()
    }

    /// Forgets every entry and starts a new epoch. The page's bytes stay
    /// until overwritten; no entry names them any more.
    pub fn reset(&mut self) {
        self.shelves.clear();
        self.slots.clear();
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Records a slot that takes no page space, resetting first when the
    /// key budget is spent.
    pub fn record(&mut self, style: Style, scalar: char, slot: Slot) {
        if self.slots.len() >= MAX_KEYS && !self.slots.contains_key(&(style, scalar)) {
            self.reset();
        }
        self.slots.insert((style, scalar), slot);
    }

    /// Copies `mask` onto the page with its gutter zeroed and records it,
    /// resetting the page first when it is full. A key already held keeps
    /// its slot. An empty mask is recorded blank; one that with its gutter
    /// would not fit the page, or whose alpha is not its width by height,
    /// is recorded missing, never placed.
    pub fn place(&mut self, style: Style, scalar: char, mask: &Mask) -> Slot {
        if let Some(slot) = self.get(style, scalar) {
            return slot;
        }
        if mask.width == 0 || mask.height == 0 {
            self.record(style, scalar, Slot::Blank);
            return Slot::Blank;
        }
        if mask.width + GUTTER > PAGE_WIDTH
            || mask.height + GUTTER > PAGE_HEIGHT
            || mask.width.checked_mul(mask.height) != Some(mask.alpha.len())
        {
            self.record(style, scalar, Slot::Missing);
            return Slot::Missing;
        }
        if self.slots.len() >= MAX_KEYS {
            self.reset();
        }
        let (x, y) = match self.allocate(mask.width, mask.height) {
            Some(at) => at,
            None => {
                self.reset();
                // An empty page fits any mask that passed the check above.
                match self.allocate(mask.width, mask.height) {
                    Some(at) => at,
                    None => return Slot::Missing,
                }
            }
        };
        // The rows and the gutter column right of them, then the gutter
        // row below: an earlier epoch's bytes there would bleed into a
        // sampler that filters across the entry's edge.
        for (row, source) in mask.alpha.chunks(mask.width).enumerate() {
            let start = (y + row) * PAGE_WIDTH + x;
            if let Some(target) = self.page.get_mut(start..start + mask.width) {
                target.copy_from_slice(source);
            }
            if let Some(gutter) = self.page.get_mut(start + mask.width) {
                *gutter = 0;
            }
        }
        let below = (y + mask.height) * PAGE_WIDTH + x;
        if let Some(gutter) = self.page.get_mut(below..below + mask.width + GUTTER) {
            gutter.fill(0);
        }
        let end = y + mask.height + GUTTER;
        self.dirty = Some(match self.dirty {
            Some((first, last)) => (first.min(y), last.max(end)),
            None => (y, end),
        });
        let slot = Slot::Placed(Entry {
            x,
            y,
            width: mask.width,
            height: mask.height,
            left: mask.left,
            top: mask.top,
        });
        self.slots.insert((style, scalar), slot);
        slot
    }

    /// The first shelf with room and a height within a quarter over the
    /// mask's, else a new shelf of the mask's height below the last.
    fn allocate(&mut self, width: usize, height: usize) -> Option<(usize, usize)> {
        let (width, height) = (width + GUTTER, height + GUTTER);
        for shelf in &mut self.shelves {
            if shelf.height >= height
                && shelf.height <= height + height / 4
                && PAGE_WIDTH - shelf.next >= width
            {
                let at = (shelf.next, shelf.y);
                shelf.next += width;
                return Some(at);
            }
        }
        let y = self
            .shelves
            .last()
            .map_or(0, |shelf| shelf.y + shelf.height);
        if PAGE_HEIGHT.checked_sub(y)? < height {
            return None;
        }
        self.shelves.push(Shelf {
            y,
            height,
            next: width,
        });
        Some((0, y))
    }
}
