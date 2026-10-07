//! `uname` — print system information, as GNU's: the fields selected by
//! `-s -n -r -v -m -p -i -o` (or their long names), always in that order and
//! space-separated; no option means `-s`, and `-a` prints all but a `-p`/`-i`
//! that is unknown.
//!
//! std has no uname(2), so the kernel's fields come from /proc/sys/kernel. The
//! machine is the architecture this binary was compiled for, which is the one
//! it is running on. The processor and hardware platform are `unknown`, as
//! upstream GNU reports them on Linux.

const USAGE: &str = "usage: uname [-asnrvmpio]";
const UNKNOWN: &str = "unknown";

/// The fields in output order: their short flag and long name.
const FIELDS: &[(char, &str)] = &[
    ('s', "--kernel-name"),
    ('n', "--nodename"),
    ('r', "--kernel-release"),
    ('v', "--kernel-version"),
    ('m', "--machine"),
    ('p', "--processor"),
    ('i', "--hardware-platform"),
    ('o', "--operating-system"),
];

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut want = vec![false; FIELDS.len()];
    let mut all = false;
    let mut set = |flag: char| match FIELDS.iter().position(|(c, _)| *c == flag) {
        Some(i) => {
            if let Some(w) = want.get_mut(i) {
                *w = true;
            }
            Ok(())
        }
        None => Err(format!("unrecognised option '-{flag}'\n{USAGE}")),
    };
    let mut operands = false;
    for a in args {
        if operands {
            return Err(format!("extra operand '{a}'\n{USAGE}"));
        } else if a == "--" {
            operands = true;
        } else if a == "--all" {
            all = true;
        } else if let Some((flag, _)) = FIELDS.iter().find(|(_, long)| long == a) {
            set(*flag)?;
        } else if let Some(flags) = a
            .strip_prefix('-')
            .filter(|f| !f.is_empty() && !a.starts_with("--"))
        {
            for c in flags.chars() {
                if c == 'a' {
                    all = true;
                } else {
                    set(c)?;
                }
            }
        } else {
            return Err(format!("extra operand '{a}'\n{USAGE}"));
        }
    }
    let fields = select(&want, all, field)?;
    crate::emit(&format!("{}\n", fields.join(" ")))?;
    Ok(0)
}

/// The values to print, in field order. `-a` drops an unknown processor or
/// hardware platform even where `-p`/`-i` names it, as GNU does; no field
/// selected means the kernel name.
fn select(
    want: &[bool],
    all: bool,
    value: impl Fn(char) -> Result<String, String>,
) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    for (i, (flag, _)) in FIELDS.iter().enumerate() {
        if !(all || want.get(i).copied().unwrap_or(false)) {
            continue;
        }
        let v = value(*flag)?;
        if all && matches!(flag, 'p' | 'i') && v == UNKNOWN {
            continue;
        }
        fields.push(v);
    }
    if fields.is_empty() {
        fields.push(value('s')?);
    }
    Ok(fields)
}

fn field(flag: char) -> Result<String, String> {
    let kernel = |name: &str| {
        let path = format!("/proc/sys/kernel/{name}");
        std::fs::read_to_string(&path)
            .map(|s| s.trim_end_matches('\n').to_string())
            .map_err(|e| format!("{path}: {e}"))
    };
    match flag {
        's' => kernel("ostype"),
        'n' => kernel("hostname"),
        'r' => kernel("osrelease"),
        'v' => kernel("version"),
        'm' => Ok(std::env::consts::ARCH.to_string()),
        'o' => Ok("GNU/Linux".to_string()),
        _ => Ok(UNKNOWN.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{run, select, FIELDS};

    /// Fields named by flag, with a host literally called `unknown`.
    fn pick(flags: &str, all: bool) -> String {
        let want: Vec<bool> = FIELDS.iter().map(|(c, _)| flags.contains(*c)).collect();
        let fake = |c: char| {
            Ok(match c {
                'n' => "unknown".to_string(),
                'p' | 'i' => "unknown".to_string(),
                c => c.to_string(),
            })
        };
        select(&want, all, fake).unwrap_or_default().join(" ")
    }

    #[test]
    fn all_drops_only_an_unknown_processor_and_platform() {
        assert_eq!(pick("", true), "s unknown r v m o");
        assert_eq!(pick("pi", true), "s unknown r v m o");
        assert_eq!(pick("p", false), "unknown");
        assert_eq!(pick("pi", false), "unknown unknown");
        assert_eq!(pick("sp", false), "s unknown");
        assert_eq!(pick("n", false), "unknown");
        assert_eq!(pick("", false), "s");
        assert_eq!(pick("ms", false), "s m");
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_and_operands() {
        assert_eq!(run(&args(&["-x"])).ok(), None);
        assert_eq!(run(&args(&["--", "-m"])).ok(), None);
        assert_eq!(run(&args(&["extra"])).ok(), None);
        assert_eq!(run(&args(&["-"])).ok(), None);
        assert_eq!(run(&args(&["-m", "--processor", "--"])).ok(), Some(0));
    }
}
