pub mod input;
pub mod screen;
pub mod views;

use crate::backend::{self, BackendCommand};
use crate::compose;
use crate::config::{AccountConfig, RetentionPolicyConfig, SpamConfig, Theme};
use crate::jmap::client::JmapClient;
use crate::regex::UserRegex;
use crate::rules::CompiledRule;
use input::read_key;
use screen::Terminal;
use std::io;
use std::time::{Duration, Instant};
use views::mailbox_list::MailboxListView;
use views::{ViewAction, ViewStack};

fn sync_mouse_for_view(term: &mut Terminal, stack: &ViewStack) -> io::Result<()> {
    let wants_mouse = stack.current().map(|v| v.wants_mouse()).unwrap_or(true);
    term.set_mouse_enabled(wants_mouse)
}

/// Wait on a fire-and-forget child in a detached thread so it does not linger
/// as a zombie. `Child` has no reaping `Drop` impl, so a dropped handle leaks a
/// PID slot for the lifetime of td-mail.
pub fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    client: Option<JmapClient>,
    accounts: Vec<AccountConfig>,
    current_account_idx: usize,
    initial_account_name: String,
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
    // Read before the backend thread is spawned and the terminal taken over,
    // so an index the account list does not hold fails on a plain screen with
    // nothing to clean up.
    let Some(first) = accounts.get(current_account_idx) else {
        return Err(io::Error::other(format!(
            "no account at index {} of {}",
            current_account_idx,
            accounts.len()
        )));
    };
    let (first_username, first_name) = (first.username.clone(), first.name.clone());

    let rules = std::sync::Arc::new(rules);
    let custom_headers = std::sync::Arc::new(custom_headers);
    // Both patterns were compiled where the configuration was read.
    let rules_mailbox_regex = std::sync::Arc::new(rules_mailbox_regex);
    let my_email_regex = std::sync::Arc::new(my_email_regex);
    let (mut cmd_tx, mut resp_rx) = backend::spawn(
        client,
        initial_account_name,
        rules.clone(),
        custom_headers.clone(),
        rules_mailbox_regex.clone(),
        my_email_regex.clone(),
        spam_config.clone(),
    );
    let mut term = Terminal::new(mouse, theme)?;

    let account_names: Vec<String> = accounts.iter().map(|a| a.name.clone()).collect();

    let mailbox_view = MailboxListView::new(
        cmd_tx.clone(),
        first_username,
        reply_from.clone(),
        browser.clone(),
        page_size,
        scrolloff,
        account_names.clone(),
        first_name,
        archive_folder.clone(),
        deleted_folder.clone(),
        retention_policies.clone(),
        sync_interval_secs,
    );
    let _ = cmd_tx.send(BackendCommand::FetchMailboxes {
        origin: "startup".to_string(),
    });

    let mut stack = ViewStack::new(Box::new(mailbox_view));
    let sync_interval = sync_interval_secs.map(Duration::from_secs);
    let mut last_user_activity = Instant::now();
    let mut last_idle_sync = Instant::now();

    let editor_cmd = editor
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| "vi".to_string());

    sync_mouse_for_view(&mut term, &stack)?;
    stack.render_current(&mut term)?;

    loop {
        if term.check_resize() {
            sync_mouse_for_view(&mut term, &stack)?;
            stack.render_current(&mut term)?;
        }

        let mut needs_render = false;
        while let Ok(response) = resp_rx.try_recv() {
            if stack.handle_response(&response) {
                needs_render = true;
            }

            if let Some(view) = stack.current_mut() {
                if let Some(ViewAction::Compose(draft_text)) = view.take_pending_action() {
                    spawn_editor(&draft_text, &editor_cmd);
                    needs_render = true;
                }
            }
        }
        if needs_render {
            sync_mouse_for_view(&mut term, &stack)?;
            stack.render_current(&mut term)?;
        }

        // Check for pending actions (e.g. mouse click that rendered feedback first)
        if let Some(view) = stack.current_mut() {
            if let Some(action) = view.take_pending_action() {
                match action {
                    ViewAction::Push(new_view) => {
                        stack.push(new_view);
                        sync_mouse_for_view(&mut term, &stack)?;
                        stack.render_current(&mut term)?;
                    }
                    ViewAction::Compose(draft_text) => {
                        spawn_editor(&draft_text, &editor_cmd);
                        sync_mouse_for_view(&mut term, &stack)?;
                        stack.render_current(&mut term)?;
                    }
                    _ => {}
                }
            }
        }

        if let Some(key) = read_key() {
            last_user_activity = Instant::now();
            let action = match stack.handle_key(key, term.rows) {
                Some(action) => action,
                None => break,
            };

            match action {
                ViewAction::Continue => {
                    sync_mouse_for_view(&mut term, &stack)?;
                    stack.render_current(&mut term)?;
                }
                ViewAction::Push(new_view) => {
                    stack.push(new_view);
                    sync_mouse_for_view(&mut term, &stack)?;
                    stack.render_current(&mut term)?;
                }
                ViewAction::Pop => {
                    if !stack.pop() {
                        break;
                    }
                    // Let the revealed view refresh state that may have changed
                    // while it was hidden (e.g. mailbox unread counts).
                    if let Some(view) = stack.current_mut() {
                        view.on_reveal();
                    }
                    sync_mouse_for_view(&mut term, &stack)?;
                    stack.render_current(&mut term)?;
                }
                ViewAction::Quit => {
                    break;
                }
                ViewAction::Compose(draft_text) => {
                    spawn_editor(&draft_text, &editor_cmd);
                    sync_mouse_for_view(&mut term, &stack)?;
                    stack.render_current(&mut term)?;
                }
                ViewAction::SwitchAccount(name) => {
                    if let Some(account) = accounts.iter().find(|a| a.name == name) {
                        // Shut down old backend
                        let _ = cmd_tx.send(BackendCommand::Shutdown);

                        let new_client = if offline {
                            Ok(None)
                        } else {
                            match crate::connect_account(account) {
                                Ok(c) => Ok(Some(c)),
                                Err(e) => Err(e),
                            }
                        };

                        match new_client {
                            Ok(client) => {
                                let (new_cmd_tx, new_resp_rx) = backend::spawn(
                                    client,
                                    account.name.clone(),
                                    rules.clone(),
                                    custom_headers.clone(),
                                    rules_mailbox_regex.clone(),
                                    my_email_regex.clone(),
                                    spam_config.clone(),
                                );
                                cmd_tx = new_cmd_tx;
                                resp_rx = new_resp_rx;

                                let mailbox_view = MailboxListView::new(
                                    cmd_tx.clone(),
                                    account.username.clone(),
                                    reply_from.clone(),
                                    browser.clone(),
                                    page_size,
                                    scrolloff,
                                    account_names.clone(),
                                    account.name.clone(),
                                    archive_folder.clone(),
                                    deleted_folder.clone(),
                                    retention_policies.clone(),
                                    sync_interval_secs,
                                );
                                let _ = cmd_tx.send(BackendCommand::FetchMailboxes {
                                    origin: "switch_account".to_string(),
                                });
                                stack = ViewStack::new(Box::new(mailbox_view));
                                last_idle_sync = Instant::now();
                            }
                            Err(e) => {
                                crate::log_error!("Failed to connect to account {}: {}", name, e);
                                // Stay on current account, just re-render
                            }
                        }
                        sync_mouse_for_view(&mut term, &stack)?;
                        stack.render_current(&mut term)?;
                    }
                }
            }
        } else if let Some(interval) = sync_interval {
            if last_user_activity.elapsed() >= interval && last_idle_sync.elapsed() >= interval {
                if let Some(view) = stack.current_mut() {
                    if view.trigger_idle_sync() {
                        last_idle_sync = Instant::now();
                    }
                }
            }
        }
    }

    let _ = cmd_tx.send(BackendCommand::Shutdown);

    Ok(())
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
    // Preserve the OS path as one quoted positional argument, not shell text.
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} \"$1\"", editor_cmd))
        .arg("sh")
        .arg(&prepared.draft_path)
        .spawn()
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
        for (command, success, saved) in [
            ("printf saved > \"$1\"; exit 0 #", true, true),
            ("test -f\n", true, false),
            ("exit 7 #", false, false),
            ("exec /definitely-absent-td-editor", false, false),
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
}
