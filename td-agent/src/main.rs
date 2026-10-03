//! td-agent's program: the window process by default, and a conversation
//! process when the window starts one (DESIGN.md §2).

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use td_agent::store::{Id, Role, StateDir};

const USAGE: &str = "usage: td-agent [--control-socket ABSOLUTE-PATH]\n\
\x20      td-agent --help\n\
\n\
td-agent is td's agent harness (td-agent/DESIGN.md). Its window lists the\n\
conversations, the orchestrator first, beside the open one. Each message\n\
is a turn with the configured model through OpenRouter, by way of td's\n\
fetch service: run it as ./agent from a td checkout, which serves that.\n\
\n\
Keys: C-Return sends the composer (Return is a newline); C-r asks a\n\
failed turn again; C-n starts a conversation; C-PageUp and C-PageDown\n\
open the one above or below; F6 and S-F6 move the focus between the\n\
list, the transcript and the composer; F10 opens the menus: File's\n\
Set OpenRouter key... stores the key, and Conversation's Model... and\n\
Effort choose the open conversation's model and reasoning effort. The\n\
control socket speaks td-ui's driven protocol.\n\
\n\
State: $XDG_STATE_HOME/td-agent. Configuration:\n\
$XDG_CONFIG_HOME/td-agent/config (TOML; unknown keys are refused). The\n\
API key: one line in $XDG_CONFIG_HOME/td-agent/openrouter.key, a file\n\
of your own, mode 0600, in directories only you and root can write,\n\
which File > Set OpenRouter key... writes.\n";

/// `td-agent conversation ID --state-dir DIR [--create ROLE]`: the
/// window starts these; a person does not.
fn conversation(args: &[String]) -> Result<(), String> {
    let usage = "usage: td-agent conversation ID --state-dir ABSOLUTE-DIR [--create ROLE]";
    let (id, rest) = args.split_first().ok_or(usage)?;
    let id = Id::parse(id).ok_or_else(|| format!("{id:?} is not a conversation id"))?;
    let (state, create) = match rest {
        [flag, dir] if flag == "--state-dir" => (dir, None),
        [flag, dir, create, role] if flag == "--state-dir" && create == "--create" => {
            let role = Role::parse(role).ok_or_else(|| format!("{role:?} is not a role"))?;
            (dir, Some(role))
        }
        _ => return Err(usage.into()),
    };
    let state = PathBuf::from(state);
    if !state.is_absolute() {
        return Err(format!("{} is not an absolute path", state.display()));
    }
    td_agent::conversation::run(&StateDir::at(state), &id, create)
}

fn window(args: &[String]) -> Result<(), String> {
    let control = match args {
        [] => None,
        [flag, path] if flag == "--control-socket" => {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(format!(
                    "the control socket {} is not an absolute path",
                    path.display()
                ));
            }
            Some(path)
        }
        _ => return Err(USAGE.trim_end().into()),
    };
    let config_path = td_agent::config::path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    );
    if config_path.is_none() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "td-agent: no configuration is read: neither XDG_CONFIG_HOME nor HOME is an absolute path"
        );
    }
    let config = td_agent::config::load(config_path.as_deref())?;
    // The key file beside the configuration; there is no other form.
    let key_path = config_path.as_deref().and_then(td_agent::key::path);
    let key = match key_path.as_deref() {
        Some(path) => td_agent::key::read(path).map_err(|problem| problem.to_string()),
        None => Err(
            "no API key: neither XDG_CONFIG_HOME nor HOME is an absolute path to find it under"
                .to_string(),
        ),
    };
    let state = StateDir::from_env(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))?;
    let program = std::env::current_exe().map_err(|e| format!("this program's path: {e}"))?;
    td_agent::window::run(config, key, key_path, state, program, control)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((first, rest)) if first == "conversation" => conversation(rest),
        Some((first, [])) if first == "--help" || first == "-h" => {
            let _ = std::io::stdout().lock().write_all(USAGE.as_bytes());
            return ExitCode::SUCCESS;
        }
        _ => window(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "td-agent: {e}");
            ExitCode::FAILURE
        }
    }
}
