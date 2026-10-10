//! Review-specific routing and a price envelope shared by admission and the wire.
use crate::{client, config::Client, cost, models, review, review_log::Journal};
use td_json::Json;

pub(crate) const MODES: &[&str] = &[
    "balanced", "cheapest", "floor", "fastest", "nitro", "latency",
];

pub(crate) fn explicit(options: &review::Options) -> bool {
    options.routing.is_some()
        || options.no_provider_fallbacks
        || !options.providers.is_empty()
        || options.max_input_price.is_some()
        || options.max_output_price.is_some()
}

pub(crate) fn prepare(
    options: &review::Options,
    client: &Client,
    journal: &mut Journal,
) -> Result<models::Models, String> {
    let model = options.model.as_deref().unwrap_or(&client.model);
    if !explicit(options) {
        return review::models(&client.base_url, &[model]);
    }
    if !model
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._/".contains(&b))
        || model.split('/').any(|p| matches!(p, "" | "." | ".."))
    {
        return Err("review routing requires a base model id; routing variants use --routing, other variants are unsupported with routing filters".into());
    }
    let response = td_fetch_client::get(
        &format!("{}/models/{model}/endpoints", client.base_url),
        &[("accept", "application/json")],
        Some(models::MAX_LIST),
        None,
    )
    .map_err(|e| format!("review endpoints: {e}"))?;
    if response.status != 200 {
        return Err(format!("review endpoints: status {}", response.status));
    }
    let raw = td_json::parse_slice(&response.body).map_err(|e| e.to_string())?;
    journal.event("routing_endpoints", raw.clone())?;
    let selected = select(options, model, raw)?;
    let metadata = models::Models::from_endpoints(model, selected.to_string().as_bytes())?;
    if metadata.pricing.is_none() {
        return Err("review routing has an unpriced endpoint".into());
    }
    Ok(models::Models {
        models: vec![metadata],
    })
}

fn select(options: &review::Options, model: &str, mut raw: Json) -> Result<Json, String> {
    let data = raw.get_mut("data").ok_or("review endpoints: no data")?;
    if data.get("id").and_then(Json::as_str) != Some(model) {
        return Err("review endpoints: model mismatch".into());
    }
    let endpoints = match data.get_mut("endpoints") {
        Some(Json::Arr(a)) => a,
        _ => return Err("review endpoints: no endpoints".into()),
    };
    endpoints.retain(|endpoint| {
        let tag = endpoint.get("tag").and_then(Json::as_str);
        let mode = options.routing.as_deref().unwrap_or("balanced");
        let tier_allowed = tag.is_none_or(|tag| match tag.rsplit('/').next() {
            Some("flex") => mode == "floor" || options.providers.iter().any(|p| p == tag),
            Some("fast" | "priority") => {
                mode == "nitro"
                    || options
                        .providers
                        .iter()
                        .any(|p| canonical_tier(p) == canonical_tier(tag))
            }
            Some("ultrafast") => options.providers.iter().any(|p| p == tag),
            _ => true,
        });
        let provider = options.providers.is_empty()
            || tag.is_some_and(|tag| {
                options.providers.iter().any(|p| {
                    canonical_tier(p) == canonical_tier(tag)
                        || (!p.contains('/')
                            && tag
                                .strip_prefix(p)
                                .is_some_and(|tail| tail.starts_with('/')))
                })
            });
        let price = |name, limit: Option<u64>| {
            limit.is_none_or(|limit| {
                endpoint
                    .get_path(&["pricing", name])
                    .and_then(Json::as_str)
                    .and_then(|s| cost::parse(s, true))
                    .is_some_and(|rate| rate <= limit)
            })
        };
        provider
            && tier_allowed
            && price("prompt", options.max_input_price)
            && price("completion", options.max_output_price)
    });
    endpoints.retain(|endpoint| {
        let supports = |name: &str| {
            endpoint
                .get("supported_parameters")
                .and_then(Json::as_arr)
                .is_some_and(|p| p.iter().any(|v| v.as_str() == Some(name)))
        };
        supports("max_tokens")
            && (options.repository.is_none() || supports("tools"))
            && (options.effort.is_none() || supports("reasoning"))
    });
    let largest = endpoints
        .iter()
        .filter_map(|e| e.get("max_completion_tokens").and_then(Json::as_u64))
        .max();
    let desired = options.max_tokens.unwrap_or(review::DEFAULT_MAX_TOKENS);
    let desired = if endpoints.iter().any(|e| {
        e.get("max_completion_tokens")
            .and_then(Json::as_u64)
            .is_none()
    }) {
        desired
    } else {
        largest.map_or(desired, |n| desired.min(n))
    };
    endpoints.retain(|e| {
        e.get("max_completion_tokens")
            .and_then(Json::as_u64)
            .is_none_or(|n| n >= desired && n > 0)
    });
    if endpoints.is_empty() {
        return Err(
            "review routing has no endpoint within provider, price and capability filters".into(),
        );
    }
    Ok(raw)
}

fn canonical_tier(tag: &str) -> String {
    tag.strip_suffix("/priority")
        .map_or_else(|| tag.to_string(), |base| format!("{base}/fast"))
}

fn rate(value: u64, divisor: u64) -> Json {
    let digits = if divisor == 1_000_000 { 6 } else { 12 };
    Json::Num(format!("{}.{:0digits$}", value / divisor, value % divisor))
}

pub(crate) fn head(
    base: &str,
    options: &review::Options,
    metadata: Option<&models::Model>,
    session: &str,
) -> Result<String, String> {
    let mut body = td_json::parse(&format!("{{{base}}}")).map_err(|e| e.to_string())?;
    let fields = match &mut body {
        Json::Obj(fields) => fields,
        _ => return Err("review request head is not an object".into()),
    };
    fields.push(("session_id".into(), Json::Str(session.into())));
    let mode = options.routing.as_deref().unwrap_or("balanced");
    if matches!(mode, "floor" | "nitro") {
        let model = fields
            .iter_mut()
            .find(|(k, _)| k == "model")
            .ok_or("review request has no model")?;
        let text = model.1.as_str().ok_or("review request model is not text")?;
        model.1 = Json::Str(format!("{text}:{mode}"));
    }
    let provider = fields
        .iter_mut()
        .find(|(k, _)| k == "provider")
        .ok_or("review request has no provider")?;
    let prefs = match &mut provider.1 {
        Json::Obj(fields) => fields,
        _ => return Err("review provider is not an object".into()),
    };
    if let Some(sort) = match mode {
        "cheapest" => Some("price"),
        "fastest" => Some("throughput"),
        "latency" => Some("latency"),
        _ => None,
    } {
        prefs.push(("sort".into(), Json::Str(sort.into())));
    }
    if options.no_provider_fallbacks {
        prefs.push(("allow_fallbacks".into(), Json::Bool(false)));
    }
    if !options.providers.is_empty() {
        prefs.push((
            "only".into(),
            Json::Arr(options.providers.iter().cloned().map(Json::Str).collect()),
        ));
    }
    if let Some(prices) = metadata.and_then(|m| m.pricing) {
        // Router filters and local reservation use the same undiscounted envelope.
        prefs.push((
            "max_price".into(),
            Json::Obj(vec![
                (
                    "prompt".into(),
                    rate(
                        options
                            .max_input_price
                            .map_or(prices.prompt, |cap| cap.min(prices.prompt)),
                        1_000_000,
                    ),
                ),
                (
                    "completion".into(),
                    rate(
                        options
                            .max_output_price
                            .map_or(prices.completion, |cap| cap.min(prices.completion)),
                        1_000_000,
                    ),
                ),
                ("request".into(), rate(prices.request, cost::ONE)),
            ]),
        ));
    }
    Ok(client::members(body))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn endpoints() -> Json {
        td_json::parse(r#"{"data":{"id":"z-ai/glm","endpoints":[{"tag":"cheap/fp8","context_length":100000,"supported_parameters":["tools","max_tokens"],"pricing":{"prompt":"0.0000001","completion":"0.0000002"}},{"tag":"dear","context_length":200000,"supported_parameters":["tools","max_tokens"],"pricing":{"prompt":"0.00001","completion":"0.00002"}}]}}"#).unwrap()
    }
    #[test]
    fn only_eligible_service_tiers_contribute_to_price_and_capability_bounds() {
        let raw = td_json::parse(r#"{"data":{"id":"openai/m","endpoints":[{"tag":"openai","context_length":100000,"supported_parameters":["tools","max_tokens"],"pricing":{"prompt":"0.00000175","completion":"0.000014"}},{"tag":"openai/fast","context_length":100000,"supported_parameters":["max_tokens"],"pricing":{"prompt":"0.0000035","completion":"0.000028"}},{"tag":"openai/flex","context_length":100000,"supported_parameters":["tools","max_tokens"],"pricing":{"prompt":"0.000000875","completion":"0.000007"}}]}}"#).unwrap();
        for (mode, count, output) in [
            ("balanced", 1, 14_000_000),
            ("cheapest", 1, 14_000_000),
            ("floor", 2, 14_000_000),
            ("nitro", 2, 28_000_000),
        ] {
            let options = review::Options {
                routing: Some(mode.into()),
                providers: vec!["openai".into()],
                ..Default::default()
            };
            let selected = select(&options, "openai/m", raw.clone()).unwrap();
            assert_eq!(
                selected
                    .get_path(&["data", "endpoints"])
                    .unwrap()
                    .as_arr()
                    .unwrap()
                    .len(),
                count,
                "{mode}"
            );
            let m = models::Models::from_endpoints("openai/m", selected.to_string().as_bytes())
                .unwrap();
            assert_eq!(m.pricing.unwrap().completion, output, "{mode}");
            if mode != "nitro" {
                assert!(m.supports("tools"));
            }
        }
        let options = review::Options {
            providers: vec!["openai/priority".into()],
            ..Default::default()
        };
        let selected = select(&options, "openai/m", raw).unwrap();
        assert_eq!(
            selected
                .get_path(&["data", "endpoints"])
                .unwrap()
                .as_arr()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn cache_write_premiums_do_not_raise_base_prompt_price_filters() {
        let mut m =
            models::Models::from_endpoints("z-ai/glm", endpoints().to_string().as_bytes()).unwrap();
        let prices = m.pricing.as_mut().unwrap();
        prices.cache_write = prices.prompt * 2;
        let base = r#""model":"z-ai/glm","provider":{}"#;
        let wire = td_json::parse(&format!(
            "{{{}}}",
            head(base, &review::Options::default(), Some(&m), "s").unwrap()
        ))
        .unwrap();
        assert_eq!(
            wire.get_path(&["provider", "max_price", "prompt"])
                .and_then(Json::as_f64),
            Some(10.0)
        );
        assert_eq!(m.pricing.unwrap().reserve(1, 0), 20_000_000);
    }
    #[test]
    fn unsupported_and_small_completion_endpoints_do_not_cap_capable_providers() {
        let raw = td_json::parse(r#"{"data":{"id":"m","endpoints":[{"tag":"good","context_length":100000,"max_completion_tokens":32768,"supported_parameters":["tools","max_tokens","reasoning"],"pricing":{"prompt":"0.000001","completion":"0.000001"}},{"tag":"no-tools","context_length":1000,"max_completion_tokens":4096,"supported_parameters":["max_tokens"],"pricing":{"prompt":"0.000001","completion":"0.000001"}},{"tag":"small","context_length":100000,"max_completion_tokens":4096,"supported_parameters":["tools","max_tokens","reasoning"],"pricing":{"prompt":"0.000001","completion":"0.000001"}}]}}"#).unwrap();
        let options = review::Options {
            repository: Some("repo".into()),
            effort: Some("high".into()),
            routing: Some("fastest".into()),
            ..Default::default()
        };
        let selected = select(&options, "m", raw).unwrap();
        let m = models::Models::from_endpoints("m", selected.to_string().as_bytes()).unwrap();
        assert!(m.supports("tools") && m.supports("reasoning"));
        assert_eq!(m.max_completion_tokens, Some(32768));
    }
    #[test]
    fn default_diff_head_has_a_price_ceiling_without_changing_router_sort() {
        let m =
            models::Models::from_endpoints("z-ai/glm", endpoints().to_string().as_bytes()).unwrap();
        let base =
            r#""model":"z-ai/glm","provider":{"require_parameters":true,"data_collection":"deny"}"#;
        let h = td_json::parse(&format!(
            "{{{}}}",
            head(base, &review::Options::default(), Some(&m), "default-diff").unwrap()
        ))
        .unwrap();
        assert!(h.get_path(&["provider", "sort"]).is_none());
        assert!(h.get_path(&["provider", "only"]).is_none());
        assert_eq!(
            h.get_path(&["provider", "max_price", "completion"])
                .and_then(Json::as_f64),
            Some(20.0)
        );
        let h = td_json::parse(&format!(
            "{{{}}}",
            head(base, &review::Options::default(), None, "unpriced").unwrap()
        ))
        .unwrap();
        assert!(h.get_path(&["provider", "max_price"]).is_none());
    }
    #[test]
    fn routing_options_refuse_ambiguous_and_invalid_values() {
        let args = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        for bad in [
            "--routing random",
            "--routing floor --routing nitro",
            "--provider A",
            "--provider a --provider a",
            "--provider a//b",
            "--max-input-price -1",
            "--max-output-price NaN",
            "--no-provider-fallbacks --no-provider-fallbacks",
        ] {
            assert!(review::Options::parse(&args(bad)).is_err(), "{bad}");
        }
        let options = review::Options::parse(&args(
            "--routing cheapest --provider cheap/fp8 --max-input-price 0.1 --max-output-price 2.0",
        ))
        .unwrap();
        assert_eq!(options.max_input_price, Some(100_000));
        assert_eq!(options.max_output_price, Some(2_000_000));
        assert!(select(
            &review::Options {
                max_output_price: Some(0),
                ..Default::default()
            },
            "z-ai/glm",
            endpoints()
        )
        .is_err());
        assert!(select(&options, "wrong/model", endpoints()).is_err());
    }
    #[test]
    fn allowlists_and_price_filters_bound_the_selected_endpoint_envelope() {
        let options = review::Options {
            providers: vec!["cheap".into()],
            ..Default::default()
        };
        let raw = select(&options, "z-ai/glm", endpoints()).unwrap();
        let metadata =
            models::Models::from_endpoints("z-ai/glm", raw.to_string().as_bytes()).unwrap();
        assert_eq!(metadata.pricing.unwrap().prompt, 100_000);
        let options = review::Options {
            max_input_price: Some(100_000),
            ..Default::default()
        };
        assert_eq!(
            select(&options, "z-ai/glm", endpoints())
                .unwrap()
                .get_path(&["data", "endpoints"])
                .unwrap()
                .as_arr()
                .unwrap()
                .len(),
            1
        );
        let options = review::Options {
            providers: vec!["che".into()],
            ..Default::default()
        };
        assert!(select(&options, "z-ai/glm", endpoints()).is_err());
    }
    #[test]
    fn routing_wire_preserves_restrictions_and_session_and_variant_semantics() {
        let base =
            r#""model":"z-ai/glm","provider":{"require_parameters":true,"data_collection":"deny"}"#;
        let metadata =
            models::Models::from_endpoints("z-ai/glm", endpoints().to_string().as_bytes()).unwrap();
        for mode in MODES {
            let options = review::Options {
                routing: Some((*mode).into()),
                providers: vec!["cheap".into()],
                no_provider_fallbacks: true,
                ..Default::default()
            };
            let h = td_json::parse(&format!(
                "{{{}}}",
                head(base, &options, Some(&metadata), "session").unwrap()
            ))
            .unwrap();
            assert_eq!(h.get("session_id").and_then(Json::as_str), Some("session"));
            let sort = match *mode {
                "cheapest" => Some("price"),
                "fastest" => Some("throughput"),
                "latency" => Some("latency"),
                _ => None,
            };
            assert_eq!(
                h.get_path(&["provider", "sort"]).and_then(Json::as_str),
                sort
            );
            assert_eq!(
                h.get_path(&["provider", "data_collection"])
                    .and_then(Json::as_str),
                Some("deny")
            );
            assert_eq!(
                h.get_path(&["provider", "only"]),
                Some(&Json::Arr(vec![Json::Str("cheap".into())]))
            );
            assert_eq!(
                h.get_path(&["provider", "allow_fallbacks"]),
                Some(&Json::Bool(false))
            );
            assert_eq!(
                h.get_path(&["provider", "require_parameters"]),
                Some(&Json::Bool(true))
            );
            let expected = if matches!(*mode, "floor" | "nitro") {
                format!("z-ai/glm:{mode}")
            } else {
                "z-ai/glm".into()
            };
            assert_eq!(
                h.get("model").and_then(Json::as_str),
                Some(expected.as_str())
            );
            assert_eq!(
                h.get_path(&["provider", "max_price", "prompt"])
                    .and_then(Json::as_f64),
                Some(10.0)
            );
        }
    }
}
