//! The one bounded session-settings file and its portal representation.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use crate::wire::{WireError, Writer};

pub const APPEARANCE: &str = "org.freedesktop.appearance";
pub const GNOME_INTERFACE: &str = "org.gnome.desktop.interface";
pub const NAMESPACE_COUNT: usize = 2;
pub const SETTING_COUNT: usize = 11;
pub const MAX_CONFIG_BYTES: u64 = 4 * 1024;
const MAX_TEXT_BYTES: usize = 64;
// The GNOME schema's own range for text-scaling-factor.
const MIN_TEXT_SCALING_FACTOR: f64 = 0.5;
const MAX_TEXT_SCALING_FACTOR: f64 = 3.0;

pub const DEFAULT_CONFIG: &str = include_str!("../default-settings.conf");

const KEYS: [&str; SETTING_COUNT + 1] = [
    "format",
    "color-scheme",
    "accent-color",
    "contrast",
    "gtk-theme",
    "icon-theme",
    "cursor-theme",
    "cursor-size",
    "font-name",
    "document-font-name",
    "monospace-font-name",
    "text-scaling-factor",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    color_scheme: u32,
    accent_color: (f64, f64, f64),
    contrast: u32,
    gtk_theme: String,
    icon_theme: String,
    cursor_theme: String,
    cursor_size: i32,
    font_name: String,
    document_font_name: String,
    monospace_font_name: String,
    // Served because GDK's portal path derives its Xft DPI from this key
    // alone (`96 * text-scaling-factor * 1024` in Xft's 1/1024-dpi units)
    // and sets the screen resolution to `96 * text-scaling-factor` from it;
    // absent the key it never sets a resolution at all
    // (gdkscreen-wayland.c `update_xft_settings`). Firefox reads that raw
    // resolution as its system-font scale, so without this key every chrome
    // font is sized at or below zero and the tab strip and URL bar draw no
    // text while page content, which never consults it, renders normally.
    text_scaling_factor: f64,
}

#[derive(Clone, Copy)]
pub(crate) enum Setting<'a> {
    Uint(u32),
    Int(i32),
    Text(&'a str),
    Rgb(f64, f64, f64),
    Double(f64),
}

impl Setting<'_> {
    fn signature(self) -> &'static str {
        match self {
            Self::Uint(_) => "u",
            Self::Int(_) => "i",
            Self::Text(_) => "s",
            Self::Rgb(_, _, _) => "(ddd)",
            Self::Double(_) => "d",
        }
    }

    fn write(self, writer: &mut Writer) -> Result<(), WireError> {
        match self {
            Self::Uint(value) => writer.uint32(value),
            Self::Int(value) => writer.int32(value),
            Self::Text(value) => writer.string(value)?,
            Self::Rgb(red, green, blue) => writer.structure(|writer| {
                writer.double(red);
                writer.double(green);
                writer.double(blue);
                Ok(())
            })?,
            Self::Double(value) => writer.double(value),
        }
        Ok(())
    }

    pub(crate) fn write_historical(self, writer: &mut Writer) -> Result<(), WireError> {
        writer.variant("v", |writer| {
            writer.variant(self.signature(), |writer| self.write(writer))
        })
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!("{} is not a regular settings file", path.display()));
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(format!(
                "{} is {} bytes, over the {MAX_CONFIG_BYTES}-byte settings ceiling",
                path.display(),
                metadata.len()
            ));
        }
        let file = fs::File::open(path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(format!(
                "{} grew past the {MAX_CONFIG_BYTES}-byte settings ceiling",
                path.display()
            ));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| format!("{} is not UTF-8", path.display()))?;
        Self::parse(text).map_err(|why| format!("{}: {why}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() || !text.ends_with('\n') || text.contains('\r') {
            return Err("settings must be nonempty LF-terminated text".into());
        }
        let mut values = BTreeMap::new();
        for (number, line) in text.lines().enumerate() {
            let line_number = number.saturating_add(1);
            let Some((key, value)) = line.split_once('=') else {
                return Err(format!("line {line_number} has no '='"));
            };
            if !KEYS.contains(&key) {
                return Err(format!("line {line_number} names unknown setting {key:?}"));
            }
            if value.is_empty() || value.trim() != value {
                return Err(format!("line {line_number} has an empty or padded value"));
            }
            if values.insert(key, value).is_some() {
                return Err(format!("line {line_number} repeats setting {key:?}"));
            }
        }
        for key in KEYS {
            if !values.contains_key(key) {
                return Err(format!("setting {key:?} is missing"));
            }
        }
        if values.len() != KEYS.len() {
            return Err("the settings file has an unexpected key count".into());
        }
        if values.get("format").copied() != Some("1") {
            return Err("format must be exactly 1".into());
        }
        let color_scheme = small_u32(&values, "color-scheme", 2)?;
        let contrast = small_u32(&values, "contrast", 1)?;
        let cursor_size = positive_i32(&values, "cursor-size", 256)?;
        let accent_color = rgb(&values, "accent-color")?;
        let text_scaling_factor = scaling_factor(&values, "text-scaling-factor")?;
        Ok(Self {
            color_scheme,
            accent_color,
            contrast,
            gtk_theme: text_value(&values, "gtk-theme")?,
            icon_theme: text_value(&values, "icon-theme")?,
            cursor_theme: text_value(&values, "cursor-theme")?,
            cursor_size,
            font_name: text_value(&values, "font-name")?,
            document_font_name: text_value(&values, "document-font-name")?,
            monospace_font_name: text_value(&values, "monospace-font-name")?,
            text_scaling_factor,
        })
    }

    pub(crate) fn setting(&self, namespace: &str, key: &str) -> Option<Setting<'_>> {
        Some(match (namespace, key) {
            (APPEARANCE, "color-scheme") => Setting::Uint(self.color_scheme),
            (APPEARANCE, "accent-color") => Setting::Rgb(
                self.accent_color.0,
                self.accent_color.1,
                self.accent_color.2,
            ),
            (APPEARANCE, "contrast") => Setting::Uint(self.contrast),
            (GNOME_INTERFACE, "gtk-theme") => Setting::Text(&self.gtk_theme),
            (GNOME_INTERFACE, "icon-theme") => Setting::Text(&self.icon_theme),
            (GNOME_INTERFACE, "cursor-theme") => Setting::Text(&self.cursor_theme),
            (GNOME_INTERFACE, "cursor-size") => Setting::Int(self.cursor_size),
            (GNOME_INTERFACE, "font-name") => Setting::Text(&self.font_name),
            (GNOME_INTERFACE, "document-font-name") => Setting::Text(&self.document_font_name),
            (GNOME_INTERFACE, "monospace-font-name") => Setting::Text(&self.monospace_font_name),
            (GNOME_INTERFACE, "text-scaling-factor") => Setting::Double(self.text_scaling_factor),
            _ => return None,
        })
    }

    pub fn write_read_all(&self, writer: &mut Writer, filters: &[&str]) -> Result<(), WireError> {
        writer.array("{sa{sv}}", |writer| {
            for namespace in [APPEARANCE, GNOME_INTERFACE] {
                if !matches_namespace(namespace, filters) {
                    continue;
                }
                writer.dict_entry(|writer| {
                    writer.string(namespace)?;
                    writer.array("{sv}", |writer| {
                        for key in namespace_keys(namespace) {
                            let Some(setting) = self.setting(namespace, key) else {
                                return Err(WireError::BadSignature);
                            };
                            writer.dict_entry(|writer| {
                                writer.string(key)?;
                                writer.variant(setting.signature(), |writer| setting.write(writer))
                            })?;
                        }
                        Ok(())
                    })
                })?;
            }
            Ok(())
        })
    }
}

fn namespace_keys(namespace: &str) -> &'static [&'static str] {
    match namespace {
        APPEARANCE => &["color-scheme", "accent-color", "contrast"],
        GNOME_INTERFACE => &[
            "gtk-theme",
            "icon-theme",
            "cursor-theme",
            "cursor-size",
            "font-name",
            "document-font-name",
            "monospace-font-name",
            "text-scaling-factor",
        ],
        _ => &[],
    }
}

fn matches_namespace(namespace: &str, filters: &[&str]) -> bool {
    if filters.is_empty() || filters.contains(&"") {
        return true;
    }
    filters.iter().any(|filter| {
        if filter == &namespace {
            return true;
        }
        let Some(prefix) = filter.strip_suffix(".*") else {
            return false;
        };
        namespace == prefix
            || namespace
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn required<'a>(values: &BTreeMap<&str, &'a str>, key: &str) -> Result<&'a str, String> {
    values
        .get(key)
        .copied()
        .ok_or_else(|| format!("setting {key:?} is missing"))
}

fn small_u32(values: &BTreeMap<&str, &str>, key: &str, maximum: u32) -> Result<u32, String> {
    let text = required(values, key)?;
    let value = text
        .parse::<u32>()
        .map_err(|_| format!("setting {key:?} is not an unsigned integer"))?;
    (value <= maximum)
        .then_some(value)
        .ok_or_else(|| format!("setting {key:?} is over {maximum}"))
}

fn positive_i32(values: &BTreeMap<&str, &str>, key: &str, maximum: i32) -> Result<i32, String> {
    let text = required(values, key)?;
    let value = text
        .parse::<i32>()
        .map_err(|_| format!("setting {key:?} is not an integer"))?;
    (value > 0 && value <= maximum)
        .then_some(value)
        .ok_or_else(|| format!("setting {key:?} is outside 1..={maximum}"))
}

fn text_value(values: &BTreeMap<&str, &str>, key: &str) -> Result<String, String> {
    let value = required(values, key)?;
    if value.len() > MAX_TEXT_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(format!(
            "setting {key:?} is not 1..={MAX_TEXT_BYTES} printable ASCII bytes"
        ));
    }
    Ok(value.to_string())
}

fn scaling_factor(values: &BTreeMap<&str, &str>, key: &str) -> Result<f64, String> {
    let text = required(values, key)?;
    let value = text
        .parse::<f64>()
        .map_err(|_| format!("setting {key:?} is not a number"))?;
    if !value.is_finite() {
        return Err(format!("setting {key:?} is not a finite number"));
    }
    if !(MIN_TEXT_SCALING_FACTOR..=MAX_TEXT_SCALING_FACTOR).contains(&value) {
        return Err(format!(
            "setting {key:?} is outside {MIN_TEXT_SCALING_FACTOR:.1}..={MAX_TEXT_SCALING_FACTOR:.1}"
        ));
    }
    Ok(value)
}

fn rgb(values: &BTreeMap<&str, &str>, key: &str) -> Result<(f64, f64, f64), String> {
    let value = required(values, key)?;
    let mut components = value.split(',');
    let red = component(components.next(), key)?;
    let green = component(components.next(), key)?;
    let blue = component(components.next(), key)?;
    if components.next().is_some() {
        return Err(format!("setting {key:?} has more than three components"));
    }
    Ok((red, green, blue))
}

fn component(value: Option<&str>, key: &str) -> Result<f64, String> {
    let text = value.ok_or_else(|| format!("setting {key:?} has fewer than three components"))?;
    let number = text
        .parse::<f64>()
        .map_err(|_| format!("setting {key:?} has a non-number component"))?;
    if !number.is_finite() || !(0.0..=1.0).contains(&number) {
        return Err(format!("setting {key:?} has a component outside 0..=1"));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::wire::{self, Endian, Limits, Value};

    #[test]
    fn the_complete_default_file_is_one_closed_schema() {
        let settings = Settings::parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(settings.color_scheme, 1);
        assert_eq!(settings.cursor_size, 24);
        assert_eq!(settings.accent_color, (0.125, 0.375, 0.75));
        assert_eq!(settings.text_scaling_factor, 1.0);
        for broken in [
            DEFAULT_CONFIG.replace("format=1\n", ""),
            DEFAULT_CONFIG.replace("format=1\n", "format=2\n"),
            format!("{DEFAULT_CONFIG}extra=yes\n"),
            DEFAULT_CONFIG.replace("contrast=0\n", "contrast=0\ncontrast=1\n"),
            DEFAULT_CONFIG.trim_end().to_string(),
        ] {
            assert!(Settings::parse(&broken).is_err(), "accepted {broken:?}");
        }
    }

    #[test]
    fn every_number_is_bounded_and_every_string_is_printable() {
        for broken in [
            DEFAULT_CONFIG.replace("color-scheme=1", "color-scheme=3"),
            DEFAULT_CONFIG.replace("contrast=0", "contrast=2"),
            DEFAULT_CONFIG.replace("cursor-size=24", "cursor-size=0"),
            DEFAULT_CONFIG.replace("0.125,0.375,0.75", "nan,0.375,0.75"),
            DEFAULT_CONFIG.replace("0.125,0.375,0.75", "1.1,0.375,0.75"),
            DEFAULT_CONFIG.replace("gtk-theme=Adwaita", "gtk-theme=bad\tvalue"),
            DEFAULT_CONFIG.replace("text-scaling-factor=1.0", "text-scaling-factor=0.4"),
            DEFAULT_CONFIG.replace("text-scaling-factor=1.0", "text-scaling-factor=3.5"),
            DEFAULT_CONFIG.replace("text-scaling-factor=1.0", "text-scaling-factor=nan"),
            DEFAULT_CONFIG.replace("text-scaling-factor=1.0", "text-scaling-factor=inf"),
            DEFAULT_CONFIG.replace("text-scaling-factor=1.0", "text-scaling-factor=-1"),
            DEFAULT_CONFIG.replace("text-scaling-factor=1.0", "text-scaling-factor=one"),
        ] {
            assert!(Settings::parse(&broken).is_err(), "accepted {broken:?}");
        }
        // Each scaling-factor rejection names its own reason.
        for (value, reason) in [
            ("one", "is not a number"),
            ("nan", "is not a finite number"),
            ("3.5", "is outside 0.5..=3.0"),
        ] {
            let broken = DEFAULT_CONFIG.replace(
                "text-scaling-factor=1.0",
                &format!("text-scaling-factor={value}"),
            );
            let error = Settings::parse(&broken).err();
            assert!(
                error.as_deref().is_some_and(|error| error.contains(reason)),
                "{value}: {error:?}"
            );
        }
        // Both ends of the GNOME range are inside it.
        for (edge, expected) in [("0.5", 0.5), ("3.0", 3.0), ("3", 3.0)] {
            let accepted = DEFAULT_CONFIG.replace(
                "text-scaling-factor=1.0",
                &format!("text-scaling-factor={edge}"),
            );
            let settings = Settings::parse(&accepted);
            assert!(settings.is_ok(), "rejected {edge}: {settings:?}");
            assert_eq!(
                settings.map(|settings| settings.text_scaling_factor),
                Ok(expected)
            );
        }
    }

    #[test]
    fn read_all_is_the_declared_nested_dictionary() {
        let settings = Settings::parse(DEFAULT_CONFIG).unwrap();
        let mut writer = Writer::new(Endian::Little);
        settings.write_read_all(&mut writer, &[]).unwrap();
        let values = wire::read_body(
            writer.as_bytes(),
            "a{sa{sv}}",
            Endian::Little,
            Limits::NO_FDS,
        )
        .unwrap();
        let namespaces = values
            .first()
            .and_then(Value::as_seq)
            .unwrap()
            .values(NAMESPACE_COUNT)
            .unwrap();
        assert_eq!(namespaces.len(), NAMESPACE_COUNT);
        let mut total = 0usize;
        for namespace in namespaces {
            let pair = namespace.as_seq().unwrap().values(2).unwrap();
            total += pair
                .get(1)
                .and_then(Value::as_seq)
                .unwrap()
                .values(SETTING_COUNT)
                .unwrap()
                .len();
        }
        assert_eq!(total, SETTING_COUNT);
    }

    /// GDK only computes a usable Xft DPI from this key, and it wants a
    /// plain `d`, which `apply_portal_setting` reads with
    /// `g_variant_get_double`.
    #[test]
    fn the_text_scaling_factor_is_published_to_gdk_as_a_unit_double() {
        let settings = Settings::parse(DEFAULT_CONFIG).unwrap();
        let mut writer = Writer::new(Endian::Little);
        settings
            .write_read_all(&mut writer, &["org.gnome.*"])
            .unwrap();
        let values = wire::read_body(
            writer.as_bytes(),
            "a{sa{sv}}",
            Endian::Little,
            Limits::NO_FDS,
        )
        .unwrap();
        // `org.gnome.*` selects exactly the one namespace GTK reads.
        let namespaces = values
            .first()
            .and_then(Value::as_seq)
            .unwrap()
            .values(1)
            .unwrap();
        assert_eq!(namespaces.len(), 1);
        let pair = namespaces
            .first()
            .and_then(Value::as_seq)
            .unwrap()
            .values(2)
            .unwrap();
        assert_eq!(pair.len(), 2);
        assert_eq!(pair.first().and_then(Value::as_str), Some(GNOME_INTERFACE));
        // Every org.gnome.desktop.interface key, pinned as a literal.
        let expected_entries = 8;
        let entries = pair
            .get(1)
            .and_then(Value::as_seq)
            .unwrap()
            .values(expected_entries)
            .unwrap();
        assert_eq!(entries.len(), expected_entries);
        let mut published = None;
        for entry in &entries {
            let entry = entry.as_seq().unwrap().values(2).unwrap();
            assert_eq!(entry.len(), 2);
            if entry.first().and_then(Value::as_str) == Some("text-scaling-factor") {
                let variant = entry.get(1).and_then(Value::as_seq).unwrap();
                assert_eq!(variant.signature(), "d");
                published = Some(variant.values(1).unwrap());
            }
        }
        assert_eq!(published, Some(vec![Value::Double(1.0)]));
    }

    #[test]
    fn namespace_filters_are_exact_or_trailing_section_globs() {
        assert!(matches_namespace(APPEARANCE, &[]));
        assert!(matches_namespace(APPEARANCE, &[""]));
        assert!(matches_namespace(APPEARANCE, &[APPEARANCE]));
        assert!(matches_namespace(APPEARANCE, &["org.freedesktop.*"]));
        assert!(!matches_namespace(APPEARANCE, &["org.gnome.*"]));
        assert!(!matches_namespace(APPEARANCE, &["org.*.appearance"]));
    }
}
