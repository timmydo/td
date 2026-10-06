//! Drives the live installer wizard with physical keys: once the live
//! session says the wizard is ready, each step waits until td-setup's boot
//! evidence says it showed a page state with every one of the step's
//! fields, after every state it acted on, and then acts through QEMU's
//! emulated keyboard. A step presses its keys, each alone, or consents the
//! way a person would: the secure attention chord, the menu's `I`, and
//! Enter only once the compositor's prompt shows exactly the expected
//! whole-disk rows, then Escape from the installed notice back to the
//! wizard. The guest learns nothing from the host but keys.

use super::recovery_screen::{KeyGlyphs, DIGITS};
use super::update::{menu_pixels_match, menu_row_matches, ppm, read_capture, row_matches};
use super::{qmp_deadline, qmp_json_path, ConsoleEvidence, Qmp, QMP_IO_TIMEOUT, VT_KEY_PACE};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How long a state may take to follow its key, or the ready marker, under
/// TCG: a review reads the medium's deployment, the rest is one frame.
pub(crate) const STEP_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a consented installation may take under TCG, until the
/// compositor says it finished.
pub(crate) const INSTALL_TIMEOUT: Duration = Duration::from_secs(1800);
/// How long the secure attention menu, then the prompt, may take to draw.
const ATTENTION_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the screen is captured while the installation runs.
const NOTICE_INTERVAL: Duration = Duration::from_secs(2);
/// The attention menu's notice row (td-compositor/src/attention.rs): the
/// second row of a 1280x800 menu.
const NOTICE_TOP: usize = (800 - 248) / 2 + 36;
const INSTALLED_NOTICE: &str = "SYSTEM INSTALLED - RESTART TO BOOT IT";
/// The attention menu's title row, the first.
const TITLE_TOP: usize = (800 - 248) / 2;
/// What a committed device-bound installation shows before the screen
/// closes by itself (td-compositor/src/attention.rs).
const RETURNING_NOTICE: &str = "INSTALLING - RETURNING TO SETUP FOR THE RECOVERY KEY";
/// How often the screen is captured from Enter until it has closed after
/// the returning notice, and how long the close may take once it showed.
const RETURN_INTERVAL: Duration = Duration::from_millis(500);
const RETURN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long after the screen is seen closed before the next key: twice
/// the compositor's settle window after a close it made itself
/// (`SELF_CLOSE_SETTLE`, td-compositor/src/input.rs), which discards a key
/// pressed inside it.
const CLOSED_QUIET: Duration = Duration::from_millis(200);
/// Begin the lines a failed drive repeats from the whole console: the
/// compositor's attention evidence and errors, and td-setup's states.
const MARKER_PREFIXES: &[&str] = &["TD-ATTENTION-", "td-compositor: ", "TD-SETUP-SHOWN "];
/// How many of those lines a failure repeats, the last ones.
const MARKERS_KEPT: usize = 16;
const FAILED_NOTICE: &str = "REQUEST FAILED";
const NO_INSTALL_NOTICE: &str = "NO INSTALLATION IS READY TO REVIEW";

/// How long the recovery-key page may take to show a key the oracle reads,
/// once td-setup said it showed it, and how often it is captured.
const KEY_SHOWN_TIMEOUT: Duration = Duration::from_secs(60);
const KEY_SHOWN_INTERVAL: Duration = Duration::from_millis(500);

/// The disposable target disk's serial, as the wizard's destinations see it.
pub(crate) const TARGET_SERIAL: &str = "td-setup-target";

/// The recovery key as the oracle read it from the display, its digits
/// zeroed on drop. Only `Act::ReadKey` fills it, and `Act::TypeKey` and
/// the host's later legs use it.
#[derive(Default)]
pub(crate) struct ReadKey {
    digits: Option<Box<[u8; DIGITS]>>,
}

impl Drop for ReadKey {
    fn drop(&mut self) {
        if let Some(digits) = self.digits.as_mut() {
            digits.fill(0);
            std::hint::black_box(&digits);
        }
    }
}

impl ReadKey {
    /// The 48 digits read, if they have been.
    pub(crate) fn digits(&self) -> Option<&[u8; DIGITS]> {
        self.digits.as_deref()
    }

    /// The digits read, moved out.
    pub(crate) fn take(&mut self) -> Option<Box<[u8; DIGITS]>> {
        self.digits.take()
    }
}

/// Where the read key is kept between the steps that read and type it.
pub(crate) type KeyCell = Rc<RefCell<ReadKey>>;

pub(crate) enum Act {
    /// Press each key alone, in turn.
    Keys(Vec<&'static str>),
    /// Consent through secure attention once the prompt shows exactly
    /// these rows, then return from the installed notice.
    Consent(Vec<String>),
    /// Consent as `Consent` does, then require the compositor's returning
    /// notice and its own close of the screen, pressing nothing: a
    /// device-bound installation finishes only after its recovery key is
    /// typed back in the wizard, so no installed notice comes first.
    ConsentThenReturn(Vec<String>),
    /// Open the prompt as `Consent` does, then press Escape on it before
    /// consent, with nothing written, and require the screen closed.
    Decline(Vec<String>),
    /// Read the recovery key from the display with these references into
    /// the cell, then press Return to type it back.
    ReadKey(Rc<KeyGlyphs>, KeyCell),
    /// Type the key the cell holds on the keyboard, then Return.
    TypeKey(KeyCell),
}

/// Once td-setup says it showed a state holding every space-separated
/// field of `shown`, within `within` of the previous step's act, act.
pub(crate) struct SetupStep {
    pub(crate) shown: String,
    pub(crate) act: Act,
    /// The target must have taken no write when the state is shown.
    pub(crate) untouched: bool,
    pub(crate) within: Duration,
}

impl SetupStep {
    pub(crate) fn press(shown: String, key: &'static str) -> Self {
        Self {
            shown,
            act: Act::Keys(vec![key]),
            untouched: false,
            within: STEP_TIMEOUT,
        }
    }
}

const LETTERS: [&str; 26] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r", "s",
    "t", "u", "v", "w", "x", "y", "z",
];

/// The keys that type `text` on td's US keymap: lower-case letters, `-`
/// and `/`, the closed set the wizard's oracle needs.
pub(crate) fn typed(text: &str) -> Result<Vec<&'static str>, String> {
    text.bytes()
        .map(|byte| match byte {
            b'a'..=b'z' => LETTERS
                .get(usize::from(byte - b'a'))
                .copied()
                .ok_or_else(|| "letter outside the alphabet".to_string()),
            b'-' => Ok("minus"),
            b'/' => Ok("slash"),
            _ => Err(format!("{text:?} escapes the wizard oracle's typed set")),
        })
        .collect()
}

/// Whether `state` holds every field of `wanted`, each whole.
pub(super) fn matches(state: &str, wanted: &str) -> bool {
    let fields: Vec<&str> = state.split(' ').collect();
    wanted.split(' ').all(|field| fields.contains(&field))
}

/// How long the guest may take, once the wizard's restart is asked for,
/// to tear down in order and leave QEMU.
pub(super) const RESTART_TIMEOUT: Duration = Duration::from_secs(600);

/// After the script's last key: td-setup saying its restart was refused or
/// can no longer be asked for, a lost evidence line, or a guest still up
/// past `RESTART_TIMEOUT`, fails the boot.
pub(super) fn restart_progress(
    evidence: &ConsoleEvidence,
    asked: Instant,
    now: Instant,
) -> Result<(), String> {
    if let Some(lost) = &evidence.td_setup_lost {
        return Err(lost.clone());
    }
    if let Some((_, state)) = evidence.td_setup_shown.iter().find(|(_, shown)| {
        matches(shown, "page=complete end=refused")
            || matches(shown, "page=complete end=unavailable")
    }) {
        return Err(format!("td-setup showed {state:?}"));
    }
    if now.saturating_duration_since(asked) > RESTART_TIMEOUT {
        return Err(format!(
            "the guest did not restart within {}s",
            RESTART_TIMEOUT.as_secs()
        ));
    }
    Ok(())
}

const DIGIT_KEYS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// Whether `key` is one the wizard's oracle may press.
pub(super) fn setup_key(key: &str) -> bool {
    LETTERS.contains(&key)
        || DIGIT_KEYS.contains(&key)
        || matches!(
            key,
            "minus" | "slash" | "tab" | "down" | "ret" | "esc" | "spc"
        )
}

/// The key that types `byte` at a console on td's US keymap: a digit, a
/// lower-case letter, a hyphen or a space.
pub(super) fn console_key(byte: u8) -> Option<&'static str> {
    match byte {
        b'0'..=b'9' => DIGIT_KEYS.get(usize::from(byte - b'0')).copied(),
        b'a'..=b'z' => LETTERS.get(usize::from(byte - b'a')).copied(),
        b'-' => Some("minus"),
        b' ' => Some("spc"),
        _ => None,
    }
}

/// The rows td-authd's whole-disk consent summary puts on the prompt
/// (td-authd/src/consent.rs), for session user 1000 and a disk that
/// reports no model and a serial QEMU keeps whole (20 bytes at most) of
/// bytes the prompt shows as themselves, with device-bound storage when
/// `encrypted`.
pub(crate) fn disk_prompt_rows(
    disk: &str,
    capacity: u64,
    serial: &str,
    hostname: &str,
    username: &str,
    deployment: &str,
    encrypted: bool,
) -> Result<Vec<String>, String> {
    let prefix = deployment
        .get(..16)
        .filter(|prefix| {
            prefix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .ok_or("the deployment ID has no 16-digit hex prefix")?;
    if serial.is_empty()
        || serial.len() > 20
        || !serial
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'\\')
    {
        return Err(format!("serial {serial:?} would not show as itself"));
    }
    Ok(vec![
        "TD SECURE ATTENTION".into(),
        "SESSION USER 1000".into(),
        "ERASE DISK AND INSTALL TD".into(),
        format!("DISK: {disk}"),
        format!(
            "SIZE: {}.{} GB, {capacity} BYTES",
            capacity / 1_000_000_000,
            capacity / 100_000_000 % 10
        ),
        "MODEL: NOT REPORTED".into(),
        format!("SERIAL: {serial}"),
        "ALL DATA ON THIS DISK WILL BE LOST".into(),
        format!("HOSTNAME: {hostname}"),
        format!("USER: {username}"),
        format!("DEPLOYMENT: {prefix}..."),
        if encrypted {
            "ENCRYPTED TO THIS COMPUTER, AUTOMATIC LOGIN".into()
        } else {
            "UNENCRYPTED STORAGE, AUTOMATIC LOGIN".into()
        },
        "ENTER: ERASE AND INSTALL   ESC: CANCEL".into(),
    ])
}

/// The top of a 1280x800 prompt's first row when it has `rows` rows of
/// 32-pixel glyphs 8 pixels apart, centred (td-compositor/src/attention.rs).
fn prompt_top(rows: usize) -> Option<usize> {
    let height = rows.checked_mul(40)?.checked_sub(8)?;
    Some(800usize.checked_sub(height)? / 2)
}

/// Which of `rows` differ from the capture, laid out above a countdown
/// row when `countdown`.
fn prompt_differs(pixels: &[u8], rows: &[String], countdown: bool) -> Result<Vec<usize>, String> {
    let count = rows.len().saturating_add(usize::from(countdown));
    let top = prompt_top(count).ok_or("the prompt does not fit the display")?;
    let mut differs = Vec::new();
    for (index, text) in rows.iter().enumerate() {
        if !row_matches(pixels, top.saturating_add(index.saturating_mul(40)), text)? {
            differs.push(index);
        }
    }
    Ok(differs)
}

/// Whether the row at `top` is the prompt's countdown, at any of the
/// seconds a trusted operation may have left.
fn countdown_matches(pixels: &[u8], top: usize) -> Result<bool, String> {
    for seconds in 1..=120u64 {
        let unit = if seconds == 1 { "SECOND" } else { "SECONDS" };
        if row_matches(
            pixels,
            top,
            &format!("TIME LEFT WHEN SHOWN: {seconds} {unit}"),
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether every line outside the `count` rows from `top` is background.
fn blank_between(pixels: &[u8], top: usize, count: usize) -> Result<bool, String> {
    let line = 1280 * 3;
    for y in 0..800usize {
        let row = y.checked_sub(top).map(|offset| (offset / 40, offset % 40));
        if matches!(row, Some((index, within)) if index < count && within < 32) {
            continue;
        }
        let bytes = pixels
            .get(y * line..(y + 1) * line)
            .ok_or("truncated prompt capture")?;
        if bytes
            .as_chunks::<3>()
            .0
            .iter()
            .any(|pixel| *pixel != [24, 32, 40])
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether the capture is the prompt with exactly `rows` above its
/// countdown and nothing else: every pixel of every row as drawn, and
/// background everywhere else.
pub(super) fn disk_prompt_matches(pixels: &[u8], rows: &[String]) -> Result<bool, String> {
    let count = rows.len().saturating_add(1);
    let top = prompt_top(count).ok_or("the prompt does not fit the display")?;
    Ok(prompt_differs(pixels, rows, true)?.is_empty()
        && countdown_matches(pixels, top.saturating_add(rows.len().saturating_mul(40)))?
        && blank_between(pixels, top, count)?)
}

fn after(now: Instant, wait: Duration) -> Result<Instant, String> {
    now.checked_add(wait)
        .ok_or_else(|| "wizard drive deadline overflow".to_string())
}

/// What a consent step does once the prompt shows exactly the review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Then {
    /// Enter, then Escape from the installed notice.
    Installed,
    /// Enter, then the compositor's returning notice and its own close:
    /// no key after Enter.
    Return,
    /// Escape before consent: declined, the target untouched.
    Decline,
}

impl Then {
    /// How often the screen is captured once Enter was pressed: the
    /// returning notice stands only `td-compositor`'s four seconds.
    fn interval(self) -> Duration {
        match self {
            Self::Return => RETURN_INTERVAL,
            Self::Installed | Self::Decline => NOTICE_INTERVAL,
        }
    }
}

/// What the secure attention screen is doing for a consent step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attention {
    Closed,
    /// The chord was pressed; the menu should offer the installation.
    Menu {
        until: Instant,
        then: Then,
    },
    /// `I` was pressed; the prompt should show exactly the review.
    Prompt {
        until: Instant,
        then: Then,
    },
    /// Enter was pressed; the installed notice should follow, or for
    /// `Then::Return` the returning notice.
    Installing {
        until: Instant,
        next: Instant,
        then: Then,
    },
    /// The returning notice showed; the screen must close by itself.
    Returning {
        until: Instant,
    },
    /// Escape was pressed on the prompt; the screen must close.
    Declining {
        until: Instant,
    },
}

/// What QEMU's count of changes to the target must be before a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Writes {
    Any,
    /// Consent is about to be given or declined: nothing written yet.
    None,
    /// Enter again only while nothing is written: the compositor drops an
    /// Enter stamped before its prompt's receipt, which a capture can
    /// precede, and ignores one after it took consent, which the
    /// installation's writes then follow.
    Retry,
    /// The installation said it finished: something written.
    Some,
}

/// Whether a key needing `writes` may be pressed given QEMU's `count` of
/// changes to the target; an error where the count disproves the drive.
fn admitted(writes: Writes, count: Option<u64>) -> Result<bool, String> {
    match (writes, count) {
        (Writes::Any, _) => Ok(true),
        (_, None) => Err("the target's changes were not counted".into()),
        (Writes::None, Some(0)) | (Writes::Retry, Some(0)) => Ok(true),
        (Writes::None, Some(count)) => Err(format!(
            "the target disk took {count} writes before consent was given"
        )),
        (Writes::Retry, Some(_)) => Ok(false),
        (Writes::Some, Some(0)) => {
            Err("the compositor said the system was installed on an unwritten target".into())
        }
        (Writes::Some, Some(_)) => Ok(true),
    }
}

/// What one capture of the attention screen calls for.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Wait,
    Press(&'static str, Writes, Attention),
    /// The screen moved on by itself: no key.
    Move(Attention),
}

/// Whether the capture shows the attention menu's layout: its title row.
fn attention_shown(pixels: &[u8]) -> Result<bool, String> {
    menu_row_matches(pixels, TITLE_TOP, "TD SECURE ATTENTION")
}

/// The last `MARKERS_KEPT` lines of `console` that begin with one of
/// `MARKER_PREFIXES`, for a failed drive to repeat: what the compositor
/// and td-setup last said, which the console's tail may have scrolled past.
pub(super) fn markers(console: &str) -> String {
    let lines: Vec<&str> = console
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| {
            MARKER_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
        .collect();
    let start = lines.len().saturating_sub(MARKERS_KEPT);
    lines
        .get(start..)
        .map(|kept| kept.join("\n"))
        .unwrap_or_default()
}

/// The decision one capture makes in `attention`, at `now`.
fn seen(
    attention: Attention,
    pixels: &[u8],
    rows: &[String],
    now: Instant,
) -> Result<Seen, String> {
    match attention {
        Attention::Closed => Ok(Seen::Wait),
        Attention::Menu { until, then } => {
            if menu_pixels_match(pixels)? {
                Ok(Seen::Press(
                    "i",
                    Writes::Any,
                    Attention::Prompt {
                        until: after(now, ATTENTION_TIMEOUT)?,
                        then,
                    },
                ))
            } else if now > until {
                Err(format!(
                    "the secure attention menu did not offer the installation within {}s",
                    ATTENTION_TIMEOUT.as_secs()
                ))
            } else {
                Ok(Seen::Wait)
            }
        }
        Attention::Prompt { until, then } => {
            if disk_prompt_matches(pixels, rows)? {
                Ok(match then {
                    Then::Decline => Seen::Press(
                        "esc",
                        Writes::None,
                        Attention::Declining {
                            until: after(now, ATTENTION_TIMEOUT)?,
                        },
                    ),
                    Then::Installed | Then::Return => Seen::Press(
                        "ret",
                        Writes::None,
                        Attention::Installing {
                            until: after(now, INSTALL_TIMEOUT)?,
                            next: after(now, then.interval())?,
                            then,
                        },
                    ),
                })
            } else if menu_row_matches(pixels, NOTICE_TOP, NO_INSTALL_NOTICE)? {
                Err("secure attention found no installation to review".into())
            } else if menu_row_matches(pixels, NOTICE_TOP, FAILED_NOTICE)? {
                Err("the compositor failed the installation's consent request".into())
            } else if now > until {
                Err(format!(
                    "the prompt did not show exactly the review within {}s: rows {:?} \
                     differ above a countdown, rows {:?} without one",
                    ATTENTION_TIMEOUT.as_secs(),
                    prompt_differs(pixels, rows, true)?,
                    prompt_differs(pixels, rows, false)?
                ))
            } else {
                Ok(Seen::Wait)
            }
        }
        Attention::Installing { until, then, .. } => {
            if menu_row_matches(pixels, NOTICE_TOP, FAILED_NOTICE)? {
                Err("the compositor said the consented installation failed".into())
            } else if disk_prompt_matches(pixels, rows)? {
                Ok(Seen::Press(
                    "ret",
                    Writes::Retry,
                    Attention::Installing {
                        until,
                        next: after(now, then.interval())?,
                        then,
                    },
                ))
            } else if then == Then::Return
                && menu_row_matches(pixels, NOTICE_TOP, RETURNING_NOTICE)?
            {
                Ok(Seen::Move(Attention::Returning {
                    until: after(now, RETURN_TIMEOUT)?,
                }))
            } else if menu_row_matches(pixels, NOTICE_TOP, INSTALLED_NOTICE)? {
                if then == Then::Return {
                    Err(
                        "the compositor said a device-bound installation was installed \
                         before its recovery key"
                            .into(),
                    )
                } else {
                    Ok(Seen::Press("esc", Writes::Some, Attention::Closed))
                }
            } else if now > until {
                Err(match then {
                    Then::Return => format!(
                        "the compositor did not say it was returning to setup within {}s",
                        INSTALL_TIMEOUT.as_secs()
                    ),
                    Then::Installed | Then::Decline => format!(
                        "the compositor did not say the system was installed within {}s",
                        INSTALL_TIMEOUT.as_secs()
                    ),
                })
            } else {
                Ok(Seen::Wait)
            }
        }
        Attention::Returning { until } => {
            if !attention_shown(pixels)? && !disk_prompt_matches(pixels, rows)? {
                Ok(Seen::Move(Attention::Closed))
            } else if menu_row_matches(pixels, NOTICE_TOP, FAILED_NOTICE)? {
                Err("the compositor said the consented installation failed".into())
            } else if now > until {
                Err(format!(
                    "the secure attention screen did not close by itself within {}s \
                     of its returning notice",
                    RETURN_TIMEOUT.as_secs()
                ))
            } else {
                Ok(Seen::Wait)
            }
        }
        Attention::Declining { until } => {
            if !attention_shown(pixels)? && !disk_prompt_matches(pixels, rows)? {
                Ok(Seen::Move(Attention::Closed))
            } else if now > until {
                Err(format!(
                    "the secure attention screen did not close within {}s of Escape",
                    ATTENTION_TIMEOUT.as_secs()
                ))
            } else {
                Ok(Seen::Wait)
            }
        }
    }
}

pub(super) struct SetupInputController<'a> {
    path: PathBuf,
    capture: PathBuf,
    qmp: Option<Qmp>,
    script: &'a [SetupStep],
    step: usize,
    /// Evidence lines already acted on; a step matches only a later one.
    consumed: usize,
    /// When the state awaited became due: the ready marker, or an act.
    since: Option<Instant>,
    attention: Attention,
    /// The rows the open prompt must show.
    rows: &'a [String],
    /// No key before this, once the screen was seen closed.
    quiet: Option<Instant>,
}

impl<'a> SetupInputController<'a> {
    pub(super) fn new(path: PathBuf, capture: PathBuf, script: &'a [SetupStep]) -> Self {
        Self {
            path,
            capture,
            qmp: None,
            script,
            step: 0,
            consumed: 0,
            since: None,
            attention: Attention::Closed,
            rows: &[],
            quiet: None,
        }
    }

    /// Acts on every step whose state td-setup has since said; true once
    /// the whole script has run.
    pub(super) fn progress(&mut self, evidence: &ConsoleEvidence) -> Result<bool, String> {
        self.progress_at(evidence, Instant::now())
    }

    pub(super) fn progress_at(
        &mut self,
        evidence: &ConsoleEvidence,
        now: Instant,
    ) -> Result<bool, String> {
        if let Some(lost) = &evidence.td_setup_lost {
            return Err(lost.clone());
        }
        if !evidence.target {
            return Ok(false);
        }
        self.since.get_or_insert(now);
        // No script waits on a failed or unknown installation.
        if let Some((_, state)) = evidence
            .td_setup_shown
            .iter()
            .skip(self.consumed)
            .find(|(_, shown)| matches(shown, "page=failed") || matches(shown, "page=unknown"))
        {
            return Err(format!("td-setup showed {state:?}"));
        }
        if !self.attend(now)? || self.quiet.is_some_and(|quiet| now < quiet) {
            return Ok(false);
        }
        while let Some(step) = self.script.get(self.step) {
            let Some(found) = evidence
                .td_setup_shown
                .iter()
                .skip(self.consumed)
                .position(|(_, shown)| matches(shown, &step.shown))
            else {
                // A key that never showed its state, taken by another
                // window or lost, fails here rather than at the ceiling.
                let since = self.since.unwrap_or(now);
                if now.saturating_duration_since(since) > step.within {
                    return Err(format!(
                        "td-setup did not show {:?} within {}s",
                        step.shown,
                        step.within.as_secs()
                    ));
                }
                return Ok(false);
            };
            self.consumed = self
                .consumed
                .checked_add(found)
                .and_then(|index| index.checked_add(1))
                .ok_or("setup evidence index overflow")?;
            if step.untouched {
                let writes = self.target_writes()?;
                if writes != 0 {
                    return Err(format!(
                        "the target disk took {writes} writes before the wizard consented"
                    ));
                }
            }
            match &step.act {
                Act::Keys(keys) if keys.is_empty() => {}
                Act::Keys(keys) => {
                    let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
                    let qmp = self.qmp(deadline)?;
                    for key in keys {
                        qmp.press_until(key, deadline)?;
                    }
                }
                Act::Consent(rows) | Act::ConsentThenReturn(rows) | Act::Decline(rows) => {
                    let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
                    let qmp = self.qmp(deadline)?;
                    qmp.key_chord_until(&["ctrl", "alt", "esc"], deadline)?;
                    // The pointer stays clear of the rows compared.
                    qmp.move_absolute_until(0, 0, deadline)?;
                    self.rows = rows;
                    self.attention = Attention::Menu {
                        until: after(now, ATTENTION_TIMEOUT)?,
                        then: match step.act {
                            Act::ConsentThenReturn(_) => Then::Return,
                            Act::Decline(_) => Then::Decline,
                            _ => Then::Installed,
                        },
                    };
                }
                Act::ReadKey(glyphs, cell) => {
                    let digits = self.read_key(glyphs)?;
                    cell.borrow_mut().digits = Some(digits);
                    self.press("ret")?;
                }
                Act::TypeKey(cell) => {
                    let keys = cell
                        .borrow()
                        .digits()
                        .ok_or("no recovery key was read to type back")?
                        .iter()
                        .map(|digit| console_key(*digit))
                        .collect::<Option<Vec<_>>>()
                        .ok_or("the key read holds a byte that is not a digit")?;
                    for key in keys.into_iter().chain(["ret"]) {
                        self.press(key)?;
                        std::thread::sleep(VT_KEY_PACE);
                    }
                }
            }
            self.step = self.step.saturating_add(1);
            self.since = Some(now);
            if !matches!(self.attention, Attention::Closed) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Advances an open secure attention screen by at most one capture;
    /// true once it is closed.
    fn attend(&mut self, now: Instant) -> Result<bool, String> {
        let next = match self.attention {
            Attention::Closed => return Ok(true),
            Attention::Installing { next, .. } => Some(next),
            Attention::Menu { .. }
            | Attention::Prompt { .. }
            | Attention::Returning { .. }
            | Attention::Declining { .. } => None,
        };
        if next.is_some_and(|next| now < next) {
            return Ok(false);
        }
        let capture = self.screen()?;
        let pixels = ppm(&capture)?;
        match seen(self.attention, pixels, self.rows, now)? {
            Seen::Wait => {
                if let Attention::Installing { until, then, .. } = self.attention {
                    self.attention = Attention::Installing {
                        until,
                        next: after(now, then.interval())?,
                        then,
                    };
                }
                Ok(false)
            }
            Seen::Move(attention) => {
                self.attention = attention;
                if attention == Attention::Closed {
                    self.since = Some(now);
                    self.quiet = Some(after(now, CLOSED_QUIET)?);
                    return Ok(false);
                }
                Ok(false)
            }
            Seen::Press(key, writes, attention) => {
                let count = match writes {
                    Writes::Any => None,
                    Writes::None | Writes::Retry | Writes::Some => Some(self.target_writes()?),
                };
                if !admitted(writes, count)? {
                    self.attention = attention;
                    return Ok(false);
                }
                self.press(key)?;
                self.attention = attention;
                if attention == Attention::Closed {
                    self.since = Some(now);
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }

    fn qmp(&mut self, deadline: Instant) -> Result<&mut Qmp, String> {
        if self.qmp.is_none() {
            self.qmp = Some(Qmp::connect_until(&self.path, deadline)?);
        }
        self.qmp
            .as_mut()
            .ok_or_else(|| "QMP controller disappeared before wizard input".to_string())
    }

    fn press(&mut self, key: &str) -> Result<(), String> {
        let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
        self.qmp(deadline)?.press_until(key, deadline)
    }

    /// The recovery key the display shows, read with `glyphs` from
    /// captures until one shows it: a misread fails at once.
    fn read_key(&mut self, glyphs: &KeyGlyphs) -> Result<Box<[u8; DIGITS]>, String> {
        let until = after(Instant::now(), KEY_SHOWN_TIMEOUT)?;
        loop {
            let capture = self.screen()?;
            if let Some(digits) = glyphs.read(ppm(&capture)?)? {
                return Ok(Box::new(digits));
            }
            if Instant::now() > until {
                return Err(format!(
                    "the recovery-key page showed no key the oracle could read within {}s",
                    KEY_SHOWN_TIMEOUT.as_secs()
                ));
            }
            std::thread::sleep(KEY_SHOWN_INTERVAL);
        }
    }

    /// The display as QEMU scans it out now.
    fn screen(&mut self) -> Result<Vec<u8>, String> {
        let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
        let filename = qmp_json_path(&self.capture)?;
        self.qmp(deadline)?
            .exchange_until(
                &format!(
                    "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":{filename}}}}}"
                ),
                deadline,
            )
            .map_err(|error| format!("capture the secure attention screen: {error}"))?;
        read_capture(&self.capture)
    }

    /// Writes, discards and zone appends QEMU completed on the target disk.
    pub(super) fn target_writes(&mut self) -> Result<u64, String> {
        let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
        self.qmp(deadline)?
            .drive_writes_until(super::install::TARGET_DRIVE_ID, deadline)
    }

    /// The state the script waits for, if it has not finished.
    pub(super) fn awaiting(&self) -> Option<&str> {
        match self.attention {
            Attention::Closed => self.script.get(self.step).map(|step| step.shown.as_str()),
            Attention::Menu { .. } => Some("the secure attention menu"),
            Attention::Prompt { .. } => Some("the whole-disk consent prompt"),
            Attention::Installing {
                then: Then::Return, ..
            } => Some("the compositor's returning notice"),
            Attention::Installing { .. } => Some("the installed notice"),
            Attention::Returning { .. } => Some("the secure attention screen closing by itself"),
            Attention::Declining { .. } => Some("the declined secure attention screen closing"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::super::update::ascii_pixel;
    use super::*;

    fn background() -> Vec<u8> {
        [24, 32, 40].repeat(1280 * 800)
    }

    /// After the last key, only the guest's own exit may end the boot: a
    /// refused or unavailable restart, a lost line or the bound fails it.
    #[test]
    fn a_restart_asked_for_fails_on_refusal_loss_or_its_bound() {
        let asked = Instant::now();
        let shown = |extra: Option<String>| {
            let mut evidence = ConsoleEvidence::default();
            evidence
                .td_setup_shown
                .push((1, "page=complete choice=restart end=offered".into()));
            evidence
                .td_setup_shown
                .push((2, "page=complete choice=restart end=asked-restart".into()));
            evidence
                .td_setup_shown
                .extend(extra.map(|state| (3, state)));
            evidence
        };
        let evidence = shown(None);
        assert_eq!(restart_progress(&evidence, asked, asked), Ok(()));
        let late = asked + RESTART_TIMEOUT + Duration::from_secs(1);
        assert!(restart_progress(&evidence, asked, late)
            .unwrap_err()
            .contains("did not restart"));
        for (choice, ended) in [
            ("restart", "refused"),
            ("poweroff", "refused"),
            ("restart", "unavailable"),
            ("poweroff", "unavailable"),
        ] {
            let evidence = shown(Some(format!("page=complete choice={choice} end={ended}")));
            assert!(restart_progress(&evidence, asked, asked)
                .unwrap_err()
                .contains(ended));
        }
        let mut lost = shown(None);
        lost.td_setup_lost = Some("td-setup's evidence line 3 was lost".into());
        assert!(restart_progress(&lost, asked, asked).is_err());
    }

    /// Draws `text` at `top` as the prompt draws a row: Unifont at twice size.
    fn prompt_row(pixels: &mut [u8], top: usize, text: &str) {
        for (column, character) in text.bytes().enumerate() {
            for y in 0..32 {
                for x in 0..16 {
                    if ascii_pixel(character, x / 2, y / 2).unwrap() {
                        let offset = ((top + y) * 1280 + 24 + column * 16 + x) * 3;
                        pixels[offset..offset + 3].fill(255);
                    }
                }
            }
        }
    }

    /// A capture of `rows`, then `countdown` if any, as the compositor
    /// centres its prompt.
    fn prompt(rows: &[String], countdown: Option<&str>) -> Vec<u8> {
        let mut pixels = background();
        let count = rows.len() + usize::from(countdown.is_some());
        let top = prompt_top(count).unwrap();
        for (row, text) in rows.iter().map(String::as_str).chain(countdown).enumerate() {
            prompt_row(&mut pixels, top + row * 40, text);
        }
        pixels
    }

    /// Draws `text` at `top` as the attention menu draws a row, from the
    /// compositor's own chrome glyphs.
    fn menu_row(pixels: &mut [u8], top: usize, text: &str) {
        let chrome = include_str!("../../../../../../td-compositor/src/ui.rs");
        for (column, character) in text.chars().enumerate() {
            let prefix = format!("b'{character}' => [");
            let rows = chrome
                .lines()
                .find_map(|line| line.trim().strip_prefix(&prefix)?.strip_suffix("],"))
                .unwrap_or("0, 0, 0, 0, 0, 0, 0");
            for (y, bits) in rows
                .split(',')
                .map(|row| row.trim().parse::<u8>().unwrap())
                .enumerate()
            {
                for x in 0..5 {
                    if bits & (1 << (4 - x)) == 0 {
                        continue;
                    }
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let offset =
                                ((top + y * 2 + dy) * 1280 + 24 + column * 12 + x * 2 + dx) * 3;
                            pixels[offset..offset + 3].fill(255);
                        }
                    }
                }
            }
        }
    }

    /// The attention menu showing `notice`, or its entries when none.
    fn menu(notice: Option<&str>) -> Vec<u8> {
        let mut pixels = background();
        menu_row(&mut pixels, 276, "TD SECURE ATTENTION");
        match notice {
            Some(notice) => menu_row(&mut pixels, NOTICE_TOP, notice),
            None => {
                menu_row(&mut pixels, NOTICE_TOP, "U: UNLOCK  R: RECOVERY TOKEN");
                menu_row(&mut pixels, 456, "I: REVIEW PENDING SYSTEM INSTALLATION");
            }
        }
        pixels
    }

    fn rows() -> Vec<String> {
        disk_prompt_rows(
            "vdb",
            12_345_678_901,
            TARGET_SERIAL,
            "td-wizard",
            "dana",
            &"0123456789abcdef".repeat(4),
            false,
        )
        .unwrap()
    }

    const COUNTDOWN: &str = "TIME LEFT WHEN SHOWN: 117 SECONDS";

    #[test]
    fn the_disk_prompt_is_matched_whole_and_only_where_it_is_drawn() {
        let rows = rows();
        assert_eq!(
            rows,
            [
                "TD SECURE ATTENTION",
                "SESSION USER 1000",
                "ERASE DISK AND INSTALL TD",
                "DISK: vdb",
                "SIZE: 12.3 GB, 12345678901 BYTES",
                "MODEL: NOT REPORTED",
                "SERIAL: td-setup-target",
                "ALL DATA ON THIS DISK WILL BE LOST",
                "HOSTNAME: td-wizard",
                "USER: dana",
                "DEPLOYMENT: 0123456789abcdef...",
                "UNENCRYPTED STORAGE, AUTOMATIC LOGIN",
                "ENTER: ERASE AND INSTALL   ESC: CANCEL",
            ]
        );
        // Thirteen rows and the countdown, centred on 800 lines.
        assert_eq!(prompt_top(14), Some(124));
        assert!(disk_prompt_matches(&prompt(&rows, Some(COUNTDOWN)), &rows).unwrap());
        for countdown in [
            "TIME LEFT WHEN SHOWN: 1 SECOND",
            "TIME LEFT WHEN SHOWN: 120 SECONDS",
        ] {
            assert!(disk_prompt_matches(&prompt(&rows, Some(countdown)), &rows).unwrap());
        }
        // A countdown out of range, or none at all, is not this prompt.
        for countdown in [
            "TIME LEFT WHEN SHOWN: 121 SECONDS",
            "TIME LEFT WHEN SHOWN: 1 SECONDS",
            "",
        ] {
            assert!(!disk_prompt_matches(&prompt(&rows, Some(countdown)), &rows).unwrap());
        }
        let lower = prompt(&rows, None);
        assert!(!disk_prompt_matches(&lower, &rows).unwrap());
        assert_eq!(
            prompt_differs(&lower, &rows, false).unwrap(),
            Vec::<usize>::new()
        );
        // Rows above and below that keep the 13 aligned are not this prompt.
        let mut taller = vec!["EXTRA".to_string()];
        taller.extend(rows.iter().cloned());
        taller.push(COUNTDOWN.into());
        let taller = prompt(&taller, Some("EXTRA"));
        assert_eq!(
            prompt_differs(&taller, &rows, true).unwrap(),
            Vec::<usize>::new()
        );
        assert!(!disk_prompt_matches(&taller, &rows).unwrap());
        // So is a stray pixel between rows.
        let mut stray = prompt(&rows, Some(COUNTDOWN));
        let gap = (prompt_top(14).unwrap() + 33) * 1280 * 3 + 600 * 3;
        stray[gap..gap + 3].fill(255);
        assert!(!disk_prompt_matches(&stray, &rows).unwrap());
        // Any other disk, size, account or deployment is refused, and so is
        // any change to any other row.
        let mut changed: Vec<(usize, String)> = [
            (3, "DISK: vdc"),
            (4, "SIZE: 12.3 GB, 12345678902 BYTES"),
            (8, "HOSTNAME: td-wizarc"),
            (9, "USER: dano"),
            (10, "DEPLOYMENT: 0123456789abcdee..."),
        ]
        .into_iter()
        .map(|(row, text)| (row, text.to_string()))
        .collect();
        changed.extend((0..rows.len()).map(|row| (row, format!("{}.", rows[row]))));
        for (row, text) in changed {
            let mut other = rows.clone();
            other[row] = text.clone();
            let drawn = prompt(&other, Some(COUNTDOWN));
            assert!(!disk_prompt_matches(&drawn, &rows).unwrap(), "{text}");
            assert_eq!(prompt_differs(&drawn, &rows, true).unwrap(), [row]);
        }
        // A serial td-authd would escape, mark or QEMU cut is refused.
        let hex = "0".repeat(64);
        for serial in ["a b", "", "a\\b", "abcdefghijklmnopqrstu"] {
            assert!(
                disk_prompt_rows("vdb", 1, serial, "h", "u", &hex, false).is_err(),
                "{serial:?}"
            );
        }
        assert!(disk_prompt_rows("vdb", 1, "abcdefghijklmnopqrst", "h", "u", &hex, false).is_ok());
        assert!(disk_prompt_rows("vdb", 1, "s", "h", "u", &"A".repeat(64), false).is_err());
        assert!(disk_prompt_rows("vdb", 1, "s", "h", "u", "0123", false).is_err());
    }

    /// A console answer types digits, letters, hyphens and spaces, and
    /// nothing else; the wizard's keys include the digits and the space.
    #[test]
    fn console_keys_are_a_closed_set() {
        assert_eq!(console_key(b'0'), Some("0"));
        assert_eq!(console_key(b'9'), Some("9"));
        assert_eq!(console_key(b'r'), Some("r"));
        assert_eq!(console_key(b'-'), Some("minus"));
        assert_eq!(console_key(b' '), Some("spc"));
        for byte in [b'A', b'\n', b'/', b'_', 0x80] {
            assert_eq!(console_key(byte), None, "{byte}");
        }
        for byte in (b'0'..=b'9').chain(b'a'..=b'z').chain([b'-', b' ']) {
            assert!(setup_key(console_key(byte).unwrap()));
        }
    }

    /// Consent that returns to the wizard presses Enter while its prompt
    /// stays, then takes the compositor's returning notice and its own
    /// close with no key; an installed notice first fails it, as does a
    /// screen that never closes.
    #[test]
    fn a_consent_that_returns_waits_for_the_screen_to_close_by_itself() {
        let rows = rows();
        let now = Instant::now();
        let installing = Attention::Installing {
            until: now + INSTALL_TIMEOUT,
            next: now,
            then: Then::Return,
        };
        assert_eq!(
            seen(installing, &prompt(&rows, Some(COUNTDOWN)), &rows, now).unwrap(),
            Seen::Press(
                "ret",
                Writes::Retry,
                Attention::Installing {
                    until: now + INSTALL_TIMEOUT,
                    next: now + RETURN_INTERVAL,
                    then: Then::Return,
                }
            )
        );
        let returning = Attention::Returning {
            until: now + RETURN_TIMEOUT,
        };
        assert_eq!(
            seen(installing, &menu(Some(RETURNING_NOTICE)), &rows, now).unwrap(),
            Seen::Move(returning)
        );
        for screen in [background(), menu(None)] {
            assert_eq!(seen(installing, &screen, &rows, now).unwrap(), Seen::Wait);
        }
        assert!(seen(installing, &menu(Some(INSTALLED_NOTICE)), &rows, now).is_err());
        assert!(seen(installing, &menu(Some(FAILED_NOTICE)), &rows, now).is_err());
        // Returning: the notice stands, then the screen is gone.
        assert_eq!(
            seen(returning, &menu(Some(RETURNING_NOTICE)), &rows, now).unwrap(),
            Seen::Wait
        );
        assert_eq!(
            seen(returning, &background(), &rows, now).unwrap(),
            Seen::Move(Attention::Closed)
        );
        assert!(seen(returning, &menu(Some(FAILED_NOTICE)), &rows, now).is_err());
        assert!(seen(
            returning,
            &menu(Some(RETURNING_NOTICE)),
            &rows,
            now + RETURN_TIMEOUT + Duration::from_secs(1)
        )
        .is_err());
        // The prompt's own choice reaches the installing state.
        let prompt_phase = Attention::Prompt {
            until: now + ATTENTION_TIMEOUT,
            then: Then::Return,
        };
        assert_eq!(
            seen(prompt_phase, &prompt(&rows, Some(COUNTDOWN)), &rows, now).unwrap(),
            Seen::Press(
                "ret",
                Writes::None,
                Attention::Installing {
                    until: now + INSTALL_TIMEOUT,
                    next: now + RETURN_INTERVAL,
                    then: Then::Return,
                }
            )
        );
    }

    /// Once the screen is seen closed, the next step waits out the quiet
    /// interval even though its state was already said, so its key cannot
    /// land in the compositor's settle window; then it acts.
    #[test]
    fn a_step_after_the_close_waits_out_the_settle_window() {
        let script = [SetupStep {
            shown: "page=recovery step=shown".into(),
            act: Act::Keys(Vec::new()),
            untouched: false,
            within: STEP_TIMEOUT,
        }];
        let mut controller = SetupInputController::new(
            PathBuf::from("/nonexistent/qmp"),
            PathBuf::from("/nonexistent/attention.ppm"),
            &script,
        );
        let now = Instant::now();
        controller.quiet = Some(now + CLOSED_QUIET);
        let mut evidence = ConsoleEvidence::default();
        evidence.target = true;
        evidence
            .td_setup_shown
            .push((1, "page=recovery step=shown".into()));
        assert_eq!(controller.progress_at(&evidence, now), Ok(false));
        assert_eq!(controller.awaiting(), Some("page=recovery step=shown"));
        assert_eq!(
            controller.progress_at(&evidence, now + CLOSED_QUIET),
            Ok(true)
        );
        // Twice the compositor's window, whose length and markers these are.
        let input = include_str!("../../../../../../td-compositor/src/input.rs");
        assert_eq!(
            input
                .matches("const SELF_CLOSE_SETTLE: u128 = 100_000_000;")
                .count(),
            1
        );
        assert!(CLOSED_QUIET >= Duration::from_millis(100) * 2);
        for marker in ["TD-ATTENTION-SELF-CLOSE", "TD-ATTENTION-SETTLED"] {
            assert!(input.contains(&format!("_MARKER: &str = \"{marker}\";")));
            assert!(MARKER_PREFIXES
                .iter()
                .any(|prefix| marker.starts_with(prefix)));
        }
    }

    /// A failed drive repeats the last compositor and td-setup lines from
    /// the whole console, and no other line.
    #[test]
    fn a_failed_drive_repeats_the_last_markers() {
        let mut console =
            String::from("noise\r\ntd-compositor: close attention after consent: x\r\n");
        console.push_str("TD-ATTENTION-SELF-CLOSE cutoff=1 settle-until=2\n");
        for n in 1..=MARKERS_KEPT {
            console.push_str(&format!("BTRFS info\nTD-SETUP-SHOWN n={n} page=a\n"));
        }
        console.push_str("  TD-SETUP-SHOWN indented\ntail\n");
        let kept = markers(&console);
        assert_eq!(kept.lines().count(), MARKERS_KEPT);
        assert!(kept.starts_with("TD-SETUP-SHOWN n=1 page=a\n"));
        assert!(kept.ends_with(&format!("n={MARKERS_KEPT} page=a")));
        let short = markers("x\ntd-compositor: e\r\nTD-ATTENTION-SETTLED cutoff=1 dropped=0\n");
        assert_eq!(
            short,
            "td-compositor: e\nTD-ATTENTION-SETTLED cutoff=1 dropped=0"
        );
        assert_eq!(markers("nothing here"), "");
    }

    /// A declined consent presses Escape on the exact prompt with nothing
    /// written, then waits for the screen to close.
    #[test]
    fn a_declined_consent_escapes_the_prompt_and_waits_for_the_close() {
        let rows = rows();
        let now = Instant::now();
        let prompt_phase = Attention::Prompt {
            until: now + ATTENTION_TIMEOUT,
            then: Then::Decline,
        };
        let declining = Attention::Declining {
            until: now + ATTENTION_TIMEOUT,
        };
        assert_eq!(
            seen(prompt_phase, &prompt(&rows, Some(COUNTDOWN)), &rows, now).unwrap(),
            Seen::Press("esc", Writes::None, declining)
        );
        for screen in [
            menu(Some("CANCELLING REQUEST")),
            prompt(&rows, Some(COUNTDOWN)),
        ] {
            assert_eq!(seen(declining, &screen, &rows, now).unwrap(), Seen::Wait);
        }
        assert_eq!(
            seen(declining, &background(), &rows, now).unwrap(),
            Seen::Move(Attention::Closed)
        );
        assert!(seen(
            declining,
            &menu(None),
            &rows,
            now + ATTENTION_TIMEOUT + Duration::from_secs(1)
        )
        .is_err());
    }

    /// Each capture moves the consent on only from the screen it waits for,
    /// and fails on a screen that says the consent cannot follow.
    #[test]
    fn the_attention_screen_is_decided_one_capture_at_a_time() {
        let rows = rows();
        let now = Instant::now();
        let later = now + Duration::from_secs(61);
        let blank = background();
        let menu_phase = Attention::Menu {
            until: now + ATTENTION_TIMEOUT,
            then: Then::Installed,
        };
        // The menu offering the installation is answered with `I`.
        assert_eq!(
            seen(menu_phase, &menu(None), &rows, now).unwrap(),
            Seen::Press(
                "i",
                Writes::Any,
                Attention::Prompt {
                    until: now + ATTENTION_TIMEOUT,
                    then: Then::Installed,
                }
            )
        );
        assert_eq!(seen(menu_phase, &blank, &rows, now).unwrap(), Seen::Wait);
        assert!(seen(menu_phase, &blank, &rows, later).is_err());
        // The exact prompt is answered with Enter, with nothing yet written.
        let prompt_phase = Attention::Prompt {
            until: now + ATTENTION_TIMEOUT,
            then: Then::Installed,
        };
        assert_eq!(
            seen(prompt_phase, &prompt(&rows, Some(COUNTDOWN)), &rows, now).unwrap(),
            Seen::Press(
                "ret",
                Writes::None,
                Attention::Installing {
                    until: now + INSTALL_TIMEOUT,
                    next: now + NOTICE_INTERVAL,
                    then: Then::Installed,
                }
            )
        );
        assert_eq!(seen(prompt_phase, &blank, &rows, now).unwrap(), Seen::Wait);
        let mut other = rows.clone();
        other[9] = "USER: dano".into();
        let wrong = prompt(&other, Some(COUNTDOWN));
        assert_eq!(seen(prompt_phase, &wrong, &rows, now).unwrap(), Seen::Wait);
        let error = seen(prompt_phase, &wrong, &rows, later).unwrap_err();
        assert!(
            error.contains("rows [9] differ above a countdown"),
            "{error}"
        );
        for notice in [NO_INSTALL_NOTICE, FAILED_NOTICE] {
            assert!(seen(prompt_phase, &menu(Some(notice)), &rows, now).is_err());
        }
        // The installed notice is answered with Escape, after writes.
        let installing = Attention::Installing {
            until: now + INSTALL_TIMEOUT,
            next: now,
            then: Then::Installed,
        };
        // A prompt still shown is answered with Enter again, while unwritten.
        assert_eq!(
            seen(installing, &prompt(&rows, Some(COUNTDOWN)), &rows, now).unwrap(),
            Seen::Press(
                "ret",
                Writes::Retry,
                Attention::Installing {
                    until: now + INSTALL_TIMEOUT,
                    next: now + NOTICE_INTERVAL,
                    then: Then::Installed,
                }
            )
        );
        assert_eq!(
            seen(installing, &menu(Some(INSTALLED_NOTICE)), &rows, now).unwrap(),
            Seen::Press("esc", Writes::Some, Attention::Closed)
        );
        assert!(seen(installing, &menu(Some(FAILED_NOTICE)), &rows, now).is_err());
        assert_eq!(
            seen(installing, &menu(Some("PREPARING REQUEST")), &rows, now).unwrap(),
            Seen::Wait
        );
        assert!(seen(
            installing,
            &blank,
            &rows,
            now + INSTALL_TIMEOUT + Duration::from_secs(1)
        )
        .is_err());
        // The notice row is the menu's second.
        assert_eq!(NOTICE_TOP, 312);
        // Consent only on an unwritten target, retried only while it stays
        // so, and its notice only after writes.
        assert_eq!(admitted(Writes::Any, None), Ok(true));
        assert_eq!(admitted(Writes::None, Some(0)), Ok(true));
        assert!(admitted(Writes::None, Some(1)).is_err());
        assert_eq!(admitted(Writes::Retry, Some(0)), Ok(true));
        assert_eq!(admitted(Writes::Retry, Some(1)), Ok(false));
        assert!(admitted(Writes::Some, Some(0)).is_err());
        assert_eq!(admitted(Writes::Some, Some(9)), Ok(true));
        for writes in [Writes::None, Writes::Retry, Writes::Some] {
            assert!(admitted(writes, None).is_err());
        }
        let mut low = background();
        menu_row(&mut low, NOTICE_TOP + 36, INSTALLED_NOTICE);
        assert_eq!(seen(installing, &low, &rows, now).unwrap(), Seen::Wait);
        assert_eq!(
            seen(Attention::Closed, &blank, &rows, now).unwrap(),
            Seen::Wait
        );
    }

    /// The rows are td-authd's own, and the layout and notices the
    /// compositor's.
    #[test]
    fn the_prompt_rows_and_notices_are_the_services_own() {
        let consent = include_str!("../../../../../../td-authd/src/consent.rs");
        for line in [
            "\"TD SECURE ATTENTION\".into(),",
            "format!(\"SESSION USER {}\", self.owner),",
            "lines.push(\"ERASE DISK AND INSTALL TD\".into());",
            "lines.push(format!(\"DISK: {disk}\"));",
            "\"SIZE: {}.{} GB, {capacity} BYTES\",",
            "capacity / 1_000_000_000,",
            "capacity / 100_000_000 % 10",
            "lines.push(label_line(\"MODEL\", model.as_ref()));",
            "lines.push(label_line(\"SERIAL\", serial.as_ref()));",
            "None => format!(\"{field}: NOT REPORTED\"),",
            "lines.push(\"ALL DATA ON THIS DISK WILL BE LOST\".into());",
            "lines.push(format!(\"HOSTNAME: {hostname}\"));",
            "lines.push(format!(\"USER: {username}\"));",
            "lines.push(format!(\"DEPLOYMENT: {prefix}...\"));",
            "Storage::Unencrypted => \"UNENCRYPTED STORAGE, AUTOMATIC LOGIN\",",
            "Storage::DeviceBound => \"ENCRYPTED TO THIS COMPUTER, AUTOMATIC LOGIN\",",
            "lines.push(\"ENTER: ERASE AND INSTALL   ESC: CANCEL\".into());",
        ] {
            assert_eq!(consent.matches(line).count(), 1, "{line}");
        }
        let attention = include_str!("../../../../../../td-compositor/src/attention.rs");
        for notice in [INSTALLED_NOTICE, FAILED_NOTICE, NO_INSTALL_NOTICE] {
            assert!(attention.contains(&format!("=> \"{notice}\",")), "{notice}");
        }
        assert!(attention.contains(&format!(
            "const RETURNING_NOTICE: &str = \"{RETURNING_NOTICE}\";"
        )));
        // The notice stands long enough for captures `RETURN_INTERVAL` apart.
        let secret = include_str!("../../../../../../td-compositor/src/secret_client.rs");
        assert_eq!(
            secret
                .matches("const RETURN_NOTICE: Duration = Duration::from_secs(4);")
                .count(),
            1
        );
        assert!(RETURN_INTERVAL * 4 <= Duration::from_secs(4));
        for line in [
            // The menu's rows: 36 apart at the doubled scale.
            "let top = height.saturating_sub(248) / 2;",
            "let pitch = 18 * scale;",
            "top.saturating_add(index.saturating_mul(pitch)),",
            // The prompt's layout: doubled Unifont rows 4 doubled pixels
            // apart, centred, from 24 pixels in, above the countdown.
            "let scale = if width >= 800 && height >= 600 { 2 } else { 1 };",
            ".checked_add(4 * scale)",
            "let top = (height - text_height) / 2;",
            "24 + (column + indent) * cell_width + x * scale,",
            "lines.push(format!(\"TIME LEFT WHEN SHOWN: {seconds} {unit}\"));",
            "let unit = if seconds == 1 { \"SECOND\" } else { \"SECONDS\" };",
            // A disk installation's budget is the non-login ceiling, which
            // `countdown_matches` spans.
            "const OPERATION_SECONDS: u64 = 120;",
            ".map_or(OPERATION_SECONDS, |ceiling| ceiling.as_secs());",
            "if !(1..=ceiling).contains(&seconds) {",
            "[0x28, 0x20, 0x18, 0],",
        ] {
            assert!(attention.contains(line), "{line}");
        }
    }
}
