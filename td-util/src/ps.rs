//! `ps` — the process table, read from /proc.
//!
//! Like busybox `ps` (the applet this replaces) every process is listed. POSIX's
//! default "same controlling terminal and effective uid" restriction would need
//! a controlling-terminal lookup this deliberately does not do, so the selection
//! flags that ask for all processes (`-e`/`-A`, and BSD `a`/`x`) are accepted as
//! no-ops. The format flags (`-f`, BSD `u`) are accepted too, and ignored: the
//! column set below is fixed. That combination is what lets a script's `ps -ef`
//! or `ps aux` keep working across the busybox cutover.

/// procfs reports CPU time in USER_HZ, which Linux fixes at 100 for /proc
/// regardless of the kernel's internal CONFIG_HZ.
const USER_HZ: u64 = 100;

/// Selection/format letters accepted and ignored, in both the `-ef` (clustered
/// SysV) and `aux` (BSD, no dash) spellings.
const ACCEPTED_SYSV: &str = "eAf";
const ACCEPTED_BSD: &str = "aux";

pub struct Proc {
    pub pid: u64,
    pub tty: String,
    pub state: char,
    pub ticks: u64,
    pub cmd: String,
}

/// Header and row share one format so the columns cannot drift apart. STAT is
/// busybox `ps`'s column; the other four are procps'.
fn row(pid: &str, tty: &str, state: &str, time: &str, cmd: &str) -> String {
    format!("{pid:>5} {tty:<8} {state:<4} {time:>8} {cmd}\n")
}

/// `-ef` is one argument, not two -- rejecting the clustered form would break the
/// single most common way `ps` is invoked from a script.
fn accept_flags(arg: &str) -> bool {
    match arg.strip_prefix('-') {
        Some(rest) => !rest.is_empty() && rest.chars().all(|c| ACCEPTED_SYSV.contains(c)),
        None => !arg.is_empty() && arg.chars().all(|c| ACCEPTED_BSD.contains(c)),
    }
}

pub fn run(args: &[String]) -> Result<u8, String> {
    for a in args {
        if !accept_flags(a) {
            return Err(format!(
                "unrecognised option '{a}'\nusage: ps [-eAf] [aux]  (all accepted and ignored; \
                 every process is always listed in one fixed format)"
            ));
        }
    }
    let mut rows = collect("/proc")?;
    rows.sort_by_key(|p| p.pid);
    let mut out = row("PID", "TTY", "STAT", "TIME", "CMD");
    for p in &rows {
        out.push_str(&row(
            &p.pid.to_string(),
            &p.tty,
            &p.state.to_string(),
            &format_time(p.ticks),
            &p.cmd,
        ));
    }
    crate::emit(&out)?;
    Ok(0)
}

/// A process that exits mid-scan makes its /proc entry vanish between readdir
/// and read; skip it rather than failing the whole listing. Everything else is
/// read as BYTES and decoded lossily: `comm` and `cmdline` are arbitrary bytes
/// (a process can `prctl(PR_SET_NAME)` itself invalid UTF-8), and a `read_to_string`
/// there would drop the process from the table entirely -- hiding from `ps` must
/// not be one syscall away.
fn collect(proc_root: &str) -> Result<Vec<Proc>, String> {
    let entries = std::fs::read_dir(proc_root).map_err(|e| format!("{proc_root}: {e}"))?;
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u64>() else {
            continue;
        };
        let Some(stat) = read_lossy(&format!("{proc_root}/{pid}/stat")) else {
            continue;
        };
        let Some(parsed) = parse_stat(&stat) else {
            continue;
        };
        let cmdline = read_lossy(&format!("{proc_root}/{pid}/cmdline")).unwrap_or_default();
        rows.push(Proc {
            pid,
            tty: tty_name(parsed.tty_nr),
            state: parsed.state,
            ticks: parsed.ticks,
            cmd: command(&cmdline, comm(&stat)),
        });
    }
    Ok(rows)
}

fn read_lossy(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub struct Stat {
    pub state: char,
    pub tty_nr: u64,
    pub ticks: u64,
}

/// /proc/<pid>/stat is `pid (comm) state ...`. `comm` is unquoted and may itself
/// contain spaces or a `)`, so the split point is the LAST `)` in the record --
/// splitting on whitespace from the left mis-parses any process whose name has a
/// space in it (`kworker/0:1-events` is fine, `(sd-pam)` is not).
///
/// A field that will not parse degrades that COLUMN, it does not drop the row:
/// an all-or-nothing parse means one odd field silently removes a process from
/// the table, which is the opposite of what a diagnostics tool should do.
pub fn parse_stat(stat: &str) -> Option<Stat> {
    let close = stat.bytes().rposition(|b| b == b')')?;
    let rest = stat.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` begins at stat field 3, so field N sits at index N-3.
    let num = |i: usize| {
        fields
            .get(i)
            .and_then(|f| f.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some(Stat {
        state: fields.first().and_then(|f| f.chars().next()).unwrap_or('?'),
        tty_nr: num(4),
        ticks: num(11).saturating_add(num(12)),
    })
}

pub fn comm(stat: &str) -> Option<&str> {
    let open = stat.bytes().position(|b| b == b'(')?;
    let close = stat.bytes().rposition(|b| b == b')')?;
    stat.get(open + 1..close)
}

/// A kernel thread has an empty cmdline; ps shows its comm in brackets.
pub fn command(cmdline: &str, comm: Option<&str>) -> String {
    let joined: Vec<&str> = cmdline.split('\0').filter(|s| !s.is_empty()).collect();
    if joined.is_empty() {
        return format!("[{}]", comm.unwrap_or("?"));
    }
    joined.join(" ")
}

/// Decode stat's `tty_nr` (an encoded major/minor) into a device name. Only the
/// devices td ships are named; anything else prints `?`, exactly as ps does for
/// a process with no controlling terminal.
pub fn tty_name(tty_nr: u64) -> String {
    if tty_nr == 0 {
        return "?".to_string();
    }
    let major = (tty_nr >> 8) & 0xfff;
    let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xfff00);
    match major {
        4 if minor >= 64 => format!("ttyS{}", minor - 64),
        4 => format!("tty{minor}"),
        5 if minor == 1 => "console".to_string(),
        136..=143 => format!("pts/{}", minor.saturating_add((major - 136) * 256)),
        _ => "?".to_string(),
    }
}

/// procps' `MMM:SS`: minutes are not wrapped into hours.
pub fn format_time(ticks: u64) -> String {
    let secs = ticks / USER_HZ;
    format!("{}:{:02}", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    // The gate lints only non-test targets, but keep `cargo clippy --tests`
    // clean for local runs too.
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    // A real init line, plus the two shapes that break naive parsers.
    const INIT: &str = "1 (init) S 0 1 1 0 -1 4194560 1234 0 0 0 12 34 0 0 20 0 1 0 5 0 0 0 0\n";
    const SPACED: &str = "42 (my proc) R 1 42 42 1025 42 0 0 0 0 0 100 200 0 0 20 0 1 0 5 0 0 0 0\n";
    const PARENS: &str = "7 ((sd-pam)) S 1 7 7 0 -1 0 0 0 0 0 1 2 0 0 20 0 1 0 5 0 0 0 0\n";

    #[test]
    fn parses_state_tty_and_cpu_time() {
        let s = parse_stat(INIT).unwrap();
        assert_eq!(s.state, 'S');
        assert_eq!(s.tty_nr, 0);
        assert_eq!(s.ticks, 12 + 34);
    }

    /// The bug this guards: splitting from the left puts every field one slot off
    /// for a process whose comm contains a space, so TIME and TTY come out wrong.
    #[test]
    fn comm_with_spaces_or_parens_does_not_shift_fields() {
        let s = parse_stat(SPACED).unwrap();
        assert_eq!(s.state, 'R');
        assert_eq!(s.tty_nr, 1025);
        assert_eq!(s.ticks, 300);
        assert_eq!(comm(SPACED), Some("my proc"));

        let p = parse_stat(PARENS).unwrap();
        assert_eq!(p.state, 'S');
        assert_eq!(p.ticks, 3);
        assert_eq!(comm(PARENS), Some("(sd-pam)"));
    }

    /// A record with no `(comm)` at all is unparseable; a TRUNCATED or odd one
    /// must still yield a row, with only the unreadable columns degraded.
    #[test]
    fn odd_records_degrade_the_column_not_the_row() {
        assert!(parse_stat("").is_none());
        assert!(parse_stat("no parens here").is_none());

        let s = parse_stat("1 (init)").unwrap();
        assert_eq!(s.state, '?');
        assert_eq!(s.tty_nr, 0);
        assert_eq!(s.ticks, 0);

        // A tty_nr the kernel printed with a signed formatter must not evict the
        // process from the table.
        let s = parse_stat("1 (init) S 0 1 1 -1 -1 0 0 0 0 0 7 8 0 0 20 0 1 0 5").unwrap();
        assert_eq!(s.state, 'S');
        assert_eq!(s.tty_nr, 0);
        assert_eq!(s.ticks, 15);
    }

    #[test]
    fn tty_numbers_decode_to_device_names() {
        assert_eq!(tty_name(0), "?");
        // major 4, minor 64 -> ttyS0, td's serial console.
        assert_eq!(tty_name((4 << 8) | 64), "ttyS0");
        assert_eq!(tty_name((4 << 8) | 1), "tty1");
        assert_eq!(tty_name((5 << 8) | 1), "console");
        assert_eq!(tty_name((136 << 8) | 3), "pts/3");
        assert_eq!(tty_name((99 << 8) | 3), "?");
    }

    #[test]
    fn cmdline_nuls_become_spaces_and_kernel_threads_bracket() {
        assert_eq!(command("/bin/sh\0-c\0echo hi\0", Some("sh")), "/bin/sh -c echo hi");
        assert_eq!(command("", Some("kthreadd")), "[kthreadd]");
        assert_eq!(command("\0\0", Some("kthreadd")), "[kthreadd]");
        assert_eq!(command("", None), "[?]");
    }

    #[test]
    fn cpu_time_renders_as_minutes_and_seconds() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(USER_HZ * 5), "0:05");
        assert_eq!(format_time(USER_HZ * 61), "1:01");
        // procps does not wrap minutes into hours.
        assert_eq!(format_time(USER_HZ * 3600), "60:00");
    }

    /// `ps -ef` and `ps aux` are how scripts actually spell this; both must be
    /// accepted, and an unknown letter must still be rejected.
    #[test]
    fn clustered_sysv_and_bsd_flags_are_accepted() {
        for ok in ["-e", "-A", "-f", "-ef", "-Af", "a", "x", "aux", "ax"] {
            assert!(accept_flags(ok), "'{ok}' must be accepted");
        }
        for bad in ["-Z", "-eZ", "auxZ", "-", "", "--help", "junk"] {
            assert!(!accept_flags(bad), "'{bad}' must be rejected");
        }
    }

    /// Non-UTF-8 in `comm`/`cmdline` must not evict the process, and a userspace
    /// process must never be rendered in the `[kernel thread]` notation.
    #[test]
    fn non_utf8_proc_bytes_still_list_the_process() {
        let root = std::env::temp_dir().join(format!("td-util-ps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (pid, comm, cmdline) in [
            (1u32, &b"init"[..], &b"/sbin/init\0"[..]),
            (2, b"bad\xffname", b"/usr/bin/caf\xe9\0-v\0"),
        ] {
            let d = root.join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            let mut stat = Vec::new();
            stat.extend_from_slice(format!("{pid} (").as_bytes());
            stat.extend_from_slice(comm);
            stat.extend_from_slice(b") S 0 1 1 0 -1 0 0 0 0 0 1 2 0 0 20 0 1 0 5\n");
            std::fs::write(d.join("stat"), &stat).unwrap();
            std::fs::write(d.join("cmdline"), cmdline).unwrap();
        }
        let rows = collect(&root.display().to_string()).unwrap();
        assert_eq!(rows.len(), 2, "a non-UTF-8 comm must not hide a process from ps");
        // `position` + index rather than the search combinator, whose name the
        // ladder guard bans in embedded step content.
        let odd = &rows[rows.iter().position(|p| p.pid == 2).unwrap()];
        assert!(
            !odd.cmd.starts_with('['),
            "a userspace process with a non-UTF-8 cmdline was rendered as a kernel thread: {:?}",
            odd.cmd
        );
        assert!(odd.cmd.contains("-v"), "cmdline arguments were lost: {:?}", odd.cmd);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The live /proc must always contain PID 1, and scanning it must not error.
    #[test]
    fn collect_reads_the_live_process_table() {
        let rows = collect("/proc").unwrap();
        assert!(rows.iter().any(|p| p.pid == 1), "PID 1 missing from the scan");
        // Every row must be fully populated -- a blank command or a degraded state
        // means the stat parse silently fell back on a real kernel's /proc.
        for p in &rows {
            assert!(!p.cmd.is_empty(), "pid {} has an empty CMD", p.pid);
            assert!(p.state.is_ascii_alphabetic(), "pid {} has state {:?}", p.pid, p.state);
        }
    }

    #[test]
    fn header_and_rows_share_column_widths() {
        let header = row("PID", "TTY", "STAT", "TIME", "CMD");
        let line = row("1", "?", "S", "0:01", "[init]");
        assert_eq!(header.len() - "CMD\n".len(), line.len() - "[init]\n".len());
        assert!(header.starts_with("  PID TTY      STAT"));
    }

    #[test]
    fn collect_on_a_missing_root_is_an_error() {
        assert!(collect("/nonexistent-proc-root").is_err());
    }
}
