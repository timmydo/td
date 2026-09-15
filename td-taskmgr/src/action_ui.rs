//! Process intent through shared menus and revision-bound confirmation widgets.
use crate::actions::{Command, Delivery, Details, Intent, Results, Scope, Signal, Update, TARGETS};
use crate::budget::{Budget, Charge, MemoryVec};
use crate::format::Text;
use crate::hierarchy::ProcessKey;
use crate::ui::Phase;
use std::fmt::Write;
use std::sync::Arc;
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::{chrome::Row, confirmations as confirm, menus};

#[derive(Clone, Copy, Debug)]
struct Action {
    scope: Scope,
    signal: Signal,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Decision {
    Send,
    Close,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Preparing,
    Confirming,
    Sending,
    Cancelling,
    Results,
}
struct Menu {
    widget: menus::Controller<'static, Action, ProcessKey>,
    _charge: Charge,
}
struct Dialog {
    widget: confirm::Controller<Decision, u64, ()>,
    _charge: Charge,
}
pub struct Controller {
    budget: Arc<Budget>,
    surface: Surface,
    available: bool,
    unavailable: Text<256>,
    menu: Option<Menu>,
    dialog: Option<Dialog>,
    request: Option<(Intent, Stage)>,
    details: Option<Details>,
    results: Option<Results>,
    command: Option<Command>,
    next: u64,
    opener: Option<(i64, i64)>,
    note: Option<Text<256>>,
    result_memory: bool,
    repaint: bool,
    refusal: Option<Text<256>>,
    _charge: Charge,
}
impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessActions")
            .field("available", &self.available)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}
fn why(error: impl std::fmt::Display) -> Text<256> {
    let mut text = Text::default();
    let _ = write!(text, "{error}");
    text
}
impl Controller {
    pub fn new(budget: &Arc<Budget>, surface: Surface) -> Result<Self, String> {
        Ok(Self {
            budget: Arc::clone(budget),
            surface,
            available: false,
            unavailable: Text::new("Process controls are starting."),
            menu: None,
            dialog: None,
            request: None,
            details: None,
            results: None,
            command: None,
            next: 0,
            opener: None,
            note: None,
            result_memory: false,
            repaint: false,
            refusal: None,
            _charge: budget
                .charge(std::mem::size_of::<Self>())
                .map_err(|e| e.to_string())?,
        })
    }
    pub fn busy(&self) -> bool {
        self.menu.is_some() || self.request.is_some() || self.dialog.is_some()
    }
    pub fn take_command(&mut self) -> Option<Command> {
        self.command.take()
    }
    pub fn take_note(&mut self) -> Option<Text<256>> {
        self.note.take()
    }
    pub fn stage(&self) -> &'static str {
        if self.menu.is_some() {
            return "menu";
        }
        match self.request.map(|(_, stage)| stage) {
            Some(Stage::Preparing) => "preparing",
            Some(Stage::Confirming) => "confirmation",
            Some(Stage::Sending) => "sending",
            Some(Stage::Cancelling) => "cancelling",
            Some(Stage::Results) => "results",
            None if self.available => "idle",
            None => "unavailable",
        }
    }
    pub fn open(&mut self, key: Option<ProcessKey>, historical: bool, point: Option<(i64, i64)>) {
        if self.busy() {
            return;
        }
        if !self.available {
            self.note = Some(self.unavailable);
            return;
        }
        if historical {
            self.note = Some(Text::new(
                "Return to Live before preparing a process action.",
            ));
            return;
        }
        let Some(key) = key else {
            self.note = Some(Text::new("Select an observed process first."));
            return;
        };
        let build = || -> Result<Menu, String> {
            let count = 2 * (6 + Signal::ALL.len());
            let mut nodes = MemoryVec::new(&self.budget, count).map_err(|e| e.to_string())?;
            let mut push = |parent, label: &'static str, item| {
                nodes
                    .push(menus::Node {
                        parent,
                        row: Row {
                            label,
                            shortcut: "",
                            enabled: true,
                            checked: false,
                        },
                        item,
                    })
                    .map_err(|_| "process menu limit".to_string())
            };
            for (scope, root) in [
                (Scope::Selected, 0usize),
                (Scope::Descendants, 6 + Signal::ALL.len()),
            ] {
                push(
                    None,
                    if scope == Scope::Selected {
                        "Selected process"
                    } else {
                        "Process and observed descendants"
                    },
                    menus::Item::Submenu,
                )?;
                for (label, signal) in [
                    ("Terminate (SIGTERM)", Signal::Terminate),
                    ("Force kill (SIGKILL)", Signal::Kill),
                    ("Suspend (SIGSTOP)", Signal::Stop),
                    ("Resume (SIGCONT)", Signal::Continue),
                ] {
                    push(
                        Some(root),
                        label,
                        menus::Item::Action(Action { scope, signal }),
                    )?;
                }
                let signals = root + 5;
                push(Some(root), "Send signal", menus::Item::Submenu)?;
                for signal in Signal::ALL {
                    push(
                        Some(signals),
                        signal.label(),
                        menus::Item::Action(Action { scope, signal }),
                    )?;
                }
            }
            let estimated = std::mem::size_of::<menus::Controller<'static, Action, ProcessKey>>()
                + count
                    * (std::mem::size_of::<menus::Node<'static, Action>>()
                        + std::mem::size_of::<usize>()
                        + std::mem::size_of::<&str>());
            let mut charge = self.budget.charge(estimated).map_err(|e| e.to_string())?;
            let model =
                menus::Model::new(menus::Kind::Context, key, &nodes).map_err(|e| e.to_string())?;
            let mut widget = menus::Controller::new(model, self.surface, menus::Fit::Adaptive)
                .map_err(|e| e.to_string())?;
            let (x, y) = point.unwrap_or((16, 48));
            widget.open_context(x, y).map_err(|e| e.to_string())?;
            let actual = std::mem::size_of_val(&widget) + widget.model().storage_bytes()
                - std::mem::size_of_val(widget.model());
            charge
                .additional(actual.saturating_sub(estimated))
                .map_err(|e| e.to_string())?;
            Ok(Menu {
                widget,
                _charge: charge,
            })
        };
        match build() {
            Ok(menu) => {
                self.menu = Some(menu);
                self.opener = point;
            }
            Err(error) => self.note = Some(why(error)),
        }
    }
    fn start(&mut self, key: ProcessKey, action: Action) {
        self.menu = None;
        let Some(revision) = self.next.checked_add(1) else {
            self.note = Some(Text::new("Process request identifiers exhausted."));
            return;
        };
        self.next = revision;
        let intent = Intent {
            revision,
            key,
            scope: action.scope,
            signal: action.signal,
        };
        self.request = Some((intent, Stage::Preparing));
        self.command = Some(Command::Prepare(intent));
        self.note = Some(Text::new(
            "Preparing the current process identities for confirmation...",
        ));
    }
    fn clear(&mut self) {
        self.menu = None;
        self.dialog = None;
        self.request = None;
        self.details = None;
        self.results = None;
        self.command = None;
        self.result_memory = false;
        self.refusal = None;
    }
    pub fn cancel_gesture(&mut self) {
        self.confirmation(confirm::Event::Other);
        self.menu_event(menus::Event::Other);
    }
    pub fn focus_lost(&mut self) {
        if self
            .request
            .is_some_and(|(_, stage)| stage == Stage::Results)
        {
            self.dialog = None;
            self.retry_results();
        } else {
            self.cancel();
        }
    }
    pub fn cancel(&mut self) {
        self.menu = None;
        self.dialog = None;
        let Some((intent, stage)) = self.request else {
            return;
        };
        if stage == Stage::Results || matches!(self.command, Some(Command::Prepare(_))) {
            self.clear();
        } else {
            self.request = Some((intent, Stage::Cancelling));
            self.command = Some(Command::Cancel(intent.revision));
        }
    }
    pub fn resize(&mut self, surface: Surface) {
        if surface == self.surface {
            return;
        }
        self.surface = surface;
        if self
            .request
            .is_some_and(|(_, stage)| stage == Stage::Results)
        {
            self.dialog = None;
            self.retry_results();
        } else {
            self.cancel();
        }
    }
    pub fn needs_result_memory(&self) -> bool {
        self.result_memory
    }
    pub fn retry_results(&mut self) {
        if !self
            .request
            .is_some_and(|(_, stage)| stage == Stage::Results)
        {
            return;
        }
        match self.dialog(Decision::Close) {
            Ok(dialog) => {
                self.dialog = Some(dialog);
                self.result_memory = false;
                self.note = Some(Text::new(
                    "Signal results are per process. Sent does not prove a state change.",
                ));
            }
            Err(error) => {
                self.result_memory =
                    error.contains("memory budget") || error.contains("allocation failed");
                let mut message =
                    Text::<256>::new("Results retained; Enter retries, Escape closes. ");
                let _ = message.write_str(&error);
                self.note = Some(message);
            }
        }
    }
    pub fn disable(&mut self, error: &str) {
        self.clear();
        self.available = false;
        self.unavailable = Text::truncated(error);
        self.note = Some(self.unavailable);
    }
    fn dialog(&self, decision: Decision) -> Result<Dialog, String> {
        let details = self.details.as_ref().ok_or("process details unavailable")?;
        let mut rendered =
            MemoryVec::new(&self.budget, details.rows.len()).map_err(|e| e.to_string())?;
        for (index, detail) in details.rows.iter().enumerate() {
            let mut row = Text::<4096>::default();
            if decision == Decision::Close {
                match self
                    .results
                    .as_ref()
                    .and_then(|results| results.deliveries.get(index))
                {
                    Some(Delivery::Sent) => {
                        let _ = row.write_str("Sent | ");
                    }
                    Some(Delivery::Exited) => {
                        let _ = row.write_str("Exited | ");
                    }
                    Some(Delivery::Permission) => {
                        let _ = row.write_str("Permission denied | ");
                    }
                    Some(Delivery::Cancelled) => {
                        let _ = row.write_str("Not sent (cancelled) | ");
                    }
                    Some(Delivery::Error(code)) => {
                        let _ = write!(row, "Error {code} | ");
                    }
                    None => return Err("incomplete process action result".into()),
                }
            }
            row.write_str(detail.as_str())
                .map_err(|_| "process detail exceeds confirmation limit")?;
            rendered
                .push(row)
                .map_err(|_| "process detail count exceeds limit")?;
        }
        let mut rows = [""; TARGETS];
        for (index, row) in rendered.iter().enumerate() {
            *rows
                .get_mut(index)
                .ok_or("process detail count exceeds limit")? = row.as_str();
        }
        let rows = rows
            .get(..rendered.len())
            .ok_or("process detail count exceeds limit")?;
        let mut title = Text::<256>::default();
        write!(
            title,
            "{}: {} ({})",
            details.intent.signal.label(),
            if decision == Decision::Close {
                "results"
            } else if details.intent.scope == Scope::Selected {
                "selected process"
            } else {
                "process and observed descendants"
            },
            rows.len()
        )
        .map_err(|_| "process confirmation title exceeds limit")?;
        let text_bytes: usize = rows.iter().map(|row| row.len()).sum();
        let estimate = std::mem::size_of::<confirm::Controller<Decision, u64, ()>>()
            + text_bytes
            + rows.len() * std::mem::size_of::<String>()
            + 2 * confirm::LABEL_BYTES
            + text_bytes.max(rows.len()).min(confirm::WRAPPED_ROWS)
                * 8
                * std::mem::size_of::<usize>();
        let mut charge = self.budget.charge(estimate).map_err(|e| e.to_string())?;
        let model = confirm::Model::new(
            title.as_str(),
            if decision == Decision::Send {
                "Send signal"
            } else {
                "Close"
            },
            rows,
            decision,
            details.intent.revision,
        )
        .map_err(|e| e.to_string())?;
        let scale = self.surface.scale.value() as u32;
        let rect = Rect {
            x: i64::from(16 * scale),
            y: i64::from(24 * scale),
            width: (self.surface.width as u32).saturating_sub(32 * scale),
            height: (self.surface.height as u32).saturating_sub(72 * scale),
        };
        let mut widget = confirm::Controller::new(model, self.surface, rect, Some(()))
            .map_err(|e| e.to_string())?;
        if decision == Decision::Send
            && self.opener.is_some_and(|(x, y)| {
                widget
                    .action_rect(confirm::Focus::Confirm)
                    .is_some_and(|rect| rect.contains(x, y))
            })
        {
            let rect = Rect {
                height: rect.height.saturating_sub(72 * scale),
                ..rect
            };
            let result = widget.event(
                Some(details.intent.revision),
                true,
                confirm::Event::Resize {
                    surface: self.surface,
                    rect,
                },
            );
            if matches!(result, confirm::Outcome::Closed { .. }) {
                return Err("enlarge the window to confirm this process action".into());
            }
        }
        if decision == Decision::Send
            && self.opener.is_some_and(|(x, y)| {
                widget
                    .action_rect(confirm::Focus::Confirm)
                    .is_some_and(|rect| rect.contains(x, y))
            })
        {
            return Err("confirmation overlaps its opener".into());
        }
        charge
            .additional(widget.storage_bytes().saturating_sub(estimate))
            .map_err(|e| e.to_string())?;
        Ok(Dialog {
            widget,
            _charge: charge,
        })
    }
    pub fn update(&mut self, update: Update) {
        match update {
            Update::Available => {
                self.available = true;
                self.unavailable.clear();
            }
            Update::Unavailable(reason) => self.disable(reason.as_str()),
            Update::Prepared(details) => {
                if self.request != Some((details.intent, Stage::Preparing)) {
                    if self.command.is_none() {
                        self.command = Some(Command::Cancel(details.intent.revision));
                    }
                    return;
                }
                self.details = Some(details);
                match self.dialog(Decision::Send) {
                    Ok(dialog) => {
                        if let Some((intent, _)) = self.request {
                            self.request = Some((intent, Stage::Confirming));
                        }
                        self.dialog = Some(dialog);
                        self.note = Some(Text::new(
                            "Review the captured processes. Cancel is the default.",
                        ));
                    }
                    Err(error) => {
                        let reason = why(error);
                        self.refusal = Some(reason);
                        self.note = Some(reason);
                        self.cancel();
                    }
                }
            }
            Update::Failed { revision, reason } => {
                if self
                    .request
                    .is_some_and(|(intent, _)| intent.revision == revision)
                {
                    self.clear();
                    self.note = Some(reason);
                }
            }
            Update::Cancelled(revision) => {
                if self
                    .request
                    .is_some_and(|(intent, _)| intent.revision == revision)
                {
                    let reason = self.refusal;
                    self.clear();
                    self.note =
                        Some(reason.unwrap_or_else(|| Text::new("Process action cancelled.")));
                }
            }
            Update::Finished(results) => {
                if !self
                    .request
                    .is_some_and(|(intent, _)| intent.revision == results.revision)
                {
                    return;
                }
                self.results = Some(results);
                if let Some((intent, _)) = self.request {
                    self.request = Some((intent, Stage::Results));
                }
                self.retry_results();
            }
        }
    }
    fn confirmation(&mut self, event: confirm::Event) -> bool {
        let Some(dialog) = &mut self.dialog else {
            return false;
        };
        let revision = self.request.map(|(intent, _)| intent.revision);
        let outcome = dialog.widget.event(revision, true, event);
        self.repaint |= matches!(
            outcome,
            confirm::Outcome::Changed | confirm::Outcome::Closed { .. }
        );
        if let confirm::Outcome::Closed { choice, .. } = outcome {
            self.dialog = None;
            match choice {
                confirm::Choice::Confirmed(Decision::Send) => {
                    if let Some((intent, Stage::Confirming)) = self.request {
                        self.request = Some((intent, Stage::Sending));
                        self.command = Some(Command::Confirm(intent.revision));
                        self.note = Some(Text::new(
                            "Sending the confirmed signal to captured processes...",
                        ));
                    }
                }
                confirm::Choice::Confirmed(Decision::Close) => self.clear(),
                _ => self.cancel(),
            }
        }
        true
    }
    fn menu_event(&mut self, event: menus::Event) -> bool {
        let Some(menu) = &mut self.menu else {
            return false;
        };
        let key = menu.widget.model().revision();
        let outcome = menu.widget.event(Some(key), event);
        self.repaint |= !matches!(
            &outcome,
            Ok(menus::Outcome::Ignored | menus::Outcome::Consumed)
        );
        match outcome {
            Ok(menus::Outcome::Activated(action)) => self.start(key, action),
            Ok(menus::Outcome::Dismissed | menus::Outcome::Stale) => self.menu = None,
            Err(error) => {
                self.menu = None;
                self.note = Some(why(error));
            }
            _ => {}
        }
        true
    }
    pub fn key(&mut self, key: &str, repeated: bool) -> bool {
        if !self.busy() || key == "C-q" {
            return false;
        }
        if self.dialog.is_none()
            && self
                .request
                .is_some_and(|(_, stage)| stage == Stage::Results)
            && matches!(key, "Return" | "Space" | " ")
            && !repeated
        {
            self.retry_results();
            return true;
        }
        let ck = match key {
            "Tab" => Some(confirm::Key::Tab),
            "S-Tab" => Some(confirm::Key::BackTab),
            "Up" => Some(confirm::Key::Up),
            "Down" => Some(confirm::Key::Down),
            "Home" => Some(confirm::Key::Home),
            "End" => Some(confirm::Key::End),
            "PageUp" => Some(confirm::Key::PageUp),
            "PageDown" => Some(confirm::Key::PageDown),
            "Return" | "Space" | " " => Some(confirm::Key::Activate),
            "Escape" => Some(confirm::Key::Escape),
            _ => None,
        };
        if self.confirmation(
            ck.map(|key| confirm::Event::Key { key, repeated })
                .unwrap_or(confirm::Event::Other),
        ) {
            return true;
        }
        let mk = match key {
            "Up" => Some(menus::Key::Up),
            "Down" => Some(menus::Key::Down),
            "Left" => Some(menus::Key::Left),
            "Right" => Some(menus::Key::Right),
            "Return" | "Space" | " " => Some(menus::Key::Activate),
            "Escape" => Some(menus::Key::Escape),
            _ => None,
        };
        if self.menu_event(
            mk.map(|key| menus::Event::Key { key, repeated })
                .unwrap_or(menus::Event::Other),
        ) {
            return true;
        }
        if key == "Escape" && !repeated {
            self.cancel();
        }
        true
    }
    pub fn pointer(&mut self, phase: Phase, x: i64, y: i64) -> Option<bool> {
        if !self.busy() {
            return None;
        }
        self.repaint = false;
        let event = match phase {
            Phase::Press => confirm::Event::Press { x, y },
            Phase::Move => confirm::Event::Move { x, y },
            Phase::Release => confirm::Event::Release { x, y },
        };
        if self.confirmation(event) {
            return Some(self.repaint);
        }
        if phase == Phase::Press {
            self.opener = Some((x, y));
        }
        self.menu_event(match phase {
            Phase::Press => menus::Event::Press { x, y },
            Phase::Move => menus::Event::Move { x, y },
            Phase::Release => menus::Event::Release,
        });
        Some(self.repaint)
    }
    pub fn scroll(&mut self, x: i64, y: i64, rows: i64) -> bool {
        if !self.busy() {
            return false;
        }
        let rows = rows.clamp(isize::MIN as i64, isize::MAX as i64) as isize;
        if !self.confirmation(confirm::Event::Wheel { x, y, rows }) {
            self.menu_event(menus::Event::Wheel { x, y, rows });
        }
        true
    }
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if let Some(menu) = &self.menu {
            menu.widget.emit(damage, sink);
        }
        if let Some(dialog) = &self.dialog {
            dialog.widget.emit(damage, sink);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    fn fixture() -> Controller {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut c = Controller::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        c.update(Update::Available);
        c
    }
    fn key() -> ProcessKey {
        ProcessKey {
            generation: 1,
            pid: 42,
            start_ticks: 99,
        }
    }
    fn prepared(c: &mut Controller, count: usize) -> Intent {
        c.start(
            key(),
            Action {
                scope: Scope::Descendants,
                signal: Signal::Stop,
            },
        );
        let Some(Command::Prepare(intent)) = c.take_command() else {
            panic!("prepare missing")
        };
        let mut rows = MemoryVec::new(&c.budget, count).unwrap();
        for index in 0..count {
            let mut row = Text::default();
            write!(row, "PID {} start 99 UID 1000: owned process", index + 42).unwrap();
            rows.push(row).unwrap();
        }
        c.update(Update::Prepared(Details { intent, rows }));
        assert_eq!(c.stage(), "confirmation", "{:?}", c.take_note());
        intent
    }
    #[test]
    fn shared_confirmation_defaults_to_cancel_and_send_is_exactly_once() {
        let mut c = fixture();
        let intent = prepared(&mut c, 256);
        assert_eq!(
            c.dialog.as_ref().unwrap().widget.focus(),
            confirm::Focus::Cancel
        );
        c.key("Return", false);
        assert_eq!(c.take_command(), Some(Command::Cancel(intent.revision)));
        c.update(Update::Cancelled(intent.revision));
        let intent = prepared(&mut c, 2);
        c.key("Tab", false);
        assert_eq!(
            c.dialog.as_ref().unwrap().widget.focus(),
            confirm::Focus::Confirm
        );
        c.key("Return", true);
        assert!(c.take_command().is_none());
        c.key("Return", false);
        assert_eq!(c.take_command(), Some(Command::Confirm(intent.revision)));
        c.key("Return", false);
        c.key(" ", true);
        assert!(c.take_command().is_none());
        assert_eq!(c.stage(), "sending");
    }
    #[test]
    fn configure_without_resize_preserves_consent_and_refusal_survives_cancellation() {
        let mut c = fixture();
        let intent = prepared(&mut c, 1);
        c.resize(c.surface);
        assert_eq!(c.stage(), "confirmation");
        assert!(c.take_command().is_none());
        let details = c.details.take().unwrap();
        c.dialog = None;
        c.request = Some((intent, Stage::Preparing));
        c.surface = Surface::new(64, 64, Default::default()).unwrap();
        c.update(Update::Prepared(details));
        let reason = c.take_note().unwrap();
        assert_eq!(c.take_command(), Some(Command::Cancel(intent.revision)));
        c.update(Update::Cancelled(intent.revision));
        assert_eq!(c.take_note().unwrap().as_str(), reason.as_str());
        assert_ne!(reason.as_str(), "Process action cancelled.");
        c.start(
            key(),
            Action {
                scope: Scope::Selected,
                signal: Signal::Stop,
            },
        );
        let Some(Command::Prepare(intent)) = c.take_command() else {
            panic!("missing prepare")
        };
        c.update(Update::Failed {
            revision: intent.revision,
            reason: Text::new("selected process exited"),
        });
        assert_eq!(c.take_note().unwrap().as_str(), "selected process exited");
    }
    #[test]
    fn mismatched_prepared_reply_cannot_overwrite_an_unsubmitted_request() {
        let mut c = fixture();
        let stale = prepared(&mut c, 1);
        let details = c.details.take().unwrap();
        c.clear();
        c.start(
            key(),
            Action {
                scope: Scope::Selected,
                signal: Signal::Stop,
            },
        );
        let pending = c.command;
        c.update(Update::Prepared(details));
        assert_eq!(c.command, pending);
        assert_ne!(c.command, Some(Command::Cancel(stale.revision)));
    }
    #[test]
    fn menus_capture_explicit_scope_and_historical_or_synthetic_rows_refuse() {
        let mut c = fixture();
        c.open(Some(key()), true, None);
        assert!(!c.busy());
        c.open(None, false, None);
        assert!(!c.busy());
        c.open(Some(key()), false, None);
        assert_eq!(c.stage(), "menu");
        c.key("Down", false);
        c.key("Right", false);
        c.key("Down", false);
        c.key("Down", false);
        c.key("Return", false);
        let Some(Command::Prepare(intent)) = c.take_command() else {
            panic!("menu did not prepare")
        };
        assert_eq!(intent.scope, Scope::Descendants);
        assert_eq!(intent.signal, Signal::Stop);
        assert_eq!(intent.key, key());
    }
    #[test]
    fn resize_cancels_authority_and_local_cancel_does_not_queue_stale_work() {
        let mut c = fixture();
        c.start(
            key(),
            Action {
                scope: Scope::Selected,
                signal: Signal::Kill,
            },
        );
        c.cancel();
        assert!(c.take_command().is_none());
        assert!(!c.busy());
        let intent = prepared(&mut c, 1);
        c.resize(Surface::new(800, 600, Default::default()).unwrap());
        assert_eq!(c.take_command(), Some(Command::Cancel(intent.revision)));
        assert_eq!(c.stage(), "cancelling");
        assert!(c.dialog.is_none());
    }
    #[test]
    fn partial_results_survive_budget_failure_and_recover_without_resending() {
        let mut c = fixture();
        let intent = prepared(&mut c, 2);
        c.key("Tab", false);
        c.key("Return", false);
        assert_eq!(c.take_command(), Some(Command::Confirm(intent.revision)));
        let mut deliveries = MemoryVec::new(&c.budget, 2).unwrap();
        deliveries.push(Delivery::Sent).unwrap();
        deliveries.push(Delivery::Exited).unwrap();
        let pressure = c
            .budget
            .charge(c.budget.maximum() - c.budget.used())
            .unwrap();
        c.update(Update::Finished(Results {
            revision: intent.revision,
            deliveries,
        }));
        assert_eq!(c.stage(), "results");
        assert!(c.needs_result_memory());
        assert!(c.dialog.is_none());
        assert_eq!(
            &*c.results.as_ref().unwrap().deliveries,
            &[Delivery::Sent, Delivery::Exited]
        );
        assert!(c.take_command().is_none());
        drop(pressure);
        c.retry_results();
        assert!(!c.needs_result_memory());
        assert!(c.dialog.is_some());
        assert!(c.take_command().is_none());
        c.focus_lost();
        assert_eq!(c.stage(), "results");
        assert!(c.results.is_some());
        c.key("Escape", false);
        assert!(!c.busy());
    }
    #[test]
    fn dialog_confirm_cannot_overlap_the_menu_pointer_opener() {
        let mut c = fixture();
        let intent = prepared(&mut c, 1);
        let original = c
            .dialog
            .as_ref()
            .unwrap()
            .widget
            .action_rect(confirm::Focus::Confirm)
            .unwrap();
        c.cancel();
        c.take_command();
        c.update(Update::Cancelled(intent.revision));
        let point = (original.x + 2, original.y + 2);
        c.opener = Some(point);
        prepared(&mut c, 1);
        assert!(!c
            .dialog
            .as_ref()
            .unwrap()
            .widget
            .action_rect(confirm::Focus::Confirm)
            .unwrap()
            .contains(point.0, point.1));
        c.pointer(Phase::Press, point.0, point.1);
        c.pointer(Phase::Release, point.0, point.1);
        assert!(c.take_command().is_none());
    }
    #[test]
    fn pointer_leave_retires_confirm_arm_without_losing_keyboard_request() {
        let mut c = fixture();
        prepared(&mut c, 1);
        let rect = c
            .dialog
            .as_ref()
            .unwrap()
            .widget
            .action_rect(confirm::Focus::Confirm)
            .unwrap();
        c.pointer(Phase::Press, rect.x + 2, rect.y + 2);
        c.cancel_gesture();
        c.pointer(Phase::Release, rect.x + 2, rect.y + 2);
        assert!(c.take_command().is_none());
        assert_eq!(c.stage(), "confirmation");
        assert_eq!(
            c.dialog.as_ref().unwrap().widget.focus(),
            confirm::Focus::Cancel
        );
    }
}
