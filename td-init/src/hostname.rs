//! `hostname` — print or set the kernel hostname.
//!
//! uutils' coreutils ships no `hostname` at all, and the flag td's boot glue
//! actually needs is `-F FILE`: sysinit reads `/etc/hostname` and calls
//! `sethostname(2)`. That is the single reason `hostname` is still busybox on
//! the image, so this applet covers the set paths (`-F FILE`, `NAME`) and the
//! print paths (`hostname`, `-s`).
//!
//! Setting goes through the syscall, not a write to `/proc/sys/kernel/hostname`,
//! because sysinit may run before `/proc` is mounted. PRINTING reads `/proc`:
//! Linux has no `gethostname` syscall, and widening the surface with `uname(2)`
//! for an interactive convenience is not a trade worth making.

use crate::sys;

const HOSTNAME_PROC: &str = "/proc/sys/kernel/hostname";

/// `__NEW_UTS_LEN` — the kernel rejects anything longer with EINVAL. Checked
/// here so the failure names the limit instead of surfacing a bare errno.
const MAX_LEN: usize = 64;

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Print { short: bool },
    SetLiteral(String),
    SetFromFile(String),
}

fn usage() -> String {
    "usage: hostname [-s] | hostname NAME | hostname -F FILE".to_string()
}

fn plan(args: &[String]) -> Result<Plan, String> {
    let mut short = false;
    let mut file: Option<String> = None;
    let mut name: Option<String> = None;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "-s" | "--short" => short = true,
            "-F" | "--file" => {
                let f = rest.next().ok_or_else(|| {
                    format!("option '{a}' needs a FILE argument\n{}", usage())
                })?;
                if file.is_some() {
                    return Err(format!("'{a}' given twice\n{}", usage()));
                }
                file = Some(f.clone());
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unrecognised option '{other}'\n{}", usage()))
            }
            other => {
                if name.is_some() {
                    return Err(format!("only one NAME may be set\n{}", usage()));
                }
                name = Some(other.to_string());
            }
        }
    }
    match (file, name) {
        (Some(_), Some(_)) => Err(format!("-F and NAME are exclusive\n{}", usage())),
        (Some(_), None) if short => Err(format!("-s applies to printing, not -F\n{}", usage())),
        (Some(f), None) => Ok(Plan::SetFromFile(f)),
        (None, Some(_)) if short => {
            Err(format!("-s applies to printing, not setting\n{}", usage()))
        }
        (None, Some(n)) => Ok(Plan::SetLiteral(n)),
        (None, None) => Ok(Plan::Print { short }),
    }
}

/// The first line that carries content: blank lines and `#` comments are skipped,
/// matching what every distribution's `/etc/hostname` reader does.
fn from_file_text(text: &str) -> Result<String, String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        return Ok(line.to_string());
    }
    Err("no hostname line in the file".to_string())
}

/// Reject locally what the kernel would reject with a bare EINVAL, plus the
/// embedded whitespace that would silently produce an unusable hostname.
fn validate(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("the hostname is empty".to_string());
    }
    if name.len() > MAX_LEN {
        return Err(format!(
            "the hostname is {} bytes; the kernel limit is {MAX_LEN}",
            name.len()
        ));
    }
    if name.chars().any(|c| c.is_whitespace() || c == '\0') {
        return Err(format!("the hostname '{name}' contains whitespace"));
    }
    Ok(())
}

fn short_form(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

fn set(name: &str) -> Result<u8, String> {
    validate(name)?;
    sys::sethostname(name.as_bytes())
        .map_err(|e| format!("sethostname('{name}'): {e}"))?;
    Ok(0)
}

pub fn run(args: &[String]) -> Result<u8, String> {
    match plan(args)? {
        Plan::Print { short } => {
            let raw = std::fs::read_to_string(HOSTNAME_PROC)
                .map_err(|e| format!("{HOSTNAME_PROC}: {e}"))?;
            let current = raw.trim();
            let shown = if short { short_form(current) } else { current };
            crate::emit(&format!("{shown}\n"))?;
            Ok(0)
        }
        Plan::SetLiteral(name) => set(&name),
        Plan::SetFromFile(path) => {
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            let name = from_file_text(&text).map_err(|e| format!("{path}: {e}"))?;
            set(&name)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn no_arguments_prints_and_dash_s_prints_short() {
        assert_eq!(plan(&argv(&[])), Ok(Plan::Print { short: false }));
        assert_eq!(plan(&argv(&["-s"])), Ok(Plan::Print { short: true }));
        assert_eq!(short_form("td.example.org"), "td");
        assert_eq!(short_form("td"), "td");
    }

    #[test]
    fn a_bare_name_sets_and_dash_f_reads_a_file() {
        assert_eq!(plan(&argv(&["td"])), Ok(Plan::SetLiteral("td".into())));
        assert_eq!(
            plan(&argv(&["-F", "/etc/hostname"])),
            Ok(Plan::SetFromFile("/etc/hostname".into()))
        );
        assert_eq!(
            plan(&argv(&["--file", "/etc/hostname"])),
            Ok(Plan::SetFromFile("/etc/hostname".into()))
        );
    }

    /// `-F` with no operand must not silently become a print: the caller asked to
    /// SET, and a boot that quietly kept the default hostname is the harder bug.
    #[test]
    fn ambiguous_or_incomplete_invocations_are_rejected() {
        assert!(plan(&argv(&["-F"])).is_err());
        assert!(plan(&argv(&["-F", "/etc/hostname", "td"])).is_err());
        assert!(plan(&argv(&["td", "other"])).is_err());
        assert!(plan(&argv(&["-s", "td"])).is_err());
        assert!(plan(&argv(&["-q"])).is_err());
        assert!(plan(&argv(&["-F", "a", "-F", "b"])).is_err());
    }

    #[test]
    fn the_file_reader_skips_blanks_and_comments() {
        assert_eq!(from_file_text("td\n").unwrap(), "td");
        assert_eq!(from_file_text("\n\n  # a comment\n  td.local \n").unwrap(), "td.local");
        assert!(from_file_text("").is_err());
        assert!(from_file_text("# only a comment\n").is_err());
    }

    #[test]
    fn validation_rejects_what_the_kernel_would_and_what_it_would_not() {
        assert!(validate("td").is_ok());
        assert!(validate("").is_err());
        assert!(validate(&"x".repeat(MAX_LEN)).is_ok());
        assert!(validate(&"x".repeat(MAX_LEN + 1)).is_err());
        // The kernel would happily accept these; an unusable hostname is worse
        // than a diagnosed one.
        assert!(validate("two words").is_err());
        assert!(validate("td\n").is_err());
    }
}
