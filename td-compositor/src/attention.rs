use crate::authority::consent::Request;
use crate::ui;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Notice {
    #[default]
    Menu,
    Pending,
    Unlocked,
    Stored,
    NoWrite,
    NoInstall,
    /// Root's `99 01`: the queued update cannot read the login record
    /// (td-authd/DESIGN.md, amendment 8).
    UpdateRefused,
    Installed,
    /// A committed device-bound disk installation, before its recovery key:
    /// the screen then closes by itself (secret_client).
    Returning,
    Enrolled,
    Unenrolled,
    Unavailable,
    Failed,
    Busy,
    /// The key-management screen `K` opens.
    LoginKeys,
    /// Removal digits: the enrolled key count and the chosen positions as
    /// bits, position 1 lowest.
    Removing {
        keys: u8,
        chosen: u8,
    },
    /// A selection this build refuses.
    NotAvailable,
    /// A login-key operation's end (td-login/TOKEN-LOGIN.md, "Failure
    /// texts").
    Login(&'static [&'static str]),
    /// A write whose outcome is uncertain, then its kind's rows.
    Uncertain(&'static [&'static str]),
}

/// The PIN field beneath a presented PIN step's prompt, or the touch
/// request that follows it: a count of bytes typed, never a byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Pin(u8),
    Touch,
}

/// The field's rows (td-login/TOKEN-LOGIN.md, "PIN entry, presence and
/// retries").
const PIN_PROMPT: &str = "ENTER THE PIN FOR THIS KEY";
const TOUCH_PROMPT: &str = "TOUCH YOUR KEY";
/// The most masks the field shows: FIDO's longest PIN.
const MASKS: usize = 63;
/// A mask's side and advance in the prompt's font pixels, so that every
/// mask fits one row inside the narrowest prompt's margins.
const MASK_SIDE: usize = 3;
const MASK_ADVANCE: usize = 4;

/// A store operation's or installation review's ceiling, in seconds; a
/// login operation's is its own (td-authd's `login_ceiling`).
const OPERATION_SECONDS: u64 = 120;

/// Rasterize once, retaining the exact immutable description beside its pixels.
#[derive(Debug)]
pub(crate) struct Prepared {
    request: Request,
    pixels: Vec<u8>,
    geometry: (usize, usize, usize),
    /// The prompt's scale and the foot of its last row: the field goes in
    /// the band below, which the prompt leaves free.
    scale: usize,
    foot: usize,
    /// A PIN step's field rows, rasterized with the prompt's font.
    field_rows: Option<FieldRows>,
}

/// Each field row's ink as offsets from the row's origin, and the rows'
/// height.
#[derive(Debug)]
struct FieldRows {
    height: usize,
    pin: Vec<(usize, usize)>,
    touch: Vec<(usize, usize)>,
}

impl Prepared {
    #[cfg(test)]
    pub fn new(
        request: Request,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<Self, String> {
        Self::with_time(request, width, height, stride, None)
    }

    pub fn with_time(
        request: Request,
        width: usize,
        height: usize,
        stride: usize,
        remaining: Option<u64>,
    ) -> Result<Self, String> {
        let length = stride
            .checked_mul(height)
            .ok_or("prompt output size overflow")?;
        if width < 320
            || height < 200
            || stride < width.saturating_mul(4)
            || length > 64 * 1024 * 1024
        {
            return Err("output cannot hold a complete trusted prompt".into());
        }
        let font = crate::font::pinned()?;
        let scale = if width >= 800 && height >= 600 { 2 } else { 1 };
        let cell_width = font
            .width()
            .checked_mul(scale)
            .ok_or("prompt cell overflow")?;
        let cell_height = font
            .height()
            .checked_mul(scale)
            .ok_or("prompt cell overflow")?;
        let columns = width
            .saturating_sub(48)
            .checked_div(cell_width)
            .filter(|value| *value >= 24)
            .ok_or("output cannot hold a complete trusted prompt")?;
        let mut lines = request.lines();
        let ceiling = request
            .login_ceiling()
            .map_or(OPERATION_SECONDS, |ceiling| ceiling.as_secs());
        if let Some(seconds) = remaining {
            if !(1..=ceiling).contains(&seconds) {
                return Err("invalid trusted operation time budget".into());
            }
            let unit = if seconds == 1 { "SECOND" } else { "SECONDS" };
            lines.push(format!("TIME LEFT WHEN SHOWN: {seconds} {unit}"));
        }
        let mut rows = Vec::new();
        for line in &lines {
            if !line.is_ascii() || !line.chars().all(|character| font.covers(character)) {
                return Err("trusted prompt contains an unsupported glyph".into());
            }
            let mut remaining = line.as_str();
            let mut indent = 0;
            while !remaining.is_empty() {
                let count = (columns - indent).min(remaining.len());
                let chunk = remaining
                    .get(..count)
                    .ok_or("invalid trusted prompt text")?;
                rows.push((indent, chunk));
                remaining = remaining
                    .get(count..)
                    .ok_or("invalid trusted prompt text")?;
                indent = 2;
            }
        }
        let row_height = cell_height
            .checked_add(4 * scale)
            .ok_or("prompt row overflow")?;
        let text_height = rows
            .len()
            .checked_mul(row_height)
            .and_then(|height| height.checked_sub(4 * scale))
            .ok_or("prompt height overflow")?;
        if text_height > height.saturating_sub(48) {
            return Err("output cannot hold every trusted prompt argument".into());
        }
        let mut pixels = vec![0; length];
        ui::fill(
            &mut pixels,
            width,
            height,
            stride,
            (0, 0, width, height),
            [0x28, 0x20, 0x18, 0],
        );
        let top = (height - text_height) / 2;
        for (row, (indent, text)) in rows.iter().enumerate() {
            for (column, character) in text.chars().enumerate() {
                let glyph = font.index(character);
                for y in 0..font.height() {
                    for x in 0..font.width() {
                        if font.pixel(glyph, x, y) {
                            ui::fill(
                                &mut pixels,
                                width,
                                height,
                                stride,
                                (
                                    24 + (column + indent) * cell_width + x * scale,
                                    top + row * row_height + y * scale,
                                    scale,
                                    scale,
                                ),
                                [0xff, 0xff, 0xff, 0],
                            );
                        }
                    }
                }
            }
        }
        let field_rows = if request.login_step().is_some_and(|step| step.asks_pin()) {
            Some(FieldRows {
                height: cell_height,
                pin: ink(&font, PIN_PROMPT, scale)?,
                touch: ink(&font, TOUCH_PROMPT, scale)?,
            })
        } else {
            None
        };
        let prepared = Self {
            request,
            pixels,
            geometry: (width, height, stride),
            scale,
            foot: top + text_height,
            field_rows,
        };
        // A PIN step whose field could not open beneath it is not shown, so
        // its operation fails before any PIN reaches the key.
        if prepared.field_rows.is_some() && !prepared.holds_field() {
            return Err("output cannot hold the PIN field beneath this prompt".into());
        }
        Ok(prepared)
    }

    pub fn request(&self) -> &Request {
        &self.request
    }

    pub fn paint(&self, frame: &mut [u8], width: usize, height: usize, stride: usize) -> bool {
        if self.geometry != (width, height, stride) || frame.len() != self.pixels.len() {
            return false;
        }
        frame.copy_from_slice(&self.pixels);
        true
    }

    /// Where the field goes: a row gap below the prompt's last row, its
    /// text row's top and its masks row's, with a row gap's margin below.
    /// None for a step that asks no PIN, or where the band the prompt
    /// leaves free cannot hold the field whole with all 63 masks: the
    /// prompt is never moved, shrunk or overdrawn to make room.
    fn field_layout(&self) -> Option<(&FieldRows, usize, usize)> {
        let rows = self.field_rows.as_ref()?;
        let (width, height, _) = self.geometry;
        let gap = 4 * self.scale;
        let text = self.foot.checked_add(gap)?;
        let masks = text.checked_add(rows.height)?.checked_add(gap)?;
        let bottom = masks
            .checked_add(MASK_SIDE * self.scale)?
            .checked_add(gap)?;
        let row = MASKS
            .checked_mul(MASK_ADVANCE * self.scale)?
            .checked_add(48)?;
        (bottom <= height && row <= width).then_some((rows, text, masks))
    }

    /// Whether the band beneath this prompt holds the PIN field.
    pub fn holds_field(&self) -> bool {
        self.field_layout().is_some()
    }

    /// The prompt exactly as presented, with `field` beneath it: the PIN
    /// field's row and one square mask per byte typed, or the touch
    /// request.
    pub fn paint_field(
        &self,
        frame: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        field: Field,
    ) -> bool {
        let Some((rows, text, masks)) = self.field_layout() else {
            return false;
        };
        let (ink, count) = match field {
            Field::Pin(count) => (&rows.pin, usize::from(count)),
            Field::Touch => (&rows.touch, 0),
        };
        if count > MASKS || !self.paint(frame, width, height, stride) {
            return false;
        }
        let white = [0xff, 0xff, 0xff, 0];
        for (x, y) in ink {
            ui::fill(
                frame,
                width,
                height,
                stride,
                (24 + x, text + y, self.scale, self.scale),
                white,
            );
        }
        let side = MASK_SIDE * self.scale;
        for column in 0..count {
            ui::fill(
                frame,
                width,
                height,
                stride,
                (24 + column * MASK_ADVANCE * self.scale, masks, side, side),
                white,
            );
        }
        true
    }
}

/// `text`'s ink in the prompt's font at `scale`: each set font pixel's
/// offset from the row's origin.
fn ink(font: &crate::font::Font, text: &str, scale: usize) -> Result<Vec<(usize, usize)>, String> {
    let mut ink = Vec::new();
    for (column, character) in text.chars().enumerate() {
        if !font.covers(character) {
            return Err("PIN field contains an unsupported glyph".into());
        }
        let glyph = font.index(character);
        for y in 0..font.height() {
            for x in 0..font.width() {
                if font.pixel(glyph, x, y) {
                    ink.push(((column * font.width() + x) * scale, y * scale));
                }
            }
        }
    }
    Ok(ink)
}

/// What a committed device-bound disk installation shows before attention
/// closes by itself: its recovery key is shown and typed back in td-setup.
pub(crate) const RETURNING_NOTICE: &str = "INSTALLING - RETURNING TO SETUP FOR THE RECOVERY KEY";

/// Display-only pixels: ordinary scene rendering never calls this painter.
/// A lifetime that left the lock surface keeps its success notice while
/// held input drains: that drain cancels nothing. Nor does the close after
/// a committed device-bound installation's returning notice.
pub(crate) fn paint(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    draining: bool,
    unlocked: bool,
    notice: Notice,
) {
    let bounds = (0, 0, width, height);
    ui::fill(frame, width, height, stride, bounds, [0x28, 0x20, 0x18, 0]);
    let (scale, columns) = layout(width, height);
    let mut rows = vec![String::from("TD SECURE ATTENTION")];
    if draining && !unlocked && notice != Notice::Returning {
        rows.push("CANCELLING REQUEST".into());
    } else {
        rows.extend(notice_rows(notice));
    }
    // The menu's rows keep their places, and the last row stays below them.
    if rows.len() < MENU_ROWS {
        rows.resize(MENU_ROWS, String::new());
    }
    rows.push(
        if draining {
            "RELEASE KEYS AND BUTTONS"
        } else {
            "ESC TO RETURN"
        }
        .into(),
    );
    draw_rows(frame, width, height, stride, scale, columns, &rows);
}

/// The lock surface's state row for an enrolled account.
const UNLOCK_ROWS: &[&str] = &["PRESS CTRL+ALT+ESC TO UNLOCK"];

/// The lock surface's rows for root's `1a` answer (td-login/TOKEN-LOGIN.md,
/// "Session lock"): its hostname, unless empty, and username, uppercase,
/// then `LOCKED` and the state's rows. With no answer, `LOCKED` alone.
pub(crate) fn lock_rows(answer: Option<&crate::authority::Answer>) -> Vec<String> {
    use crate::authority::LoginState;
    use crate::secret_client::login_failure;
    let Some(answer) = answer else {
        return vec!["LOCKED".into()];
    };
    let state = match answer.state() {
        LoginState::Enrolled(_) => Some(UNLOCK_ROWS),
        LoginState::Unavailable(cause) => login_failure(*cause, 0),
        LoginState::Unenrolled => login_failure(0x09, 0),
    };
    let hostname = Some(answer.hostname()).filter(|name| !name.is_empty());
    hostname
        .into_iter()
        .chain([answer.username(), "LOCKED"])
        .map(str::to_ascii_uppercase)
        .chain(
            state
                .unwrap_or_default()
                .iter()
                .map(|row| String::from(*row)),
        )
        .collect()
}

/// The lock surface, over the whole output: display-only pixels, as the
/// attention screen's are, in its chrome rows and place. A row wider than
/// the output's columns, as a long hostname is, wraps.
pub(crate) fn paint_lock(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    rows: &[String],
) {
    ui::fill(
        frame,
        width,
        height,
        stride,
        (0, 0, width, height),
        [0x28, 0x20, 0x18, 0],
    );
    let (scale, columns) = layout(width, height);
    draw_rows(frame, width, height, stride, scale, columns, rows);
}

/// `rows`, each wrapped to `columns`, from the menu's place down.
fn draw_rows(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    scale: usize,
    columns: usize,
    rows: &[String],
) {
    let bounds = (0, 0, width, height);
    let rows: Vec<String> = rows.iter().flat_map(|row| wrap(row, columns)).collect();
    let pitch = 18 * scale;
    let block = rows
        .len()
        .saturating_sub(1)
        .saturating_mul(pitch)
        .saturating_add(ui::GLYPH_HEIGHT * scale);
    let top = rows_top(height, block);
    for (index, text) in rows.iter().enumerate() {
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            24,
            top.saturating_add(index.saturating_mul(pitch)),
            scale,
            text,
            [0xff, 0xff, 0xff, 0],
            bounds,
        );
    }
}

/// Every screen's rows are padded to the title and six more, and its last
/// row is drawn below them: where the menu's was before `L`. The menu, a
/// row longer, draws its last row one lower (td-compositor/DESIGN.md,
/// "Session lock and login-key entry", item 5).
const MENU_ROWS: usize = 7;

/// The first row's top for rows `block` pixels tall, the last row's foot
/// included: the menu's long-standing place, where the boot oracles read
/// its rows, wherever the whole block fits below it; on an output too short
/// for that, the least shift up that makes it fit.
fn rows_top(height: usize, block: usize) -> usize {
    let top = height.saturating_sub(248) / 2;
    top.min(height.saturating_sub(block))
}

/// The chrome font's scale on an output, the prompt's rule, and the columns
/// a row has from 24 pixels in with as much to spare on the right.
pub(crate) fn layout(width: usize, height: usize) -> (usize, usize) {
    let scale = if width >= 800 && height >= 600 { 2 } else { 1 };
    (
        scale,
        width.saturating_sub(48) / (ui::GLYPH_ADVANCE * scale),
    )
}

/// A row too wide for `columns` as several, broken at a space where one
/// falls within the row; a row that fits is kept exactly as it is.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut rows = Vec::new();
    let mut rest = text;
    while rest.len() > columns {
        let cut = rest
            .get(..=columns)
            .and_then(|head| head.rfind(' '))
            .filter(|cut| *cut > 0)
            .unwrap_or(columns);
        let (Some(head), Some(tail)) = (rest.get(..cut), rest.get(cut..)) else {
            break;
        };
        rows.push(head.trim_end().to_string());
        rest = tail.trim_start();
    }
    if !rest.is_empty() || rows.is_empty() {
        rows.push(rest.to_string());
    }
    rows
}

/// A notice's rows below the title. The menu's rows are fixed in place, `K`
/// comes below `I` and `L` below `K`, so the boot oracles' rows do not move.
fn notice_rows(notice: Notice) -> Vec<String> {
    let first = match notice {
        Notice::Menu => "U: UNLOCK  R: RECOVERY TOKEN",
        Notice::Pending => "PREPARING REQUEST",
        Notice::NoInstall => "NO INSTALLATION IS READY TO REVIEW",
        Notice::UpdateRefused => "UPDATE CANNOT READ LOGIN KEYS",
        Notice::Installed => "SYSTEM INSTALLED - RESTART TO BOOT IT",
        Notice::Returning => RETURNING_NOTICE,
        Notice::Stored => "CREDENTIAL STORED",
        Notice::NoWrite => "NO READY CREDENTIAL WRITE - RUN TD-SECRET SET FIRST",
        Notice::Unlocked => "SECRETS UNLOCKED",
        Notice::Enrolled => "STORE ENROLLED - REOPEN AND PRESS U TO UNLOCK",
        Notice::Unenrolled => "STORE NOT ENROLLED - REOPEN TO TRY AGAIN",
        Notice::Unavailable => "STORE STATE UNAVAILABLE",
        Notice::Failed => "REQUEST FAILED",
        Notice::Busy => "PREVIOUS REQUEST IS STILL FINISHING",
        Notice::LoginKeys => "LOGIN KEYS",
        Notice::NotAvailable => "NOT AVAILABLE IN THIS BUILD",
        Notice::Uncertain(_) => "RESULT UNCERTAIN",
        Notice::Login(rows) => return rows.iter().map(|row| String::from(*row)).collect(),
        Notice::Removing { keys, chosen } => {
            let named: Vec<String> = (1..=keys)
                .filter(|position| {
                    1u8.checked_shl(u32::from(position.saturating_sub(1)))
                        .is_some_and(|bit| chosen & bit != 0)
                })
                .map(|position| position.to_string())
                .collect();
            return vec![
                format!("REMOVE: PRESS 1 TO {keys} THEN ENTER"),
                if named.is_empty() {
                    "SELECTED: NONE".into()
                } else {
                    format!("SELECTED: {}", named.join(" "))
                },
            ];
        }
    };
    let rest: &[&str] = match notice {
        Notice::Menu => &[
            "E: ENROLL TWO TOKENS (HAVE BOTH READY)",
            "X: ENROLL WITHOUT RECOVERY - LOSS IS FINAL",
            "W: REVIEW PENDING CREDENTIAL WRITE",
            "I: REVIEW PENDING SYSTEM INSTALLATION",
            "K: LOGIN KEYS",
            "L: LOCK SCREEN",
        ],
        Notice::LoginKeys => &[
            "1: ENROLL ONE KEY",
            "2: ENROLL TWO KEYS",
            "A: ADD A KEY",
            "D: REMOVE KEYS",
        ],
        Notice::Uncertain(rows) => rows,
        _ => &[],
    };
    std::iter::once(first)
        .chain(rest.iter().copied())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use crate::authority::consent::Operation;

    fn request(name: &str) -> Request {
        Request::new(
            [1; 32],
            1000,
            Operation::Set {
                role: crate::authority::consent::Role::Primary,
                application: "mail".into(),
                name: name.into(),
                application_uid: 65537,
                requester: 1000,
            },
        )
        .unwrap()
    }

    #[test]
    fn complete_prompt_preserves_identifier_case_and_refuses_clipping() {
        let upper = Prepared::new(request("Main"), 800, 600, 3200).unwrap();
        let lower = Prepared::new(request("main"), 800, 600, 3200).unwrap();
        assert_ne!(upper.pixels, lower.pixels);
        assert_eq!(upper.request(), &request("Main"));
        assert!(Prepared::new(request(&"a".repeat(64)), 320, 200, 1280).is_err());
        assert!(Prepared::new(request("main"), 200, 200, 800).is_err());
        assert!(Prepared::new(request("main"), 800, 600, 1).is_err());
        assert!(Prepared::new(request("main"), 800, usize::MAX, 3200).is_err());
        let mut display = vec![0; 800 * 600 * 4];
        assert!(upper.paint(&mut display, 800, 600, 3200));
        assert_eq!(display, upper.pixels);
        let before = display.clone();
        assert!(!upper.paint(&mut display, 799, 600, 3200));
        assert_eq!(display, before);
    }

    /// The widest disk installation summary, every field at its bound and a
    /// time line, is shown whole from 1024x768; smaller outputs that cannot
    /// hold it refuse rather than clip.
    #[test]
    fn the_widest_disk_installation_prompt_fits_a_1024_by_768_output() {
        use crate::authority::consent::{
            Label, Storage, DISK_NAME_BYTES, HOSTNAME_BYTES, USERNAME_BYTES,
        };
        let widest = Request::new(
            [1; 32],
            1000,
            Operation::InstallDisk {
                requester: 1000,
                disk: "d".repeat(DISK_NAME_BYTES),
                capacity: u64::MAX,
                model: Some(Label::model(&[0xff; 64])),
                serial: Some(Label::serial(&[0xff; 64])),
                hostname: "h".repeat(HOSTNAME_BYTES),
                username: "u".repeat(USERNAME_BYTES),
                deployment: [0xff; 8],
                storage: Storage::DeviceBound,
            },
        )
        .unwrap();
        assert!(Prepared::with_time(widest.clone(), 1024, 768, 4096, Some(120)).is_ok());
        assert_eq!(
            Prepared::with_time(widest, 800, 600, 3200, Some(120))
                .err()
                .unwrap(),
            "output cannot hold every trusted prompt argument"
        );
        let typical = Request::new(
            [1; 32],
            1000,
            Operation::InstallDisk {
                requester: 1000,
                disk: "nvme0n1".into(),
                capacity: 512_110_190_592,
                model: Some(Label::model(b"Samsung SSD 980 PRO 1TB")),
                serial: Some(Label::serial(b"S5GXNX0T123456A")),
                hostname: "td-laptop".into(),
                username: "tester".into(),
                deployment: [0xab; 8],
                storage: Storage::Unencrypted,
            },
        )
        .unwrap();
        assert!(Prepared::with_time(typical, 800, 600, 3200, Some(120)).is_ok());
    }

    /// The returning notice is drawn whole at a 1280-wide output, and is
    /// what the screen says while it closes, never a cancellation.
    #[test]
    fn the_returning_notice_is_whole_and_never_says_cancelling() {
        let font = crate::font::pinned().unwrap();
        assert!(RETURNING_NOTICE.chars().all(|c| font.covers(c)));
        assert!(24 + RETURNING_NOTICE.len() * font.width() * 2 <= 1280);
        assert_eq!(
            notice_rows(Notice::Returning),
            vec![String::from(RETURNING_NOTICE)]
        );
        let painted = |draining: bool, notice: Notice| {
            let mut frame = vec![0; 1280 * 800 * 4];
            paint(&mut frame, 1280, 800, 1280 * 4, draining, false, notice);
            frame
        };
        assert_ne!(
            painted(false, Notice::Returning),
            painted(false, Notice::Installed)
        );
        // Every other notice says CANCELLING REQUEST while draining; the
        // returning notice stays.
        assert_eq!(
            painted(true, Notice::Installed),
            painted(true, Notice::Menu)
        );
        assert_ne!(
            painted(true, Notice::Returning),
            painted(true, Notice::Menu)
        );
    }

    /// The elevation prompts, rollback with both IDs and a hostname change
    /// with both names at 63 bytes, are shown whole with a time line from
    /// 800x600, and 320x200, which cannot hold their wrapped arguments,
    /// refuses rather than clips. The key is on the prompt.
    #[test]
    fn the_widest_elevation_prompts_fit_an_800_by_600_output() {
        use crate::authority::consent::ApprovalKey;
        let prompts = |digits: &[u8; 2]| {
            let key = ApprovalKey::new(*digits).unwrap();
            [
                Operation::DeployRollback {
                    key,
                    current: "a".repeat(64),
                    previous: "b".repeat(64),
                },
                Operation::SetHostname {
                    key,
                    requester: 65533,
                    old: "a".repeat(63),
                    new: "b".repeat(63),
                },
            ]
            .map(|operation| Request::new([1; 32], 65533, operation).unwrap())
        };
        for (request, swapped) in prompts(b"47").into_iter().zip(prompts(b"74")) {
            assert!(Prepared::with_time(request.clone(), 800, 600, 3200, Some(120)).is_ok());
            assert_eq!(
                Prepared::with_time(request.clone(), 320, 200, 1280, Some(120))
                    .err()
                    .unwrap(),
                "output cannot hold every trusted prompt argument"
            );
            assert_ne!(
                Prepared::new(request, 800, 600, 3200).unwrap().pixels,
                Prepared::new(swapped, 800, 600, 3200).unwrap().pixels
            );
        }
    }

    /// Consent keeps every login row within `PROMPT_COLUMNS`, this renderer's
    /// columns at its narrowest accepted width, so wrapping never splits a
    /// fingerprint. The widest login prompt asks for a PIN, and at 800x600
    /// its field would not fit beneath it, so it is shown, with room for
    /// its field, at 1024x768.
    #[test]
    fn login_rows_fit_the_narrowest_prompt_and_the_tallest_refuse_800_by_600() {
        use crate::authority::consent::{LoginStep, Slot, LOGIN_KEYS, PROMPT_COLUMNS};
        let font = crate::font::pinned().unwrap();
        assert_eq!((320 - 48) / font.width(), PROMPT_COLUMNS);
        let widest = Request::new(
            [1; 32],
            65533,
            Operation::LoginRemove {
                account: 65533,
                before: LOGIN_KEYS,
                after: 0,
                removed: (1..=LOGIN_KEYS)
                    .map(|position| Slot {
                        position,
                        key: [0xff; 4],
                    })
                    .collect(),
                step: LoginStep::Authorize {
                    key: [0xff; 4],
                    retries: 255,
                },
            },
        )
        .unwrap();
        assert!(Prepared::with_time(widest.clone(), 320, 480, 1280, Some(120)).is_ok());
        assert!(Prepared::with_time(widest.clone(), 319, 480, 1276, Some(120)).is_err());
        assert_eq!(
            Prepared::with_time(widest.clone(), 800, 600, 3200, Some(120))
                .err()
                .unwrap(),
            "output cannot hold the PIN field beneath this prompt"
        );
        assert!(Prepared::with_time(widest, 1024, 768, 4096, Some(120))
            .unwrap()
            .holds_field());
    }

    /// A login prompt's time budget is its operation's ceiling, up to 240
    /// seconds for two ceremonies; every other prompt's stays 120.
    #[test]
    fn a_prompts_time_budget_is_its_operations_ceiling() {
        use crate::authority::consent::LoginStep;
        let add = Request::new(
            [1; 32],
            1000,
            Operation::LoginAdd {
                account: 1000,
                before: 1,
                after: 2,
                step: LoginStep::Identify,
            },
        )
        .unwrap();
        let unlock = Request::new(
            [1; 32],
            1000,
            Operation::LoginUnlock {
                account: 1000,
                before: 1,
                after: 1,
                step: LoginStep::Identify,
            },
        )
        .unwrap();
        for (request, ceiling) in [(add, 240), (unlock, 120), (request("main"), 120)] {
            for seconds in [1, ceiling] {
                assert!(
                    Prepared::with_time(request.clone(), 800, 600, 3200, Some(seconds)).is_ok()
                );
            }
            for seconds in [0, ceiling + 1] {
                assert_eq!(
                    Prepared::with_time(request.clone(), 800, 600, 3200, Some(seconds))
                        .err()
                        .unwrap(),
                    "invalid trusted operation time budget"
                );
            }
        }
    }

    /// The menu's rows where the update and setup oracles read them on a
    /// 1280x800 output, as before `L`: the title at 276, the selections
    /// from 312 and `I` at 456, 36 apart; `K` below `I`, `L` below `K` at
    /// 528, and the last row, which every other screen keeps at 528, one
    /// lower at 564. On 800x600 the nine rows start at 176, and on 320x200,
    /// at single scale, the fit rule moves them up to 0.
    #[test]
    fn the_menu_keeps_its_rows_and_adds_k_and_l_below_i() {
        const MENU: &[&str] = &[
            "TD SECURE ATTENTION",
            "U: UNLOCK  R: RECOVERY TOKEN",
            "E: ENROLL TWO TOKENS (HAVE BOTH READY)",
            "X: ENROLL WITHOUT RECOVERY - LOSS IS FINAL",
            "W: REVIEW PENDING CREDENTIAL WRITE",
            "I: REVIEW PENDING SYSTEM INSTALLATION",
            "K: LOGIN KEYS",
            "L: LOCK SCREEN",
            "ESC TO RETURN",
        ];
        for (width, height, top, scale) in
            [(1280, 800, 276, 2), (800, 600, 176, 2), (320, 200, 0, 1)]
        {
            let stride = width * 4;
            let mut painted = vec![0; stride * height];
            paint(
                &mut painted,
                width,
                height,
                stride,
                false,
                false,
                Notice::Menu,
            );
            let mut expected = vec![0; stride * height];
            let bounds = (0, 0, width, height);
            ui::fill(
                &mut expected,
                width,
                height,
                stride,
                bounds,
                [0x28, 0x20, 0x18, 0],
            );
            for (index, text) in MENU.iter().enumerate() {
                ui::draw_text_clipped(
                    &mut expected,
                    width,
                    height,
                    stride,
                    24,
                    top + index * 18 * scale,
                    scale,
                    text,
                    [0xff, 0xff, 0xff, 0],
                    bounds,
                );
            }
            assert!(painted == expected, "{width}x{height}");
        }
        // Nine doubled rows, the last row's foot included, are 302 tall
        // and keep their top on 800 and 600 lines; on 200 single rows fit
        // only from the top.
        assert_eq!(rows_top(800, 8 * 36 + 14), 276);
        assert_eq!(rows_top(600, 8 * 36 + 14), 176);
        assert_eq!(rows_top(200, 8 * 18 + 7), 0);
    }

    /// HEAD's painter, before narrow outputs: every row doubled, 36 apart
    /// from `(height - 248) / 2`, padded to seven rows and then the last.
    /// The menu, eight rows since `L`, has no padding, so only its last row
    /// moves.
    fn unchanged(width: usize, height: usize, draining: bool, notice: Notice) -> Vec<u8> {
        let stride = width * 4;
        let mut frame = vec![0; stride * height];
        let bounds = (0, 0, width, height);
        ui::fill(
            &mut frame,
            width,
            height,
            stride,
            bounds,
            [0x28, 0x20, 0x18, 0],
        );
        let mut rows = vec![String::from("TD SECURE ATTENTION")];
        if draining && notice != Notice::Returning {
            rows.push("CANCELLING REQUEST".into());
        } else {
            rows.extend(notice_rows(notice));
        }
        if rows.len() < 7 {
            rows.resize(7, String::new());
        }
        rows.push(
            if draining {
                "RELEASE KEYS AND BUTTONS"
            } else {
                "ESC TO RETURN"
            }
            .into(),
        );
        let top = (height - 248) / 2;
        for (index, text) in rows.iter().enumerate() {
            ui::draw_text_clipped(
                &mut frame,
                width,
                height,
                stride,
                24,
                top + index * 36,
                2,
                text,
                [0xff, 0xff, 0xff, 0],
                bounds,
            );
        }
        frame
    }

    /// Every screen on every common output, where the old layout fits, is
    /// drawn exactly as at HEAD but for the menu's `L` and the last row
    /// below it, so the boot oracles' rows do not move: at 1280x800 the
    /// title stays at 276, the notice at 312 and `I` at 456, and every
    /// other screen's last row at 528.
    #[test]
    fn rows_do_not_move_where_the_old_layout_fits() {
        for (width, height) in [
            (800, 600),
            (1024, 768),
            (1280, 720),
            (1280, 800),
            (1280, 1024),
            (1366, 768),
            (1440, 900),
            (1600, 900),
            (1920, 1080),
            (1920, 1200),
            (2560, 1440),
        ] {
            for draining in [false, true] {
                for notice in SCREENS {
                    // Newer than HEAD's painter, which clipped it where it
                    // is now wrapped; where it fits one row it is the same.
                    if *notice == Notice::Returning && width < 24 + RETURNING_NOTICE.len() * 16 {
                        continue;
                    }
                    let stride = width * 4;
                    let mut frame = vec![0; stride * height];
                    paint(&mut frame, width, height, stride, draining, false, *notice);
                    assert!(
                        frame == unchanged(width, height, draining, *notice),
                        "{width}x{height} {notice:?}"
                    );
                }
            }
        }
        // The eight doubled rows end 266 below the title, inside 800.
        assert_eq!(rows_top(800, 7 * 36 + 14), 276);
        // Rows the old place cannot hold move up only as far as they must.
        assert_eq!(rows_top(800, 600), 200);
        assert_eq!(rows_top(250, 7 * 36 + 14), 0);
        assert_eq!(rows_top(200, 7 * 18 + 7), 0);
    }

    const SCREENS: &[Notice] = &[
        Notice::Menu,
        Notice::Pending,
        Notice::Unlocked,
        Notice::Stored,
        Notice::NoWrite,
        Notice::NoInstall,
        Notice::UpdateRefused,
        Notice::Installed,
        Notice::Returning,
        Notice::Enrolled,
        Notice::Unenrolled,
        Notice::Unavailable,
        Notice::Failed,
        Notice::Busy,
        Notice::LoginKeys,
        Notice::NotAvailable,
        Notice::Removing {
            keys: 8,
            chosen: 0xff,
        },
        Notice::Removing { keys: 1, chosen: 0 },
        Notice::Login(&["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"]),
        Notice::Login(&["A RETAINED SYSTEM CANNOT READ KEYS"]),
        Notice::Login(&["THE OPERATION FAILED"]),
        Notice::Uncertain(&["LOGIN KEY STATE UNAVAILABLE:", "DIRECTORY DAMAGED"]),
    ];

    /// Every screen's rows, wrapped where the output needs it, inside the
    /// output from 24 pixels in, at the smallest output a prompt takes and
    /// at the oracles'. A row that fits is drawn as it is.
    #[test]
    fn every_screen_fits_the_output_it_is_drawn_on() {
        for (width, height) in [(320, 200), (799, 600), (800, 600), (1280, 800)] {
            let (scale, columns) = layout(width, height);
            for draining in [false, true] {
                for notice in SCREENS.iter().copied() {
                    let stride = width * 4;
                    let mut frame = vec![0; stride * height];
                    paint(&mut frame, width, height, stride, draining, false, notice);
                    // The painted extent: inked rows and columns.
                    let ink = |x: usize, y: usize| frame[y * stride + x * 4] == 0xff;
                    let lowest = (0..height)
                        .rev()
                        .find(|y| (0..width).any(|x| ink(x, *y)))
                        .unwrap();
                    let right = (0..width)
                        .rev()
                        .find(|x| (0..height).any(|y| ink(*x, y)))
                        .unwrap();
                    assert!(right < width - 24, "{width} {notice:?} {right}");
                    // The last row, the return line, whose first glyph inks
                    // its foot, is whole and on screen.
                    assert!(lowest < height - 1, "{width}x{height} {notice:?}");
                    for row in notice_rows(notice) {
                        for part in wrap(&row, columns) {
                            assert!(part.len() <= columns, "{part}");
                        }
                        if row.len() <= columns {
                            assert_eq!(wrap(&row, columns), std::slice::from_ref(&row));
                        }
                    }
                }
            }
            assert_eq!(scale, if width >= 800 { 2 } else { 1 });
        }
        // At 320 pixels a row has 45 columns; at 800, 62.
        assert_eq!(layout(320, 200), (1, 45));
        assert_eq!(layout(800, 600), (2, 62));
        assert_eq!(
            wrap("NO READY CREDENTIAL WRITE - RUN TD-SECRET SET FIRST", 45),
            ["NO READY CREDENTIAL WRITE - RUN TD-SECRET SET", "FIRST"]
        );
        assert_eq!(
            wrap("U: UNLOCK  R: RECOVERY TOKEN", 12),
            ["U: UNLOCK", "R: RECOVERY", "TOKEN"]
        );
        assert_eq!(wrap("ABCDEFGH", 3), ["ABC", "DEF", "GH"]);
        assert_eq!(wrap("", 3), [""]);
    }

    #[test]
    fn login_screens_and_ends_fit_and_draw_every_glyph() {
        let notices = [
            Notice::LoginKeys,
            Notice::NotAvailable,
            Notice::Removing {
                keys: 8,
                chosen: 0xff,
            },
            Notice::Removing { keys: 1, chosen: 0 },
            Notice::Login(&["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"]),
            Notice::Login(&["PIN BLOCKED; USE ANOTHER KEY"]),
            Notice::Uncertain(&["TIMED OUT"]),
        ];
        for notice in notices {
            let rows = notice_rows(notice);
            assert!(!rows.is_empty() && rows.len() < MENU_ROWS, "{notice:?}");
            for row in &rows {
                // Unwrapped on the narrowest output.
                assert!(row.len() <= layout(320, 200).1, "{row}");
                assert!(row.bytes().all(ui::is_mapped), "{row}");
            }
        }
        assert_eq!(
            notice_rows(Notice::Removing {
                keys: 3,
                chosen: 0b101
            }),
            ["REMOVE: PRESS 1 TO 3 THEN ENTER", "SELECTED: 1 3"]
        );
        assert_eq!(
            notice_rows(Notice::Uncertain(&["TIMED OUT"])),
            ["RESULT UNCERTAIN", "TIMED OUT"]
        );
        assert_eq!(
            notice_rows(Notice::LoginKeys),
            [
                "LOGIN KEYS",
                "1: ENROLL ONE KEY",
                "2: ENROLL TWO KEYS",
                "A: ADD A KEY",
                "D: REMOVE KEYS"
            ]
        );
    }

    /// After a login unlock's success the drain keeps the notice above
    /// `RELEASE KEYS AND BUTTONS`: it cancels nothing. Not draining, the
    /// screen is the ordinary one.
    #[test]
    fn an_unlocked_lifetimes_drain_keeps_its_success_notice() {
        let (width, height, stride) = (1280, 800, 1280 * 4);
        let notice = Notice::Login(&["SESSION UNLOCKED"]);
        let mut painted = vec![0x55; stride * height];
        paint(&mut painted, width, height, stride, true, true, notice);
        let mut expected = vec![0; stride * height];
        let bounds = (0, 0, width, height);
        ui::fill(
            &mut expected,
            width,
            height,
            stride,
            bounds,
            [0x28, 0x20, 0x18, 0],
        );
        for (row, text) in [
            (0, "TD SECURE ATTENTION"),
            (1, "SESSION UNLOCKED"),
            (7, "RELEASE KEYS AND BUTTONS"),
        ] {
            ui::draw_text_clipped(
                &mut expected,
                width,
                height,
                stride,
                24,
                276 + row * 36,
                2,
                text,
                [0xff, 0xff, 0xff, 0],
                bounds,
            );
        }
        assert!(painted == expected);
        let mut ordinary = vec![0; stride * height];
        paint(&mut painted, width, height, stride, false, true, notice);
        paint(&mut ordinary, width, height, stride, false, false, notice);
        assert!(painted == ordinary);
    }

    /// Root's `9a` for `state`, for the primary `tester`, as the worker
    /// keeps it.
    fn answer(state: &[u8], hostname: &str) -> crate::authority::Answer {
        let login = crate::authority::Login::default();
        let host = u8::try_from(hostname.len()).unwrap();
        let bytes = [
            &[0x9a][..],
            state,
            b"\x06tester",
            &[host],
            hostname.as_bytes(),
            &[0],
        ]
        .concat();
        login.answer(&bytes).unwrap();
        login.current().unwrap()
    }

    /// The lock surface's background with `rows` drawn at their tops.
    fn lock_drawn(width: usize, height: usize, scale: usize, rows: &[(usize, &str)]) -> Vec<u8> {
        let stride = width * 4;
        let mut expected = vec![0; stride * height];
        let bounds = (0, 0, width, height);
        ui::fill(
            &mut expected,
            width,
            height,
            stride,
            bounds,
            [0x28, 0x20, 0x18, 0],
        );
        for (top, text) in rows {
            ui::draw_text_clipped(
                &mut expected,
                width,
                height,
                stride,
                24,
                *top,
                scale,
                text,
                [0xff, 0xff, 0xff, 0],
                bounds,
            );
        }
        expected
    }

    fn lock_painted(width: usize, height: usize, rows: &[String]) -> Vec<u8> {
        let stride = width * 4;
        let mut painted = vec![0x55; stride * height];
        paint_lock(&mut painted, width, height, stride, rows);
        assert!(painted.chunks(4).all(|pixel| pixel[0] != 0x55));
        painted
    }

    /// The lock surface is the attention screen's chrome over the whole
    /// output, so no client pixel survives it: the `1a` answer's hostname
    /// and username, uppercase, then `LOCKED` and the state's rows, from
    /// the menu's place down. On 1280x800 a one-row hostname is at 276,
    /// the username at 312, `LOCKED` at 348 and the next row at 384; an
    /// empty hostname draws no row.
    #[test]
    fn the_lock_surface_draws_the_names_then_the_states_rows() {
        let enrolled = [1, 2, 1, 1, 1, 1, 2, 2, 2, 2];
        let unlock = "PRESS CTRL+ALT+ESC TO UNLOCK";
        let cases: &[(&[u8], &str, &[&str])] = &[
            (
                &enrolled,
                "td-laptop",
                &["TD-LAPTOP", "TESTER", "LOCKED", unlock],
            ),
            (
                &[2, 0x0a],
                "td-laptop",
                &[
                    "TD-LAPTOP",
                    "TESTER",
                    "LOCKED",
                    "LOGIN KEY STATE UNAVAILABLE:",
                    "DIRECTORY DAMAGED",
                ],
            ),
            (
                &[2, 0x0b],
                "td",
                &[
                    "TD",
                    "TESTER",
                    "LOCKED",
                    "LOGIN KEY STATE UNAVAILABLE:",
                    "RECORD DAMAGED",
                ],
            ),
            (
                &[2, 0x0c],
                "td",
                &[
                    "TD",
                    "TESTER",
                    "LOCKED",
                    "LOGIN KEY STATE UNAVAILABLE:",
                    "STATE COULD NOT BE READ",
                ],
            ),
            (
                &[0],
                "td-laptop",
                &["TD-LAPTOP", "TESTER", "LOCKED", "NO LOGIN KEYS ENROLLED"],
            ),
            (&enrolled, "", &["TESTER", "LOCKED", unlock]),
        ];
        for (state, hostname, expected) in cases {
            let rows = lock_rows(Some(&answer(state, hostname)));
            assert_eq!(rows, *expected);
            let placed: Vec<(usize, &str)> = expected
                .iter()
                .enumerate()
                .map(|(index, row)| (276 + 36 * index, *row))
                .collect();
            assert!(
                lock_painted(1280, 800, &rows) == lock_drawn(1280, 800, 2, &placed),
                "{expected:?}"
            );
            // Every row but the hostname fits the narrowest output whole,
            // and every glyph is the font's.
            for (index, row) in rows.iter().enumerate() {
                assert!(row.bytes().all(ui::is_mapped), "{row}");
                if index > 0 || hostname.is_empty() {
                    assert!(row.len() <= layout(320, 200).1, "{row}");
                }
            }
        }
        // With no answer, which no paired generation has once connected.
        assert_eq!(lock_rows(None), ["LOCKED"]);
        assert!(
            lock_painted(1280, 800, &lock_rows(None))
                == lock_drawn(1280, 800, 2, &[(276, "LOCKED")])
        );
    }

    /// A 63-byte hostname has no space to wrap at, so where it is wider
    /// than the output's columns it breaks at the last column, and the
    /// username and the state's rows follow below it.
    #[test]
    fn a_63_byte_hostname_wraps_at_the_last_column() {
        let hostname = format!("{}.{}", "a".repeat(31), "b".repeat(31));
        assert_eq!(hostname.len(), 63);
        let upper = hostname.to_ascii_uppercase();
        let rows = lock_rows(Some(&answer(&[2, 0x0c], &hostname)));
        assert_eq!(rows[0], upper);
        let cause = ["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"];
        // 1280x800 has 102 columns: one row.
        let mut placed = vec![(276, upper.as_str()), (312, "TESTER"), (348, "LOCKED")];
        placed.extend([(384, cause[0]), (420, cause[1])]);
        assert!(lock_painted(1280, 800, &rows) == lock_drawn(1280, 800, 2, &placed));
        // 800x600 has 62: the last byte wraps, from the menu's place, 176.
        assert_eq!(layout(800, 600), (2, 62));
        let placed = [
            (176, &upper[..62]),
            (212, &upper[62..]),
            (248, "TESTER"),
            (284, "LOCKED"),
            (320, cause[0]),
            (356, cause[1]),
        ];
        assert!(lock_painted(800, 600, &rows) == lock_drawn(800, 600, 2, &placed));
        // The smallest output a prompt takes has 45, at single scale, and
        // the block moves up to fit, as little as it must: to the top.
        assert_eq!(layout(320, 200), (1, 45));
        let placed = [
            (0, &upper[..45]),
            (18, &upper[45..]),
            (36, "TESTER"),
            (54, "LOCKED"),
            (72, cause[0]),
            (90, cause[1]),
        ];
        assert!(lock_painted(320, 200, &rows) == lock_drawn(320, 200, 1, &placed));
    }

    // The PIN field.

    /// Every login PIN step consent accepts, each operation's at its
    /// widest: the largest account, fingerprints and retries, every count
    /// of keys before and after, and every removal of a leading run.
    fn pin_steps() -> Vec<Request> {
        use crate::authority::consent::{LoginStep, Slot, LOGIN_KEYS};
        let key = [0xff; 4];
        let steps = [
            LoginStep::Authorize { key, retries: 255 },
            LoginStep::Create { retries: 255 },
            LoginStep::Prove { key, retries: 255 },
            LoginStep::Repeat { key, retries: 255 },
            LoginStep::Unlock { key, retries: 255 },
        ];
        let mut operations = Vec::new();
        for before in 0..=LOGIN_KEYS {
            for after in 0..=LOGIN_KEYS {
                for step in steps {
                    let account = 65533;
                    operations.push(Operation::LoginUnlock {
                        account,
                        before,
                        after,
                        step,
                    });
                    operations.push(Operation::LoginAdd {
                        account,
                        before,
                        after,
                        step,
                    });
                    for created in 1..=2 {
                        operations.push(Operation::LoginEnroll {
                            account,
                            before,
                            after,
                            key: created,
                            step,
                        });
                    }
                    for removed in 1..=before {
                        operations.push(Operation::LoginRemove {
                            account,
                            before,
                            after,
                            removed: (1..=removed)
                                .map(|position| Slot { position, key })
                                .collect(),
                            step,
                        });
                    }
                }
            }
        }
        let requests: Vec<Request> = operations
            .into_iter()
            .filter_map(|operation| Request::new([1; 32], 65533, operation).ok())
            .collect();
        assert!(requests
            .iter()
            .all(|request| request.login_step().is_some_and(LoginStep::asks_pin)));
        requests
    }

    /// The prompt with its whole time line, the longest a login prompt shows.
    fn pin_prompt(request: &Request, width: usize, height: usize) -> Option<Prepared> {
        let ceiling = request.login_ceiling().unwrap().as_secs();
        Prepared::with_time(request.clone(), width, height, width * 4, Some(ceiling)).ok()
    }

    /// The field drawn independently of `paint_field`: the prompt's pixels,
    /// then the row in the prompt's font and the masks, a row gap below the
    /// prompt's last row.
    fn expected_field(prompt: &Prepared, field: Field) -> Vec<u8> {
        let (width, height, stride) = prompt.geometry;
        let font = crate::font::pinned().unwrap();
        let scale = prompt.scale;
        let mut expected = prompt.pixels.clone();
        let white = [0xff, 0xff, 0xff, 0];
        let text = prompt.foot + 4 * scale;
        let (line, count) = match field {
            Field::Pin(count) => ("ENTER THE PIN FOR THIS KEY", usize::from(count)),
            Field::Touch => ("TOUCH YOUR KEY", 0),
        };
        for (column, character) in line.chars().enumerate() {
            let glyph = font.index(character);
            for y in 0..font.height() {
                for x in 0..font.width() {
                    if font.pixel(glyph, x, y) {
                        let left = 24 + (column * font.width() + x) * scale;
                        let rect = (left, text + y * scale, scale, scale);
                        ui::fill(&mut expected, width, height, stride, rect, white);
                    }
                }
            }
        }
        let masks = text + font.height() * scale + 4 * scale;
        for column in 0..count {
            let rect = (24 + column * 4 * scale, masks, 3 * scale, 3 * scale);
            ui::fill(&mut expected, width, height, stride, rect, white);
        }
        expected
    }

    /// The field goes beneath the presented step's prompt and leaves it
    /// exactly as presented: every prompt row, the operation, the key's
    /// fingerprint, its retries and the time line, keeps its pixels. No
    /// PIN step's prompt fits 320x200, so none is shown there; at 320x240,
    /// the narrowest output that shows one, and at 1280x800 every PIN step
    /// shown holds the field, the tallest included, and at 1280x800 every
    /// one is shown.
    #[test]
    fn the_pin_field_leaves_the_prompt_untouched_beneath_every_pin_step() {
        let steps = pin_steps();
        assert!(steps
            .iter()
            .all(|step| pin_prompt(step, 320, 200).is_none()));
        for (width, height) in [(320, 240), (1280, 800)] {
            let prompts: Vec<Prepared> = steps
                .iter()
                .filter_map(|step| pin_prompt(step, width, height))
                .collect();
            assert!(prompts.iter().all(Prepared::holds_field));
            if width == 1280 {
                assert_eq!(prompts.len(), steps.len());
            }
            let tallest = prompts.iter().max_by_key(|prompt| prompt.foot).unwrap();
            let gap = 4 * tallest.scale;
            for field in (0..=63).map(Field::Pin).chain([Field::Touch]) {
                let mut painted = vec![0; width * 4 * height];
                assert!(tallest.paint_field(&mut painted, width, height, width * 4, field));
                // The prompt's rows, and the row gap below them, untouched.
                let rows = (tallest.foot + gap) * width * 4;
                assert!(painted[..rows] == tallest.pixels[..rows], "{field:?}");
                // Beneath them only the field's row and its masks.
                assert!(painted == expected_field(tallest, field), "{field:?}");
                // A row gap's margin below the field, and 24 pixels right.
                let ink = |x: usize, y: usize| painted[(y * width + x) * 4] == 0xff;
                let margin = height - gap;
                assert!((margin..height).all(|y| (0..width).all(|x| !ink(x, y))));
                assert!((0..height).all(|y| (width - 24..width).all(|x| !ink(x, y))));
            }
            // Each step's prompt with its widest field.
            for prompt in &prompts {
                let mut painted = vec![0; width * 4 * height];
                assert!(prompt.paint_field(&mut painted, width, height, width * 4, Field::Pin(63)));
                assert!(painted == expected_field(prompt, Field::Pin(63)));
            }
        }
        // At 320x240 the PIN steps shown leave a 32-pixel band, one more
        // than the field needs; taller ones are refused whole, as before.
        let shown: Vec<Prepared> = steps
            .iter()
            .filter_map(|step| pin_prompt(step, 320, 240))
            .collect();
        assert!(shown.iter().all(|prompt| 240 - prompt.foot == 32));
        assert!(shown.len() < steps.len());
    }

    /// Where the band beneath a PIN step's prompt cannot hold the field
    /// whole, the prompt is not shown at all, so the operation fails before
    /// any PIN reaches the key; the field is never squeezed and
    /// the prompt never moves. The paint keeps the check as a backstop. A
    /// step that asks no PIN has no field.
    #[test]
    fn a_pin_step_without_room_for_its_field_is_not_presented() {
        const NO_FIELD: &str = "output cannot hold the PIN field beneath this prompt";
        const NO_PROMPT: &str = "output cannot hold every trusted prompt argument";
        let steps = pin_steps();
        let widest = steps.iter().max_by_key(|step| step.lines().len()).unwrap();
        let ceiling = widest.login_ceiling().unwrap().as_secs();
        let (mut held, mut refused) = (0, 0);
        for height in 200..=600 {
            match Prepared::with_time(widest.clone(), 320, height, 1280, Some(ceiling)) {
                Ok(prompt) => {
                    assert!(height - prompt.foot >= 4 + 16 + 4 + 3 + 4, "{height}");
                    assert!(prompt.holds_field());
                    held += 1;
                }
                Err(error) if error == NO_FIELD => refused += 1,
                Err(error) => assert_eq!(error, NO_PROMPT),
            }
        }
        assert!(held > 0 && refused > 0);
        // At 800x600 the five tallest, removals of five keys or more, are
        // refused for their field; every other is shown with room for it.
        let refused: Vec<&Request> = steps
            .iter()
            .filter(|step| {
                let ceiling = step.login_ceiling().unwrap().as_secs();
                match Prepared::with_time((*step).clone(), 800, 600, 3200, Some(ceiling)) {
                    Ok(prompt) => {
                        assert!(prompt.holds_field());
                        false
                    }
                    Err(error) => {
                        assert_eq!(error, NO_FIELD);
                        true
                    }
                }
            })
            .collect();
        assert_eq!(refused.len(), 5);
        assert!(refused.iter().all(|step| matches!(
            step.operation(),
            Operation::LoginRemove { removed, .. } if removed.len() >= 5
        )));
        // The backstop: a prompt whose band is too small paints no field.
        let mut prompt = pin_prompt(&steps[0], 800, 600).unwrap();
        let mut painted = vec![0; 800 * 600 * 4];
        assert!(prompt.paint_field(&mut painted, 800, 600, 3200, Field::Pin(63)));
        // More masks than a PIN has bytes are refused.
        assert!(!prompt.paint_field(&mut painted, 800, 600, 3200, Field::Pin(64)));
        prompt.foot = 600 - 61;
        assert!(!prompt.holds_field());
        assert!(!prompt.paint_field(&mut painted, 800, 600, 3200, Field::Touch));
        let set = Prepared::new(request("main"), 800, 600, 3200).unwrap();
        assert!(!set.holds_field());
        assert!(!set.paint_field(&mut painted, 800, 600, 3200, Field::Touch));
    }
}
