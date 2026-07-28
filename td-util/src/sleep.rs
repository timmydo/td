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
    let secs = parse(spec)?;
    std::thread::sleep(std::time::Duration::from_secs(secs));
    Ok(0)
}

/// Whole seconds, no suffix. td issues `1` and `300`. A fractional or suffixed
/// operand is refused rather than truncated: a `0.5` read as 0 turns a settle
/// wait into a spin, and the caller's retry budget silently expires at once.
fn parse(spec: &str) -> Result<u64, String> {
    if spec.is_empty() || !spec.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "invalid interval '{spec}' (whole seconds, no suffix)\nusage: sleep SECONDS"
        ));
    }
    spec.parse().map_err(|e| format!("invalid interval '{spec}': {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn whole_seconds_only() {
        assert_eq!(parse("1"), Ok(1));
        assert_eq!(parse("300"), Ok(300));
        assert_eq!(parse("0"), Ok(0));
        for bad in ["0.5", "1s", "", "-1", "1m", "abc", " 1"] {
            assert!(parse(bad).is_err(), "'{bad}' was accepted as an interval");
        }
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
