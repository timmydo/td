//! The library's pure half: the sidecar's grammar, what a roll lists, and
//! the rule that dates an import. Bytes and names come in, values and
//! lines go out; `main` is where files are opened and written.

use std::fmt;

use crate::tiff::{tag, Reader};

/// A sidecar's first line: the format's name and version.
pub const SIDECAR_HEADER: &str = "td-photo edit 1";
/// What a sidecar adds to its original's name.
pub const SIDECAR_SUFFIX: &str = ".edit";
/// A sidecar past this is refused as a whole.
pub const MAX_SIDECAR_BYTES: usize = 64 * 1024;
/// A sidecar past this many lines, the header counted, is refused as a
/// whole.
pub const MAX_SIDECAR_LINES: usize = 1024;
/// Exposure in hundredths of a stop, at most this magnitude either way.
pub const MAX_EXPOSURE: i32 = 500;
/// Crop fractions are ten-thousandths of the oriented image.
pub const CROP_UNIT: u32 = 10_000;
/// A crop edge is at least this long (0.05 of the image).
pub const MIN_CROP_EDGE: u32 = 500;
/// A look's stem is at most this many bytes.
pub const MAX_LOOK_STEM: usize = 64;
/// A sidecar key is at most this many bytes.
pub const MAX_KEY: usize = 32;
/// The folder an import files a photo under when it has no date.
pub const UNDATED: &str = "undated";
/// The folder under a roll that rejects are moved into.
pub const REJECTED: &str = "rejected";
/// The folder under a roll that exports are written into.
pub const EXPORTED: &str = "exported";

/// Why a sidecar, or one value for it, is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// More than `MAX_SIDECAR_BYTES`.
    TooLong,
    /// More than `MAX_SIDECAR_LINES`.
    TooManyLines,
    /// Not UTF-8.
    Utf8,
    /// A first line other than `SIDECAR_HEADER`.
    Header,
    /// The numbered line is not `key value`.
    Line(usize),
    /// A known key's value is outside its grammar.
    Value(Key),
    /// A known key appears twice.
    Repeated(Key),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "sidecar over {} bytes", MAX_SIDECAR_BYTES),
            Self::TooManyLines => write!(f, "sidecar over {} lines", MAX_SIDECAR_LINES),
            Self::Utf8 => f.write_str("sidecar is not UTF-8"),
            Self::Header => write!(f, "first line is not `{SIDECAR_HEADER}`"),
            Self::Line(number) => write!(f, "line {number} is not `key value`"),
            Self::Value(key) => write!(f, "malformed {} value", key.name()),
            Self::Repeated(key) => write!(f, "{} given twice", key.name()),
        }
    }
}

impl std::error::Error for Error {}

/// The keys this version knows. Any other key is kept verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Flag,
    Exposure,
    Crop,
    Look,
}

impl Key {
    pub const ALL: [Key; 4] = [Key::Flag, Key::Exposure, Key::Crop, Key::Look];

    pub fn name(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Exposure => "exposure",
            Self::Crop => "crop",
            Self::Look => "look",
        }
    }

    pub fn parse(name: &str) -> Option<Key> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }
}

/// A cull decision; absent is unflagged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flag {
    Pick,
    Reject,
}

impl Flag {
    pub fn word(self) -> &'static str {
        match self {
            Self::Pick => "pick",
            Self::Reject => "reject",
        }
    }

    pub fn parse(word: &str) -> Option<Flag> {
        match word {
            "pick" => Some(Self::Pick),
            "reject" => Some(Self::Reject),
            _ => None,
        }
    }
}

/// A crop as fractions of the oriented image, in ten-thousandths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Crop {
    /// Edges at least `MIN_CROP_EDGE`, the box inside the image.
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Result<Crop, Error> {
        let inside = |start: u32, length: u32| {
            start
                .checked_add(length)
                .is_some_and(|end| end <= CROP_UNIT)
        };
        if width < MIN_CROP_EDGE
            || height < MIN_CROP_EDGE
            || !inside(x, width)
            || !inside(y, height)
        {
            return Err(Error::Value(Key::Crop));
        }
        Ok(Crop {
            x,
            y,
            width,
            height,
        })
    }

    /// `x y w h`, each `D.DDDD`.
    pub fn parse(text: &str) -> Result<Crop, Error> {
        let mut fields = text.split(' ');
        let mut next = || -> Result<u32, Error> {
            fields
                .next()
                .and_then(|field| fraction(field, 4))
                .ok_or(Error::Value(Key::Crop))
        };
        let (x, y, width, height) = (next()?, next()?, next()?, next()?);
        if fields.next().is_some() {
            return Err(Error::Value(Key::Crop));
        }
        Crop::new(x, y, width, height)
    }

    /// The `crop` value as the sidecar writes it.
    pub fn text(self) -> String {
        format!(
            "{} {} {} {}",
            fixed(self.x, 4),
            fixed(self.y, 4),
            fixed(self.width, 4),
            fixed(self.height, 4)
        )
    }
}

/// Exposure from `-D.DD` or `D.DD`, in hundredths of a stop within
/// `MAX_EXPOSURE`; `-0.00` is not a spelling of zero.
pub fn exposure(text: &str) -> Result<i32, Error> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let magnitude = fraction(digits, 2)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value <= MAX_EXPOSURE && !(negative && *value == 0))
        .ok_or(Error::Value(Key::Exposure))?;
    Ok(if negative { -magnitude } else { magnitude })
}

/// The `exposure` value as the sidecar writes it.
pub fn exposure_text(hundredths: i32) -> String {
    let sign = if hundredths < 0 { "-" } else { "" };
    format!("{sign}{}", fixed(hundredths.unsigned_abs(), 2))
}

/// A look's file stem: 1 to `MAX_LOOK_STEM` bytes of ASCII letters, digits,
/// `-`, `_` and `.`, not starting with `.`.
pub fn valid_look(stem: &str) -> bool {
    !stem.is_empty()
        && stem.len() <= MAX_LOOK_STEM
        && !stem.starts_with('.')
        && stem
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// `D.F...`: one integer digit and exactly `decimals` fraction digits, as
/// an integer in units of `10^-decimals`.
fn fraction(text: &str, decimals: u32) -> Option<u32> {
    let (whole, part) = text.split_once('.')?;
    let digits = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole, 1) || !digits(part, decimals as usize) {
        return None;
    }
    let scale = 10u32.checked_pow(decimals)?;
    let whole: u32 = whole.parse().ok()?;
    let part: u32 = part.parse().ok()?;
    whole.checked_mul(scale)?.checked_add(part)
}

/// `value` in units of `10^-decimals` as `D.F...`.
fn fixed(value: u32, decimals: u32) -> String {
    let scale = 10u32.checked_pow(decimals).unwrap_or(u32::MAX);
    let width = decimals as usize;
    format!("{}.{:0width$}", value / scale, value % scale)
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY
        && key.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn check(key: Key, value: &str) -> Result<(), Error> {
    let ok = match key {
        Key::Flag => Flag::parse(value).is_some(),
        Key::Exposure => exposure(value).is_ok(),
        Key::Crop => Crop::parse(value).is_ok(),
        Key::Look => valid_look(value),
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Value(key))
    }
}

/// One photo's edits: the header, then `key value` lines in the order the
/// file had them. Known keys are validated; any other line is kept
/// verbatim and rewritten in place, so a later version's values survive
/// this one's edit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sidecar {
    lines: Vec<(String, String)>,
}

impl Sidecar {
    /// A sidecar's bytes, refused as a whole at the first fault.
    pub fn parse(bytes: &[u8]) -> Result<Sidecar, Error> {
        if bytes.len() > MAX_SIDECAR_BYTES {
            return Err(Error::TooLong);
        }
        let text = std::str::from_utf8(bytes).map_err(|_| Error::Utf8)?;
        // The newline ending the last line is not an empty line after it.
        let mut lines = text.strip_suffix('\n').unwrap_or(text).split('\n');
        if lines.next() != Some(SIDECAR_HEADER) {
            return Err(Error::Header);
        }
        let mut sidecar = Sidecar::default();
        for (index, line) in lines.enumerate() {
            let number = index + 2;
            if number > MAX_SIDECAR_LINES {
                return Err(Error::TooManyLines);
            }
            let (key, value) = line.split_once(' ').ok_or(Error::Line(number))?;
            if !valid_key(key)
                || value.is_empty()
                || value.starts_with(' ')
                || value.ends_with(' ')
                || value.chars().any(char::is_control)
            {
                return Err(Error::Line(number));
            }
            if let Some(known) = Key::parse(key) {
                if sidecar.get(known).is_some() {
                    return Err(Error::Repeated(known));
                }
                check(known, value)?;
            }
            sidecar.lines.push((key.to_string(), value.to_string()));
        }
        Ok(sidecar)
    }

    fn get(&self, key: Key) -> Option<&str> {
        self.lines
            .iter()
            .find(|(name, _)| name == key.name())
            .map(|(_, value)| value.as_str())
    }

    pub fn flag(&self) -> Option<Flag> {
        self.get(Key::Flag).and_then(Flag::parse)
    }

    /// Hundredths of a stop.
    pub fn exposure(&self) -> Option<i32> {
        self.get(Key::Exposure)
            .and_then(|value| exposure(value).ok())
    }

    pub fn crop(&self) -> Option<Crop> {
        self.get(Key::Crop)
            .and_then(|value| Crop::parse(value).ok())
    }

    pub fn look(&self) -> Option<&str> {
        self.get(Key::Look)
    }

    /// The value a known key has, as written.
    pub fn value(&self, key: Key) -> Option<&str> {
        self.get(key)
    }

    /// Every line, in order, known and unknown alike.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.lines
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Sets a known key in place, appending it if absent, or clears it
    /// with `None`. The value is held to the key's grammar, the same one
    /// `parse` holds a file to.
    pub fn set(&mut self, key: Key, value: Option<&str>) -> Result<(), Error> {
        let Some(value) = value else {
            self.lines.retain(|(name, _)| name != key.name());
            return Ok(());
        };
        check(key, value)?;
        match self.lines.iter_mut().find(|(name, _)| name == key.name()) {
            Some(line) => line.1 = value.to_string(),
            None => self.lines.push((key.name().to_string(), value.to_string())),
        }
        Ok(())
    }

    /// Back to the camera's defaults: exposure, crop and look cleared; the
    /// flag, a cull decision, and any unknown line kept.
    pub fn reset(&mut self) {
        self.lines
            .retain(|(name, _)| Key::parse(name).is_none_or(|key| key == Key::Flag));
    }

    /// The file's text: the header, then the lines, each ended by a newline.
    pub fn text(&self) -> String {
        let mut out = String::from(SIDECAR_HEADER);
        out.push('\n');
        for (key, value) in &self.lines {
            out.push_str(key);
            out.push(' ');
            out.push_str(value);
            out.push('\n');
        }
        out
    }
}

/// Whether a name is an original a roll lists: the `nef` or `NEF`
/// extension on a stem, so temporaries (`NAME.part`) and dotfiles are not,
/// and no control character, so a listing's rows stay rows.
pub fn is_original(name: &str) -> bool {
    name.strip_suffix(".nef")
        .or_else(|| name.strip_suffix(".NEF"))
        .is_some_and(|stem| !stem.is_empty() && !stem.starts_with('.'))
        && !name.chars().any(char::is_control)
}

/// The sidecar's name beside an original.
pub fn sidecar_name(original: &str) -> String {
    format!("{original}{SIDECAR_SUFFIX}")
}

/// Which photos a listing shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Filter {
    All,
    Picks,
    Rejects,
    Unflagged,
}

impl Filter {
    pub fn admits(self, flag: Option<Flag>) -> bool {
        match self {
            Self::All => true,
            Self::Picks => flag == Some(Flag::Pick),
            Self::Rejects => flag == Some(Flag::Reject),
            Self::Unflagged => flag.is_none(),
        }
    }
}

/// The roll folder an import files a photo under, from its
/// `DateTimeOriginal`: `YYYY/YYYY-MM-DD` when the value is exactly
/// `YYYY:MM:DD HH:MM:SS` naming a calendar date and a time of day, or
/// `undated` when it is absent or anything else (the blanks a camera whose
/// clock was never set writes, a truncated or suffixed value, February
/// 30th).
pub fn roll_folder(taken: Option<&str>) -> String {
    let Some(taken) = taken else {
        return UNDATED.to_string();
    };
    let bytes = taken.as_bytes();
    let number = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .filter(|digits| digits.iter().all(u8::is_ascii_digit))
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| digits.parse::<u32>().ok())
    };
    let separator = |at: usize, byte: u8| bytes.get(at) == Some(&byte);
    let shaped = bytes.len() == 19
        && separator(4, b':')
        && separator(7, b':')
        && separator(10, b' ')
        && separator(13, b':')
        && separator(16, b':');
    let date = (number(0..4), number(5..7), number(8..10));
    let time = (number(11..13), number(14..16), number(17..19));
    match (shaped, date, time) {
        (true, (Some(year), Some(month), Some(day)), (Some(hour), Some(minute), Some(second)))
            if (1900..=2999).contains(&year)
                && (1..=days_in(year, month)).contains(&day)
                && hour < 24
                && minute < 60
                && second < 60 =>
        {
            format!("{year:04}/{year:04}-{month:02}-{day:02}")
        }
        _ => UNDATED.to_string(),
    }
}

/// Days in a month of the Gregorian calendar, none for a month there is
/// not.
fn days_in(year: u32, month: u32) -> u32 {
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    }
}

/// `DateTimeOriginal` from a TIFF-shaped file's Exif IFD, when it has one.
/// Only the two IFDs on that path are read, so a file that is not a whole
/// NEF still dates itself.
pub fn taken(data: &[u8]) -> Option<String> {
    let (reader, first) = Reader::new(data, 0).ok()?;
    let ifd0 = reader.ifd(first).ok()?;
    let offset = reader.integer(ifd0.find(tag::EXIF_IFD)?, 0).ok()?;
    let exif = reader.ifd(offset).ok()?;
    let value = reader.ascii(exif.find(tag::DATE_TIME_ORIGINAL)?).ok()?;
    Some(value.to_string())
}
