use std::path::PathBuf;

use crate::toml::{self, Toml};

#[derive(Debug)]
pub struct Config {
    pub ui: UiConfig,
    pub theme: Theme,
    pub feeds: Vec<FeedConfig>,
}

#[derive(Debug)]
pub struct UiConfig {
    pub page_size: usize,
    pub scrolloff: usize,
    pub mouse: bool,
    pub sync_interval_secs: u64,
    pub browser: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            page_size: default_page_size(),
            scrolloff: default_scrolloff(),
            mouse: true,
            sync_interval_secs: default_sync_interval(),
            browser: None,
        }
    }
}

fn default_page_size() -> usize {
    100
}

fn default_scrolloff() -> usize {
    0
}

fn default_true() -> bool {
    true
}

fn default_sync_interval() -> u64 {
    300
}

#[derive(Debug, Default)]
pub struct Theme {
    pub bg: Option<String>,
    pub fg: Option<String>,
    pub bold_fg: Option<String>,
    pub selection_bg: Option<String>,
    pub selection_fg: Option<String>,
    pub status_bg: Option<String>,
    pub status_fg: Option<String>,
    pub header_fg: Option<String>,
}

impl Theme {
    pub fn parse_color(hex: &str) -> Result<(u8, u8, u8), String> {
        if hex.len() != 7 || !hex.starts_with('#') {
            return Err(format!("invalid color '{}': expected #RRGGBB", hex));
        }
        let r =
            u8::from_str_radix(&hex[1..3], 16).map_err(|_| format!("invalid color '{}'", hex))?;
        let g =
            u8::from_str_radix(&hex[3..5], 16).map_err(|_| format!("invalid color '{}'", hex))?;
        let b =
            u8::from_str_radix(&hex[5..7], 16).map_err(|_| format!("invalid color '{}'", hex))?;
        Ok((r, g, b))
    }
}

#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub name: String,
    pub url: String,
}

impl UiConfig {
    /// `[ui]`. Every key has a default, so an absent table is the default
    /// table and an absent key is its own default.
    fn from_toml(table: &Toml) -> Result<UiConfig, toml::Error> {
        Ok(UiConfig {
            page_size: table
                .optional_usize("page_size")?
                .unwrap_or_else(default_page_size),
            scrolloff: table
                .optional_usize("scrolloff")?
                .unwrap_or_else(default_scrolloff),
            mouse: table.optional_bool("mouse")?.unwrap_or_else(default_true),
            sync_interval_secs: table
                .optional_u64("sync_interval_secs")?
                .unwrap_or_else(default_sync_interval),
            browser: table.optional_str("browser")?.map(str::to_string),
        })
    }
}

impl Theme {
    /// `[theme]`. Eight optional colours, checked for shape by `load`.
    fn from_toml(table: &Toml) -> Result<Theme, toml::Error> {
        let colour = |key: &str| -> Result<Option<String>, toml::Error> {
            Ok(table.optional_str(key)?.map(str::to_string))
        };
        Ok(Theme {
            bg: colour("bg")?,
            fg: colour("fg")?,
            bold_fg: colour("bold_fg")?,
            selection_bg: colour("selection_bg")?,
            selection_fg: colour("selection_fg")?,
            status_bg: colour("status_bg")?,
            status_fg: colour("status_fg")?,
            header_fg: colour("header_fg")?,
        })
    }
}

impl FeedConfig {
    /// One `[[feed]]`. Both keys are required, and a key present but empty
    /// names nothing and reaches nothing, so it is refused here rather than
    /// at the first fetch.
    fn from_toml(table: &Toml) -> Result<FeedConfig, toml::Error> {
        let field = |key: &str| -> Result<String, toml::Error> {
            let value = table.require_str(key)?;
            if value.is_empty() {
                return Err(toml::Error::new(format!("empty field `{}`", key)));
            }
            Ok(value.to_string())
        };
        Ok(FeedConfig {
            name: field("name")?,
            url: field("url")?,
        })
    }
}

impl Config {
    /// Map a configuration document. Tables and the `feed` array are all
    /// optional here; `load` is what refuses a document with no feed.
    fn from_toml(doc: &Toml) -> Result<Config, toml::Error> {
        let empty = Toml::Table(Vec::new());
        let ui = UiConfig::from_toml(doc.optional_table("ui")?.unwrap_or(&empty))?;
        let theme = Theme::from_toml(doc.optional_table("theme")?.unwrap_or(&empty))?;
        let mut feeds = Vec::new();
        for entry in doc.optional_arr("feed")?.unwrap_or(&[]) {
            feeds.push(FeedConfig::from_toml(entry)?);
        }
        Ok(Config { ui, theme, feeds })
    }

    /// Parse and map one configuration document.
    pub fn parse(contents: &str) -> Result<Config, String> {
        let doc = toml::parse(contents).map_err(|e| format!("config parse error: {}", e))?;
        Config::from_toml(&doc).map_err(|e| format!("config parse error: {}", e))
    }

    pub fn load(path: Option<&str>) -> Result<Config, String> {
        let config_path = match path {
            Some(p) => PathBuf::from(p),
            None => {
                let xdg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
                    let home = std::env::var("HOME").unwrap_or_default();
                    format!("{}/.config", home)
                });
                PathBuf::from(xdg).join("tn").join("config.toml")
            }
        };

        let contents = std::fs::read_to_string(&config_path)
            .map_err(|e| format!("failed to read {}: {}", config_path.display(), e))?;

        let config = Config::parse(&contents)?;

        if config.feeds.is_empty() {
            return Err("no [[feed]] entries in config".to_string());
        }

        // Validate theme colors
        for (name, val) in [
            ("bg", &config.theme.bg),
            ("fg", &config.theme.fg),
            ("bold_fg", &config.theme.bold_fg),
            ("selection_bg", &config.theme.selection_bg),
            ("selection_fg", &config.theme.selection_fg),
            ("status_bg", &config.theme.status_bg),
            ("status_fg", &config.theme.status_fg),
            ("header_fg", &config.theme.header_fg),
        ] {
            if let Some(hex) = val {
                Theme::parse_color(hex).map_err(|e| format!("theme.{}: {}", name, e))?;
            }
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build "#RRGGBB" strings without tripping the Rust 2021 lexer.
    fn hex(s: &str) -> String {
        format!("#{}", s)
    }

    /// The configuration td's image actually ships, copied verbatim from
    /// `td-firstboot/src/main.rs`'s `TN_CONFIG`. It is what the first
    /// window of a fresh install reads, and it is provisioned once and
    /// never rewritten, so a mapping that could not read it would leave a
    /// person with an error and no feeds.
    #[test]
    fn the_shipped_configuration_parses_into_its_two_feeds() {
        const TN_CONFIG: &str = "\
# td news (tn). Provisioned on first boot; edit freely, it is never rewritten.
# The client reads this file when it starts. The feeds below are public
# starting points: replace or delete them, and nothing is fetched until you
# name a feed of your own.

[[feed]]
name = \"LWN\"
url = \"https://lwn.net/headlines/rss\"

[[feed]]
name = \"Rust Blog\"
url = \"https://blog.rust-lang.org/feed.xml\"
";
        let config = Config::parse(TN_CONFIG).expect("the shipped configuration must parse");
        let feeds: Vec<(&str, &str)> = config
            .feeds
            .iter()
            .map(|feed| (feed.name.as_str(), feed.url.as_str()))
            .collect();
        assert_eq!(
            feeds,
            [
                ("LWN", "https://lwn.net/headlines/rss"),
                ("Rust Blog", "https://blog.rust-lang.org/feed.xml"),
            ]
        );
        // It names no `[ui]` or `[theme]`, so every default applies and
        // `load`'s colour check has nothing to refuse.
        assert_eq!(config.ui.page_size, 100);
        assert_eq!(config.ui.scrolloff, 0);
        assert_eq!(config.ui.sync_interval_secs, 300);
        assert!(config.ui.mouse);
        assert!(config.ui.browser.is_none());
        assert!(config.theme.bg.is_none() && config.theme.header_fg.is_none());
    }

    #[test]
    fn parse_minimal_config() {
        let toml_str = "[[feed]]\nname = \"Test\"\nurl = \"https://example.com/rss\"\n";
        let config = Config::parse(toml_str).unwrap();
        assert_eq!(config.feeds.len(), 1);
        assert_eq!(config.feeds[0].name, "Test");
        assert_eq!(config.ui.page_size, 100);
        assert_eq!(config.ui.scrolloff, 0);
        assert_eq!(config.ui.sync_interval_secs, 300);
        assert!(config.ui.mouse);
        assert!(config.ui.browser.is_none());
    }

    #[test]
    fn parse_full_config() {
        let toml_str = concat!(
            "[ui]\n",
            "page_size = 50\n",
            "scrolloff = 3\n",
            "mouse = false\n",
            "sync_interval_secs = 600\n",
            "\n",
            "[theme]\n",
            "bg = \"#002b36\"\n",
            "fg = \"#839496\"\n",
            "\n",
            "[[feed]]\n",
            "name = \"HN\"\n",
            "url = \"https://news.ycombinator.com/rss\"\n",
            "\n",
            "[[feed]]\n",
            "name = \"LWN\"\n",
            "url = \"https://lwn.net/headlines/rss\"\n",
        );
        let config = Config::parse(toml_str).unwrap();
        assert_eq!(config.feeds.len(), 2);
        assert_eq!(config.ui.page_size, 50);
        assert_eq!(config.ui.scrolloff, 3);
        assert!(!config.ui.mouse);
        assert_eq!(config.theme.bg.as_deref(), Some(hex("002b36")).as_deref());
    }

    /// The mapping's refusals, in serde's words: a `[[feed]]` short of a
    /// key, one whose key is of the wrong type, and a `[ui]` number that is
    /// not one. An unknown key is still ignored, as serde ignored it.
    #[test]
    fn a_refused_document_says_which_field_and_why() {
        let refused = |text: &str| Config::parse(text).unwrap_err();
        assert_eq!(
            refused("[[feed]]\nurl = \"https://example.com/rss\"\n"),
            format!(
                "config parse error: {}",
                toml::missing_field_message("name")
            )
        );
        assert_eq!(
            refused("[[feed]]\nname = 1\nurl = \"u\"\n"),
            "config parse error: invalid type for `name`: expected a string, found integer"
        );
        assert_eq!(
            refused("[ui]\npage_size = \"many\"\n"),
            "config parse error: invalid type for `page_size`: expected an integer, found string"
        );
        assert_eq!(
            refused("[ui]\nscrolloff = -1\n"),
            format!(
                "config parse error: invalid value for `scrolloff`: expected an integer between 0 and {}, found -1",
                usize::MAX
            )
        );
        // A named feed with nothing in the name is no name at all.
        assert_eq!(
            refused("[[feed]]\nname = \"\"\nurl = \"u\"\n"),
            "config parse error: empty field `name`"
        );
        // Unknown keys are ignored, and a position is reported for a
        // document that is not TOML at all.
        assert!(Config::parse("[ui]\nnot_a_key = 1\n").is_ok());
        assert_eq!(
            refused("[ui\n"),
            "config parse error: line 1, column 4: expected `]` to close the table header"
        );
    }

    #[test]
    fn parse_color_valid() {
        assert_eq!(Theme::parse_color(&hex("002b36")), Ok((0, 43, 54)));
        assert_eq!(Theme::parse_color(&hex("ffffff")), Ok((255, 255, 255)));
    }

    #[test]
    fn parse_color_invalid() {
        assert!(Theme::parse_color("002b36").is_err());
        assert!(Theme::parse_color(&hex("gggggg")).is_err());
        assert!(Theme::parse_color(&hex("abc")).is_err());
    }
}
