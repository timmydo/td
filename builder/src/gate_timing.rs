//! gate_timing.rs — reduce one check run's per-gate START/END events (logged
//! natively by the gate runner, `td-builder gate-run`) into a per-gate
//! wall-clock table, longest first. Makes latency regressions visible; the
//! runner reads the reduced table back (latest.txt, `gates.rs::duration_table`)
//! to start heavy gates longest-first, so the LPT order is data-driven rather
//! than hand-renumbered.
//!
//! The typed port of tools/gate-timing-report.sh (#318 axis 2 — the loop's
//! prelude/report helpers leave shell): same event format (`<gate>\tSTART|END\t<ns>`,
//! integer nanoseconds), same reduction (first START, last END per gate), same
//! table shape — `duration_table` parses the output it always parsed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// `run-*.log` files under `dir`, newest (mtime) first.
fn run_logs_newest_first(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut logs: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy().into_owned();
        if !(name.starts_with("run-") && name.ends_with(".log")) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        let mtime = md.modified().unwrap_or(std::time::UNIX_EPOCH);
        logs.push((mtime, md.len(), e.path()));
    }
    logs.sort();
    logs.reverse();
    logs.into_iter().map(|(_, _, p)| p).collect()
}

/// One reduced row: a gate's wall span over the run (first START → last END).
struct Row {
    dur_ns: u128,
    heavy: bool,
    gate: String,
}

/// Reduce the raw event log (`<gate>\tSTART|END\t<ns>` lines) to one row per
/// gate that has both a START and an END: span = last END − first START.
fn reduce(text: &str, heavy: &[String]) -> Vec<Row> {
    let mut span: HashMap<&str, (Option<u128>, Option<u128>)> = HashMap::new();
    for line in text.lines() {
        let mut it = line.split('\t');
        let (Some(gate), Some(ev), Some(ts)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if gate.is_empty() {
            continue;
        }
        let Ok(ts) = ts.trim().parse::<u128>() else { continue };
        let e = span.entry(gate).or_insert((None, None));
        match ev {
            "START" => e.0 = Some(e.0.map_or(ts, |old| old.min(ts))),
            "END" => e.1 = Some(e.1.map_or(ts, |old| old.max(ts))),
            _ => {}
        }
    }
    let mut rows: Vec<Row> = span
        .into_iter()
        .filter_map(|(gate, (s, e))| {
            let (s, e) = (s?, e?);
            Some(Row {
                dur_ns: e.saturating_sub(s),
                heavy: heavy.iter().any(|h| h == gate),
                gate: gate.to_string(),
            })
        })
        .collect();
    // Longest first; name breaks ties so the table is deterministic.
    rows.sort_by(|a, b| b.dur_ns.cmp(&a.dur_ns).then_with(|| a.gate.cmp(&b.gate)));
    rows
}

/// Nanoseconds → `S.mmm` (the table's SECONDS column).
fn fmt_secs(ns: u128) -> String {
    format!("{}.{:03}", ns / 1_000_000_000, (ns % 1_000_000_000) / 1_000_000)
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ` (pure std; civil-from-days,
/// Howard Hinnant's algorithm — no chrono in the dependency-free engine).
fn utc_iso(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Render the report text for one run log. Format-stable: `duration_table`
/// (gates.rs) whitespace-splits each row into `name kind seconds` and skips
/// `#` comments and the `GATE` header.
fn render(rows: &[Row], log_name: &str, when_secs: u64) -> String {
    let mut s = String::new();
    s.push_str(&format!("# td gate wall-clock — {} — {}\n", utc_iso(when_secs), log_name));
    s.push_str("# per-gate wall span (gates run in parallel; the sum is NOT the wall time).\n");
    s.push_str("# heavy rows drive the heavy-gate LPT start order (gate-run reads this table back).\n");
    s.push_str(&format!("{:<34} {:<6} {:>10}\n", "GATE", "KIND", "SECONDS"));
    let mut sum_heavy: u128 = 0;
    for r in rows {
        let kind = if r.heavy { "heavy" } else { "cheap" };
        if r.heavy {
            sum_heavy += r.dur_ns;
        }
        s.push_str(&format!("{:<34} {:<6} {:>10}\n", r.gate, kind, fmt_secs(r.dur_ns)));
    }
    s.push_str(&format!(
        "# heavy work total (sum across heavy gates, not wall): {}s\n",
        fmt_secs(sum_heavy)
    ));
    s
}

/// Reduce the newest non-empty run log under `<root>/.td-build-cache/gate-timing`
/// into the per-gate table: print it, write it to `latest.txt` (the table
/// `duration_table` reads back), and prune the dir to the newest 10 run logs.
/// Best-effort throughout — a report hiccup must never red a green run.
pub fn report(root: &Path, heavy_gates: &[String]) {
    let dir = root.join(".td-build-cache/gate-timing");
    let out = dir.join("latest.txt");
    let logs = run_logs_newest_first(&dir);
    let Some((log, text)) = logs.iter().find_map(|p| {
        let text = std::fs::read_to_string(p).ok()?;
        if text.is_empty() {
            return None;
        }
        Some((p.clone(), text))
    }) else {
        println!("gate-timing: no run log in {} yet (nothing to report)", dir.display());
        return;
    };
    let rows = reduce(&text, heavy_gates);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let log_name = log.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let report = render(&rows, &log_name, now);
    print!("{report}");
    let _ = std::fs::create_dir_all(&dir);
    if std::fs::write(&out, &report).is_ok() {
        println!("gate-timing: report written to {}", out.display());
    }
    // Keep the timing dir bounded — newest 10 run logs.
    for old in logs.iter().skip(10) {
        let _ = std::fs::remove_file(old);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(gate: &str, kind: &str, ns: u128) -> String {
        format!("{gate}\t{kind}\t{ns}")
    }

    #[test]
    fn reduce_takes_first_start_and_last_end_per_gate() {
        // b's events arrive interleaved and out of order; a retried gate logs
        // two START/END pairs — the span is first START → last END.
        let log = [
            ev("a", "START", 1_000_000_000),
            ev("b", "START", 2_000_000_000),
            ev("a", "END", 3_500_000_000),
            ev("a", "START", 3_000_000_000), // later duplicate START: ignored (min wins)
            ev("b", "END", 2_100_000_000),
            ev("b", "END", 9_000_000_000), // last END wins
        ]
        .join("\n");
        let rows = reduce(&log, &["b".to_string()]);
        let got: Vec<(String, u128, bool)> =
            rows.iter().map(|r| (r.gate.clone(), r.dur_ns, r.heavy)).collect();
        // longest first: b spans 7s, a spans 2.5s
        assert_eq!(
            got,
            vec![
                ("b".to_string(), 7_000_000_000, true),
                ("a".to_string(), 2_500_000_000, false),
            ]
        );
    }

    #[test]
    fn reduce_drops_incomplete_and_malformed_rows() {
        let log = [
            ev("only-start", "START", 1),
            ev("only-end", "END", 2),
            "malformed line with no tabs".to_string(),
            "\tSTART\t3".to_string(), // empty gate name
            ev("ok", "START", 10),
            ev("ok", "END", 30),
        ]
        .join("\n");
        let rows = reduce(&log, &[]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows.first().map(|r| r.gate.as_str()), Some("ok"));
        assert_eq!(rows.first().map(|r| r.dur_ns), Some(20));
    }

    #[test]
    fn render_round_trips_through_the_runner_parser() {
        // The table this writes is the one gates.rs::duration_table reads back
        // for the LPT order — prove the round trip on this exact bytes.
        let rows = vec![
            Row { dur_ns: 83_412_000_000, heavy: true, gate: "build-recipes".to_string() },
            Row { dur_ns: 1_002_000_000, heavy: false, gate: "eval".to_string() },
        ];
        let text = render(&rows, "run-1.log", 0);
        let table = crate::gates::parse_duration_table(&text);
        assert_eq!(table.get("build-recipes").copied(), Some(83.412));
        assert_eq!(table.get("eval").copied(), Some(1.002));
        assert_eq!(table.len(), 2);
        // heavy sum footer states the heavy total only
        assert!(text.contains("# heavy work total (sum across heavy gates, not wall): 83.412s"));
    }

    #[test]
    fn utc_iso_matches_known_dates() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00Z");
        // date -ud @1751500800 → 2025-07-03T00:00:00Z
        assert_eq!(utc_iso(1_751_500_800), "2025-07-03T00:00:00Z");
        // leap-year boundary: date -ud @951782400 → 2000-02-29T00:00:00Z
        assert_eq!(utc_iso(951_782_400), "2000-02-29T00:00:00Z");
    }
}
