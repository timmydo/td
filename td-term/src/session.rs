//! What td-term runs in its PTY and as whom: under td's profile the account
//! the session belongs to, the child's constructed environment and its
//! literal argv; under the desktop profile the inherited environment, the
//! person's shell and the terminfo entry written where the child finds it
//! (td-term/DESIGN.md §7). Functions over the process's own status and
//! account files, tested without a device; td-ui's `pty` owns the device
//! and the threads.

use std::ffi::{OsStr, OsString};
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

/// The uid td-term runs as, from the live process's status.
pub fn current_uid(status: &Path) -> Result<u32, String> {
    effective_uid(&read_bounded(status, MAX_STATUS_BYTES)?)
}

/// The account td-term runs as, read from the live process and account files.
pub fn current_account(status: &Path, passwd: &Path) -> Result<Account, String> {
    account(
        &read_bounded(passwd, MAX_PASSWD_BYTES)?,
        current_uid(status)?,
    )
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

/// What the desktop profile replaces in the environment it inherits: the
/// outer terminal's description, the terminal's own display connection, a
/// size the child must ask the terminal for, and the value it sets.
const DESKTOP_REPLACED: [&str; 6] = [
    "COLORTERM",
    "COLUMNS",
    "LINES",
    "TERM",
    "WAYLAND_DISPLAY",
    "WAYLAND_SOCKET",
];

/// The desktop profile's shell: `$SHELL` when it names one, else `/bin/sh`.
pub fn desktop_shell(shell: Option<OsString>) -> PathBuf {
    shell
        .filter(|shell| !shell.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_SHELL), PathBuf::from)
}

/// The desktop profile's child: the shell, or the command as given, either
/// leading a session on the terminal, since a desktop's programs expect the
/// terminal's signals and no jail claims it; a bare name is found on the
/// child's `PATH`.
pub fn desktop_command(shell: PathBuf, command: &[OsString]) -> ChildCommand {
    match command.split_first() {
        Some((program, arguments)) => ChildCommand {
            program: PathBuf::from(program),
            arguments: arguments.to_vec(),
            leads_session: true,
        },
        None => ChildCommand {
            program: shell,
            arguments: Vec::new(),
            leads_session: true,
        },
    }
}

/// The desktop profile's child environment: td-term's own, as a desktop
/// terminal's child inherits it, less `DESKTOP_REPLACED`, with this
/// terminal's description and the display it dialled. `terminfo`, the
/// directory holding td-term's entry, is `TERMINFO`, which ncurses searches
/// before `~/.terminfo` and `TERMINFO_DIRS` and then goes on to them, so a
/// stale `td-term` there cannot shadow it and every other entry is still
/// found; without one an inherited `TERMINFO` stands.
pub fn desktop_environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    wayland_display: &str,
    terminfo: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    let mut environment: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(name, _)| {
            !DESKTOP_REPLACED.iter().any(|replaced| name == replaced)
                && !(terminfo.is_some() && name == "TERMINFO")
        })
        .collect();
    let mut set = |name: &str, value: &OsStr| {
        environment.push((OsString::from(name), value.to_os_string()));
    };
    set("COLORTERM", OsStr::new("truecolor"));
    set("TERM", OsStr::new("td-term"));
    set("WAYLAND_DISPLAY", OsStr::new(wayland_display));
    if let Some(directory) = terminfo {
        set("TERMINFO", directory.as_os_str());
    }
    environment
}

/// Refuses a directory anyone but `uid` could have put a name in: not a
/// directory (`follow` decides whether a link to one counts), another
/// owner's, or open to group or others.
fn private_directory(path: &Path, uid: u32, follow: bool) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    }
    .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "{} is not a directory private to uid {uid}",
            path.display()
        ));
    }
    Ok(())
}

/// Where the window's frame buffers are backed: the session's runtime
/// directory when it is absolute and private to `uid`, else `temporary`.
/// Every present writes frame rows into a pool file, and a runtime
/// directory is memory, where a temporary directory may be a disk that
/// writes those pages back.
pub fn pool_directory(runtime: Option<&OsStr>, uid: Option<u32>, temporary: PathBuf) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    // The owner must also be able to make files there.
    let writable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|metadata| metadata.mode() & 0o300 == 0o300)
    };
    match (runtime.map(Path::new), uid) {
        (Some(runtime), Some(uid))
            if runtime.is_absolute()
                && private_directory(runtime, uid, true).is_ok()
                && writable(runtime) =>
        {
            runtime.to_path_buf()
        }
        _ => temporary,
    }
}

/// Writes td-term's compiled entry under `runtime` (the session's
/// `XDG_RUNTIME_DIR`, which must already be private to `uid`) and answers
/// the directory holding it. Each directory below is made, or found, private
/// and no link, so no other account can choose where the entry or the
/// directory ncurses searches goes. The entry is written to a new file
/// beside it and renamed over, so terminals starting together each leave a
/// whole one.
pub fn install_runtime_terminfo(runtime: &Path, uid: u32, entry: &[u8]) -> Result<PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    if !runtime.is_absolute() {
        return Err(format!(
            "XDG_RUNTIME_DIR '{}' is not absolute",
            runtime.display()
        ));
    }
    private_directory(runtime, uid, true)?;
    let directory = runtime.join("td-term").join("terminfo");
    let relative = td_ui::vt_terminfo::INSTALL_PATH
        .strip_prefix("share/terminfo/")
        .ok_or("the terminfo install path is not under share/terminfo")?;
    let path = directory.join(relative);
    let parent = path.parent().ok_or("the terminfo entry has no directory")?;
    let mut made = runtime.to_path_buf();
    let below = parent.strip_prefix(runtime).map_err(|e| e.to_string())?;
    for component in below.components() {
        made.push(component);
        match std::fs::DirBuilder::new().mode(0o700).create(&made) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(format!("create {}: {e}", made.display()));
            }
            _ => private_directory(&made, uid, false)?,
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let staging = parent.join(format!(".td-term.{}.{nanos}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&staging)
        .map_err(|e| format!("create {}: {e}", staging.display()))?;
    // Only a file this call made is removed: a name another writer holds
    // failed the open above and is left to it.
    let written = file
        .write_all(entry)
        .map_err(|e| format!("write {}: {e}", staging.display()))
        .and_then(|()| {
            std::fs::rename(&staging, &path).map_err(|e| format!("rename {}: {e}", path.display()))
        });
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written.map(|()| directory)
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
    let desktop = desktop_environment(
        [("TERM".into(), "foot".into()), ("HOME".into(), "/h".into())],
        "/run/user/1000/wayland-1",
        Some(Path::new("/run/user/1000/td-term/terminfo")),
    );
    let term = desktop.iter().filter(|(name, _)| name == "TERM");
    if term.map(|(_, value)| value.as_os_str()).collect::<Vec<_>>() != [OsStr::new("td-term")]
        || desktop.len() != 5
    {
        return Err("session selftest built the wrong desktop environment".into());
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
    fn a_desktop_child_inherits_all_but_another_terminals_description() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![0xff]);
        let pair = |name: &str, value: &str| (OsString::from(name), OsString::from(value));
        let inherited = vec![
            pair("HOME", "/home/p"),
            pair("TERM", "foot"),
            pair("COLORTERM", "24bit"),
            pair("LINES", "40"),
            pair("COLUMNS", "100"),
            pair("WAYLAND_SOCKET", "5"),
            pair("WAYLAND_DISPLAY", "wayland-1"),
            pair("TERMINFO_DIRS", "/usr/local/share/terminfo"),
            pair("TERMINFO", "/etc/terminfo"),
            (OsString::from("RAW"), raw.clone()),
        ];
        let terminfo = Path::new("/run/user/1000/td-term/terminfo");
        let environment = desktop_environment(
            inherited.clone(),
            "/run/user/1000/wayland-1",
            Some(terminfo),
        );
        let value = |environment: &[(OsString, OsString)], name: &str| {
            let values: Vec<OsString> = environment
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .collect();
            assert!(values.len() <= 1, "{name} twice");
            values.into_iter().next()
        };
        assert_eq!(value(&environment, "HOME"), Some("/home/p".into()));
        assert_eq!(value(&environment, "RAW"), Some(raw));
        assert_eq!(value(&environment, "TERM"), Some("td-term".into()));
        assert_eq!(value(&environment, "COLORTERM"), Some("truecolor".into()));
        assert_eq!(
            value(&environment, "WAYLAND_DISPLAY"),
            Some("/run/user/1000/wayland-1".into())
        );
        for gone in ["LINES", "COLUMNS", "WAYLAND_SOCKET"] {
            assert_eq!(value(&environment, gone), None, "{gone}");
        }
        // The entry written is searched first; the inherited list after it.
        assert_eq!(
            value(&environment, "TERMINFO"),
            Some("/run/user/1000/td-term/terminfo".into())
        );
        assert_eq!(
            value(&environment, "TERMINFO_DIRS"),
            Some("/usr/local/share/terminfo".into())
        );
        assert_eq!(environment.len(), 7);
        // Without an entry written, the inherited TERMINFO stands.
        let kept = desktop_environment(inherited, "/w", None);
        assert_eq!(value(&kept, "TERMINFO"), Some("/etc/terminfo".into()));
        let none = desktop_environment([pair("HOME", "/h")], "/w", None);
        assert_eq!(value(&none, "TERMINFO"), None);
    }

    #[test]
    fn a_desktop_child_is_the_persons_shell_or_command_leading_a_session() {
        assert_eq!(
            desktop_shell(Some("/bin/zsh".into())),
            Path::new("/bin/zsh")
        );
        assert_eq!(desktop_shell(Some("".into())), Path::new(DEFAULT_SHELL));
        assert_eq!(desktop_shell(None), Path::new(DEFAULT_SHELL));
        let shell = desktop_command(PathBuf::from("/bin/zsh"), &[]);
        assert_eq!(shell.program, Path::new("/bin/zsh"));
        assert!(shell.arguments.is_empty() && shell.leads_session);
        let command = [OsString::from("htop"), OsString::from("-d")];
        let explicit = desktop_command(PathBuf::from("/bin/zsh"), &command);
        assert_eq!(explicit.program, Path::new("htop"));
        assert_eq!(explicit.arguments, vec![OsString::from("-d")]);
        assert!(explicit.leads_session);
    }

    #[test]
    fn the_runtime_terminfo_entry_is_whole_where_ncurses_looks() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let uid = current_uid(Path::new("/proc/self/status")).unwrap();
        let runtime = std::env::temp_dir().join(format!("td-term-runtime-{}", std::process::id()));
        // A failed run under a pid since reused leaves its directory here.
        let _ = std::fs::remove_dir_all(&runtime);
        let entry = td_ui::vt_terminfo::entry().unwrap();
        // A runtime directory that is missing, or open to others, is refused.
        assert!(install_runtime_terminfo(&runtime, uid, &entry).is_err());
        assert!(!runtime.exists(), "the runtime directory is not made");
        std::fs::create_dir(&runtime).unwrap();
        // Set after creation, which the umask cannot narrow.
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(install_runtime_terminfo(&runtime, uid, &entry).is_err());
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(install_runtime_terminfo(&runtime, uid + 1, &entry).is_err());
        // A link where a directory goes is refused, not followed.
        let elsewhere = runtime.join("elsewhere");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&elsewhere)
            .unwrap();
        std::os::unix::fs::symlink(&elsewhere, runtime.join("td-term")).unwrap();
        assert!(install_runtime_terminfo(&runtime, uid, &entry).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        std::fs::remove_file(runtime.join("td-term")).unwrap();
        for _ in 0..2 {
            let directory = install_runtime_terminfo(&runtime, uid, &entry).unwrap();
            assert_eq!(directory, runtime.join("td-term/terminfo"));
            assert_eq!(std::fs::read(directory.join("t/td-term")).unwrap(), entry);
            let names: Vec<_> = std::fs::read_dir(directory.join("t"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(names, vec![OsString::from("td-term")], "no staging left");
        }
        std::fs::remove_dir_all(&runtime).unwrap();
        assert!(install_runtime_terminfo(Path::new("run/user"), uid, &entry).is_err());
    }

    #[test]
    fn frame_buffers_are_backed_in_a_private_runtime_directory() {
        use std::os::unix::fs::PermissionsExt;
        let uid = current_uid(Path::new("/proc/self/status")).unwrap();
        let scratch = std::env::temp_dir().join(format!("td-term-pools-{}", std::process::id()));
        // A run that failed before cleaning up leaves its directory behind.
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir(&scratch).unwrap();
        let runtime = scratch.join("runtime");
        let temporary = PathBuf::from("/tmp");
        let pools = |runtime: &Path, uid: Option<u32>| {
            pool_directory(Some(runtime.as_os_str()), uid, temporary.clone())
        };
        let mode = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        assert_eq!(
            pool_directory(None, Some(uid), temporary.clone()),
            temporary
        );
        assert_eq!(pools(&runtime, Some(uid)), temporary, "missing");
        std::fs::write(&runtime, b"").unwrap();
        mode(&runtime, 0o600);
        assert_eq!(pools(&runtime, Some(uid)), temporary, "a file");
        std::fs::remove_file(&runtime).unwrap();
        // Modes are set after creation, which the umask cannot narrow.
        std::fs::create_dir(&runtime).unwrap();
        mode(&runtime, 0o755);
        assert_eq!(pools(&runtime, Some(uid)), temporary, "open");
        mode(&runtime, 0o750);
        assert_eq!(pools(&runtime, Some(uid)), temporary, "group");
        mode(&runtime, 0o500);
        assert_eq!(pools(&runtime, Some(uid)), temporary, "unwritable");
        mode(&runtime, 0o700);
        assert_eq!(pools(&runtime, Some(uid)), runtime);
        assert_eq!(pools(&runtime, Some(uid + 1)), temporary, "another's");
        assert_eq!(pools(&runtime, None), temporary, "no uid");
        let link = scratch.join("link");
        std::os::unix::fs::symlink(&runtime, &link).unwrap();
        assert_eq!(pools(&link, Some(uid)), link, "a link to a private one");
        assert_eq!(
            pools(Path::new("run/user"), Some(uid)),
            temporary,
            "relative"
        );
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    #[test]
    fn the_selftest_covers_the_policy() {
        selftest().unwrap();
    }
}
