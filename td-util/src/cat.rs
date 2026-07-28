//! `cat` — concatenate files to stdout.
//!
//! uutils serves `/bin/cat`, but it is dynamically linked. Every caller here
//! runs where that is not safe to assume: the pre-pivot initramfs, which has no
//! loader at all, and the boot self-check, whose job is to report a broken
//! runtime closure rather than die of one.

use std::io::Write;

pub fn run(args: &[String]) -> Result<u8, String> {
    let files = parse(args)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (status, problems) = concat(&files, &mut out)?;
    out.flush().map_err(|e| e.to_string())?;
    for p in problems {
        crate::emit_err(&format!("{p}\n"));
    }
    Ok(status)
}

fn parse(args: &[String]) -> Result<Vec<&str>, String> {
    let mut files: Vec<&str> = Vec::new();
    for a in args {
        match a.as_str() {
            // `-` is stdin to most cats; td never uses it, and silently reading
            // stdin for an operand a caller meant as a path would hang a boot.
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unrecognised option '{other}'\nusage: cat FILE..."))
            }
            other => files.push(other),
        }
    }
    if files.is_empty() {
        return Err("usage: cat FILE...".to_string());
    }
    Ok(files)
}

/// Concatenate into OUT, returning the status and the diagnostics.
///
/// The diagnostics are RETURNED rather than written here, so a test can prove they
/// never reach the output stream. That is not fussiness: `/etc/bootsuccess` reads
/// `deployment=$(td-util cat /run/td-deployment 2>/dev/null)` and then tests it
/// non-empty, so a diagnostic on stdout would make a MISSING file look like a
/// deployment id and carry that string into `td-boot success`.
fn concat(files: &[&str], out: &mut impl Write) -> Result<(u8, Vec<String>), String> {
    let mut status = 0u8;
    let mut problems = Vec::new();
    for f in files {
        match std::fs::read(f) {
            // Bytes, not a String: a file this reads may hold any byte, and a
            // lossy decode would put U+FFFD into a boot marker's contents.
            Ok(bytes) => {
                if let Err(e) = out.write_all(&bytes) {
                    // A broken pipe is the reader leaving (`cat x | head`), not a
                    // failure of this program; `main`'s own writer tolerates it for
                    // the same reason, since `panic = "abort"` makes the alternative
                    // a SIGABRT.
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        return Ok((status, problems));
                    }
                    return Err(format!("{f}: {e}"));
                }
            }
            Err(e) => {
                problems.push(format!("cat: {f}: {e}"));
                status = 1;
            }
        }
    }
    Ok((status, problems))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn an_operand_is_required_and_options_are_refused() {
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["-n"])).is_err());
        assert_eq!(parse(&args(&["/a", "/b"])), Ok(vec!["/a", "/b"]));
    }

    /// A missing file sets the STATUS, keeps going, and its diagnostic stays OFF
    /// the output stream.
    ///
    /// All three halves matter and none was covered before: `/etc/bootsuccess`
    /// captures this applet's stdout into `$deployment` and tests it non-empty, so
    /// a diagnostic written there turns "the marker is missing" into a deployment
    /// id made of an error message — and the boot proceeds on it.
    #[test]
    fn a_missing_file_sets_the_status_without_stopping_or_polluting_stdout() {
        let d = std::env::temp_dir().join(format!("td-util-cat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let good = d.join("good");
        std::fs::write(&good, b"payload").unwrap();
        let missing = d.join("nope");
        let mut out: Vec<u8> = Vec::new();
        let (status, problems) = concat(
            &[&missing.to_string_lossy(), &good.to_string_lossy()],
            &mut out,
        )
        .unwrap();
        assert_eq!(status, 1, "a missing operand must set status 1");
        assert_eq!(
            out, b"payload",
            "the later file was not emitted (the loop stopped), or a diagnostic \
             reached stdout, which /etc/bootsuccess would read as a deployment id"
        );
        assert_eq!(problems.len(), 1, "the missing file was not reported at all");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// /proc files report st_size 0; reading must not short-circuit on that.
    #[test]
    fn a_zero_length_stat_still_yields_contents() {
        let mut out: Vec<u8> = Vec::new();
        let (status, problems) = concat(&["/proc/self/cmdline"], &mut out).unwrap();
        assert_eq!((status, problems.len()), (0, 0));
        assert!(!out.is_empty(), "/proc/self/cmdline came back empty");
    }
}
