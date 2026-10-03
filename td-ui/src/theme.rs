//! The colour themes a raster paints in: six fixed palettes over the roles
//! the shared palette names (`raster`'s constants and status inks, and the
//! panel's selected row and disabled ink), each giving every one of those
//! colours its own; the chord a widget window cycles them with; and the
//! path and text of the file a program's choice is kept in. A colour that
//! is not one of the palette's passes through, so a document's or a
//! chart's own colours are left as they are. `theme_file` reads and writes
//! the file; nothing here touches the environment or the filesystem.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::chrome::{DISABLED, SELECTED_ROW};
use crate::raster::{
    GlyphStyle, Primitive, ACCENT, BORDER, CHROME, INACTIVE_SELECTION, INK, LINE_NUMBER,
    MISSPELLED, PAPER, SELECTED, SUCCESS, WARNING,
};

/// The roles a theme colours.
pub const ROLES: usize = 13;

/// The colour each role is drawn in by the shared palette, in the order a
/// theme lists its own; `SAND` is these colours.
pub const KEYS: [u32; ROLES] = [
    PAPER,
    INK,
    CHROME,
    BORDER,
    SELECTED,
    INACTIVE_SELECTION,
    LINE_NUMBER,
    MISSPELLED,
    SELECTED_ROW & 0xff_ffff,
    DISABLED & 0xff_ffff,
    SUCCESS,
    WARNING,
    ACCENT,
];

/// The chord that moves a widget window to the next theme. No consumer
/// and no editor key profile binds it, so the window keeps it; the editor
/// core's `F3` and `F7` and the consumers' `F2` and `F6` stay theirs.
pub const CHORD: &str = "F12";
/// The file a program's theme is kept in, under its own directory in the
/// configuration home.
pub const FILE: &str = "theme";
/// The most bytes the file is read for: a name, a newline and room.
pub const MAX_FILE_BYTES: usize = 64;
/// The longest program id a path is built for.
pub const MAX_APP_ID: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// The role colours in `KEYS` order.
    pub colors: [u32; ROLES],
}

/// The shared palette as it is: warm paper.
pub static SAND: Theme = Theme {
    name: "sand",
    colors: KEYS,
};
/// Cool blue-grey paper with a harbour-blue selection.
pub static HARBOR: Theme = Theme {
    name: "harbor",
    colors: [
        0xe4ebf0, 0x2c3a46, 0xd3dee6, 0x9aaebd, 0x2f638c, 0xbccbd6, 0x66798a, 0xa13a3a, 0xb6c8d5,
        0x66788b, 0x39693a, 0x7a5810, 0x66498a,
    ],
};
/// Pale green paper with a fern selection.
pub static MOSS: Theme = Theme {
    name: "moss",
    colors: [
        0xe7ecdc, 0x37402f, 0xd7dfc9, 0xa5b293, 0x4c6b37, 0xc3cdb2, 0x707a63, 0x973f33, 0xbfcaab,
        0x707a63, 0x3b6630, 0x785a14, 0x6b476b,
    ],
};
/// Blush paper with a plum selection.
pub static ROSE: Theme = Theme {
    name: "rose",
    colors: [
        0xf2e5e7, 0x47363c, 0xe6d4d8, 0xbc9fa6, 0x7a4560, 0xd4bfc4, 0x856c73, 0xa0302b, 0xd9c1c7,
        0x89727a, 0x42653a, 0x7f5713, 0x674b8b,
    ],
};
/// Dark blue-slate ground with pale ink.
pub static DUSK: Theme = Theme {
    name: "dusk",
    colors: [
        0x1e2330, 0xd2d8e2, 0x2a3141, 0x4b5569, 0x8db2d8, 0x3a4457, 0x8590a4, 0xee8f80, 0x37425a,
        0x7f8aa0, 0x9cc78b, 0xdeb86b, 0xc79fd8,
    ],
};
/// Dark warm-brown ground with an amber selection.
pub static EMBER: Theme = Theme {
    name: "ember",
    colors: [
        0x28221e, 0xe8ddcf, 0x352d27, 0x5e5148, 0xdba468, 0x4a3f36, 0x9a8b7a, 0xf08f7a, 0x4a3d33,
        0x948675, 0xabc98d, 0xe8c26c, 0xd6a3cb,
    ],
};

/// Every theme, in the order the chord moves through them; the first is
/// the default.
pub static THEMES: [&Theme; 6] = [&SAND, &HARBOR, &MOSS, &ROSE, &DUSK, &EMBER];

impl Theme {
    /// `color` as this theme draws it: a palette colour becomes the
    /// theme's for its role, its top byte kept; any other passes through.
    pub fn map(&self, color: u32) -> u32 {
        let rgb = color & 0xff_ffff;
        KEYS.iter()
            .zip(self.colors)
            .find(|(key, _)| **key == rgb)
            .map_or(color, |(_, mapped)| mapped | (color & 0xff00_0000))
    }

    /// `primitive` with each of its colours mapped. A glyph's ink and
    /// background are mapped before the raster derives a fringe or blends
    /// coverage between them.
    pub fn primitive(&self, primitive: Primitive) -> Primitive {
        if self.colors == KEYS {
            return primitive;
        }
        match primitive {
            Primitive::Fill { rect, color } => Primitive::Fill {
                rect,
                color: self.map(color),
            },
            Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            } => Primitive::Glyph {
                x,
                y,
                scalar,
                style: GlyphStyle {
                    ink: self.map(style.ink),
                    background: self.map(style.background),
                    ..style
                },
            },
            Primitive::Mark { x, y, scalar, ink } => Primitive::Mark {
                x,
                y,
                scalar,
                ink: self.map(ink),
            },
        }
    }

    /// The theme after this one in `THEMES`, the last followed by the
    /// first; a theme not in the list is followed by the first.
    pub fn next(&self) -> &'static Theme {
        let at = THEMES.iter().position(|theme| theme.name == self.name);
        at.and_then(|at| THEMES.get(at + 1))
            .or(THEMES.first())
            .copied()
            .unwrap_or(&SAND)
    }
}

/// The theme called `name`.
pub fn named(name: &str) -> Option<&'static Theme> {
    THEMES.iter().copied().find(|theme| theme.name == name)
}

/// The theme a file's bytes name: one name, surrounding ASCII whitespace
/// ignored, in at most `MAX_FILE_BYTES`.
pub fn parse(bytes: &[u8]) -> Option<&'static Theme> {
    if bytes.len() > MAX_FILE_BYTES {
        return None;
    }
    named(std::str::from_utf8(bytes).ok()?.trim_ascii())
}

/// What the file holds for `theme`.
pub fn text(theme: &Theme) -> String {
    format!("{}\n", theme.name)
}

/// Where program `app_id` keeps its theme, given `XDG_CONFIG_HOME` and
/// `HOME`: `FILE` in the program's directory under the configuration home
/// (`XDG_CONFIG_HOME`, else `HOME/.config`), beside the `config.toml`
/// td-news and td-mail read. A relative or empty value is ignored, as the
/// XDG base directory specification says. An id that is not one plain
/// name of at most `MAX_APP_ID` ASCII letters, digits, `-`, `_` and `.`,
/// not beginning with `.`, has no file.
pub fn path(config_home: Option<&OsStr>, home: Option<&OsStr>, app_id: &str) -> Option<PathBuf> {
    let plain = !app_id.is_empty()
        && app_id.len() <= MAX_APP_ID
        && !app_id.starts_with('.')
        && app_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !plain {
        return None;
    }
    fn absolute(value: Option<&OsStr>) -> Option<&Path> {
        value.map(Path::new).filter(|path| path.is_absolute())
    }
    let base = match absolute(config_home) {
        Some(config) => config.to_path_buf(),
        None => absolute(home)?.join(".config"),
    };
    Some(base.join(app_id).join(FILE))
}

#[cfg(test)]
#[allow(clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// WCAG 2 contrast between two colours, times 100.
    fn contrast(a: u32, b: u32) -> u32 {
        fn linear(channel: u32) -> f64 {
            let c = f64::from(channel & 255) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        let luminance = |color: u32| {
            0.2126 * linear(color >> 16) + 0.7152 * linear(color >> 8) + 0.0722 * linear(color)
        };
        let (a, b) = (luminance(a), luminance(b));
        ((a.max(b) + 0.05) / (a.min(b) + 0.05) * 100.0) as u32
    }

    #[test]
    fn every_theme_keeps_the_pairs_the_widgets_draw_legible() {
        // (foreground role, ground role, least contrast x100), each at or
        // under the default's own: body text, menu text, a panel's
        // selected row, an unfocused selection, selected text on its band,
        // line numbers, a disabled entry and button, borders, the status
        // inks, which also band paper-ink rows, and the message list's
        // tone inks on chrome and on a selected row.
        let pairs = [
            (1, 0, 700),
            (1, 2, 600),
            (1, 8, 500),
            (1, 5, 500),
            (0, 4, 450),
            (6, 0, 300),
            (6, 2, 280),
            (9, 2, 280),
            (3, 0, 160),
            (3, 2, 140),
            (7, 0, 450),
            (10, 0, 450),
            (11, 0, 450),
            (12, 0, 450),
            (9, 0, 340),
            (9, 8, 230),
            (4, 2, 400),
            (4, 8, 310),
            (7, 2, 390),
            (7, 8, 300),
            (6, 8, 230),
        ];
        for theme in THEMES {
            for (fore, ground, least) in pairs {
                let got = contrast(theme.colors[fore], theme.colors[ground]);
                assert!(
                    got >= least,
                    "{}: role {fore} on role {ground} is {got}",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn sand_is_the_shared_palette_and_the_roles_are_distinct() {
        for key in KEYS {
            assert_eq!(SAND.map(key), key);
            assert_eq!(key & 0xff00_0000, 0);
        }
        for (at, key) in KEYS.iter().enumerate() {
            assert!(!KEYS[at + 1..].contains(key), "role colour {key:06x} twice");
        }
    }

    #[test]
    fn a_theme_maps_palette_colours_keeps_their_top_byte_and_passes_others() {
        assert_eq!(DUSK.map(PAPER), 0x1e2330);
        assert_eq!(DUSK.map(INK), 0xd2d8e2);
        assert_eq!(DUSK.map(SELECTED_ROW), 0xff37_425a);
        assert_eq!(DUSK.map(DISABLED), 0xff7f_8aa0);
        assert_eq!(DUSK.map(0x0033aa), 0x0033aa);
        assert_eq!(DUSK.map(0xff00_0000), 0xff00_0000);
        let style = GlyphStyle::medium(INK, CHROME);
        let Primitive::Glyph { style: mapped, .. } = HARBOR.primitive(Primitive::Glyph {
            x: 1,
            y: 2,
            scalar: 'a',
            style,
        }) else {
            panic!("a glyph stays a glyph");
        };
        assert_eq!(mapped, GlyphStyle::medium(0x2c3a46, 0xd3dee6));
    }

    #[test]
    fn the_chord_visits_every_theme_once_and_returns() {
        let mut theme = &SAND;
        let mut seen = Vec::new();
        for _ in 0..THEMES.len() {
            seen.push(theme.name);
            theme = theme.next();
        }
        assert_eq!(theme.name, "sand");
        assert_eq!(seen, ["sand", "harbor", "moss", "rose", "dusk", "ember"]);
        let stranger = Theme {
            name: "stranger",
            colors: KEYS,
        };
        assert_eq!(stranger.next().name, "sand");
    }

    #[test]
    fn the_file_names_one_theme() {
        for theme in THEMES {
            assert_eq!(parse(text(theme).as_bytes()), Some(theme));
            assert_eq!(named(theme.name), Some(theme));
        }
        assert_eq!(parse(b"  moss\r\n"), Some(&MOSS));
        for refused in [
            &b""[..],
            b"Moss",
            b"moss dusk",
            b"\xff",
            &[b' '; MAX_FILE_BYTES + 1],
        ] {
            assert_eq!(parse(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn the_path_is_the_programs_own_under_the_configuration_home() {
        let os = |text: &'static str| Some(OsStr::new(text));
        assert_eq!(
            path(os("/c"), os("/h"), "td-news"),
            Some(PathBuf::from("/c/td-news/theme"))
        );
        assert_eq!(
            path(None, os("/h"), "td-mail"),
            Some(PathBuf::from("/h/.config/td-mail/theme"))
        );
        assert_eq!(
            path(os("relative"), os("/h"), "td-mail"),
            Some(PathBuf::from("/h/.config/td-mail/theme"))
        );
        assert_eq!(
            path(os(""), os("/h"), "td-mail"),
            Some(PathBuf::from("/h/.config/td-mail/theme"))
        );
        assert_eq!(path(None, os("h"), "td-mail"), None);
        assert_eq!(path(None, None, "td-mail"), None);
        let long = "a".repeat(MAX_APP_ID + 1);
        for refused in ["", ".", "..", ".hidden", "a/b", "a b", "é", long.as_str()] {
            assert_eq!(path(os("/c"), None, refused), None, "{refused:?}");
        }
        assert!(path(os("/c"), None, &long[1..]).is_some());
    }
}
