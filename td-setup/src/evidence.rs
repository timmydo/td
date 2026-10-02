//! The installer's boot evidence for the QEMU oracle that drives it. Only
//! when the boot's command line carries `td.setup-input=1` does the window
//! say, on standard error, one line per page state the compositor showed
//! while the window held the keyboard; the oracle sends its next key only
//! after the state it expects is shown. Nothing here changes what the
//! wizard does.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// The command-line word that asks for the evidence.
pub const PROOF_CMDLINE_TOKEN: &str = "td.setup-input=1";
/// Begins every evidence line, before `n=SEQUENCE` and the page state.
pub const SHOWN_PREFIX: &str = "TD-SETUP-SHOWN ";
/// The kernel's own command line, which the token is read from.
pub const CMDLINE: &str = "/proc/cmdline";
/// More than any x86 kernel command line.
const MAX_CMDLINE_BYTES: usize = 4096;

/// Whether the command line at `path` asks for the evidence. A host
/// without the file, or one that cannot open it, has not asked.
pub fn enabled(path: &Path) -> Result<bool, String> {
    let Ok(file) = File::open(path) else {
        return Ok(false);
    };
    let limit = u64::try_from(MAX_CMDLINE_BYTES.saturating_add(1))
        .map_err(|_| "installer command-line limit escaped u64")?;
    let mut bytes = Vec::with_capacity(MAX_CMDLINE_BYTES.saturating_add(1));
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() > MAX_CMDLINE_BYTES {
        return Err(format!(
            "{} exceeded {MAX_CMDLINE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(bytes
        .split(u8::is_ascii_whitespace)
        .any(|word| word == PROOF_CMDLINE_TOKEN.as_bytes()))
}

/// Appends ` key=value` to a page state, with every byte of the value
/// outside a closed set written as `\xHH`, so typed text can neither end
/// the line nor forge another field.
pub fn field(state: &mut String, key: &str, value: &str) {
    state.push(' ');
    state.push_str(key);
    state.push('=');
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'+' | b':' | b'-') {
            state.push(char::from(byte));
        } else {
            state.push_str(&format!("\\x{byte:02x}"));
        }
    }
}

/// The page states drawn, shown and said. A state is said once the
/// compositor has shown the frame it was drawn in and the window holds
/// the keyboard, and only when it differs from the one said last.
#[derive(Debug, Default)]
pub struct Proof {
    drawn: Option<String>,
    shown: Option<String>,
    said: Option<String>,
    sequence: u32,
}

impl Proof {
    /// A frame showing `state` was presented. It replaces whatever was on
    /// screen, so a state shown before it and not yet said never is.
    pub fn drawn(&mut self, state: String) {
        self.drawn = Some(state);
        self.shown = None;
    }

    /// A frame showing no page, the too-small ground, was presented: no
    /// state shown so far is on screen any longer.
    pub fn blank(&mut self) {
        self.drawn = None;
        self.shown = None;
    }

    /// The compositor is done with the frame last presented.
    pub fn frame_done(&mut self) {
        if let Some(state) = self.drawn.take() {
            self.shown = Some(state);
        }
    }

    /// The line to say now, if any: `focused` is whether the window holds
    /// the keyboard, its key state current.
    pub fn due(&mut self, focused: bool) -> Option<String> {
        if !focused || self.shown.is_none() || self.shown == self.said {
            return None;
        }
        let state = self.shown.take()?;
        self.sequence = self.sequence.checked_add(1)?;
        let line = format!("{SHOWN_PREFIX}n={} {state}\n", self.sequence);
        self.said = Some(state);
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn only_the_exact_word_asks() {
        let dir = std::env::temp_dir().join(format!("td-setup-evidence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cmdline = dir.join("cmdline");
        for (text, asked) in [
            ("console=ttyS0 td.setup-input=1 td.live=1\n", true),
            ("td.setup-input=1", true),
            ("td.setup-input=10 td.setup-input", false),
            ("xtd.setup-input=1", false),
            ("", false),
        ] {
            std::fs::write(&cmdline, text).unwrap();
            assert_eq!(enabled(&cmdline), Ok(asked), "{text}");
        }
        std::fs::write(&cmdline, vec![b'a'; MAX_CMDLINE_BYTES + 1]).unwrap();
        assert!(enabled(&cmdline).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(enabled(&dir.join("absent")), Ok(false));
    }

    #[test]
    fn values_cannot_forge_a_field_or_a_line() {
        let mut state = String::from("page=settings");
        field(&mut state, "username", "dana");
        field(&mut state, "hostname", "a b=c\nTD\u{e9}\\");
        field(&mut state, "zone", "");
        assert_eq!(
            state,
            "page=settings username=dana hostname=a\\x20b\\x3dc\\x0aTD\\xc3\\xa9\\x5c zone="
        );
    }

    #[test]
    fn a_state_is_said_once_shown_while_focused() {
        let mut proof = Proof::default();
        proof.drawn("page=welcome".into());
        // Presented but not yet shown.
        assert_eq!(proof.due(true), None);
        proof.frame_done();
        // Shown, but the keyboard is elsewhere: kept until it arrives.
        assert_eq!(proof.due(false), None);
        assert_eq!(
            proof.due(true).as_deref(),
            Some("TD-SETUP-SHOWN n=1 page=welcome\n")
        );
        assert_eq!(proof.due(true), None);
        // A redraw of the same state is not said again.
        proof.drawn("page=welcome".into());
        proof.frame_done();
        assert_eq!(proof.due(true), None);
        // A frame done with nothing presented since shows nothing new.
        proof.frame_done();
        assert_eq!(proof.due(true), None);
        proof.drawn("page=waiting".into());
        proof.frame_done();
        assert_eq!(
            proof.due(true).as_deref(),
            Some("TD-SETUP-SHOWN n=2 page=waiting\n")
        );
        // A shown state superseded before it was said is never said: by a
        // later frame, which is said once it is shown,
        proof.drawn("page=a".into());
        proof.frame_done();
        proof.drawn("page=b".into());
        assert_eq!(proof.due(true), None);
        proof.frame_done();
        assert_eq!(
            proof.due(true).as_deref(),
            Some("TD-SETUP-SHOWN n=3 page=b\n")
        );
        // or by the blank ground.
        proof.drawn("page=welcome".into());
        proof.frame_done();
        proof.blank();
        proof.frame_done();
        assert_eq!(proof.due(true), None);
    }
}
