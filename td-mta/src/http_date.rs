//! Fixed-state HTTP dates, including allocation-refusal responses.
use crate::ports::Error;

pub(crate) struct HttpDate {
    weekday: &'static str,
    month: &'static str,
    day: u64,
    year: u64,
    hour: u64,
    minute: u64,
    second: u64,
}
impl std::fmt::Display for HttpDate {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
            self.weekday, self.day, self.month, self.year, self.hour, self.minute, self.second
        )
    }
}

pub(crate) fn http_date(utc_ms: i64) -> Result<HttpDate, Error> {
    let seconds = u64::try_from(utc_ms).map_err(|_| Error::Invalid)? / 1000;
    if seconds > 253402300799 {
        return Err(Error::Invalid);
    }
    let day = seconds / 86400;
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"]
        .get((day % 7) as usize)
        .copied()
        .ok_or(Error::Invalid)?;
    let mut remaining = day;
    let mut year = 1970;
    let leap = |year| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    loop {
        let days = if leap(year) { 366 } else { 365 };
        if remaining < days {
            break;
        }
        remaining -= days;
        year += 1;
    }
    let months = [
        ("Jan", 31),
        ("Feb", if leap(year) { 29 } else { 28 }),
        ("Mar", 31),
        ("Apr", 30),
        ("May", 31),
        ("Jun", 30),
        ("Jul", 31),
        ("Aug", 31),
        ("Sep", 30),
        ("Oct", 31),
        ("Nov", 30),
        ("Dec", 31),
    ];
    for (month, days) in months {
        if remaining < days {
            return Ok(HttpDate {
                weekday,
                month,
                day: remaining + 1,
                year,
                hour: seconds / 3600 % 24,
                minute: seconds / 60 % 60,
                second: seconds % 60,
            });
        }
        remaining -= days;
    }
    Err(Error::Invalid)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn date_uses_imf_fixdate_across_leap_and_century_boundaries() {
        for (millis, expected) in [
            (0, "Thu, 01 Jan 1970 00:00:00 GMT"),
            (951782400000, "Tue, 29 Feb 2000 00:00:00 GMT"),
            (4107542400000, "Mon, 01 Mar 2100 00:00:00 GMT"),
            (253402300799000, "Fri, 31 Dec 9999 23:59:59 GMT"),
        ] {
            let date = http_date(millis).unwrap();
            let mut bytes = [0; 29];
            let mut text = crate::bounded::TextBuffer::new(&mut bytes);
            text.format(format_args!("{date}")).unwrap();
            assert_eq!(text.as_str().unwrap(), expected);
        }
        assert!(http_date(-1).is_err());
        assert!(http_date(i64::MAX).is_err());
    }
}
