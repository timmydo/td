//! The client in a window: td-ui's screen window drives the view stack
//! with the keys it translates, polls the backend's channel each turn and
//! presents the frame the top view renders. The window owns the Wayland
//! connection; the session owns the views, the backend and the account.

pub mod input;
pub mod screen;
pub mod views;

use crate::backend::{self, BackendCommand, BackendResponse};
use crate::compose;
use crate::config::{AccountConfig, RetentionPolicyConfig, SpamConfig, Theme};
use crate::regex::UserRegex;
use crate::rules::CompiledRule;
use input::Key;
use screen::Terminal;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};
use td_ui::screen::{Input, Screen, Style};
use td_ui::screen_app::{Flow, Handler};
use views::mailbox_list::MailboxListView;
use views::{ViewAction, ViewStack};

/// The tenth of a second the terminal's read timed out at, kept as the
/// longest a turn waits for the backend's answer or the idle-sync clock.
const POLL_MS: u64 = 100;

/// Wait on a fire-and-forget child in a detached thread so it does not linger
/// as a zombie. `Child` has no reaping `Drop` impl, so a dropped handle leaks a
/// PID slot for the lifetime of td-mail.
pub fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// What a session keeps of the configuration to open an account's views.
struct Setup {
    accounts: Vec<AccountConfig>,
    account_names: Vec<String>,
    page_size: u32,
    scrolloff: usize,
    browser: Option<String>,
    reply_from: Option<String>,
    archive_folder: String,
    deleted_folder: String,
    retention_policies: Vec<RetentionPolicyConfig>,
    sync_interval_secs: Option<u64>,
    rules: Arc<Vec<CompiledRule>>,
    custom_headers: Arc<Vec<String>>,
    rules_mailbox_regex: Arc<UserRegex>,
    my_email_regex: Arc<UserRegex>,
    spam_config: SpamConfig,
    offline: bool,
    editor_cmd: String,
    theme: Theme,
    /// None offline: no connection is ever asked for.
    connector: Option<Sender<ConnectRequest>>,
}

impl Setup {
    fn mailbox_view(
        &self,
        cmd_tx: &Sender<BackendCommand>,
        account: &AccountConfig,
    ) -> MailboxListView {
        MailboxListView::new(
            cmd_tx.clone(),
            account.username.clone(),
            self.reply_from.clone(),
            self.browser.clone(),
            self.page_size,
            self.scrolloff,
            self.account_names.clone(),
            account.name.clone(),
            self.archive_folder.clone(),
            self.deleted_folder.clone(),
            self.retention_policies.clone(),
            self.sync_interval_secs,
        )
    }

    /// Opens an account: its backend starts with no connection, so the
    /// window opens at once on the mailbox list's loading state. The
    /// session offline fetches the mailboxes from the cache now; otherwise
    /// the connector is asked for the account's connection, which can
    /// take minutes to make, and the backend holds the fetch, and every
    /// command a view sends meanwhile, until the connection is decided,
    /// so nothing is answered from the cache that the connection then
    /// supersedes and the window never says the client is offline while
    /// it connects.
    fn open_account(
        &self,
        account: &AccountConfig,
        origin: &str,
    ) -> (Sender<BackendCommand>, Receiver<BackendResponse>) {
        let (cmd_tx, resp_rx) = backend::spawn(
            None,
            !self.offline,
            true,
            account.name.clone(),
            self.rules.clone(),
            self.custom_headers.clone(),
            self.rules_mailbox_regex.clone(),
            self.my_email_regex.clone(),
            self.spam_config.clone(),
        );
        let _ = cmd_tx.send(BackendCommand::FetchMailboxes {
            origin: origin.to_string(),
        });
        if let Some(connector) = &self.connector {
            let _ = connector.send((account.clone(), cmd_tx.clone()));
        }
        (cmd_tx, resp_rx)
    }
}

/// A connection to make: the account, and the backend it is for.
type ConnectRequest = (AccountConfig, Sender<BackendCommand>);

/// The connector: the session's one thread for the credential and
/// discovery round trips, so however many times the account is switched
/// while a connection is pending, one is in flight (the package caps the
/// jail's tasks). It serves the latest request, since an account switched
/// away from needs no connection, and sends the backend its connection,
/// or the failure, on which the backend answers what it held; a backend
/// shut down meanwhile has dropped its receiver, and the send fails
/// unheard. A connection that fails leaves the account on its cache,
/// logged; selecting the account again retries. The thread ends with the
/// session, which holds the sender.
fn spawn_connector() -> Sender<ConnectRequest> {
    let (tx, rx) = mpsc::channel::<ConnectRequest>();
    std::thread::spawn(move || {
        while let Ok(mut request) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                request = newer;
            }
            let (account, cmd_tx) = request;
            crate::log_info!(
                "[Connect] connecting to {} ({})",
                account.name,
                account.well_known_url
            );
            let decision = match crate::connect_account(&account) {
                Ok(client) => {
                    crate::log_info!("[Connect] connected to {}", account.name);
                    BackendCommand::Connect(Box::new(client))
                }
                Err(e) => {
                    crate::log_error!(
                        "[Connect] {} failed: {}; on the cache until it is selected again",
                        account.name,
                        e
                    );
                    BackendCommand::ConnectFailed
                }
            };
            let _ = cmd_tx.send(decision);
        }
    });
    tx
}

struct Session {
    setup: Setup,
    stack: ViewStack,
    cmd_tx: Sender<BackendCommand>,
    resp_rx: Receiver<BackendResponse>,
    mouse: bool,
    /// The grid's rows as the window last laid it out, which the views'
    /// key handling takes as the terminal's height.
    rows: u16,
    sync_interval: Option<Duration>,
    last_user_activity: Instant,
    last_idle_sync: Instant,
    dirty: bool,
    quitting: bool,
    /// Reused per input so a wheel frame allocates nothing.
    keys: Vec<Key>,
}

impl Session {
    fn redraw(&mut self) {
        self.dirty = true;
    }

    fn act(&mut self, action: ViewAction) {
        match action {
            ViewAction::Continue => self.redraw(),
            ViewAction::Push(new_view) => {
                self.stack.push(new_view);
                self.redraw();
            }
            ViewAction::Pop => {
                if !self.stack.pop() {
                    self.quitting = true;
                    return;
                }
                // Let the revealed view refresh state that may have changed
                // while it was hidden (e.g. mailbox unread counts).
                if let Some(view) = self.stack.current_mut() {
                    view.on_reveal();
                }
                self.redraw();
            }
            ViewAction::Quit => self.quitting = true,
            ViewAction::Compose(draft_text) => {
                spawn_editor(&draft_text, &self.setup.editor_cmd);
                self.redraw();
            }
            ViewAction::SwitchAccount(name) => self.switch_account(&name),
        }
    }

    /// Switches to the named account as the session started on the first:
    /// its mailbox list at once, its connection when the connector has
    /// it. The old backend is told to shut down first; one still finishing
    /// a request holds its store's lock a while longer, and the backend of
    /// an account switched away from and back opens the store once it is
    /// free, blocking nothing meanwhile.
    fn switch_account(&mut self, name: &str) {
        let Some(account) = self.setup.accounts.iter().find(|a| a.name == name) else {
            return;
        };
        let _ = self.cmd_tx.send(BackendCommand::Shutdown);
        let (cmd_tx, resp_rx) = self.setup.open_account(account, "switch_account");
        let mailbox_view = self.setup.mailbox_view(&cmd_tx, account);
        self.cmd_tx = cmd_tx;
        self.resp_rx = resp_rx;
        self.stack = ViewStack::new(Box::new(mailbox_view));
        self.last_idle_sync = Instant::now();
        self.redraw();
    }

    /// A pending action the top view raised from a response or a click
    /// that rendered its feedback first.
    fn take_pending(&mut self) {
        let Some(view) = self.stack.current_mut() else {
            return;
        };
        match view.take_pending_action() {
            Some(ViewAction::Push(new_view)) => {
                self.stack.push(new_view);
                self.redraw();
            }
            Some(ViewAction::Compose(draft_text)) => {
                spawn_editor(&draft_text, &self.setup.editor_cmd);
                self.redraw();
            }
            _ => {}
        }
    }

    fn wants_mouse(&self) -> bool {
        self.mouse && self.stack.current().map(|v| v.wants_mouse()).unwrap_or(true)
    }
}

impl Handler for Session {
    fn title(&self) -> &str {
        "Mail"
    }

    fn app_id(&self) -> &str {
        "td-mail"
    }

    fn ground(&self) -> Style {
        screen::base_style(&self.setup.theme)
    }

    fn input(&mut self, input: Input) -> Flow {
        match input {
            Input::Close => return Flow::Quit,
            Input::Resize { rows, .. } => {
                self.rows = u16::try_from(rows).unwrap_or(u16::MAX);
                self.redraw();
                return Flow::Continue;
            }
            _ => {}
        }
        self.keys.clear();
        input::translate(input, self.wants_mouse(), &mut self.keys);
        let keys = std::mem::take(&mut self.keys);
        for key in keys.iter().cloned() {
            if self.quitting {
                break;
            }
            self.last_user_activity = Instant::now();
            match self.stack.handle_key(key, self.rows) {
                Some(action) => self.act(action),
                None => self.quitting = true,
            }
        }
        self.keys = keys;
        if self.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn poll(&mut self, _now: u64) -> Flow {
        // A backend that is gone answers nothing more; the views stay up
        // on what they hold, as they did in the terminal.
        while let Ok(response) = self.resp_rx.try_recv() {
            if self.stack.handle_response(&response) {
                self.redraw();
            }
            if let Some(view) = self.stack.current_mut() {
                if let Some(ViewAction::Compose(draft_text)) = view.take_pending_action() {
                    spawn_editor(&draft_text, &self.setup.editor_cmd);
                    self.redraw();
                }
            }
        }
        self.take_pending();
        if let Some(interval) = self.sync_interval {
            if self.last_user_activity.elapsed() >= interval
                && self.last_idle_sync.elapsed() >= interval
            {
                if let Some(view) = self.stack.current_mut() {
                    if view.trigger_idle_sync() {
                        self.last_idle_sync = Instant::now();
                    }
                }
            }
        }
        if self.quitting {
            Flow::Quit
        } else {
            Flow::Continue
        }
    }

    fn wait_ms(&self, _now: u64) -> u64 {
        POLL_MS
    }

    fn needs_redraw(&self) -> bool {
        self.dirty
    }

    fn render(&mut self, screen: &mut Screen) {
        let mut term = Terminal::new(screen, self.setup.theme.clone());
        // The grid the views key against is the one they were drawn on.
        self.rows = term.rows;
        if let Err(e) = self.stack.render_current(&mut term) {
            crate::log_error!("render failed: {}", e);
        }
        self.dirty = false;
    }

    fn notice(&mut self, message: &str) {
        crate::log_error!("window: {}", message);
    }
}

/// Runs the client in a window on the compositor the environment names,
/// until the window closes or the top view quits. The window opens before
/// the account connects, its mailbox list loading until the connector
/// has decided the connection, so the toplevel is up within the unit's
/// readiness wait however long discovery takes.
#[allow(clippy::too_many_arguments)]
pub fn run(
    accounts: Vec<AccountConfig>,
    current_account_idx: usize,
    page_size: u32,
    scrolloff: usize,
    editor: Option<String>,
    browser: Option<String>,
    mouse: bool,
    sync_interval_secs: Option<u64>,
    archive_folder: String,
    deleted_folder: String,
    reply_from: Option<String>,
    rules_mailbox_regex: UserRegex,
    my_email_regex: UserRegex,
    retention_policies: Vec<RetentionPolicyConfig>,
    rules: Vec<CompiledRule>,
    custom_headers: Vec<String>,
    theme: Theme,
    spam_config: SpamConfig,
    offline: bool,
) -> io::Result<()> {
    // Read before the backend thread is spawned and the window opened, so
    // an index the account list does not hold fails with nothing to close.
    let Some(first) = accounts.get(current_account_idx) else {
        return Err(io::Error::other(format!(
            "no account at index {} of {}",
            current_account_idx,
            accounts.len()
        )));
    };
    let first = first.clone();
    let editor_cmd = editor
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_string());
    let setup = Setup {
        account_names: accounts.iter().map(|a| a.name.clone()).collect(),
        accounts,
        page_size,
        scrolloff,
        browser,
        reply_from,
        archive_folder,
        deleted_folder,
        retention_policies,
        sync_interval_secs,
        rules: Arc::new(rules),
        custom_headers: Arc::new(custom_headers),
        // Both patterns were compiled where the configuration was read.
        rules_mailbox_regex: Arc::new(rules_mailbox_regex),
        my_email_regex: Arc::new(my_email_regex),
        spam_config,
        offline,
        editor_cmd,
        theme,
        connector: (!offline).then(spawn_connector),
    };

    let endpoint = td_ui::wayland::endpoint(
        std::env::var_os("WAYLAND_SOCKET"),
        std::env::var_os("WAYLAND_DISPLAY"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    )
    .map_err(io::Error::other)?;
    let stream = td_ui::wayland::connect(endpoint).map_err(io::Error::other)?;

    let (cmd_tx, resp_rx) = setup.open_account(&first, "startup");
    let mailbox_view = setup.mailbox_view(&cmd_tx, &first);

    let mut session = Session {
        stack: ViewStack::new(Box::new(mailbox_view)),
        cmd_tx,
        resp_rx,
        mouse,
        rows: 0,
        sync_interval: sync_interval_secs.map(Duration::from_secs),
        last_user_activity: Instant::now(),
        last_idle_sync: Instant::now(),
        dirty: true,
        quitting: false,
        keys: Vec::new(),
        setup,
    };
    let outcome = td_ui::screen_app::run(&mut session, stream, std::env::temp_dir());
    let _ = session.cmd_tx.send(BackendCommand::Shutdown);
    // The backend answers what it held from the cache on the way out;
    // wait for it to go, up to two seconds of silence, so a mutation it
    // queues is written before the process ends. One mid-request is left
    // to the exit.
    let mut silent = 0;
    while silent < 20 {
        match session.resp_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => silent += 1,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    outcome.map_err(io::Error::other)
}

fn spawn_editor(draft: &compose::ComposeDraft, editor_cmd: &str) {
    // Retain the draft and sidecars independently of the editor's lifetime.
    let prepared = match compose::write_compose_draft(draft) {
        Ok(prepared) => prepared,
        Err(e) => {
            crate::log_error!("Failed to retain draft: {}", e);
            return;
        }
    };

    crate::log_info!("Draft retained at {}", prepared.draft_path.display());
    if let Some(path) = &prepared.attachment_dir {
        crate::log_info!("Draft attachments retained at {}", path.display());
    }
    match launch_draft_editor(&prepared, editor_cmd) {
        Ok(child) => reap_in_background(child),
        Err(e) => crate::log_error!("Failed to spawn editor; draft retained: {}", e),
    }
}

fn launch_draft_editor(
    prepared: &compose::PreparedDraft,
    editor_cmd: &str,
) -> io::Result<std::process::Child> {
    let editor_cmd = editor_cmd.trim();
    if editor_cmd.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "editor command is empty",
        ));
    }
    // A command of plain words needs no shell and gets none: the program is
    // executed directly with the draft path as its last argument, which is
    // what `sh -c 'cmd "$1"'` does for such a command, on the application
    // runtime that has no `sh` (the `mail` package ships `/app/bin/td-editor`
    // as `$EDITOR` on the data-only static runtime) as much as on a host.
    if let Some((program, args)) = plain_command(editor_cmd) {
        return std::process::Command::new(program)
            .args(args)
            .arg(&prepared.draft_path)
            .spawn();
    }
    // Anything else is shell text. Preserve the OS path as one quoted
    // positional argument, not part of that text.
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} \"$1\"", editor_cmd))
        .arg("sh")
        .arg(&prepared.draft_path)
        .spawn()
}

/// A byte a shell passes through unchanged in an unquoted word.
fn plain_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"._/+:@,-".contains(&byte)
}

/// Leading words made of plain bytes that a shell interprets itself rather
/// than resolving by `PATH`: its reserved words, and the builtins that
/// have no identically behaving utility on `PATH`. A command starting with
/// one keeps the shell, since executing it directly would look for a
/// program of that name. A builtin with an identical external utility
/// (`true`, `pwd`) need not be listed: the direct path runs the utility.
const SHELL_WORDS: &[&str] = &[
    ".", ":", "alias", "bg", "break", "builtin", "case", "cd", "command",
    "continue", "do", "done", "elif", "else", "esac", "eval", "exec", "exit",
    "export", "fc", "fg", "fi", "for", "function", "getopts", "hash", "if",
    "in", "jobs", "local", "read", "readonly", "return", "select", "set",
    "shift", "source", "then", "time", "times", "trap", "type", "ulimit",
    "umask", "unalias", "unset", "until", "wait", "while",
];

/// The program and arguments of an editor command that needs no shell:
/// words of ASCII letters, digits and `._/+:@,-`, plus `=` after the first
/// word where a shell passes it through unchanged, separated by spaces or
/// tabs, whose first word is not one a shell interprets itself. Anything
/// else (quotes, `$`, redirections, globs, `~`, `#`, a leading assignment,
/// a newline or other control byte, a non-ASCII byte) makes the whole
/// command shell text: `None`. The `mail` package names
/// `/app/bin/td-editor` as `$EDITOR` (recipes/src/recipes/mail.rs) and its
/// runtime has no shell, so that value must stay one this accepts; the
/// test below pins it.
fn plain_command(editor_cmd: &str) -> Option<(&str, Vec<&str>)> {
    if editor_cmd
        .bytes()
        .any(|byte| byte.is_ascii_control() && byte != b'\t')
    {
        return None;
    }
    let mut words = editor_cmd.split([' ', '\t']).filter(|word| !word.is_empty());
    let program = words.next()?;
    if !program.bytes().all(plain_byte) || SHELL_WORDS.contains(&program) {
        return None;
    }
    let args: Vec<&str> = words.collect();
    if args
        .iter()
        .any(|word| !word.bytes().all(|byte| plain_byte(byte) || byte == b'='))
    {
        return None;
    }
    Some((program, args))
}

#[cfg(test)]
mod draft_tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn editor_exit_keeps_saved_draft_and_attachments_with_exact_path_bytes() -> io::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "td-mail-editor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let path = root.join(std::ffi::OsString::from_vec(b"draft ;$' \xff.eml".to_vec()));
        let sidecar = root.join("attachments");
        std::fs::create_dir(&sidecar)?;
        std::fs::write(sidecar.join("original"), b"attachment")?;
        let prepared = compose::PreparedDraft {
            draft_path: path.clone(),
            attachment_dir: Some(sidecar.clone()),
        };
        assert!(launch_draft_editor(&prepared, "  ").is_err());
        // A plain-word command is executed directly: an absent program is a
        // spawn error here, where a shell would have returned a child that
        // exits 127. That difference is the proof no shell stood between.
        assert!(launch_draft_editor(&prepared, "/definitely-absent-td-editor").is_err());
        // The direct path hands the program the exact path bytes as its
        // last argument: `cp SOURCE` receives the draft path, `\xff` and
        // all, and writes the source's bytes there. The command is plain
        // words, so this is the direct path and not the shell's.
        std::fs::write(root.join("source"), b"saved")?;
        let direct = format!("cp {}", root.join("source").display());
        assert!(plain_command(&direct).is_some(), "{direct}");
        std::fs::write(&path, b"original")?;
        assert!(launch_draft_editor(&prepared, &direct)?.wait()?.success());
        assert_eq!(std::fs::read(&path)?, b"saved");
        // Shell text keeps the shell: a leading builtin the shell must
        // resolve, and an interior newline the shell reads as a command
        // separator, with `"$1"` still landing on the last line.
        for (command, success, saved) in [
            ("printf saved > \"$1\"; exit 0 #", true, true),
            ("true\nprintf saved > \"$1\"; exit 0 #", true, true),
            ("exit 7 #", false, false),
            ("exec /definitely-absent-td-editor", false, false),
            ("sh -c 'exit 7' sh #", false, false),
        ] {
            std::fs::write(&path, b"original")?;
            let status = launch_draft_editor(&prepared, command)?.wait()?;
            assert_eq!(status.success(), success);
            assert_eq!(
                std::fs::read(&path)?,
                if saved {
                    b"saved".as_slice()
                } else {
                    b"original"
                }
            );
            assert_eq!(std::fs::read(sidecar.join("original"))?, b"attachment");
        }
        // The background reaper receives only Child, never retained paths.
        let source = include_str!("mod.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("remove_file"));
        assert!(!production.contains("remove_dir_all"));
        std::fs::remove_dir_all(root)
    }

    /// The direct path takes exactly the commands a shell would run
    /// unchanged; every construct a shell would interpret keeps the shell.
    #[test]
    fn plain_words_run_without_a_shell_and_shell_text_keeps_one() {
        assert_eq!(
            plain_command("/app/bin/td-editor"),
            Some(("/app/bin/td-editor", vec![]))
        );
        assert_eq!(
            plain_command("  emacs -nw\t--keys=x  "),
            Some(("emacs", vec!["-nw", "--keys=x"])),
            "spaces and tabs separate; `=` after the first word is literal"
        );
        assert_eq!(
            plain_command("/usr/bin/vi.1+2:3@4,5-6"),
            Some(("/usr/bin/vi.1+2:3@4,5-6", vec![]))
        );
        for text in [
            "",
            " \t",
            "vim -u ~/.vimrc",
            "printf saved > \"$1\"",
            "exit 7 #",
            "a=b vi",
            "vi=x",
            "vi *",
            "vi 'x'",
            "vi\nx",
            "vi\rx",
            "vi\u{c}x",
            "vi\tx\u{1}",
            "vi;",
            "vi|less",
            "vi&",
            "vi (x)",
            "vi `x`",
            "vi \\x",
            "vi {x}",
            "vi [x]",
            "vi x?",
            "vi x!",
            "vi <x",
            "vi %x",
            "édit",
            "exec vim",
            "command emacs -nw",
            ":",
            ". vi",
            "eval vi",
            "time vi",
            "if vi",
        ] {
            assert_eq!(plain_command(text), None, "{text:?}");
        }
        // Every reserved word is one the byte rule would otherwise admit,
        // so the list is what keeps it on the shell; and a program named
        // like one is still reachable by its path.
        for word in SHELL_WORDS {
            assert!(word.bytes().all(plain_byte), "{word}");
            assert_eq!(plain_command(word), None, "{word}");
        }
        assert_eq!(plain_command("/bin/time vi"), Some(("/bin/time", vec!["vi"])));
    }
}
