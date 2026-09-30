use crate::types::{Recipe, Step};

const TD_TERM: &str = "{in:td-term}/bin/td-term";
const ENTRY: &str = "{out}/share/terminfo/t/td-term";

/// td-term's compiled terminfo entry, as a data package of its own. The
/// Cargo build system installs binaries and nothing else, so the entry is
/// written here by the just-built td-term: one encoder, and the bytes the
/// image installs are the bytes its tests decode. `tic` and a host terminfo
/// database are not inputs. The image points `/etc/terminfo` at this
/// output's `share/terminfo`, which is where td-term tells its child to look.
pub fn recipe() -> Recipe {
    Recipe::mesboot("td-term-terminfo", "0.1.0")
        .native_inputs(&["td-term"])
        .steps(vec![
            // A binary that failed to build is reported as that rather than as
            // a mysteriously failing write.
            Step::Require {
                paths: vec![TD_TERM.into()],
                exec: true,
            },
            Step::MkDir {
                path: "{out}/share/terminfo/t".into(),
            },
            Step::run("{out}", &[TD_TERM, "terminfo", ENTRY]),
            Step::Require {
                paths: vec![ENTRY.into()],
                exec: false,
            },
        ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recipe writes the entry at a path td-term's encoder also spells,
    /// and the two never compile against each other, so a divergence would be
    /// caught by nothing without this. The binary refuses a path not ending
    /// in its constant, which makes a wrong STORE path a build failure.
    ///
    /// Reaching it is the image's half: the child is given
    /// `TERMINFO=/etc/terminfo`, and `IMMUTABLE_ETC` in the system recipe
    /// points that name at this output.
    #[test]
    fn the_entry_is_written_where_the_encoder_expects() {
        let terminfo = include_str!("../../../td-ui/src/vt_terminfo.rs");
        assert!(terminfo.contains(r#"pub const INSTALL_PATH: &str = "share/terminfo/t/td-term";"#));
        let main = include_str!("../../../td-term/src/main.rs");
        assert!(main.contains("\"terminfo\" =>"));
        let r = recipe();
        assert_eq!(r.native_inputs, Some(vec!["td-term".into()]));
        let steps = r.steps.expect("steps");
        let position = |found: Option<usize>, what: &str| -> usize {
            found.unwrap_or_else(|| panic!("nothing {what}"))
        };
        let required = position(
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: true }
                    if paths.iter().any(|path| path == TD_TERM))
            }),
            "requires td-term",
        );
        let written = position(
            steps.iter().position(|step| {
                matches!(step, Step::Run { argv, .. }
                    if *argv == [TD_TERM, "terminfo", ENTRY])
            }),
            "writes the entry with td-term",
        );
        let present = position(
            steps.iter().position(|step| {
                matches!(step, Step::Require { paths, exec: false }
                    if paths.iter().any(|path| path == ENTRY))
            }),
            "requires the written entry",
        );
        assert!(required < written && written < present);
        assert!(ENTRY.ends_with("share/terminfo/t/td-term"));
    }
}
