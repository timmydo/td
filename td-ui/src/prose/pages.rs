//! The prose faces at sizes other than a raster's own: one family at one
//! size a page, at most `PAGES` pages, held in fixed storage, covering
//! their glyphs in one shared scratch. A size no page holds takes an empty
//! place, else the place of the page drawn from longest ago, whose atlas
//! storage the new face covers into, so after the first `PAGES` no page is
//! allocated again; a page let go is made again when its size is next
//! drawn, its glyphs covered again as they are asked for.
use super::{Styles, PAGES, PAGE_KEYS};
use crate::atlas::{Atlas, Slot, Style};
use crate::face::{Face, Scratch};
use crate::raster::TextFamily;

#[derive(Debug)]
struct Page {
    family: TextFamily,
    pixels: u16,
    /// The lookup that last drew from it.
    used: u64,
    face: Face,
}

#[derive(Debug)]
pub(super) struct Pages {
    held: [Option<Page>; PAGES],
    scratch: Scratch,
    /// Lookups so far: the age each page's `used` is read against.
    clock: u64,
    /// Pages made, each remade one counted again.
    made: u64,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            held: std::array::from_fn(|_| None),
            scratch: Scratch::default(),
            clock: 0,
            made: 0,
        }
    }
}

impl Pages {
    /// `scalar`'s slot in `style` from the face of `family`, whose bytes
    /// are `styles`, at `pixels` per em, with the atlas it is on: the page
    /// holding that face, else one made in an empty place or in the place
    /// of the page drawn from longest ago. None only when the face is
    /// refused at the size, which `Faces::new`'s checks rule out.
    pub(super) fn glyph(
        &mut self,
        styles: &Styles,
        family: TextFamily,
        pixels: u16,
        style: Style,
        scalar: char,
    ) -> Option<(Slot, &Atlas)> {
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        let found = self.held.iter().position(|page| {
            page.as_ref()
                .is_some_and(|page| page.family == family && page.pixels == pixels)
        });
        let index = match found {
            Some(index) => index,
            None => {
                let index = self
                    .held
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, page)| page.as_ref().map_or(0, |page| page.used))
                    .map(|(index, _)| index)?;
                let place = self.held.get_mut(index)?;
                let atlas = place.take().map_or_else(
                    || Atlas::with_keys(PAGE_KEYS),
                    |page| page.face.into_atlas(),
                );
                let face = styles.face_in(atlas, pixels).ok()?;
                self.made = self.made.saturating_add(1);
                *place = Some(Page {
                    family,
                    pixels,
                    used: clock,
                    face,
                });
                index
            }
        };
        let page = self.held.get_mut(index)?.as_mut()?;
        page.used = clock;
        let slot = page.face.glyph_with(&mut self.scratch, style, scalar);
        Some((slot, page.face.atlas()))
    }

    /// The pages held.
    pub(super) fn held(&self) -> usize {
        self.held.iter().flatten().count()
    }

    pub(super) fn made(&self) -> u64 {
        self.made
    }

    /// The most keys a page held resets at.
    pub(super) fn keys(&self) -> Option<usize> {
        self.held
            .iter()
            .flatten()
            .map(|page| page.face.atlas().keys())
            .max()
    }
}
