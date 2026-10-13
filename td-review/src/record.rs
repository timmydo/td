//! The per-commit review record (AGENTS.md, "Code review"), read back out of a
//! commit message. `td-builder ready` gates an agent's branch on this; here it
//! is what the integrator sees per branch before opening one, because a landing
//! replays commits individually and a commit nobody reviewed must not be one of
//! them.
//!
//! The rule has two implementations: td-review is outside the builder
//! workspace and cannot depend on it. They share transport normalization;
//! `tests/review-record.cases` pins their remaining rules together. Both
//! crates fail if a case disagrees with them.

#[path = "reviewer.rs"]
mod reviewer;

pub use reviewer::problems;
#[cfg(test)]
use reviewer::{parse, Record};

/// One commit's identity and the message the record is read from.
pub struct Commit {
    pub oid: String,
    pub message: String,
    pub paths: Vec<String>,
    /// Parent count. A landing REPLAYS commits, and `land.rs` refuses a merge —
    /// so a merge is not landable however good its record is, and `--name-only`
    /// shows it no paths, which a `docs-only` waiver would otherwise ride.
    pub parents: usize,
}

/// A branch's record status, as the list column shows it.
#[derive(Debug, PartialEq, Eq)]
pub enum Readiness {
    /// Every commit carries its record.
    Ready,
    /// This many of `total` do not.
    Missing { missing: usize, total: usize },
    /// Nothing to land.
    Empty,
    /// The commits could not be read (git failed) — never rendered as ready.
    Unknown,
}

impl Readiness {
    pub fn label(&self) -> String {
        match self {
            Readiness::Ready => "ok".to_string(),
            Readiness::Missing { missing, total } => format!("{missing}/{total}!"),
            Readiness::Empty => "-".to_string(),
            Readiness::Unknown => "?".to_string(),
        }
    }
}

/// Everything wrong with one commit, record and shape both.
pub fn commit_problems(c: &Commit) -> Vec<String> {
    let mut out = problems(&c.message, &c.paths);
    if c.parents > 1 {
        out.push("merge commit — a replay landing cannot take it".to_string());
    }
    out
}

/// Roll a branch's commits up into the one word the list column shows.
pub fn readiness(commits: &[Commit]) -> Readiness {
    if commits.is_empty() {
        return Readiness::Empty;
    }
    let missing = commits
        .iter()
        .filter(|c| !commit_problems(c).is_empty())
        .count();
    if missing == 0 {
        Readiness::Ready
    } else {
        Readiness::Missing {
            missing,
            total: commits.len(),
        }
    }
}

/// The same, for records read on behalf of `expected` commits counted another
/// way. A disagreement is UNKNOWN and never ready: the batch can only be
/// trusted when it accounts for every commit a landing would replay.
pub fn readiness_for(expected: usize, records: &[Commit]) -> Readiness {
    if records.len() != expected {
        return Readiness::Unknown;
    }
    readiness(records)
}

/// Parse `git log --format=%x1e%H%x1f%P%x1f%B%x1f --name-only` output into commits.
/// Record-separated rather than line-parsed, because a message body is free text
/// and nothing line-based can delimit it.
///
/// `None` when any chunk does not parse, and the whole batch fails with it: a
/// body may itself contain those separators, which splits the record it belongs
/// to and would otherwise DROP that commit — silently turning a commit with no
/// review record into a branch that reads as ready. Fail closed instead; the
/// caller shows "unknown". Every record must open with a full object id, so a
/// forged separator has to be followed by 40 hex digits to get that far.
pub fn parse_commits(out: &str) -> Option<Vec<Commit>> {
    let mut chunks = out.split('\x1e');
    // git's own output opens with the separator, so nothing precedes the first.
    if !chunks.next().unwrap_or_default().trim().is_empty() {
        return None;
    }
    let mut commits = Vec::new();
    for chunk in chunks {
        let (oid, rest) = chunk.split_once('\x1f')?;
        let oid = oid.trim();
        if oid.len() != 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        // Exactly two field separators may follow the oid. A body carrying one
        // would otherwise end the message early, and a record that looks
        // complete in the prefix would read as ready with the disqualifying
        // prose parsed as a path list.
        if rest.matches('\x1f').count() != 2 {
            return None;
        }
        let (parents, rest) = rest.split_once('\x1f')?;
        let (message, names) = rest.split_once('\x1f')?;
        commits.push(Commit {
            oid: oid.to_string(),
            parents: parents.split_whitespace().count(),
            message: message.to_string(),
            paths: names
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
        });
    }
    Some(commits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn code(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn complete_record_is_ready() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: green\n";
        assert!(problems(m, &code(&["a.rs"])).is_empty());
    }

    #[test]
    fn a_quoted_record_does_not_count() {
        let m = "docs: explain it\n\nlike so:\n\n    Reviewed-by: subagent/opus-4.8\n    \
                 Checks: green\n\nthat is all\n";
        assert_eq!(parse(m), Record::default());
    }

    #[test]
    fn readiness_counts_the_commits_missing_one() {
        let good = Commit {
            oid: "a".into(),
            message: "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\nReviewed-by: agy/gemini-3.1-pro\n\
                      Checks: green\n"
                .into(),
            paths: code(&["a.rs"]),
            parents: 1,
        };
        let bad = Commit {
            oid: "b".into(),
            message: "t\n\nChecks: green\n".into(),
            paths: code(&["b.rs"]),
            parents: 1,
        };
        assert_eq!(readiness(&[]), Readiness::Empty);
        assert_eq!(readiness(std::slice::from_ref(&good)), Readiness::Ready);
        assert_eq!(
            readiness(&[good, bad]),
            Readiness::Missing {
                missing: 1,
                total: 2
            }
        );
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn commits_parse_out_of_the_record_separated_log() {
        let out = format!("\x1e{A}\x1f{A}\x1fsubject one\n\nbody\n\x1f\nsrc/a.rs\nsrc/b.rs\n\x1e{B}\x1f{A} {B}\x1fsubject two\n\x1f\nAGENTS.md\n");
        let commits = parse_commits(&out).expect("parses");
        assert_eq!(commits.len(), 2);
        let first = commits.first().expect("first");
        assert_eq!(first.oid, A);
        assert_eq!(first.paths, code(&["src/a.rs", "src/b.rs"]));
        let second = commits.get(1).expect("second");
        assert_eq!(second.paths, code(&["AGENTS.md"]));
    }

    /// The acting model reviewing its own work through a second front end is
    /// not a second opinion — the mirror of builder's own assertion.
    #[test]
    fn the_acting_model_cannot_be_its_own_cross_model_review() {
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: claude/opus-4.8\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        assert_eq!(problems(m, &code(&["a.rs"])).len(), 1);
        let m = "s\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                 Reviewed-by: agy/gemini-3.1-pro\nChecks: g\n";
        assert!(problems(m, &code(&["a.rs"])).is_empty());
    }

    /// A body carrying the FIELD separator ends the message early: the record
    /// in the prefix could look complete while the prose that disqualifies it
    /// is parsed as a path list. Fail the batch closed instead.
    #[test]
    fn a_body_cannot_truncate_its_own_message() {
        let out = format!("\x1e{A}\x1f{A}\x1fsubject\n\nReviewed-by: subagent/opus-4.8\n\x1fand more prose\n\x1f\nsrc/a.rs\n");
        assert!(parse_commits(&out).is_none());
    }

    /// A merge cannot be replayed, and `--name-only` shows it no paths — so the
    /// column must not read `ok` for a branch carrying one, however complete
    /// its record is. `td-builder ready` refuses the same shape.
    #[test]
    fn a_merge_commit_is_not_ready_however_good_its_record() {
        let merge = Commit {
            oid: A.to_string(),
            message: "m\n\nReviewed-by: subagent/opus-4.8\nReviewed-by: codex/gpt-5.6-sol\n\
                      Reviewed-by: agy/gemini-3.1-pro\nChecks: green\n"
                .to_string(),
            paths: Vec::new(),
            parents: 2,
        };
        assert!(problems(&merge.message, &merge.paths).is_empty());
        assert!(commit_problems(&merge)
            .iter()
            .any(|p| p.contains("replay landing cannot take it")));
        assert_eq!(
            readiness(std::slice::from_ref(&merge)),
            Readiness::Missing {
                missing: 1,
                total: 1
            }
        );
    }

    #[test]
    fn a_malformed_batch_is_rejected_whole() {
        // No field separator after the oid.
        let out = format!("\x1e{A}\x1esubject\n\x1f\x1f\nsrc/a.rs\n");
        assert!(parse_commits(&out).is_none());
        // An id that is not an object id.
        assert!(parse_commits("\x1eaaa\x1faaa\x1fsubject\n\x1f\nsrc/a.rs\n").is_none());
        // Output that does not open with the record separator.
        assert!(parse_commits(&format!("noise\x1e{A}\x1f{A}\x1fs\n\x1f\n")).is_none());
        // No commits at all is not malformed.
        assert_eq!(parse_commits("").map(|c| c.len()), Some(0));
    }

    /// A body may contain the separators, which splits the record it belongs to.
    /// The commit must not vanish from the scan: a dropped commit is one whose
    /// missing review record nobody counts, so the batch fails closed instead.
    #[test]
    fn a_body_cannot_drop_or_forge_a_record() {
        let out =
            format!("\x1e{A}\x1f{A}\x1fsubject\n\nbody with \x1e and \x1f in it\n\x1f\nsrc/a.rs\n");
        assert!(parse_commits(&out).is_none());
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    /// The fixture `td-builder ready` also reads. Two implementations of one
    /// rule; nothing but this makes them agree.
    #[test]
    fn shared_fixture_cases() {
        let path = repo_root().join("tests/review-record.cases");
        let text = std::fs::read_to_string(&path).expect("tests/review-record.cases");
        let cases = parse_cases(&text);
        assert!(cases.len() >= 26, "fixture shrank: {} cases", cases.len());
        for (name, expect_ready, paths, message) in cases {
            let probs = problems(&message, &paths);
            assert_eq!(
                probs.is_empty(),
                expect_ready,
                "case {name}: problems {probs:?}"
            );
        }
    }

    /// `case <name> <ready|not-ready> [paths=a,b]`, then the message until the
    /// next case. Mirrored in builder's `ready.rs`.
    fn parse_cases(text: &str) -> Vec<(String, bool, Vec<String>, String)> {
        let mut out: Vec<(String, bool, Vec<String>, String)> = Vec::new();
        let mut cur: Option<(String, bool, Vec<String>, Vec<String>)> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("case ") {
                if let Some((n, r, p, body)) = cur.take() {
                    out.push((n, r, p, body.join("\n")));
                }
                let mut fields = rest.split_whitespace();
                let name = fields.next().expect("case needs a name").to_string();
                // A malformed case line panics rather than quietly meaning
                // not-ready: this fixture is the only thing keeping the two
                // implementations honest.
                let ready = match fields.next() {
                    Some("ready") => true,
                    Some("not-ready") => false,
                    other => panic!("case {name}: verdict must be ready|not-ready, got {other:?}"),
                };
                let mut paths = vec!["src/a.rs".to_string()];
                for f in fields {
                    let value = f
                        .strip_prefix("paths=")
                        .unwrap_or_else(|| panic!("case {name}: unknown field {f}"));
                    paths = if value.is_empty() {
                        Vec::new()
                    } else {
                        value.split(',').map(str::to_string).collect()
                    };
                }
                cur = Some((name, ready, paths, Vec::new()));
            } else if let Some((_, _, _, body)) = cur.as_mut() {
                body.push(line.to_string());
            }
        }
        if let Some((n, r, p, body)) = cur.take() {
            out.push((n, r, p, body.join("\n")));
        }
        out
    }
}
