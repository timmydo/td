//! A device-bound installation's recovery key on its way to completion
//! (td-install/INSTALLER.md "User flow and authority" and "Device-bound
//! records"): the key the service sends once, held in memory only and
//! zeroed on drop; its display, beside a QR code of its 48 digits a phone
//! can keep; and the step that asks the person to say they kept it. Nobody
//! types a new key; reinstall accepts an existing key in a visible entry. This file alone names the disk protector's crate,
//! and only that codec (tests/confinement.rs). Nothing here prints, logs or
//! copies the digits anywhere but the page.

use std::fmt;

use td_protector::recovery::{RecoveryKey, RecoveryText, PASSPHRASE_LEN};
use td_ui::chrome::{Status, ROW};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::qr::{self, Code};

const INSET: usize = CELL_WIDTH;
const ASKING_FOOTER: &str = "Recovery key \u{b7} step 6 of 6";
const SHOWN_FOOTER: &str = "Recovery key \u{b7} step 6 of 6 \u{b7} Return once it is kept";
const KEPT_FOOTER: &str = "Recovery key \u{b7} step 6 of 6 \u{b7} Escape shows the key again";
/// The key's row.
const KEY_ROW: usize = 6;
/// The rows beside the code, from the first under the heading to the last
/// above the rows that run the full width, are at most this many columns.
const SIDE_COLUMNS: usize = 56;
const CODE_TOP_ROW: usize = 3;
const CODE_END_ROW: usize = 13;
/// The smallest module side, in unscaled pixels, a phone reads at arm's
/// length off a laptop panel.
const MIN_MODULE: usize = 4;
/// A QR code's own colours: a reader needs dark on light, so no theme
/// maps them.
const DARK: u32 = 0x00_0000;
const LIGHT: u32 = 0xff_ffff;
/// Where a scanned key belongs, said beside the code.
const SCAN_ADVICE: &str = "Keep a scan in a password manager, not a camera app.";
/// The row that says what closing the window now does, and the lowest
/// row any step draws.
const LAST_ROW: usize = 14;
/// Closing the window ends the connection, and the service withdraws the
/// installation it can no longer finish (INSTALLER.md "Device-bound
/// records").
pub const CLOSING: &str =
    "Closing this window now withdraws the installation; the disk will not start.";

/// Validate a typed existing key without retaining another copy.
pub(crate) fn valid_entry(entry: &[u8]) -> bool {
    RecoveryKey::parse(entry).is_ok()
}

/// A recovery key the installer service sent: its 48 digits, every group's
/// value and check digit admitted, in one allocation zeroed on drop. It is
/// neither `Clone` nor printable; `Debug` names no digit.
pub struct Key(RecoveryKey);

impl Key {
    /// The 48 digits as the wire carries them, without separators. A group
    /// whose value or check digit fails refuses the whole key: a key that
    /// would not open the disk is not shown.
    pub fn from_digits(digits: &[u8]) -> Result<Self, String> {
        if digits.len() != PASSPHRASE_LEN {
            return Err("the installer service's recovery key is not 48 digits".into());
        }
        RecoveryKey::parse(digits)
            .map(Self)
            .map_err(|error| format!("the installer service's recovery key: {error}"))
    }

    /// Lends the 48 digits, without separators, to `use_digits`; the copy
    /// lent is zeroed when it returns.
    pub fn with_digits<T>(&self, use_digits: impl FnOnce(&[u8]) -> T) -> T {
        let digits = self.0.passphrase();
        use_digits(digits.expose())
    }

    /// The display form, eight groups joined by hyphens, zeroed on drop.
    pub fn display(&self) -> Shown {
        Shown(self.0.display())
    }

    /// The QR code of the 48 digits alone, what cryptsetup takes; zeroed
    /// on drop.
    pub fn code(&self) -> Option<Code> {
        self.with_digits(qr::numeric)
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Key(..)")
    }
}

/// Compares every digit.
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.0.matches(&other.0)
    }
}

impl Eq for Key {}

/// A key's display form, drawn from the borrow and zeroed on drop.
pub struct Shown(RecoveryText);

impl Shown {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// Which recovery-key view to paint.
pub enum Step<'a> {
    Unlock {
        entry: &'a str,
        notice: Option<&'a str>,
    },
    /// The key is asked for and not yet here.
    Asking,
    /// The key, in its display form, and its QR code.
    Shown {
        key: &'a str,
        code: Option<&'a Code>,
    },
    /// The key is not shown again: Return says it was kept, Escape shows
    /// it again until then. `notice` says what became of the
    /// acknowledgement once it was said.
    Kept { notice: Option<&'a str> },
}

/// One recovery-key view: a pure rendering of the key the window holds,
/// or of the question whether it was kept. It writes nothing anywhere.
pub struct RecoveryPage<'a> {
    surface: Surface,
    step: Step<'a>,
    /// The code's square, quiet zone included, and its module's side.
    code: (i64, i64, usize),
    footer: Status,
}

impl<'a> RecoveryPage<'a> {
    pub fn new(surface: Surface, step: Step<'a>) -> Option<Self> {
        crate::supported_page(surface)?;
        let code = code_place(surface)?;
        let footer = Status::new(surface);
        let scale = surface.scale.value();
        if ((LAST_ROW * ROW + CELL_HEIGHT) * scale) as i64 > footer.rect().y {
            return None;
        }
        Some(Self {
            surface,
            step,
            code,
            footer,
        })
    }
}

/// Where the code goes: flush right, beside the side rows and above the
/// full-width ones, at the largest whole module that fits, or `None` when
/// that is smaller than a phone reads.
fn code_place(surface: Surface) -> Option<(i64, i64, usize)> {
    let scale = surface.scale.value();
    let side = qr::SIZE + 2 * qr::QUIET_ZONE;
    let left = (INSET + (SIDE_COLUMNS + 1) * CELL_WIDTH) * scale;
    let right = surface.width.checked_sub(INSET * scale)?;
    let top = CODE_TOP_ROW * ROW * scale;
    let end = CODE_END_ROW * ROW * scale;
    let module = (right.checked_sub(left)? / side).min(end.checked_sub(top)? / side);
    if module < MIN_MODULE * scale {
        return None;
    }
    let x = right.checked_sub(side * module)?;
    Some((i64::try_from(x).ok()?, i64::try_from(top).ok()?, module))
}

impl Composition for RecoveryPage<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        fill(self.surface.bounds(), CHROME, damage, sink);
        let surface = self.surface;
        let footer = match &self.step {
            Step::Unlock { entry, notice } => {
                row(surface, 1, "Unlock existing installation", damage, sink);
                row(
                    surface,
                    3,
                    "Type the saved recovery key. Hyphens between groups are optional.",
                    damage,
                    sink,
                );
                row(surface, 5, entry, damage, sink);
                row(
                    surface,
                    7,
                    "Unlock inspects your installation without changing the disk.",
                    damage,
                    sink,
                );
                row(
                    surface,
                    8,
                    "You will review and confirm reinstall separately.",
                    damage,
                    sink,
                );
                row(
                    surface,
                    10,
                    "Camera scanning is not available yet.",
                    damage,
                    sink,
                );
                if let Some(notice) = notice {
                    row(surface, 12, notice, damage, sink);
                }
                "Return to unlock · Escape to cancel"
            }
            Step::Asking => {
                row(surface, 1, "Recovery key", damage, sink);
                row(
                    surface,
                    4,
                    "Asking the installer service for the recovery key\u{2026}",
                    damage,
                    sink,
                );
                row(
                    surface,
                    6,
                    "The system is written and verified. It completes once its recovery",
                    damage,
                    sink,
                );
                row(surface, 7, "key is shown and kept.", damage, sink);
                row(surface, 9, CLOSING, damage, sink);
                ASKING_FOOTER
            }
            Step::Shown { key, code } => {
                row(surface, 1, "Recovery key", damage, sink);
                let lead: &[&str] = match code {
                    Some(code) => {
                        emit_code(code, self.code, damage, sink);
                        &[
                            "Write down this recovery key, or scan the code with",
                            "a phone: it reads as the same 48 digits.",
                        ]
                    }
                    None => &["Write down this recovery key."],
                };
                for (index, text) in lead.iter().enumerate() {
                    side_row(surface, CODE_TOP_ROW + index, text, damage, sink);
                }
                side_row(surface, KEY_ROW, key, damage, sink);
                for (index, text) in [
                    "It is shown only now; the installer keeps no copy.",
                    "It is the only way to open this disk if the computer's",
                    "TPM, its firmware measurements or its boot chain",
                    "change. Keep it safe, away from the computer.",
                ]
                .iter()
                .enumerate()
                {
                    side_row(surface, KEY_ROW + 2 + index, text, damage, sink);
                }
                // A camera app may send what it scans to its maker.
                if code.is_some() {
                    side_row(surface, KEY_ROW + 6, SCAN_ADVICE, damage, sink);
                }
                row(
                    surface,
                    CODE_END_ROW,
                    "Press Return once it is kept. The installation then completes.",
                    damage,
                    sink,
                );
                row(surface, LAST_ROW, CLOSING, damage, sink);
                SHOWN_FOOTER
            }
            Step::Kept { notice } => {
                row(surface, 1, "Keep the recovery key", damage, sink);
                for (index, text) in [
                    "Once you go on, the recovery key is not shown again, and nothing on",
                    "this computer keeps a copy of it.",
                ]
                .iter()
                .enumerate()
                {
                    row(surface, 3 + index, text, damage, sink);
                }
                row(
                    surface,
                    6,
                    "Press Return if it is written down or scanned. The installation",
                    damage,
                    sink,
                );
                row(surface, 7, "then completes.", damage, sink);
                row(surface, 12, CLOSING, damage, sink);
                // Once said kept, it is not shown again from here.
                if let Some(notice) = notice {
                    row(surface, 10, notice, damage, sink);
                    ASKING_FOOTER
                } else {
                    row(
                        surface,
                        8,
                        "Press Escape to see the key again.",
                        damage,
                        sink,
                    );
                    KEPT_FOOTER
                }
            }
        };
        self.footer.emit(footer.chars(), damage, sink);
    }
}

/// The code on its light square, a dark run of modules per fill.
fn emit_code(
    code: &Code,
    (x, y, module): (i64, i64, usize),
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let side = (qr::SIZE + 2 * qr::QUIET_ZONE) * module;
    let at = |offset: usize| i64::try_from(offset).unwrap_or(i64::MAX);
    let square = Rect {
        x,
        y,
        width: u32::try_from(side).unwrap_or(0),
        height: u32::try_from(side).unwrap_or(0),
    };
    fill(square, LIGHT, damage, sink);
    let origin = qr::QUIET_ZONE * module;
    for line in 0..qr::SIZE {
        let mut column = 0;
        while column < qr::SIZE {
            if !code.dark(column, line) {
                column += 1;
                continue;
            }
            let start = column;
            while column < qr::SIZE && code.dark(column, line) {
                column += 1;
            }
            let run = Rect {
                x: x.saturating_add(at(origin + start * module)),
                y: y.saturating_add(at(origin + line * module)),
                width: u32::try_from((column - start) * module).unwrap_or(0),
                height: u32::try_from(module).unwrap_or(0),
            };
            fill(run, DARK, damage, sink);
        }
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

/// One line of text on a row beside the code, cut to `SIDE_COLUMNS` so
/// that no text ever runs under the symbol.
fn side_row(surface: Surface, index: usize, text: &str, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let cut = text
        .char_indices()
        .nth(SIDE_COLUMNS)
        .and_then(|(end, _)| text.get(..end))
        .unwrap_or(text);
    row(surface, index, cut, damage, sink);
}

/// One line of text on row `index`, cut to the width.
fn row(surface: Surface, index: usize, text: &str, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    let scale = surface.scale;
    let inset = (INSET * scale.value()) as i64;
    let y = (index * ROW * scale.value()) as i64;
    let rect = Rect {
        x: inset,
        y,
        width: surface.width.saturating_sub(2 * INSET * scale.value()) as u32,
        height: (CELL_HEIGHT * scale.value()) as u32,
    };
    let columns = (rect.width as usize) / (CELL_WIDTH * scale.value());
    text_run(
        scale,
        text.chars().take(columns),
        (inset, y),
        rect,
        GlyphStyle::medium(INK, CHROME),
        damage,
        sink,
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use td_ui::raster::Scale;

    const KEY: &[u8] = b"000013005150010290015439020571025716030859035998";
    const SHOWN: &str = "000013-005150-010290-015439-020571-025716-030859-035998";

    fn surface(width: usize, height: usize) -> Surface {
        Surface::new(width, height, Scale::new(1).unwrap()).unwrap()
    }

    fn glyphs(view: &dyn Composition, screen: Surface) -> String {
        let mut text = String::new();
        view.emit(screen.bounds(), &mut |draw| {
            if let Primitive::Glyph { scalar, .. } = draw.primitive {
                text.push(scalar);
            }
        });
        text
    }

    #[test]
    fn unlock_shows_partial_and_complete_entries_at_each_supported_scale() {
        for screen in [
            surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT),
            Surface::new(1504, 960, Scale::new(2).unwrap()).unwrap(),
        ] {
            for entry in ["000013-005", SHOWN, std::str::from_utf8(KEY).unwrap()] {
                let page = RecoveryPage::new(
                    screen,
                    Step::Unlock {
                        entry,
                        notice: None,
                    },
                )
                .unwrap();
                let text = glyphs(&page, screen);
                assert!(text.contains(entry), "the typed entry must be readable");
                assert!(!text.contains('*'), "the entry must not be masked");
                assert!(text.contains("Return to unlock"));
            }
        }
    }

    #[test]
    fn a_key_is_admitted_only_whole_with_every_group_checked() {
        let key = Key::from_digits(KEY).unwrap();
        assert_eq!(key.display().as_str(), SHOWN);
        assert_eq!(key.with_digits(<[u8]>::to_vec), KEY);
        assert_eq!(format!("{key:?}"), "Key(..)");
        assert_eq!(key, Key::from_digits(KEY).unwrap());
        assert!(Key::from_digits(KEY.get(..47).unwrap()).is_err());
        // The display form is the page's; the wire carries 48 digits.
        assert!(Key::from_digits(SHOWN.as_bytes()).is_err());
        let mut wrong = KEY.to_vec();
        *wrong.get_mut(5).unwrap() = b'4';
        let refused = Key::from_digits(&wrong).unwrap_err();
        assert!(refused.contains("group 1"), "{refused}");
        assert!(!refused.contains("000013"), "{refused}");
    }

    /// The code drawn is the digits' own, module for module, dark on a
    /// light square with its quiet zone, in colours no theme changes, and
    /// no text runs into it.
    #[test]
    fn the_shown_key_draws_its_code_beside_the_text() {
        let key = Key::from_digits(KEY).unwrap();
        let code = key.code().unwrap();
        let expected = qr::numeric(KEY).unwrap();
        for screen in [
            surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT),
            surface(1280, 800),
            Surface::new(1504, 960, Scale::new(2).unwrap()).unwrap(),
        ] {
            let page = RecoveryPage::new(
                screen,
                Step::Shown {
                    key: SHOWN,
                    code: Some(&code),
                },
            )
            .unwrap();
            let (x, y, module) = page.code;
            assert!(module >= MIN_MODULE * screen.scale.value());
            let side = (qr::SIZE + 2 * qr::QUIET_ZONE) * module;
            let side = i64::try_from(side).unwrap();
            assert!(x + side <= i64::try_from(screen.width).unwrap());
            let mut dark = vec![false; qr::SIZE * qr::SIZE];
            let mut square = false;
            page.emit(screen.bounds(), &mut |draw| match draw.primitive {
                Primitive::Fill { rect, color } if color == LIGHT => {
                    assert_eq!((rect.x, rect.y, i64::from(rect.width)), (x, y, side));
                    square = true;
                }
                Primitive::Fill { rect, color } if color == DARK => {
                    let module = i64::try_from(module).unwrap();
                    let quiet = i64::try_from(qr::QUIET_ZONE).unwrap() * module;
                    let column = (rect.x - x - quiet) / module;
                    let line = (rect.y - y - quiet) / module;
                    for offset in 0..i64::from(rect.width) / module {
                        let at = usize::try_from(line).unwrap() * qr::SIZE
                            + usize::try_from(column + offset).unwrap();
                        *dark.get_mut(at).unwrap() = true;
                    }
                }
                Primitive::Glyph {
                    x: left, y: top, ..
                } => {
                    let beside = top >= y && top < y + side;
                    assert!(!beside || left + 8 <= x, "a glyph at ({left}, {top})");
                }
                _ => {}
            });
            assert!(square);
            for line in 0..qr::SIZE {
                for column in 0..qr::SIZE {
                    assert_eq!(
                        *dark.get(line * qr::SIZE + column).unwrap(),
                        expected.dark(column, line),
                        "({column}, {line})"
                    );
                }
            }
        }
        for theme in td_ui::theme::THEMES {
            assert_eq!(theme.map(DARK), DARK);
            assert_eq!(theme.map(LIGHT), LIGHT);
        }
        // Without a code the key is still shown, with no word of scanning.
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let plain = RecoveryPage::new(
            screen,
            Step::Shown {
                key: SHOWN,
                code: None,
            },
        )
        .unwrap();
        let text = glyphs(&plain, screen);
        assert!(text.contains(SHOWN) && !text.contains("scan"));
        plain.emit(screen.bounds(), &mut |draw| {
            assert!(!matches!(
                draw.primitive,
                Primitive::Fill { color, .. } if color == DARK || color == LIGHT
            ));
        });
    }

    #[test]
    fn each_step_says_what_the_key_is_for_and_fits_the_tile() {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let key = Key::from_digits(KEY).unwrap();
        let code = key.code().unwrap();
        let asking = glyphs(&RecoveryPage::new(screen, Step::Asking).unwrap(), screen);
        assert!(asking.contains("Asking the installer service for the recovery key"));
        assert!(asking.contains(ASKING_FOOTER) && asking.contains(CLOSING));
        assert!(!asking.contains("Write down"));
        let shown = glyphs(
            &RecoveryPage::new(
                screen,
                Step::Shown {
                    key: SHOWN,
                    code: Some(&code),
                },
            )
            .unwrap(),
            screen,
        );
        for said in [
            SHOWN,
            "Write down this recovery key, or scan the code",
            "the same 48 digits",
            SCAN_ADVICE,
            "shown only now",
            "keeps no copy",
            "only way to open this disk",
            "TPM",
            "firmware measurements or its boot chain",
            "Press Return once it is kept",
            CLOSING,
            SHOWN_FOOTER,
        ] {
            assert!(shown.contains(said), "{said}");
        }
        let kept = glyphs(
            &RecoveryPage::new(screen, Step::Kept { notice: None }).unwrap(),
            screen,
        );
        for said in [
            "Keep the recovery key",
            "not shown again",
            "Press Return if it is written down or scanned",
            "Press Escape to see the key again",
            CLOSING,
            KEPT_FOOTER,
        ] {
            assert!(kept.contains(said), "{said}");
        }
        // Once said kept, it is with the service and Escape is not offered.
        let telling = glyphs(
            &RecoveryPage::new(
                screen,
                Step::Kept {
                    notice: Some("Telling the installer service"),
                },
            )
            .unwrap(),
            screen,
        );
        assert!(telling.contains("Telling the installer service") && telling.contains(CLOSING));
        assert!(!telling.contains("Escape"));
        // The key itself is shown only on its own step, and nothing asks
        // for any of it to be typed.
        assert!(!kept.contains(SHOWN) && !asking.contains(SHOWN));
        for text in [&asking, &shown, &kept] {
            assert!(!text.contains("type") && !text.contains("Type"), "{text}");
        }
        for step in [
            Step::Asking,
            Step::Shown {
                key: SHOWN,
                code: None,
            },
        ] {
            assert!(RecoveryPage::new(surface(751, 480), step).is_none());
        }
        assert!(RecoveryPage::new(surface(752, 479), Step::Asking).is_none());
        let scaled = Surface::new(1504, 960, Scale::new(2).unwrap()).unwrap();
        assert!(RecoveryPage::new(scaled, Step::Kept { notice: None }).is_some());
        assert!(CLOSING.chars().count() <= (crate::MIN_PAGE_WIDTH - 2 * INSET) / CELL_WIDTH);
    }
}
