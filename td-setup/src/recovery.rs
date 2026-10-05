//! A device-bound installation's recovery key on its way to completion
//! (td-install/INSTALLER.md "User flow and authority" and "Device-bound
//! records"): the key the service sends once, held in memory only and
//! zeroed on drop; its display; and the field it is typed back in, whose
//! groups td-protector's recovery-key codec checks as they are typed. This
//! file alone names the disk protector's crate, and only that codec
//! (tests/confinement.rs). Nothing here prints, logs or copies the digits
//! anywhere but the page.

use std::fmt;

use td_protector::recovery::{
    EntryError, RecoveryKey, RecoveryText, MAX_ENTRY_LEN, PASSPHRASE_LEN,
};
use td_ui::chrome::{Status, TextEntry, ROW};
use td_ui::entry_model::{Action, EntryModel, Motion, Outcome};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK,
};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

const INSET: usize = CELL_WIDTH;
const ASKING_FOOTER: &str = "Recovery key \u{b7} step 6 of 6";
const SHOWN_FOOTER: &str = "Recovery key \u{b7} step 6 of 6 \u{b7} Return to type it back";
const TYPE_BACK_FOOTER: &str =
    "Recovery key \u{b7} step 6 of 6 \u{b7} Return to confirm \u{b7} Escape to show it again";
/// The type-back field's row, and the rows under it.
const FIELD_ROW: usize = 5;
const FEEDBACK_ROW: usize = 7;
const NOTICE_ROW: usize = 8;
/// The row that says what closing the window now does, and the lowest
/// row any step draws.
const LAST_ROW: usize = 14;
/// Closing the window ends the connection, and the service withdraws the
/// installation it can no longer finish (INSTALLER.md "Device-bound
/// records").
pub const CLOSING: &str =
    "Closing this window now withdraws the installation; the disk will not start.";
/// Said in place of the field when its buffer could not be reserved.
const UNUSABLE: &str = "The field for the key could not be made, so it cannot be typed back.";

/// What the type-back field takes, for the window's key list.
pub const TYPE_BACK_KEYS: &[(&str, &str)] = &[
    ("Left/Right", "move the caret"),
    ("Home/End", "the caret to the start or end of the key typed"),
    (
        "Backspace/Delete",
        "delete the character before or at the caret",
    ),
    (
        "a character",
        "typed at the caret: digits, and spaces or hyphens between groups",
    ),
];

/// A recovery key the installer service sent: its 48 digits, every group's
/// value and check digit admitted, in one allocation zeroed on drop. It is
/// neither `Clone` nor printable; `Debug` names no digit.
pub struct Key(RecoveryKey);

impl Key {
    /// The 48 digits as the wire carries them, without separators. A group
    /// whose value or check digit fails refuses the whole key: a key that
    /// cannot be typed back is not shown.
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

/// The type-back field: digits, and spaces or hyphens, typed at a caret.
/// Its buffer is reserved once at td-protector's entry bound, so no edit
/// reallocates, and td-ui's entry zeroes what an edit leaves, a clear and
/// the drop. `Debug` shows its length, never its text.
#[derive(Debug)]
pub struct Entry(Option<EntryModel>);

impl Default for Entry {
    /// An entry the allocator cannot reserve takes no key.
    fn default() -> Self {
        Self(EntryModel::new(MAX_ENTRY_LEN).ok())
    }
}

impl Entry {
    /// Applies one key chord; true when the text changed, not when only
    /// the caret moved. Any other character than a digit, space or hyphen
    /// is not taken.
    pub fn key(&mut self, chord: &str) -> bool {
        let Some(model) = self.0.as_mut() else {
            return false;
        };
        let motion = |motion| Action::Move {
            motion,
            extend: false,
        };
        let action = match chord {
            "Left" => motion(Motion::Left),
            "Right" => motion(Motion::Right),
            "Home" => motion(Motion::Home),
            "End" => motion(Motion::End),
            "Backspace" => Action::Backspace,
            "Delete" => Action::Delete,
            _ => {
                let mut chars = chord.chars();
                match (chars.next(), chars.next()) {
                    (Some(typed @ ('0'..='9' | ' ' | '-')), None) => Action::Insert(typed),
                    _ => return false,
                }
            }
        };
        model.act(action) == Ok(Outcome::Changed)
    }

    /// Forgets what was typed, zeroing it.
    pub fn clear(&mut self) {
        if let Some(model) = self.0.as_mut() {
            model.clear();
        }
    }

    fn text(&self) -> &str {
        self.0.as_ref().map_or("", EntryModel::text)
    }

    /// What the field says of the key typed so far: how many digits, or
    /// the first group whose value or check digit fails once it is whole.
    pub fn feedback(&self) -> String {
        if self.0.is_none() {
            return UNUSABLE.into();
        }
        match RecoveryKey::check_partial(self.text().as_bytes()) {
            Ok(0) => "Type the 48 digits; spaces or hyphens between groups are optional.".into(),
            Ok(PASSPHRASE_LEN) => {
                "All 48 digits are typed. Return asks the installer service to confirm them.".into()
            }
            Ok(digits) => format!("{digits} of 48 digits typed."),
            Err(error) => refusal(&error),
        }
    }

    /// The key typed, once it is 48 digits whose every group checks; else
    /// what is wrong with it, a whole group that fails named before a
    /// short count. The field keeps its text.
    pub fn parse(&self) -> Result<Key, String> {
        if self.0.is_none() {
            return Err(UNUSABLE.into());
        }
        let typed = self.text().as_bytes();
        RecoveryKey::check_partial(typed)
            .and_then(|_| RecoveryKey::parse(typed))
            .map(Key)
            .map_err(|error| refusal(&error))
    }
}

/// An entry refusal in the installer's words; it names a group, never a
/// digit.
fn refusal(error: &EntryError) -> String {
    match error {
        EntryError::TooLong => "The entry is longer than any recovery key.".into(),
        EntryError::Character { .. } => "Type only digits, spaces and hyphens.".into(),
        EntryError::GroupLength { group, digits } => {
            format!("Group {group} has {digits} digits before a space or hyphen; groups have 6.")
        }
        EntryError::Length { digits } => format!("{digits} digits are typed; the key has 48."),
        EntryError::Value { group } => {
            format!("Group {group} is wrong: its first five digits are above 65535.")
        }
        EntryError::Check { group } => {
            format!(
                "Group {group} is wrong: its check digit does not agree. Compare it with the key."
            )
        }
    }
}

/// Which recovery-key view to paint.
pub enum Step<'a> {
    /// The key is asked for and not yet here.
    Asking,
    /// The key, in its display form.
    Shown(&'a str),
    /// The type-back field, what it says of the key typed so far, and
    /// what became of the last confirmation asked for.
    TypeBack {
        entry: &'a Entry,
        feedback: &'a str,
        notice: Option<&'a str>,
    },
}

/// One recovery-key view: a pure rendering of the key the window holds or
/// the field it is typed back in. It writes nothing anywhere.
pub struct RecoveryPage<'a> {
    surface: Surface,
    step: Step<'a>,
    field: TextEntry,
    footer: Status,
}

impl<'a> RecoveryPage<'a> {
    pub fn new(surface: Surface, step: Step<'a>) -> Option<Self> {
        crate::supported_page(surface)?;
        let scale = surface.scale.value();
        let width = surface.width.checked_sub(2 * INSET * scale)?;
        let field = TextEntry::new(
            surface,
            Rect {
                x: (INSET * scale) as i64,
                y: (FIELD_ROW * ROW * scale) as i64,
                width: width as u32,
                height: (ROW * scale) as u32,
            },
        )?;
        let footer = Status::new(surface);
        if ((LAST_ROW * ROW + CELL_HEIGHT) * scale) as i64 > footer.rect().y {
            return None;
        }
        Some(Self {
            surface,
            step,
            field,
            footer,
        })
    }
}

impl Composition for RecoveryPage<'_> {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let bounds = self.surface.bounds();
        if let Some(clip) = bounds.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: bounds,
                    color: CHROME,
                },
            });
        }
        let surface = self.surface;
        let footer = match &self.step {
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
                row(surface, 7, "key is typed back.", damage, sink);
                row(surface, 9, CLOSING, damage, sink);
                ASKING_FOOTER
            }
            Step::Shown(key) => {
                row(surface, 1, "Recovery key", damage, sink);
                row(
                    surface,
                    3,
                    "Write down this recovery key now:",
                    damage,
                    sink,
                );
                row(surface, FIELD_ROW, key, damage, sink);
                for (index, text) in [
                    "It is shown only now, and the installer stores no copy of it.",
                    "It is the only way to open this disk if the computer's TPM, its",
                    "firmware measurements or its boot chain change. Keep it safe,",
                    "away from the computer.",
                ]
                .iter()
                .enumerate()
                {
                    row(surface, FEEDBACK_ROW + index, text, damage, sink);
                }
                row(
                    surface,
                    LAST_ROW - 2,
                    "Press Return to type it back. The installation completes once it is confirmed.",
                    damage,
                    sink,
                );
                row(surface, LAST_ROW, CLOSING, damage, sink);
                SHOWN_FOOTER
            }
            Step::TypeBack {
                entry,
                feedback,
                notice,
            } => {
                row(surface, 1, "Type the recovery key back", damage, sink);
                row(
                    surface,
                    3,
                    "Type the 48 digits as you wrote them down.",
                    damage,
                    sink,
                );
                if let Some(model) = &entry.0 {
                    let mut field = model.field("The recovery key's 48 digits", true, true);
                    field.first =
                        self.field
                            .reveal(model.text().chars().count(), field.caret, field.first);
                    self.field.emit(field, damage, sink);
                }
                row(surface, FEEDBACK_ROW, feedback, damage, sink);
                if let Some(notice) = notice {
                    row(surface, NOTICE_ROW, notice, damage, sink);
                }
                row(
                    surface,
                    NOTICE_ROW + 2,
                    "Escape shows the key again. The installation completes once it is confirmed.",
                    damage,
                    sink,
                );
                row(surface, NOTICE_ROW + 4, CLOSING, damage, sink);
                TYPE_BACK_FOOTER
            }
        };
        self.footer.emit(footer.chars(), damage, sink);
    }
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

    fn typed(text: &str) -> Entry {
        let mut entry = Entry::default();
        for typed in text.chars() {
            entry.key(&typed.to_string());
        }
        entry
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

    #[test]
    fn the_entry_takes_the_forms_the_codec_admits_and_shows_no_text_in_debug() {
        let mut entry = typed(SHOWN);
        assert_eq!(entry.parse().unwrap(), Key::from_digits(KEY).unwrap());
        let debug = format!("{entry:?}");
        assert!(!debug.contains("000013"), "{debug}");
        // The caret moves and edits as the field binds them; only an edit
        // is a change.
        for (chord, changed) in [
            ("Home", false),
            ("Delete", true),
            ("End", false),
            ("Left", false),
            ("Backspace", true),
            ("Right", false),
        ] {
            assert_eq!(entry.key(chord), changed, "{chord}");
        }
        assert!(entry.parse().is_err());
        entry.clear();
        assert_eq!(entry.text(), "");
        assert!(!entry.key("Backspace"));
        for spaced in [
            "000013 005150 010290 015439 020571 025716 030859 035998",
            " 000013005150010290015439020571025716030859035998 ",
        ] {
            assert_eq!(
                typed(spaced).parse().unwrap(),
                Key::from_digits(KEY).unwrap()
            );
        }
        // The buffer holds no more than the codec scans.
        let mut long = Entry::default();
        for _ in 0..MAX_ENTRY_LEN + 8 {
            long.key("-");
        }
        assert_eq!(long.text().len(), MAX_ENTRY_LEN);
    }

    #[test]
    fn each_step_says_what_the_key_is_for_and_fits_the_tile() {
        let screen = surface(crate::MIN_PAGE_WIDTH, crate::MIN_PAGE_HEIGHT);
        let asking = glyphs(&RecoveryPage::new(screen, Step::Asking).unwrap(), screen);
        assert!(asking.contains("Asking the installer service for the recovery key"));
        assert!(asking.contains(ASKING_FOOTER));
        assert!(!asking.contains("Write down"));
        let shown = glyphs(
            &RecoveryPage::new(screen, Step::Shown(SHOWN)).unwrap(),
            screen,
        );
        for said in [
            SHOWN,
            "Write down this recovery key now:",
            "shown only now",
            "stores no copy",
            "only way to open this disk",
            "TPM",
            "firmware measurements or its boot chain change",
            "Press Return to type it back",
            CLOSING,
            SHOWN_FOOTER,
        ] {
            assert!(shown.contains(said), "{said}");
        }
        let entry = typed("000013-0051");
        let feedback = entry.feedback();
        assert_eq!(feedback, "10 of 48 digits typed.");
        let page = RecoveryPage::new(
            screen,
            Step::TypeBack {
                entry: &entry,
                feedback: &feedback,
                notice: Some("That is not the recovery key shown."),
            },
        )
        .unwrap();
        let typing = glyphs(&page, screen);
        for said in [
            "Type the recovery key back",
            "000013-0051",
            "10 of 48 digits typed.",
            "That is not the recovery key shown.",
            "Escape shows the key again",
            CLOSING,
            TYPE_BACK_FOOTER,
        ] {
            assert!(typing.contains(said), "{said}");
        }
        assert!(asking.contains(CLOSING));
        // An entry whose buffer could not be reserved says so, and what
        // closing does, rather than taking keys silently.
        let mut unusable = Entry(None);
        assert!(!unusable.key("0"));
        assert_eq!(unusable.feedback(), UNUSABLE);
        assert_eq!(unusable.parse().unwrap_err(), UNUSABLE);
        let feedback = unusable.feedback();
        let dead = glyphs(
            &RecoveryPage::new(
                screen,
                Step::TypeBack {
                    entry: &unusable,
                    feedback: &feedback,
                    notice: None,
                },
            )
            .unwrap(),
            screen,
        );
        assert!(dead.contains(UNUSABLE) && dead.contains(CLOSING));
        // The key itself is not on the type-back page.
        assert!(!typing.contains(SHOWN));
        for step in [Step::Asking, Step::Shown(SHOWN)] {
            assert!(RecoveryPage::new(surface(751, 480), step).is_none());
        }
        assert!(RecoveryPage::new(surface(752, 479), Step::Asking).is_none());
        let scaled = Surface::new(1504, 960, Scale::new(2).unwrap()).unwrap();
        assert!(RecoveryPage::new(scaled, Step::Shown(SHOWN)).is_some());
        // Every refusal, at its widest, fits the narrowest tile whole.
        let columns = (crate::MIN_PAGE_WIDTH - 2 * INSET) / CELL_WIDTH;
        for error in [
            EntryError::TooLong,
            EntryError::Character { position: 256 },
            EntryError::GroupLength {
                group: 8,
                digits: 256,
            },
            EntryError::Length { digits: 256 },
            EntryError::Value { group: 8 },
            EntryError::Check { group: 8 },
        ] {
            let said = refusal(&error);
            assert!(said.chars().count() <= columns, "{said}");
        }
        assert!(CLOSING.chars().count() <= columns && UNUSABLE.chars().count() <= columns);
        for feedback in [typed("").feedback(), typed(SHOWN).feedback()] {
            assert!(feedback.chars().count() <= columns, "{feedback}");
        }
    }
}
