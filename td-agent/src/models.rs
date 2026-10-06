//! What td-agent knows of the provider's models and of the key's credit
//! (DESIGN.md §5). `GET /models` is fetched by the window process at
//! startup and cached in the state directory as `models`, then again once
//! each configured model it leaves out is looked for in its own `GET
//! /models/{id}/endpoints` (Jev's is listed only there), each model cut
//! to what td-agent uses: its context length, its largest completion, its
//! prices and the parameters it supports. A conversation process reads the
//! cache before each request; a cached list serves a start without the
//! network. `GET /key` gives the key's remaining credit for the status row.

use std::path::Path;

use crate::cost::{self, Pricing};
use td_json::Json;

/// The cache's file in the state directory.
pub const CACHE: &str = "models";
/// The longest models list taken from the provider, and the longest cache
/// read back.
pub const MAX_LIST: u64 = 16 * 1024 * 1024;
const MAX_CACHE: u64 = 8 * 1024 * 1024;

/// Whether a model id may stand in a URL's path as it is: letters,
/// digits and `-._:/`, no segment empty or a dot segment.
fn pathable(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._:/".contains(&b))
        && id.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

/// A GET of `url` under the list's bound: the body of a 200.
fn get(url: &str) -> Result<Vec<u8>, String> {
    let response =
        td_fetch_client::get(url, &[("accept", "application/json")], Some(MAX_LIST), None)
            .map_err(|e| e.to_string())?;
    if response.status != 200 {
        return Err(format!("status {}", response.status));
    }
    Ok(response.body)
}

/// `GET /models` from `base_url`.
pub fn fetch_list(base_url: &str) -> Result<Models, String> {
    Models::from_provider(&get(&format!("{base_url}/models"))?)
}

/// Each of `wanted` that `models` leaves out, from its `GET
/// /models/{id}/endpoints` as `Models::from_endpoints` reads it, added:
/// whether any was. One not found there, or not `pathable`, stays out,
/// why said on standard error, and is refused where it is used, as an
/// unlisted model is.
pub fn look_up(base_url: &str, models: &mut Models, wanted: &[&str]) -> bool {
    let mut added = false;
    for id in wanted {
        if models.find(id).is_some() {
            continue;
        }
        let found = if pathable(id) {
            get(&format!("{base_url}/models/{id}/endpoints"))
                .and_then(|body| Models::from_endpoints(id, &body))
        } else {
            Err("its id cannot be named in a URL as it is".into())
        };
        match found {
            Ok(model) => {
                models.models.push(model);
                added = true;
            }
            Err(why) => eprintln!(
                "td-agent: {id} is not in the models list, nor found by its endpoints: {why}"
            ),
        }
    }
    added
}

/// `fetch_list`, then `look_up` of `wanted`.
pub fn fetch(base_url: &str, wanted: &[&str]) -> Result<Models, String> {
    let mut models = fetch_list(base_url)?;
    look_up(base_url, &mut models, wanted);
    Ok(models)
}

/// One model as td-agent uses it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    pub id: String,
    pub context_length: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    /// `None` when the list gives no usable price: none at all, or a
    /// negative one, which marks a router whose price is not fixed.
    pub pricing: Option<Pricing>,
    pub supported_parameters: Vec<String>,
    /// The list's price strings as given, kept so the cache says what the
    /// provider said.
    prices: Vec<(String, String)>,
}

impl Model {
    pub fn supports(&self, parameter: &str) -> bool {
        self.supported_parameters.iter().any(|p| p == parameter)
    }
}

/// The models list.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Models {
    pub models: Vec<Model>,
}

/// The price fields td-agent reads, as the list names them.
const PRICES: [&str; 6] = [
    "prompt",
    "completion",
    "request",
    "internal_reasoning",
    "input_cache_read",
    "input_cache_write",
];

fn number(value: Option<&Json>) -> Option<u64> {
    value.and_then(Json::as_u64)
}

/// A price string or number's text, `None` when absent.
fn price_text(value: &Json) -> Option<String> {
    match value {
        Json::Str(text) | Json::Num(text) => Some(text.clone()),
        _ => None,
    }
}

fn pricing(prices: &[(String, String)]) -> Option<Pricing> {
    let get = |name: &str| -> Option<Option<u64>> {
        match prices.iter().find(|(key, _)| key == name) {
            None => Some(None),
            // A negative or unreadable price is no price: the whole list
            // entry is then unpriced, never priced at zero.
            Some((_, text)) => cost::parse(text, true).map(Some),
        }
    };
    Some(Pricing {
        prompt: get("prompt")??,
        completion: get("completion")??,
        request: get("request")?.unwrap_or(0),
        reasoning: get("internal_reasoning")?.unwrap_or(0),
        cache_read: get("input_cache_read")?.unwrap_or(0),
        cache_write: get("input_cache_write")?.unwrap_or(0),
    })
}

impl Model {
    fn from_parts(
        id: String,
        context_length: Option<u64>,
        max_completion_tokens: Option<u64>,
        prices: Vec<(String, String)>,
        supported_parameters: Vec<String>,
    ) -> Self {
        Self {
            pricing: pricing(&prices),
            id,
            context_length,
            max_completion_tokens,
            supported_parameters,
            prices,
        }
    }

    fn prices_of(value: Option<&Json>) -> Vec<(String, String)> {
        let Some(object) = value.and_then(Json::as_obj) else {
            return Vec::new();
        };
        PRICES
            .iter()
            .filter_map(|name| {
                let value = object.iter().find(|(key, _)| key == name)?;
                Some((name.to_string(), price_text(&value.1)?))
            })
            .collect()
    }

    fn parameters_of(value: Option<&Json>) -> Vec<String> {
        value
            .and_then(Json::as_arr)
            .map(|list| {
                list.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("id".into(), Json::Str(self.id.clone())),
            (
                "context_length".into(),
                self.context_length.map_or(Json::Null, Json::from),
            ),
            (
                "max_completion_tokens".into(),
                self.max_completion_tokens.map_or(Json::Null, Json::from),
            ),
            (
                "pricing".into(),
                Json::Obj(
                    self.prices
                        .iter()
                        .map(|(k, v)| (k.clone(), Json::Str(v.clone())))
                        .collect(),
                ),
            ),
            (
                "supported_parameters".into(),
                Json::Arr(
                    self.supported_parameters
                        .iter()
                        .map(|p| Json::Str(p.clone()))
                        .collect(),
                ),
            ),
        ])
    }
}

impl Models {
    /// The provider's `GET /models` body: `data`, each with an `id`.
    pub fn from_provider(body: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(body).map_err(|e| format!("the models list: {e}"))?;
        let data = value
            .get("data")
            .and_then(Json::as_arr)
            .ok_or("the models list has no `data` array")?;
        let mut models = Vec::new();
        for entry in data {
            let Some(id) = entry.get("id").and_then(Json::as_str) else {
                continue;
            };
            let top = entry.get("top_provider");
            models.push(Model::from_parts(
                id.to_string(),
                number(entry.get("context_length"))
                    .or_else(|| number(top.and_then(|t| t.get("context_length")))),
                number(top.and_then(|t| t.get("max_completion_tokens"))),
                Model::prices_of(entry.get("pricing")),
                Model::parameters_of(entry.get("supported_parameters")),
            ));
        }
        Ok(Self { models })
    }

    /// A model the list leaves out, from its `GET /models/{id}/endpoints`
    /// body: priced as its dearest endpoint for each price, or unpriced
    /// when any endpoint is, so its price is never understated; its
    /// context and completion the least any endpoint takes, and the
    /// parameters every one supports.
    pub fn from_endpoints(id: &str, body: &[u8]) -> Result<Model, String> {
        let value = td_json::parse_slice(body).map_err(|e| format!("{id}'s endpoints: {e}"))?;
        let data = value
            .get("data")
            .ok_or_else(|| format!("{id}'s endpoints: no `data`"))?;
        if data.get("id").and_then(Json::as_str) != Some(id) {
            return Err(format!("{id}'s endpoints name another model"));
        }
        let endpoints = data
            .get("endpoints")
            .and_then(Json::as_arr)
            .filter(|all| !all.is_empty())
            .ok_or_else(|| format!("{id} has no endpoints"))?;
        let mut prices: Vec<(String, String)> = Vec::new();
        let mut priced = true;
        let mut context: Option<u64> = None;
        let mut completion: Option<u64> = None;
        let mut parameters: Option<Vec<String>> = None;
        let least = |held: Option<u64>, one: Option<u64>| match (held, one) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        for endpoint in endpoints {
            let mut own = Model::prices_of(endpoint.get("pricing"));
            priced &= pricing(&own).is_some();
            // Without a cache-read price an endpoint bills cached tokens
            // at its prompt rate, which another's cheaper one must not
            // stand for.
            let prompt = own.iter().find(|(name, _)| name == "prompt").cloned();
            if let Some((_, text)) = prompt {
                if !own.iter().any(|(name, _)| name == "input_cache_read") {
                    own.push(("input_cache_read".into(), text));
                }
            }
            for (name, text) in own {
                let dearer = match prices.iter().find(|(held, _)| *held == name) {
                    Some((_, held)) => cost::parse(&text, true) > cost::parse(held, true),
                    None => true,
                };
                if dearer {
                    prices.retain(|(held, _)| *held != name);
                    prices.push((name, text));
                }
            }
            context = least(context, number(endpoint.get("context_length")));
            completion = least(completion, number(endpoint.get("max_completion_tokens")));
            let own = Model::parameters_of(endpoint.get("supported_parameters"));
            parameters = Some(match parameters {
                Some(held) => held.into_iter().filter(|p| own.contains(p)).collect(),
                None => own,
            });
        }
        if !priced {
            prices.clear();
        }
        Ok(Model::from_parts(
            id.to_string(),
            context,
            completion,
            prices,
            parameters.unwrap_or_default(),
        ))
    }

    /// The cache's text: what `from_cache` reads back.
    pub fn to_cache(&self) -> String {
        Json::Arr(self.models.iter().map(Model::to_json).collect()).to_string()
    }

    pub fn from_cache(body: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(body).map_err(|e| format!("the models cache: {e}"))?;
        let list = value.as_arr().ok_or("the models cache is not a list")?;
        let mut models = Vec::new();
        for entry in list {
            let id = entry
                .get("id")
                .and_then(Json::as_str)
                .ok_or("a cached model with no id")?;
            models.push(Model::from_parts(
                id.to_string(),
                number(entry.get("context_length")),
                number(entry.get("max_completion_tokens")),
                Model::prices_of(entry.get("pricing")),
                Model::parameters_of(entry.get("supported_parameters")),
            ));
        }
        Ok(Self { models })
    }

    /// The cache in `state`, when there is one.
    pub fn load(state: &Path) -> Result<Option<Self>, String> {
        let path = state.join(CACHE);
        match crate::store::read_bounded(&path, MAX_CACHE) {
            Ok(bytes) => Self::from_cache(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, state: &Path) -> Result<(), String> {
        crate::store::replace(state, CACHE, self.to_cache().as_bytes())
    }

    pub fn find(&self, id: &str) -> Option<&Model> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// The key's credit as `GET /key` gives it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Credit {
    /// What the key may still spend, `None` when the key has no limit of
    /// its own.
    pub remaining: Option<u64>,
    /// What the key has spent.
    pub usage: u64,
}

impl Credit {
    pub fn from_provider(body: &[u8]) -> Result<Self, String> {
        let value = td_json::parse_slice(body).map_err(|e| format!("the key's record: {e}"))?;
        let data = value.get("data").ok_or("the key's record has no `data`")?;
        let amount = |name: &str| -> Result<Option<u64>, String> {
            match data.get(name) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::Num(text)) => cost::parse(text, false)
                    .map(Some)
                    // A negative remainder is a key past its limit.
                    .or_else(|| text.starts_with('-').then_some(Some(0)))
                    .ok_or_else(|| format!("the key's `{name}` is {text}")),
                Some(_) => Err(format!("the key's `{name}` is not a number")),
            }
        };
        Ok(Self {
            remaining: amount("limit_remaining")?,
            usage: amount("usage")?.unwrap_or(0),
        })
    }

    /// The status row's word for it.
    pub fn show(&self) -> String {
        match self.remaining {
            Some(remaining) => format!("credit {}", cost::show(remaining)),
            None => format!("used {} (no key limit)", cost::show(self.usage)),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    /// A model the list leaves out, as its endpoints listing says (Jev's,
    /// recorded): priced per field at its dearest endpoint, unpriced when
    /// any endpoint is; its least context and completion and the
    /// parameters all share; kept through the cache; refused when the
    /// listing names another model or has no endpoint.
    #[test]
    fn a_model_left_out_of_the_list_is_read_from_its_endpoints() {
        let recorded = include_bytes!("../tests/fixtures/openrouter/endpoints-jev.json");
        let jev = Models::from_endpoints("typesafe/jev-1.13", recorded).unwrap();
        let pricing = jev.pricing.unwrap();
        assert_eq!(pricing.completion, 0);
        assert_eq!(Some(pricing.prompt), cost::parse("0.000000042", true));
        assert_eq!(jev.context_length, Some(32000));
        let cached = Models {
            models: vec![jev.clone()],
        };
        assert_eq!(
            Models::from_cache(cached.to_cache().as_bytes())
                .unwrap()
                .models,
            [jev]
        );
        let two = |a: &str, b: &str| {
            format!(
                r#"{{"data":{{"id":"x/y","endpoints":[{{"pricing":{a},"context_length":8,"supported_parameters":["max_tokens","tools"]}},{{"pricing":{b},"context_length":4,"max_completion_tokens":2,"supported_parameters":["max_tokens"]}}]}}}}"#
            )
        };
        let both = Models::from_endpoints(
            "x/y",
            two(
                r#"{"prompt":"0.000001","completion":"0.000003"}"#,
                r#"{"prompt":"0.000002","completion":"0.000001"}"#,
            )
            .as_bytes(),
        )
        .unwrap();
        let pricing = both.pricing.unwrap();
        assert_eq!(Some(pricing.prompt), cost::parse("0.000002", true));
        assert_eq!(Some(pricing.completion), cost::parse("0.000003", true));
        assert_eq!(
            (both.context_length, both.max_completion_tokens),
            (Some(4), Some(2))
        );
        assert!(both.supports("max_tokens") && !both.supports("tools"));
        let routed = Models::from_endpoints(
            "x/y",
            two(
                r#"{"prompt":"0.000001","completion":"0"}"#,
                r#"{"prompt":"-1","completion":"-1"}"#,
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(routed.pricing, None);
        // One endpoint's cache-read discount does not stand for another
        // that has none, which bills cached tokens at its prompt rate.
        let cached = Models::from_endpoints(
            "x/y",
            two(
                r#"{"prompt":"0.000001","completion":"0","input_cache_read":"0.0000001"}"#,
                r#"{"prompt":"0.000001","completion":"0"}"#,
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            Some(cached.pricing.unwrap().cache_read),
            cost::parse("0.000001", true)
        );
        assert!(Models::from_endpoints("other/model", recorded).is_err());
        assert!(Models::from_endpoints("x/y", br#"{"data":{"id":"x/y","endpoints":[]}}"#).is_err());
    }

    /// Only an id that stands in a path as it is is looked for.
    #[test]
    fn only_a_plain_model_id_is_named_in_a_path() {
        for id in [
            "typesafe/jev-1.13",
            "openai/gpt-oss-safeguard-20b",
            "a:b/c_d",
        ] {
            assert!(pathable(id), "{id}");
        }
        for id in [
            "", "a/../b", "a//b", "./a", "a?b", "a#b", "a%2fb", "a b", "/a",
        ] {
            assert!(!pathable(id), "{id}");
        }
    }
    use super::*;

    const LIST: &str = r#"{"data":[
        {"id":"anthropic/claude-sonnet-5.5","name":"Claude","context_length":1000000,
         "pricing":{"prompt":"0.000003","completion":"0.000015","request":"0","image":"0.0048",
                    "input_cache_read":"0.0000003","input_cache_write":"0.00000375"},
         "top_provider":{"context_length":1000000,"max_completion_tokens":64000,"is_moderated":false},
         "supported_parameters":["max_tokens","reasoning","tools","tool_choice"]},
        {"id":"openrouter/auto","context_length":2000000,
         "pricing":{"prompt":"-1","completion":"-1"},
         "top_provider":{"max_completion_tokens":null},"supported_parameters":[]},
        {"id":"free/model","pricing":{"prompt":"0","completion":"0"},
         "top_provider":{"context_length":8192}},
        {"name":"no id"}
    ]}"#;

    #[test]
    fn the_list_is_cut_to_what_td_agent_uses_and_survives_its_cache() {
        let models = Models::from_provider(LIST.as_bytes()).unwrap();
        assert_eq!(models.models.len(), 3);
        let sonnet = models.find("anthropic/claude-sonnet-5.5").unwrap();
        assert_eq!(sonnet.context_length, Some(1_000_000));
        assert_eq!(sonnet.max_completion_tokens, Some(64_000));
        assert!(sonnet.supports("reasoning") && !sonnet.supports("web_search"));
        assert_eq!(
            sonnet.pricing,
            Some(Pricing {
                prompt: 3_000_000,
                completion: 15_000_000,
                request: 0,
                reasoning: 0,
                cache_read: 300_000,
                cache_write: 3_750_000,
            })
        );
        // A negative price marks a router whose price is not fixed.
        assert_eq!(models.find("openrouter/auto").unwrap().pricing, None);
        let free = models.find("free/model").unwrap();
        assert_eq!(free.pricing, Some(Pricing::default()));
        assert_eq!(free.context_length, Some(8192));
        assert!(models.find("nope").is_none());
        let again = Models::from_cache(models.to_cache().as_bytes()).unwrap();
        assert_eq!(again, models);
        assert!(Models::from_provider(b"{}").is_err());
        assert!(Models::from_cache(b"{}").is_err());
    }

    #[test]
    fn the_credit_is_read_from_the_keys_record() {
        let credit = Credit::from_provider(
            br#"{"data":{"label":"sk-or-v1-abc...","limit":10,"usage":2.5,"limit_remaining":7.5,"is_free_tier":false}}"#,
        )
        .unwrap();
        assert_eq!(credit.remaining, Some(7 * cost::ONE + cost::ONE / 2));
        assert_eq!(credit.show(), "credit $7.5000");
        let unlimited = Credit::from_provider(
            br#"{"data":{"limit":null,"usage":0.25,"limit_remaining":null}}"#,
        )
        .unwrap();
        assert_eq!(unlimited.show(), "used $0.2500 (no key limit)");
        let over = Credit::from_provider(br#"{"data":{"usage":11,"limit_remaining":-1}}"#).unwrap();
        assert_eq!(over.remaining, Some(0));
        assert!(Credit::from_provider(br#"{"data":{"usage":"x"}}"#).is_err());
        assert!(Credit::from_provider(b"[]").is_err());
    }
}
