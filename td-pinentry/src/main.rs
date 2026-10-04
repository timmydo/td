//! td-pinentry: a td-ui window that asks for a passphrase on behalf of
//! gpg-agent (the pinentry protocol on standard input and output) or of
//! ssh and git (the askpass convention), on a foreign Wayland desktop
//! (`DESIGN.md`).
#![forbid(unsafe_code)]

mod app;
mod askpass;
mod assuan;
mod mode;
mod request;
mod window;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "usage: td-pinentry [--help | --version | PROMPT | pinentry options]";

const HELP: &str = "usage: td-pinentry [--help | --version | PROMPT | pinentry options]\n\
    Asks for a passphrase in a window on the Wayland session.\n\
    With no argument, or with gpg-agent's options, it speaks the pinentry \
    protocol: set `pinentry-program` to it in ~/.gnupg/gpg-agent.conf.\n\
    With one PROMPT it is an askpass program: name it in SSH_ASKPASS \
    (with SSH_ASKPASS_REQUIRE=force) or GIT_ASKPASS.";

/// How the program was asked to run.
#[derive(Debug, PartialEq)]
enum Invocation {
    Help,
    Version,
    Pinentry,
    Askpass(String),
}

/// No argument, or an option first (gpg-agent passes `--display` and the
/// like, which the protocol's `OPTION` lines repeat), is pinentry; one
/// other argument is an askpass prompt.
fn invocation(args: &[String]) -> Result<Invocation, String> {
    match args {
        [] => Ok(Invocation::Pinentry),
        [help] if help == "--help" => Ok(Invocation::Help),
        [version] if version == "--version" => Ok(Invocation::Version),
        [first, ..] if first.starts_with('-') => Ok(Invocation::Pinentry),
        [prompt] => Ok(Invocation::Askpass(prompt.clone())),
        _ => Err(USAGE.to_owned()),
    }
}

fn main() -> ExitCode {
    // An argument that is not text is neither a prompt nor an option.
    let Some(args) = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string().ok())
        .collect::<Option<Vec<String>>>()
    else {
        return exit(Err(USAGE.to_owned()));
    };
    let invocation = match invocation(&args) {
        Ok(invocation) => invocation,
        Err(usage) => return exit(Err(usage)),
    };
    let text = match invocation {
        Invocation::Help => Some(HELP.to_owned()),
        Invocation::Version => Some(format!("td-pinentry {}", env!("CARGO_PKG_VERSION"))),
        Invocation::Pinentry | Invocation::Askpass(_) => None,
    };
    if let Some(text) = text {
        return exit(
            writeln!(std::io::stdout(), "{text}")
                .map_err(|error| format!("cannot write the usage: {error}")),
        );
    }
    if let Err(reason) = mode::admit(mode::os_release().as_deref()) {
        return exit(Err(reason.to_owned()));
    }
    match invocation {
        Invocation::Askpass(prompt) => {
            let hint = std::env::var("SSH_ASKPASS_PROMPT").ok();
            let answer = window::ask(askpass::request(&prompt, hint.as_deref()), None);
            match askpass::finish(answer, &mut std::io::stdout().lock()) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(why) => exit(Err(why)),
            }
        }
        Invocation::Pinentry => exit(pinentry()),
        Invocation::Help | Invocation::Version => ExitCode::SUCCESS,
    }
}

/// The pinentry conversation until `BYE` or the agent's end.
fn pinentry() -> Result<(), String> {
    let receiver = assuan::spawn_reader(std::io::stdin())?;
    let mut inbox = assuan::Inbox::new(receiver);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    assuan::serve(&mut inbox, &mut out, &mut |request, inbox| {
        window::ask(request, Some(inbox))
    })
    .map_err(|error| format!("the conversation with gpg-agent: {error}"))
}

fn exit(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("td-pinentry: {message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn options_or_nothing_are_pinentry_and_one_prompt_is_askpass() {
        assert_eq!(invocation(&args(&[])), Ok(Invocation::Pinentry));
        assert_eq!(
            invocation(&args(&["--display", ":0", "--ttyname", "/dev/pts/1"])),
            Ok(Invocation::Pinentry)
        );
        assert_eq!(invocation(&args(&["--help"])), Ok(Invocation::Help));
        assert_eq!(invocation(&args(&["--version"])), Ok(Invocation::Version));
        assert_eq!(
            invocation(&args(&["Password for 'https://github.com': "])),
            Ok(Invocation::Askpass(
                "Password for 'https://github.com': ".to_owned()
            ))
        );
        assert!(invocation(&args(&["one", "two"])).is_err());
    }

    #[test]
    fn the_key_list_is_spelled_as_the_keymap_spells_chords() {
        assert_eq!(td_ui::keys::check(&app::keys()), Vec::<String>::new());
    }
}
