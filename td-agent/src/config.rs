//! `$XDG_CONFIG_HOME/td-agent/config`, TOML (DESIGN.md §15). Every key the
//! design lists is known here: the ones this increment reads are parsed
//! and checked, each other one is accepted by name and reported as read by
//! the increment that first uses it, and an unknown key, `limits`
//! included, is refused by name. A missing file is every default.

use std::path::{Path, PathBuf};

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
    ("base_url", Use::Later(5)),
    ("model", Use::Later(5)),
    ("orchestrator_model", Use::Later(5)),
    ("title_model", Use::Later(5)),
    ("classifier_fast_model", Use::Later(13)),
    ("classifier_model", Use::Later(13)),
    ("jev_threshold", Use::Later(13)),
    ("jev_required", Use::Later(13)),
    ("reasoning_effort", Use::Later(5)),
    ("mode", Use::Read),
    ("data_collection", Use::Later(5)),
    ("max_cost_per_turn", Use::Later(5)),
    ("max_cost_per_conversation", Use::Later(5)),
    ("max_cost_per_day", Use::Later(5)),
    ("workspace_root", Use::Later(10)),
    ("shared", Use::Later(10)),
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
    /// One line per key present that a later increment reads, so a
    /// setting that does nothing yet is said, never silently ignored.
    pub notes: Vec<String>,
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
    let table = crate::toml::parse(text).map_err(|e| e.to_string())?;
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
    fn every_listed_key_is_accepted_and_the_unread_ones_said() {
        let example = "model = \"anthropic/claude-sonnet-5.5\"\nmode = \"auto\"\n\
                       max_cost_per_day = 25\n\n[[shared]]\npath = \"~/Downloads\"\n\n\
                       [[shared]]\npath = \"~/src/reference\"\nwrite = false\n";
        let config = parse(example).unwrap();
        assert_eq!(config.mode, Mode::Auto);
        assert_eq!(config.notes.len(), 3, "{:?}", config.notes);
        assert!(config.notes.iter().any(|n| n.starts_with("`shared`")));
        for (key, use_) in KEYS {
            let config = parse(&format!("{key} = 1")).map(|c| c.notes);
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
