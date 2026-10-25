//! The step buttons: Back and Next at the right of every step page's
//! footer row, so the wizard can be walked with the pointer as with
//! Escape and Return. Welcome labels the two slots Reinstall and Fresh
//! install, with Reinstall following its R shortcut. A button is a view of the page's own keys: it holds
//! no state and authorizes nothing the key would not.
//!
//! The two are fixed slots of one width, Next always rightmost and Back
//! always left of it, a lone Back included: a button never sits where the
//! next page puts the other kind, so a double click cannot take a step
//! and then its opposite.

use td_ui::chrome::{Button, Status, BUTTON_MARGIN};
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::CELL_WIDTH;

/// A step slot; the page maps it to its action (R or Return on welcome).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Go {
    Back,
    Next,
}

/// A page's step buttons: Back's label, and Next's with whether it can be
/// pressed now. A page with neither shows none.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Steps {
    pub back: Option<&'static str>,
    pub next: Option<(&'static str, bool)>,
}

/// Welcome's two installation choices, which the live window and the still
/// preview both paint.
pub const WELCOME: Steps = Steps {
    back: Some("Reinstall"),
    next: Some(("Fresh install", true)),
};

/// The cells a label may take, the longest's ("Fresh install"); a slot is a
/// cell wider each side.
const LABEL_CELLS: usize = 13;
const SLOT_CELLS: usize = LABEL_CELLS + 2;

/// The two slots laid out on a surface's footer row, flush right.
pub struct Nav {
    back: Option<(&'static str, Button)>,
    next: Option<(&'static str, bool, Button)>,
}

impl Nav {
    /// The buttons for `steps`; `None` when the page has none, a label is
    /// longer than a slot holds, or the row cannot hold the slots.
    pub fn new(surface: Surface, steps: Steps) -> Option<Self> {
        if steps.back.is_none() && steps.next.is_none() {
            return None;
        }
        let mut labels = steps
            .back
            .into_iter()
            .chain(steps.next.map(|(label, _)| label));
        if labels.any(|label| label.chars().count() > LABEL_CELLS) {
            return None;
        }
        let s = surface.scale.value();
        let cell = CELL_WIDTH.checked_mul(s)?;
        let slot = SLOT_CELLS.checked_mul(cell)?;
        let footer = Status::new(surface).rect();
        let margin = BUTTON_MARGIN.checked_mul(s)?;
        let height = (footer.height as usize).checked_sub(2 * margin)?;
        // A cell after Next, and a cell between the slots.
        let next_x = surface.width.checked_sub(cell + slot)?;
        let back_x = next_x.checked_sub(cell + slot)?;
        let at = |x: usize| {
            Button::new(
                surface,
                Rect {
                    x: i64::try_from(x).ok()?,
                    y: footer.y + margin as i64,
                    width: u32::try_from(slot).ok()?,
                    height: u32::try_from(height).ok()?,
                },
            )
        };
        let back = match steps.back {
            Some(label) => Some((label, at(back_x)?)),
            None => None,
        };
        let next = match steps.next {
            Some((label, enabled)) => Some((label, enabled, at(next_x)?)),
            None => None,
        };
        Some(Self { back, next })
    }

    /// Where the slots begin on `surface`'s footer row, Back's left edge:
    /// a page's footer text must end before it.
    pub fn reach(surface: Surface) -> Option<i64> {
        let both = Self::new(
            surface,
            Steps {
                back: Some(""),
                next: Some(("", true)),
            },
        )?;
        both.back.map(|(_, button)| button.rect().x)
    }

    /// The enabled button under the point.
    pub fn hit(&self, x: i64, y: i64) -> Option<Go> {
        if self.back.is_some_and(|(_, button)| button.hit(x, y)) {
            return Some(Go::Back);
        }
        self.next
            .filter(|(_, enabled, button)| *enabled && button.hit(x, y))
            .map(|_| Go::Next)
    }

    /// Paints the buttons over the footer row's right end, each label
    /// centred in its slot, and an enabled Next in the selection's colour
    /// as the step Return takes.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if let Some((label, button)) = self.back {
            button.emit(&centred(label), false, true, damage, sink);
        }
        if let Some((label, enabled, button)) = self.next {
            button.emit(&centred(label), enabled, enabled, damage, sink);
        }
    }
}

/// `label` padded to start where it is centred in a slot's text cells.
fn centred(label: &str) -> String {
    let pad = LABEL_CELLS.saturating_sub(label.chars().count()) / 2;
    format!("{:pad$}{label}", "")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use td_ui::chrome::DISABLED as DISABLED_INK;
    use td_ui::raster::{Primitive, Scale, INK, PAPER, SELECTED};

    fn surface(scale: u8) -> Surface {
        let s = usize::from(scale);
        Surface::new(800 * s, 600 * s, Scale::new(scale).unwrap()).unwrap()
    }

    const BOTH: Steps = Steps {
        back: Some("Back"),
        next: Some(("Next", true)),
    };

    #[test]
    fn the_slots_sit_flush_right_on_the_footer_row_at_every_scale() {
        for scale in 1..=4u8 {
            let s = i64::from(scale);
            let surface = surface(scale);
            let nav = Nav::new(surface, BOTH).unwrap();
            let footer = Status::new(surface).rect();
            let back = nav.back.unwrap().1.rect();
            let next = nav.next.unwrap().2.rect();
            // Next ends a cell from the right; Back a cell before it; both
            // fifteen cells wide, inside the row's margins.
            assert_eq!(next.x + i64::from(next.width), 800 * s - 8 * s);
            assert_eq!(back.x + i64::from(back.width), next.x - 8 * s);
            assert_eq!(
                (back.width, next.width),
                ((120 * s) as u32, (120 * s) as u32)
            );
            assert_eq!(next.y, footer.y + 2 * s);
            assert_eq!(next.y + i64::from(next.height), footer.y + 22 * s);
            assert_eq!(nav.hit(back.x + s, back.y + 4 * s), Some(Go::Back));
            assert_eq!(nav.hit(next.x + s, next.y + 4 * s), Some(Go::Next));
            // The gap between, and the row's top border, are no button.
            assert_eq!(nav.hit(next.x - 4 * s, next.y + 4 * s), None);
            assert_eq!(nav.hit(next.x + s, footer.y), None);
        }
    }

    #[test]
    fn a_lone_back_or_withdraw_keeps_the_back_slot_never_next_s() {
        let surface = surface(1);
        let both = Nav::new(surface, BOTH).unwrap();
        let next = both.next.unwrap().2.rect();
        for label in ["Back", "Withdraw"] {
            let lone = Nav::new(
                surface,
                Steps {
                    back: Some(label),
                    next: None,
                },
            )
            .unwrap();
            assert_eq!(lone.back.unwrap().1.rect(), both.back.unwrap().1.rect());
            assert_eq!(lone.hit(next.x + 4, next.y + 4), None);
        }
        let lone = Nav::new(surface, WELCOME).unwrap();
        assert_eq!(lone.next.unwrap().2.rect(), next);
        // Labels are centred within the shared slots.
        assert_eq!(centred("Back"), "    Back");
        assert_eq!(centred("Withdraw"), "  Withdraw");
        assert_eq!(centred("Install"), "   Install");
        let long = Steps {
            back: Some("Label too long"),
            next: None,
        };
        assert!(Nav::new(surface, long).is_none());
        assert!(Nav::new(surface, Steps::default()).is_none());
    }

    #[test]
    fn a_disabled_next_paints_disabled_and_an_enabled_one_as_the_default() {
        let surface = surface(1);
        let inks = |steps| {
            let nav = Nav::new(surface, steps).unwrap();
            let next = nav.next.unwrap().2.rect();
            let mut inks = Vec::new();
            nav.emit(surface.bounds(), &mut |draw| {
                if let Primitive::Glyph { x, style, .. } = draw.primitive {
                    inks.push((x >= next.x, style.ink, style.background));
                }
            });
            (nav, next, inks)
        };
        let (nav, next, disabled) = inks(Steps {
            back: Some("Back"),
            next: Some(("Next", false)),
        });
        assert_eq!(nav.hit(next.x + 1, next.y + 4), None);
        assert!(disabled
            .iter()
            .all(|(on_next, ink, _)| !*on_next || *ink == DISABLED_INK));
        assert!(disabled
            .iter()
            .any(|(on_next, ink, _)| !*on_next && *ink == INK));
        let (_, _, enabled) = inks(BOTH);
        assert!(enabled
            .iter()
            .all(|(on_next, ink, ground)| !*on_next || (*ink == PAPER && *ground == SELECTED)));
        // Damage off the footer row paints nothing.
        let mut any = false;
        Nav::new(surface, BOTH).unwrap().emit(
            Rect {
                x: 0,
                y: 0,
                width: 800,
                height: 500,
            },
            &mut |_| any = true,
        );
        assert!(!any);
    }
}
