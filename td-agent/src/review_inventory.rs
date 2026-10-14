//! Inventory retained runs without assuming that they cover account spending.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use td_json::Json;

const USAGE: &str = "usage: td-agent review-logs [DIRECTORY] [--day YYYY-MM-DD] [--key-fingerprint SHA256] [--billed-cost USD]\nLists retained runs with model, binary, key fingerprint and reported/accounted costs.\nDays are UTC start dates. Billed cost requires --day and must cover the same key/day.\nAn unexplained difference is not proof of lost logging or a provider overcharge.\n";

const SCOPE_NOTICE: &str = concat!(
    "Local traces cover retained reviews only. Other clients, keys, directories, ",
    "deleted logs, interrupted or unreported requests and billing-period ",
    "boundaries may explain a difference. Verify key and UTC day before ",
    "interpreting billed cost; unknown fingerprints cannot establish attribution."
);

#[derive(Default)]
struct Options {
    directory: Option<PathBuf>,
    day: Option<String>,
    key: Option<String>,
    billed: Option<u64>,
}

fn day(ms: u64) -> String {
    let (y, m, d) = td_civil::tzif::civil_from_days((ms / 86_400_000) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn valid_day(text: &str) -> bool {
    let parts: Vec<_> = text.split('-').collect();
    let parsed = parts
        .first()
        .and_then(|s| s.parse::<i64>().ok())
        .zip(parts.get(1).and_then(|s| s.parse::<u8>().ok()))
        .zip(parts.get(2).and_then(|s| s.parse::<u8>().ok()));
    let Some(((y, m), d)) = parsed else {
        return false;
    };
    if text.len() != 10
        || parts.len() != 3
        || !(1970..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
    {
        return false;
    }
    let days = td_civil::tzif::days_from_civil(y, m, d);
    td_civil::tzif::civil_from_days(days as i64) == (y, m, d)
        && text == format!("{y:04}-{m:02}-{d:02}")
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut out = Options::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--day" => {
                let value = iter.next().ok_or("--day needs a UTC date")?;
                if !valid_day(value) || out.day.replace(value.clone()).is_some() {
                    return Err("invalid or duplicate --day".into());
                }
            }
            "--key-fingerprint" => {
                let value = iter.next().ok_or("--key-fingerprint needs SHA256")?;
                if value.len() != 64
                    || !value.bytes().all(|b| b.is_ascii_hexdigit())
                    || out.key.replace(value.to_ascii_lowercase()).is_some()
                {
                    return Err("invalid or duplicate key fingerprint".into());
                }
            }
            "--billed-cost" => {
                let value = iter
                    .next()
                    .and_then(|s| crate::cost::parse(s, false))
                    .ok_or("invalid billed cost")?;
                if out.billed.replace(value).is_some() {
                    return Err("duplicate billed cost".into());
                }
            }
            value if !value.starts_with('-') && out.directory.is_none() => {
                out.directory = Some(PathBuf::from(value))
            }
            _ => return Err(USAGE.into()),
        }
    }
    if out.billed.is_some() && out.day.is_none() {
        return Err(
            "--billed-cost requires --day matching the provider's UTC billing period".into(),
        );
    }
    Ok(out)
}

fn inventory(directory: &Path, options: &Options) -> Result<Json, String> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_file()
            && entry.path().extension().is_some_and(|s| s == "jsonl")
        {
            paths.push(entry.path());
        }
        if paths.len() > 4096 {
            return Err("review inventory exceeds 4096 traces; select a smaller directory".into());
        }
    }
    paths.sort();
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let mut reported = 0u64;
    let mut accounted = 0u64;
    let mut unknown = 0u64;
    let mut missing_time = 0u64;
    let mut unreported_requests = 0u64;
    let mut unknown_accounting = 0u64;
    let mut unfinished_runs = 0u64;
    let mut keys = std::collections::BTreeSet::new();
    for path in paths {
        let result = OpenOptions::new()
            .read(true)
            .custom_flags(crate::store::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| e.to_string())
            .and_then(crate::review_metrics::summarize);
        let summary = match result {
            Ok(s) => s,
            Err(e) => {
                errors.push(Json::Obj(vec![
                    ("path".into(), Json::Str(path.display().to_string())),
                    ("error".into(), Json::Str(e)),
                ]));
                continue;
            }
        };
        let date = summary.get("start_time_ms").and_then(Json::as_u64).map(day);
        if date.is_none() {
            missing_time += 1;
        }
        if options
            .day
            .as_ref()
            .is_some_and(|d| Some(d) != date.as_ref())
        {
            continue;
        }
        let key = summary.get("billing_key_sha256").and_then(Json::as_str);
        if key.is_none() {
            unknown += 1;
        }
        if options.key.as_deref().is_some_and(|k| Some(k) != key) {
            continue;
        }
        if let Some(key) = key {
            keys.insert(key.to_string());
        }
        unreported_requests = unreported_requests.saturating_add(
            summary
                .get("requests")
                .and_then(Json::as_u64)
                .unwrap_or(0)
                .saturating_sub(
                    summary
                        .get("cost_reports")
                        .and_then(Json::as_u64)
                        .unwrap_or(0),
                ),
        );
        unknown_accounting += u64::from(
            summary
                .get("accounted_cost")
                .and_then(Json::as_u64)
                .is_none(),
        );
        unfinished_runs += u64::from(summary.get("end").is_none_or(|e| matches!(e, Json::Null)));
        let cost = summary
            .get("reported_cost")
            .and_then(Json::as_u64)
            .unwrap_or(0);
        reported = reported
            .checked_add(cost)
            .ok_or("review cost total overflow")?;
        accounted = accounted
            .checked_add(
                summary
                    .get("accounted_cost")
                    .and_then(Json::as_u64)
                    .unwrap_or(0),
            )
            .ok_or("review accounted cost overflow")?;
        let start = summary.get("start");
        let mut row = vec![
            ("path".into(), Json::Str(path.display().to_string())),
            ("utc_day".into(), date.map_or(Json::Null, Json::Str)),
        ];
        for name in [
            "session_id",
            "model",
            "agent_binary_sha256",
            "repository",
            "revision",
        ] {
            row.push((
                name.into(),
                start
                    .and_then(|s| s.get(name))
                    .cloned()
                    .unwrap_or(Json::Null),
            ));
        }
        for name in [
            "billing_key_sha256",
            "requests",
            "complete",
            "partial_tail",
            "end",
            "reported_cost",
            "accounted_cost",
            "unresolved_attempts",
            "unreported_accounting",
        ] {
            row.push((
                name.into(),
                summary.get(name).cloned().unwrap_or(Json::Null),
            ));
        }
        rows.push(Json::Obj(row));
    }
    Ok(Json::Obj(vec![
        (
            "unreported_cost_requests".into(),
            Json::from(unreported_requests),
        ),
        (
            "unknown_accounting_runs".into(),
            Json::from(unknown_accounting),
        ),
        ("unfinished_runs".into(), Json::from(unfinished_runs)),
        ("runs".into(), Json::Arr(rows)),
        ("errors".into(), Json::Arr(errors)),
        ("reported_cost_pico".into(), Json::from(reported)),
        ("accounted_cost_pico".into(), Json::from(accounted)),
        ("unknown_key_runs".into(), Json::from(unknown)),
        ("missing_timestamp_runs".into(), Json::from(missing_time)),
        (
            "known_keys".into(),
            Json::Arr(keys.into_iter().map(Json::Str).collect()),
        ),
        (
            "billed_cost_pico".into(),
            options.billed.map_or(Json::Null, Json::from),
        ),
        (
            "unexplained_billed_cost_pico".into(),
            options
                .billed
                .map_or(Json::Null, |b| Json::from(b.saturating_sub(reported))),
        ),
        (
            "local_cost_above_billed_pico".into(),
            options
                .billed
                .map_or(Json::Null, |b| Json::from(reported.saturating_sub(b))),
        ),
        ("scope_notice".into(), Json::Str(SCOPE_NOTICE.into())),
    ]))
}

pub fn run(args: &[String]) -> Result<(), String> {
    if matches!(args,[arg] if arg=="--help" || arg=="-h") {
        return std::io::stdout()
            .lock()
            .write_all(USAGE.as_bytes())
            .map_err(|e| e.to_string());
    }
    let options = options(args)?;
    let directory = match &options.directory {
        Some(p) => p.clone(),
        None => crate::store::StateDir::from_env(
            std::env::var_os("XDG_STATE_HOME"),
            std::env::var_os("HOME"),
        )?
        .root()
        .join("reviews"),
    };
    let result = inventory(&directory, &options)?;
    writeln!(std::io::stdout().lock(), "{result}").map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn utc_dates_and_billing_scope_are_explicit() {
        assert!(valid_day("2024-02-29"));
        assert!(!valid_day("2025-02-29"));
        assert!(!valid_day("2025-13-01"));
        assert!(options(&["--billed-cost".into(), "1".into()]).is_err());
        assert_eq!(day(0), "1970-01-01");
    }
    #[test]
    fn inventory_keeps_failed_and_interrupted_runs_and_unknown_keys() {
        let root = td_fs::TempDir::new("review-inventory").unwrap();
        let trace = |id: &str, cost: u64, end: bool| {
            let mut events = vec![
                (
                    "start",
                    Json::Obj(vec![
                        ("version".into(), Json::from(1u64)),
                        ("session_id".into(), Json::Str(id.into())),
                    ]),
                ),
                (
                    "completion",
                    Json::Obj(vec![(
                        "usage".into(),
                        Json::Obj(vec![("cost".into(), Json::from(cost))]),
                    )]),
                ),
            ];
            if end {
                events.push((
                    "end",
                    Json::Obj(vec![("success".into(), Json::Bool(false))]),
                ));
            }
            events
                .into_iter()
                .enumerate()
                .map(|(i, (kind, data))| {
                    Json::Obj(vec![
                        ("sequence".into(), Json::from(i as u64)),
                        ("time_ms".into(), Json::from(0u64)),
                        ("kind".into(), Json::Str(kind.into())),
                        ("data".into(), data),
                    ])
                    .to_string()
                        + "\n"
                })
                .collect::<String>()
        };
        fs::write(root.path().join("a.jsonl"), trace("a", 7, true)).unwrap();
        fs::write(root.path().join("b.jsonl"), trace("b", 11, false)).unwrap();
        let options = Options {
            day: Some("1970-01-01".into()),
            billed: Some(25),
            ..Options::default()
        };
        let result = inventory(root.path(), &options).unwrap();
        assert_eq!(
            result.get("reported_cost_pico").and_then(Json::as_u64),
            Some(18)
        );
        assert_eq!(
            result
                .get("unexplained_billed_cost_pico")
                .and_then(Json::as_u64),
            Some(7)
        );
        assert_eq!(
            result.get("unknown_key_runs").and_then(Json::as_u64),
            Some(2)
        );
        assert_eq!(result.get("runs").and_then(Json::as_arr).unwrap().len(), 2);
    }
}
