//! What td-term runs in its PTY and as whom: the account the session belongs
//! to, the child's constructed environment, and its literal argv. Pure
//! functions over the process's own status and account files, tested
//! without a device; td-ui's `pty` owns the device and the threads.

use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use td_ui::proc_status::effective_uid;
use td_ui::pty::ChildCommand;

/// The shell td's session runs when no command is given.
pub const DEFAULT_SHELL: &str = "/bin/sh";

/// Bounded reads of the two small files the child environment is derived from.
const MAX_STATUS_BYTES: usize = 64 * 1024;
const MAX_PASSWD_BYTES: usize = 1024 * 1024;

/// The graphical account, as `/proc/self/status` and `/etc/passwd` agree it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Account {
    pub uid: u32,
    pub name: String,
    pub home: String,
}

fn read_bounded(path: &Path, limit: usize) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if metadata.len() > limit as u64 {
        return Err(format!(
            "{} is larger than the {limit}-byte bound",
            path.display()
        ));
    }
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.min(4096));
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() > limit {
        return Err(format!(
            "{} is larger than the {limit}-byte bound",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{} is not UTF-8", path.display()))
}

/// The unique `/etc/passwd` entry for a uid. Fail-closed on every ambiguity:
/// a duplicate uid, an absent one, or any malformed line closes the terminal
/// rather than starting a shell whose HOME belongs to somebody else.
pub fn account(passwd: &str, uid: u32) -> Result<Account, String> {
    let mut found: Option<Account> = None;
    for (number, line) in passwd.lines().enumerate() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() != 7 {
            return Err(format!(
                "passwd line {} has {} fields, expected 7",
                number.saturating_add(1),
                fields.len()
            ));
        }
        let name = fields.first().copied().unwrap_or_default();
        let entry_uid = fields.get(2).copied().unwrap_or_default();
        let home = fields.get(5).copied().unwrap_or_default();
        let entry_uid: u32 = entry_uid.parse().map_err(|_| {
            format!(
                "passwd line {} has non-numeric uid '{entry_uid}'",
                number.saturating_add(1)
            )
        })?;
        if entry_uid != uid {
            continue;
        }
        if found.is_some() {
            return Err(format!("passwd has more than one entry for uid {uid}"));
        }
        if name.is_empty() {
            return Err(format!("passwd entry for uid {uid} has no user name"));
        }
        if !home.starts_with('/') {
            return Err(format!(
                "passwd entry for uid {uid} has a relative home '{home}'"
            ));
        }
        found = Some(Account {
            uid,
            name: name.to_string(),
            home: home.to_string(),
        });
    }
    found.ok_or_else(|| format!("passwd has no entry for uid {uid}"))
}

/// The account td-term runs as, read from the live process and account files.
pub fn current_account(status: &Path, passwd: &Path) -> Result<Account, String> {
    let uid = effective_uid(&read_bounded(status, MAX_STATUS_BYTES)?)?;
    account(&read_bounded(passwd, MAX_PASSWD_BYTES)?, uid)
}

/// The child's whole environment: `spawn` clears and sets exactly this, so a
/// variable absent here is absent from the shell whatever the terminal was
/// started with. It is constructed, never inherited: an outer `TERM`
/// describes the parent terminal and would be a false capability claim for
/// this one.
///
/// Socket paths come from the terminal launch configuration. They are
/// arguments so this remains a pure function. The production caller supplies
/// its connected Wayland path and the control path from its environment.
pub fn environment(
    account: &Account,
    control_socket: Option<&str>,
    wayland_socket: &str,
) -> Vec<(String, String)> {
    let mut environment = vec![
        ("COLORTERM".into(), "truecolor".into()),
        ("HOME".into(), account.home.clone()),
        ("LOGNAME".into(), account.name.clone()),
        ("PATH".into(), "/bin".into()),
        ("SHELL".into(), DEFAULT_SHELL.into()),
        ("TERM".into(), "td-term".into()),
        ("TERMINFO".into(), "/etc/terminfo".into()),
        ("USER".into(), account.name.clone()),
        ("WAYLAND_DISPLAY".into(), wayland_socket.into()),
        (
            "XDG_RUNTIME_DIR".into(),
            format!("/run/user/{}", account.uid),
        ),
    ];
    // Last, and only when there is one: a shell in this terminal is where a
    // person runs `td-ctl`, and without this they would have to name the
    // socket by hand on a machine that already knows it.
    if let Some(socket) = control_socket {
        environment.push(("TD_CONTROL_SOCKET".into(), socket.into()));
    }
    environment
}

/// `shell`, leading a session on the terminal, by default, or exactly the
/// command supplied on td-term's own command line.
///
/// The shell leads a session whose controlling terminal is the slave, because
/// a shell expects job control and the line discipline's signals and creates
/// neither; td-ui's `spawn` makes it so as it starts the child. An explicit
/// command does NOT: it is exec'd as given, with the slave on its stdio and no
/// session or controlling terminal of its own. A program that wants one says
/// so, as td-authd's launch does through `/bin/cttyhack --stdin`; td-jail's
/// terminal grant (`devices=tty` in APPLICATIONS.md) instead acquires the
/// terminal inside its own detached session, which the kernel refuses for a
/// terminal another session already holds. Both paths must be absolute: a
/// relative program would be resolved against an ambient PATH this adapter
/// deliberately does not have.
pub fn child_command(shell: &Path, command: &[OsString]) -> Result<ChildCommand, String> {
    if !shell.is_absolute() {
        return Err(format!(
            "terminal shell '{}' is not absolute",
            shell.display()
        ));
    }
    let Some(program) = command.first() else {
        return Ok(ChildCommand {
            program: shell.to_path_buf(),
            arguments: Vec::new(),
            leads_session: true,
        });
    };
    if !Path::new(program).is_absolute() {
        return Err(format!(
            "terminal command '{}' is not absolute",
            program.to_string_lossy()
        ));
    }
    Ok(ChildCommand {
        program: PathBuf::from(program),
        arguments: command.iter().skip(1).cloned().collect(),
        leads_session: false,
    })
}

/// The packaged binary's own check of the session policy. It reads no file,
/// so it runs wherever the artifact does.
pub fn selftest() -> Result<(), String> {
    let account = account(
        "root:x:0:0:root:/root:/bin/sh\ntd:x:1000:1000::/var/home/td:/bin/sh\n",
        1000,
    )?;
    if account.name != "td" || account.home != "/var/home/td" {
        return Err("session selftest selected the wrong account".into());
    }
    if effective_uid("Name:\tsh\nUid:\t1000\t1000\t1000\t1000\n")? != 1000 {
        return Err("session selftest misread its own uid".into());
    }
    let environment = environment(&account, None, "/run/td-compositor/1000/wayland-0");
    let named = |name: &str| {
        let mut value = None;
        for (key, candidate) in &environment {
            if key == name {
                value = Some(candidate.as_str());
            }
        }
        value
    };
    if named("TERM") != Some("td-term")
        || named("XDG_RUNTIME_DIR") != Some("/run/user/1000")
        || named("HOME") != Some("/var/home/td")
        || environment.len() != 10
    {
        return Err("session selftest built the wrong child environment".into());
    }
    let command = child_command(Path::new(DEFAULT_SHELL), &[])?;
    if command.program != Path::new(DEFAULT_SHELL)
        || !command.arguments.is_empty()
        || !command.leads_session
    {
        return Err("session selftest composed the wrong child command".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          td:x:1000:1000:td user:/var/home/td:/bin/sh\n";

    #[test]
    fn the_child_is_told_where_the_control_socket_is() {
        // `spawn` clears the environment and sets exactly what this returns,
        // so a variable missing here is missing from the shell — which is how
        // `td-ctl` came to need `--socket` on a machine that already knew the
        // path. Absent when there is none, so a session without a control
        // socket does not advertise one.
        let account = Account {
            name: "tester".into(),
            uid: 1000,
            home: "/var/home/tester".into(),
        };
        let without = environment(&account, None, "/run/td-compositor/1000/wayland-0");
        assert!(
            !without.iter().any(|(name, _)| name == "TD_CONTROL_SOCKET"),
            "a session with no control socket advertised one"
        );
        let with = environment(
            &account,
            Some("/run/td-compositor/1000/td-control"),
            "/run/td-compositor/1000/wayland-0",
        );
        assert_eq!(
            with.iter()
                .find(|(name, _)| name == "TD_CONTROL_SOCKET")
                .map(|(_, value)| value.as_str()),
            Some("/run/td-compositor/1000/td-control"),
            "the shell was not told where the control socket is"
        );
        // And it is the only difference, so nothing else changed shape.
        assert_eq!(with.len(), without.len() + 1);
    }
    #[test]
    fn the_account_must_be_unique_well_formed_and_present() {
        let account = account(PASSWD, 1000).unwrap();
        assert_eq!(
            account,
            Account {
                uid: 1000,
                name: "td".into(),
                home: "/var/home/td".into(),
            }
        );
        assert!(account_error(PASSWD, 1001).contains("no entry for uid 1001"));
        let duplicate = format!("{PASSWD}other:x:1000:1000::/var/home/other:/bin/sh\n");
        assert!(account_error(&duplicate, 1000).contains("more than one entry"));
        assert!(account_error("td:x:1000:1000::/var/home/td\n", 1000).contains("6 fields"));
        assert!(
            account_error(":x:1000:1000::/var/home/td:/bin/sh\n", 1000).contains("no user name")
        );
        assert!(
            account_error("td:x:1000:1000::var/home/td:/bin/sh\n", 1000).contains("relative home")
        );
        assert!(account_error("td:x:x:1000::/var/home/td:/bin/sh\n", 1000).contains("non-numeric"));
        // Whole-file strictness reaches a blank line too: it is a line td
        // cannot account for, and the entry being looked up may sit after it.
        // `lines()` drops the trailing newline, so a well-formed file has none.
        let blank = format!("\n{PASSWD}");
        assert!(account_error(&blank, 1000).contains("line 1 has 1 fields"));
        let internal = PASSWD.replace("td:x:1000", "\ntd:x:1000");
        assert!(account_error(&internal, 1000).contains("1 fields"));
    }

    fn account_error(passwd: &str, uid: u32) -> String {
        account(passwd, uid).unwrap_err()
    }

    #[test]
    fn the_child_environment_is_constructed_rather_than_inherited() {
        let account = account(PASSWD, 1000).unwrap();
        let environment = environment(&account, None, "/run/td-compositor/1000/wayland-0");
        let names: Vec<&str> = environment.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "COLORTERM",
                "HOME",
                "LOGNAME",
                "PATH",
                "SHELL",
                "TERM",
                "TERMINFO",
                "USER",
                "WAYLAND_DISPLAY",
                "XDG_RUNTIME_DIR",
            ]
        );
        let value = |name: &str| {
            let mut found = None;
            for (key, candidate) in &environment {
                if key == name {
                    found = Some(candidate.clone());
                }
            }
            found.unwrap()
        };
        assert_eq!(value("TERM"), "td-term");
        assert_eq!(value("HOME"), "/var/home/td");
        assert_eq!(value("USER"), "td");
        assert_eq!(value("LOGNAME"), "td");
        assert_eq!(value("XDG_RUNTIME_DIR"), "/run/user/1000");
        assert_eq!(
            value("WAYLAND_DISPLAY"),
            "/run/td-compositor/1000/wayland-0"
        );
        assert_eq!(value("TERMINFO"), "/etc/terminfo");
    }

    #[test]
    fn the_default_child_is_the_shell_leading_a_session() {
        let default = child_command(Path::new(DEFAULT_SHELL), &[]).unwrap();
        assert_eq!(default.program, PathBuf::from("/bin/sh"));
        assert!(default.arguments.is_empty());
        assert!(default.leads_session);
        assert!(child_command(Path::new("sh"), &[]).is_err());
    }

    #[test]
    fn an_explicit_command_is_literal_argv_without_the_wrapper() {
        use std::os::unix::ffi::OsStringExt;
        let words = |list: &[&str]| -> Vec<OsString> { list.iter().map(OsString::from).collect() };
        let explicit = child_command(
            Path::new(DEFAULT_SHELL),
            &words(&["/bin/mail", "--cli", "echo hi"]),
        )
        .unwrap();
        assert_eq!(explicit.program, PathBuf::from("/bin/mail"));
        assert_eq!(explicit.arguments, vec!["--cli", "echo hi"]);
        assert!(!explicit.leads_session, "the terminal is left unowned");
        // A caller that wants a session spells out the wrapper and gets
        // exactly that.
        let wrapped = child_command(
            Path::new(DEFAULT_SHELL),
            &words(&["/bin/cttyhack", "--stdin", "/bin/sh"]),
        )
        .unwrap();
        assert_eq!(wrapped.program, PathBuf::from("/bin/cttyhack"));
        assert_eq!(wrapped.arguments, vec!["--stdin", "/bin/sh"]);
        assert!(!wrapped.leads_session);
        // Literal means bytes: an argument that is not UTF-8 is carried as-is.
        let raw = OsString::from_vec(vec![0x2f, 0x74, 0x6d, 0x70, 0x2f, 0xff]);
        let bytes = child_command(
            Path::new(DEFAULT_SHELL),
            &[OsString::from("/bin/mail"), raw.clone()],
        )
        .unwrap();
        assert_eq!(bytes.arguments, vec![raw]);
        assert!(child_command(Path::new(DEFAULT_SHELL), &words(&["sh"])).is_err());
        // The shell's path is checked even when the command does not use it.
        assert!(child_command(Path::new("sh"), &words(&["/bin/mail"])).is_err());
    }

    #[test]
    fn current_account_reads_the_live_process_and_account_files() {
        let directory =
            std::env::temp_dir().join(format!("td-term-account-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let passwd = directory.join("passwd");
        std::fs::write(&passwd, PASSWD).unwrap();
        let status = directory.join("status");
        std::fs::write(&status, "Uid:\t1000\t1000\t1000\t1000\n").unwrap();
        assert_eq!(current_account(&status, &passwd).unwrap().name, "td");
        std::fs::write(&status, "Uid:\t1000\t4242\t4242\t4242\n").unwrap();
        assert!(current_account(&status, &passwd).is_err());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn the_selftest_covers_the_policy() {
        selftest().unwrap();
    }
}
