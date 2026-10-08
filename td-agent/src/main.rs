//! td-agent's program: the window process by default, a conversation
//! process when the window starts one, and the tool host a conversation
//! starts for its tools (DESIGN.md §2).

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use td_agent::store::{Id, Role, StateDir};
use td_agent::workspace::Workspace;

const USAGE: &str = "usage: td-agent [--control-socket ABSOLUTE-PATH]\n\
\x20      td-agent review [--model MODEL] [--effort LEVEL] [--max-tokens N] [--] [FILE]\n\
\x20      td-agent calibrate FIXTURES\n\
\x20      td-agent --help\n\
\n\
td-agent is td's agent harness (td-agent/DESIGN.md). Its window lists the\n\
conversations, the most recently active first, beside the open one.\n\
Each message is a turn with the configured model through OpenRouter, by\n\
way of td's fetch service: on td, start it from the launcher's CODING\n\
AGENT card; on a host, run it as ./install-apps installs it from a td\n\
checkout, whose launch serves that.\n\
\n\
Keys: Return in the composer sends it (S-Return is a newline, and\n\
C-Return sends from outside a dialog); C-r asks a failed turn again;\n\
C-n starts a conversation from a workspace template\n\
(Empty, Directory... or one configured; td-agent/DESIGN.md §7), or\n\
with no workspace when its jail is absent; C-PageUp and C-PageDown\n\
open the one above or below; C-S-m opens the Messages window, which\n\
keeps td-agent's notes whole and with their times (the status row\n\
counts the unread); F6 and S-F6 move the focus between the\n\
list, the transcript and the composer; F10 opens the menus: File's\n\
Set OpenRouter key... stores the key and Export diagnostics writes an\n\
archive of the state and configuration, never the key file, to\n\
~/Downloads (else the home directory);\n\
Conversation's Model... and Effort choose the open conversation's model\n\
and reasoning effort, Default model... the model new conversations use,\n\
and Delete conversation... deletes the open one for good; F1, or\n\
Help > Keys, lists every key. The control socket speaks td-ui's driven\n\
protocol.\n\
\n\
State: $XDG_STATE_HOME/td-agent. Configuration:\n\
$XDG_CONFIG_HOME/td-agent/config (TOML; unknown keys are refused). The\n\
API key: one line in $XDG_CONFIG_HOME/td-agent/openrouter.key, a file\n\
of your own, mode 0600, in directories only you and root can write,\n\
which File > Set OpenRouter key... writes.\n\
\n\
td-agent review reviews the git commit in FILE (or standard input) as\n\
`git show` prints it, with the same configuration and key, and writes\n\
the review to standard output; td-agent review --help says more.\n\
\n\
td-agent calibrate puts the classifier fixtures in FIXTURES to both of\n\
its stages, live, and counts their false allows and escalations;\n\
td-agent calibrate --help says more.\n\
\n\
td-agent check-jail runs one workspace instance as a conversation\n\
would, from TD_AGENT_JAIL and TD_AGENT_TXT, writes a file in it and\n\
runs git there, and prints TD-AGENT-JAIL-OK when all of it held.\n";

/// `td-agent conversation ID --state-dir DIR [--create ROLE [--workspace
/// WORKSPACE]]`: the window starts these; a person does not.
fn conversation(args: &[String]) -> Result<(), String> {
    let usage = "usage: td-agent conversation ID --state-dir ABSOLUTE-DIR \
                 [--create ROLE [--workspace scratch|template:NAME|ABSOLUTE-DIR]]";
    let (id, rest) = args.split_first().ok_or(usage)?;
    let id = Id::parse(id).ok_or_else(|| format!("{id:?} is not a conversation id"))?;
    let role = |role: &str| Role::parse(role).ok_or_else(|| format!("{role:?} is not a role"));
    let (state, create, workspace) = match rest {
        [flag, dir] if flag == "--state-dir" => (dir, None, None),
        [flag, dir, create, named] if flag == "--state-dir" && create == "--create" => {
            (dir, Some(role(named)?), None)
        }
        [flag, dir, create, named, inside, word]
            if flag == "--state-dir" && create == "--create" && inside == "--workspace" =>
        {
            (
                dir,
                Some(role(named)?),
                Some(Workspace::parse_argument(word)?),
            )
        }
        _ => return Err(usage.into()),
    };
    let state = PathBuf::from(state);
    if !state.is_absolute() {
        return Err(format!("{} is not an absolute path", state.display()));
    }
    td_agent::conversation::run(&StateDir::at(state), &id, create, workspace)
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
        Some((first, rest)) if first == "review" => td_agent::review::run(rest),
        Some((first, rest)) if first == "calibrate" => td_agent::calibrate::run(rest),
        Some((first, rest)) if first == "check-jail" => td_agent::check::run(rest),
        Some((first, rest)) if first == "tool-host" => td_agent::toolhost::Config::parse(rest)
            .and_then(|config| {
                td_agent::toolhost::serve(std::io::stdin(), std::io::stdout(), config)
            }),
        // A maintenance instance's entry: its answer is its one line out.
        Some((first, rest)) if first == td_agent::repo::MAINTAIN => {
            td_agent::repo::maintain(rest, &mut std::io::stdout().lock())
        }
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
