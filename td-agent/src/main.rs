//! td-agent's program: the window by default, the broker as `serve`, what
//! joins a far window to it as `attach`, a conversation process when the
//! broker starts one, and the tool host a conversation
//! starts for its tools (DESIGN.md §2).

#![forbid(unsafe_code)]

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use td_agent::store::{Id, Role, StateDir};
use td_agent::workspace::Workspace;

const USAGE: &str = "usage: td-agent [--control-socket ABSOLUTE-PATH]\n\
\x20                [--remote HOST [--remote-program PATH]]\n\
\x20      td-agent serve [--stop]\n\
\x20      td-agent attach\n\
\x20      td-agent review --commit REV [--model opus-latest|gpt-sol-latest]\n\
\x20                      [--max-cost USD] [--repo DIRECTORY]\n\
\x20      td-agent review-log FILE\n\
\x20      td-agent review-logs [DIRECTORY] [--day YYYY-MM-DD]\n\
\x20                           [--key-fingerprint SHA256] [--billed-cost USD]\n\
\x20      td-agent calibrate FIXTURES\n\
\x20      td-agent --help\n\
\n\
td-agent is td's agent harness (td-agent/DESIGN.md). Its window lists the\n\
conversations, the most recently active first, beside the open one.\n\
Each message is a turn with the configured model through OpenRouter, by\n\
way of td's fetch service: on td, start it from the launcher's CODING\n\
AGENT card or reach it with --remote; on a host, run it as ./install-apps\n\
installs it from a td checkout, whose launch serves that.\n\
\n\
The conversations run in td-agent serve, which the window starts when none\n\
is running: closing the window leaves what is running going on, and serve\n\
ends when nothing is. td-agent --remote HOST opens a window here on HOST's\n\
conversations, over ssh, which needs td-agent there; --remote-program PATH\n\
finds it off a non-login PATH. It runs td-agent attach there, which joins\n\
standard input and output to HOST's td-agent serve.\n\
\n\
Keys: Return in the composer sends it (S-Return is a newline, and\n\
C-Return sends from outside a dialog); C-r asks a failed turn again;\n\
C-n starts a conversation from a workspace template\n\
(Empty, Directory..., one configured, or Empty or Directory... unconfined,\n\
whose tools run outside any jail; td-agent/DESIGN.md §7, §8), or\n\
with no workspace when its jail is absent (unconfined ones only, with\n\
unconfined_without_jail); C-PageUp and C-PageDown\n\
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
td-agent review --commit REV reviews a local commit from the current\n\
directory at high effort, prepares its own read-only checkout and scratch,\n\
and writes the report to standard output. td-agent review --help says more.\n\
\n\
td-agent calibrate puts the classifier fixtures in FIXTURES to both of\n\
its stages, live, and counts their false allows and escalations;\n\
td-agent calibrate --help says more.\n";

/// `td-agent conversation ID --state-dir DIR [--create ROLE [--workspace
/// WORKSPACE]]`: the window starts these; a person does not.
fn conversation(args: &[String]) -> Result<(), String> {
    let usage = "usage: td-agent conversation ID --state-dir ABSOLUTE-DIR \
                 [--create ROLE [--workspace scratch|template:NAME|ABSOLUTE-DIR [--unconfined]]]";
    let (id, rest) = args.split_first().ok_or(usage)?;
    let id = Id::parse(id).ok_or_else(|| format!("{id:?} is not a conversation id"))?;
    let role = |role: &str| Role::parse(role).ok_or_else(|| format!("{role:?} is not a role"));
    let (state, create, workspace, unconfined) = match rest {
        [flag, dir] if flag == "--state-dir" => (dir, None, None, false),
        [flag, dir, create, named] if flag == "--state-dir" && create == "--create" => {
            (dir, Some(role(named)?), None, false)
        }
        [flag, dir, create, named, inside, word, unconfined @ ..]
            if flag == "--state-dir"
                && create == "--create"
                && inside == "--workspace"
                && matches!(unconfined, [] | [_]) =>
        {
            let unconfined = match unconfined {
                [] => false,
                [word] if word == "--unconfined" => true,
                _ => return Err(usage.into()),
            };
            (
                dir,
                Some(role(named)?),
                Some(Workspace::parse_argument(word)?),
                unconfined,
            )
        }
        _ => return Err(usage.into()),
    };
    let state = PathBuf::from(state);
    if !state.is_absolute() {
        return Err(format!("{} is not an absolute path", state.display()));
    }
    td_agent::conversation::run(&StateDir::at(state), &id, create, workspace, unconfined)
}

/// What a window was asked for.
#[derive(Debug, Default)]
struct WindowArgs {
    control: Option<PathBuf>,
    host: Option<td_remote::Host>,
    program: Option<td_remote::RemoteProgram>,
}

fn window_args(args: &[String]) -> Result<WindowArgs, String> {
    let mut parsed = WindowArgs::default();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let Some(value) = args.next() else {
            return Err(USAGE.trim_end().into());
        };
        match flag.as_str() {
            "--control-socket" if parsed.control.is_none() => {
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(format!(
                        "the control socket {} is not an absolute path",
                        path.display()
                    ));
                }
                parsed.control = Some(path);
            }
            "--remote" if parsed.host.is_none() => {
                parsed.host = Some(td_remote::Host::parse(value).map_err(|e| e.to_string())?);
            }
            "--remote-program" if parsed.program.is_none() => {
                parsed.program =
                    Some(td_remote::RemoteProgram::parse(value).map_err(|e| e.to_string())?);
            }
            _ => return Err(USAGE.trim_end().into()),
        }
    }
    Ok(parsed)
}

fn window(args: &[String]) -> Result<(), String> {
    let WindowArgs {
        control,
        host,
        program,
    } = window_args(args)?;
    let broker = match (host, program) {
        (None, None) => td_agent::window::Broker::Here {
            state: state()?,
            program: self::program()?,
        },
        (None, Some(_)) => return Err("--remote-program needs --remote".into()),
        (Some(host), program) => td_agent::window::Broker::Over {
            host,
            program: match program {
                Some(program) => program,
                None => td_remote::RemoteProgram::parse("td-agent").map_err(|e| e.to_string())?,
            },
        },
    };
    td_agent::window::run(broker, control)
}

fn state() -> Result<StateDir, String> {
    StateDir::from_env(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn program() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("this program's path: {e}"))
}

/// The broker: the one that reads the configuration and holds the key.
fn serve() -> Result<(), String> {
    // Its services are named by the environment, read all through this program, so
    // what it lacks comes by starting over with it: only then is it all there.
    let lacking = td_agent::window::system_environment(&program()?);
    if !lacking.is_empty() {
        let again = Command::new(program()?)
            .args(std::env::args_os().skip(1))
            .envs(lacking)
            .exec();
        return Err(format!("td-agent serve: {again}"));
    }
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
    td_agent::broker::run(config, key, key_path, state()?, program()?)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((first, rest)) if first == "conversation" => conversation(rest),
        Some((first, [])) if first == "serve" => serve(),
        Some((first, [flag])) if first == "serve" && flag == "--stop" => {
            state().and_then(|state| td_agent::window::stop(&state))
        }
        Some((first, [])) if first == "attach" => {
            state().and_then(|state| td_agent::window::attach(&state, &program()?))
        }
        Some((first, rest)) if first == "review-logs" => td_agent::review_inventory::run(rest),
        Some((first, rest)) if first == "review-log" => td_agent::review_metrics::run(rest),
        Some((first, rest)) if first == "review" => td_agent::review::run(rest),
        Some((first, rest)) if first == "calibrate" => td_agent::calibrate::run(rest),
        Some((first, [])) if first == "workspace-oracle" => td_agent::workspace_oracle::run(),
        Some((first, rest)) if first == "workspace-import" => {
            td_agent::workspace_upgrade::inside(rest)
        }
        Some((first, rest)) if first == "workspace-probe" => {
            td_agent::workspace_oracle::inside(rest)
        }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn args(args: &[&str]) -> Result<WindowArgs, String> {
        window_args(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn a_window_takes_each_of_its_options_once_and_refuses_the_rest() {
        let parsed = args(&[]).unwrap();
        assert!(parsed.control.is_none() && parsed.host.is_none());
        let parsed = args(&[
            "--remote",
            "me@box",
            "--remote-program",
            "bin/td-agent",
            "--control-socket",
            "/a",
        ])
        .unwrap();
        assert_eq!(parsed.host.unwrap().as_str(), "me@box");
        assert_eq!(parsed.control.unwrap(), PathBuf::from("/a"));
        for refused in [
            &["--remote"][..],
            &["--remote", "-oProxyCommand=x"],
            &["--remote", "a", "--remote", "b"],
            &["--remote-program", "a b", "--remote", "c"],
            &["--control-socket", "relative"],
            &["--other", "x"],
        ] {
            assert!(args(refused).is_err(), "{refused:?}");
        }
        assert!(window(&["--remote-program".into(), "x".into()])
            .unwrap_err()
            .contains("needs --remote"));
    }
}
