//! The reader in a window: td-ui's widget window drives the `App` with
//! its inputs, polls the backend's channel each turn and presents the
//! frame the app paints. The window owns the Wayland connection, the
//! clipboard and the key list; the app owns every view and the document
//! pane, hands the window the selection a copy chord asked for, and
//! lists its keys for the key list `?` or `F1` shows.

use std::sync::mpsc;

use td_ui::keys::Section;
use td_ui::raster::{Raster, Surface};
use td_ui::window::{Clipboard, Flow, Handler, Input};

use super::App;
use crate::backend::{BackendCommand, BackendResponse};
use crate::cache::Cache;
use crate::config::Config;

/// The pace the terminal loop read its channel at, kept as the longest a
/// turn waits for the backend's answer; the window's own idle wait is
/// shorter still.
const POLL_MS: u64 = 120;

struct Session<'a> {
    app: App,
    cache: &'a Cache,
    cmd_tx: &'a mpsc::Sender<BackendCommand>,
    resp_rx: &'a mpsc::Receiver<BackendResponse>,
}

impl Handler for Session<'_> {
    fn app_id(&self) -> &str {
        "td-news"
    }

    fn title(&self) -> &str {
        "Timmy's News"
    }

    fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        let quit = self.app.input(input, self.cache, self.cmd_tx);
        self.app.serve_clipboard(clipboard);
        if quit {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn poll(&mut self, now: u64) -> Flow {
        // A backend that is gone answers nothing more; the reader stays up
        // on what it holds, as the terminal loop did, until the user quits.
        while let Ok(response) = self.resp_rx.try_recv() {
            self.app.handle_backend(response, self.cache);
        }
        self.app.tick(now);
        if self.app.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        POLL_MS
    }

    fn needs_redraw(&self) -> bool {
        self.app.pending_redraw
    }

    fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        self.app.surface = surface;
        self.app.paint(raster)
    }

    fn notice(&mut self, message: &str) {
        crate::log::error(format!("window: {message}"));
    }

    fn keys(&self) -> Vec<Section> {
        self.app.key_sections()
    }

    fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.app.show_keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings;
    use crate::testing::tempdir;
    use crate::ui::tests::{seed_cache, test_config, Board};
    use crate::ui::View;

    /// The copy a chord asked for is offered to the clipboard before the
    /// handler's `input` returns, at the input that asked it, and once.
    #[test]
    fn the_copy_is_served_within_the_input_that_asked_it() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (_resp_tx, resp_rx) = mpsc::channel();
        let app = App::new(&config, &cache, true).expect("app");
        let mut session = Session {
            app,
            cache: &cache,
            cmd_tx: &cmd_tx,
            resp_rx: &resp_rx,
        };
        let mut board = Board::default();
        let key = |session: &mut Session<'_>, chord: &str, board: &mut Board| {
            session.input(
                Input::Key {
                    chord,
                    repeat: false,
                },
                board,
            )
        };
        for chord in ["Down", "Down", "Return", "Return"] {
            assert_eq!(key(&mut session, chord, &mut board), Flow::Continue);
        }
        // The frame between inputs loads the article into the pane.
        session.app.prepare_frame();
        assert_eq!(key(&mut session, "C-a", &mut board), Flow::Continue);
        assert!(board.copies.is_empty());
        assert_eq!(key(&mut session, "C-c", &mut board), Flow::Continue);
        assert_eq!(board.copies.len(), 1);
        assert!(board.copies[0].contains("content"), "{}", board.copies[0]);
        assert!(session.app.copy.is_none(), "served once, within the input");
        assert_eq!(key(&mut session, "Down", &mut board), Flow::Continue);
        assert_eq!(board.copies.len(), 1);
    }

    /// `?` asks the window for its key list once and changes nothing of
    /// the reader's; the list leads with the view the reader is in, and
    /// in the search field `?` is a character of the search.
    #[test]
    fn question_mark_asks_for_the_key_list_and_leaves_the_view() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        seed_cache(&cache, false);
        let config = test_config();
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let (_resp_tx, resp_rx) = mpsc::channel();
        let app = App::new(&config, &cache, true).expect("app");
        let mut session = Session {
            app,
            cache: &cache,
            cmd_tx: &cmd_tx,
            resp_rx: &resp_rx,
        };
        let mut board = Board::default();
        let mut key = |session: &mut Session<'_>, chord: &str| {
            let input = Input::Key {
                chord,
                repeat: false,
            };
            assert_eq!(session.input(input, &mut board), Flow::Continue);
        };
        let lead = |session: &Session<'_>| session.keys().first().map(|section| section.title);
        assert!(!session.take_show_keys());
        key(&mut session, "?");
        assert!(session.take_show_keys());
        assert!(!session.take_show_keys(), "an edge, taken once");
        assert_eq!(session.app.view, View::FeedList);
        assert_eq!(session.app.selected_feed, 0);
        assert_eq!(lead(&session), Some(keybindings::FEEDS));
        for chord in ["Down", "Down", "Return", "/", "?"] {
            key(&mut session, chord);
        }
        assert!(!session.take_show_keys(), "the search's character");
        assert_eq!(session.app.search, "?");
        for chord in ["Backspace", "Return", "?"] {
            key(&mut session, chord);
        }
        assert!(session.take_show_keys());
        assert!(!session.app.search_mode);
        assert_eq!(session.app.view, View::ArticleList);
        assert_eq!(lead(&session), Some(keybindings::ARTICLES));
        key(&mut session, "Return");
        let open = session.app.open_article.as_ref().map(|a| a.hash.clone());
        assert!(open.is_some());
        key(&mut session, "?");
        assert!(session.take_show_keys());
        assert_eq!(session.app.view, View::Article);
        assert_eq!(
            session.app.open_article.as_ref().map(|a| a.hash.clone()),
            open
        );
        assert_eq!(lead(&session), Some(keybindings::ARTICLE));
    }
}

/// Runs the reader in a window on the compositor the environment names,
/// until the window closes or the app quits.
pub fn run(
    config: &Config,
    cache: &Cache,
    cmd_tx: &mpsc::Sender<BackendCommand>,
    resp_rx: &mpsc::Receiver<BackendResponse>,
    offline: bool,
) -> Result<(), String> {
    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )?;
    let stream = td_ui::wayland::connect(endpoint)?;
    let mut session = Session {
        app: App::new(config, cache, offline)?,
        cache,
        cmd_tx,
        resp_rx,
    };
    let typeface = td_ui::pinned_face::load_or_note(
        "td-news",
        std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
    );
    td_ui::window::run(&mut session, stream, std::env::temp_dir(), typeface)
}
