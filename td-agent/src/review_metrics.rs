//! Read-only summaries of retained review traces, including incomplete sessions.

use std::io::{BufRead, BufReader, Read};
use td_json::Json;

const LINE_LIMIT: u64 = 64 * 1024 * 1024;

pub fn summarize(input: impl Read) -> Result<Json, String> {
    let mut input = BufReader::new(input);
    let mut sequence = 0u64;
    let mut requests = 0u64;
    let mut tools = 0u64;
    let mut repeated = 0u64;
    let mut shortened = 0u64;
    let mut raw = 0u64;
    let mut visible = 0u64;
    let mut retained = 0u64;
    let mut reported = 0u64;
    let mut cost_reports = 0u64;
    let mut accounted = None;
    let mut reservation = None;
    let mut peak = None;
    let mut tool_metrics = 0u64;
    let mut token_reports = [0u64; 5];
    let mut error_count = 0u64;
    let mut duration_ms = 0u64;
    let mut preflight_failures = 0u64;
    let mut nonzero_exits = 0u64;
    let mut failed_tools = 0u64;
    let mut unsuccessful = 0u64;
    let mut tokens = [0u64; 5];
    let mut ended = None;
    let mut cleanup = None;
    let mut errors = Vec::new();
    let mut partial = false;
    let mut last_elapsed = 0u64;
    loop {
        let mut line = Vec::new();
        let count = Read::by_ref(&mut input)
            .take(LINE_LIMIT + 1)
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        if count as u64 > LINE_LIMIT {
            return Err("review trace line exceeds 64 MiB".into());
        }
        if !line.ends_with(b"\n") {
            partial = true;
            break;
        }
        let event = td_json::parse_slice(&line)
            .map_err(|e| format!("review trace record {sequence}: {e}"))?;
        if event.get("sequence").and_then(Json::as_u64) != Some(sequence) {
            return Err("review trace sequence is not contiguous".into());
        }
        sequence = sequence.saturating_add(1);
        last_elapsed = event
            .get("elapsed_ms")
            .and_then(Json::as_u64)
            .unwrap_or(last_elapsed);
        let kind = event
            .get("kind")
            .and_then(Json::as_str)
            .ok_or("review trace has no kind")?;
        let data = event.get("data").ok_or("review trace has no data")?;
        let count = |key| data.get(key).and_then(Json::as_u64).unwrap_or(0);
        match kind {
            "start" if data.get("version").and_then(Json::as_u64) != Some(1) => {
                return Err("unsupported review trace version".into())
            }
            "budget_reservation" => {
                reservation = data
                    .get("charged")
                    .and_then(Json::as_u64)
                    .zip(data.get("reserved").and_then(Json::as_u64));
            }
            "request_body" => {
                requests = requests.saturating_add(1);
                if let Some((charged, reserved)) = reservation.take() {
                    accounted = Some(charged.saturating_add(reserved));
                }
            }
            "request_metrics" => peak = Some(peak.unwrap_or(0u64).max(count("context_bytes"))),
            "request_duration_ms" => {
                duration_ms = duration_ms.saturating_add(data.as_u64().unwrap_or(0))
            }
            "test_preflight" => {
                preflight_failures = preflight_failures.saturating_add(u64::from(
                    data.get("available").and_then(Json::as_bool) == Some(false),
                ))
            }
            "tool_call" => tools = tools.saturating_add(1),
            "tool_metrics" => {
                tool_metrics = tool_metrics.saturating_add(1);
                unsuccessful = unsuccessful.saturating_add(u64::from(
                    data.get("unsuccessful_process").and_then(Json::as_bool) == Some(true),
                ));
                failed_tools = failed_tools.saturating_add(u64::from(
                    data.get("failed").and_then(Json::as_bool) == Some(true),
                ));
                nonzero_exits = nonzero_exits.saturating_add(u64::from(
                    data.get("exit_status")
                        .and_then(Json::as_i64)
                        .is_some_and(|n| n != 0),
                ));
                repeated = repeated.saturating_add(u64::from(count("repetitions") > 1));
                shortened = shortened.saturating_add(u64::from(
                    data.get("shortened").and_then(Json::as_bool) == Some(true),
                ));
                raw = raw.saturating_add(count("raw_stream_bytes"));
                retained = retained.saturating_add(count("retained_bytes"));
                visible = visible.saturating_add(count("model_visible_bytes"));
            }
            "completion" => {
                if let Some(usage) = data.get("usage") {
                    if let Some(cost) = usage.get("cost").and_then(Json::as_u64) {
                        reported = reported.saturating_add(cost);
                        cost_reports = cost_reports.saturating_add(1);
                    }
                    for ((slot, reports), name) in
                        tokens.iter_mut().zip(token_reports.iter_mut()).zip([
                            "prompt_tokens",
                            "completion_tokens",
                            "reasoning_tokens",
                            "cached_tokens",
                            "cache_write_tokens",
                        ])
                    {
                        if let Some(n) = usage.get(name).and_then(Json::as_u64) {
                            *slot = slot.saturating_add(n);
                            *reports = reports.saturating_add(1);
                        }
                    }
                }
            }
            "budget_total" | "budget_settlement" => {
                accounted = data.get("charged").and_then(Json::as_u64)
            }
            "request_error" => {
                error_count = error_count.saturating_add(1);
                if errors.len() < 32 {
                    errors.push(data.clone());
                }
            }
            "end" => ended = Some(data.clone()),
            "cleanup" => cleanup = Some(data.clone()),
            _ => {}
        }
    }
    let complete = !partial
        && ended
            .as_ref()
            .and_then(|e| e.get("success"))
            .and_then(Json::as_bool)
            == Some(true);
    Ok(Json::Obj(vec![
        ("metrics_version".into(), Json::from(1u64)),
        ("records".into(), Json::from(sequence)),
        ("complete".into(), Json::Bool(complete)),
        ("partial_tail".into(), Json::Bool(partial)),
        ("end".into(), ended.unwrap_or(Json::Null)),
        ("cleanup".into(), cleanup.unwrap_or(Json::Null)),
        ("elapsed_ms".into(), Json::from(last_elapsed)),
        ("requests".into(), Json::from(requests)),
        ("cost_reports".into(), Json::from(cost_reports)),
        ("reported_cost".into(), Json::from(reported)),
        (
            "accounted_cost".into(),
            accounted.map_or(Json::Null, Json::from),
        ),
        (
            "unreported_accounting".into(),
            accounted.map_or(Json::Null, |n| Json::from(n.saturating_sub(reported))),
        ),
        (
            "peak_context_bytes".into(),
            peak.map_or(Json::Null, Json::from),
        ),
        ("tool_metric_records".into(), Json::from(tool_metrics)),
        ("request_duration_ms".into(), Json::from(duration_ms)),
        ("preflight_failures".into(), Json::from(preflight_failures)),
        ("nonzero_tool_exits".into(), Json::from(nonzero_exits)),
        ("failed_tools".into(), Json::from(failed_tools)),
        ("unsuccessful_processes".into(), Json::from(unsuccessful)),
        ("request_error_count".into(), Json::from(error_count)),
        ("tool_calls".into(), Json::from(tools)),
        ("repeated_calls".into(), Json::from(repeated)),
        ("shortened_results".into(), Json::from(shortened)),
        ("raw_command_bytes".into(), Json::from(raw)),
        ("retained_result_bytes".into(), Json::from(retained)),
        ("model_visible_tool_bytes".into(), Json::from(visible)),
        (
            "tokens".into(),
            Json::Obj(
                tokens
                    .into_iter()
                    .zip(token_reports)
                    .zip(["prompt", "completion", "reasoning", "cached", "cache_write"])
                    .map(|((n, reports), k)| {
                        (
                            k.into(),
                            if reports == 0 {
                                Json::Null
                            } else {
                                Json::from(n)
                            },
                        )
                    })
                    .collect(),
            ),
        ),
        ("request_errors".into(), Json::Arr(errors)),
    ]))
}

pub fn run(args: &[String]) -> Result<(), String> {
    let [path] = args else {
        return Err("usage: td-agent review-log FILE".into());
    };
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let summary = summarize(file)?;
    use std::io::Write;
    writeln!(std::io::stdout().lock(), "{}", summary).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn incomplete_trace_retains_reported_and_reserved_costs_separately() {
        let input = concat!(
            "{\"sequence\":0,\"kind\":\"completion\",\"data\":{\"usage\":{\"cost\":12,\"cached_tokens\":20}}}\n",
            "{\"sequence\":1,\"kind\":\"budget_total\",\"data\":{\"charged\":42}}\n",
            "{\"sequence\":2,\"kind\":");
        let result = summarize(input.as_bytes()).unwrap();
        assert_eq!(result.get("complete").and_then(Json::as_bool), Some(false));
        assert_eq!(
            result.get("partial_tail").and_then(Json::as_bool),
            Some(true)
        );
        assert_eq!(result.get("reported_cost").and_then(Json::as_u64), Some(12));
        assert_eq!(
            result.get("unreported_accounting").and_then(Json::as_u64),
            Some(30)
        );
        assert_eq!(
            result
                .get_path(&["tokens", "cached"])
                .and_then(Json::as_u64),
            Some(20)
        );
        assert!(
            summarize(b"{\"sequence\":2,\"kind\":\"end\",\"data\":null}\n".as_slice()).is_err()
        );
    }
    #[test]
    fn in_flight_requests_include_their_reservation_but_rejected_plans_do_not() {
        let settled = "{\"sequence\":0,\"kind\":\"budget_settlement\",\"data\":{\"charged\":12}}\n";
        let planned = "{\"sequence\":1,\"kind\":\"budget_reservation\",\"data\":{\"charged\":12,\"reserved\":30}}\n";
        let sent = "{\"sequence\":2,\"kind\":\"request_body\",\"data\":\"{}\"}\n";
        let rejected = summarize(format!("{settled}{planned}").as_bytes()).unwrap();
        assert_eq!(
            rejected.get("accounted_cost").and_then(Json::as_u64),
            Some(12)
        );
        let pending = summarize(format!("{settled}{planned}{sent}").as_bytes()).unwrap();
        assert_eq!(
            pending.get("accounted_cost").and_then(Json::as_u64),
            Some(42)
        );
        assert_eq!(pending.get("complete").and_then(Json::as_bool), Some(false));
    }
}
