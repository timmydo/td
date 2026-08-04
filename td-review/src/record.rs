//! The per-commit review record (AGENTS.md, "Code review"), read back out of a
//! commit message. `td-builder ready` gates an agent's branch on this; here it
//! is what the integrator sees per branch before opening one, because a landing
//! replays commits individually and a commit nobody reviewed must not be one of
//! them.
//!
//! This is a SECOND implementation of that rule: td-review is outside the
//! builder workspace and cannot depend on it. `tests/review-record.cases` is
//! what keeps the two answering the same — both crates fail if a case there
//! disagrees with them.

/// The reviewers a record may name. Closed: a misspelt reviewer is a review
/// nobody ran, and an open roster cannot tell those two apart.
const SUBAGENT: &str = "subagent";
const CLI_REVIEWERS: [&str; 3] = ["agy", "claude", "codex"];
/// The cross-model reviewer both acting-agent rosters name; the other is the
/// model that is not acting, so "any two of three" would be laxer than AGENTS.md.
const SHARED_CLI: &str = "agy";
/// The waiver standing in for all three, on a commit whose every path is docs.
const DOCS_ONLY: &str = "docs-only";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Record {
    /// `(tool, model)`: the rule is two distinct MODELS, so `subagent/opus-4.8`
    /// beside `claude/opus-4.8` is one model behind two front ends.
    pub reviewers: Vec<(String, String)>,
    pub waivers: Vec<(String, String)>,
    pub checks: Option<String>,
    pub malformed: Vec<String>,
}

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

/// An unindented `Key: value` line. Indentation is what separates a record from
/// a message that merely SHOWS one, so a commit explaining the format does not
/// thereby claim it.
fn is_trailer_line(line: &str) -> bool {
    match line.split_once(':') {
        Some((key, _)) => {
            key.starts_with(|c: char| c.is_ascii_alphabetic())
                && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        }
        None => false,
    }
}

/// A line that looks like part of the record, wherever it sits — only used to
/// tell "no review" apart from "a review that does not close the message".
fn is_record_line(line: &str) -> bool {
    is_trailer_line(line)
        && line.split_once(':').is_some_and(|(key, _)| {
            ["reviewed-by", "review-waiver", "checks"]
                .iter()
                .any(|k| key.eq_ignore_ascii_case(k))
        })
}

/// The trailing run of trailer lines, the way git's own trailers work.
fn trailer_block(message: &str) -> Vec<&str> {
    let lines: Vec<&str> = message.lines().collect();
    let mut end = lines.len();
    while end > 0 && lines.get(end - 1).is_some_and(|l| l.trim().is_empty()) {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && lines.get(start - 1).is_some_and(|l| is_trailer_line(l)) {
        start -= 1;
    }
    lines
        .get(start..end)
        .map(<[&str]>::to_vec)
        .unwrap_or_default()
}

fn split_waiver(value: &str) -> (String, String) {
    let mut parts = value.splitn(2, char::is_whitespace);
    // The punctuation after the name is the separator, not part of the name.
    let subject = parts
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches([':', '-', ','])
        .to_string();
    let reason = parts
        .next()
        .unwrap_or_default()
        .trim_start_matches(|c: char| c == '—' || c == '-' || c == ':' || c.is_whitespace())
        .trim()
        .to_string();
    (subject, reason)
}

pub fn parse(message: &str) -> Record {
    let mut rec = Record::default();
    for line in trailer_block(message) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        // Case-insensitively, as git matches its own trailer tokens.
        let key = key.to_ascii_lowercase();
        match key.as_str() {
            "reviewed-by" => match value.split_once('/') {
                Some((tool, model)) if !tool.trim().is_empty() && !model.trim().is_empty() => {
                    let tool = tool.trim().to_string();
                    let model = model.trim().to_string();
                    if !rec.reviewers.iter().any(|(t, _)| *t == tool) {
                        rec.reviewers.push((tool, model));
                    }
                }
                _ => rec
                    .malformed
                    .push(format!("`Reviewed-by: {value}` is not <tool>/<model>")),
            },
            "review-waiver" => {
                let (subject, reason) = split_waiver(value);
                if subject.is_empty() {
                    rec.malformed
                        .push("`Review-waiver:` names no reviewer".to_string());
                } else {
                    rec.waivers.push((subject, reason));
                }
            }
            "checks" if !value.is_empty() => rec.checks = Some(value.to_string()),
            _ => {}
        }
    }
    rec
}

/// Who approved this waiver, out of its reason. A waiver is not something an
/// agent may issue: it records the answer a HUMAN gave when asked, so the
/// answer has to name them. The gate cannot see that the question was asked —
/// naming the person who answered is the closest checkable thing to it.
fn approver(reason: &str) -> Option<&str> {
    let lower = reason.to_ascii_lowercase();
    let at = lower.find("approved by")?;
    let who = reason.get(at.saturating_add("approved by".len())..)?.trim();
    (!who.is_empty()).then_some(who)
}

fn known_reviewer(name: &str) -> bool {
    name == SUBAGENT || CLI_REVIEWERS.contains(&name)
}

/// The model families an identity may name — closed, like the tool roster, and
/// for the same reason: this has to answer WHICH MODEL looked, and a spelling it
/// does not recognise is one it cannot compare. Splitting on the first `-` was
/// not enough: `claude-opus-4.8` and `opus-4.8` are the same model, and that is
/// the spelling an agent naturally writes for its own subagent.
const MODELS: [&str; 6] = ["opus", "sonnet", "haiku", "fable", "gpt", "gemini"];

fn family(model: &str) -> Option<&'static str> {
    let lower = model.to_ascii_lowercase();
    MODELS.into_iter().find(|m| {
        lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| token == *m)
    })
}

/// Everything wrong with one commit's record; empty means it may land.
pub fn problems(message: &str, paths: &[String]) -> Vec<String> {
    let rec = parse(message);
    let mut out = rec.malformed.clone();

    let docs_only = rec.waivers.iter().any(|(s, _)| s == DOCS_ONLY);
    for (subject, reason) in &rec.waivers {
        if subject == DOCS_ONLY {
            // No paths is not "no code": a merge shows none, and so does a
            // failed query. The one claim checked against reality may not be
            // satisfied by silence.
            if paths.is_empty() {
                out.push("waived as docs-only, but no paths could be read".to_string());
            } else if let Some(p) = paths
                .iter()
                .find(|p| !p.to_ascii_lowercase().ends_with(".md"))
            {
                out.push(format!("waived as docs-only, but it touches {p}"));
            }
        } else if subject == SUBAGENT {
            // The waiver exists for a reviewer CLI that is UNAVAILABLE. The
            // subagent is not a CLI and cannot be unavailable, so waiving it is
            // just declining the one review that always could have run.
            out.push("the subagent review cannot be waived".to_string());
        } else if !known_reviewer(subject) {
            out.push(format!("waives unknown reviewer '{subject}'"));
        } else if reason.is_empty() {
            out.push(format!("waiver for '{subject}' gives no reason"));
        } else if approver(reason).is_none() {
            out.push(format!(
                "waiver for '{subject}' names nobody who approved it — a waiver records \
                 a human's answer, so it ends `approved by <who>`"
            ));
        }
    }

    // One at most — a backstop, not an allowance. Nothing here can see whether
    // the human was actually asked, so what the gate CAN do is bound what a
    // waiver reaches: stacked ones get to "no reviews at all" in four extra
    // words, and a commit with two reviewers down is one to stop on anyway.
    let reviewer_waivers = rec
        .waivers
        .iter()
        .filter(|(s, _)| s != DOCS_ONLY)
        .count();
    if reviewer_waivers > 1 {
        out.push(format!(
            "{reviewer_waivers} reviewers waived at once — ask a human rather than record around it"
        ));
    }

    for (tool, model) in &rec.reviewers {
        if !known_reviewer(tool) {
            out.push(format!("unknown reviewer '{tool}'"));
        } else if family(model).is_none() {
            out.push(format!(
                "'{model}' is not a known model ({}) — the cross-model rule cannot compare it",
                MODELS.join("/")
            ));
        }
    }

    if !docs_only {
        // Only a waiver that names its approver stands in for a review.
        let waived =
            |who: &str| rec.waivers.iter().any(|(s, r)| s == who && approver(r).is_some());
        let model_of = |who: &str| {
            rec.reviewers
                .iter()
                .find(|(t, _)| t == who)
                .map(|(_, m)| m.as_str())
        };
        let have = |who: &str| model_of(who).is_some() || waived(who);
        if !have(SUBAGENT) {
            out.push("no subagent review".to_string());
        }
        if !have(SHARED_CLI) {
            out.push(format!(
                "no {SHARED_CLI} review — both acting-agent rosters name it"
            ));
        }
        // The remaining slot is the model that is NOT acting: comparing tools
        // alone would accept one model reviewing its own work through a second
        // front end, which is the blind spot the rule exists to cover.
        let acting = model_of(SUBAGENT).and_then(family);
        let others: Vec<&str> = CLI_REVIEWERS
            .iter()
            .copied()
            .filter(|c| *c != SHARED_CLI)
            .collect();
        // An unrecognised model counts as NOT cross-model: it was reported just
        // above, and a spelling nothing can compare must not settle the check.
        let cross = others.iter().any(|c| match model_of(c) {
            Some(m) => family(m).is_some_and(|f| acting.is_none_or(|a| f != a)),
            None => waived(c),
        });
        if !cross {
            let named = others.join(" or ");
            match others.iter().find_map(|c| model_of(c)) {
                Some(m) => out.push(format!(
                    "the {named} review is {m}, the model that reviewed it as subagent — \
                     the second opinion must be the model that is not acting"
                )),
                None => out.push(format!("no cross-model review from {named}")),
            }
        }
    }

    if rec.checks.is_none() {
        out.push("no `Checks:` trailer".to_string());
    }

    // Only when something is missing: a record that does not CLOSE the message
    // is invisible to the block scan, and reporting the reviews as absent would
    // point an agent at the wrong thing. A complete record beside a quoted
    // example is not an error, so this is not checked unconditionally.
    if !out.is_empty() {
        let block = trailer_block(message);
        let stray = message
            .lines()
            .filter(|l| is_record_line(l) && !block.contains(l))
            .count();
        if stray > 0 {
            out.push(format!(
                "{stray} record line(s) do not close the message — the record must be the \
                 last block, one trailer per line (do not wrap them)"
            ));
        }
    }
    out
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
        let out = format!("\x1e{A}\x1f{A}\x1fsubject\n\nbody with \x1e and \x1f in it\n\x1f\nsrc/a.rs\n");
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
