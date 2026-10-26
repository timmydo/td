//! What a close would end (td-term/DESIGN.md §4): whether the terminal's
//! child is running a job, read from `/proc` with bounded std file reads,
//! so asking adds no syscall of td-term's own.
//!
//! The kernel keeps the terminal's foreground process group, and a
//! session leader's `/proc/PID/stat` reports it (`tpgid`). A shell with job
//! control hands the terminal to each job's group, so another group in the
//! foreground is a job. td's own shell has no job control: its commands
//! run in its own group, so the shell's group holding any process but the
//! shell is a job too, and so is a job a Control-z stopped, which the
//! hangup would end with whatever it holds. Only a close asks, so the
//! scan of `/proc` that finds them runs once per close request, not per
//! turn.

use std::io;
use std::path::Path;

/// Where the kernel's process table is read. td-term's children are
/// named by the pids its own namespace gives them, so this is that
/// namespace's procfs.
pub const PROC: &str = "/proc";

/// A `stat` line is some fifty numbers and a name of at most 64 bytes.
const MAX_STAT_BYTES: u64 = 4096;
/// The most process entries one scan reads; past it the scan is not
/// whole, and a group it found nobody else in is not known to be idle.
const MAX_ENTRIES: usize = 65_536;
/// The kernel's own name is at most 15 bytes; a longer one is cut.
const MAX_NAME_CHARS: usize = 32;
const MAX_KERNEL_NAME_BYTES: usize = 15;

/// What closing the terminal now would end.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Busy {
    /// The shell alone, or nothing at all: the close goes ahead.
    Idle,
    /// A job, with its program's name when one could be read.
    Job(Option<String>),
    /// A stopped job and nothing in the foreground but the shell.
    Stopped(Option<String>),
    /// What the terminal holds could not be read, so a close asks.
    Unknown,
}

/// The fields of `/proc/PID/stat` a close needs.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Stat {
    pid: u32,
    name: String,
    state: char,
    ppid: u32,
    pgrp: i64,
    session: i64,
    /// The foreground group of the process's controlling terminal, or -1
    /// when it has none.
    tpgid: i64,
}

impl Stat {
    /// Exited, whether or not it has been reaped yet.
    fn gone(&self) -> bool {
        matches!(self.state, 'Z' | 'X' | 'x')
    }
}

/// One `stat` line: the pid, the name in parentheses (which may hold
/// spaces and parentheses itself, so it ends at the LAST `)`), then the
/// fields after it.
fn parse(bytes: &[u8]) -> Option<Stat> {
    let open = bytes.iter().position(|byte| *byte == b'(')?;
    let close = bytes.iter().rposition(|byte| *byte == b')')?;
    let pid = std::str::from_utf8(bytes.get(..open)?)
        .ok()?
        .trim_end()
        .parse()
        .ok()?;
    let name = String::from_utf8_lossy(bytes.get(open.checked_add(1)?..close)?).into_owned();
    let rest = std::str::from_utf8(bytes.get(close.checked_add(1)?..)?).ok()?;
    let mut fields = rest.split_ascii_whitespace();
    let mut state = fields.next()?.chars();
    let state = state.next().filter(|_| state.next().is_none())?;
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    let _tty = fields.next()?;
    let tpgid = fields.next()?.parse().ok()?;
    Some(Stat {
        pid,
        name,
        state,
        ppid,
        pgrp,
        session,
        tpgid,
    })
}

/// ESRCH: a process that exits between the open of its `stat` and the
/// read fails the read with it, which is a process gone, not one unread.
const ESRCH: i32 = 3;

/// Whether a failed open or read of a `stat` file says the process is
/// gone: no such entry, or ESRCH from the read.
fn vanished(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(ESRCH)
}

/// `path`'s bytes, refused past `MAX_STAT_BYTES`, or `None` when the
/// process is gone. A plain std read rather than td-fs's, which names the
/// path in a new error and so loses the OS code `vanished` reads.
fn read_bounded(path: &Path) -> io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    let read = std::fs::File::open(path).and_then(|file| {
        file.take(MAX_STAT_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
    });
    match read {
        Ok(_) if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_STAT_BYTES) => {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: too long", path.display()),
            ))
        }
        Ok(_) => Ok(Some(bytes)),
        Err(error) if vanished(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// `pid`'s stat, or `None` when there is no such process.
fn read_stat(proc: &Path, pid: u32) -> io::Result<Option<Stat>> {
    let path = proc.join(pid.to_string()).join("stat");
    let Some(bytes) = read_bounded(&path)? else {
        return Ok(None);
    };
    parse(&bytes).map(Some).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: not a stat line", path.display()),
        )
    })
}

/// The name the kernel gives a process that `program` was exec'd from:
/// its final component, cut to the 15 bytes `/proc/PID/stat` holds.
pub fn kernel_name(program: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    let name = program.file_name()?.as_bytes();
    let name = name.get(..name.len().min(MAX_KERNEL_NAME_BYTES))?;
    Some(String::from_utf8_lossy(name).into_owned())
}

/// A name a status line can show: what the shared report-text predicate
/// would not print is a space, bounded, and nothing when nothing is left.
fn shown(name: &str) -> Option<String> {
    let name: String = name
        .chars()
        .take(MAX_NAME_CHARS)
        .map(|c| {
            if td_ui::reportable::reportable(c) {
                c
            } else {
                ' '
            }
        })
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// A stopped job: what a Control-z leaves, outside the foreground but
/// ended by the hangup with whatever it holds.
const STOPPED: u8 = 3;

/// What one scan of `proc` found of the shell's jobs: the foreground
/// group's members other than the shell, and the session's stopped
/// processes.
struct Members {
    /// The one to name: the foreground group's leader, else a child of
    /// the shell, else any foreground member, else a stopped process
    /// (`STOPPED`), the lowest pid first within each.
    best: Option<(u8, u32, String)>,
    /// Every entry was read: none can have been missed.
    whole: bool,
}

fn members(proc: &Path, group: i64, shell: u32) -> io::Result<Members> {
    let mut found = Members {
        best: None,
        whole: true,
    };
    for (count, entry) in std::fs::read_dir(proc)?.enumerate() {
        if count >= MAX_ENTRIES {
            found.whole = false;
            break;
        }
        let Ok(entry) = entry else {
            found.whole = false;
            continue;
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == shell {
            continue;
        }
        let stat = match read_stat(proc, pid) {
            Ok(Some(stat)) => stat,
            // Exited between the listing and the read.
            Ok(None) => continue,
            Err(_) => {
                found.whole = false;
                continue;
            }
        };
        let rank = if stat.gone() {
            continue;
        } else if stat.pgrp == group && i64::from(stat.pid) == group {
            0
        } else if stat.pgrp == group && stat.ppid == shell {
            1
        } else if stat.pgrp == group {
            2
        } else if stat.session == i64::from(shell) && matches!(stat.state, 'T' | 't') {
            STOPPED
        } else {
            continue;
        };
        if found
            .best
            .as_ref()
            .is_none_or(|(best, at, _)| (rank, stat.pid) < (*best, *at))
        {
            found.best = Some((rank, stat.pid, stat.name));
        }
    }
    Ok(found)
}

/// What a close would end, for the terminal whose child is `child`.
/// `shell` is the kernel's name for the shell td-term started
/// (`kernel_name`), or `None` when the child is a program its invocation
/// named: such a program, and a shell that has since exec'd another, is
/// the job for as long as it runs, since td-term cannot tell what it
/// holds (td-term/DESIGN.md §4).
pub fn busy(proc: &Path, child: u32, shell: Option<&str>) -> Busy {
    let stat = match read_stat(proc, child) {
        Ok(Some(stat)) if !stat.gone() => stat,
        // Exited: its status is on its way, and the terminal ends with it.
        Ok(_) => return Busy::Idle,
        Err(_) => return Busy::Unknown,
    };
    let foreground = stat.tpgid;
    let leads = stat.session == i64::from(child);
    if shell != Some(stat.name.as_str()) {
        // Named by the group its terminal gave the foreground to, when the
        // child leads the session on it and that is another group.
        let leader = u32::try_from(foreground)
            .ok()
            .filter(|_| leads && foreground > 0 && foreground != stat.pgrp)
            .and_then(|leader| read_stat(proc, leader).ok().flatten())
            .filter(|leader| !leader.gone());
        let name = leader.map_or(stat.name, |leader| leader.name);
        return Busy::Job(shown(&name));
    }
    // The default shell leads its session on the terminal; one that holds
    // no terminal, or leads nothing, is not what td-term started.
    if foreground <= 0 || !leads {
        return Busy::Unknown;
    }
    let Ok(found) = members(proc, foreground, child) else {
        return Busy::Unknown;
    };
    let stopped = found
        .best
        .as_ref()
        .is_some_and(|(rank, _, _)| *rank == STOPPED);
    if foreground != stat.pgrp {
        // A job-control shell gave the terminal to a job.
        let name = found
            .best
            .filter(|(rank, _, _)| *rank != STOPPED)
            .and_then(|(_, _, name)| shown(&name));
        return Busy::Job(name);
    }
    match found.best {
        Some((_, _, name)) if stopped => Busy::Stopped(shown(&name)),
        Some((_, _, name)) => Busy::Job(shown(&name)),
        None if !found.whole => Busy::Unknown,
        None => Busy::Idle,
    }
}

pub fn selftest() -> Result<(), String> {
    let stat = parse(b"42 (vi (x) y) S 7 42 7 34816 42 4194304 0 0 0 0 0 0 0 0 20 0 1 0")
        .ok_or("job selftest refused a stat line")?;
    if stat.pid != 42
        || stat.name != "vi (x) y"
        || stat.ppid != 7
        || stat.pgrp != 42
        || stat.session != 7
        || stat.tpgid != 42
    {
        return Err("job selftest misread a stat line".into());
    }
    if shown("a\u{1b}b").as_deref() != Some("a b") {
        return Err("job selftest showed a control character".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use td_fs::TempDir;

    /// A process table of `(pid, name, state, ppid, pgrp, session, tpgid)`.
    fn table(processes: &[(u32, &str, char, u32, i64, i64, i64)]) -> TempDir {
        let proc = TempDir::new("td-term-job").unwrap();
        for (pid, name, state, ppid, pgrp, session, tpgid) in processes {
            let dir = proc.path().join(pid.to_string());
            std::fs::create_dir(&dir).unwrap();
            let line = format!(
                "{pid} ({name}) {state} {ppid} {pgrp} {session} 34816 {tpgid} 4194304 0 0\n"
            );
            std::fs::write(dir.join("stat"), line).unwrap();
        }
        // Entries that are not processes are passed over.
        std::fs::create_dir(proc.path().join("self")).unwrap();
        std::fs::write(proc.path().join("uptime"), "1.0 1.0\n").unwrap();
        proc
    }

    fn path(proc: &TempDir) -> PathBuf {
        proc.path().to_path_buf()
    }

    #[test]
    fn a_stat_line_is_read_past_a_name_that_holds_parentheses() {
        let stat = parse(b"9 (a) b) (c) R 1 9 9 0 -1 0").unwrap();
        assert_eq!(stat.name, "a) b) (c");
        assert_eq!((stat.state, stat.ppid, stat.pgrp), ('R', 1, 9));
        assert_eq!((stat.session, stat.tpgid), (9, -1));
        for malformed in [
            &b""[..],
            b"9 a R 1 9 9 0 9",
            b"x (a) R 1 9 9 0 9",
            b"9 (a) R 1 9 9 0",
            b"9 (a) RR 1 9 9 0 9",
            b"9 (a) R x 9 9 0 9",
            b"9 )a( R 1 9 9 0 9",
        ] {
            assert_eq!(parse(malformed), None, "{malformed:?}");
        }
    }

    /// This process's own line, as the kernel writes it, parses to its pid.
    #[test]
    fn the_kernels_own_line_parses() {
        let stat = read_stat(Path::new(PROC), std::process::id())
            .unwrap()
            .unwrap();
        assert_eq!(stat.pid, std::process::id());
        assert!(!stat.gone());
        assert_eq!(read_stat(Path::new("/nonexistent-proc"), 1).unwrap(), None);
    }

    /// An idle shell holds the foreground alone; a job-control shell's
    /// job is another group, named by its leader; td's shell, without job
    /// control, runs its command in its own group, named as its child.
    #[test]
    fn a_job_is_another_process_in_the_foreground() {
        let idle = table(&[
            (100, "sh", 'S', 1, 100, 100, 100),
            (7, "x", 'S', 1, 7, 7, -1),
        ]);
        assert_eq!(busy(&path(&idle), 100, Some("sh")), Busy::Idle);
        let job_control = table(&[
            (100, "bash", 'S', 1, 100, 100, 200),
            (201, "cc1", 'R', 200, 200, 100, 200),
            (200, "make", 'S', 100, 200, 100, 200),
        ]);
        assert_eq!(
            busy(&path(&job_control), 100, Some("bash")),
            Busy::Job(Some("make".into()))
        );
        let td_sh = table(&[
            (100, "sh", 'S', 1, 100, 100, 100),
            (150, "less", 'S', 140, 100, 100, 100),
            (140, "vim", 'S', 100, 100, 100, 100),
            // A background job in a group of its own is not in the
            // foreground, and a zombie is not running.
            (130, "sleep", 'S', 100, 130, 100, 100),
            (120, "old", 'Z', 100, 100, 100, 100),
        ]);
        assert_eq!(
            busy(&path(&td_sh), 100, Some("sh")),
            Busy::Job(Some("vim".into()))
        );
    }

    /// The foreground group whose leader has gone is still a job, named by
    /// a member if one remains, or by none.
    #[test]
    fn a_foreground_group_without_its_leader_is_still_a_job() {
        let proc = table(&[
            (100, "bash", 'S', 1, 100, 100, 200),
            (202, "tail", 'S', 1, 200, 100, 200),
        ]);
        assert_eq!(
            busy(&path(&proc), 100, Some("bash")),
            Busy::Job(Some("tail".into()))
        );
        let proc = table(&[(100, "bash", 'S', 1, 100, 100, 200)]);
        assert_eq!(busy(&path(&proc), 100, Some("bash")), Busy::Job(None));
    }

    /// A named program is the job while it runs, named by the group the
    /// terminal gave the foreground to when that is another, else by
    /// itself; its own command line cannot put controls on the line.
    #[test]
    fn a_named_program_is_the_job_while_it_runs() {
        let proc = table(&[(100, "vim", 'S', 1, 100, 100, 100)]);
        assert_eq!(busy(&path(&proc), 100, None), Busy::Job(Some("vim".into())));
        let proc = table(&[
            (100, "bash", 'S', 1, 100, 100, 300),
            (300, "top", 'S', 100, 300, 100, 300),
        ]);
        assert_eq!(busy(&path(&proc), 100, None), Busy::Job(Some("top".into())));
        // td's profile: a command that leads no session has no terminal.
        let proc = table(&[(100, "td-jail\u{7}", 'S', 1, 1, 1, -1)]);
        assert_eq!(
            busy(&path(&proc), 100, None),
            Busy::Job(Some("td-jail".into()))
        );
    }

    /// A child already gone ends the terminal anyway; one that cannot be
    /// read, or a shell that holds no terminal, is asked about.
    #[test]
    fn what_cannot_be_read_is_asked_about() {
        let proc = table(&[(100, "sh", 'Z', 1, 100, 100, -1)]);
        assert_eq!(busy(&path(&proc), 100, Some("sh")), Busy::Idle);
        assert_eq!(busy(&path(&proc), 101, Some("sh")), Busy::Idle, "reaped");
        let proc = table(&[(100, "sh", 'S', 1, 100, 100, -1)]);
        assert_eq!(busy(&path(&proc), 100, Some("sh")), Busy::Unknown);
        let proc = table(&[(100, "sh", 'S', 1, 100, 1, 100)]);
        assert_eq!(
            busy(&path(&proc), 100, Some("sh")),
            Busy::Unknown,
            "no leader"
        );
        let proc = table(&[(100, "sh", 'S', 1, 100, 100, 100)]);
        std::fs::write(proc.path().join("100/stat"), "garbage").unwrap();
        assert_eq!(busy(&path(&proc), 100, Some("sh")), Busy::Unknown);
        // A member whose line cannot be read leaves the scan not whole.
        let proc = table(&[
            (100, "sh", 'S', 1, 100, 100, 100),
            (101, "x", 'S', 1, 1, 1, 1),
        ]);
        std::fs::write(proc.path().join("101/stat"), "garbage").unwrap();
        assert_eq!(busy(&path(&proc), 100, Some("sh")), Busy::Unknown);
    }

    /// A job a Control-z stopped asks though the shell holds the
    /// foreground, named after any running one; a stopped process of
    /// another session does not.
    #[test]
    fn a_stopped_job_asks() {
        let proc = table(&[
            (100, "bash", 'S', 1, 100, 100, 100),
            (300, "vim", 'T', 100, 300, 100, 100),
            (400, "other", 'T', 1, 400, 400, -1),
        ]);
        assert_eq!(
            busy(&path(&proc), 100, Some("bash")),
            Busy::Stopped(Some("vim".into()))
        );
        let proc = table(&[
            (100, "sh", 'S', 1, 100, 100, 100),
            (300, "vim", 't', 100, 300, 100, 100),
            (350, "make", 'R', 100, 100, 100, 100),
        ]);
        assert_eq!(
            busy(&path(&proc), 100, Some("sh")),
            Busy::Job(Some("make".into()))
        );
        let proc = table(&[
            (100, "bash", 'S', 1, 100, 100, 200),
            (300, "vim", 'T', 100, 300, 100, 200),
        ]);
        assert_eq!(busy(&path(&proc), 100, Some("bash")), Busy::Job(None));
    }

    /// A shell that exec'd another program is that program, a job while
    /// it runs; a named program that leads no session is named by itself,
    /// whatever terminal it inherited says, and a zombie leader names
    /// nothing.
    #[test]
    fn an_execd_shell_or_a_detached_program_is_named_by_itself() {
        let proc = table(&[(100, "vim", 'S', 1, 100, 100, 100)]);
        assert_eq!(
            busy(&path(&proc), 100, Some("sh")),
            Busy::Job(Some("vim".into()))
        );
        let proc = table(&[
            (100, "app", 'S', 1, 50, 50, 555),
            (555, "elsewhere", 'S', 1, 555, 50, 555),
        ]);
        assert_eq!(busy(&path(&proc), 100, None), Busy::Job(Some("app".into())));
        let proc = table(&[
            (100, "bash", 'S', 1, 100, 100, 300),
            (300, "gone", 'Z', 100, 300, 100, 300),
        ]);
        assert_eq!(
            busy(&path(&proc), 100, None),
            Busy::Job(Some("bash".into()))
        );
    }

    /// The kernel's name for a program is its last component's first 15
    /// bytes.
    #[test]
    fn the_kernels_name_is_the_programs_cut_to_fifteen_bytes() {
        assert_eq!(kernel_name(Path::new("/bin/sh")).as_deref(), Some("sh"));
        assert_eq!(
            kernel_name(Path::new("/usr/bin/a-very-long-shell-name")).as_deref(),
            Some("a-very-long-she")
        );
        assert_eq!(kernel_name(Path::new("/")), None);
        let own = read_stat(Path::new(PROC), std::process::id())
            .unwrap()
            .unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(kernel_name(&exe), Some(own.name));
    }

    /// A process gone between the listing and the read is skipped, by
    /// either way the kernel says so; other failures are not, and a line
    /// past the bound is refused.
    #[test]
    fn a_vanished_process_is_told_from_an_unreadable_one() {
        assert!(vanished(&io::Error::from_raw_os_error(ESRCH)));
        assert!(vanished(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(!vanished(&io::Error::from_raw_os_error(13)));
        assert!(!vanished(&io::Error::from(io::ErrorKind::InvalidData)));
        let proc = table(&[(100, "sh", 'S', 1, 100, 100, 100)]);
        let long = "x".repeat(usize::try_from(MAX_STAT_BYTES).unwrap() + 1);
        std::fs::write(proc.path().join("100/stat"), long).unwrap();
        assert!(read_stat(&path(&proc), 100).is_err());
        assert_eq!(read_stat(&path(&proc), 101).unwrap(), None);
    }

    #[test]
    fn a_shown_name_is_bounded_and_printable() {
        assert_eq!(shown(&"x".repeat(100)).unwrap().len(), MAX_NAME_CHARS);
        assert_eq!(shown("\u{202e}\u{1b}"), None);
        assert_eq!(shown(" vim "), Some("vim".into()));
    }

    const FIXTURE: &str = "TD_TERM_JOB_FIXTURE";

    /// A child of the live test, run by it: as `leader`, a stand-in shell
    /// without job control that runs a member in its own group until a
    /// line arrives, then waits alone for another; as `member`, that
    /// command.
    #[test]
    #[ignore = "a child of the live job test, run by it"]
    fn job_fixture() {
        let Some(role) = std::env::var_os(FIXTURE) else {
            return;
        };
        let mut line = String::new();
        if role != "leader" {
            std::io::stdin().read_line(&mut line).unwrap();
            return;
        }
        let mut member = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "job::tests::job_fixture", "--ignored"])
            .env(FIXTURE, "member")
            .spawn()
            .unwrap();
        println!("TD-TERM-JOB-FIXTURE member {}", member.id());
        assert!(member.wait().unwrap().success());
        println!("TD-TERM-JOB-FIXTURE alone");
        std::io::stdin().read_line(&mut line).unwrap();
    }

    /// The kernel's own table, through a real terminal: a session leader
    /// running a command in its group is busy, named as the command, and
    /// idle once it is alone.
    #[test]
    fn a_live_terminal_reports_its_leaders_command() {
        use std::io::Write;
        use std::sync::mpsc::sync_channel;
        use td_ui::pty::{self, ChildCommand, Output, Pty};
        let pty = Pty::open().unwrap();
        let slave = pty.peer().unwrap();
        let program = std::env::current_exe().unwrap();
        let command = ChildCommand {
            program: program.clone(),
            leads_session: true,
            arguments: [
                "--exact",
                "job::tests::job_fixture",
                "--ignored",
                "--nocapture",
            ]
            .map(Into::into)
            .to_vec(),
        };
        let environment = [(FIXTURE, "leader")];
        let mut leader = pty::spawn(&command, &environment, &std::env::temp_dir(), slave).unwrap();
        let mut input = pty.master().try_clone().unwrap();
        let (sender, receiver) = sync_channel(64);
        let output = pty.master().try_clone().unwrap();
        pty::spawn_reader(output, sender, std::convert::identity, || {}).unwrap();
        let mut seen = String::new();
        let mut marker = |wanted: &str| -> String {
            loop {
                let complete = seen.rsplit_once('\n').map_or("", |(head, _)| head);
                let found = complete.lines().find_map(|line| {
                    line.trim_end()
                        .strip_prefix("TD-TERM-JOB-FIXTURE ")
                        .and_then(|tail| tail.strip_prefix(wanted))
                        .map(str::to_owned)
                });
                if let Some(found) = found {
                    return found;
                }
                match receiver.recv_timeout(std::time::Duration::from_secs(30)) {
                    Ok(Output::Bytes(chunk)) => seen.push_str(&String::from_utf8_lossy(&chunk)),
                    other => panic!("no {wanted:?} in {seen:?}: {other:?}"),
                }
            }
        };
        let member: u32 = marker("member ").parse().unwrap();
        let shell = kernel_name(&program);
        let shell = shell.as_deref();
        let name = read_stat(Path::new(PROC), member).unwrap().unwrap().name;
        assert_eq!(
            busy(Path::new(PROC), leader.id(), shell),
            Busy::Job(shown(&name))
        );
        assert_eq!(
            busy(Path::new(PROC), leader.id(), None),
            Busy::Job(shown(&name)),
            "named by itself, its own group the foreground"
        );
        input.write_all(b"\n").unwrap();
        marker("alone");
        assert_eq!(busy(Path::new(PROC), leader.id(), shell), Busy::Idle);
        input.write_all(b"\n").unwrap();
        assert!(leader.wait().unwrap().success());
        assert_eq!(
            busy(Path::new(PROC), leader.id(), shell),
            Busy::Idle,
            "reaped"
        );
    }

    #[test]
    fn the_selftest_passes() {
        selftest().unwrap();
    }
}
