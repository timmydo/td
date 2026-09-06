pub mod email_list;
pub mod email_view;
pub mod help;
pub mod mailbox_list;
pub mod retention_preview;
pub mod rules_preview;
pub mod thread_view;

use super::input::Key;
use super::screen::Terminal;
use crate::backend::BackendResponse;
use crate::civil::{self, Zone};
use std::io;
use std::sync::OnceLock;
use std::time::SystemTime;

/// The system zone, read once. `Zone::local` opens and parses `/etc/localtime`,
/// and this is called from a render path.
static ZONE: OnceLock<Zone> = OnceLock::new();

pub fn format_system_time(time: SystemTime) -> String {
    let duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    let (local, _offset) = ZONE.get_or_init(Zone::local).to_local(secs);
    civil::format_hms(&local)
}

pub enum ViewAction {
    Continue,
    Push(Box<dyn View>),
    Pop,
    Quit,
    Compose(crate::compose::ComposeDraft),
    SwitchAccount(String),
}

pub trait View {
    fn render(&self, term: &mut Terminal) -> io::Result<()>;
    fn handle_key(&mut self, key: Key, term_rows: u16) -> ViewAction;
    /// Whether this view wants terminal mouse tracking enabled.
    fn wants_mouse(&self) -> bool {
        true
    }
    /// Handle a response from the backend thread.
    /// Returns true if the view consumed the response and should re-render.
    fn on_response(&mut self, response: &BackendResponse) -> bool;
    /// Check for a pending action triggered by an async response.
    fn take_pending_action(&mut self) -> Option<ViewAction> {
        None
    }
    /// Trigger a background sync after the UI has been idle.
    /// Returns true if this changed view state and should re-render.
    fn trigger_idle_sync(&mut self) -> bool {
        false
    }
    /// Called when this view becomes the top of the stack again after a child
    /// view was popped (e.g. returning from a folder). Lets a view refresh
    /// state that may have changed while it was hidden. Returns true if it
    /// changed state and should re-render.
    fn on_reveal(&mut self) -> bool {
        false
    }
}

pub struct ViewStack {
    views: Vec<Box<dyn View>>,
}

impl ViewStack {
    pub fn new(initial: Box<dyn View>) -> Self {
        ViewStack {
            views: vec![initial],
        }
    }

    pub fn render_current(&self, term: &mut Terminal) -> io::Result<()> {
        if let Some(view) = self.views.last() {
            view.render(term)?;
        }
        Ok(())
    }

    pub fn handle_key(&mut self, key: Key, term_rows: u16) -> Option<ViewAction> {
        self.views
            .last_mut()
            .map(|view| view.handle_key(key, term_rows))
    }

    /// Route a backend response to all views (top-most can trigger re-render).
    pub fn handle_response(&mut self, response: &BackendResponse) -> bool {
        let top = self.views.len().saturating_sub(1);
        let mut needs_render = false;
        for (idx, view) in self.views.iter_mut().enumerate() {
            if view.on_response(response) && idx == top {
                needs_render = true;
            }
        }
        needs_render
    }

    pub fn current_mut(&mut self) -> Option<&mut Box<dyn View>> {
        self.views.last_mut()
    }

    pub fn current(&self) -> Option<&dyn View> {
        self.views.last().map(|v| v.as_ref())
    }

    pub fn push(&mut self, view: Box<dyn View>) {
        self.views.push(view);
    }

    pub fn pop(&mut self) -> bool {
        if self.views.len() > 1 {
            self.views.pop();
            true
        } else {
            false
        }
    }
}
