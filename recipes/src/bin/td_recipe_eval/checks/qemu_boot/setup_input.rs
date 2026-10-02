//! Drives the live installer wizard with physical keys: once the live
//! session says the wizard is ready, each step waits until td-setup's boot
//! evidence says it showed a page state with every one of the step's
//! fields, after every state it acted on, and then presses its one key
//! through QEMU's emulated keyboard. The guest learns nothing from the host
//! but keys.

use super::{qmp_deadline, ConsoleEvidence, Qmp, QMP_IO_TIMEOUT};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long a state may take to follow its key, or the ready marker, under
/// TCG: a review reads the medium's deployment, the rest is one frame.
const STEP_TIMEOUT: Duration = Duration::from_secs(300);

/// The disposable target disk's serial, as the wizard's destinations see it.
pub(crate) const TARGET_SERIAL: &str = "td-setup-target";

/// Once td-setup says it showed a state holding every space-separated
/// field of `shown`, press `keys`, each alone, in turn.
pub(crate) struct SetupStep {
    pub(crate) shown: String,
    pub(crate) keys: Vec<&'static str>,
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

/// Whether `key` is one the wizard's oracle may press.
pub(super) fn setup_key(key: &str) -> bool {
    LETTERS.contains(&key) || matches!(key, "minus" | "slash" | "tab" | "down" | "ret" | "esc")
}

pub(super) struct SetupInputController<'a> {
    path: PathBuf,
    qmp: Option<Qmp>,
    script: &'a [SetupStep],
    step: usize,
    /// Evidence lines already acted on; a step matches only a later one.
    consumed: usize,
    /// When the state awaited became due: the ready marker, or a key.
    since: Option<Instant>,
}

impl<'a> SetupInputController<'a> {
    pub(super) fn new(path: PathBuf, script: &'a [SetupStep]) -> Self {
        Self {
            path,
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
                if now.saturating_duration_since(since) > STEP_TIMEOUT {
                    return Err(format!(
                        "td-setup did not show {:?} within {}s",
                        step.shown,
                        STEP_TIMEOUT.as_secs()
                    ));
                }
                return Ok(false);
            };
            self.consumed = self
                .consumed
                .checked_add(found)
                .and_then(|index| index.checked_add(1))
                .ok_or("setup evidence index overflow")?;
            if !step.keys.is_empty() {
                let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
                if self.qmp.is_none() {
                    self.qmp = Some(Qmp::connect_until(&self.path, deadline)?);
                }
                let qmp = self
                    .qmp
                    .as_mut()
                    .ok_or("QMP controller disappeared before wizard input")?;
                for key in &step.keys {
                    qmp.press_until(key, deadline)?;
                }
            }
            self.step = self.step.saturating_add(1);
            self.since = Some(now);
        }
        Ok(true)
    }

    /// Writes, discards and zone appends QEMU completed on the target disk;
    /// asked once the script has run, before the machine is stopped.
    pub(super) fn target_writes(&mut self) -> Result<u64, String> {
        let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
        if self.qmp.is_none() {
            self.qmp = Some(Qmp::connect_until(&self.path, deadline)?);
        }
        self.qmp
            .as_mut()
            .ok_or("QMP controller disappeared before the write count")?
            .drive_writes_until(super::install::TARGET_DRIVE_ID, deadline)
    }

    /// The state the script waits for, if it has not finished.
    pub(super) fn awaiting(&self) -> Option<&str> {
        self.script.get(self.step).map(|step| step.shown.as_str())
    }
}
