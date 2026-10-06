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
    Installed,
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

/// A store operation's or installation review's ceiling, in seconds; a
/// login operation's is its own (td-authd's `login_ceiling`).
const OPERATION_SECONDS: u64 = 120;

/// Rasterize once, retaining the exact immutable description beside its pixels.
#[derive(Debug)]
pub(crate) struct Prepared {
    request: Request,
    pixels: Vec<u8>,
    geometry: (usize, usize, usize),
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
        Ok(Self {
            request,
            pixels,
            geometry: (width, height, stride),
        })
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
}

/// Display-only pixels: ordinary scene rendering never calls this painter.
pub(crate) fn paint(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    draining: bool,
    notice: Notice,
) {
    let bounds = (0, 0, width, height);
    ui::fill(frame, width, height, stride, bounds, [0x28, 0x20, 0x18, 0]);
    let (scale, columns) = layout(width, height);
    let mut rows = vec![String::from("TD SECURE ATTENTION")];
    if draining {
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

/// The title and the menu's rows: the last row is drawn below them.
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

/// A notice's rows below the title. The menu's rows are fixed in place, and
/// `K` comes below `I`, so the boot oracles' rows do not move.
fn notice_rows(notice: Notice) -> Vec<String> {
    let first = match notice {
        Notice::Menu => "U: UNLOCK  R: RECOVERY TOKEN",
        Notice::Pending => "PREPARING REQUEST",
        Notice::NoInstall => "NO INSTALLATION IS READY TO REVIEW",
        Notice::Installed => "SYSTEM INSTALLED - RESTART TO BOOT IT",
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
        use crate::authority::consent::{Label, DISK_NAME_BYTES, HOSTNAME_BYTES, USERNAME_BYTES};
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
            },
        )
        .unwrap();
        assert!(Prepared::with_time(typical, 800, 600, 3200, Some(120)).is_ok());
    }

    /// Consent keeps every login row within `PROMPT_COLUMNS`, this renderer's
    /// columns at its narrowest accepted width, so wrapping never splits a
    /// fingerprint; the widest login prompt is shown whole from 800x600.
    #[test]
    fn login_rows_fit_the_narrowest_prompt_and_an_800_by_600_output() {
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
        assert!(Prepared::with_time(widest, 800, 600, 3200, Some(120)).is_ok());
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
    /// 1280x800 output, as at HEAD: the title at 276, the selections from
    /// 312 and `I` at 456, 36 apart; `K` below `I`, and the last row below
    /// that.
    #[test]
    fn the_menu_keeps_its_rows_and_adds_k_below_i() {
        let (width, height, stride) = (1280, 800, 1280 * 4);
        let mut painted = vec![0; stride * height];
        paint(&mut painted, width, height, stride, false, Notice::Menu);
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
        for (top, text) in [
            (276, "TD SECURE ATTENTION"),
            (312, "U: UNLOCK  R: RECOVERY TOKEN"),
            (348, "E: ENROLL TWO TOKENS (HAVE BOTH READY)"),
            (384, "X: ENROLL WITHOUT RECOVERY - LOSS IS FINAL"),
            (420, "W: REVIEW PENDING CREDENTIAL WRITE"),
            (456, "I: REVIEW PENDING SYSTEM INSTALLATION"),
            (492, "K: LOGIN KEYS"),
            (528, "ESC TO RETURN"),
        ] {
            ui::draw_text_clipped(
                &mut expected,
                width,
                height,
                stride,
                24,
                top,
                2,
                text,
                [0xff, 0xff, 0xff, 0],
                bounds,
            );
        }
        assert!(painted == expected);
    }

    /// HEAD's painter, before narrow outputs: every row doubled, 36 apart
    /// from `(height - 248) / 2`, padded to seven rows and then the last.
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
        if draining {
            rows.push("CANCELLING REQUEST".into());
        } else {
            rows.extend(notice_rows(notice));
        }
        rows.resize(MENU_ROWS, String::new());
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
    /// drawn exactly as at HEAD, so the boot oracles' rows do not move: at
    /// 1280x800 the title stays at 276, the notice at 312 and `I` at 456.
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
                    let stride = width * 4;
                    let mut frame = vec![0; stride * height];
                    paint(&mut frame, width, height, stride, draining, *notice);
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
        Notice::Installed,
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
                    paint(&mut frame, width, height, stride, draining, notice);
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
}
