//! Drives the live installer wizard with physical keys: once the live
//! session says the wizard is ready, each step waits until td-setup's boot
//! evidence says it showed a page state with every one of the step's
//! fields, after every state it acted on, and then acts through QEMU's
//! emulated keyboard. Return on the settled review confirms installation.
//! The guest learns nothing from the host but keys.

use super::recovery_screen::{KeyGlyphs, DIGITS};
use super::update::{ppm, read_capture};
use super::{qmp_deadline, qmp_json_path, ConsoleEvidence, Qmp, QMP_IO_TIMEOUT};
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
/// Begin the lines a failed drive repeats from the whole console: the
/// compositor's attention evidence and errors, and td-setup's states.
const MARKER_PREFIXES: &[&str] = &["TD-ATTENTION-", "td-compositor: ", "TD-SETUP-SHOWN "];
/// How many of those lines a failure repeats, the last ones.
const MARKERS_KEPT: usize = 16;

/// How long the recovery-key page may take to show a key the oracle reads,
/// once td-setup said it showed it, and how often it is captured.
const KEY_SHOWN_TIMEOUT: Duration = Duration::from_secs(60);
const KEY_SHOWN_INTERVAL: Duration = Duration::from_millis(500);

/// The disposable target disk's serial, as the wizard's destinations see it.
pub(crate) const TARGET_SERIAL: &str = "td-setup-target";

/// The recovery key as the oracle read it from the display, its digits
/// zeroed on drop. Only `Act::ReadKey` fills it, and the host's later
/// legs use it: the wizard never has any of it typed.
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
    /// Read the recovery key from the display with these references into
    /// the cell, then press Return to go on to say it is kept.
    ReadKey(Rc<KeyGlyphs>, KeyCell),
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

fn after(now: Instant, wait: Duration) -> Result<Instant, String> {
    now.checked_add(wait)
        .ok_or_else(|| "wizard drive deadline overflow".to_string())
}

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
                    if step.shown.starts_with("page=review ") && keys.contains(&"ret") {
                        std::thread::sleep(Duration::from_millis(600));
                    }
                    let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
                    let qmp = self.qmp(deadline)?;
                    for key in keys {
                        qmp.press_until(key, deadline)?;
                    }
                }
                Act::ReadKey(glyphs, cell) => {
                    let digits = self.read_key(glyphs)?;
                    cell.borrow_mut().digits = Some(digits);
                    self.press("ret")?;
                }
            }
            self.step = self.step.saturating_add(1);
            self.since = Some(now);
        }
        Ok(true)
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
        self.script.get(self.step).map(|step| step.shown.as_str())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
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
}
