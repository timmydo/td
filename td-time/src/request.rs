//! Bounded time settings shared with the secure-attention description.
#[path = "../../td-civil/src/tzif.rs"]
#[allow(dead_code, reason = "shared calendar and timezone validation")]
mod calendar;

pub const MIN_EPOCH: u64 = 1_577_836_800;
pub const MAX_EPOCH: u64 = 4_102_444_799;
pub const MAX_VALUE: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Change {
    Utc(u64),
    Timezone(String),
    Automatic(bool),
}

impl Change {
    pub fn parse(action: &str, value: &str) -> Result<Self, String> {
        match action {
            "set" => parse_utc(value).map(Self::Utc),
            "timezone" if timezone_name(value) => Ok(Self::Timezone(value.into())),
            "automatic" if value == "on" => Ok(Self::Automatic(true)),
            "automatic" if value == "off" => Ok(Self::Automatic(false)),
            _ => Err(
                "expected set YYYY-MM-DDTHH:MM:SSZ, timezone Area/City, or automatic on|off".into(),
            ),
        }
    }

    pub fn action(&self) -> &'static str {
        match self {
            Self::Utc(_) => "set",
            Self::Timezone(_) => "timezone",
            Self::Automatic(_) => "automatic",
        }
    }

    pub fn value(&self) -> Result<String, String> {
        match self {
            Self::Utc(epoch) => format_utc(*epoch),
            Self::Timezone(name) if timezone_name(name) => Ok(name.clone()),
            Self::Timezone(_) => Err("invalid timezone name".into()),
            Self::Automatic(on) => Ok(if *on { "on" } else { "off" }.into()),
        }
    }

    pub fn encode(&self) -> Result<String, String> {
        Ok(format!("{} {}", self.action(), self.value()?))
    }

    pub fn decode(value: &str) -> Result<Self, String> {
        if value.len() > MAX_VALUE + 10 {
            return Err("time setting is too long".into());
        }
        let (action, value) = value.split_once(' ').ok_or("invalid time setting")?;
        Self::parse(action, value)
    }
}

pub fn timezone_name(name: &str) -> bool {
    let parts: Vec<_> = name.split('/').collect();
    name.len() <= MAX_VALUE
        && (2..=3).contains(&parts.len())
        && parts.iter().all(|part| {
            part.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_+-".contains(&byte))
        })
}

fn number(bytes: &[u8], start: usize, end: usize) -> Result<u32, String> {
    let part = bytes.get(start..end).ok_or("incomplete UTC time")?;
    if !part.iter().all(u8::is_ascii_digit) {
        return Err("UTC time needs decimal digits".into());
    }
    part.iter().try_fold(0u32, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(*byte - b'0')))
            .ok_or_else(|| "UTC component overflow".into())
    })
}

pub fn parse_utc(text: &str) -> Result<u64, String> {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || [
            (4, b'-'),
            (7, b'-'),
            (10, b'T'),
            (13, b':'),
            (16, b':'),
            (19, b'Z'),
        ]
        .iter()
        .any(|(index, byte)| bytes.get(*index) != Some(byte))
    {
        return Err("UTC time must be YYYY-MM-DDTHH:MM:SSZ".into());
    }
    let year = i64::from(number(bytes, 0, 4)?);
    let month = u8::try_from(number(bytes, 5, 7)?).map_err(|_| "invalid month")?;
    let day = u8::try_from(number(bytes, 8, 10)?).map_err(|_| "invalid day")?;
    let hour = u64::from(number(bytes, 11, 13)?);
    let minute = u64::from(number(bytes, 14, 16)?);
    let second = u64::from(number(bytes, 17, 19)?);
    if !(2020..=2099).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("UTC time is outside the supported calendar".into());
    }
    let days =
        i64::try_from(calendar::days_from_civil(year, month, day)).map_err(|_| "date overflow")?;
    if calendar::civil_from_days(days) != (year, month, day) {
        return Err("invalid calendar date".into());
    }
    let days = u64::try_from(days).map_err(|_| "date before epoch")?;
    Ok(days * 86_400 + hour * 3600 + minute * 60 + second)
}

pub fn format_utc(epoch: u64) -> Result<String, String> {
    if !(MIN_EPOCH..=MAX_EPOCH).contains(&epoch) {
        return Err("UTC time must be in 2020 through 2099".into());
    }
    let days = i64::try_from(epoch / 86_400).map_err(|_| "date overflow")?;
    let (year, month, day) = calendar::civil_from_days(days);
    let seconds = epoch % 86_400;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn calendar_is_exact_at_bounds_leap_days_and_ntp_era_boundary() {
        for text in [
            "2020-01-01T00:00:00Z",
            "2024-02-29T12:34:56Z",
            "2036-02-07T06:28:16Z",
            "2099-12-31T23:59:59Z",
        ] {
            assert_eq!(format_utc(parse_utc(text).unwrap()).unwrap(), text);
        }
        for text in [
            "2019-12-31T23:59:59Z",
            "2100-01-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-10-30T24:00:00Z",
            "2026-10-30T06:11:60Z",
            "2026-10-30T06:11:00",
            "2026-10-30 06:11:00Z",
        ] {
            assert!(parse_utc(text).is_err(), "{text}");
        }
    }

    #[test]
    fn settings_round_trip_and_refuse_path_and_frame_injection() {
        for text in [
            "set 2026-10-30T06:11:00Z",
            "timezone America/Los_Angeles",
            "automatic on",
            "automatic off",
        ] {
            assert_eq!(Change::decode(text).unwrap().encode().unwrap(), text);
        }
        for text in [
            "timezone ../UTC",
            "timezone /etc/passwd",
            "timezone America/../UTC",
            "automatic on\n",
            "automatic true",
            "set 0",
            "timezone Etc/UTC\0",
            "timezone Etc/UTC extra",
        ] {
            assert!(Change::decode(text).is_err(), "{text:?}");
        }
    }
}
