//! `$XDG_CONFIG_HOME/td-agent/config`, TOML (DESIGN.md §15). Every key the
//! design lists is known here: the ones built so far are parsed and
//! checked, each other one is accepted by name and reported as read by the
//! increment that first uses it, a retired one is accepted and said to be
//! read no more, and an unknown key, `limits` included, is refused by
//! name. A missing file is every default.

use std::path::{Path, PathBuf};

use crate::cost::{self, Limits};
use crate::workspace::{Shared, Workspace};
use td_json::Json;
use td_toml::Toml;

/// Where models are asked for (DESIGN.md §5).
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// The model a conversation uses by default.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5.5";
/// The cheap model titles come from (DESIGN.md §13).
pub const DEFAULT_TITLE_MODEL: &str = "anthropic/claude-haiku-4.5";
/// The classifier's two stages' models (DESIGN.md §11): Jev, and the
/// reasoning stage's bring-your-own-policy safety model.
pub const DEFAULT_JEV_MODEL: &str = "typesafe/jev-1.13";
pub const DEFAULT_CLASSIFIER_MODEL: &str = "openai/gpt-oss-safeguard-20b";
/// OpenRouter's reasoning efforts, `reasoning.effort`.
pub const EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];
const DEFAULT_EFFORT: &str = "medium";
/// The longest base URL or model id taken.
pub(crate) const MAX_NAME: usize = 2048;

/// What the model client reads of the configuration: the window process
/// parses it and hands it to every conversation process (DESIGN.md §2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Client {
    /// The API's root, `https://` and no trailing slash: the requests go
    /// to its `/chat/completions`, `/models` and `/key`.
    pub base_url: String,
    pub model: String,
    pub title_model: String,
    pub reasoning_effort: String,
    /// `provider.data_collection`: whether providers that may keep or
    /// train on prompts may serve a request; `deny` by default.
    pub allow_data_collection: bool,
    /// The classifier's models (DESIGN.md §11): Jev, and the reasoning
    /// stage's.
    pub classifier_fast_model: String,
    pub classifier_model: String,
    /// The probability Jev's answers must reach, in thousandths;
    /// `JEV_THRESHOLD_SHIPPED` by default.
    pub jev_threshold: u16,
    /// Whether the classifier allows nothing without Jev; `true` by
    /// default.
    pub jev_required: bool,
    pub limits: Limits,
    /// The shared directories the window admitted (DESIGN.md §8),
    /// resolved and absolute: what every workspace instance binds.
    pub shared: Vec<Shared>,
    /// Every configured template, with the shared directories of its
    /// own its workspaces bind in place of `shared`, admitted as it is.
    pub template_shared: Vec<TemplateShared>,
    /// The most background processes a conversation runs at once.
    pub max_background: u32,
    /// Whether a conversation past `compact_at` is compacted, or its turn
    /// stopped (DESIGN.md §14).
    pub auto_compact: bool,
    /// The share of a model's context, in percent, past which a request
    /// compacts first.
    pub compact_at: u8,
    /// The most tokens of a compacted conversation's recent tail.
    pub compact_keep_tokens: u64,
    /// The model that writes a compaction's summary; the conversation's
    /// own when none.
    pub compact_model: Option<String>,
    /// How long, in seconds, a provider's prompt cache is taken to last
    /// (DESIGN.md §14).
    pub cache_ttl: u64,
    /// The estimated prompt past which a turn resuming after `cache_ttl`
    /// first asks the human; none never asks.
    pub cold_resume_tokens: Option<u64>,
    /// The bytes of each background process's output kept.
    pub background_output_bytes: u64,
    /// The branches a push to which is always the person's (DESIGN.md
    /// §9, Pushing), beside each workspace's bases.
    pub protected_branches: Vec<String>,
    /// The network policy of a workspace whose template sets none
    /// (DESIGN.md §10).
    pub network: Network,
    /// The destinations every workspace's allowlist starts from.
    pub network_allowlist: Vec<Destination>,
}

/// A configured template and its own shared directories, admitted; none
/// when it names none and its workspaces bind `shared`; and its own
/// network policy, none when its workspaces take `network`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateShared {
    pub name: String,
    pub shared: Option<Vec<Shared>>,
    pub network: Option<Network>,
}

/// A workspace's network policy (DESIGN.md §10).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Network {
    /// No proxy: nothing leaves the jail.
    Off,
    /// The proxy admits the workspace's allowlist.
    #[default]
    Allowlist,
    /// The proxy admits any destination the relay reaches; only the
    /// human sets it, in a template.
    Open,
}

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Allowlist => "allowlist",
            Self::Open => "open",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(Self::Off),
            "allowlist" => Some(Self::Allowlist),
            "open" => Some(Self::Open),
            _ => None,
        }
    }
}

/// The most destinations an allowlist holds.
pub const MAX_ALLOWLIST: usize = 256;

/// The shipped `network_allowlist` (DESIGN.md §10): download hosts
/// alone, none that also takes uploads with a token.
pub const DEFAULT_ALLOWLIST: &[&str] = &[
    "static.crates.io",
    "index.crates.io",
    "static.rust-lang.org",
    "pypi.org",
    "files.pythonhosted.org",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "proxy.golang.org",
    "sum.golang.org",
];

/// A destination on an allowlist: a host, a DNS name in lower case or
/// an IP address, and a port.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Destination {
    pub host: String,
    pub port: u16,
}

impl Destination {
    /// `host`, `host:port`, `[v6]` or `[v6]:port`; 443 when no port is
    /// named.
    pub fn parse(text: &str) -> Result<Self, String> {
        let wrong = || format!("{text:?} is not a host with an optional port");
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (inner, after) = rest.split_once(']').ok_or_else(wrong)?;
            let v6 = inner.parse::<std::net::Ipv6Addr>().map_err(|_| wrong())?;
            let port = match after {
                "" => None,
                after => Some(after.strip_prefix(':').ok_or_else(wrong)?),
            };
            (format!("[{v6}]"), port)
        } else {
            let (host, port) = match text.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (text, None),
            };
            (dns_or_v4(host).ok_or_else(wrong)?, port)
        };
        let port = match port {
            None => 443,
            Some(port) => Some(port)
                .filter(|port| (1..=5).contains(&port.len()))
                .filter(|port| port.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|port| port.parse::<u16>().ok())
                .filter(|port| *port != 0)
                .ok_or_else(wrong)?,
        };
        Ok(Self { host, port })
    }

    /// As configuration writes it: the port left off when it is 443.
    pub fn text(&self) -> String {
        if self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `host` as a destination names it: an IPv4 address, or a DNS name of
/// letters, digits and hyphens, in lower case, a trailing dot dropped,
/// whose last label is not a number, decimal or `0x` hexadecimal, since
/// the resolver reads such a name (`10.1`, `0x7f.1`, `2130706433`) as an
/// address. The egress relay's `parse_host` (net/src/egress.rs) holds
/// the same rule.
pub fn dns_or_v4(host: &str) -> Option<String> {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return Some(host.to_string());
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    let numeric = name.rsplit('.').next().is_some_and(|last| {
        let hex = last
            .get(..2)
            .filter(|prefix| prefix.eq_ignore_ascii_case("0x"))
            .and_then(|_| last.get(2..));
        last.bytes().all(|b| b.is_ascii_digit())
            || hex.is_some_and(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
    });
    (!name.is_empty() && name.len() <= 253 && name.split('.').all(label_ok) && !numeric)
        .then(|| name.to_ascii_lowercase())
}

/// `network_allowlist`'s list, each a destination, at most
/// `MAX_ALLOWLIST`, each once.
fn allowlist(items: Option<&[String]>) -> Result<Vec<Destination>, String> {
    let wrong = format!(
        "`network_allowlist` is a list of at most {MAX_ALLOWLIST} hosts, each with an optional port"
    );
    let items = items.ok_or(wrong.as_str())?;
    if items.len() > MAX_ALLOWLIST {
        return Err(wrong);
    }
    let mut found: Vec<Destination> = Vec::new();
    for item in items {
        let destination =
            Destination::parse(item).map_err(|e| format!("`network_allowlist`: {e}"))?;
        if !found.contains(&destination) {
            found.push(destination);
        }
    }
    Ok(found)
}

/// The shipped allowlist.
pub fn default_allowlist() -> Vec<Destination> {
    DEFAULT_ALLOWLIST
        .iter()
        .map(|host| Destination {
            host: (*host).to_string(),
            port: 443,
        })
        .collect()
}

impl Default for Client {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            title_model: DEFAULT_TITLE_MODEL.into(),
            reasoning_effort: DEFAULT_EFFORT.into(),
            allow_data_collection: false,
            classifier_fast_model: DEFAULT_JEV_MODEL.into(),
            classifier_model: DEFAULT_CLASSIFIER_MODEL.into(),
            jev_threshold: JEV_THRESHOLD_SHIPPED,
            jev_required: true,
            limits: Limits::default(),
            shared: Vec::new(),
            template_shared: Vec::new(),
            max_background: DEFAULT_MAX_BACKGROUND,
            auto_compact: true,
            compact_at: DEFAULT_COMPACT_AT,
            compact_keep_tokens: DEFAULT_COMPACT_KEEP_TOKENS,
            compact_model: None,
            cache_ttl: DEFAULT_CACHE_TTL,
            cold_resume_tokens: Some(DEFAULT_COLD_RESUME_TOKENS),
            background_output_bytes: DEFAULT_BACKGROUND_OUTPUT_BYTES,
            network: Network::default(),
            network_allowlist: default_allowlist(),
            protected_branches: crate::git::PROTECTED
                .iter()
                .map(|branch| (*branch).to_string())
                .collect(),
        }
    }
}

/// The most protected branches configured.
const MAX_PROTECTED: usize = 64;

/// `protected_branches`' list, each a branch a push could name, at most
/// `MAX_PROTECTED`.
fn protected_branches(items: Option<&[String]>) -> Result<Vec<String>, String> {
    let wrong = format!("`protected_branches` is a list of at most {MAX_PROTECTED} branch names");
    let items = items.ok_or(wrong.as_str())?;
    if items.len() > MAX_PROTECTED {
        return Err(wrong);
    }
    let mut branches: Vec<String> = Vec::new();
    for item in items {
        crate::git::push_branch(item).map_err(|e| format!("`protected_branches`: {e}"))?;
        if !branches.contains(item) {
            branches.push(item.clone());
        }
    }
    Ok(branches)
}

/// Shared directories as `to_json` writes them.
fn shared_json(shared: &[Shared]) -> Json {
    Json::Arr(
        shared
            .iter()
            .map(|shared| {
                Json::Obj(vec![
                    ("path".into(), Json::Str(shared.path.display().to_string())),
                    ("write".into(), Json::Bool(shared.write)),
                ])
            })
            .collect(),
    )
}

/// `shared_json`'s value back.
fn shared_from_json(value: &Json) -> Result<Vec<Shared>, String> {
    let Json::Arr(items) = value else {
        return Err("shared is not a list".into());
    };
    items
        .iter()
        .map(|item| {
            let path = item
                .get("path")
                .and_then(Json::as_str)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or("a shared directory with no absolute path")?;
            let write = item
                .get("write")
                .and_then(Json::as_bool)
                .ok_or("a shared directory with no write flag")?;
            Ok(Shared { path, write })
        })
        .collect()
}

impl Client {
    /// The models this configuration names, each looked for beyond the
    /// models list when it leaves one out (`models::fetch`).
    pub fn wanted(&self) -> Vec<String> {
        let mut wanted: Vec<String> = Vec::new();
        for id in [
            &self.model,
            &self.title_model,
            &self.classifier_model,
            &self.classifier_fast_model,
        ]
        .into_iter()
        .chain(&self.compact_model)
        {
            if !wanted.contains(id) {
                wanted.push(id.clone());
            }
        }
        wanted
    }

    /// What a workspace binds: a template's own shared directories when
    /// it was made from one that names them, `shared` when it names none,
    /// and none when its template is no longer configured, so removing or
    /// renaming one never widens what its conversations reach.
    pub fn shared_for(&self, workspace: &Workspace) -> &[Shared] {
        let template = match workspace {
            Workspace::Template(name) => Some(name),
            Workspace::Repositories(repositories) => Some(&repositories.template),
            Workspace::Scratch | Workspace::Directory(_) => None,
        };
        match template {
            Some(name) => match self.template_shared.iter().find(|t| &t.name == name) {
                Some(TemplateShared {
                    shared: Some(own), ..
                }) => own,
                Some(TemplateShared { shared: None, .. }) => &self.shared,
                None => &[],
            },
            None => &self.shared,
        }
    }

    /// A workspace's network policy: its template's own when it names
    /// one, `network` when it names none or the workspace has no
    /// template, and `off` when its template is no longer configured, so
    /// removing or renaming one never widens what its conversations
    /// reach.
    pub fn network_for(&self, workspace: &Workspace) -> Network {
        let template = match workspace {
            Workspace::Template(name) => Some(name),
            Workspace::Repositories(repositories) => Some(&repositories.template),
            Workspace::Scratch | Workspace::Directory(_) => None,
        };
        match template {
            Some(name) => match self.template_shared.iter().find(|t| &t.name == name) {
                Some(TemplateShared {
                    network: Some(own), ..
                }) => *own,
                Some(TemplateShared { network: None, .. }) => self.network,
                None => Network::Off,
            },
            None => self.network,
        }
    }

    /// The settings as the socketpair carries them.
    pub fn to_json(&self) -> Json {
        let limit = |l: Option<u64>| l.map_or(Json::Null, Json::from);
        Json::Obj(vec![
            ("base_url".into(), Json::Str(self.base_url.clone())),
            ("model".into(), Json::Str(self.model.clone())),
            ("title_model".into(), Json::Str(self.title_model.clone())),
            (
                "reasoning_effort".into(),
                Json::Str(self.reasoning_effort.clone()),
            ),
            (
                "data_collection".into(),
                Json::Str(
                    if self.allow_data_collection {
                        "allow"
                    } else {
                        "deny"
                    }
                    .into(),
                ),
            ),
            (
                "classifier_fast_model".into(),
                Json::Str(self.classifier_fast_model.clone()),
            ),
            (
                "classifier_model".into(),
                Json::Str(self.classifier_model.clone()),
            ),
            (
                "jev_threshold".into(),
                Json::from(u64::from(self.jev_threshold)),
            ),
            ("jev_required".into(), Json::Bool(self.jev_required)),
            ("max_cost_per_turn".into(), limit(self.limits.turn)),
            (
                "max_cost_per_conversation".into(),
                limit(self.limits.conversation),
            ),
            ("max_cost_per_day".into(), limit(self.limits.day)),
            (
                "max_background".into(),
                Json::from(u64::from(self.max_background)),
            ),
            ("auto_compact".into(), Json::Bool(self.auto_compact)),
            ("compact_at".into(), Json::from(u64::from(self.compact_at))),
            (
                "compact_keep_tokens".into(),
                Json::from(self.compact_keep_tokens),
            ),
            (
                "compact_model".into(),
                self.compact_model.clone().map_or(Json::Null, Json::Str),
            ),
            ("cache_ttl".into(), Json::from(self.cache_ttl)),
            (
                "cold_resume_tokens".into(),
                self.cold_resume_tokens
                    .map_or(Json::Str("none".into()), Json::from),
            ),
            (
                "background_output_bytes".into(),
                Json::from(self.background_output_bytes),
            ),
            ("shared".into(), shared_json(&self.shared)),
            (
                "protected_branches".into(),
                Json::Arr(
                    self.protected_branches
                        .iter()
                        .map(|branch| Json::Str(branch.clone()))
                        .collect(),
                ),
            ),
            ("network".into(), Json::Str(self.network.name().into())),
            (
                "network_allowlist".into(),
                Json::Arr(
                    self.network_allowlist
                        .iter()
                        .map(|destination| Json::Str(destination.text()))
                        .collect(),
                ),
            ),
            (
                "template_shared".into(),
                Json::Arr(
                    self.template_shared
                        .iter()
                        .map(|template| {
                            Json::Obj(vec![
                                ("name".into(), Json::Str(template.name.clone())),
                                (
                                    "shared".into(),
                                    template.shared.as_deref().map_or(Json::Null, shared_json),
                                ),
                                (
                                    "network".into(),
                                    template
                                        .network
                                        .map_or(Json::Null, |n| Json::Str(n.name().into())),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }

    /// `to_json`'s value back, each field checked as the file's is.
    pub fn from_json(value: &Json) -> Result<Self, String> {
        let text = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_str)
                .ok_or_else(|| format!("no {name}"))
        };
        let limit = |name: &str| match value.get(name) {
            Some(Json::Null) => Ok(None),
            Some(v) => v.as_u64().map(Some).ok_or_else(|| format!("no {name}")),
            None => Err(format!("no {name}")),
        };
        let client = Self {
            base_url: base_url(text("base_url")?)?,
            model: model_id("model", text("model")?)?,
            title_model: model_id("title_model", text("title_model")?)?,
            reasoning_effort: effort(text("reasoning_effort")?)?,
            allow_data_collection: data_collection(text("data_collection")?)?,
            classifier_fast_model: model_id(
                "classifier_fast_model",
                text("classifier_fast_model")?,
            )?,
            classifier_model: model_id("classifier_model", text("classifier_model")?)?,
            jev_threshold: match value.get("jev_threshold").map(Json::as_u64) {
                None => return Err("no jev_threshold".into()),
                Some(None) => return Err("jev_threshold is not a whole number".into()),
                Some(Some(t)) => u16::try_from(t)
                    .ok()
                    .filter(|t| JEV_THRESHOLD.contains(t))
                    .ok_or("jev_threshold is out of range")?,
            },
            jev_required: value
                .get("jev_required")
                .and_then(Json::as_bool)
                .ok_or("no jev_required")?,
            limits: Limits {
                turn: limit("max_cost_per_turn")?,
                conversation: limit("max_cost_per_conversation")?,
                day: limit("max_cost_per_day")?,
            },
            auto_compact: match value.get("auto_compact") {
                None => true,
                Some(on) => on.as_bool().ok_or("auto_compact is not true or false")?,
            },
            compact_at: match value.get("compact_at") {
                None => DEFAULT_COMPACT_AT,
                Some(n) => n
                    .as_u64()
                    .and_then(|n| u8::try_from(n).ok())
                    .filter(|n| COMPACT_AT.contains(n))
                    .ok_or("compact_at is out of range")?,
            },
            compact_keep_tokens: match value.get("compact_keep_tokens") {
                None => DEFAULT_COMPACT_KEEP_TOKENS,
                Some(n) => n
                    .as_u64()
                    .filter(|n| COMPACT_KEEP_TOKENS.contains(n))
                    .ok_or("compact_keep_tokens is out of range")?,
            },
            compact_model: match value.get("compact_model") {
                None | Some(Json::Null) => None,
                Some(id) => Some(model_id(
                    "compact_model",
                    id.as_str().ok_or("compact_model is not text")?,
                )?),
            },
            cache_ttl: match value.get("cache_ttl") {
                None => DEFAULT_CACHE_TTL,
                Some(n) => n
                    .as_u64()
                    .filter(|n| CACHE_TTL.contains(n))
                    .ok_or("cache_ttl is out of range")?,
            },
            cold_resume_tokens: match value.get("cold_resume_tokens") {
                None => Some(DEFAULT_COLD_RESUME_TOKENS),
                Some(Json::Str(none)) if none == "none" => None,
                Some(n) => Some(
                    n.as_u64()
                        .filter(|n| COLD_RESUME_TOKENS.contains(n))
                        .ok_or("cold_resume_tokens is out of range")?,
                ),
            },
            max_background: match value.get("max_background") {
                None => DEFAULT_MAX_BACKGROUND,
                Some(n) => n
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| MAX_BACKGROUND.contains(n))
                    .ok_or("max_background is out of range")?,
            },
            background_output_bytes: match value.get("background_output_bytes") {
                None => DEFAULT_BACKGROUND_OUTPUT_BYTES,
                Some(n) => n
                    .as_u64()
                    .filter(|n| BACKGROUND_OUTPUT_BYTES.contains(n))
                    .ok_or("background_output_bytes is out of range")?,
            },
            shared: match value.get("shared") {
                None => Vec::new(),
                Some(shared) => shared_from_json(shared)?,
            },
            // A setup from a window before it: the defaults.
            protected_branches: match value.get("protected_branches") {
                None => Self::default().protected_branches,
                Some(list) => {
                    let items: Option<Vec<String>> = list.as_arr().and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().map(str::to_string))
                            .collect()
                    });
                    protected_branches(items.as_deref())?
                }
            },
            // As in the file: `open` is a template's alone.
            network: match value.get("network") {
                None => Network::default(),
                Some(network) => network
                    .as_str()
                    .and_then(Network::parse)
                    .filter(|network| *network != Network::Open)
                    .ok_or("`network` is off or allowlist")?,
            },
            network_allowlist: match value.get("network_allowlist") {
                None => default_allowlist(),
                Some(list) => {
                    let items: Option<Vec<String>> = list.as_arr().and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().map(str::to_string))
                            .collect()
                    });
                    allowlist(items.as_deref())?
                }
            },
            template_shared: match value.get("template_shared") {
                None => Vec::new(),
                Some(Json::Arr(items)) => items
                    .iter()
                    .map(|item| {
                        let name = item
                            .get("name")
                            .and_then(Json::as_str)
                            .ok_or("a template's shared directories with no name")?;
                        let shared = match item.get("shared") {
                            Some(Json::Null) => None,
                            Some(shared) => Some(
                                shared_from_json(shared)
                                    .map_err(|e| format!("template {name:?}: {e}"))?,
                            ),
                            None => return Err(format!("template {name:?} with no shared")),
                        };
                        let network = match item.get("network") {
                            None | Some(Json::Null) => None,
                            Some(network) => Some(
                                network.as_str().and_then(Network::parse).ok_or_else(|| {
                                    format!(
                                        "template {name:?}: `network` is off, allowlist or open"
                                    )
                                })?,
                            ),
                        };
                        Ok(TemplateShared {
                            name: template_name(name)?,
                            shared,
                            network,
                        })
                    })
                    .collect::<Result<_, String>>()?,
                Some(_) => return Err("template_shared is not a list".into()),
            },
        };
        Ok(client)
    }
}

/// `base_url`: an `https` URL, so the key never crosses in the clear, with
/// no query, fragment, space or control byte; a trailing slash is dropped.
fn base_url(text: &str) -> Result<String, String> {
    let url = text.trim_end_matches('/');
    let rest = url.strip_prefix("https://").ok_or_else(|| {
        format!("`base_url` must be an https:// URL, so the key is never sent in the clear; not {text:?}")
    })?;
    if rest.is_empty()
        || url.len() > MAX_NAME
        || url.contains(['?', '#'])
        || !url.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(format!(
            "`base_url` must be a plain https:// URL with a host and no query, fragment or space; not {text:?}"
        ));
    }
    Ok(url.to_string())
}

/// A model id: visible ASCII, as the provider's ids are.
pub(crate) fn model_id(key: &str, text: &str) -> Result<String, String> {
    if text.is_empty() || text.len() > MAX_NAME || !text.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "`{key}` must be a model id such as {DEFAULT_MODEL:?}; not {text:?}"
        ));
    }
    Ok(text.to_string())
}

pub(crate) fn effort(text: &str) -> Result<String, String> {
    if EFFORTS.contains(&text) {
        Ok(text.to_string())
    } else {
        Err(format!(
            "`reasoning_effort` is one of {}; not {text:?}",
            EFFORTS.join(", ")
        ))
    }
}

/// `jev_threshold`'s default, in thousandths: a threshold at which no
/// case of `calibration/crossings.json` that should be asked about was
/// allowed by both stages in either of two live runs, 0.055 above the
/// nearest such case Jev answered `matches` to (DESIGN.md §11; the
/// commit that set it records the counts).
const JEV_THRESHOLD_SHIPPED: u16 = 775;
/// `compact_keep_tokens`'s default, and what it may be.
const DEFAULT_COMPACT_KEEP_TOKENS: u64 = 20_000;
const COMPACT_KEEP_TOKENS: std::ops::RangeInclusive<u64> = 1_000..=1_000_000;
/// `cache_ttl`'s default, Anthropic's ephemeral cache's lifetime, and
/// what it may be: 0 takes every resumption as cold.
const DEFAULT_CACHE_TTL: u64 = 300;
const CACHE_TTL: std::ops::RangeInclusive<u64> = 0..=86_400;
/// `cold_resume_tokens`'s default, and what it may be.
const DEFAULT_COLD_RESUME_TOKENS: u64 = 32_000;
const COLD_RESUME_TOKENS: std::ops::RangeInclusive<u64> = 1_000..=10_000_000;
/// `compact_at`'s default, in percent.
const DEFAULT_COMPACT_AT: u8 = 80;
/// What `compact_at` may be, in percent.
const COMPACT_AT: std::ops::RangeInclusive<u8> = 10..=100;

/// `compact_at`: a share of the context from 0.1 to 1 in at most two
/// decimal places, in percent.
fn compact_at(value: &Toml) -> Result<u8, String> {
    let wrong = || {
        format!("`compact_at` is a share of the context from 0.1 to 1 in at most two decimal places, not {value:?}")
    };
    let p = match value {
        Toml::Float(p) => *p,
        Toml::Int(1) => 1.0,
        _ => return Err(wrong()),
    };
    let percent = (p * 100.0).round();
    if !p.is_finite() || (p * 100.0 - percent).abs() > 1e-6 {
        return Err(wrong());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let percent = percent as i64;
    u8::try_from(percent)
        .ok()
        .filter(|p| COMPACT_AT.contains(p))
        .ok_or_else(wrong)
}

/// The thresholds `jev_threshold` may be, in thousandths.
const JEV_THRESHOLD: std::ops::RangeInclusive<u16> = 500..=1000;

/// `jev_threshold`: a probability from 0.5 to 1 in at most three
/// decimal places, in thousandths.
fn jev_threshold(value: &Toml) -> Result<u16, String> {
    let wrong = || {
        format!("`jev_threshold` is a probability from 0.5 to 1 in at most three decimal places, not {value:?}")
    };
    let p = match value {
        Toml::Float(p) => *p,
        Toml::Int(1) => 1.0,
        _ => return Err(wrong()),
    };
    let thousandths = (p * 1000.0).round();
    if !p.is_finite() || (p * 1000.0 - thousandths).abs() > 1e-6 {
        return Err(wrong());
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let thousandths = thousandths as i64;
    u16::try_from(thousandths)
        .ok()
        .filter(|t| JEV_THRESHOLD.contains(t))
        .ok_or_else(wrong)
}

fn data_collection(text: &str) -> Result<bool, String> {
    match text {
        "deny" => Ok(false),
        "allow" => Ok(true),
        other => Err(format!(
            "`data_collection` is `deny` or `allow`, not {other:?}"
        )),
    }
}

/// Why a `cold_resume_tokens` value is refused.
fn cold_resume_refused(value: &Toml) -> String {
    format!(
        "`cold_resume_tokens` is a whole number from {} to {}, or \"none\"; not {value:?}",
        COLD_RESUME_TOKENS.start(),
        COLD_RESUME_TOKENS.end()
    )
}

/// A cost limit: credits, a whole or decimal number of at least zero, or
/// `"none"` for no limit.
fn limit(key: &str, value: &Toml) -> Result<Option<u64>, String> {
    let refused =
        || format!("`{key}` is a number of credits of at least zero, or \"none\"; not {value:?}");
    match value {
        Toml::Str(text) if text == "none" => Ok(None),
        Toml::Int(whole) => u64::try_from(*whole)
            .ok()
            .and_then(|whole| whole.checked_mul(cost::ONE))
            .map(Some)
            .ok_or_else(refused),
        Toml::Float(number) if number.is_finite() && *number >= 0.0 => {
            cost::parse(&number.to_string(), false)
                .map(Some)
                .ok_or_else(refused)
        }
        _ => Err(refused()),
    }
}

/// The longest configuration file read.
const MAX_BYTES: u64 = 1024 * 1024;

/// The mode a workspace starts in (DESIGN.md §11).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Mode {
    #[default]
    Auto,
    Ask,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Ask => "ask",
        }
    }
}

/// What this increment does with a key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Use {
    /// Parsed and checked now.
    Read,
    /// Accepted by name; the DESIGN.md §18 increment given reads it.
    Later(u8),
}

/// Every key DESIGN.md §15 lists, in its order.
const KEYS: &[(&str, Use)] = &[
    ("base_url", Use::Read),
    ("model", Use::Read),
    ("title_model", Use::Read),
    ("classifier_fast_model", Use::Read),
    ("classifier_model", Use::Read),
    ("jev_threshold", Use::Read),
    ("jev_required", Use::Read),
    ("reasoning_effort", Use::Read),
    ("mode", Use::Read),
    ("data_collection", Use::Read),
    ("max_cost_per_turn", Use::Read),
    ("max_cost_per_conversation", Use::Read),
    ("max_cost_per_day", Use::Read),
    ("workspace_root", Use::Read),
    ("shared", Use::Read),
    ("template", Use::Read),
    ("remotes", Use::Read),
    ("network", Use::Read),
    ("network_allowlist", Use::Read),
    ("protected_branches", Use::Read),
    ("fetch_interval", Use::Read),
    ("fetch_concurrency", Use::Later(11)),
    ("max_background", Use::Read),
    ("background_output_bytes", Use::Read),
    ("auto_compact", Use::Read),
    ("compact_at", Use::Read),
    ("compact_keep_tokens", Use::Read),
    ("compact_model", Use::Read),
    ("cache_ttl", Use::Read),
    ("cold_resume_tokens", Use::Read),
];

/// Keys no longer read, accepted with a note of why, so a file written
/// for an earlier td-agent still loads (DESIGN.md §15).
const RETIRED: &[(&str, &str)] = &[(
    "orchestrator_model",
    "there is no orchestrator: every conversation uses `model`, or the model chosen for it",
)];

/// Keys refused with a reason of their own rather than as unknown.
const REFUSED: &[(&str, &str)] = &[(
    "limits",
    "there is no `limits` key until resource limits land (DESIGN.md §8, §15)",
)];

/// The configuration this increment uses.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Config {
    pub mode: Mode,
    pub client: Client,
    /// The `model` key as the file gives it, none when it is left out:
    /// what a default chosen in the window is set over (DESIGN.md §4).
    pub model_key: Option<String>,
    /// One line per key present that a later increment reads, so a
    /// setting that does nothing yet is said, never silently ignored.
    pub notes: Vec<String>,
    /// `workspace_root` as the file gives it, `~` unexpanded; none is
    /// `DEFAULT_WORKSPACE_ROOT`.
    pub workspace_root: Option<PathBuf>,
    /// `[[shared]]` as the file gives them, `~` unexpanded; empty by
    /// default, so no shared directory is bound unless it is named here
    /// or in its template.
    pub shared: Vec<Shared>,
    /// `[[template]]` in the order written (DESIGN.md §7).
    pub templates: Vec<Template>,
    /// `remotes`: the git remotes the human admitted, each a remote or a
    /// host with a path prefix (DESIGN.md §7, Admitted remotes).
    pub remotes: Vec<crate::git::Admission>,
    /// `fetch_interval` in seconds as the file gives it; none is
    /// `DEFAULT_FETCH_INTERVAL`.
    pub fetch_interval: Option<u64>,
}

/// The most background processes a conversation runs at once
/// (DESIGN.md §12, §15), by default and as a file may set it.
pub const DEFAULT_MAX_BACKGROUND: u32 = 4;
const MAX_BACKGROUND: std::ops::RangeInclusive<u32> = 1..=16;
/// The bytes of a background process's output kept, the latest
/// (DESIGN.md §12, §15), by default and as a file may set it.
pub const DEFAULT_BACKGROUND_OUTPUT_BYTES: u64 = 16 << 20;
const BACKGROUND_OUTPUT_BYTES: std::ops::RangeInclusive<u64> = (64 << 10)..=(1 << 30);

/// How often, in seconds, the stores are fetched in the background
/// (DESIGN.md §7, Keeping current), and the bounds a file may set.
pub const DEFAULT_FETCH_INTERVAL: u64 = 600;
const FETCH_INTERVAL: std::ops::RangeInclusive<u64> = 60..=86_400;

impl Config {
    /// How often the stores are fetched in the background.
    pub fn fetch_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.fetch_interval.unwrap_or(DEFAULT_FETCH_INTERVAL))
    }
}

/// A workspace template (DESIGN.md §7, §15).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Template {
    pub name: String,
    /// The repositories to check out, which increment 11 prepares; until
    /// then a template naming any is refused at creation.
    pub repos: Vec<Repo>,
    /// Its own `[[shared]]`, `~` unexpanded, in place of the top-level
    /// list; none is the top-level list.
    pub shared: Option<Vec<Shared>>,
    /// Its own network policy, in place of the top-level `network`.
    pub network: Option<Network>,
}

/// One of a template's repositories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repo {
    pub remote: String,
    pub base: String,
    pub branch: String,
    /// The cone-mode paths to check out; none is the whole tree.
    pub sparse: Option<Vec<String>>,
}

/// The longest template name.
pub const MAX_TEMPLATE_NAME: usize = 64;
/// The most templates a configuration lists.
pub const MAX_TEMPLATES: usize = 64;
/// The chooser's built-ins and its row that makes a template, which no
/// template may be named.
pub const EMPTY: &str = "Empty";
pub const DIRECTORY: &str = "Directory\u{2026}";
pub const NEW_TEMPLATE: &str = "New template\u{2026}";

/// A template's name: visible, at most `MAX_TEMPLATE_NAME` bytes, and
/// neither built-in's, ASCII case aside.
pub fn template_name(name: &str) -> Result<String, String> {
    // As a card would show it: nothing invisible, no bidirectional
    // control, so no name can look like another's.
    let visible = crate::tools::visible(name) == name;
    if name.is_empty() || name.len() > MAX_TEMPLATE_NAME || name.trim() != name || !visible {
        return Err(format!(
            "a template's `name` is visible text of at most {MAX_TEMPLATE_NAME} bytes with no space at either end, not {name:?}"
        ));
    }
    let lower = name.to_ascii_lowercase();
    let reserved = [
        "empty",
        "directory\u{2026}",
        "directory...",
        "new template\u{2026}",
        "new template...",
    ];
    if reserved.contains(&lower.as_str()) {
        return Err(format!(
            "no template may be named {name:?}: the chooser lists {EMPTY}, {DIRECTORY} and {NEW_TEMPLATE} itself"
        ));
    }
    Ok(name.to_string())
}

/// A `[[shared]]` list under `key`.
fn shared_list(key: &str, value: &Toml) -> Result<Vec<Shared>, String> {
    let wrong = || format!("`{key}` is a list of `[[{key}]]` tables");
    let items = value.as_arr().ok_or_else(wrong)?;
    let mut shared = Vec::new();
    for item in items {
        if !item.is_table() {
            return Err(wrong());
        }
        item.check_known_keys(&["path", "write"])
            .map_err(|e| format!("`{key}`: {e}"))?;
        let path = item
            .optional_str("path")
            .map_err(|e| format!("`{key}`: {e}"))?
            .ok_or_else(|| format!("a `[[{key}]]` table has no `path`"))?;
        let write = match item.get("write") {
            None => false,
            Some(write) => write
                .as_bool()
                .ok_or_else(|| format!("`{key}`'s `write` is true or false"))?,
        };
        shared.push(Shared {
            path: configured_path(key, path)?,
            write,
        });
    }
    Ok(shared)
}

/// A repository field: visible text of at most `MAX_NAME` bytes.
fn repo_text<'a>(item: &'a Toml, key: &str) -> Result<&'a str, String> {
    let text = item
        .optional_str(key)
        .map_err(|e| format!("`template.repos`: {e}"))?
        .ok_or_else(|| format!("a `[[template.repos]]` table has no `{key}`"))?;
    if text.is_empty() || text.len() > MAX_NAME || text.chars().any(char::is_control) {
        return Err(format!(
            "`template.repos`'s `{key}` is text of at most {MAX_NAME} bytes, not {text:?}"
        ));
    }
    Ok(text)
}

const SPARSE: &str =
    "`template.repos`'s `sparse` is a list of relative paths with no `..` or control character";

/// A sparse path as given: relative, inside the tree, at most `MAX_NAME`
/// bytes, nothing that would break git's line-based sparse file.
fn sparse_path(path: &str) -> Option<String> {
    let inside = !path.starts_with('/') && !path.split('/').any(|part| part == "..");
    (!path.is_empty() && path.len() <= MAX_NAME && inside && !path.contains(char::is_control))
        .then(|| path.to_string())
}

/// `[[template]]`.
fn templates(value: &Toml) -> Result<Vec<Template>, String> {
    let wrong = "`template` is a list of `[[template]]` tables";
    let items = value.as_arr().ok_or(wrong)?;
    if items.len() > MAX_TEMPLATES {
        return Err(format!("at most {MAX_TEMPLATES} templates are listed"));
    }
    let mut templates: Vec<Template> = Vec::new();
    for item in items {
        if !item.is_table() {
            return Err(wrong.into());
        }
        item.check_known_keys(&["name", "repos", "shared", "network"])
            .map_err(|e| format!("`template`: {e}"))?;
        let name = item
            .optional_str("name")
            .map_err(|e| format!("`template`: {e}"))?
            .ok_or("a `[[template]]` table has no `name`")?;
        let name = template_name(name)?;
        if templates.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("two templates are named {name:?}"));
        }
        let network = match item.optional_str("network") {
            Ok(None) => None,
            Ok(Some(text)) => Some(Network::parse(text).ok_or_else(|| {
                format!("template {name:?}: `network` is off, allowlist or open, not {text:?}")
            })?),
            Err(e) => return Err(format!("template {name:?}: {e}")),
        };
        let mut repos = Vec::new();
        if let Some(value) = item.get("repos") {
            let wrong = "`template.repos` is a list of `[[template.repos]]` tables";
            for repo in value.as_arr().ok_or(wrong)? {
                if !repo.is_table() {
                    return Err(wrong.into());
                }
                repo.check_known_keys(&["remote", "base", "branch", "sparse"])
                    .map_err(|e| format!("`template.repos`: {e}"))?;
                let sparse = match repo.get("sparse") {
                    None => None,
                    Some(paths) => Some(
                        paths
                            .as_arr()
                            .ok_or(SPARSE)?
                            .iter()
                            .map(|path| path.as_str().and_then(sparse_path).ok_or(SPARSE))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                };
                repos.push(Repo {
                    remote: repo_text(repo, "remote")?.into(),
                    base: repo_text(repo, "base")?.into(),
                    branch: repo_text(repo, "branch")?.into(),
                    sparse,
                });
            }
        }
        let shared = match item.get("shared") {
            None => None,
            Some(value) => Some(shared_list("template.shared", value)?),
        };
        templates.push(Template {
            name,
            repos,
            shared,
            network,
        });
    }
    Ok(templates)
}

/// A repository of a template made in the window (DESIGN.md §7,
/// Templates made in the window), checked whole as preparing it would
/// check it: its remote as td-agent records it, its base a git branch
/// name, its branch one a push could name, and its sparse paths, none
/// being the whole tree.
pub fn checked_repo(
    remote: &str,
    base: &str,
    branch: &str,
    sparse: Option<Vec<String>>,
) -> Result<Repo, String> {
    let remote = crate::git::Remote::parse(remote)?.url();
    crate::git::branch_name(base).map_err(|e| format!("the base: {e}"))?;
    if base.len() > MAX_NAME {
        return Err(format!("the base is past {MAX_NAME} bytes"));
    }
    // The branch is the one its pushes name.
    crate::git::push_branch(branch).map_err(|e| format!("the branch: {e}"))?;
    let sparse = match sparse {
        None => None,
        Some(paths) if paths.len() > MAX_SPARSE => {
            return Err(format!("at most {MAX_SPARSE} sparse paths"))
        }
        Some(paths) => Some(
            paths
                .iter()
                .map(|path| sparse_path(path).ok_or(SPARSE))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    // As the checkout takes them: no `.`, empty segment or pattern.
    crate::repo::cone(sparse.as_deref())?;
    Ok(Repo {
        remote,
        base: base.to_string(),
        branch: branch.to_string(),
        sparse,
    })
}

/// The most sparse paths a template made in the window names.
pub const MAX_SPARSE: usize = 64;
/// The most shared directories a template made in the window names.
pub const MAX_TEMPLATE_SHARED: usize = 16;

/// A template's shared directories as the dialog's field gives them:
/// paths parted by spaces, each absolute or under `~`, read-only unless
/// it ends `:rw`, with no control character, which the field cannot
/// show; none when the field is empty, so its workspaces bind the
/// top-level list.
pub fn shared_field(text: &str) -> Result<Option<Vec<Shared>>, String> {
    if text.contains(char::is_control) {
        return Err("a shared folder's path holds a control character".into());
    }
    let mut shared: Vec<Shared> = Vec::new();
    for word in text.split_whitespace() {
        if shared.len() == MAX_TEMPLATE_SHARED {
            return Err(format!("at most {MAX_TEMPLATE_SHARED} shared folders"));
        }
        let (path, write) = match word.strip_suffix(":rw") {
            Some(path) => (path, true),
            None => (word, false),
        };
        let path = configured_path("shared", path).map_err(|_| {
            format!("a shared folder is an absolute path or one under `~`, ending `:rw` to let it be written, not {word:?}")
        })?;
        if shared.iter().any(|s| s.path == path) {
            return Err(format!("{} is named twice", path.display()));
        }
        shared.push(Shared { path, write });
    }
    Ok((!shared.is_empty()).then_some(shared))
}

/// `shared_field`'s text for `shared`, back.
pub fn shared_text(shared: Option<&[Shared]>) -> String {
    shared
        .unwrap_or_default()
        .iter()
        .map(|shared| {
            let path = shared.path.display();
            if shared.write {
                format!("{path}:rw")
            } else {
                path.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Templates made in the window as their file holds them: a name, its
/// network policy and shared directories when it names its own, and its
/// repositories, none for a scratch workspace's.
pub fn templates_json(templates: &[Template]) -> Json {
    Json::Arr(
        templates
            .iter()
            .map(|template| {
                let mut fields = vec![("name".into(), Json::Str(template.name.clone()))];
                // Only a template given one names a network policy.
                if let Some(network) = template.network {
                    fields.push(("network".into(), Json::Str(network.name().into())));
                }
                if let Some(shared) = &template.shared {
                    fields.push((
                        "shared".into(),
                        Json::Arr(
                            shared
                                .iter()
                                .map(|shared| {
                                    Json::Obj(vec![
                                        (
                                            "path".into(),
                                            Json::Str(shared.path.display().to_string()),
                                        ),
                                        ("write".into(), Json::Bool(shared.write)),
                                    ])
                                })
                                .collect(),
                        ),
                    ));
                }
                fields.push((
                    "repos".into(),
                    Json::Arr(
                        template
                            .repos
                            .iter()
                            .map(|repo| {
                                Json::Obj(vec![
                                    ("remote".into(), Json::Str(repo.remote.clone())),
                                    ("base".into(), Json::Str(repo.base.clone())),
                                    ("branch".into(), Json::Str(repo.branch.clone())),
                                    (
                                        "sparse".into(),
                                        repo.sparse.as_ref().map_or(Json::Null, |paths| {
                                            Json::Arr(
                                                paths.iter().cloned().map(Json::Str).collect(),
                                            )
                                        }),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ));
                Json::Obj(fields)
            })
            .collect(),
    )
}

/// Whether `value` is an object whose keys are all among `keys`.
fn only_keys(value: &Json, keys: &[&str]) -> bool {
    match value {
        Json::Obj(fields) => fields.iter().all(|(key, _)| keys.contains(&key.as_str())),
        _ => false,
    }
}

/// Where a template's workspace is planned when it is only checked: paths
/// longer than a typical state directory's; the window plans it again
/// where it would be made before saving it.
const CHECK_PATH: usize = 256;

/// `templates_json`'s value back, each template checked as one made in
/// the window is: at most `MAX_TEMPLATES`, each named once, ASCII case
/// aside, its keys only those td-agent writes, its shared directories
/// as `shared_field` takes them, and one naming repositories planned as
/// a workspace would be (`workspace::plan`), shared directories aside.
pub fn templates_from_json(value: &Json) -> Result<Vec<Template>, String> {
    let wrong = "not a list of templates, each a name and its repositories";
    let wrong_shared =
        "a template's shared directories are a list, each a path and whether it may be written";
    let items = value.as_arr().ok_or(wrong)?;
    if items.len() > MAX_TEMPLATES {
        return Err(format!("more than {MAX_TEMPLATES} templates"));
    }
    let mut templates: Vec<Template> = Vec::new();
    let id = crate::store::Id::parse(&"0".repeat(32)).ok_or("no placeholder id")?;
    let place = std::path::PathBuf::from(format!("/{}", "x".repeat(CHECK_PATH)));
    for item in items {
        if !only_keys(item, &["name", "network", "shared", "repos"]) {
            return Err(wrong.into());
        }
        let network = match item.get("network") {
            None => None,
            Some(named) => Some(
                named
                    .as_str()
                    .and_then(Network::parse)
                    .ok_or("a template's network is off, allowlist or open")?,
            ),
        };
        let name = template_name(item.get("name").and_then(Json::as_str).ok_or(wrong)?)?;
        if templates.iter().any(|t| t.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("two templates are named {name:?}"));
        }
        let shared = match item.get("shared") {
            None => None,
            Some(list) => {
                let list = list.as_arr().ok_or(wrong_shared)?;
                if list.len() > MAX_TEMPLATE_SHARED {
                    return Err(format!(
                        "template {name:?} shares more than {MAX_TEMPLATE_SHARED} directories"
                    ));
                }
                let shared = list
                    .iter()
                    .map(|entry| {
                        if !only_keys(entry, &["path", "write"]) {
                            return Err(wrong_shared.to_string());
                        }
                        let path = entry
                            .get("path")
                            .and_then(Json::as_str)
                            .ok_or(wrong_shared)?;
                        let write = entry
                            .get("write")
                            .and_then(Json::as_bool)
                            .ok_or(wrong_shared)?;
                        Ok(Shared {
                            path: configured_path("shared", path)?,
                            write,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()
                    .map_err(|e| format!("template {name:?}: {e}"))?;
                // As the dialog would take them, so the file reads back
                // as written.
                let text = shared_text(Some(&shared));
                if shared_field(&text).ok().flatten().as_ref() != Some(&shared) {
                    return Err(format!(
                        "template {name:?}: shared directories the dialog would not take"
                    ));
                }
                Some(shared)
            }
        };
        let repos = item.get("repos").and_then(Json::as_arr).ok_or(wrong)?;
        if repos.len() > crate::workspace::MAX_ENTRIES {
            return Err(format!(
                "template {name:?} has more than {} repositories",
                crate::workspace::MAX_ENTRIES
            ));
        }
        let repos = repos
            .iter()
            .map(|repo| {
                if !only_keys(repo, &["remote", "base", "branch", "sparse"]) {
                    return Err(wrong.into());
                }
                let text = |key: &str| repo.get(key).and_then(Json::as_str).ok_or(wrong);
                let sparse = match repo.get("sparse") {
                    None | Some(Json::Null) => None,
                    Some(paths) => Some(
                        paths
                            .as_arr()
                            .ok_or(wrong)?
                            .iter()
                            .map(|path| path.as_str().map(str::to_string).ok_or(wrong))
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                };
                let checked =
                    checked_repo(text("remote")?, text("base")?, text("branch")?, sparse)?;
                // As td-agent records it, so the file reads back as written.
                if Some(checked.remote.as_str()) != repo.get("remote").and_then(Json::as_str) {
                    return Err(format!(
                        "{:?} is not a remote as td-agent records one",
                        checked.remote
                    ));
                }
                Ok(checked)
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(|e| format!("template {name:?}: {e}"))?;
        let template = Template {
            network,
            name,
            repos,
            shared,
        };
        if !template.repos.is_empty() {
            crate::workspace::plan(&template, &id, &place, &place, 0)?;
        }
        templates.push(template);
    }
    Ok(templates)
}

impl Template {
    /// Its shared directories as configured, `~` expanded against `home`;
    /// none when it names none of its own.
    pub fn shared(&self, home: &Path) -> Option<Vec<Shared>> {
        self.shared
            .as_ref()
            .map(|shared| expand_shared(shared, home))
    }
}

/// `shared` with `~` expanded against `home`.
fn expand_shared(shared: &[Shared], home: &Path) -> Vec<Shared> {
    shared
        .iter()
        .map(|shared| Shared {
            path: expand(&shared.path, home),
            write: shared.write,
        })
        .collect()
}

/// Where repository workspaces live (DESIGN.md §7).
pub const DEFAULT_WORKSPACE_ROOT: &str = "~/td-agent";

impl Config {
    /// `workspace_root`, `~` expanded against `home`.
    pub fn workspace_root(&self, home: &Path) -> PathBuf {
        expand(
            self.workspace_root
                .as_deref()
                .unwrap_or(Path::new(DEFAULT_WORKSPACE_ROOT)),
            home,
        )
    }

    /// The shared directories as configured, `~` expanded against `home`,
    /// not yet admitted (`workspace::admit_shared`).
    pub fn shared(&self, home: &Path) -> Vec<Shared> {
        expand_shared(&self.shared, home)
    }
}

/// `~` and `~/...` against `home`; any other path as it is.
pub fn expand(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// A configured path: absolute, or under `~`.
fn configured_path(key: &str, text: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(text);
    if text.len() > MAX_NAME || !(path.is_absolute() || path.starts_with("~")) {
        return Err(format!(
            "`{key}` is an absolute path or one under `~`, not {text:?}"
        ));
    }
    Ok(path)
}

/// `$XDG_CONFIG_HOME/td-agent/config`, else `$HOME/.config/td-agent/config`.
pub fn path(
    config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    // A relative XDG_CONFIG_HOME is invalid and ignored, as the XDG base
    // directory rules say, rather than read relative to the directory the
    // program happens to start in.
    let base = td_ui::xdg::dir(
        td_ui::xdg::Base::Config,
        config_home.as_deref(),
        home.as_deref(),
    )?;
    Some(base.join("td-agent").join("config"))
}

/// The configuration at `path`, every default when there is no file.
pub fn load(path: Option<&Path>) -> Result<Config, String> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let bytes = match td_fs::read_bounded(path, MAX_BYTES) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(e.to_string()),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| format!("{}: stream did not contain valid UTF-8", path.display()))?;
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// The configuration a file's text gives.
pub fn parse(text: &str) -> Result<Config, String> {
    let table = td_toml::parse(text).map_err(|e| e.to_string())?;
    let mut config = Config::default();
    for key in table.table_keys() {
        if let Some((_, why)) = REFUSED.iter().find(|(name, _)| *name == key) {
            return Err(format!("`{key}`: {why}"));
        }
        if let Some((_, why)) = RETIRED.iter().find(|(name, _)| *name == key) {
            config
                .notes
                .push(format!("`{key}` is accepted and read no more: {why}"));
            continue;
        }
        match KEYS.iter().find(|(name, _)| *name == key) {
            None => {
                return Err(format!(
                    "unknown key `{key}`; the keys are {}",
                    KEYS.iter()
                        .map(|(name, _)| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
            Some((_, Use::Later(increment))) => config.notes.push(format!(
                "`{key}` is accepted and not read yet: increment {increment} reads it"
            )),
            Some((_, Use::Read)) => {}
        }
    }
    config.mode = match table.optional_str("mode").map_err(|e| e.to_string())? {
        None | Some("auto") => Mode::Auto,
        Some("ask") => Mode::Ask,
        Some(other) => return Err(format!("`mode` is `auto` or `ask`, not {other:?}")),
    };
    let text = |key: &str| table.optional_str(key).map_err(|e| e.to_string());
    let client = &mut config.client;
    if let Some(url) = text("base_url")? {
        client.base_url = base_url(url)?;
    }
    for (key, slot) in [
        ("model", &mut client.model),
        ("title_model", &mut client.title_model),
        ("classifier_fast_model", &mut client.classifier_fast_model),
        ("classifier_model", &mut client.classifier_model),
    ] {
        if let Some(id) = text(key)? {
            *slot = model_id(key, id)?;
        }
    }
    if let Some(word) = text("reasoning_effort")? {
        client.reasoning_effort = effort(word)?;
    }
    config.model_key = text("model")?.map(str::to_string);
    if let Some(word) = text("data_collection")? {
        client.allow_data_collection = data_collection(word)?;
    }
    if let Some(root) = text("workspace_root")? {
        config.workspace_root = Some(configured_path("workspace_root", root)?);
    }
    if let Some(value) = table.get("shared") {
        config.shared = shared_list("shared", value)?;
    }
    if let Some(value) = table.get("template") {
        config.templates = templates(value)?;
    }
    // `open` is the human's alone, on a card or in a template (§10):
    // never every workspace's default.
    match table.optional_str("network").map_err(|e| e.to_string())? {
        None => {}
        Some("off") => config.client.network = Network::Off,
        Some("allowlist") => config.client.network = Network::Allowlist,
        Some("open") => {
            return Err(
                "`network` is `off` or `allowlist`: `open` is set in a template or on a card"
                    .into(),
            )
        }
        Some(other) => return Err(format!("`network` is `off` or `allowlist`, not {other:?}")),
    }
    if let Some(value) = table.get("network_allowlist") {
        let items: Option<Vec<String>> = value.as_arr().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect()
        });
        config.client.network_allowlist = allowlist(items.as_deref())?;
    }
    if let Some(value) = table.get("protected_branches") {
        let items: Option<Vec<String>> = value.as_arr().and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect()
        });
        config.client.protected_branches = protected_branches(items.as_deref())?;
    }
    if let Some(value) = table.get("remotes") {
        let wrong = "`remotes` is a list of remotes, each a URL or a host with a path prefix";
        config.remotes = value
            .as_arr()
            .ok_or(wrong)?
            .iter()
            .map(|item| {
                let text = item.as_str().ok_or(wrong)?;
                crate::git::Admission::parse(text).map_err(|e| format!("`remotes`: {e}"))
            })
            .collect::<Result<_, String>>()?;
    }
    if let Some(value) = table.get("fetch_interval") {
        config.fetch_interval = Some(
            match value {
                Toml::Int(seconds) => u64::try_from(*seconds).ok(),
                _ => None,
            }
            .filter(|seconds| FETCH_INTERVAL.contains(seconds))
            .ok_or_else(|| {
                format!(
                    "`fetch_interval` is a whole number of seconds from {} to {}, not {value:?}",
                    FETCH_INTERVAL.start(),
                    FETCH_INTERVAL.end()
                )
            })?,
        );
    }
    if let Some(value) = table.get("jev_threshold") {
        config.client.jev_threshold = jev_threshold(value)?;
    }
    if let Some(value) = table.get("jev_required") {
        config.client.jev_required = match value {
            Toml::Bool(required) => *required,
            _ => {
                return Err(format!(
                    "`jev_required` is `true` or `false`, not {value:?}"
                ))
            }
        };
    }
    if let Some(value) = table.get("auto_compact") {
        config.client.auto_compact = match value {
            Toml::Bool(on) => *on,
            _ => {
                return Err(format!(
                    "`auto_compact` is `true` or `false`, not {value:?}"
                ))
            }
        };
    }
    if let Some(value) = table.get("compact_at") {
        config.client.compact_at = compact_at(value)?;
    }
    if let Some(value) = table.get("compact_keep_tokens") {
        config.client.compact_keep_tokens = match value {
            Toml::Int(n) => u64::try_from(*n).ok(),
            _ => None,
        }
        .filter(|n| COMPACT_KEEP_TOKENS.contains(n))
        .ok_or_else(|| {
            format!(
                "`compact_keep_tokens` is a whole number from {} to {}, not {value:?}",
                COMPACT_KEEP_TOKENS.start(),
                COMPACT_KEEP_TOKENS.end()
            )
        })?;
    }
    if let Some(id) = table
        .optional_str("compact_model")
        .map_err(|e| e.to_string())?
    {
        config.client.compact_model = Some(model_id("compact_model", id)?);
    }
    if let Some(value) = table.get("cache_ttl") {
        config.client.cache_ttl = match value {
            Toml::Int(n) => u64::try_from(*n).ok(),
            _ => None,
        }
        .filter(|n| CACHE_TTL.contains(n))
        .ok_or_else(|| {
            format!(
                "`cache_ttl` is a whole number of seconds from {} to {}, not {value:?}",
                CACHE_TTL.start(),
                CACHE_TTL.end()
            )
        })?;
    }
    if let Some(value) = table.get("cold_resume_tokens") {
        config.client.cold_resume_tokens = match value {
            Toml::Str(none) if none == "none" => None,
            Toml::Int(n) => Some(
                u64::try_from(*n)
                    .ok()
                    .filter(|n| COLD_RESUME_TOKENS.contains(n))
                    .ok_or_else(|| cold_resume_refused(value))?,
            ),
            _ => return Err(cold_resume_refused(value)),
        };
    }
    if let Some(value) = table.get("max_background") {
        config.client.max_background = match value {
            Toml::Int(n) => u32::try_from(*n).ok(),
            _ => None,
        }
        .filter(|n| MAX_BACKGROUND.contains(n))
        .ok_or_else(|| {
            format!(
                "`max_background` is a whole number from {} to {}, not {value:?}",
                MAX_BACKGROUND.start(),
                MAX_BACKGROUND.end()
            )
        })?;
    }
    if let Some(value) = table.get("background_output_bytes") {
        config.client.background_output_bytes = match value {
            Toml::Int(n) => u64::try_from(*n).ok(),
            _ => None,
        }
        .filter(|n| BACKGROUND_OUTPUT_BYTES.contains(n))
        .ok_or_else(|| {
            format!(
                "`background_output_bytes` is a whole number of bytes from {} to {}, not {value:?}",
                BACKGROUND_OUTPUT_BYTES.start(),
                BACKGROUND_OUTPUT_BYTES.end()
            )
        })?;
    }
    let client = &mut config.client;
    for (key, slot) in [
        ("max_cost_per_turn", &mut client.limits.turn),
        ("max_cost_per_conversation", &mut client.limits.conversation),
        ("max_cost_per_day", &mut client.limits.day),
    ] {
        if let Some(value) = table.get(key) {
            *slot = limit(key, value)?;
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn an_empty_or_missing_configuration_is_every_default() {
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(Config::default().mode, Mode::Auto);
        let client = Config::default().client;
        assert_eq!(client.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(client.reasoning_effort, "medium");
        assert!(!client.allow_data_collection);
        assert_eq!(
            client.limits,
            Limits {
                turn: Some(5 * cost::ONE),
                conversation: Some(10 * cost::ONE),
                day: Some(25 * cost::ONE)
            }
        );
        assert_eq!(load(None).unwrap(), Config::default());
        assert_eq!(
            load(Some(Path::new("/nonexistent/td-agent/config"))).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn the_mode_is_read_and_checked() {
        assert_eq!(parse("mode = \"ask\"").unwrap().mode, Mode::Ask);
        assert_eq!(parse("mode = \"auto\"").unwrap().mode, Mode::Auto);
        let e = parse("mode = \"yolo\"").unwrap_err();
        assert!(e.contains("`mode`"), "{e}");
        assert!(parse("mode = 1").unwrap_err().contains("mode"));
    }

    /// A retired key loads with a note, whatever its value, and sets
    /// nothing.
    #[test]
    fn a_retired_key_is_noted_and_not_read() {
        let config = parse("orchestrator_model = \"c/d\"\nmodel = \"a/b\"\n").unwrap();
        assert_eq!(config.client.model, "a/b");
        assert_eq!(config.notes.len(), 1);
        assert!(config
            .notes
            .first()
            .unwrap()
            .starts_with("`orchestrator_model` is accepted and read no more"));
        assert_eq!(parse("orchestrator_model = 1\n").unwrap().notes.len(), 1);
    }

    #[test]
    fn unknown_keys_and_limits_are_refused_by_name() {
        let e = parse("modle = \"auto\"").unwrap_err();
        assert!(e.starts_with("unknown key `modle`"), "{e}");
        let e = parse("limits = 1").unwrap_err();
        assert!(e.contains("`limits`") && e.contains("§8"), "{e}");
        let e = parse("[limits]\nmemory = 1").unwrap_err();
        assert!(e.contains("`limits`"), "{e}");
    }

    /// The design's example parses, every key it sets accepted and each
    /// one a later increment reads said.
    #[test]
    fn workspaces_and_shared_directories_are_configured() {
        let home = Path::new("/home/u");
        let config = parse("").unwrap();
        assert_eq!(config.workspace_root(home), Path::new("/home/u/td-agent"));
        assert!(config.shared(home).is_empty());
        let config =
            parse("workspace_root = \"/srv/ws\"\n[[shared]]\npath = \"~\"\nwrite = true\n")
                .unwrap();
        assert_eq!(config.workspace_root(home), Path::new("/srv/ws"));
        assert_eq!(
            config.shared(home),
            [Shared {
                path: "/home/u".into(),
                write: true
            }]
        );
        assert!(parse("shared = []").unwrap().shared(home).is_empty());
        for bad in [
            "workspace_root = \"rel\"",
            "workspace_root = 3",
            "shared = 3",
            "[shared]\npath = \"/a\"",
            "[[shared]]\nwrite = true",
            "[[shared]]\npath = \"rel\"",
            "[[shared]]\npath = \"/a\"\nwrite = \"yes\"",
            "[[shared]]\npath = \"/a\"\nmode = \"ro\"",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let client = Client {
            shared: vec![Shared {
                path: "/d".into(),
                write: true,
            }],
            ..Client::default()
        };
        assert_eq!(Client::from_json(&client.to_json()).unwrap(), client);
    }

    #[test]
    fn remotes_are_read_as_admissions() {
        let config = parse("remotes = [\"github.com/timmydo\", \"git@example.org:a/b\"]").unwrap();
        let remote = |text| crate::git::Remote::parse(text).unwrap();
        assert_eq!(config.remotes.len(), 2);
        assert!(config
            .remotes
            .first()
            .unwrap()
            .admits(&remote("https://github.com/timmydo/td")));
        assert!(config
            .remotes
            .get(1)
            .unwrap()
            .admits(&remote("git@example.org:a/b.git")));
        for (text, why) in [
            ("remotes = \"github.com\"", "a list"),
            ("remotes = [1]", "a list"),
            ("remotes = [\"http://github.com/a\"]", "`remotes`"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(why), "{text}: {e}");
        }
        assert!(parse("").unwrap().remotes.is_empty());
    }

    #[test]
    fn every_listed_key_is_accepted_and_the_unread_ones_said() {
        // DESIGN.md §15's example, whole.
        let example = "model = \"anthropic/claude-sonnet-5.5\"\nmode = \"auto\"\n\
                       max_cost_per_day = 25\n\n[[shared]]\npath = \"~/Downloads\"\n\n\
                       [[shared]]\npath = \"~/src/reference\"\nwrite = false\n\n\
                       [[template]]\nname = \"td\"\n\n[[template.repos]]\n\
                       remote = \"https://github.com/timmydo/td\"\nbase = \"main\"\n\
                       branch = \"agent\"\nsparse = [\"td-agent\", \"td-ui\"]\n";
        let config = parse(example).unwrap();
        assert_eq!(config.mode, Mode::Auto);
        assert!(config.notes.is_empty(), "{:?}", config.notes);
        assert_eq!(
            config.templates,
            [Template {
                network: None,
                name: "td".into(),
                repos: vec![Repo {
                    remote: "https://github.com/timmydo/td".into(),
                    base: "main".into(),
                    branch: "agent".into(),
                    sparse: Some(vec!["td-agent".into(), "td-ui".into()]),
                }],
                shared: None,
            }]
        );
        assert_eq!(
            config.shared(Path::new("/home/u")),
            [
                Shared {
                    path: "/home/u/Downloads".into(),
                    write: false
                },
                Shared {
                    path: "/home/u/src/reference".into(),
                    write: false
                }
            ]
        );
        assert_eq!(config.client.model, "anthropic/claude-sonnet-5.5");
        assert_eq!(config.client.limits.day, Some(25 * cost::ONE));
        for (key, use_) in KEYS {
            // A table is no key's value: each read key refuses it.
            let config = parse(&format!("[{key}]\nx = 1")).map(|c| c.notes);
            match use_ {
                Use::Read => assert!(config.is_err(), "{key} is checked"),
                Use::Later(n) => assert_eq!(
                    config.unwrap(),
                    [format!(
                        "`{key}` is accepted and not read yet: increment {n} reads it"
                    )]
                ),
            }
        }
    }

    /// The classifier's keys (DESIGN.md §11): two models, a threshold
    /// in thousandths from 0.5 to 1 and unset by default, and whether Jev
    /// is required, `true` by default; each crosses to a conversation.
    #[test]
    fn the_classifiers_keys_are_read_and_checked() {
        let client = Config::default().client;
        assert_eq!(client.classifier_fast_model, "typesafe/jev-1.13");
        assert_eq!(client.classifier_model, "openai/gpt-oss-safeguard-20b");
        assert_eq!(client.jev_threshold, 775);
        assert!(client.jev_required);
        let config = parse(
            "classifier_fast_model = \"~typesafe/jev-latest\"\nclassifier_model = \"m/safe\"\n\
             jev_threshold = 0.925\njev_required = false\n",
        )
        .unwrap();
        assert!(config.notes.is_empty(), "{:?}", config.notes);
        let client = config.client;
        assert_eq!(client.classifier_fast_model, "~typesafe/jev-latest");
        assert_eq!(client.classifier_model, "m/safe");
        assert_eq!(client.jev_threshold, 925);
        assert!(!client.jev_required);
        assert_eq!(Client::from_json(&client.to_json()).unwrap(), client);
        for (text, thousandths) in [("0.5", 500), ("1", 1000), ("1.0", 1000), ("0.999", 999)] {
            assert_eq!(
                parse(&format!("jev_threshold = {text}"))
                    .unwrap()
                    .client
                    .jev_threshold,
                thousandths,
                "{text}"
            );
        }
        for text in [
            "0.4999", "0.49", "1.001", "2", "0", "0.9255", "\"0.9\"", "true",
        ] {
            let refused = parse(&format!("jev_threshold = {text}")).unwrap_err();
            assert!(
                refused.contains("`jev_threshold` is a probability"),
                "{text}: {refused}"
            );
        }
        assert!(parse("jev_required = \"no\"")
            .unwrap_err()
            .contains("`jev_required` is `true` or `false`"));
        assert!(parse("classifier_model = \"\"").is_err());
        let mut value = client.to_json();
        value.insert("jev_threshold", 400u64);
        assert!(Client::from_json(&value).is_err());
        value.insert("jev_threshold", Json::Null);
        assert_eq!(
            Client::from_json(&value).unwrap_err(),
            "jev_threshold is not a whole number"
        );
    }

    #[test]
    fn compaction_is_configured_by_its_share_of_the_context() {
        let client = Config::default().client;
        assert!(client.auto_compact);
        assert_eq!(client.compact_at, 80);
        let client = parse("auto_compact = false\ncompact_at = 0.65\n")
            .unwrap()
            .client;
        assert!(!client.auto_compact);
        assert_eq!(client.compact_at, 65);
        assert_eq!(Client::from_json(&client.to_json()).unwrap(), client);
        // A setup without them, from a window before them, has the
        // defaults.
        let mut older = client.to_json();
        older.remove("auto_compact");
        older.remove("compact_at");
        let older = Client::from_json(&older).unwrap();
        assert!(older.auto_compact);
        assert_eq!(older.compact_at, 80);
        let default = Config::default().client;
        assert_eq!(
            (default.compact_keep_tokens, default.compact_model),
            (20_000, None)
        );
        let client = parse("compact_keep_tokens = 8000\ncompact_model = \"a/cheap\"\n")
            .unwrap()
            .client;
        assert_eq!(client.compact_keep_tokens, 8000);
        assert_eq!(client.compact_model.as_deref(), Some("a/cheap"));
        assert_eq!(Client::from_json(&client.to_json()).unwrap(), client);
        assert!(client.wanted().contains(&"a/cheap".to_string()));
        // Resuming cold: defaults, a value, `none`, carried, and an older
        // client's absent keys defaulted.
        assert_eq!(
            (default.cache_ttl, default.cold_resume_tokens),
            (300, Some(32_000))
        );
        let client = parse("cache_ttl = 0\ncold_resume_tokens = 50000\n")
            .unwrap()
            .client;
        assert_eq!(
            (client.cache_ttl, client.cold_resume_tokens),
            (0, Some(50_000))
        );
        assert_eq!(Client::from_json(&client.to_json()).unwrap(), client);
        let never = parse("cold_resume_tokens = \"none\"\n").unwrap().client;
        assert_eq!(never.cold_resume_tokens, None);
        assert_eq!(Client::from_json(&never.to_json()).unwrap(), never);
        let mut older = never.to_json();
        older.remove("cache_ttl");
        older.remove("cold_resume_tokens");
        let older = Client::from_json(&older).unwrap();
        assert_eq!(
            (older.cache_ttl, older.cold_resume_tokens),
            (300, Some(32_000))
        );
        for (text, why) in [
            (
                "compact_keep_tokens = 999",
                "`compact_keep_tokens` is a whole number",
            ),
            (
                "compact_keep_tokens = \"9\"",
                "`compact_keep_tokens` is a whole number",
            ),
            ("compact_model = \"a b\"", "`compact_model` must be"),
            (
                "cache_ttl = 86401",
                "`cache_ttl` is a whole number of seconds",
            ),
            (
                "cache_ttl = \"300\"",
                "`cache_ttl` is a whole number of seconds",
            ),
            (
                "cold_resume_tokens = 999",
                "`cold_resume_tokens` is a whole number",
            ),
            (
                "cold_resume_tokens = \"never\"",
                "`cold_resume_tokens` is a whole number",
            ),
        ] {
            let refused = parse(text).unwrap_err();
            assert!(refused.contains(why), "{text}: {refused}");
        }
        for (text, percent) in [("1", 100), ("1.0", 100), ("0.1", 10), ("0.95", 95)] {
            assert_eq!(
                parse(&format!("compact_at = {text}"))
                    .unwrap()
                    .client
                    .compact_at,
                percent,
                "{text}"
            );
        }
        for text in ["0.05", "0.805", "1.01", "80", "0", "\"0.8\"", "true"] {
            let refused = parse(&format!("compact_at = {text}")).unwrap_err();
            assert!(
                refused.contains("`compact_at` is a share of the context"),
                "{text}: {refused}"
            );
        }
        assert!(parse("auto_compact = 1")
            .unwrap_err()
            .contains("`auto_compact` is `true` or `false`"));
    }

    #[test]
    fn the_fetch_interval_is_whole_seconds_within_its_bounds() {
        assert_eq!(
            Config::default().fetch_interval(),
            std::time::Duration::from_secs(600)
        );
        for (text, seconds) in [("60", 60), ("3600", 3600), ("86400", 86_400)] {
            let config = parse(&format!("fetch_interval = {text}")).unwrap();
            assert_eq!(
                config.fetch_interval(),
                std::time::Duration::from_secs(seconds)
            );
            assert!(config.notes.is_empty(), "{:?}", config.notes);
        }
        for text in ["59", "86401", "-1", "600.5", "\"10m\""] {
            let refused = parse(&format!("fetch_interval = {text}")).unwrap_err();
            assert!(refused.contains("whole number of seconds"), "{refused}");
        }
    }

    #[test]
    fn background_output_bytes_are_bounded_and_cross_whole() {
        assert_eq!(Config::default().client.background_output_bytes, 16 << 20);
        for (text, bytes) in [("65536", 65_536), ("1073741824", 1 << 30)] {
            let config = parse(&format!("background_output_bytes = {text}")).unwrap();
            assert_eq!(config.client.background_output_bytes, bytes);
            assert!(config.notes.is_empty(), "{:?}", config.notes);
            let client = &config.client;
            assert_eq!(&Client::from_json(&client.to_json()).unwrap(), client);
        }
        for text in ["65535", "1073741825", "-1", "\"16M\""] {
            let refused = parse(&format!("background_output_bytes = {text}")).unwrap_err();
            assert!(refused.contains("`background_output_bytes`"), "{refused}");
        }
    }

    /// A template made in the window keeps the network it names, none
    /// writing no key, and a network that is none of the three is
    /// refused.
    #[test]
    fn a_template_made_in_the_window_keeps_its_network() {
        let repo = checked_repo("/srv/td", "main", "agent", None).unwrap();
        let templates: Vec<Template> = [
            None,
            Some(Network::Off),
            Some(Network::Allowlist),
            Some(Network::Open),
        ]
        .into_iter()
        .enumerate()
        .map(|(n, network)| Template {
            network,
            name: format!("t{n}"),
            repos: vec![repo.clone()],
            shared: None,
        })
        .collect();
        let written = templates_json(&templates);
        assert_eq!(templates_from_json(&written).unwrap(), templates);
        // None names no policy: the key is left out.
        assert!(
            !written.to_string().contains(r#""name":"t0","network""#),
            "{written}"
        );
        assert!(
            written.to_string().contains(r#""network":"open""#),
            "{written}"
        );
        let wrong = td_json::parse(
            &written
                .to_string()
                .replace(r#""network":"open""#, r#""network":"anywhere""#),
        )
        .unwrap();
        assert!(templates_from_json(&wrong)
            .unwrap_err()
            .contains("off, allowlist or open"));
    }

    /// The dialog's shared folders: paths parted by spaces, read-only
    /// unless one ends `:rw`, each absolute or under `~`, none named twice
    /// and at most `MAX_TEMPLATE_SHARED`; an empty field is none, so the
    /// top-level list; and the text reads back as it was given.
    #[test]
    fn a_templates_shared_folders_are_parsed_from_the_dialog() {
        assert_eq!(shared_field("  ").unwrap(), None);
        let shared = shared_field("~/notes  /srv/out:rw").unwrap().unwrap();
        assert_eq!(
            shared,
            [
                Shared {
                    path: "~/notes".into(),
                    write: false
                },
                Shared {
                    path: "/srv/out".into(),
                    write: true
                }
            ]
        );
        assert_eq!(shared_text(Some(&shared)), "~/notes /srv/out:rw");
        assert_eq!(shared_text(None), "");
        for (text, why) in [
            ("notes", "absolute path"),
            ("~/a ~/a:rw", "named twice"),
            ("~/a\u{1b}b", "control character"),
            (":rw", "absolute path"),
        ] {
            let e = shared_field(text).unwrap_err();
            assert!(e.contains(why), "{text}: {e}");
        }
        let many = vec!["/x"; MAX_TEMPLATE_SHARED + 1]
            .iter()
            .enumerate()
            .map(|(n, p)| format!("{p}{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(shared_field(&many).unwrap_err().contains("at most"));
    }

    /// A template made in the window may name no repository, making a
    /// scratch workspace, and may name its own shared folders, which its
    /// file keeps; folders the dialog would not take back as written are
    /// refused, as is a list past its bound.
    #[test]
    fn a_scratch_template_made_in_the_window_keeps_its_shared_folders() {
        let templates = vec![
            Template {
                network: Some(Network::Off),
                name: "system".into(),
                repos: Vec::new(),
                shared: Some(vec![Shared {
                    path: "~/notes".into(),
                    write: true,
                }]),
            },
            Template {
                network: None,
                name: "plain".into(),
                repos: Vec::new(),
                shared: None,
            },
        ];
        let written = templates_json(&templates);
        assert_eq!(templates_from_json(&written).unwrap(), templates);
        assert!(
            !written.to_string().contains(r#""name":"plain","shared""#),
            "{written}"
        );
        for (from, to, why) in [
            (r#""~/notes""#, r#""notes""#, "absolute path"),
            (r#""~/notes""#, r#""~/my notes""#, "dialog would not take"),
            (
                r#""write":true"#,
                r#""write":1"#,
                "whether it may be written",
            ),
            (
                r#""write":true"#,
                r#""write":true,"x":1"#,
                "whether it may be written",
            ),
        ] {
            let wrong = td_json::parse(&written.to_string().replacen(from, to, 1)).unwrap();
            let e = templates_from_json(&wrong).unwrap_err();
            assert!(e.contains(why), "{to}: {e}");
        }
        let many = Template {
            shared: Some(
                (0..=MAX_TEMPLATE_SHARED)
                    .map(|n| Shared {
                        path: format!("/x{n}").into(),
                        write: false,
                    })
                    .collect(),
            ),
            ..templates.first().unwrap().clone()
        };
        let e = templates_from_json(&templates_json(&[many])).unwrap_err();
        assert!(e.contains("more than 16"), "{e}");
    }

    /// A template made in the window is checked as preparing it would
    /// check it, its remote recorded as td-agent records one, and reads
    /// back from its file as written; a file td-agent would not have
    /// written is refused whole.
    #[test]
    fn templates_made_in_the_window_are_checked_and_read_back() {
        let repo = checked_repo("git@Example.org:/a/td.git", "main", "agent", None).unwrap();
        assert_eq!(repo.remote, "ssh://git@example.org/a/td.git");
        let local = checked_repo(
            "/srv/git/td",
            "main",
            "agent",
            Some(vec!["td-agent".into()]),
        )
        .unwrap();
        assert_eq!(local.remote, "file:///srv/git/td");
        for (remote, base, branch, sparse, why) in [
            ("http://h/a", "main", "a", None, "no other transport"),
            ("/srv/td", "-main", "a", None, "the base"),
            ("/srv/td", "main", "refs/heads/a", None, "the branch"),
            (
                "/srv/td",
                "main",
                "a",
                Some(vec!["../x".to_string()]),
                "sparse",
            ),
            (
                "/srv/td",
                "main",
                "a",
                Some(vec!["/x".to_string()]),
                "sparse",
            ),
            // What the checkout's cone refuses, which `sparse_path` takes.
            (
                "/srv/td",
                "main",
                "a",
                Some(vec!["./x".to_string()]),
                "checks out",
            ),
            (
                "/srv/td",
                "main",
                "a",
                Some(vec!["x*".to_string()]),
                "checks out",
            ),
            (
                "/srv/td",
                "main",
                "a",
                Some(vec!["a//b".to_string()]),
                "checks out",
            ),
            (
                "/srv/td",
                "main",
                "a",
                Some(vec!["x".to_string(); MAX_SPARSE + 1]),
                "sparse paths",
            ),
        ] {
            let e = checked_repo(remote, base, branch, sparse).unwrap_err();
            assert!(e.contains(why), "{remote} {base} {branch}: {e}");
        }
        let templates = vec![
            Template {
                network: None,
                name: "td".into(),
                repos: vec![repo.clone(), local.clone()],
                shared: None,
            },
            Template {
                network: None,
                name: "notes".into(),
                repos: vec![local],
                shared: None,
            },
        ];
        let value = templates_json(&templates);
        assert_eq!(templates_from_json(&value).unwrap(), templates);
        let one = |name: &str, remote: &str| {
            Json::Arr(vec![Json::Obj(vec![
                ("name".into(), Json::Str(name.into())),
                (
                    "repos".into(),
                    Json::Arr(vec![Json::Obj(vec![
                        ("remote".into(), Json::Str(remote.into())),
                        ("base".into(), Json::Str("main".into())),
                        ("branch".into(), Json::Str("a".into())),
                    ])]),
                ),
            ])])
        };
        templates_from_json(&one("td", "file:///srv/td")).unwrap();
        for (value, why) in [
            (one("td", "/srv/td"), "as td-agent records"),
            (one("Empty", "file:///srv/td"), "no template may be named"),
            (one(" td", "file:///srv/td"), "visible text"),
            (Json::Str("x".into()), "not a list"),
        ] {
            let e = templates_from_json(&value).unwrap_err();
            assert!(e.contains(why), "{why}: {e}");
        }
        let mut twice = templates_json(templates.get(..1).unwrap());
        if let (Json::Arr(items), Json::Arr(more)) = (
            &mut twice,
            templates_json(&[Template {
                network: None,
                name: "TD".into(),
                ..templates.get(1).unwrap().clone()
            }]),
        ) {
            items.extend(more);
        }
        assert!(templates_from_json(&twice)
            .unwrap_err()
            .contains("two templates"));
        let many = Json::Arr(
            (0..=MAX_TEMPLATES)
                .map(|n| match one(&format!("t{n}"), "file:///srv/td") {
                    Json::Arr(mut items) => items.remove(0),
                    other => other,
                })
                .collect(),
        );
        assert!(templates_from_json(&many)
            .unwrap_err()
            .contains("more than"));
        // Only the keys td-agent writes.
        let mut extra = one("td", "file:///srv/td");
        if let Json::Arr(items) = &mut extra {
            if let Some(Json::Obj(fields)) = items.first_mut() {
                fields.push(("since".into(), Json::Arr(Vec::new())));
            }
        }
        assert!(templates_from_json(&extra)
            .unwrap_err()
            .contains("not a list"));
        let mut extra = one("td", "file:///srv/td");
        if let Json::Arr(items) = &mut extra {
            if let Some(Json::Obj(fields)) = items.first_mut() {
                if let Some((_, Json::Arr(repos))) = fields.get_mut(1) {
                    if let Some(Json::Obj(repo)) = repos.first_mut() {
                        repo.push(("network".into(), Json::Null));
                    }
                }
            }
        }
        assert!(templates_from_json(&extra)
            .unwrap_err()
            .contains("not a list"));
        // Planned as a workspace: one branch of a remote named twice, and
        // a record past its bound, refused; no sparse path reads back.
        let twice = Template {
            network: None,
            name: "td".into(),
            repos: vec![repo.clone(), repo.clone()],
            shared: None,
        };
        assert!(templates_from_json(&templates_json(&[twice]))
            .unwrap_err()
            .contains("twice"));
        let long = checked_repo(
            "/srv/td",
            "main",
            "a",
            Some(
                (0..MAX_SPARSE)
                    .map(|n| format!("{n}{}", "x".repeat(300)))
                    .collect(),
            ),
        )
        .unwrap();
        let long = Template {
            network: None,
            name: "td".into(),
            repos: vec![long],
            shared: None,
        };
        assert!(templates_from_json(&templates_json(&[long]))
            .unwrap_err()
            .contains("fewer sparse paths"));
        let none = vec![Template {
            network: None,
            name: "td".into(),
            repos: vec![checked_repo("/srv/td", "main", "a", Some(Vec::new())).unwrap()],
            shared: None,
        }];
        assert_eq!(templates_from_json(&templates_json(&none)).unwrap(), none);
    }

    /// `protected_branches` names branches a push could, each once, and
    /// crosses whole; `main` and `master` when left out.
    #[test]
    fn protected_branches_are_branches_and_cross_whole() {
        assert_eq!(
            Config::default().client.protected_branches,
            ["main", "master"]
        );
        let config = parse("protected_branches = [\"release\", \"main\", \"release\"]").unwrap();
        assert_eq!(config.client.protected_branches, ["release", "main"]);
        assert!(config.notes.is_empty(), "{:?}", config.notes);
        let client = &config.client;
        assert_eq!(&Client::from_json(&client.to_json()).unwrap(), client);
        assert!(parse("protected_branches = []")
            .unwrap()
            .client
            .protected_branches
            .is_empty());
        for text in ["\"main\"", "[1]", "[\"-x\"]", "[\"refs/heads/main\"]"] {
            let refused = parse(&format!("protected_branches = {text}")).unwrap_err();
            assert!(refused.contains("`protected_branches`"), "{refused}");
        }
        let many: Vec<String> = (0..=MAX_PROTECTED).map(|n| format!("\"b{n}\"")).collect();
        assert!(parse(&format!("protected_branches = [{}]", many.join(", "))).is_err());
        let mut value = Client::default().to_json();
        if let Json::Obj(pairs) = &mut value {
            for (name, branches) in pairs.iter_mut() {
                if name == "protected_branches" {
                    *branches = Json::Arr(vec![Json::Str("-x".into())]);
                }
            }
        }
        assert!(Client::from_json(&value).is_err());
        let mut older = config.client.to_json();
        older.remove("protected_branches");
        assert_eq!(
            Client::from_json(&older).unwrap().protected_branches,
            ["main", "master"]
        );
    }

    #[test]
    fn max_background_is_a_count_within_its_bounds_and_crosses_whole() {
        assert_eq!(Config::default().client.max_background, 4);
        for (text, most) in [("1", 1), ("16", 16)] {
            let config = parse(&format!("max_background = {text}")).unwrap();
            assert_eq!(config.client.max_background, most);
            assert!(config.notes.is_empty(), "{:?}", config.notes);
            let client = &config.client;
            assert_eq!(&Client::from_json(&client.to_json()).unwrap(), client);
        }
        for text in ["0", "17", "-1", "2.5", "\"4\""] {
            let refused = parse(&format!("max_background = {text}")).unwrap_err();
            assert!(refused.contains("`max_background`"), "{refused}");
        }
        let mut value = Client::default().to_json();
        if let Json::Obj(pairs) = &mut value {
            for (name, n) in pairs.iter_mut() {
                if name == "max_background" {
                    *n = Json::from(17u64);
                }
            }
        }
        assert!(Client::from_json(&value).is_err());
    }

    #[test]
    fn the_model_clients_keys_are_read_and_checked() {
        let config = parse(
            "base_url = \"https://example.test/api/v1/\"\nmodel = \"a/b\"\n\
             title_model = \"e/f\"\n\
             reasoning_effort = \"high\"\ndata_collection = \"allow\"\n\
             max_cost_per_turn = 0.5\nmax_cost_per_conversation = \"none\"\n\
             max_cost_per_day = 0\n",
        )
        .unwrap();
        // The key as written, which a window default is set over; none
        // when left out, whatever the built-in default.
        assert_eq!(config.model_key.as_deref(), Some("a/b"));
        assert_eq!(parse("mode = \"ask\"\n").unwrap().model_key, None);
        let client = &config.client;
        assert_eq!(client.base_url, "https://example.test/api/v1");
        assert_eq!(client.model, "a/b");
        assert_eq!(client.title_model, "e/f");
        assert_eq!(client.reasoning_effort, "high");
        assert!(client.allow_data_collection);
        assert_eq!(
            client.limits,
            Limits {
                turn: Some(cost::ONE / 2),
                conversation: None,
                day: Some(0)
            }
        );
        // The settings cross the socketpair and come back the same.
        assert_eq!(&Client::from_json(&client.to_json()).unwrap(), client);
        for (line, said) in [
            ("base_url = \"http://openrouter.ai/api/v1\"", "https://"),
            ("base_url = \"https://\"", "https:// URL"),
            ("base_url = \"https://h h\"", "with a host"),
            ("base_url = \"https://h/x?y\"", "no query"),
            ("model = \"\"", "`model` must be a model id"),
            ("title_model = \"a b\"", "`title_model` must be"),
            ("reasoning_effort = \"max\"", "`reasoning_effort` is one of"),
            ("data_collection = \"maybe\"", "`data_collection` is"),
            ("max_cost_per_turn = -1", "`max_cost_per_turn` is a number"),
            ("max_cost_per_day = -0.5", "`max_cost_per_day` is a number"),
            (
                "max_cost_per_day = \"lots\"",
                "`max_cost_per_day` is a number",
            ),
            ("max_cost_per_day = true", "`max_cost_per_day` is a number"),
            (
                "max_cost_per_turn = 1e30",
                "`max_cost_per_turn` is a number",
            ),
        ] {
            let e = parse(line).unwrap_err();
            assert!(e.contains(said), "{line}: {e}");
        }
    }

    #[test]
    fn the_path_follows_xdg() {
        assert_eq!(
            path(Some("/c".into()), Some("/h".into())),
            Some(PathBuf::from("/c/td-agent/config"))
        );
        assert_eq!(
            path(None, Some("/h".into())),
            Some(PathBuf::from("/h/.config/td-agent/config"))
        );
        assert_eq!(path(Some("rel".into()), None), None);
        // A relative XDG_CONFIG_HOME is ignored for HOME's.
        assert_eq!(
            path(Some("rel".into()), Some("/h".into())),
            Some(PathBuf::from("/h/.config/td-agent/config"))
        );
        assert_eq!(path(None, None), None);
    }

    /// `network` is `off` or `allowlist`, `open` being a template's or
    /// a card's; `network_allowlist` is hosts with optional ports, the
    /// shipped download hosts by default; both cross to a conversation
    /// whole, a template's own policy beside them; and a workspace's
    /// policy is its template's, else the top level's, and `off` for a
    /// template no longer configured.
    #[test]
    fn the_network_policy_and_allowlist_are_read_and_cross_whole() {
        let config = Config::default();
        assert_eq!(config.client.network, Network::Allowlist);
        assert_eq!(
            config
                .client
                .network_allowlist
                .iter()
                .map(Destination::text)
                .collect::<Vec<_>>(),
            DEFAULT_ALLOWLIST
        );
        assert!(!DEFAULT_ALLOWLIST.iter().any(|host| [
            "github.com",
            "crates.io",
            "registry.npmjs.org"
        ]
        .contains(host)));
        let config = parse(
            "network = \"off\"\nnetwork_allowlist = [\"Example.COM.\", \"example.com:443\", \
             \"git.example.org:8443\", \"192.0.2.7:80\", \"[2001:db8::1]\", \"[2001:db8::2]:22\"]\n\
             [[template]]\nname = \"open\"\nnetwork = \"open\"\n\
             [[template]]\nname = \"plain\"\n",
        )
        .unwrap();
        assert_eq!(config.client.network, Network::Off);
        assert_eq!(
            config
                .client
                .network_allowlist
                .iter()
                .map(Destination::text)
                .collect::<Vec<_>>(),
            [
                "example.com",
                "git.example.org:8443",
                "192.0.2.7:80",
                "[2001:db8::1]",
                "[2001:db8::2]:22"
            ]
        );
        assert_eq!(
            config.templates.first().unwrap().network,
            Some(Network::Open)
        );
        for (text, said) in [
            ("network = \"open\"", "set in a template or on a card"),
            ("network = \"on\"", "`off` or `allowlist`"),
            ("network_allowlist = \"a\"", "a list of at most"),
            ("network_allowlist = [\"a:0\"]", "not a host"),
            ("network_allowlist = [\"a:+1\"]", "not a host"),
            ("network_allowlist = [\"a_b\"]", "not a host"),
            ("network_allowlist = [\"[1.2.3.4]\"]", "not a host"),
            ("network_allowlist = [\"*.example.com\"]", "not a host"),
            (
                "[[template]]\nname = \"t\"\nnetwork = \"on\"",
                "off, allowlist or open",
            ),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(said), "{text}: {e}");
        }
        let many = (0..=MAX_ALLOWLIST)
            .map(|n| format!("\"h{n}.example\""))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(parse(&format!("network_allowlist = [{many}]")).is_err());
        let most = (0..MAX_ALLOWLIST)
            .map(|n| format!("\"h{n}.example\""))
            .collect::<Vec<_>>()
            .join(", ");
        let config_most = parse(&format!("network_allowlist = [{most}]")).unwrap();
        assert_eq!(config_most.client.network_allowlist.len(), MAX_ALLOWLIST);
        assert!(parse("[[template]]\nname = \"t\"\nnetwork = 1").is_err());
        // A destination's edges.
        let text = |given: &str| Destination::parse(given).map(|d| d.text());
        assert_eq!(text("Example.com.:8443"), Ok("example.com:8443".into()));
        assert_eq!(text("example.com:0443"), Ok("example.com".into()));
        assert_eq!(text("[::FFFF:1.2.3.4]"), Ok("[::ffff:1.2.3.4]".into()));
        assert_eq!(
            text(&format!("{}.com", "a".repeat(63))).map(|t| t.len()),
            Ok(67)
        );
        for bad in [
            format!("{}.com", "a".repeat(64)),
            format!("{}.com", "a.".repeat(125)),
            "[::1]x".into(),
            "[::1]:".into(),
            "[fe80::1%eth0]".into(),
            "::1".into(),
            "bücher.example".into(),
            "a..b".into(),
            "a.b..".into(),
            "10.1".into(),
            "2130706433".into(),
            "0x7f.1".into(),
            "a.0X7F".into(),
            "192.0.2.010".into(),
        ] {
            assert!(Destination::parse(&bad).is_err(), "{bad}");
        }
        assert_eq!(text("a.0x7g"), Ok("a.0x7g".into()));
        assert_eq!(text("1a.example"), Ok("1a.example".into()));
        // Across the setup frame, whole.
        let mut client = config.client.clone();
        client.template_shared = vec![
            TemplateShared {
                name: "open".into(),
                shared: None,
                network: Some(Network::Open),
            },
            TemplateShared {
                name: "plain".into(),
                shared: None,
                network: None,
            },
        ];
        let crossed = Client::from_json(&client.to_json()).unwrap();
        assert_eq!(crossed, client);
        // A frame refuses what the file does, and a frame without the
        // keys, from a window before them, takes the defaults.
        let with = |key: &str, value: Json| {
            let mut pairs = match client.to_json() {
                Json::Obj(pairs) => pairs,
                _ => Vec::new(),
            };
            pairs.retain(|(k, _)| k != key);
            pairs.push((key.into(), value));
            Client::from_json(&Json::Obj(pairs))
        };
        assert!(with("network", Json::Str("open".into())).is_err());
        assert!(with("network", Json::Str("on".into())).is_err());
        assert!(with(
            "network_allowlist",
            Json::Arr(vec![Json::Str("a_b".into())])
        )
        .is_err());
        let mut pairs = match client.to_json() {
            Json::Obj(pairs) => pairs,
            _ => Vec::new(),
        };
        pairs.retain(|(k, _)| k != "network" && k != "network_allowlist");
        let old = Client::from_json(&Json::Obj(pairs)).unwrap();
        assert_eq!(old.network, Network::Allowlist);
        assert_eq!(old.network_allowlist, default_allowlist());
        // Each workspace's.
        let open = Workspace::Template("open".into());
        assert_eq!(crossed.network_for(&open), Network::Open);
        assert_eq!(
            crossed.network_for(&Workspace::Template("plain".into())),
            Network::Off
        );
        assert_eq!(
            crossed.network_for(&Workspace::Template("gone".into())),
            Network::Off
        );
        let allowing = Client {
            network: Network::Allowlist,
            ..crossed.clone()
        };
        assert_eq!(
            allowing.network_for(&Workspace::Template("plain".into())),
            Network::Allowlist
        );
        assert_eq!(
            allowing.network_for(&Workspace::Scratch),
            Network::Allowlist
        );
        assert_eq!(
            allowing.network_for(&Workspace::Directory("/home/u/src".into())),
            Network::Allowlist
        );
        let repositories = |template: &str| {
            Workspace::Repositories(crate::workspace::Repositories {
                template: template.into(),
                name: "w".into(),
                entries: Vec::new(),
            })
        };
        assert_eq!(allowing.network_for(&repositories("open")), Network::Open);
        assert_eq!(allowing.network_for(&repositories("gone")), Network::Off);
        assert_eq!(allowing.network_for(&open), Network::Open);
        assert_eq!(
            allowing.network_for(&Workspace::Template("gone".into())),
            Network::Off
        );
    }

    #[test]
    fn templates_are_read_in_order_and_checked() {
        let config = parse(
            "[[template]]\nname = \"notes\"\n[[template.shared]]\npath = \"~/notes\"\n\
             write = true\n\n[[template]]\nname = \"bare\"\nshared = []\nnetwork = \"off\"\n\n\
             [[template]]\nname = \"repo\"\n[[template.repos]]\nremote = \"r\"\n\
             base = \"main\"\nbranch = \"b\"\n",
        )
        .unwrap();
        let names: Vec<&str> = config.templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["notes", "bare", "repo"]);
        let home = Path::new("/home/u");
        let shared = |n: usize| config.templates.get(n).unwrap().shared(home);
        assert_eq!(
            shared(0),
            Some(vec![Shared {
                path: "/home/u/notes".into(),
                write: true
            }])
        );
        assert_eq!(shared(1), Some(Vec::new()));
        assert_eq!(shared(2), None);
        assert_eq!(config.templates.get(2).unwrap().repos.len(), 1);
        let repo = config.templates.get(2).unwrap().repos.first().unwrap();
        assert_eq!(repo.sparse, None);
        assert!(config.notes.is_empty(), "{:?}", config.notes);
        let network = |n: usize| config.templates.get(n).unwrap().network;
        assert_eq!(
            [network(0), network(1), network(2)],
            [None, Some(Network::Off), None]
        );
        for (text, said) in [
            ("template = 3", "a list of `[[template]]` tables"),
            ("[[template]]\nshared = []", "has no `name`"),
            ("[[template]]\nname = \"\"", "visible text"),
            ("[[template]]\nname = \" x\"", "visible text"),
            ("[[template]]\nname = \"a\\tb\"", "visible text"),
            ("[[template]]\nname = \"empty\"", "the chooser lists"),
            ("[[template]]\nname = \"a\\u202Eb\"", "visible text"),
            ("[[template]]\nname = \"a\\u200Bb\"", "visible text"),
            (
                "[[template]]\nname = \"Notes\"\n[[template]]\nname = \"notes\"",
                "two templates are named \"notes\"",
            ),
            ("[[template]]\nname = \"Directory...\"", "the chooser lists"),
            (
                "[[template]]\nname = \"a\"\n[[template]]\nname = \"a\"",
                "two templates are named \"a\"",
            ),
            (
                "[[template]]\nname = \"a\"\nmode = 1",
                "unknown field `mode`",
            ),
            (
                "[[template]]\nname = \"a\"\nrepos = 1",
                "`template.repos` is a list",
            ),
            (
                "[[template]]\nname = \"a\"\n[[template.repos]]\nremote = \"r\"\nbase = \"m\"",
                "has no `branch`",
            ),
            (
                "[[template]]\nname = \"a\"\n[[template.repos]]\nremote = \"r\"\nbase = \"m\"\n\
                 branch = \"b\"\nsparse = \"x\"",
                "`sparse` is a list of relative paths",
            ),
            (
                "[[template]]\nname = \"a\"\n[[template.repos]]\nremote = \"r\"\nbase = \"m\"\n\
                 branch = \"b\"\nsparse = [\"a/../../b\"]",
                "no `..`",
            ),
            (
                "[[template]]\nname = \"a\"\n[[template.repos]]\nremote = \"r\"\nbase = \"m\"\n\
                 branch = \"b\"\nsparse = [\"/etc\"]",
                "relative paths",
            ),
            (
                "[[template]]\nname = \"a\"\n[[template.repos]]\nremote = \"r\"\nbase = \"m\"\n\
                 branch = \"b\"\nsparse = [\"a\\nb\"]",
                "control character",
            ),
            (
                "[[template]]\nname = \"a\"\n[[template.shared]]\npath = \"rel\"",
                "`template.shared` is an absolute path",
            ),
            (
                "[[template]]\nname = \"a\"\nshared = 1",
                "`template.shared` is a list of `[[template.shared]]` tables",
            ),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(said), "{text}: {e}");
        }
        let many = "[[template]]\nname = \"t\"\n".repeat(MAX_TEMPLATES + 1);
        assert!(parse(&many).unwrap_err().contains("at most"));
        assert!(template_name(&"x".repeat(MAX_TEMPLATE_NAME)).is_ok());
        assert!(template_name(&"x".repeat(MAX_TEMPLATE_NAME + 1)).is_err());
        for reserved in ["New template\u{2026}", "new template..."] {
            assert!(template_name(reserved)
                .unwrap_err()
                .contains("New template"));
        }
    }

    #[test]
    fn a_template_workspace_binds_its_own_shared_directories() {
        let own = vec![Shared {
            path: "/n".into(),
            write: true,
        }];
        let client = Client {
            shared: vec![Shared {
                path: "/d".into(),
                write: false,
            }],
            template_shared: vec![
                TemplateShared {
                    name: "notes".into(),
                    shared: Some(own.clone()),
                    network: None,
                },
                TemplateShared {
                    name: "plain".into(),
                    shared: None,
                    network: None,
                },
            ],
            ..Client::default()
        };
        assert_eq!(client.shared_for(&Workspace::Template("notes".into())), own);
        // A template that names none, and the built-ins, bind the
        // top-level list.
        for workspace in [
            Workspace::Template("plain".into()),
            Workspace::Scratch,
            Workspace::Directory("/w".into()),
        ] {
            assert_eq!(client.shared_for(&workspace), client.shared);
        }
        // One removed or renamed since binds none: never wider.
        assert!(client
            .shared_for(&Workspace::Template("gone".into()))
            .is_empty());
        assert_eq!(Client::from_json(&client.to_json()).unwrap(), client);
    }
}
