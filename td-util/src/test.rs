//! `test` — the conditional the boot scripts branch on.
//!
//! Exit status IS the answer: 0 true, 1 false, 2 for a malformed expression, which
//! POSIX keeps distinct from "false" so that a typo cannot read as one. Every caller
//! is a `while`/`if`/`&&` in a boot script, so a wrong answer here is a boot that
//! takes the other branch and says nothing.
//!
//! `-r`, `-w` and `-x` are refused rather than answered (see `unary`). Refusing `-w`
//! is why this applet needed no access(2) amendment: /etc/rootcheck writes and checks
//! the outcome instead of asking.
use std::path::Path;

/// Errors become exit 2 here rather than propagating as `Err` for the dispatcher to
/// turn into exit 1. For every other applet 1 means "failed"; for `test` it means
/// FALSE, so `test -w /var` handed back as `Err` would reach the shell as a confident
/// "not writable" and PASS the check the refusal exists to stop.
pub fn run(args: &[String]) -> Result<u8, String> {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match eval(&argv) {
        Ok(status) => Ok(status),
        Err(msg) => {
            crate::emit_err(&format!("test: {msg}\n"));
            Ok(2)
        }
    }
}

/// POSIX `test`, by ARGUMENT COUNT — the shape the standard specifies, because
/// operator-first parsing mis-reads operands that look like operators
/// (`test = = =` is a true string comparison).
fn eval(argv: &[&str]) -> Result<u8, String> {
    match argv {
        [] => Ok(1),
        // One operand: true when non-empty. `test -n` alone lands here, and "-n" is
        // non-empty, which is what POSIX says.
        [a] => Ok(truth(!a.is_empty())),
        ["!", a] => Ok(truth(a.is_empty())),
        [op, a] => unary(op, a),
        // Three operands: a binary operator in the middle WINS over a leading `!`,
        // so `test ! = x` compares rather than negating.
        [a, op, b] if is_binary(op) => binary(a, op, b),
        ["!", op, a] => unary(op, a).map(flip),
        ["!", a, op, b] if is_binary(op) => binary(a, op, b).map(flip),
        // `-a`/`-o` are deliberately not served: POSIX itself deprecates them as
        // ambiguous, and every caller here chains with the shell's own `&&`/`||`,
        // which cannot be misparsed.
        _ => Err(format!("unknown expression\n{}", usage())),
    }
}

/// LOAD-BEARING, not documentation: `unary` gates on these and `is_binary` is nothing
/// but a lookup, so an operator deleted here stops working. The recipe scan reads the
/// three lists out of this file rather than restating them, so a call site using an
/// operator this applet dropped reds at build time.
const UNARY: &[&str] = &["-b", "-d", "-e", "-f", "-s", "-n", "-z"];
const BINARY: &[&str] = &["=", "!=", "-eq", "-ne", "-lt", "-le", "-gt", "-ge"];
const REFUSED: &[&str] = &["-r", "-w", "-x"];

fn usage() -> String {
    "usage: test EXPRESSION  (-b -d -e -f -n -s -z, = != -eq -ne -lt -le -gt -ge, !)"
        .to_string()
}

fn truth(b: bool) -> u8 {
    u8::from(!b)
}

fn flip(status: u8) -> u8 {
    truth(status != 0)
}

fn is_binary(op: &str) -> bool {
    BINARY.contains(&op)
}

fn unary(op: &str, arg: &str) -> Result<u8, String> {
    // The credential-dependent trio, refused as one. All three are access(2), and a
    // mode-bits stand-in is wrong in the direction that matters: an unprivileged
    // caller reads root-owned 0755 `/var` as writable and a root-owned 0400 file as
    // readable, because the owner bit is set and the caller is not the owner.
    if REFUSED.contains(&op) {
        return Err(format!(
            "'{op}' is not served: it is access(2), which this multicall cannot reach, \
             and a mode-bits answer would be wrong for any caller that does not own the \
             path — do the operation and check the outcome, as /etc/rootcheck's write \
             probes do"
        ));
    }
    if !UNARY.contains(&op) {
        return Err(format!("unknown operator '{op}'\n{}", usage()));
    }
    let path = Path::new(arg);
    // symlink_metadata for -e only would diverge from POSIX, which FOLLOWS links
    // for every one of these; a dangling link is `! -e`.
    let meta = std::fs::metadata(path);
    let answer = match op {
        "-e" => meta.is_ok(),
        "-f" => meta.as_ref().is_ok_and(std::fs::Metadata::is_file),
        "-d" => meta.as_ref().is_ok_and(std::fs::Metadata::is_dir),
        "-s" => meta.as_ref().is_ok_and(|m| m.len() > 0),
        "-b" => meta.as_ref().is_ok_and(is_block_device),
        "-n" => !arg.is_empty(),
        "-z" => arg.is_empty(),
        // Unreachable: UNARY gated above. Kept as the exhaustiveness arm rather than
        // an `unreachable!`, which the no-panic rule forbids.
        other => return Err(format!("unknown operator '{other}'\n{}", usage())),
    };
    Ok(truth(answer))
}

fn is_block_device(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    m.file_type().is_block_device()
}

fn binary(a: &str, op: &str, b: &str) -> Result<u8, String> {
    if op == "=" {
        return Ok(truth(a == b));
    }
    if op == "!=" {
        return Ok(truth(a != b));
    }
    // Integer comparison. A non-numeric operand is an ERROR, not false: `test
    // "$n" -lt 5` with an unset `n` must not read as "0 < 5" and let a retry loop
    // run forever.
    let x = number(a)?;
    let y = number(b)?;
    let answer = match op {
        "-eq" => x == y,
        "-ne" => x != y,
        "-lt" => x < y,
        "-le" => x <= y,
        "-gt" => x > y,
        "-ge" => x >= y,
        other => return Err(format!("unknown operator '{other}'\n{}", usage())),
    };
    Ok(truth(answer))
}

/// `trim()` because busybox and dash both accept ` 1 `, and this landing SWAPS the
/// implementation under every live boot conditional — a divergence that turns a
/// previously-true comparison into exit 2 is a boot regression, not a strictness win.
/// An operand that is not an integer at all is still an error.
fn number(s: &str) -> Result<i64, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("'{s}' is not an integer\n{}", usage()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn t(list: &[&str]) -> Result<u8, String> {
        eval(list)
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("td-util-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 0 is TRUE and 1 is FALSE. Inverting this inverts every boot branch.
    #[test]
    fn zero_is_true_and_one_is_false() {
        assert_eq!(t(&["-n", "x"]), Ok(0), "a true expression must exit 0");
        assert_eq!(t(&["-z", "x"]), Ok(1), "a false expression must exit 1");
    }

    /// The string and integer forms the scripts use.
    #[test]
    fn the_scripted_comparisons_answer_correctly() {
        assert_eq!(t(&["1", "=", "1"]), Ok(0));
        assert_eq!(t(&["1", "=", "0"]), Ok(1));
        assert_eq!(t(&["0", "-lt", "5"]), Ok(0));
        assert_eq!(t(&["5", "-lt", "5"]), Ok(1));
        assert_eq!(t(&["-n", ""]), Ok(1));
        assert_eq!(t(&["-z", ""]), Ok(0));
        assert_eq!(t(&[]), Ok(1), "an empty expression is false, not an error");
        assert_eq!(t(&["x"]), Ok(0), "a lone non-empty operand is true");
        assert_eq!(t(&[""]), Ok(1));
    }

    /// `!` negates, in each arity the scripts use.
    #[test]
    fn negation_applies_to_both_arities() {
        assert_eq!(t(&["!", "-z", "x"]), Ok(0));
        assert_eq!(t(&["!", "-n", "x"]), Ok(1));
        assert_eq!(t(&["!", "1", "=", "1"]), Ok(1));
        assert_eq!(t(&["!", "1", "=", "0"]), Ok(0));
        assert_eq!(t(&["!", ""]), Ok(0), "! of an empty operand is true");
    }

    /// A non-numeric operand is an ERROR, not false.
    ///
    /// `while test "$n" -lt 5` with an unset `n` reading as `0 -lt 5` is a loop
    /// that never ends — the boot hangs instead of failing.
    #[test]
    fn a_non_numeric_integer_operand_is_an_error() {
        assert!(t(&["", "-lt", "5"]).is_err());
        assert!(t(&["x", "-lt", "5"]).is_err());
        assert!(t(&["1", "-lt", "y"]).is_err());
        assert!(t(&["1.5", "-lt", "5"]).is_err());
    }

    /// The file tests, against real files.
    #[test]
    fn the_file_operators_answer_from_the_filesystem() {
        let d = scratch("files");
        let f = d.join("f");
        std::fs::write(&f, b"x").unwrap();
        let fs = f.to_string_lossy().into_owned();
        let ds = d.to_string_lossy().into_owned();
        assert_eq!(t(&["-e", &fs]), Ok(0));
        assert_eq!(t(&["-f", &fs]), Ok(0));
        assert_eq!(t(&["-d", &fs]), Ok(1));
        assert_eq!(t(&["-d", &ds]), Ok(0));
        assert_eq!(t(&["-f", &ds]), Ok(1));
        assert_eq!(t(&["-s", &fs]), Ok(0));
        assert_eq!(t(&["-e", "/nonexistent/td-util-test"]), Ok(1));
        assert_eq!(t(&["-b", &fs]), Ok(1), "a regular file is not a block device");
        // A DANGLING symlink is `! -e`: these follow links, as POSIX says.
        let dangle = d.join("dangle");
        std::os::unix::fs::symlink(d.join("gone"), &dangle).unwrap();
        assert_eq!(t(&["-e", &dangle.to_string_lossy()]), Ok(1));
        let empty = d.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(t(&["-s", &empty.to_string_lossy()]), Ok(1));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `-b` really distinguishes a block device.
    #[test]
    fn a_block_device_is_recognised() {
        // /dev/vda is what the boot scripts wait for; any block device proves the
        // predicate, so use whichever this host has rather than requiring one.
        let mut seen = false;
        for cand in ["/dev/loop0", "/dev/sda", "/dev/vda", "/dev/ram0"] {
            if std::fs::metadata(cand).is_ok_and(|m| is_block_device(&m)) {
                assert_eq!(t(&["-b", cand]), Ok(0), "{cand} is a block device");
                seen = true;
            }
        }
        if !seen {
            eprintln!("note: no block device available here; -b positive case skipped");
        }
        assert_eq!(t(&["-b", "/dev/null"]), Ok(1), "a char device is not -b");
    }

    /// `-r`, `-w` and `-x` are refused LOUDLY, not answered from mode bits.
    ///
    /// This is the whole reason the applet could be written without a syscall
    /// amendment: answering any of them here would put back the prediction that
    /// `/etc/rootcheck` was silently getting wrong. `-w` is the one that decided a
    /// boot, but `-r`/`-x` fail identically, so all three go together — a served
    /// `-x` is just a wrong answer waiting for its first caller.
    #[test]
    fn the_credential_operators_are_refused_rather_than_approximated() {
        for op in ["-r", "-w", "-x"] {
            let err = t(&[op, "/tmp"]).unwrap_err();
            assert!(err.contains("access(2)"), "{op}: the refusal must say why: {err}");
            assert!(err.contains(op), "{op}: the refusal must name the operator: {err}");
            assert!(t(&["!", op, "/tmp"]).is_err(), "negated {op} is refused too");
        }
        // ...and the predicates that are NOT about the caller still answer.
        assert_eq!(t(&["-d", "/tmp"]), Ok(0), "-d is not a credential question");
    }

    /// Surrounding blanks on an integer operand parse, as busybox and dash do.
    ///
    /// This landing swaps the implementation under live boot conditionals, so a
    /// comparison that was true under busybox becoming exit 2 is a boot regression.
    /// A non-integer is still an error — this widens whitespace, not the grammar.
    #[test]
    fn integer_operands_tolerate_surrounding_blanks() {
        assert_eq!(t(&[" 1 ", "-eq", "1"]), Ok(0));
        assert_eq!(t(&["1", "-lt", " 5"]), Ok(0));
        assert_eq!(t(&["\t2\n", "-gt", "1"]), Ok(0));
        assert!(t(&["  ", "-lt", "5"]).is_err(), "blanks alone are not an integer");
        assert!(t(&[" x ", "-lt", "5"]).is_err(), "trimming must not admit non-integers");
    }

    /// An error EXITS 2, not 1.
    ///
    /// 1 is FALSE. `test -w /var` exiting 1 would read as "not writable" and pass
    /// the check the refusal exists to stop; `while test "$n" -lt 5` with a
    /// non-numeric `n` exiting 1 would read as "not less than" and end a retry
    /// loop that had never run. This is the one applet where the dispatcher's
    /// `Err -> 1` is the wrong answer, so `run` converts rather than propagating.
    #[test]
    fn an_error_exits_two_and_never_one() {
        let call = |a: &[&str]| run(&a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        assert_eq!(call(&["-w", "/tmp"]), Ok(2), "-w is refused, not answered false");
        assert_eq!(call(&["-r", "/tmp"]), Ok(2));
        assert_eq!(call(&["-x", "/tmp"]), Ok(2));
        assert_eq!(call(&["", "-lt", "5"]), Ok(2), "a bad integer operand is an error");
        assert_eq!(call(&["-q", "/tmp"]), Ok(2), "an unknown operator is an error");
        assert_eq!(call(&["a", "b", "c", "d", "e"]), Ok(2));
        // ...and the ordinary answers still come through unchanged.
        assert_eq!(call(&["-n", "x"]), Ok(0));
        assert_eq!(call(&["-z", "x"]), Ok(1));
    }

    /// An unknown operator is an ERROR (status 2 territory), never a quiet false.
    #[test]
    fn an_unknown_operator_is_not_silently_false() {
        assert!(t(&["-q", "/tmp"]).is_err());
        assert!(t(&["1", "-foo", "2"]).is_err());
        assert!(t(&["a", "b", "c", "d", "e"]).is_err());
        // ...but an operand that LOOKS like an operator still compares.
        assert_eq!(t(&["=", "=", "="]), Ok(0), "operands may look like operators");
    }
}
