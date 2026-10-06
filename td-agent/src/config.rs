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
    pub limits: Limits,
    /// The shared directories the window admitted (DESIGN.md §8),
    /// resolved and absolute: what every workspace instance binds.
    pub shared: Vec<Shared>,
    /// Every configured template, with the shared directories of its
    /// own its workspaces bind in place of `shared`, admitted as it is.
    pub template_shared: Vec<TemplateShared>,
    /// The most background processes a conversation runs at once.
    pub max_background: u32,
    /// The bytes of each background process's output kept.
    pub background_output_bytes: u64,
}

/// A configured template and its own shared directories, admitted; none
/// when it names none and its workspaces bind `shared`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateShared {
    pub name: String,
    pub shared: Option<Vec<Shared>>,
}

impl Default for Client {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.into(),
            model: DEFAULT_MODEL.into(),
            title_model: DEFAULT_TITLE_MODEL.into(),
            reasoning_effort: DEFAULT_EFFORT.into(),
            allow_data_collection: false,
            limits: Limits::default(),
            shared: Vec::new(),
            template_shared: Vec::new(),
            max_background: DEFAULT_MAX_BACKGROUND,
            background_output_bytes: DEFAULT_BACKGROUND_OUTPUT_BYTES,
        }
    }
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
            (
                "background_output_bytes".into(),
                Json::from(self.background_output_bytes),
            ),
            ("shared".into(), shared_json(&self.shared)),
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
            limits: Limits {
                turn: limit("max_cost_per_turn")?,
                conversation: limit("max_cost_per_conversation")?,
                day: limit("max_cost_per_day")?,
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
                        Ok(TemplateShared {
                            name: template_name(name)?,
                            shared,
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
const KEYS: &[(&str, Use)] = &[
    ("base_url", Use::Read),
    ("model", Use::Read),
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
    ("template", Use::Read),
    ("remotes", Use::Read),
    ("network", Use::Later(15)),
    ("network_allowlist", Use::Later(15)),
    ("protected_branches", Use::Later(14)),
    ("fetch_interval", Use::Read),
    ("fetch_concurrency", Use::Later(11)),
    ("max_background", Use::Read),
    ("background_output_bytes", Use::Read),
    ("auto_compact", Use::Later(16)),
    ("compact_at", Use::Later(16)),
    ("compact_keep_tokens", Use::Later(16)),
    ("compact_model", Use::Later(16)),
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
    /// `[[shared]]` as the file gives them, `~` unexpanded; none is
    /// `~/Downloads` read-only, and an empty list none at all.
    pub shared: Option<Vec<Shared>>,
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
const MAX_TEMPLATES: usize = 64;
/// The chooser's built-ins, which no template may be named.
pub const EMPTY: &str = "Empty";
pub const DIRECTORY: &str = "Directory\u{2026}";

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
    if lower == "empty" || lower == "directory\u{2026}" || lower == "directory..." {
        return Err(format!(
            "no template may be named {name:?}: the chooser lists {EMPTY} and {DIRECTORY} itself"
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

/// `[[template]]`, and a note for each key it has that is not read yet.
fn templates(value: &Toml, notes: &mut Vec<String>) -> Result<Vec<Template>, String> {
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
        if item.get("network").is_some() {
            notes.push(format!(
                "template {name:?}'s `network` is accepted and not read yet: increment 15 reads it"
            ));
        }
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
        });
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
            Some(shared) => expand_shared(shared, home),
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
        config.shared = Some(shared_list("shared", value)?);
    }
    if let Some(value) = table.get("template") {
        config.templates = templates(value, &mut config.notes)?;
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
        assert_eq!(
            config.notes,
            ["template \"bare\"'s `network` is accepted and not read yet: increment 15 reads it"]
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
                },
                TemplateShared {
                    name: "plain".into(),
                    shared: None,
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
