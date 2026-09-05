//! The exit-status contract td-builder and td-recipe-eval speak across a
//! process boundary — the one copy, so the two cannot drift.
//!
//! Two failures look alike from the outside and must never be confused:
//!
//! - a HOST gap — nothing here can run the work (no toolchain reachable in the
//!   loop sandbox). Nothing is wrong with the tree; the run degrades to a
//!   tolerated skip.
//! - a PROVENANCE rejection — the bootstrap graph is unbuildable on EVERY host
//!   until a rejected input exists as a td recipe output. A real gap in the
//!   chain, and never a skip.
//!
//! Both used to leave as 69, so a caller reading the code alone could not tell
//! "cannot run here" from "cannot run anywhere". They carry distinct codes now,
//! and the host gap additionally proves itself with [`UNPROVISIONED_SENTINEL`]
//! on stderr — the code alone is not enough, because 69 is EX_UNAVAILABLE and
//! any tool in a pipeline may pick it for its own reasons.

/// EX_UNAVAILABLE. A host gap: this machine cannot run the work.
pub const EXIT_UNPROVISIONED: i32 = 69;

/// EX_CONFIG. The graph declares an input with no admissible provenance, so no
/// host can build it. Distinct from [`EXIT_UNPROVISIONED`] on purpose: a caller
/// that tolerates the host gap must NOT tolerate this.
///
/// Deliberately NOT held to the two-part test below. That test exists because
/// tolerating a code you did not earn hides regressions; a stray 78 has the
/// opposite failure mode — it REDS something that might have been fine, which
/// is the safe direction and self-correcting on the next run.
pub const EXIT_PROVENANCE_REJECTED: i32 = 78;

/// The stderr token every [`EXIT_UNPROVISIONED`] exit prints. Distinctive
/// enough that no unrelated tool emits it by chance, which is what lets a
/// caller separate td's own "nothing to run here" from a stray EX_UNAVAILABLE.
pub const UNPROVISIONED_SENTINEL: &str = "[td-unprovisioned-69:re#469]";

/// The stdout token a `check-run` prints when it answers from its verdict
/// memo instead of running: the check passed here before with every input
/// it reads unchanged since. The gate counts such a check as passed and says
/// how many were, so a run that re-ran nothing cannot read as one that
/// re-proved everything. Distinctive for the same reason as the sentinel
/// above.
pub const CHECK_MEMO_SENTINEL: &str = "[td-check-memo:pass]";

/// Did a child actually report a host gap? BOTH halves are required. The code
/// alone is not proof — a tolerating caller re-emits the sentinel for whatever
/// it believes, so accepting a bare 69 lets any other failure mint a skip and
/// hide behind it.
pub fn child_reported_host_gap(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> bool {
    host_gap_from_parts(
        code,
        contains_sentinel(stderr) || contains_sentinel(stdout),
    )
}

/// The same rule for callers that cannot hand over the bytes — a streamed tee
/// that scanned as it forwarded, or a log already on disk. Kept as the ONE
/// statement of the test so those shapes cannot quietly drift from this one.
pub fn host_gap_from_parts(code: Option<i32>, saw_sentinel: bool) -> bool {
    code == Some(EXIT_UNPROVISIONED) && saw_sentinel
}

/// Scanned as BYTES: the sentinel is ASCII, and the streams this runs over are
/// whole captured child outputs that may not decode at all.
fn contains_sentinel(bytes: &[u8]) -> bool {
    let needle = UNPROVISIONED_SENTINEL.as_bytes();
    bytes.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the module: a caller holding only the number can tell
    /// the two apart. If these ever collide, every "is this a skip?" decision in
    /// both bins silently answers wrong.
    #[test]
    fn the_host_gap_and_a_provenance_rejection_do_not_share_a_code() {
        assert_ne!(EXIT_UNPROVISIONED, EXIT_PROVENANCE_REJECTED);
        assert_eq!(EXIT_UNPROVISIONED, 69);
        assert_eq!(EXIT_PROVENANCE_REJECTED, 78);
    }

    #[test]
    fn a_host_gap_needs_the_code_and_the_sentinel() {
        let s = UNPROVISIONED_SENTINEL.as_bytes();
        assert!(child_reported_host_gap(Some(69), b"", s));
        assert!(child_reported_host_gap(Some(69), s, b""));
        assert!(
            !child_reported_host_gap(Some(69), b"", b"linker not found"),
            "a 69 with no sentinel is some other failure and must not read as a skip"
        );
        assert!(
            !child_reported_host_gap(Some(1), b"", s),
            "the sentinel alone is not a skip; the code must agree"
        );
        assert!(!child_reported_host_gap(None, b"", s), "killed by a signal");
        // Undecodable bytes around it must not hide it — the streams this reads
        // are raw child output, not text.
        let mut noisy = vec![0xff, 0xfe, b'\n'];
        noisy.extend_from_slice(s);
        assert!(child_reported_host_gap(Some(69), b"", &noisy));
        assert!(!child_reported_host_gap(Some(69), b"", &[0xff, 0xfe]));
        assert!(!child_reported_host_gap(
            Some(EXIT_PROVENANCE_REJECTED),
            b"",
            s
        ));
    }
}
