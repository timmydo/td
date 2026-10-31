//! `ps` — the process table, read from /proc.
//!
//! The default table lists every process; legacy all-process flags remain
//! accepted. `-p` selects positive PIDs and repeatable `-o` chooses columns.
//! `stat` reports the kernel state letter. `lstart` uses English names and
//! td-civil's system/TZ zone (UTC fallback), independent of locale.

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
    pub start_ticks: Option<u64>,
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

const USAGE: &str = "usage: ps [-eAf] [aux] [-p PID[,PID...]] [-o FIELD[=HEADER][,FIELD...]]\nfields: pid, tty, stat (state letter), time, cmd, lstart (English local start time)\nlstart uses absolute TZ file paths or /etc/localtime, falling back to UTC.";

struct Column {
    name: String,
    header: String,
}

#[derive(Default)]
struct Options {
    pids: Vec<u64>,
    all: bool,
    columns: Vec<Column>,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "-p" || arg == "-o" {
            let value = args
                .next()
                .ok_or_else(|| format!("{arg} requires a value\n{USAGE}"))?;
            if arg == "-p" {
                for part in value.split(',').flat_map(str::split_whitespace) {
                    let pid = part
                        .parse::<u32>()
                        .ok()
                        .filter(|p| *p > 0 && *p <= i32::MAX as u32)
                        .ok_or_else(|| format!("invalid PID '{part}'"))?;
                    options.pids.push(u64::from(pid));
                }
                if value.trim().is_empty() || value.split(',').any(|v| v.trim().is_empty()) {
                    return Err("empty PID list element".into());
                }
            } else {
                for part in value.split(',') {
                    let (name, custom) = match part.split_once('=') {
                        Some((name, header)) => (name, Some(header)),
                        None => (part, None),
                    };
                    let header = match name {
                        "pid" => "PID",
                        "tty" => "TTY",
                        "stat" => "STAT",
                        "time" => "TIME",
                        "cmd" => "CMD",
                        "lstart" => "STARTED",
                        _ => return Err(format!("unknown output field '{name}'\n{USAGE}")),
                    };
                    options.columns.push(Column {
                        name: name.into(),
                        header: custom.unwrap_or(header).into(),
                    });
                }
            }
        } else if accept_flags(arg) {
            options.all |= arg.chars().any(|c| "eAax".contains(c));
        } else {
            return Err(format!("unrecognised option '{arg}'\n{USAGE}"));
        }
    }
    Ok(options)
}

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.len() == 1 && args.first().is_some_and(|a| a == "--help") {
        crate::emit(&format!("{USAGE}\n"))?;
        return Ok(0);
    }
    let options = options(args)?;
    let (out, code) = render("/proc", &options, &td_civil::Zone::local())?;
    crate::emit(&out)?;
    Ok(code)
}

fn render(
    proc_root: &str,
    options: &Options,
    zone: &td_civil::Zone,
) -> Result<(String, u8), String> {
    let mut rows = collect(proc_root)?;
    rows.retain(|p| options.all || options.pids.is_empty() || options.pids.contains(&p.pid));
    rows.sort_by_key(|p| p.pid);
    let boot = if options.columns.iter().any(|c| c.name == "lstart") && !rows.is_empty() {
        Some(
            boot_time(
                &std::fs::read_to_string(format!("{proc_root}/stat"))
                    .map_err(|e| format!("cannot read boot time: {e}"))?,
            )
            .ok_or("missing or invalid boot time")?,
        )
    } else {
        None
    };
    let mut out = String::new();
    if options.columns.is_empty() {
        out.push_str(&row("PID", "TTY", "STAT", "TIME", "CMD"));
    } else if options.columns.iter().any(|c| !c.header.is_empty()) {
        out.push_str(
            &options
                .columns
                .iter()
                .map(|c| c.header.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
        out.push('\n');
    }
    for p in &rows {
        if options.columns.is_empty() {
            out.push_str(&row(
                &p.pid.to_string(),
                &p.tty,
                &p.state.to_string(),
                &format_time(p.ticks),
                &p.cmd,
            ));
        } else {
            let values = options
                .columns
                .iter()
                .map(|column| {
                    Ok(match column.name.as_str() {
                        "pid" => p.pid.to_string(),
                        "tty" => p.tty.clone(),
                        "stat" => p.state.to_string(),
                        "time" => format_time(p.ticks),
                        "cmd" => p.cmd.clone(),
                        "lstart" => boot
                            .zip(p.start_ticks)
                            .and_then(|(b, t)| start_time(b, t, zone))
                            .ok_or_else(|| format!("cannot read start time for PID {}", p.pid))?,
                        _ => return Err("unknown output field".into()),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            out.push_str(&values.join(" "));
            out.push('\n');
        }
    }
    Ok((out, u8::from(rows.is_empty())))
}

fn boot_time(stat: &str) -> Option<u64> {
    for line in stat.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some("btime") {
            let value = fields.next()?.parse().ok()?;
            return fields.next().is_none().then_some(value);
        }
    }
    None
}

fn start_time(boot: u64, ticks: u64, zone: &td_civil::Zone) -> Option<String> {
    let unix = i64::try_from(boot.checked_add(ticks / USER_HZ)?).ok()?;
    let local = unix.checked_add(i64::from(zone.offset_at(unix)))?;
    let c = td_civil::unix_to_civil_utc_checked(local)?;
    const DAYS: &[&str] = &["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let day = DAYS.get(usize::try_from(local.div_euclid(86400).rem_euclid(7)).ok()?)?;
    let month = MONTHS.get(usize::from(c.month.checked_sub(1)?))?;
    Some(format!(
        "{day} {month} {:2} {:02}:{:02}:{:02} {:04}",
        c.day, c.hour, c.minute, c.second, c.year
    ))
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
            start_ticks: parsed.start_ticks,
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
    pub start_ticks: Option<u64>,
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
        start_ticks: fields.get(19).and_then(|f| f.parse().ok()),
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

    #[test]
    fn start_dates_and_bad_identity_fields() {
        let utc = td_civil::Zone::utc();
        assert_eq!(
            start_time(0, 0, &utc).as_deref(),
            Some("Thu Jan  1 00:00:00 1970")
        );
        assert_eq!(
            start_time(1709164800, 12345, &utc).as_deref(),
            Some("Thu Feb 29 00:02:03 2024")
        );
        assert!(start_time(u64::MAX, 100, &utc).is_none());
        assert!(start_time(i64::MAX as u64, 0, &utc).is_none());
        assert_eq!(parse_stat(SPACED).unwrap().start_ticks, Some(5));
        assert_eq!(parse_stat(PARENS).unwrap().start_ticks, Some(5));
        assert_eq!(parse_stat("1 (init) S").unwrap().start_ticks, None);
        assert_eq!(boot_time("cpu 1 2\nbtime 123\nprocesses 2\n"), Some(123));
        for bad in ["", "btime -1", "btime 1 2", "btime nope"] {
            assert!(boot_time(bad).is_none());
        }
    }

    #[test]
    fn local_start_time_crosses_calendar_day_and_year() {
        let mut data = Vec::new();
        // No transitions, one fixed +05:30 type in both TZif v2 blocks.
        for _ in 0..2 {
            data.extend_from_slice(b"TZif2");
            data.extend_from_slice(&[0; 15]);
            for count in [0u32, 0, 0, 0, 1, 4] {
                data.extend_from_slice(&count.to_be_bytes());
            }
            data.extend_from_slice(&19800i32.to_be_bytes());
            data.extend_from_slice(&[0, 0]);
            data.extend_from_slice(b"FIX\0");
        }
        data.extend_from_slice(b"\n<FIX>-5:30\n");
        let zone = td_civil::Zone::from_tzif(&data).unwrap();
        assert_eq!(
            start_time(1704047400, 0, &zone).as_deref(),
            Some("Mon Jan  1 00:00:00 2024")
        );
    }

    #[test]
    fn repeated_selection_custom_headers_and_missing_start_time() {
        let root = td_fs::TempDir::new("td-util-ps-fields").unwrap();
        std::fs::create_dir(root.join("42")).unwrap();
        std::fs::write(root.join("42/stat"), SPACED).unwrap();
        std::fs::write(root.join("stat"), "btime 0\n").unwrap();
        let args = [
            "-p",
            "1,42",
            "-p",
            "42",
            "-o",
            "pid=Process,stat=",
            "-o",
            "lstart=",
        ]
        .map(String::from);
        let opts = options(&args).unwrap();
        let (text, code) = render(root.to_str().unwrap(), &opts, &td_civil::Zone::utc()).unwrap();
        assert_eq!(code, 0);
        assert_eq!(text, "Process  \n42 R Thu Jan  1 00:00:00 1970\n");
        std::fs::write(root.join("42/stat"), "42 (gone) Z").unwrap();
        assert!(render(root.to_str().unwrap(), &opts, &td_civil::Zone::utc()).is_err());
    }

    // A real init line, plus the two shapes that break naive parsers.
    const INIT: &str = "1 (init) S 0 1 1 0 -1 4194560 1234 0 0 0 12 34 0 0 20 0 1 0 5 0 0 0 0\n";
    const SPACED: &str =
        "42 (my proc) R 1 42 42 1025 42 0 0 0 0 0 100 200 0 0 20 0 1 0 5 0 0 0 0\n";
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
        assert_eq!(
            command("/bin/sh\0-c\0echo hi\0", Some("sh")),
            "/bin/sh -c echo hi"
        );
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
        let root = td_fs::TempDir::new("td-util-ps").unwrap();
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
        assert_eq!(
            rows.len(),
            2,
            "a non-UTF-8 comm must not hide a process from ps"
        );
        // `position` + index rather than the search combinator, whose name the
        // ladder guard bans in embedded step content.
        let odd = &rows[rows.iter().position(|p| p.pid == 2).unwrap()];
        assert!(
            !odd.cmd.starts_with('['),
            "a userspace process with a non-UTF-8 cmdline was rendered as a kernel thread: {:?}",
            odd.cmd
        );
        assert!(
            odd.cmd.contains("-v"),
            "cmdline arguments were lost: {:?}",
            odd.cmd
        );
    }

    /// The live /proc must always contain PID 1, and scanning it must not error.
    #[test]
    fn collect_reads_the_live_process_table() {
        let rows = collect("/proc").unwrap();
        assert!(
            rows.iter().any(|p| p.pid == 1),
            "PID 1 missing from the scan"
        );
        // Every row must be fully populated -- a blank command or a degraded state
        // means the stat parse silently fell back on a real kernel's /proc.
        for p in &rows {
            assert!(!p.cmd.is_empty(), "pid {} has an empty CMD", p.pid);
            assert!(
                p.state.is_ascii_alphabetic(),
                "pid {} has state {:?}",
                p.pid,
                p.state
            );
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
