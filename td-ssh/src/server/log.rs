//! The server's only way to write its log, which is stderr, for td-svc's capture.
//! `eprintln!` panics when the write fails, which under `panic = "abort"` would
//! let a closed log end a stage. Every line also passes the control filter here,
//! so nothing a peer or an account supplied can start a line of its own, and is
//! bounded, so nothing they supplied can fill the capture with one line.

use crate::error::peer_text;
use std::fmt;
use std::io::Write;

/// The most a line carries after filtering, its marker included.
const MAX_LINE: usize = 1024;
const CUT_MARKER: &str = "...[cut]";

fn render(text: fmt::Arguments<'_>) -> String {
    let mut line = peer_text(text.to_string().as_bytes());
    if line.len() > MAX_LINE {
        line.truncate(line.floor_char_boundary(MAX_LINE - CUT_MARKER.len()));
        line.push_str(CUT_MARKER);
    }
    line.push('\n');
    line
}

/// One log line. A failing log has nowhere to be reported, so it is ignored.
pub fn line(text: fmt::Arguments<'_>) {
    let _ = std::io::stderr().write_all(render(text).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hostile_string_cannot_start_a_line_of_its_own() {
        let name = "x\nAccepted publickey for root\r\x1b[2J";
        let rendered = render(format_args!("td-sshd: {name}"));
        assert_eq!(rendered.lines().count(), 1);
        assert_eq!(rendered, "td-sshd: x?Accepted publickey for root??[2J\n");
    }

    #[test]
    fn a_line_at_the_bound_is_whole_and_past_it_is_cut_with_a_marker() {
        let whole = "x".repeat(MAX_LINE);
        assert_eq!(render(format_args!("{whole}")), format!("{whole}\n"));
        let long = format!("{whole}x");
        let rendered = render(format_args!("{long}"));
        assert_eq!(rendered.len(), MAX_LINE + 1);
        assert!(rendered.ends_with(&format!("{CUT_MARKER}\n")), "{rendered}");
    }

    #[test]
    fn the_cut_lands_on_a_character_and_the_bound_is_after_filtering() {
        // Each of these three-byte characters is split by no cut.
        for pad in 0..3 {
            let long = format!("{}{}", "x".repeat(pad), "\u{20ac}".repeat(MAX_LINE));
            let rendered = render(format_args!("{long}"));
            assert!(rendered.len() <= MAX_LINE + 1, "{}", rendered.len());
            assert!(rendered.ends_with(&format!("\u{20ac}{CUT_MARKER}\n")));
        }
        // A control character is one byte before and after, and the marker still
        // follows filtered text.
        let controls = "\x1b".repeat(MAX_LINE * 2);
        let rendered = render(format_args!("{controls}"));
        assert_eq!(rendered.len(), MAX_LINE + 1);
        assert!(rendered.starts_with('?'));
        assert_eq!(rendered.lines().count(), 1);
    }
}
