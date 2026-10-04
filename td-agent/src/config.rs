//! `$XDG_CONFIG_HOME/td-agent/config`, TOML (DESIGN.md §15). Every key the
//! design lists is known here: the ones built so far are parsed and
//! checked, each other one is accepted by name and reported as read by the
//! increment that first uses it, and an unknown key, `limits` included, is
//! refused by name. A missing file is every default.

use std::path::{Path, PathBuf};

use crate::cost::{self, Limits};
use crate::workspace::Shared;
use td_json::Json;
use td_toml::Toml;

/// Where models are asked for (DESIGN.md §5).
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// The model a conversation and the orchestrator use by default.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-5.5";
/// The cheap model titles come from (DESIGN.md §13).
pub const DEFAULT_TITLE_MODEL: &str = "anthropic/claude-haiku-4.5";
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
    pub orchestrator_model: String,
    pub title_model: String,
    pub reasoning_effort: String,
    /// `provider.data_collection`: whether providers that may keep or
    /// train on prompts may serve a request; `deny` by default.
    pub allow_data_collection: bool,
    pub limits: Limits,
    /// The shared directories the window admitted (DESIGN.md §8),
    /// resolved and absolute: what every workspace instance binds.
    pub shared: Vec<Shared>,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            orchestrator_model: DEFAULT_MODEL.into(),
            title_model: DEFAULT_TITLE_MODEL.into(),
            reasoning_effort: DEFAULT_EFFORT.into(),
            allow_data_collection: false,
            limits: Limits::default(),
            shared: Vec::new(),
        }
    }
}

impl Client {
    /// The settings as the socketpair carries them.
    pub fn to_json(&self) -> Json {
        let limit = |l: Option<u64>| l.map_or(Json::Null, Json::from);
        Json::Obj(vec![
            ("base_url".into(), Json::Str(self.base_url.clone())),
            ("model".into(), Json::Str(self.model.clone())),
            (
                "orchestrator_model".into(),
                Json::Str(self.orchestrator_model.clone()),
            ),
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
            ("max_cost_per_turn".into(), limit(self.limits.turn)),
            (
                "max_cost_per_conversation".into(),
                limit(self.limits.conversation),
            ),
            ("max_cost_per_day".into(), limit(self.limits.day)),
            (
                "shared".into(),
                Json::Arr(
                    self.shared
                        .iter()
                        .map(|shared| {
                            Json::Obj(vec![
                                ("path".into(), Json::Str(shared.path.display().to_string())),
                                ("write".into(), Json::Bool(shared.write)),
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
            orchestrator_model: model_id("orchestrator_model", text("orchestrator_model")?)?,
            title_model: model_id("title_model", text("title_model")?)?,
            reasoning_effort: effort(text("reasoning_effort")?)?,
            allow_data_collection: data_collection(text("data_collection")?)?,
            limits: Limits {
                turn: limit("max_cost_per_turn")?,
                conversation: limit("max_cost_per_conversation")?,
                day: limit("max_cost_per_day")?,
            },
            shared: match value.get("shared") {
                None => Vec::new(),
                Some(Json::Arr(items)) => items
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
                    .collect::<Result<_, String>>()?,
                Some(_) => return Err("shared is not a list".into()),
            },
        };
        Ok(client)
    }

    /// The model a conversation of `role` talks to.
    pub fn model_for(&self, role: crate::store::Role) -> &str {
        match role {
            crate::store::Role::Orchestrator => &self.orchestrator_model,
            crate::store::Role::Conversation => &self.model,
        }
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

fn data_collection(text: &str) -> Result<bool, String> {
    match text {
        "deny" => Ok(false),
        "allow" => Ok(true),
        other => Err(format!(
            "`data_collection` is `deny` or `allow`, not {other:?}"
        )),
    }
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
const KEYS: [(&str, Use); 28] = [
    ("base_url", Use::Read),
    ("model", Use::Read),
    ("orchestrator_model", Use::Read),
    ("title_model", Use::Read),
    ("classifier_fast_model", Use::Later(13)),
    ("classifier_model", Use::Later(13)),
    ("jev_threshold", Use::Later(13)),
    ("jev_required", Use::Later(13)),
    ("reasoning_effort", Use::Read),
    ("mode", Use::Read),
    ("data_collection", Use::Read),
    ("max_cost_per_turn", Use::Read),
    ("max_cost_per_conversation", Use::Read),
    ("max_cost_per_day", Use::Read),
    ("workspace_root", Use::Read),
    ("shared", Use::Read),
    ("remotes", Use::Later(11)),
    ("network", Use::Later(15)),
    ("network_allowlist", Use::Later(15)),
    ("protected_branches", Use::Later(14)),
    ("fetch_interval", Use::Later(11)),
    ("fetch_concurrency", Use::Later(11)),
    ("max_background", Use::Later(12)),
    ("background_output_bytes", Use::Later(12)),
    ("auto_compact", Use::Later(16)),
    ("compact_at", Use::Later(16)),
    ("compact_keep_tokens", Use::Later(16)),
    ("compact_model", Use::Later(16)),
];

/// Keys refused with a reason of their own rather than as unknown.
const REFUSED: [(&str, &str); 1] = [(
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
    /// `[[shared]]` as the file gives them, `~` unexpanded; none is
    /// `~/Downloads` read-only, and an empty list none at all.
    pub shared: Option<Vec<Shared>>,
}

/// Where repository workspaces live (DESIGN.md §7).
pub const DEFAULT_WORKSPACE_ROOT: &str = "~/td-agent";
/// The shared directory every workspace gets unless configured otherwise.
pub const DEFAULT_SHARED: &str = "~/Downloads";

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
        match &self.shared {
            None => vec![Shared {
                path: expand(Path::new(DEFAULT_SHARED), home),
                write: false,
            }],
            Some(shared) => shared
                .iter()
                .map(|shared| Shared {
                    path: expand(&shared.path, home),
                    write: shared.write,
                })
                .collect(),
        }
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
    let base = match config_home.map(PathBuf::from).filter(|v| v.is_absolute()) {
        Some(dir) => dir,
        None => PathBuf::from(home.filter(|v| !v.is_empty())?).join(".config"),
    };
    base.is_absolute()
        .then(|| base.join("td-agent").join("config"))
}

/// The configuration at `path`, every default when there is no file.
pub fn load(path: Option<&Path>) -> Result<Config, String> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = match std::fs::File::open(path) {
        Ok(file) => {
            use std::io::Read;
            let mut text = String::new();
            file.take(MAX_BYTES.saturating_add(1))
                .read_to_string(&mut text)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if text.len() as u64 > MAX_BYTES {
                return Err(format!(
                    "{} is longer than {MAX_BYTES} bytes",
                    path.display()
                ));
            }
            text
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
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
        ("orchestrator_model", &mut client.orchestrator_model),
        ("title_model", &mut client.title_model),
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
        let items = value
            .as_arr()
            .ok_or("`shared` is a list of `[[shared]]` tables")?;
        let mut shared = Vec::new();
        for item in items {
            if !item.is_table() {
                return Err("`shared` is a list of `[[shared]]` tables".into());
            }
            item.check_known_keys(&["path", "write"])
                .map_err(|e| format!("`shared`: {e}"))?;
            let path = item
                .optional_str("path")
                .map_err(|e| format!("`shared`: {e}"))?
                .ok_or("a `[[shared]]` table has no `path`")?;
            let write = match item.get("write") {
                None => false,
                Some(write) => write
                    .as_bool()
                    .ok_or("`shared`'s `write` is true or false")?,
            };
            shared.push(Shared {
                path: configured_path("shared", path)?,
                write,
            });
        }
        config.shared = Some(shared);
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
                turn: Some(cost::ONE),
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
        assert_eq!(
            config.shared(home),
            [Shared {
                path: "/home/u/Downloads".into(),
                write: false
            }]
        );
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
    fn every_listed_key_is_accepted_and_the_unread_ones_said() {
        let example = "model = \"anthropic/claude-sonnet-5.5\"\nmode = \"auto\"\n\
                       max_cost_per_day = 25\n\n[[shared]]\npath = \"~/Downloads\"\n\n\
                       [[shared]]\npath = \"~/src/reference\"\nwrite = false\n";
        let config = parse(example).unwrap();
        assert_eq!(config.mode, Mode::Auto);
        assert!(config.notes.is_empty(), "{:?}", config.notes);
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

    #[test]
    fn the_model_clients_keys_are_read_and_checked() {
        let config = parse(
            "base_url = \"https://example.test/api/v1/\"\nmodel = \"a/b\"\n\
             orchestrator_model = \"c/d\"\ntitle_model = \"e/f\"\n\
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
        assert_eq!(
            (client.model.as_str(), client.orchestrator_model.as_str()),
            ("a/b", "c/d")
        );
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
        assert_eq!(client.model_for(crate::store::Role::Orchestrator), "c/d");
        assert_eq!(client.model_for(crate::store::Role::Conversation), "a/b");
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
}
