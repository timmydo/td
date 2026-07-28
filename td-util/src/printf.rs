//! `printf` — the `'%s\n' ARG` the boot scripts write markers with.
//!
//! Not a general printf. `%s`, `%%` and the backslash escapes are supported;
//! numeric conversions are REFUSED rather than approximated, because a marker
//! written by a format this misread is a boot state recorded wrong.

use std::io::Write;

pub fn run(args: &[String]) -> Result<u8, String> {
    let usage = "usage: printf FORMAT [ARG...]";
    let Some((format, operands)) = args.split_first() else {
        return Err(usage.to_string());
    };
    let text = render(format, operands)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    out.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(0)
}

/// POSIX reuses the format until the operands run out; with none it runs once
/// and `%s` is empty. Both matter: `printf '%s\n' a b` is two lines.
fn render(format: &str, operands: &[String]) -> Result<String, String> {
    let mut out = String::new();
    let mut next = 0usize;
    loop {
        let consumed = pass(format, operands, &mut next, &mut out)?;
        if next >= operands.len() || consumed == 0 {
            return Ok(out);
        }
    }
}

/// One pass over the format. Returns how many operands it consumed, so a format
/// with no conversions terminates instead of looping on the same operand.
fn pass(
    format: &str,
    operands: &[String],
    next: &mut usize,
    out: &mut String,
) -> Result<usize, String> {
    let mut consumed = 0usize;
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('\\') => out.push('\\'),
                // An unknown escape stays literal, as dash's printf does.
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '%' => match chars.next() {
                Some('s') => {
                    if let Some(a) = operands.get(*next) {
                        out.push_str(a);
                    }
                    *next = next.saturating_add(1);
                    consumed = consumed.saturating_add(1);
                }
                Some('%') => out.push('%'),
                Some(other) => {
                    return Err(format!(
                        "unsupported conversion '%{other}' (only %s and %% are served)"
                    ))
                }
                None => return Err("trailing '%' in format".to_string()),
            },
            other => out.push(other),
        }
    }
    Ok(consumed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn s(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    /// The one form every boot marker is written with.
    #[test]
    fn the_marker_form_renders_exactly() {
        assert_eq!(render("%s\\n", &s(&["td-rootcheck-v1"])), Ok("td-rootcheck-v1\n".to_string()));
        assert_eq!(render("%s\\n", &s(&["waiting"])), Ok("waiting\n".to_string()));
    }

    /// The format REPEATS while operands remain, and runs once with none.
    #[test]
    fn the_format_is_reused_until_the_operands_run_out() {
        assert_eq!(render("%s\\n", &s(&["a", "b"])), Ok("a\nb\n".to_string()));
        assert_eq!(render("%s\\n", &s(&[])), Ok("\n".to_string()));
        // No conversion: one pass, not an infinite loop on an unconsumed operand.
        assert_eq!(render("hi\\n", &s(&["a", "b"])), Ok("hi\n".to_string()));
    }

    /// A numeric conversion is refused, not guessed at.
    #[test]
    fn unsupported_conversions_are_refused() {
        for bad in ["%d", "%i\\n", "%x", "%"] {
            assert!(render(bad, &s(&["1"])).is_err(), "'{bad}' was accepted");
        }
        assert_eq!(render("100%%\\n", &s(&[])), Ok("100%\n".to_string()));
    }

    #[test]
    fn a_format_is_required() {
        assert!(run(&s(&[])).is_err());
    }
}
