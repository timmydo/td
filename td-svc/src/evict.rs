//! What a PREVIOUS supervisor started, so this one can evict it.
//!
//! PID 1 respawns td-svc unconditionally, so a td-svc that dies leaves its
//! services running — reparented to PID 1, which does not supervise them. The
//! replacement knows nothing about them and starts its own copies: two sshds
//! on one port, two greeters on one terminal. That is the same duplicate
//! `abandon` refuses to create, reached by a different road.
//!
//! Not adoption. Orphans are PID 1's children and td-svc cannot `wait4` them,
//! so **I4** ("stopped" needs the leader REAPED) is unreachable for them by
//! construction; the most this can do is empty the containment and say so when
//! it cannot.
//!
//! ## Killing from a file
//!
//! A pid on its own is a promise the kernel does not keep: the recorded process
//! may be long gone and its number reissued to something this supervisor must
//! not touch. `(pid, starttime)` is the pair that already closes the TERM→KILL
//! reuse window (DESIGN.md §4), and it works here for the same reason — field
//! 22 of `/proc/<pid>/stat` is set at fork and never changes, so a match means
//! the very process that was recorded, not merely its number.
//!
//! The CONTAINMENT is recorded rather than re-derived, and that is the part
//! worth stating: a `tty=` unit's device lived in the dead supervisor's memory,
//! and the running child never carries it (td-svc opens the console
//! `O_NOCTTY`, so its field 7 is 0 for life). Re-deriving from `/proc` would
//! therefore narrow a console unit to its wrapper and leave the login tree —
//! the exact thing `Containment::Console` exists to reach — running.
//!
//! ## The file
//!
//! One line per live service, `pid starttime tty name`, rewritten whole on
//! every spawn rather than appended to: a crash-looping service would grow an
//! append-only file without bound, and this lives in `/run`, which is RAM.
//! Rewriting from the live set also makes it self-cleaning, so nothing has to
//! remember to remove an entry when a service exits.
//!
//! Stale entries are harmless by design — the identity check filters them —
//! which is what lets the file be a superset of the truth and lets the writer
//! run at exactly one place in the supervisor.
//!
//! Whole-file rewriting has one trap: an orphan that could NOT be cleared would
//! drop out of the record on the next spawn, so a supervisor that dies after a
//! failed eviction would hand its successor a clean file and a running
//! duplicate. Those entries are carried FORWARD instead, and only a process
//! proven gone leaves the record.
//!
//! ## Who may write it
//!
//! These lines name processes td-svc signals as root, so being able to write
//! them is being able to choose them. `/run/td-svc` is created at 0700 by
//! whichever of `control::bind` and this module reaches it first — eviction
//! runs BEFORE the socket is bound, so it cannot use `create_dir_all` and its
//! `0777 & ~umask` — and a record found in a directory that fails that test is
//! ignored rather than narrowed, because narrowing does not revoke a descriptor
//! somebody already holds.

use std::fs;
use std::io;

/// Where the record lives. `/run` is tmpfs, so a fresh boot starts with no
/// file and nothing to evict — which is the ordinary case, not a special one.
pub const STATE: &str = "/run/td-svc/started";

/// One service a previous supervisor started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub pid: i32,
    /// Field 22 of `/proc/<pid>/stat`: set at fork, never changes. Without it
    /// this file is a list of numbers the kernel may have reissued.
    pub starttime: u64,
    /// The terminal the instance ACTUALLY got, or 0 for none. Recorded because
    /// it cannot be recovered: see the module header.
    pub tty: i32,
    pub name: String,
}

/// Render the record. `name` last and unquoted: the table refuses any name
/// outside `[A-Za-z0-9_-]`, so it can hold neither a space nor a newline.
pub fn render(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&format!("{} {} {} {}\n", e.pid, e.starttime, e.tty, e.name));
    }
    out
}

/// Parse the record, returning what was understood and a complaint per line
/// that was not. `source` names the file the complaints are about — `read`
/// takes a path, so a scratch record must not be reported against `STATE`.
///
/// A line that does not parse is SKIPPED, not fatal. Refusing the whole file
/// over one bad line would leave every orphan it named running, which is the
/// failure this module exists to prevent — and the identity check downstream
/// is what keeps a skipped-vs-kept decision from ever being dangerous: a line
/// that survives parsing still has to name a process that is really there.
pub fn parse(source: &str, text: &str) -> (Vec<Entry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut problems = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let number = index.saturating_add(1);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(4, ' ');
        let parsed = (|| {
            let pid: i32 = fields.next()?.parse().ok()?;
            let starttime: u64 = fields.next()?.parse().ok()?;
            let tty: i32 = fields.next()?.parse().ok()?;
            let name = fields.next()?;
            if name.is_empty() {
                return None;
            }
            Some(Entry {
                pid,
                starttime,
                tty,
                name: name.to_string(),
            })
        })();
        match parsed {
            // A pid that is not positive names no process, and `0` and `-1`
            // are the two `kill(2)` targets that mean something td-svc never
            // does. Refused here as well as in `send_signal`, because a record
            // that reached the signal path at all would already be a bug.
            Some(entry) if entry.pid > 0 => entries.push(entry),
            Some(entry) => problems.push(format!(
                "{source} line {number}: pid {} is not a process",
                entry.pid
            )),
            None => problems.push(format!("{source} line {number}: cannot read '{line}'")),
        }
    }
    (entries, problems)
}

/// Write the record, atomically. Same staging-then-rename shape as the
/// shutdown marker: a reader must see the whole previous record or the whole
/// new one, never a half-written line that parses into a pid.
pub fn write(path: &str, entries: &[Entry]) -> io::Result<()> {
    let staging = format!("{path}.new");
    if let Some(dir) = std::path::Path::new(path).parent().and_then(|d| d.to_str()) {
        // NOT `create_dir_all`, which is `0777 & ~umask` and td-svc inherits
        // PID 1's umask. Eviction writes here before the control socket is
        // bound, so this is the first creator and sets the mode for both.
        crate::control::ensure_dir(dir)?;
    }
    fs::write(&staging, render(entries))?;
    // A failed rename would otherwise leave the staging file behind on a
    // filesystem the supervisor has already shown it cannot write.
    match fs::rename(&staging, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&staging);
            Err(e)
        }
    }
}

/// Read the record. A missing file is not an error — it is the ordinary case.
pub fn read(path: &str) -> (Vec<Entry>, Vec<String>) {
    // The entries name processes root is about to signal, so a directory
    // somebody else can write is a way to choose them. Refused rather than
    // narrowed, for the reason `bind` gives: narrowing does not revoke a
    // descriptor already held.
    if let Some(dir) = std::path::Path::new(path).parent().and_then(|d| d.to_str()) {
        if fs::symlink_metadata(path).is_ok() && !crate::control::dir_is_trusted(dir) {
            return (
                Vec::new(),
                vec![format!("{dir}: not ours or writable by others; ignoring the record in it")],
            );
        }
    }
    match fs::read_to_string(path) {
        Ok(text) => parse(path, &text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (Vec::new(), Vec::new()),
        Err(e) => (Vec::new(), vec![format!("{path}: {e}")]),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn entry(pid: i32, starttime: u64, tty: i32, name: &str) -> Entry {
        Entry {
            pid,
            starttime,
            tty,
            name: name.to_string(),
        }
    }

    /// What is written is what is read back.
    #[test]
    fn a_record_round_trips() {
        let entries = vec![entry(41, 900, 0, "sshd"), entry(42, 901, 1032, "greeter")];
        let (back, problems) = parse(STATE, &render(&entries));
        assert_eq!(back, entries, "the record did not survive a round trip");
        assert!(problems.is_empty(), "a clean record complained: {problems:?}");
    }

    /// The tty is carried, because it cannot be recovered.
    ///
    /// A console unit whose device is lost narrows to its wrapper, and the
    /// login tree the eviction exists to reach survives it. Nothing else in
    /// the record is irreplaceable — pid and starttime are both readable from
    /// `/proc` — so this is the field a future edit is most likely to drop as
    /// redundant.
    #[test]
    fn the_terminal_is_part_of_the_record() {
        let (back, _) = parse(STATE, &render(&[entry(7, 5, 1032, "greeter")]));
        assert_eq!(
            back.first().map(|e| e.tty),
            Some(1032),
            "the recorded terminal was lost"
        );
    }

    /// One unreadable line does not discard the rest.
    ///
    /// The alternative — refusing a record that does not wholly parse — leaves
    /// every orphan it named running, which is the failure this file exists to
    /// prevent. Skipping is safe because a line that DOES parse still has to
    /// name a process that is really there before anything is signalled.
    #[test]
    fn a_torn_line_is_skipped_and_the_rest_kept() {
        let text = "41 900 0 sshd\nrubbish\n\n42 901 0 netup\n";
        let (entries, problems) = parse(STATE, text);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["sshd", "netup"],
            "a torn line took good entries with it"
        );
        assert_eq!(problems.len(), 1, "the skipped line was not reported");
        assert!(
            problems.first().is_some_and(|p| p.contains("rubbish")),
            "the complaint must name the line: {problems:?}"
        );
    }

    /// A record that would signal something other than a process is refused.
    ///
    /// `0` addresses the caller's own process group and `-1` every process it
    /// may signal. Neither can be produced by the writer, which is exactly why
    /// a READER must not assume it: the file is the one input here that a
    /// wedged disk or a foreign writer can shape.
    #[test]
    fn a_pid_that_is_not_a_process_is_refused() {
        for bad in ["0 900 0 a", "-1 900 0 a", "-99 900 0 a"] {
            let (entries, problems) = parse(STATE, bad);
            assert!(entries.is_empty(), "'{bad}' was accepted as a target");
            assert_eq!(problems.len(), 1, "'{bad}' was dropped without complaint");
        }
    }

    /// A name with no fields before it, a short line, a missing name.
    #[test]
    fn a_line_missing_a_field_is_not_half_read() {
        for bad in ["41", "41 900", "41 900 0", "41 900 0 ", "x 900 0 a", "41 y 0 a"] {
            let (entries, _) = parse(STATE, bad);
            assert!(entries.is_empty(), "'{bad}' parsed into an entry");
        }
    }

    /// The writer replaces; it does not append.
    ///
    /// An append-only record grows without bound under a crash-looping
    /// service, and this lives in `/run`, which is RAM.
    #[test]
    fn writing_replaces_the_whole_record() {
        let dir = format!(
            "{}/td-svc-evict-{}",
            std::env::temp_dir().display(),
            std::process::id()
        );
        let _ = fs::create_dir_all(&dir);
        let path = format!("{dir}/started");

        write(&path, &[entry(1, 1, 0, "a"), entry(2, 2, 0, "b")]).unwrap();
        write(&path, &[entry(3, 3, 0, "c")]).unwrap();
        let (entries, _) = read(&path);
        assert_eq!(
            entries,
            vec![entry(3, 3, 0, "c")],
            "the record accumulated instead of being replaced"
        );

        // And the staging file is not left behind to be read as a record.
        assert!(
            !std::path::Path::new(&format!("{path}.new")).exists(),
            "the staging file outlived the rename"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A missing record is the ordinary case, not a fault.
    #[test]
    fn no_record_is_not_an_error() {
        let (entries, problems) = read("/nonexistent/td-svc/started");
        assert!(entries.is_empty());
        assert!(
            problems.is_empty(),
            "a fresh boot must not complain about having nothing to evict: {problems:?}"
        );
    }
}
