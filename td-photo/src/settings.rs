//! The export settings' grammar: what the export view sets, the batch
//! export applies to every pick, and the user's settings file holds
//! between sessions. Bytes come in, text goes out; `main` reads and
//! writes the file.

use std::fmt;

use crate::image::MAX_AXIS;
use crate::jpeg::QUALITY;

/// The settings file's first line: the format's name and version.
pub const HEADER: &str = "td-photo export 1";
/// The settings file's name under the user's td-photo configuration.
pub const FILE: &str = "export";
/// A settings file past this is refused as a whole.
pub const MAX_BYTES: usize = 4096;
/// The longest long edge an export may be asked for: the largest buffer
/// axis, since an export is never enlarged past its source anyway.
pub const MAX_LONG_EDGE: u32 = MAX_AXIS as u32;
/// The long edge's word for the source's own size.
pub const FULL: &str = "full";

/// What every export is written with: the JPEG quality and the long edge
/// the image is shrunk to, `None` for the source's own size. Never
/// enlarged: a long edge past the source's is the source's.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Settings {
    /// 1 to 100, the encoder's scale of the standard tables.
    pub quality: u8,
    /// 1 to `MAX_LONG_EDGE`, or `None` for the source's own size.
    pub long_edge: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            quality: QUALITY,
            long_edge: None,
        }
    }
}

/// Why a settings file, or one value for it, is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// More than `MAX_BYTES`.
    TooLong,
    /// Not UTF-8.
    Utf8,
    /// A first line other than `HEADER`.
    Header,
    /// The numbered line is not `key value` over a key this version knows.
    Line(usize),
    /// The named key's value is outside its grammar.
    Value(&'static str),
    /// The named key appears twice.
    Repeated(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "settings over {MAX_BYTES} bytes"),
            Self::Utf8 => f.write_str("settings are not UTF-8"),
            Self::Header => write!(f, "first line is not `{HEADER}`"),
            Self::Line(number) => write!(f, "line {number} is not a known `key value`"),
            Self::Value(key) => write!(f, "malformed {key} value"),
            Self::Repeated(key) => write!(f, "{key} given twice"),
        }
    }
}

impl std::error::Error for Error {}

const QUALITY_KEY: &str = "quality";
const LONG_EDGE_KEY: &str = "long-edge";

/// A quality's text, or `None` when it is not 1 to 100 in plain decimal.
pub fn parse_quality(text: &str) -> Option<u8> {
    decimal(text)
        .and_then(|value| u8::try_from(value).ok())
        .filter(|quality| (1..=100).contains(quality))
}

/// A long edge's text, or `None` when it is neither `full` nor 1 to
/// `MAX_LONG_EDGE` in plain decimal; `Some(None)` for `full`.
pub fn parse_long_edge(text: &str) -> Option<Option<u32>> {
    if text == FULL {
        return Some(None);
    }
    decimal(text)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|edge| (1..=MAX_LONG_EDGE).contains(edge))
        .map(Some)
}

/// A long edge's word: the pixels, or `full`.
pub fn long_edge_text(long_edge: Option<u32>) -> String {
    long_edge.map_or_else(|| FULL.to_string(), |edge| edge.to_string())
}

/// Plain ASCII digits, no sign, no leading zero past a lone zero.
fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if text.len() > 1 && text.starts_with('0') {
        return None;
    }
    text.parse().ok()
}

impl Settings {
    /// A settings file's bytes, refused as a whole at the first fault: the
    /// header, then `quality N` and `long-edge N|full`, each at most once,
    /// a key left out at its default. An unknown key is refused: the file
    /// is td-photo's own, so one it does not know is another version's.
    pub fn parse(bytes: &[u8]) -> Result<Settings, Error> {
        if bytes.len() > MAX_BYTES {
            return Err(Error::TooLong);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Error::Utf8)?;
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            return Err(Error::Header);
        }
        let mut settings = Settings::default();
        let (mut quality, mut long_edge) = (false, false);
        for (index, line) in lines.enumerate() {
            let number = index + 2;
            let (key, value) = line.split_once(' ').ok_or(Error::Line(number))?;
            match key {
                QUALITY_KEY => {
                    if std::mem::replace(&mut quality, true) {
                        return Err(Error::Repeated(QUALITY_KEY));
                    }
                    settings.quality = parse_quality(value).ok_or(Error::Value(QUALITY_KEY))?;
                }
                LONG_EDGE_KEY => {
                    if std::mem::replace(&mut long_edge, true) {
                        return Err(Error::Repeated(LONG_EDGE_KEY));
                    }
                    settings.long_edge =
                        parse_long_edge(value).ok_or(Error::Value(LONG_EDGE_KEY))?;
                }
                _ => return Err(Error::Line(number)),
            }
        }
        Ok(settings)
    }

    /// The file's text: the header, then both keys, each ended by a
    /// newline; what `parse` reads back to the same settings.
    pub fn text(&self) -> String {
        format!(
            "{HEADER}\n{QUALITY_KEY} {}\n{LONG_EDGE_KEY} {}\n",
            self.quality,
            long_edge_text(self.long_edge)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_text_reads_back_and_a_key_left_out_is_its_default() {
        for settings in [
            Settings::default(),
            Settings {
                quality: 1,
                long_edge: Some(1),
            },
            Settings {
                quality: 100,
                long_edge: Some(MAX_LONG_EDGE),
            },
            Settings {
                quality: 75,
                long_edge: Some(2048),
            },
        ] {
            assert_eq!(Settings::parse(settings.text().as_bytes()), Ok(settings));
        }
        assert_eq!(
            Settings::parse(b"td-photo export 1\n"),
            Ok(Settings::default())
        );
        assert_eq!(
            Settings::parse(b"td-photo export 1\nlong-edge 1600\n"),
            Ok(Settings {
                quality: QUALITY,
                long_edge: Some(1600),
            })
        );
        assert_eq!(
            Settings::default().text(),
            "td-photo export 1\nquality 92\nlong-edge full\n"
        );
    }

    #[test]
    fn a_fault_refuses_the_file_whole() {
        let cases: [(&[u8], Error); 12] = [
            (b"", Error::Header),
            (b"td-photo edit 1\nquality 50\n", Error::Header),
            (b"td-photo export 1\nquality\n", Error::Line(2)),
            (b"td-photo export 1\nformat avif\n", Error::Line(2)),
            (b"td-photo export 1\nquality 0\n", Error::Value("quality")),
            (b"td-photo export 1\nquality 101\n", Error::Value("quality")),
            (b"td-photo export 1\nquality 092\n", Error::Value("quality")),
            (b"td-photo export 1\nquality +5\n", Error::Value("quality")),
            (
                b"td-photo export 1\nlong-edge 0\n",
                Error::Value("long-edge"),
            ),
            (
                b"td-photo export 1\nlong-edge 16385\n",
                Error::Value("long-edge"),
            ),
            (
                b"td-photo export 1\nquality 50\nquality 60\n",
                Error::Repeated("quality"),
            ),
            (b"td-photo export 1\nquality \xff\n", Error::Utf8),
        ];
        for (bytes, error) in cases {
            assert_eq!(Settings::parse(bytes), Err(error), "{bytes:?}");
        }
        let long = vec![b' '; MAX_BYTES + 1];
        assert_eq!(Settings::parse(&long), Err(Error::TooLong));
    }

    #[test]
    fn the_words_are_the_actions_grammar() {
        assert_eq!(parse_quality("1"), Some(1));
        assert_eq!(parse_quality("100"), Some(100));
        assert_eq!(parse_quality("0"), None);
        assert_eq!(parse_quality("-1"), None);
        assert_eq!(parse_quality(""), None);
        assert_eq!(parse_long_edge("full"), Some(None));
        assert_eq!(parse_long_edge("2048"), Some(Some(2048)));
        assert_eq!(parse_long_edge("16384"), Some(Some(16384)));
        assert_eq!(parse_long_edge("16385"), None);
        assert_eq!(parse_long_edge("Full"), None);
        assert_eq!(parse_long_edge("-"), None);
        assert_eq!(long_edge_text(None), "full");
        assert_eq!(long_edge_text(Some(1024)), "1024");
    }
}
