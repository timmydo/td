//! `sleep` — the boot scripts' device-settle and park waits.

pub fn run(args: &[String]) -> Result<u8, String> {
    let usage = "usage: sleep SECONDS";
    let mut operands: Vec<&str> = Vec::new();
    for a in args {
        if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unrecognised option '{a}'\n{usage}"));
        }
        operands.push(a.as_str());
    }
    let (Some(spec), 1) = (operands.first(), operands.len()) else {
        return Err(usage.to_string());
    };
    std::thread::sleep(parse(spec)?);
    Ok(0)
}

/// Decimal seconds, no suffix: `1`, `300`, `0.2`, `.5`. A fraction is kept
/// exactly to the nanosecond rather than truncated, since a `0.5` read as 0
/// turns a settle wait into a spin and the caller's retry budget silently
/// expires at once. A suffix, a sign or a tenth fraction digit is refused.
fn parse(spec: &str) -> Result<std::time::Duration, String> {
    let bad =
        || format!("invalid interval '{spec}' (decimal seconds, no suffix)\nusage: sleep SECONDS");
    let (whole, frac) = spec.split_once('.').unwrap_or((spec, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (whole.is_empty() && frac.is_empty()) || !digits(whole) || !digits(frac) || frac.len() > 9 {
        return Err(bad());
    }
    let secs = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .map_err(|e| format!("invalid interval '{spec}': {e}"))?
    };
    let mut nanos: u32 = 0;
    for (i, b) in frac.bytes().enumerate() {
        let place = 10u32.pow(8 - u32::try_from(i).map_err(|_| bad())?);
        nanos += u32::from(b - b'0') * place;
    }
    Ok(std::time::Duration::new(secs, nanos))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn decimal_seconds_exactly() {
        use std::time::Duration;
        assert_eq!(parse("1"), Ok(Duration::from_secs(1)));
        assert_eq!(parse("300"), Ok(Duration::from_secs(300)));
        assert_eq!(parse("0"), Ok(Duration::ZERO));
        assert_eq!(parse("0.5"), Ok(Duration::from_millis(500)));
        assert_eq!(parse(".2"), Ok(Duration::from_millis(200)));
        assert_eq!(parse("2."), Ok(Duration::from_secs(2)));
        assert_eq!(parse("0.000000001"), Ok(Duration::from_nanos(1)));
        assert_eq!(parse("1.25"), Ok(Duration::from_millis(1250)));
        for bad in [
            "1s",
            "",
            ".",
            "-1",
            "1m",
            "abc",
            " 1",
            "0.5s",
            "1.2.3",
            "+1",
            "0.0000000001",
            "1e3",
        ] {
            assert!(parse(bad).is_err(), "'{bad}' was accepted as an interval");
        }
    }

    #[test]
    fn a_fractional_sleep_waits_its_fraction() {
        let start = std::time::Instant::now();
        assert_eq!(run(&["0.2".to_string()]), Ok(0));
        let waited = start.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(190),
            "sleep 0.2 returned after {waited:?}"
        );
    }

    /// The UNIT is seconds.
    ///
    /// `parse` returning 1 proves nothing about what `run` waits: `from_secs` ->
    /// `from_millis` left every other test green, and turns the /init device-settle
    /// loop (five iterations of `sleep 1`) into a 5 ms spin that expires the retry
    /// budget before /dev/vda appears — the same failure the rejection of `0.5`
    /// exists to prevent.
    #[test]
    fn a_one_second_sleep_really_waits_a_second() {
        let start = std::time::Instant::now();
        assert_eq!(run(&["1".to_string()]), Ok(0));
        let waited = start.elapsed();
        assert!(
            waited >= std::time::Duration::from_millis(900),
            "sleep 1 returned after {waited:?}; the unit is not seconds"
        );
    }

    #[test]
    fn exactly_one_operand() {
        let s = |l: &[&str]| l.iter().map(|a| (*a).to_string()).collect::<Vec<String>>();
        assert!(run(&s(&[])).is_err());
        assert!(run(&s(&["1", "2"])).is_err());
        assert!(run(&s(&["-x"])).is_err());
        assert_eq!(run(&s(&["0"])), Ok(0));
    }
}
