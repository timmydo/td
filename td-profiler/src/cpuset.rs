use std::collections::BTreeSet;

const MAX_CPU: u32 = 65_535;

pub fn parse(text: &str) -> Result<Vec<u32>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("CPU set is empty".into());
    }
    let mut cpus = BTreeSet::new();
    for field in text.split(',') {
        if field.is_empty() {
            return Err("CPU set contains an empty field".into());
        }
        let (first, last) = match field.split_once('-') {
            Some((first, last)) if !first.is_empty() && !last.is_empty() => {
                if last.contains('-') {
                    return Err(format!("CPU range has more than one dash: {field}"));
                }
                (number(first)?, number(last)?)
            }
            Some(_) => return Err(format!("CPU range is incomplete: {field}")),
            None => {
                let cpu = number(field)?;
                (cpu, cpu)
            }
        };
        if first > last {
            return Err(format!("CPU range runs backwards: {field}"));
        }
        if last.saturating_sub(first) > MAX_CPU {
            return Err(format!("CPU range is too large: {field}"));
        }
        cpus.extend(first..=last);
    }
    Ok(cpus.into_iter().collect())
}

fn number(text: &str) -> Result<u32, String> {
    if text.len() > 1 && text.starts_with('0') {
        return Err(format!("CPU number is not canonical: {text}"));
    }
    let value = text
        .parse::<u32>()
        .map_err(|_| format!("invalid CPU number: {text}"))?;
    if value > MAX_CPU {
        return Err(format!("CPU number exceeds {MAX_CPU}: {value}"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::parse;

    #[test]
    fn parses_linux_cpu_lists_into_sorted_unique_values() {
        assert_eq!(
            parse("0-3,8,10-12\n").unwrap(),
            vec![0, 1, 2, 3, 8, 10, 11, 12]
        );
        assert_eq!(parse("3,1-3").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn rejects_ambiguous_or_hostile_lists() {
        for bad in ["", "0,", "-1", "1-", "4-2", "01", "1-2-3", "65536"] {
            assert!(parse(bad).is_err(), "accepted {bad:?}");
        }
    }
}
