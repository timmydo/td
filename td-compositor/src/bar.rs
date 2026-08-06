use crate::ui;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tall enough for one line of 2x glyphs with a little air. The tiling area
/// is the output minus this, so the number is layout, not decoration.
pub const BAR_HEIGHT: usize = 24;
const TEXT_TOP: usize = 5;
const TEXT_LEFT: usize = 8;
const SCALE: usize = 2;
const SEPARATOR: &str = "  ";
/// The clock shows seconds, so this is what makes it tick. The renderer
/// writes only the rows that changed, so a second's repaint is the bar's own
/// rows rather than the screen.
const TICK: Duration = Duration::from_secs(1);

/// Everything the bar shows, sampled together so one line cannot mix two
/// moments. Missing readings stay `None` rather than defaulting to zero: a
/// load of 0.00 and a load nobody could read are different claims.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Readings {
    pub load_centi: Option<u64>,
    pub used_kb: Option<u64>,
    pub total_kb: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub epoch_secs: Option<u64>,
}

impl Readings {
    /// Read from a `/proc` root — a parameter so the tests can hand it a
    /// fixture, since the host's own `/proc` is neither fixed nor reproducible.
    pub fn sample(proc_root: &Path, epoch_secs: Option<u64>) -> Self {
        let loadavg = std::fs::read_to_string(proc_root.join("loadavg")).ok();
        let meminfo = std::fs::read_to_string(proc_root.join("meminfo")).ok();
        let uptime = std::fs::read_to_string(proc_root.join("uptime")).ok();
        let total_kb = meminfo.as_deref().and_then(|text| meminfo_kb(text, "MemTotal"));
        let available_kb = meminfo
            .as_deref()
            .and_then(|text| meminfo_kb(text, "MemAvailable"));
        Self {
            load_centi: loadavg.as_deref().and_then(parse_load_centi),
            // Used is what a person means by "how much is gone", which is
            // total minus AVAILABLE — MemFree counts neither cache nor
            // reclaimable slab and reads alarmingly low on an idle machine.
            used_kb: match (total_kb, available_kb) {
                (Some(total), Some(available)) => Some(total.saturating_sub(available)),
                _ => None,
            },
            total_kb,
            uptime_secs: uptime.as_deref().and_then(parse_uptime_secs),
            epoch_secs,
        }
    }
}

/// `/proc/loadavg`'s first field, as hundredths. Parsed as two integers
/// rather than a float because the shape is fixed and `f64` would make
/// rounding a question nobody needs to answer.
fn parse_load_centi(text: &str) -> Option<u64> {
    let field = text.split_ascii_whitespace().next()?;
    let (whole, fraction) = match field.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (field, "0"),
    };
    let whole: u64 = whole.parse().ok()?;
    // Two digits, PADDED rather than required: the kernel writes `%lu.%02lu`
    // so a shorter one cannot appear here, but reading `1.5` as `1.00` is the
    // wrong answer rather than a refusal, and this is the arithmetic that
    // would carry it.
    let digits = fraction.get(..2).unwrap_or(fraction);
    let hundredths: u64 = match digits.len() {
        0 => 0,
        1 => digits.parse::<u64>().ok()?.checked_mul(10)?,
        _ => digits.parse().ok()?,
    };
    whole.checked_mul(100)?.checked_add(hundredths)
}

/// A named `/proc/meminfo` line's kB value. A line without a colon is
/// SKIPPED rather than ending the search: one malformed line must not hide
/// every field below it.
fn meminfo_kb(text: &str, name: &str) -> Option<u64> {
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if key != name {
            continue;
        }
        return rest.split_ascii_whitespace().next()?.parse().ok();
    }
    None
}

/// `/proc/uptime`'s first field, whole seconds.
fn parse_uptime_secs(text: &str) -> Option<u64> {
    let field = text.split_ascii_whitespace().next()?;
    let whole = field.split_once('.').map_or(field, |(whole, _)| whole);
    whole.parse().ok()
}

/// Round to one decimal in the largest unit that keeps the number small.
/// Integer arithmetic throughout: a bar is not worth a float.
fn human_bytes(kb: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1024 * 1024 * 1024, "T"),
        (1024 * 1024, "G"),
        (1024, "M"),
    ];
    for (divisor, suffix) in UNITS {
        if kb >= *divisor {
            // The multiply is done in `u128`. `kb.saturating_mul(10)` clamps
            // above `u64::MAX / 10` and the clamp happens BEFORE the divide,
            // so it does not bound the answer — it divides it by ten, and
            // reports a tenth of the truth as though it were a number.
            let tenths = u64::try_from(u128::from(kb).saturating_mul(10) / u128::from(*divisor))
                .unwrap_or(u64::MAX);
            return format!("{}.{}{suffix}", tenths / 10, tenths % 10);
        }
    }
    format!("{kb}K")
}

/// Days, hours and minutes — seconds would churn the line for nothing, and
/// nobody reads an uptime to that resolution.
fn human_uptime(secs: u64) -> String {
    let minutes = secs / 60;
    let (days, rest) = (minutes / (60 * 24), minutes % (60 * 24));
    let (hours, minutes) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}D {hours:02}:{minutes:02}")
    } else {
        format!("{hours:02}:{minutes:02}")
    }
}

/// UTC, spelled out. There is no TZif parser here, so naming the zone is the
/// honest thing: a local-looking time that is silently UTC is worse than a
/// UTC one that says so.
fn utc_stamp(epoch_secs: u64) -> String {
    let days = epoch_secs / 86_400;
    let seconds = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days since 1970-01-01 to a civil date, by Howard Hinnant's shift-the-era
/// method: March-based years put the leap day last, so the month-length
/// pattern repeats with no table and no branch on February.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days.saturating_add(719_468);
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The line, left to right. A reading that could not be taken shows its
/// label with `?` rather than vanishing, so a bar with a broken source looks
/// broken instead of looking like a machine with less to report.
pub fn line(readings: &Readings) -> String {
    let load = readings.load_centi.map_or_else(
        || "LOAD ?".to_string(),
        |centi| format!("LOAD {}.{:02}", centi / 100, centi % 100),
    );
    let memory = match (readings.used_kb, readings.total_kb) {
        (Some(used), Some(total)) => {
            format!("MEM {}/{}", human_bytes(used), human_bytes(total))
        }
        _ => "MEM ?".to_string(),
    };
    let uptime = readings
        .uptime_secs
        .map_or_else(|| "UP ?".to_string(), |secs| format!("UP {}", human_uptime(secs)));
    let clock = readings
        .epoch_secs
        .map_or_else(|| "CLOCK ?".to_string(), utc_stamp);
    [load, memory, uptime, clock].join(SEPARATOR)
}

/// Paint the strip across the top. The caller owns the text, so this never
/// reads a clock or a file — the same split the launcher and the sheet have.
pub fn paint(frame: &mut [u8], width: usize, height: usize, stride: usize, text: &str) {
    // Not clamped to `height`: both primitives clip against it, and a second
    // clamp here would read as though they did not.
    let bar = (0, 0, width, BAR_HEIGHT);
    ui::fill(frame, width, height, stride, bar, [0x18, 0x14, 0x20, 0]);
    ui::draw_text_clipped(
        frame,
        width,
        height,
        stride,
        TEXT_LEFT,
        TEXT_TOP,
        SCALE,
        text,
        [0xd0, 0xc8, 0xe0, 0],
        bar,
    );
}

/// At most this many failures are named between good paints. Deduplication
/// alone bounds the REPEATED fault and not the alternating one: a framebuffer
/// failing every other paint, or two faults taking turns, is a fresh error
/// every tick to a record that remembers only the last.
const MAX_REPORTS: usize = 4;

/// One line per DISTINCT failure rather than one per tick, and a hard stop
/// after `MAX_REPORTS` of them. The retry is not optional — `set_status` puts
/// the previous line back when a paint fails, so the next second's text
/// differs and the paint is attempted again — and an output broken for good
/// would otherwise write a line a second forever, burying whatever else the
/// session had to say.
#[derive(Default)]
struct Reported {
    last: Option<String>,
    since_good: usize,
}

impl Reported {
    /// What to print for this outcome, if anything. A paint that worked clears
    /// the record, so a fault that returns after the bar recovered is named
    /// again rather than swallowed for the life of the session.
    fn note(&mut self, error: Option<&str>) -> Option<String> {
        let Some(error) = error else {
            self.last = None;
            self.since_good = 0;
            return None;
        };
        if self.last.as_deref() == Some(error) {
            return None;
        }
        self.last = Some(error.to_string());
        self.since_good = self.since_good.saturating_add(1);
        if self.since_good < MAX_REPORTS {
            Some(error.to_string())
        } else if self.since_good == MAX_REPORTS {
            Some(format!("{error} (silent until a paint succeeds)"))
        } else {
            None
        }
    }
}

/// What one sample decided. `Poisoned` means another thread panicked holding
/// the runtime: nothing this one does can help, and looping on it would spin a
/// core printing.
#[derive(Debug, Eq, PartialEq)]
enum Tick {
    Continue(Option<String>),
    Poisoned,
}

/// One sample published. Split out of the loop so the WIRING is reachable from
/// a test — the record, the report, and releasing the lock before printing are
/// each things a refactor can quietly drop while `Reported`'s own tests stay
/// green. `epoch_secs` is a parameter for the same reason the `/proc` root is.
fn tick(
    runtime: &Mutex<crate::runtime::Runtime>,
    proc_root: &Path,
    epoch_secs: Option<u64>,
    reported: &mut Reported,
) -> Tick {
    let readings = Readings::sample(proc_root, epoch_secs);
    let line = line(&readings);
    // The guard is released BEFORE anything is printed. `eprintln!` takes
    // stderr's own lock and blocks on a slow console, and holding the runtime
    // across that would stop the compositor over the least important thing on
    // the screen — the opposite of what the reporting is for.
    let outcome = match runtime.lock() {
        Ok(mut runtime) => runtime.set_status(line),
        Err(_) => return Tick::Poisoned,
    };
    Tick::Continue(reported.note(outcome.err().as_deref()))
}

/// Sample and publish on a cadence. A compositor with no timer repaints only
/// on input, so a clock would sit at the moment the machine booted; this is
/// the only thing in the process that wakes without one.
pub fn start(
    runtime: Arc<Mutex<crate::runtime::Runtime>>,
    proc_root: PathBuf,
) -> Result<(), String> {
    // `thread::Builder`, not `thread::spawn`: the latter PANICS when the OS
    // refuses a thread, which would take the session down over the least
    // important thing on the screen.
    thread::Builder::new()
        .name("td-status-bar".to_string())
        .spawn(move || {
            let mut reported = Reported::default();
            loop {
                // A paint failure is REPORTED, never fatal: the bar is the
                // least important thing on the screen and must not take the
                // session down with it.
                match tick(&runtime, &proc_root, unix_epoch_secs(), &mut reported) {
                    Tick::Continue(report) => {
                        if let Some(report) = report {
                            eprintln!("td-compositor: status bar: {report}");
                        }
                    }
                    Tick::Poisoned => {
                        eprintln!("td-compositor: status bar: runtime poisoned, clock stopped");
                        return;
                    }
                }
                thread::sleep(TICK);
            }
        })
        .map(|_| ())
        .map_err(|error| format!("spawn status bar thread: {error}"))
}

/// Wall clock as UNIX seconds, or `None` before the epoch — which a machine
/// with no RTC and no network can genuinely report.
fn unix_epoch_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Fixture(std::path::PathBuf);

    impl Fixture {
        fn new(loadavg: &str, meminfo: &str, uptime: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "td-bar-proc-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("loadavg"), loadavg).unwrap();
            std::fs::write(root.join("meminfo"), meminfo).unwrap();
            std::fs::write(root.join("uptime"), uptime).unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const MEMINFO: &str = "MemTotal:        8039384 kB\n\
                           MemFree:          204512 kB\n\
                           MemAvailable:    5242880 kB\n\
                           Buffers:          131072 kB\n";

    #[test]
    fn a_sample_reads_every_field_from_its_proc_root() {
        let fixture = Fixture::new("0.42 0.31 0.28 2/517 9182\n", MEMINFO, "187245.31 91.2\n");
        let readings = Readings::sample(&fixture.0, Some(1_770_000_000));
        assert_eq!(readings.load_centi, Some(42));
        assert_eq!(readings.total_kb, Some(8_039_384));
        // Used is total minus AVAILABLE, not minus free.
        assert_eq!(readings.used_kb, Some(8_039_384 - 5_242_880));
        assert_eq!(readings.uptime_secs, Some(187_245));
        assert_eq!(
            line(&readings),
            "LOAD 0.42  MEM 2.6G/7.6G  UP 2D 04:00  2026-02-02 02:40:00 UTC"
        );
    }

    #[test]
    fn a_missing_or_malformed_source_shows_a_question_mark_not_a_zero() {
        let empty = std::env::temp_dir().join(format!(
            "td-bar-absent-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let readings = Readings::sample(&empty, None);
        assert_eq!(readings, Readings::default());
        assert_eq!(line(&readings), "LOAD ?  MEM ?  UP ?  CLOCK ?");

        // Present but unparseable is the same answer, and each field fails on
        // its own: a garbled loadavg must not take the clock down with it.
        let fixture = Fixture::new("not-a-number\n", "MemTotal: elephants\n", "\n");
        let readings = Readings::sample(&fixture.0, Some(0));
        assert_eq!(readings.load_centi, None);
        assert_eq!(readings.total_kb, None);
        assert_eq!(readings.used_kb, None);
        assert_eq!(readings.uptime_secs, None);
        assert_eq!(
            line(&readings),
            "LOAD ?  MEM ?  UP ?  1970-01-01 00:00:00 UTC"
        );
    }

    #[test]
    fn memory_needs_both_halves_before_it_reports_either() {
        // MemAvailable absent: "used" is unanswerable, so the whole segment
        // is, rather than reporting a total beside a made-up used.
        let fixture = Fixture::new("0.00 0 0 1/1 1\n", "MemTotal:        1048576 kB\n", "0\n");
        let readings = Readings::sample(&fixture.0, Some(0));
        assert_eq!(readings.total_kb, Some(1_048_576));
        assert_eq!(readings.used_kb, None);
        assert!(line(&readings).contains("MEM ?"));
    }

    #[test]
    fn load_parses_the_first_field_at_two_decimals() {
        assert_eq!(parse_load_centi("0.42 0.31 0.28 2/517 9182"), Some(42));
        assert_eq!(parse_load_centi("12.07 0.31 0.28"), Some(1207));
        // Three decimals is not this file's format, but truncating beats
        // refusing: the extra digit changes nothing a bar shows.
        assert_eq!(parse_load_centi("1.234"), Some(123));
        // One decimal is not this file's format either, and padding it is
        // the right answer where truncating to `1.00` is a wrong one.
        assert_eq!(parse_load_centi("1.5"), Some(150));
        assert_eq!(parse_load_centi("2."), Some(200));
        assert_eq!(parse_load_centi("7 1 1"), Some(700));
        assert_eq!(parse_load_centi(""), None);
        assert_eq!(parse_load_centi("x.yz"), None);
    }

    #[test]
    fn meminfo_matches_a_whole_key_and_not_a_prefix() {
        // `MemTotal` must not be answered by `MemTotalSwap`, and the search
        // must reach a key that is not the first line.
        let text = "MemTotalSwap:   1 kB\nMemFree:  2 kB\nMemTotal:   4096 kB\n";
        assert_eq!(meminfo_kb(text, "MemTotal"), Some(4096));
        assert_eq!(meminfo_kb(text, "MemFree"), Some(2));
        assert_eq!(meminfo_kb(text, "MemAvailable"), None);
        // A line with no colon must not hide the fields below it.
        let ragged = "MemTotal:  4096 kB\nnonsense\nMemAvailable:  2048 kB\n";
        assert_eq!(meminfo_kb(ragged, "MemAvailable"), Some(2048));
    }

    #[test]
    fn bytes_and_uptime_render_in_the_units_a_person_reads() {
        assert_eq!(human_bytes(0), "0K");
        assert_eq!(human_bytes(512), "512K");
        assert_eq!(human_bytes(1024), "1.0M");
        assert_eq!(human_bytes(1536), "1.5M");
        assert_eq!(human_bytes(8_039_384), "7.6G");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0T");
        // Recomputed independently rather than read off the implementation:
        // u64::MAX kB IS 17179869183.9T, and a `u64` tenths multiply used to
        // print a tenth of that with no sign that anything had clamped.
        assert_eq!(human_bytes(u64::MAX), "17179869183.9T");

        assert_eq!(human_uptime(0), "00:00");
        assert_eq!(human_uptime(59), "00:00");
        assert_eq!(human_uptime(60), "00:01");
        assert_eq!(human_uptime(3600), "01:00");
        assert_eq!(human_uptime(86_400), "1D 00:00");
        assert_eq!(human_uptime(187_245), "2D 04:00");
    }

    #[test]
    fn the_civil_calendar_holds_across_leap_years_and_centuries() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(86_399), "1970-01-01 23:59:59 UTC");
        assert_eq!(utc_stamp(86_400), "1970-01-02 00:00:00 UTC");
        // 1972 is a leap year and 2000 is (the 400 rule); 2100 will not be
        // (the 100 rule), which is the one a naive every-fourth-year gets
        // wrong. 1900 is the other side of that rule and cannot be asked
        // here — it is before the epoch, and these are unsigned days since
        // it.
        assert_eq!(utc_stamp(68_255_999), "1972-02-29 23:59:59 UTC");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_stamp(4_107_542_400), "2100-03-01 00:00:00 UTC");
        assert_eq!(utc_stamp(4_107_456_000), "2100-02-28 00:00:00 UTC");
        assert_eq!(utc_stamp(1_770_000_000), "2026-02-02 02:40:00 UTC");
        // Every day of a leap year and the year after it, walked in order.
        // The round trip alone would pass against a matching pair of wrong
        // functions, so each step is also required to ADVANCE the calendar
        // by exactly one day — which only `civil_from_days` can be wrong
        // about.
        for (year, length) in [(2024u64, 366u64), (2025, 365)] {
            let start = days_from_civil(year, 1, 1);
            let mut previous = civil_from_days(start);
            assert_eq!(previous, (year, 1, 1));
            for offset in 1..length {
                let today = civil_from_days(start + offset);
                assert_eq!(days_from_civil(today.0, today.1, today.2), start + offset);
                assert_eq!(next_day(previous), today, "after {previous:?}");
                previous = today;
            }
            assert_eq!(next_day(previous), (year + 1, 1, 1), "after {previous:?}");
        }
    }

    /// The next date, written out as month lengths and the leap rule rather
    /// than as arithmetic on a day count — so a walk through the year checks
    /// `civil_from_days` against a different method rather than against its
    /// own inverse, which a matching pair of wrong functions would satisfy.
    fn next_day((year, month, day): (u64, u64, u64)) -> (u64, u64, u64) {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let length = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            _ => 28,
        };
        if day < length {
            (year, month, day + 1)
        } else if month < 12 {
            (year, month + 1, 1)
        } else {
            (year + 1, 1, 1)
        }
    }

    #[test]
    fn a_repeated_failure_is_reported_once_and_a_returning_one_again() {
        let mut reported = Reported::default();
        assert_eq!(reported.note(Some("no framebuffer")).as_deref(), Some("no framebuffer"));
        assert_eq!(reported.note(Some("no framebuffer")), None);
        assert_eq!(reported.note(Some("no framebuffer")), None);
        // A different fault is news even while the first is unresolved.
        assert_eq!(reported.note(Some("short write")).as_deref(), Some("short write"));
        assert_eq!(reported.note(Some("short write")), None);
        assert_eq!(reported.note(None), None);
        assert_eq!(reported.note(Some("short write")).as_deref(), Some("short write"));
    }

    #[test]
    fn alternating_failures_go_quiet_rather_than_reporting_forever() {
        // Deduplication alone does not bound this: each of these is a fresh
        // error to a record that remembers only the last, so without the cap
        // it is a line per tick for as long as the fault flaps.
        let mut reported = Reported::default();
        let mut lines = 0usize;
        for turn in 0..50 {
            let error = if turn % 2 == 0 { "odd write" } else { "even write" };
            if reported.note(Some(error)).is_some() {
                lines = lines.saturating_add(1);
            }
        }
        assert_eq!(lines, MAX_REPORTS, "{lines} lines for one flapping fault");
        // And a good paint re-arms it, so a fault that flaps again later is
        // not silent for the life of the session.
        assert_eq!(reported.note(None), None);
        assert!(reported.note(Some("odd write")).is_some());
    }

    #[test]
    fn the_sampler_reports_a_failure_once_and_names_it_again_after_a_good_paint() {
        // The WIRING, not the record: deleting the reset or the dedupe inside
        // `tick` leaves `Reported`'s own tests green, because nothing else
        // calls it.
        let path = std::env::temp_dir().join(format!(
            "td-bar-tick-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&path, 320, 200, 320 * 4).unwrap();
        let runtime = Mutex::new(crate::runtime::Runtime::new(framebuffer));
        let fixture = Fixture::new("0.42 0.31 0.28 2/517 9182\n", MEMINFO, "187245.31 91.2\n");
        let mut reported = Reported::default();
        // Each tick gets its own second, or the line would not change and no
        // paint would be attempted at all.
        let mut at = 1_770_000_000u64;
        let mut next = |reported: &mut Reported| {
            at = at.saturating_add(1);
            tick(&runtime, &fixture.0, Some(at), reported)
        };

        assert_eq!(next(&mut reported), Tick::Continue(None), "a good paint says nothing");

        runtime.lock().unwrap().fail_next_repaint();
        let Tick::Continue(Some(first)) = next(&mut reported) else {
            panic!("the first failure was not reported");
        };
        assert!(first.contains("injected framebuffer paint failure"));

        // Same fault again: reported once, not once a second.
        runtime.lock().unwrap().fail_next_repaint();
        assert_eq!(next(&mut reported), Tick::Continue(None));

        // It paints, and then fails the same way again — which is news.
        assert_eq!(next(&mut reported), Tick::Continue(None));
        runtime.lock().unwrap().fail_next_repaint();
        assert!(matches!(next(&mut reported), Tick::Continue(Some(_))));

        let _ = std::fs::remove_file(&path);
    }

    /// The inverse, for the round-trip above only.
    fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = year / 400;
        let year_of_era = year - era * 400;
        let month_prime = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
        let day_of_era =
            year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    #[test]
    fn every_character_the_line_can_spell_is_in_the_font() {
        // The decimal point is the one that mattered: an unmapped byte drew a
        // glyph shaped like `?`, which is exactly what this line prints for a
        // reading it could not take — so `LOAD 0.42` and `LOAD ?` looked the
        // same. Both a full line and an all-failed one are checked, since the
        // failure marker is a character of the vocabulary too.
        let full = line(&Readings {
            load_centi: Some(1_234),
            used_kb: Some(2_800_000),
            total_kb: Some(8_039_384),
            uptime_secs: Some(187_245),
            epoch_secs: Some(1_770_000_000),
        });
        let failed = line(&Readings::default());
        for text in [full.as_str(), failed.as_str()] {
            for byte in text.bytes() {
                assert!(ui::is_mapped(byte), "{:?} in {text:?} has no glyph", byte as char);
            }
        }
        assert!(full.contains('.') && failed.contains('?'));
    }

    #[test]
    fn the_strip_paints_only_its_own_rows() {
        let (width, height) = (400usize, 200usize);
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        paint(&mut frame, width, height, stride, "LOAD 0.42");
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                let Some(pixel) = frame.get(offset..offset + 4) else {
                    continue;
                };
                if pixel.iter().any(|byte| *byte != 0) {
                    assert!(y < BAR_HEIGHT, "painted row {y} below the bar");
                }
            }
        }
        // And it fills its rows rather than leaving the desktop showing
        // between glyphs.
        assert!(frame
            .get(..stride)
            .is_some_and(|row| row.iter().any(|byte| *byte != 0)));

        // The text sits WHOLLY inside the band, which "only its own rows"
        // does not say: `draw_text_clipped`'s clip rect IS the band, so a
        // `TEXT_TOP` that cut every glyph in half would clip rather than
        // overflow and nothing above would notice.
        let rows: Vec<usize> = (0..height)
            .filter(|y| {
                (0..width).any(|x| {
                    let offset = y * stride + x * 4;
                    frame.get(offset..offset + 4) == Some(&[0xd0, 0xc8, 0xe0, 0][..])
                })
            })
            .collect();
        assert_eq!(rows.first(), Some(&TEXT_TOP), "the text does not start at TEXT_TOP");
        assert_eq!(
            rows.last(),
            Some(&(TEXT_TOP + ui::GLYPH_HEIGHT * SCALE - 1)),
            "the text is clipped inside its own band"
        );
    }

    #[test]
    fn an_output_shorter_than_the_bar_still_clips() {
        // The band is 24 rows and the output is 8, so every clip in the path
        // has to hold. The buffer carries rows the output does not, which is
        // what makes "clips" observable: a length assertion could not fail.
        let (width, height, guard) = (64usize, 8usize, 4usize);
        let stride = width * 4;
        let mut frame = vec![0u8; stride * (height + guard)];
        paint(&mut frame, width, height, stride, "LOAD 0.42  MEM 2.7G/7.7G");
        assert!(frame
            .get(stride * height..)
            .is_some_and(|rows| rows.iter().all(|byte| *byte == 0)));
        // Clipped, not SKIPPED: an output too short for the band still gets
        // every row of it, or a small screen would show the desktop where the
        // bar's rows are while the tiling area is reserved for it anyway.
        for y in 0..height {
            assert_eq!(
                frame.get(y * stride..y * stride + 4),
                Some(&[0x18, 0x14, 0x20, 0][..]),
                "row {y} is not the bar"
            );
        }
    }
}
