//! Derive transcript and cache diagnostics from the exact recorded request view.
use std::collections::BTreeMap;
use td_json::Json;

const MAX_ROWS: usize = 64;

#[derive(Default)]
pub(crate) struct Analysis {
    rows: Vec<Json>,
    requests: u64,
    previous: Option<Vec<u8>>,
    reservation: Option<Json>,
    metrics: Option<Json>,
    provider: Option<String>,
    switches: u64,
}

impl Analysis {
    pub(crate) fn event(&mut self, kind: &str, data: &Json) -> Result<(), String> {
        match kind {
            "request_metrics" => self.metrics = Some(data.clone()),
            "budget_reservation" => self.reservation = Some(data.clone()),
            "request_body" => {
                self.requests = self.requests.saturating_add(1);
                let text = data.as_str().unwrap_or_default();
                let body = td_json::parse(text).unwrap_or(Json::Null);
                let Some(messages) = body.get("messages").and_then(Json::as_arr) else {
                    self.previous = None;
                    self.metrics = None;
                    self.reservation = None;
                    if self.rows.len() < MAX_ROWS {
                        self.rows.push(Json::Obj(vec![
                            ("index".into(), Json::from(self.requests)),
                            ("analysis_unavailable".into(), Json::Bool(true)),
                            ("served_provider".into(), Json::Null),
                            ("reported_usage".into(), Json::Null),
                            ("cache_tokens".into(), Json::Null),
                        ]));
                    }
                    return Ok(());
                };
                let mut content: BTreeMap<&str, u64> = BTreeMap::new();
                for (index, m) in messages.iter().enumerate() {
                    let role = m.get("role").and_then(Json::as_str).unwrap_or("unknown");
                    let category = match role {
                        "system" | "developer" => "instructions",
                        "tool" => "tool_results",
                        "assistant" => "assistant_text",
                        "user"
                            if m.get("content")
                                .and_then(Json::as_str)
                                .is_some_and(|s| s.starts_with("[Review harness")) =>
                        {
                            "harness_status"
                        }
                        "user" if index <= 1 => "review_material",
                        _ => "other",
                    };
                    let bytes = m.get("content").map_or(0, content_bytes);
                    *content.entry(category).or_default() += bytes;
                    for field in ["reasoning", "reasoning_details", "tool_calls"] {
                        if let Some(value) = m.get(field).filter(|v| !v.is_null()) {
                            *content.entry(field).or_default() += value.to_string().len() as u64;
                        }
                    }
                }
                // Provider caches prompt content, not changing generation/routing options.
                let context = Json::Obj(vec![
                    (
                        "tools".into(),
                        body.get("tools").cloned().unwrap_or(Json::Null),
                    ),
                    ("messages".into(), Json::Arr(messages.to_vec())),
                ])
                .to_string()
                .into_bytes();
                let shared = self.previous.as_ref().map(|previous| {
                    previous
                        .iter()
                        .zip(context.iter())
                        .take_while(|(a, b)| a == b)
                        .count() as u64
                });
                let previous_bytes = self.previous.as_ref().map(|p| p.len() as u64);
                self.previous = Some(context.clone());
                let reservation = self.reservation.take().unwrap_or(Json::Null);
                let metrics = self.metrics.take().unwrap_or(Json::Null);
                if self.rows.len() < MAX_ROWS {
                    self.rows.push(Json::Obj(vec![
                        ("index".into(), Json::from(self.requests)),
                        ("request_bytes".into(), Json::from(text.len() as u64)),
                        ("context_bytes".into(), Json::from(context.len() as u64)),
                        (
                            "previous_context_bytes".into(),
                            previous_bytes.map_or(Json::Null, Json::from),
                        ),
                        (
                            "shared_serialized_prefix_bytes".into(),
                            shared.map_or(Json::Null, Json::from),
                        ),
                        ("messages".into(), Json::from(messages.len() as u64)),
                        (
                            "content_bytes".into(),
                            Json::Obj(
                                content
                                    .into_iter()
                                    .map(|(k, v)| (k.into(), Json::from(v)))
                                    .collect(),
                            ),
                        ),
                        (
                            "model".into(),
                            body.get("model").cloned().unwrap_or(Json::Null),
                        ),
                        (
                            "session_id".into(),
                            body.get("session_id").cloned().unwrap_or(Json::Null),
                        ),
                        (
                            "provider_policy".into(),
                            body.get("provider").cloned().unwrap_or(Json::Null),
                        ),
                        (
                            "cache_control".into(),
                            body.get("cache_control").cloned().unwrap_or(Json::Null),
                        ),
                        ("reservation".into(), reservation),
                        ("admission".into(), metrics),
                        ("served_provider".into(), Json::Null),
                        ("reported_usage".into(), Json::Null),
                        ("cache_tokens".into(), Json::Null),
                    ]));
                }
            }
            "completion" => {
                let provider = data
                    .get("provider")
                    .and_then(Json::as_str)
                    .map(str::to_owned);
                if let Some(provider) = provider {
                    if self
                        .provider
                        .as_ref()
                        .is_some_and(|prior| prior != &provider)
                    {
                        self.switches = self.switches.saturating_add(1);
                    }
                    self.provider = Some(provider);
                }
                if self.requests <= MAX_ROWS as u64 {
                    if let Some(Json::Obj(row)) = self.rows.last_mut() {
                        for (name, value) in [
                            (
                                "served_provider",
                                data.get("provider").cloned().unwrap_or(Json::Null),
                            ),
                            ("reported_usage", reported_usage(data)),
                            ("cache_tokens", cache_tokens(data)),
                        ] {
                            if let Some((_, slot)) = row.iter_mut().find(|(k, _)| k == name) {
                                *slot = value;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(crate) fn json(self) -> Json {
        Json::Obj(vec![
            ("requests".into(), Json::Arr(self.rows)),
            (
                "requests_omitted".into(),
                Json::from(self.requests.saturating_sub(MAX_ROWS as u64)),
            ),
            (
                "observed_provider_switches".into(),
                Json::from(self.switches),
            ),
        ])
    }
}

const TOKEN_FIELDS: &[&str] = &[
    "prompt_tokens",
    "completion_tokens",
    "reasoning_tokens",
    "cached_tokens",
    "cache_write_tokens",
];

pub(crate) fn reported_tokens(data: &Json, name: &str) -> Option<u64> {
    let path: &[&str] = match name {
        "reasoning_tokens" => &["completion_tokens_details", "reasoning_tokens"],
        "cached_tokens" => &["prompt_tokens_details", "cached_tokens"],
        "cache_write_tokens" => &["prompt_tokens_details", "cache_write_tokens"],
        _ => &[name],
    };
    if let Some(raw) = data.get("raw_usage").filter(|u| !u.is_null()) {
        return raw.get_path(path).and_then(Json::as_u64);
    }
    let usage = data.get("usage")?;
    let defaulted = usage
        .get("missing_token_counts_default_to_zero")
        .and_then(Json::as_bool)
        == Some(true);
    usage.get(name).and_then(Json::as_u64).filter(|n| {
        *n != 0 || !(defaulted || matches!(name, "cached_tokens" | "cache_write_tokens"))
    })
}
fn reported_usage(data: &Json) -> Json {
    let mut fields: Vec<(String, Json)> = TOKEN_FIELDS
        .iter()
        .map(|name| {
            (
                (*name).into(),
                reported_tokens(data, name).map_or(Json::Null, Json::from),
            )
        })
        .collect();
    fields.push((
        "provider_cost_usd".into(),
        data.get_path(&["raw_usage", "cost"])
            .filter(|v| matches!(v,Json::Num(s) if s.len()<=128))
            .cloned()
            .unwrap_or(Json::Null),
    ));
    fields.push((
        "normalized_cost_pico".into(),
        data.get_path(&["usage", "cost"])
            .and_then(Json::as_u64)
            .map_or(Json::Null, Json::from),
    ));
    Json::Obj(fields)
}

fn content_bytes(value: &Json) -> u64 {
    if value.is_null() {
        0
    } else if let Some(text) = value.as_str() {
        text.len() as u64
    } else {
        value.to_string().len() as u64
    }
}

fn cache_tokens(data: &Json) -> Json {
    Json::Obj(vec![
        (
            "read".into(),
            reported_tokens(data, "cached_tokens").map_or(Json::Null, Json::from),
        ),
        (
            "write".into(),
            reported_tokens(data, "cache_write_tokens").map_or(Json::Null, Json::from),
        ),
    ])
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn transcript_growth_and_cache_unknowns_are_observed_from_wire_records() {
        let mut a = Analysis::default();
        let first=Json::Str(r#"{"model":"m","messages":[{"role":"system","content":"rules"},{"role":"user","content":"diff"}]}"#.into());
        a.event("request_body", &first).unwrap();
        a.event("completion",&td_json::parse(r#"{"provider":"P","raw_usage":{"prompt_tokens":10,"prompt_tokens_details":{"cached_tokens":0}}}"#).unwrap()).unwrap();
        a.event("request_body",&Json::Str(r#"{"model":"m","messages":[{"role":"system","content":"rules"},{"role":"user","content":"diff"},{"role":"assistant","content":"inspect","reasoning_details":[{"opaque":"x"}]},{"role":"tool","content":"evidence"}]}"#.into())).unwrap();
        a.event(
            "completion",
            &td_json::parse(r#"{"provider":"Q","usage":{"cached_tokens":0}}"#).unwrap(),
        )
        .unwrap();
        let result = a.json();
        let rows = result.get("requests").unwrap().as_arr().unwrap();
        assert_eq!(
            rows[0].get_path(&["cache_tokens", "read"]),
            Some(&Json::from(0u64))
        );
        assert_eq!(
            rows[0].get_path(&["cache_tokens", "write"]),
            Some(&Json::Null)
        );
        assert_eq!(
            rows[1].get_path(&["cache_tokens", "read"]),
            Some(&Json::Null)
        );
        assert_eq!(
            rows[1].get_path(&["content_bytes", "tool_results"]),
            Some(&Json::from(8u64))
        );
        assert!(
            rows[1]
                .get("shared_serialized_prefix_bytes")
                .unwrap()
                .as_u64()
                .unwrap()
                > 50
        );
        assert_eq!(
            result.get("observed_provider_switches"),
            Some(&Json::from(1u64))
        );
    }
    #[test]
    fn diagnostic_rows_are_bounded_and_omitted_completions_do_not_overwrite_them() {
        let mut a = Analysis::default();
        let request = Json::Str(r#"{"messages":[{"role":"user","content":"diff"}]}"#.into());
        for _ in 0..65 {
            a.event("request_body", &request).unwrap();
        }
        a.event(
            "completion",
            &td_json::parse(r#"{"provider":"late"}"#).unwrap(),
        )
        .unwrap();
        let result = a.json();
        let rows = result.get("requests").unwrap().as_arr().unwrap();
        assert_eq!(rows.len(), 64);
        assert_eq!(
            rows.last().unwrap().get("served_provider"),
            Some(&Json::Null)
        );
        assert_eq!(result.get("requests_omitted"), Some(&Json::from(1u64)));
    }
}
