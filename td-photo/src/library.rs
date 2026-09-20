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
/// The develop history's line keys: `step-1`, `step-2`, ...
pub const STEP_PREFIX: &str = "step-";
/// A history holds at most this many steps.
pub const MAX_STEPS: usize = 128;
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
    /// The numbered line is a `step-N` out of sequence, past `MAX_STEPS`,
    /// or not `on|off KEY VALUE` over a develop key.
    Step(usize),
    /// The history holds `MAX_STEPS`; a step must go before one comes.
    HistoryFull,
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
            Self::Step(number) => write!(f, "line {number} is not the next step"),
            Self::HistoryFull => write!(f, "history holds {MAX_STEPS} steps"),
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

    /// Whether the key is a develop setting, one the history records; the
    /// flag is a cull decision and is not.
    pub fn develops(self) -> bool {
        !matches!(self, Self::Flag)
    }
}

/// One step of a photo's develop history: a develop key set to a value or
/// cleared (`None`), and whether it is on. The settings in force are the
/// steps that are on, folded in order, the last word on each key winning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Step {
    pub on: bool,
    pub key: Key,
    pub value: Option<String>,
}

impl Step {
    /// The step as its line's value: `on|off KEY VALUE`, `-` a clear.
    pub fn text(&self) -> String {
        format!(
            "{} {} {}",
            if self.on { "on" } else { "off" },
            self.key.name(),
            self.value.as_deref().unwrap_or("-")
        )
    }

    /// The step a line's value spells, or `None` when it is not one.
    fn parse(text: &str) -> Option<Step> {
        let (state, rest) = text.split_once(' ')?;
        let on = match state {
            "on" => true,
            "off" => false,
            _ => return None,
        };
        let (name, value) = rest.split_once(' ')?;
        let key = Key::parse(name).filter(|key| key.develops())?;
        let value = if value == "-" {
            None
        } else {
            check(key, value).ok()?;
            Some(value.to_string())
        };
        Some(Step { on, key, value })
    }
}

/// The step number a `step-N` key names: `N` in decimal without a leading
/// zero, from 1.
fn step_number(key: &str) -> Option<usize> {
    let digits = key.strip_prefix(STEP_PREFIX)?;
    if digits.is_empty() || digits.starts_with('0') || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
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
    // A bare `-` is the clear sentinel everywhere a look is set (the verb,
    // the action, a history step), so it cannot name a look.
    !stem.is_empty()
        && stem != "-"
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
/// file had them, then the develop history's `step-N` lines. Known keys
/// are validated; any other line is kept verbatim and rewritten in place,
/// so a later version's values survive this one's edit. The develop keys
/// (exposure, crop, look) are the history's summary: what its steps that
/// are on fold to, rewritten from it whenever it changes, so a reader
/// that knows only the keys sees the settings in force. A file with a
/// history is read by it; one without and with develop keys (an earlier
/// write) seeds a step per key, so every edit from then on is a step.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Sidecar {
    lines: Vec<(String, String)>,
    steps: Vec<Step>,
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
            } else if key.starts_with(STEP_PREFIX) {
                let next = sidecar.steps.len() + 1;
                if step_number(key) != Some(next) || next > MAX_STEPS {
                    return Err(Error::Step(number));
                }
                let step = Step::parse(value).ok_or(Error::Step(number))?;
                sidecar.steps.push(step);
                continue;
            }
            sidecar.lines.push((key.to_string(), value.to_string()));
        }
        if sidecar.steps.is_empty() {
            sidecar.seed();
        } else {
            sidecar.derive();
        }
        Ok(sidecar)
    }

    /// The history of a file written before there was one: a step per
    /// develop key the file holds, in the file's order.
    fn seed(&mut self) {
        for (name, value) in &self.lines {
            if let Some(key) = Key::parse(name).filter(|key| key.develops()) {
                self.steps.push(Step {
                    on: true,
                    key,
                    value: Some(value.clone()),
                });
            }
        }
    }

    /// Rewrites the develop keys from the history: each the last value a
    /// step that is on gives it, set in place or appended, or dropped when
    /// no step sets it.
    fn derive(&mut self) {
        for key in Key::ALL.into_iter().filter(|key| key.develops()) {
            let value = self
                .steps
                .iter()
                .rev()
                .find(|step| step.on && step.key == key)
                .and_then(|step| step.value.clone());
            self.set_line(key, value.as_deref());
        }
    }

    fn set_line(&mut self, key: Key, value: Option<&str>) {
        let Some(value) = value else {
            self.lines.retain(|(name, _)| name != key.name());
            return;
        };
        match self.lines.iter_mut().find(|(name, _)| name == key.name()) {
            Some(line) => line.1 = value.to_string(),
            None => self.lines.push((key.name().to_string(), value.to_string())),
        }
    }

    /// The develop history, oldest first.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Takes the last step back, `false` when there is none.
    pub fn undo(&mut self) -> bool {
        let taken = self.steps.pop().is_some();
        if taken {
            self.derive();
        }
        taken
    }

    /// Turns step `index` off or back on; `false` when there is no such
    /// step.
    pub fn toggle_step(&mut self, index: usize) -> bool {
        let Some(step) = self.steps.get_mut(index) else {
            return false;
        };
        step.on = !step.on;
        self.derive();
        true
    }

    /// Deletes step `index`, the later ones closing up; `false` when there
    /// is no such step.
    pub fn delete_step(&mut self, index: usize) -> bool {
        if index >= self.steps.len() {
            return false;
        }
        self.steps.remove(index);
        self.derive();
        true
    }

    /// What the lines take as text after the header, counted without
    /// making it: what a roll's sidecar budget holds.
    pub fn bytes(&self) -> usize {
        let lines: usize = self
            .lines
            .iter()
            .map(|(key, value)| key.len() + value.len() + 2)
            .sum();
        let steps: usize = self
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                // `step-N on|off KEY VALUE\n`, the value `-` when cleared.
                let state = if step.on { "on".len() } else { "off".len() };
                let value = step.value.as_ref().map_or(1, String::len);
                STEP_PREFIX.len()
                    + decimal_width(index + 1)
                    + 1
                    + state
                    + 1
                    + step.key.name().len()
                    + 1
                    + value
                    + 1
            })
            .sum();
        lines + steps
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

    /// Every line but the history's, in order, known and unknown alike.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.lines
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// Sets a known key, or clears it with `None`. The flag is set in
    /// place, appended if absent. A develop key is a step of the history:
    /// when the last step is on and sets the same key to a value, that
    /// step takes the new value (a run of exposure nudges, crops or looks
    /// is one step, undone as one); otherwise one is added, and a full
    /// history refuses it. A clear is its own step, never taken up and
    /// never taking a value up, so undoing it brings the value back. A
    /// value the key already holds is no step. The keys are rewritten
    /// from the history. The value is held to the key's grammar, the same
    /// one `parse` holds a file to.
    pub fn set(&mut self, key: Key, value: Option<&str>) -> Result<(), Error> {
        if let Some(value) = value {
            check(key, value)?;
        }
        if !key.develops() {
            self.set_line(key, value);
            return Ok(());
        }
        if self.get(key) == value {
            return Ok(());
        }
        let value = value.map(str::to_string);
        match self.steps.last_mut() {
            Some(last) if last.on && last.key == key && last.value.is_some() && value.is_some() => {
                last.value = value;
            }
            _ => {
                if self.steps.len() >= MAX_STEPS {
                    return Err(Error::HistoryFull);
                }
                self.steps.push(Step {
                    on: true,
                    key,
                    value,
                });
            }
        }
        self.derive();
        Ok(())
    }

    /// Back to the camera's defaults: the history cleared, and with it
    /// exposure, crop and look; the flag, a cull decision, and any unknown
    /// line kept.
    pub fn reset(&mut self) {
        self.steps.clear();
        self.lines
            .retain(|(name, _)| Key::parse(name).is_none_or(|key| key == Key::Flag));
    }

    /// The file's text: the header, then the lines, then the history's
    /// `step-N` lines, each ended by a newline.
    pub fn text(&self) -> String {
        let mut out = String::from(SIDECAR_HEADER);
        out.push('\n');
        for (key, value) in &self.lines {
            out.push_str(key);
            out.push(' ');
            out.push_str(value);
            out.push('\n');
        }
        for (index, step) in self.steps.iter().enumerate() {
            out.push_str(STEP_PREFIX);
            out.push_str(&(index + 1).to_string());
            out.push(' ');
            out.push_str(&step.text());
            out.push('\n');
        }
        out
    }
}

/// The digits a number takes in decimal.
fn decimal_width(mut n: usize) -> usize {
    let mut width = 1;
    while n >= 10 {
        n /= 10;
        width += 1;
    }
    width
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
    /// The filter's word, as the window's `state` and filter strip spell
    /// it; for
    /// the three `list` can switch to, its switch is `--WORD`.
    pub fn word(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Picks => "picks",
            Self::Rejects => "rejects",
            Self::Unflagged => "unflagged",
        }
    }

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
