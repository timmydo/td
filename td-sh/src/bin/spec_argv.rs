//! `argv.py`'s answer, without Python: print the arguments as a Python list.
//!
//! The Oils spec corpus asks 333 of its cases what a word EXPANDED to, and the
//! way it asks is `argv.py a b c`, whose goldens are a `repr` of the argument
//! list — `['a', 'b', 'c']`. 294 of those were `skip`ped by td-sh's overlay for
//! want of that helper, which was 77% of every skip: cases about word
//! splitting, quoting and expansion, the parts of a shell this corpus is most
//! worth running against. The helper is a few lines of Python upstream and is
//! not vendored here, so this is it, in the language the rest of td is.
//!
//! It is NOT part of the shell. `recipes/src/recipes/td-sh.rs` compiles
//! `src/main.rs` plus a named list of modules with a direct `rustc`, so a
//! second `[[bin]]` cannot reach the image even by accident; this exists for
//! `run_case`'s staged PATH and nothing else.
//!
//! What has to be right is `repr`, because the goldens ARE its output and a
//! quoting rule off by one case fails cases that have nothing to do with it.
//! The rule that matters most is that this is a BYTE repr, not a text one: an
//! argument is a sequence of bytes and every byte outside printable ASCII is
//! `\xNN`, so `μ` is `'\xce\xbc'` and not `'μ'`. That is not a guess about
//! Python versions — it is what the pinned corpus's own goldens contain, in
//! all nine that carry a high byte, and not one golden anywhere carries a
//! literal non-ASCII character. Reading it the other way round is a way to
//! fail cases the shell got RIGHT. The equivalent modern spelling, used by the
//! differential test, is `repr(os.fsencode(arg))` with the `b` prefix dropped.
//!
//! The rest is ordinary `repr`, in the order the rules apply:
//!   * the list is `[` + `, `-joined element reprs + `]`, and `[]` when empty;
//!   * an element is quoted with `'` unless it CONTAINS one and no `"`, in
//!     which case `"` is used and the `'` inside needs no escape;
//!   * `\` is doubled, and the quote actually used is escaped;
//!   * `\n`, `\r`, `\t` are those names, and every other byte outside
//!     printable ASCII — C0, DEL and everything from `\x80` up — is `\xNN`
//!     with LOWERCASE hex.
//!
//! Because the unit of work is one byte, there is no decoding, no lookahead
//! and no cursor: the loop below advances by exactly one byte per iteration
//! and terminates structurally. An earlier text-oriented draft did not, and
//! spun forever on a UTF-8 continuation byte.

// A bin is its own CRATE ROOT, so `main.rs`'s attribute does not reach here and
// nothing but this line keeps td-sh's "one scoped allow, in sys.rs" true of the
// whole crate. `forbid` rather than `deny` because no scoped allow belongs in
// this one, and `forbid` is the spelling a later `#[allow]` cannot override;
// `lib.rs`'s `every_crate_root_refuses_unsafe` pins it over the roots it
// DISCOVERS, since one added later would be as quiet as this was.
#![forbid(unsafe_code)]

use std::io::Write;
use std::os::unix::ffi::OsStrExt;

/// `repr` of one argument, appended to `out`.
fn repr(arg: &[u8], out: &mut String) {
    // `'` unless that would need escaping and `"` would not.
    let quote = if arg.contains(&b'\'') && !arg.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for &byte in arg {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b if b == quote as u8 => {
                out.push('\\');
                out.push(quote);
            }
            0x20..=0x7e => out.push(char::from(byte)),
            _ => push_hex(out, byte),
        }
    }
    out.push(quote);
}

/// `\xNN`, lowercase. Written nibble by nibble rather than through `format!`,
/// which would heap-allocate once per escaped byte.
fn push_hex(out: &mut String, byte: u8) {
    out.push_str("\\x");
    for nibble in [byte >> 4, byte & 0x0f] {
        // `from_digit` is Some for every value below the radix, and a nibble is.
        if let Some(digit) = char::from_digit(u32::from(nibble), 16) {
            out.push(digit);
        }
    }
}

fn main() {
    let mut out = String::from("[");
    for (i, arg) in std::env::args_os().skip(1).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        repr(arg.as_bytes(), &mut out);
    }
    out.push_str("]\n");
    // Ignored deliberately: this is a test helper, and a closed stdout is the
    // harness having stopped reading, which the case's own status reports.
    let _ = std::io::stdout().write_all(out.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(arg: &[u8]) -> String {
        let mut out = String::new();
        repr(arg, &mut out);
        out
    }

    /// Each expectation is a string the PINNED CORPUS contains as a golden, or
    /// `repr(os.fsencode(...))` from python3 with its `b` prefix dropped —
    /// never the rules as read.
    #[test]
    fn repr_matches_the_corpus_goldens() {
        assert_eq!(r(b"a"), "'a'");
        assert_eq!(r(b""), "''");
        // A `'` inside switches the quoting, and then needs no escape.
        assert_eq!(r(b"it's"), "\"it's\"");
        // ...unless a `"` is there too, when `'` comes back and IS escaped.
        assert_eq!(r(b"it's \"x\""), "'it\\'s \"x\"'");
        assert_eq!(r(b"say \"x\""), "'say \"x\"'");
        assert_eq!(r(b"back\\slash"), "'back\\\\slash'");
        assert_eq!(r(b"a\nb\tc\rd"), "'a\\nb\\tc\\rd'");
        assert_eq!(r(b"\x00\x1b\x7f"), "'\\x00\\x1b\\x7f'");
        // Non-ASCII is BYTES. These four are goldens in the corpus verbatim:
        // var-op-strip.test.sh's unicode strip cases and builtin-printf's
        // `☠` / `\U0000065f` / `\377`.
        assert_eq!(r("μabcμ".as_bytes()), "'\\xce\\xbcabc\\xce\\xbc'");
        assert_eq!(r("☠".as_bytes()), "'\\xe2\\x98\\xa0'");
        assert_eq!(r("ٟ".as_bytes()), "'\\xd9\\x9f'");
        assert_eq!(r(b"\xff"), "'\\xff'");
    }

    /// Every byte is accounted for exactly once, whatever it is. This is the
    /// test that does not depend on the corpus's contents: a text-oriented
    /// draft of `repr` decoded UTF-8 with a cursor and hung forever on a
    /// continuation byte, which no corpus case carries.
    #[test]
    fn every_byte_is_consumed_exactly_once() {
        for byte in 0..=u8::MAX {
            let out = r(&[byte]);
            assert!(out.len() >= 3, "byte {byte:#04x} produced {out:?}");
            // Everything outside printable ASCII is `\xNN`, bar the three
            // control bytes spelled by name.
            if !(0x20..=0x7e).contains(&byte) && !matches!(byte, b'\t' | b'\n' | b'\r') {
                assert_eq!(out, format!("'\\x{byte:02x}'"), "byte {byte:#04x}");
            }
        }
        // A high byte is escaped whether or not it is part of a valid
        // sequence, so neighbours are never swallowed.
        assert_eq!(r(b"a\xc3"), "'a\\xc3'");
        assert_eq!(r(b"\xc3a"), "'\\xc3a'");
        assert_eq!(r(b"\xe2\x82\xac"), "'\\xe2\\x82\\xac'");
    }
}
