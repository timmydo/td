//! gate_lint — a static scan for the comment-SPLICE trap in backslash-continued
//! shell/make lines (issue #300). Runs as a `#[test]` in the `cargo-test` gate
//! (the check-engine smoke tier + the hosted cargo-test CI job on every PR) over
//! every REGISTERED gate script body and `mk/harness.mk`.
//!
//! ## The trap (stepped on live in PR #291)
//!
//! A full-line `#` comment is NOT neutral inside a `\`-continued line. Both bash
//! and make splice a `\`+newline BEFORE they parse comments, so:
//!
//!   (a) a comment line FOLLOWING a `\`-continued line is joined ONTO that logical
//!       line — the comment then eats whatever the chain meant to continue. In
//!       PR #291's make-recipe form this silently split the recipe into separate
//!       shells (every variable and `set -euo pipefail` died at the split, and
//!       later assertions ran against empty strings — a full gate-debug cycle
//!       lost, #300). In a bash script it truncates the logical line the same way.
//!
//!   (b) a comment line ENDING in `\` continues nothing in bash — the backslash is
//!       comment TEXT, so the chain the author believed was one logical line
//!       silently breaks after the comment. In a makefile's non-recipe context a
//!       trailing-`\` comment is WORSE: it swallows the next line into the comment.
//!
//! Neither shape is reliably caught by `bash -n`: the spliced text is usually
//! still valid shell — which is exactly what made #291 so expensive to debug. This
//! scan is the cheap static guard. The gate bodies (`builder/src/gate_defs/*.rs`)
//! are written in the pervasive `; \` continuation idiom, so the hazard is one
//! stray comment line away at all times; scanning the compiled registry means new
//! gates are covered automatically (no enrollment).
//!
//! A trailing run of backslashes only continues a line when the run length is ODD
//! (an even run is escaped backslashes, e.g. `printf 'a\\'`).

/// Does `s` end in an ODD number of backslashes (i.e. a real line continuation)?
fn ends_in_odd_backslash_run(s: &str) -> bool {
    s.bytes().rev().take_while(|&b| b == b'\\').count() % 2 == 1
}

/// Scan one script/recipe body for the comment-splice hazard, returning a
/// human-readable finding per offending line (empty when clean). `origin` names
/// the source (a gate name or a file path) for the message.
pub fn comment_splice_hazards(origin: &str, script: &str) -> Vec<String> {
    let mut findings = Vec::new();
    let mut prev_continues = false;
    for (i, line) in script.lines().enumerate() {
        let lineno = i + 1;
        // Continuation is decided on the RAW line end: a `\` is a line continuation
        // ONLY when it sits immediately before the newline. `str::lines()` already
        // stripped the `\n` (and a trailing `\r`), so `line` ending in an odd
        // backslash run == the source had `\`+newline. Do NOT trim the end first —
        // `\` followed by whitespace escapes that space, it does NOT continue the
        // line, so trimming would falsely see a continuation.
        let raw_continues = ends_in_odd_backslash_run(line);
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            if prev_continues {
                findings.push(format!(
                    "{origin}:{lineno}: full-line comment inside a backslash-continued chain — \
                     bash/make splice it onto the previous logical line and it eats the \
                     continuation: `{trimmed}`"
                ));
            }
            if raw_continues {
                findings.push(format!(
                    "{origin}:{lineno}: comment line ends in `\\` — a backslash inside a comment \
                     continues nothing (bash) / swallows the next line (make): `{trimmed}`"
                ));
            }
            // A comment's own trailing `\` is text; it never continues the NEXT line.
            prev_continues = false;
        } else {
            prev_continues = raw_continues;
        }
    }
    findings
}

/// Every registered gate body + `mk/harness.mk`, scanned. Returns all findings
/// (empty when the tree is clean). The single entry point the gate/test drives.
pub fn scan_all() -> Vec<String> {
    let mut findings = Vec::new();
    for (_stem, def) in crate::gates::defs() {
        findings.extend(comment_splice_hazards(def.name, def.script));
    }
    // mk/harness.mk is the one surviving make file (the loop's /td/store harness
    // recipe). It is NOT a compiled gate, so scan it from disk relative to the
    // crate manifest (builder/), the same anchor the affected-checks tests use.
    findings.extend(scan_harness_mk(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))));
    findings
}

/// The mk/harness.mk leg of the scan, anchored at the crate manifest dir.
///
/// The repo/packaged-crate discrimination: `%builder-source` (system/
/// td-builder.scm) stages ONLY the builder/ crate, so inside the guix package
/// build — whose `cargo test` runs this test — `../mk` does not exist AT ALL.
/// That context is a SKIP (there is legitimately nothing to scan; the
/// registered gate bodies, the main surface, are compiled in and always
/// scanned). A repo where `mk/` exists but harness.mk is missing is still a
/// loud finding — the rename-detection the hard error existed for. Without
/// this split, ANY builder-source change reds the drv fixtures' td-builder
/// package rebuild (td-realize/eval/build-hermetic/…) inside `cargo test`.
fn scan_harness_mk(manifest_dir: &std::path::Path) -> Vec<String> {
    let mk_dir = manifest_dir.join("../mk");
    if !mk_dir.is_dir() {
        // Packaged-crate build (or a post-harness.mk-retirement tree): no mk/
        // to scan. The compiled gate bodies were already scanned above.
        return Vec::new();
    }
    let harness = mk_dir.join("harness.mk");
    match std::fs::read_to_string(&harness) {
        Ok(text) => comment_splice_hazards("mk/harness.mk", &text),
        Err(e) => vec![format!(
            "mk/harness.mk: could not read {} for the comment-splice scan: {e} \
             (was it renamed? update gate_lint::scan_all)",
            harness.display()
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_backslash_run_detects_real_continuations() {
        assert!(ends_in_odd_backslash_run("echo hi \\"));
        assert!(!ends_in_odd_backslash_run("echo hi")); // no backslash
        assert!(!ends_in_odd_backslash_run("printf 'a\\\\'")); // even run: escaped backslash
        assert!(ends_in_odd_backslash_run("printf 'a\\\\\\")); // odd run
    }

    #[test]
    fn flags_comment_spliced_into_a_continuation() {
        // The exact #291 shape: a `#` line right after a `\`-continued line.
        let script = "set -euo pipefail; \\\n# this comment eats the next assignment\nx=$(compute); \\\ntest -n \"$x\"";
        let f = comment_splice_hazards("fixture", script);
        assert!(
            f.iter().any(|m| m.contains("fixture:2") && m.contains("eats the continuation")),
            "expected a splice finding on line 2, got: {f:?}"
        );
    }

    #[test]
    fn flags_comment_line_ending_in_backslash() {
        let script = "echo start\n# a trailing backslash here is comment text \\\necho end";
        let f = comment_splice_hazards("fixture", script);
        assert!(
            f.iter().any(|m| m.contains("fixture:2") && m.contains("ends in `\\`")),
            "expected a trailing-backslash finding on line 2, got: {f:?}"
        );
    }

    #[test]
    fn clean_continuation_chain_has_no_findings() {
        // A normal `; \` chain with NO comment lines — the shape every gate uses.
        let script = "set -euo pipefail; \\\nx=$(compute); \\\ntest -n \"$x\"; \\\necho ok";
        assert!(comment_splice_hazards("fixture", script).is_empty());
    }

    #[test]
    fn a_comment_not_in_a_continuation_is_fine() {
        // A standalone comment (previous line does NOT continue) is legitimate.
        let script = "echo one\n# an ordinary standalone comment\necho two";
        assert!(comment_splice_hazards("fixture", script).is_empty());
    }

    #[test]
    fn backslash_followed_by_whitespace_is_not_a_continuation() {
        // `\` + trailing whitespace escapes the space, it does NOT continue the
        // line — so the following comment is NOT spliced and must not be flagged
        // (a `line.trim_end()` continuation check would false-positive here).
        let script = "echo one \\  \n# a legitimate standalone comment\necho two";
        assert!(
            comment_splice_hazards("fixture", script).is_empty(),
            "trailing whitespace after `\\` is not a continuation — no splice"
        );
    }

    /// The packaged-crate discrimination (issue: any builder-source change
    /// red the drv fixtures' td-builder rebuild): NO mk/ dir at all ⇒ skip
    /// (the guix package stages only builder/); mk/ present WITHOUT
    /// harness.mk ⇒ still a loud finding (rename detection).
    #[test]
    fn harness_scan_skips_outside_the_repo_but_detects_a_rename() {
        let base = std::env::temp_dir().join(format!("td-gatelint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // 1) packaged-crate shape: <base>/builder with NO sibling mk/ — skip.
        let manifest = base.join("builder");
        std::fs::create_dir_all(&manifest).unwrap();
        assert!(scan_harness_mk(&manifest).is_empty(), "no mk/ at all must be a skip");
        // 2) repo shape with mk/ but harness.mk missing — loud finding.
        std::fs::create_dir_all(base.join("mk")).unwrap();
        let findings = scan_harness_mk(&manifest);
        assert_eq!(findings.len(), 1, "mk/ without harness.mk must stay a finding");
        assert!(findings.iter().any(|f| f.contains("was it renamed")));
        // 3) repo shape with a clean harness.mk — no findings.
        std::fs::write(base.join("mk/harness.mk"), "all:\n\techo ok\n").unwrap();
        assert!(scan_harness_mk(&manifest).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The live guard: every registered gate body + mk/harness.mk is clean. This
    /// is the assertion the issue's "passes on the current tree" clause names; a
    /// future stray comment inside a `; \` chain reds it here (and in CI's
    /// cargo-test job) instead of costing a gate-debug cycle at runtime.
    #[test]
    fn gate_scripts_carry_no_comment_splice_hazard() {
        let findings = scan_all();
        assert!(
            findings.is_empty(),
            "comment-splice hazard(s) found (issue #300 — a `#` line inside a `; \\` chain \
             splices onto the previous logical line):\n{}",
            findings.join("\n")
        );
    }
}
