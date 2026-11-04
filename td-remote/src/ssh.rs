//! The one way td reaches another machine: an `ssh` child with fixed options,
//! its standard streams the link. sshd hands a remote command to the login
//! shell as a line, so what crosses as that line is only fixed words and a
//! program name of characters no shell treats specially; a path or a file's
//! bytes travel in frames, never as an argument.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Link, Options};

const HOST_BYTES: usize = 255;
const PROGRAM_BYTES: usize = 1024;

/// What ssh is told ahead of the host. `-T` keeps a terminal out of a binary
/// stream, whatever the configuration's `RequestTTY` says; the rest keep a
/// host's configured forwardings and command out of a link that wants none.
/// `ControlPath=none` makes it a connection of its own, since another's master
/// would hold it to that one's keepalives. These notice a dead peer, as git's
/// transport does (td-agent/src/git.rs).
const OPTIONS: &[&str] = &[
    "-T",
    "-x",
    "-a",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "RemoteCommand=none",
    "-o",
    "ControlPath=none",
    "-o",
    "ConnectTimeout=30",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
];

fn host_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
}

fn program_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_')
}

/// Another machine, as ssh names one: `[user@]name`, or an alias its
/// configuration defines. No ssh option can pass as one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Host(String);

impl Host {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() || text.len() > HOST_BYTES {
            return Err(format!("a host is 1..={HOST_BYTES} bytes"));
        }
        // A user may precede the name: what ssh reads as the name must not pass
        // for an option either, in whatever a configuration hands it to.
        let (user, name) = match text.rsplit_once('@') {
            Some((user, name)) => (Some(user), name),
            None => (None, text),
        };
        if text.starts_with('-')
            || name.starts_with('-')
            || name.is_empty()
            || user.is_some_and(str::is_empty)
            || !text.bytes().all(host_byte)
        {
            return Err(format!(
                "{text:?} is not a host: [user@]name, of letters, digits and . - _ @ : \
                 only, neither starting with -"
            ));
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The program to run there, where a non-interactive shell's `PATH` may not
/// reach it: a name or a path, relative to the account's home, that the login
/// shell reads literally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteProgram(String);

impl RemoteProgram {
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() || text.len() > PROGRAM_BYTES {
            return Err(format!("a remote program is 1..={PROGRAM_BYTES} bytes"));
        }
        if text.starts_with('-') || !text.bytes().all(program_byte) {
            return Err(format!(
                "{text:?} is not a remote program: letters, digits and / . - _ only, not starting with -"
            ));
        }
        Ok(Self(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn command(host: &Host, program: &RemoteProgram, words: &[&'static str]) -> Command {
    let mut command = Command::new("ssh");
    command.args(OPTIONS).arg("--").arg(host.as_str());
    command.arg(program.as_str()).args(words);
    let chosen = ["SSH_ASKPASS", "SSH_ASKPASS_REQUIRE"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    let found = prompter(
        std::env::current_exe().ok().as_deref(),
        chosen,
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("PATH").as_deref(),
    );
    ask_through(&mut command, found);
    command
}

fn ask_through(command: &mut Command, prompter: Option<PathBuf>) {
    if let Some(prompter) = prompter {
        command
            .env("SSH_ASKPASS", prompter)
            .env("SSH_ASKPASS_REQUIRE", "force");
    }
}

/// td-pinentry, for ssh's passphrase and host-key questions: a window has no
/// terminal to ask on. Nothing when the person chose for themselves
/// (`td-pinentry/DESIGN.md`) or there is no display for one to open on. It is
/// looked for beside this program, then where `./install-apps` puts it, then
/// on `PATH`.
fn prompter(
    exe: Option<&Path>,
    chosen: bool,
    display: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    if chosen {
        return None;
    }
    display.filter(|value| !value.is_empty())?;
    let dir = exe.and_then(Path::parent);
    let installed = dir
        .filter(|dir| dir.ends_with("lib/td"))
        .and_then(|dir| Some(dir.parent()?.parent()?.join("bin")));
    [dir.map(Path::to_path_buf), installed]
        .into_iter()
        .flatten()
        .find_map(|dir| td_fs::executable_in(Some(dir.as_os_str()), "td-pinentry"))
        .or_else(|| td_fs::executable_in(path, "td-pinentry"))
}

impl Link {
    /// Connects to `program words…` on `host`. A thread may wait here for as
    /// long as a person takes to answer ssh's questions, so it is never the
    /// one that draws a window.
    pub fn ssh(
        host: &Host,
        program: &RemoteProgram,
        words: &[&'static str],
        options: Options,
    ) -> Result<Self, Error> {
        Self::spawn(command(host, program, words), options)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_host_is_a_name_and_never_an_option() {
        for good in [
            "host",
            "me@host.example",
            "me-too@host",
            "10.0.0.1",
            "::1",
            "a_b-c",
        ] {
            assert_eq!(Host::parse(good).unwrap().as_str(), good);
        }
        for bad in [
            "",
            // Only the leading dash is wrong with these two.
            "-v",
            "-oProxyCommand",
            "-oProxyCommand=x",
            // What ssh reads as the name, after a user.
            "me@-oProxyCommand",
            "me@",
            "@host",
            "host name",
            "host;id",
            "$(id)",
            "ssh://host",
            "a\nb",
        ] {
            assert!(Host::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Host::parse(&"h".repeat(256)).is_err());
    }

    #[test]
    fn a_remote_program_holds_nothing_a_shell_would_read() {
        for good in ["td-editor", ".local/bin/td-editor", "/bin/td-editor"] {
            assert_eq!(RemoteProgram::parse(good).unwrap().as_str(), good);
        }
        for bad in [
            "", "-x", "a b", "a;b", "$HOME/x", "~/x", "a`b`", "a\nb", "*",
        ] {
            assert!(RemoteProgram::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_command_ends_options_before_the_host_and_asks_for_no_terminal() {
        let host = Host::parse("host").unwrap();
        let program = RemoteProgram::parse("td-editor").unwrap();
        let built = command(&host, &program, &["--file-host"]);
        assert_eq!(built.get_program(), "ssh");
        let args: Vec<_> = built
            .get_args()
            .map(|arg| arg.to_str().unwrap().to_string())
            .collect();
        let at = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            args.get(at + 1..).unwrap(),
            ["host", "td-editor", "--file-host"]
        );
        // All of it: each is a promise of DESIGN.md.
        assert_eq!(
            args.get(..at).unwrap(),
            [
                "-T",
                "-x",
                "-a",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "RemoteCommand=none",
                "-o",
                "ControlPath=none",
                "-o",
                "ConnectTimeout=30",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=4",
            ]
        );
    }

    #[test]
    fn the_prompter_is_found_beside_installed_or_on_path_unless_one_is_named() {
        let owned = td_fs::TempDir::new("td-remote-prompter").unwrap();
        let dir = owned.path();
        let (bin, lib) = (dir.join("bin"), dir.join("lib/td"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        let program = bin.join("td-pinentry");
        std::fs::write(&program, "").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let display = Some(OsStr::new("wayland-1"));
        let beside = bin.join("td-editor");
        let installed = lib.join("td-editor");
        let elsewhere = dir.join("td-editor");
        assert_eq!(
            prompter(Some(&beside), false, display, None).as_deref(),
            Some(program.as_path())
        );
        assert_eq!(
            prompter(Some(&installed), false, display, None).as_deref(),
            Some(program.as_path())
        );
        assert_eq!(prompter(Some(&elsewhere), false, display, None), None);
        assert_eq!(
            prompter(Some(&elsewhere), false, display, Some(bin.as_os_str())).as_deref(),
            Some(program.as_path())
        );
        // A choice the person made is theirs; no display opens no window.
        assert_eq!(prompter(Some(&beside), true, display, None), None);
        assert_eq!(prompter(Some(&beside), false, None, None), None);
        assert_eq!(
            prompter(Some(&beside), false, Some(OsStr::new("")), None),
            None
        );
    }

    #[test]
    fn a_prompter_found_is_the_one_ssh_is_made_to_use() {
        let mut none = Command::new("ssh");
        ask_through(&mut none, None);
        assert_eq!(none.get_envs().count(), 0);
        let mut command = Command::new("ssh");
        ask_through(&mut command, Some(PathBuf::from("/bin/td-pinentry")));
        let envs: Vec<_> = command.get_envs().collect();
        assert_eq!(
            envs,
            [
                (
                    OsStr::new("SSH_ASKPASS"),
                    Some(OsStr::new("/bin/td-pinentry"))
                ),
                (OsStr::new("SSH_ASKPASS_REQUIRE"), Some(OsStr::new("force"))),
            ]
        );
    }
}
