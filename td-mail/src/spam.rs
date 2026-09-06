//! Self-contained Bayesian spam classifier.
//!
//! This module is deliberately free of any JMAP / network / TUI dependencies so
//! it can be unit-tested in isolation. It tokenizes a raw RFC822 message
//! (headers + body), scores it with a Robinson-Fisher combiner over a trained
//! token store, and persists the store as a single JSON model file.
//!
//! The classifier never takes actions on its own. Callers obtain a numeric
//! score and a [`Verdict`]; in td-mail the verdict is surfaced as a synthetic
//! `X-Tmc-Spam-Verdict` header so that the existing rules engine (rules.toml)
//! decides what to do with the message.

use crate::json::{self, Json, ObjectBuilder, ToJson};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Maximum number of most-significant tokens fed into the combiner. Bounds work
/// and keeps a few extreme tokens from being diluted by a long body.
const MAX_TOKENS: usize = 150;

/// Hard cap on distinct tokens kept per message. A safety bound so a pathological
/// message (e.g. one that slips a lot of encoded data past the blob filter)
/// cannot bloat the trained model.
const MAX_MESSAGE_TOKENS: usize = 2000;

/// Robinson smoothing: strength `s` and assumed prior probability `x`. A token
/// seen few times is pulled toward `ASSUMED_PROB`, so rare/unknown tokens carry
/// little weight until there is real evidence.
const STRENGTH: f64 = 1.0;
const ASSUMED_PROB: f64 = 0.5;

/// Label used when training the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Label {
    Spam,
    Ham,
}

/// Outcome of classifying a message against a threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Spam,
    Ham,
    /// Either the model is not yet trained enough, or the score landed in the
    /// uncertain band between the ham and spam thresholds.
    Unsure,
}

impl Verdict {
    /// Lowercase label used for the synthetic `X-Tmc-Spam-Verdict` header.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Spam => "spam",
            Verdict::Ham => "ham",
            Verdict::Unsure => "unsure",
        }
    }
}

/// A raw message to be tokenized. Holds the decoded RFC822 source.
#[derive(Debug, Clone)]
pub struct RawMessage {
    raw: String,
}

impl RawMessage {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        RawMessage {
            raw: String::from_utf8_lossy(bytes).into_owned(),
        }
    }
}

/// Per-token training counts: number of *messages* of each class that contained
/// the token (Robinson counts message presence, not raw occurrences).
#[derive(Debug, Default, Clone, Copy)]
struct TokenCounts {
    spam: u32,
    ham: u32,
}

impl ToJson for TokenCounts {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("spam", self.spam)
            .set("ham", self.ham)
            .build()
    }
}

impl TokenCounts {
    /// A count absent from the file, or too large for a `u32`, reads as zero,
    /// which is what `#[serde(default)]` gave a missing one.
    fn from_json(value: &Json) -> TokenCounts {
        let count = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        };
        TokenCounts {
            spam: count("spam"),
            ham: count("ham"),
        }
    }
}

/// The trained model: token statistics plus per-class message totals.
#[derive(Debug, Default)]
pub struct SpamModel {
    tokens: HashMap<String, TokenCounts>,
    spam_messages: u32,
    ham_messages: u32,
}

impl ToJson for SpamModel {
    fn to_json(&self) -> Json {
        ObjectBuilder::new()
            .set("tokens", &self.tokens)
            .set("spam_messages", self.spam_messages)
            .set("ham_messages", self.ham_messages)
            .build()
    }
}

impl SpamModel {
    /// The model file. `tokens` is required, as serde required it; the two
    /// totals carried `#[serde(default)]` and still do.
    fn from_json(value: &Json) -> Result<SpamModel, String> {
        let entries = value
            .get("tokens")
            .and_then(Json::as_obj)
            .ok_or_else(|| "missing field `tokens`".to_string())?;
        let mut tokens = HashMap::with_capacity(entries.len());
        for (token, counts) in entries {
            tokens.insert(token.clone(), TokenCounts::from_json(counts));
        }
        let total = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        };
        Ok(SpamModel {
            tokens,
            spam_messages: total("spam_messages"),
            ham_messages: total("ham_messages"),
        })
    }
}

impl SpamModel {
    pub fn spam_messages(&self) -> u32 {
        self.spam_messages
    }

    pub fn ham_messages(&self) -> u32 {
        self.ham_messages
    }

    /// True once at least `min_per_class` messages of *each* class have been
    /// trained. Below this the classifier reports [`Verdict::Unsure`] and never
    /// auto-acts (the cold-start gate).
    pub fn is_trained(&self, min_per_class: u32) -> bool {
        self.spam_messages >= min_per_class && self.ham_messages >= min_per_class
    }

    /// Add a message to the model under the given label.
    pub fn train(&mut self, msg: &RawMessage, label: Label) {
        match label {
            Label::Spam => self.spam_messages += 1,
            Label::Ham => self.ham_messages += 1,
        }
        for token in tokenize(msg) {
            let counts = self.tokens.entry(token).or_default();
            match label {
                Label::Spam => counts.spam += 1,
                Label::Ham => counts.ham += 1,
            }
        }
    }

    /// Robinson per-token probability that a message is spam given the token,
    /// smoothed toward [`ASSUMED_PROB`].
    fn token_prob(&self, counts: TokenCounts) -> f64 {
        let nspam = self.spam_messages.max(1) as f64;
        let nham = self.ham_messages.max(1) as f64;
        let b = counts.spam as f64 / nspam;
        let g = counts.ham as f64 / nham;
        if b + g == 0.0 {
            return ASSUMED_PROB;
        }
        let p = b / (b + g);
        let n = (counts.spam + counts.ham) as f64;
        (STRENGTH * ASSUMED_PROB + n * p) / (STRENGTH + n)
    }

    /// Score a message in `[0, 1]`; higher is more spam-like. Untrained or
    /// signal-free messages score `0.5`.
    pub fn score(&self, msg: &RawMessage) -> f64 {
        let mut probs: Vec<f64> = Vec::new();
        for token in tokenize(msg) {
            if let Some(&counts) = self.tokens.get(&token) {
                if counts.spam + counts.ham == 0 {
                    continue;
                }
                probs.push(self.token_prob(counts).clamp(0.0001, 0.9999));
            }
        }
        if probs.is_empty() {
            return 0.5;
        }

        // Keep the most informative tokens (farthest from 0.5).
        probs.sort_by(|a, b| {
            (b - 0.5)
                .abs()
                .partial_cmp(&(a - 0.5).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        probs.truncate(MAX_TOKENS);

        let n = probs.len();
        let mut ln_spam = 0.0;
        let mut ln_ham = 0.0;
        for &f in &probs {
            ln_spam += f.ln();
            ln_ham += (1.0 - f).ln();
        }

        // Fisher's method: combine token probabilities into two chi-square tail
        // probabilities, then fold them into a single spamicity indicator.
        let s = chi2_prob(-2.0 * ln_spam, 2 * n);
        let h = chi2_prob(-2.0 * ln_ham, 2 * n);
        (1.0 + s - h) / 2.0
    }

    /// Score a message and reduce it to a [`Verdict`] using the configured
    /// thresholds and cold-start gate.
    pub fn classify(
        &self,
        msg: &RawMessage,
        spam_threshold: f64,
        ham_threshold: f64,
        min_per_class: u32,
    ) -> (f64, Verdict) {
        let score = self.score(msg);
        if !self.is_trained(min_per_class) {
            return (score, Verdict::Unsure);
        }
        let verdict = if score >= spam_threshold {
            Verdict::Spam
        } else if score <= ham_threshold {
            Verdict::Ham
        } else {
            Verdict::Unsure
        };
        (score, verdict)
    }

    /// Load a model from disk, returning an empty model if it does not exist or
    /// cannot be parsed (training data is rebuildable, so we never hard-fail).
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => json::parse_slice(&bytes)
                .map_err(|e| e.to_string())
                .and_then(|value| SpamModel::from_json(&value))
                .unwrap_or_else(|e| {
                    log_warn!("[Spam] failed to parse model at {:?}: {}", path, e);
                    SpamModel::default()
                }),
            Err(_) => SpamModel::default(),
        }
    }

    /// Persist the model atomically (write to a temp file, then rename) so a
    /// crash mid-write cannot corrupt the trained store.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create model dir {:?}: {}", parent, e))?;
        }
        // Writing a value cannot fail, so there is no serialize error here.
        let bytes = self.to_json().to_vec();
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write {:?}: {}", tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename into {:?}: {}", path, e))?;
        Ok(())
    }
}

/// Default model location: `$XDG_DATA_HOME/td-mail/spam-model.json`.
///
/// Lives under the *data* dir, not the cache: the trained store is curated by
/// the user (via the J / un-junk training keys) and must survive cache clears.
pub fn model_path() -> PathBuf {
    let data_dir = if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("share")
    } else {
        PathBuf::from(".")
    };
    data_dir.join("td-mail").join("spam-model.json")
}

// --- Tokenization ---

/// Headers whose values carry signal. Each maps to a short namespace prefix so
/// that, e.g., the word "account" in a Subject is distinct from "account" in a
/// From line or the body.
fn header_prefix(name: &str) -> Option<&'static str> {
    match name {
        "from" | "sender" | "return-path" => Some("from"),
        "reply-to" => Some("rply"),
        "to" | "cc" => Some("to"),
        "subject" => Some("subj"),
        "received" => Some("recv"),
        // Note: dkim-signature is intentionally excluded — its value is a base64
        // cryptographic blob, not linguistic signal.
        "authentication-results" | "received-spf" => Some("auth"),
        "list-id" | "list-unsubscribe" => Some("list"),
        "content-type" => Some("ctype"),
        "x-mailer" | "user-agent" => Some("mailer"),
        _ => None,
    }
}

/// Tokenize a raw message into a de-duplicated set of tokens. Header tokens from
/// interesting headers are namespaced; the body is tokenized as plain text with
/// HTML tags stripped.
fn tokenize(msg: &RawMessage) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    let (headers, body) = split_headers_body(&msg.raw);

    for (name, value) in parse_headers(headers) {
        if let Some(prefix) = header_prefix(&name) {
            collect_words(&value, Some(prefix), &mut set);
        }
    }

    let body_text = strip_html(body);
    collect_words(&body_text, None, &mut set);

    set.into_iter().collect()
}

/// Split a raw message at the first blank line into (headers, body).
fn split_headers_body(raw: &str) -> (&str, &str) {
    if let Some(idx) = raw.find("\r\n\r\n") {
        (&raw[..idx], &raw[idx + 4..])
    } else if let Some(idx) = raw.find("\n\n") {
        (&raw[..idx], &raw[idx + 2..])
    } else {
        (raw, "")
    }
}

/// Parse a header block into (lowercased-name, value) pairs, joining folded
/// continuation lines (those beginning with whitespace).
fn parse_headers(headers: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in headers.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            out.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    out
}

/// Remove `<...>` tags so HTML bodies tokenize as their visible text.
fn strip_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// True if a line is almost entirely base64/hex characters — an encoded
/// attachment, inline image, DKIM signature, or hash — rather than prose. Such
/// lines carry no linguistic signal and would otherwise explode the token store
/// (a single image attachment is thousands of junk fragments).
fn is_high_entropy_blob(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 40 {
        return false;
    }
    let coded = t
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .count();
    // Prose is broken up by spaces/punctuation; base64 lines are ~100% coded.
    coded * 100 >= t.len() * 95
}

/// Extract word tokens from `text`, optionally namespaced with `prefix`, into
/// `set`. Tokens are lowercased, 2..=40 chars, and split on anything outside a
/// small token alphabet (letters, digits, and a few mail-relevant symbols).
/// Base64/hex blob lines are skipped, and the set is capped per message.
fn collect_words(text: &str, prefix: Option<&str>, set: &mut HashSet<String>) {
    let mut current = String::new();
    let flush = |current: &mut String, set: &mut HashSet<String>| {
        if current.len() >= 2 && current.len() <= 40 && set.len() < MAX_MESSAGE_TOKENS {
            match prefix {
                Some(p) => set.insert(format!("{}:{}", p, current)),
                None => set.insert(current.clone()),
            };
        }
        current.clear();
    };

    for line in text.lines() {
        if set.len() >= MAX_MESSAGE_TOKENS {
            break;
        }
        if is_high_entropy_blob(line) {
            continue;
        }
        for ch in line.chars() {
            if is_token_char(ch) {
                current.extend(ch.to_lowercase());
            } else {
                flush(&mut current, set);
            }
        }
        // End of line is a token boundary too.
        flush(&mut current, set);
    }
}

fn is_token_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '$' | '!' | '\'' | '_' | '-' | '.' | '@' | '/')
}

// --- Math ---

/// Survival function P(X > chi2) for a chi-square distribution with `df` (even)
/// degrees of freedom. Used by Fisher's method to combine token probabilities.
fn chi2_prob(chi2: f64, df: usize) -> f64 {
    if chi2 <= 0.0 || df == 0 {
        return 1.0;
    }
    let m = chi2 / 2.0;
    let mut term = (-m).exp();
    let mut sum = term;
    let half = df / 2;
    for i in 1..half {
        term *= m / i as f64;
        sum += term;
    }
    sum.min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(headers: &[(&str, &str)], body: &str) -> RawMessage {
        let mut raw = String::new();
        for (k, v) in headers {
            raw.push_str(&format!("{}: {}\r\n", k, v));
        }
        raw.push_str("\r\n");
        raw.push_str(body);
        RawMessage::from_bytes(raw.as_bytes())
    }

    #[test]
    fn split_prefers_crlf_blank_line() {
        let (h, b) = split_headers_body("From: a\r\n\r\nhello body");
        assert_eq!(h, "From: a");
        assert_eq!(b, "hello body");
    }

    #[test]
    fn split_falls_back_to_lf_blank_line() {
        let (h, b) = split_headers_body("Subject: x\n\nthe body");
        assert_eq!(h, "Subject: x");
        assert_eq!(b, "the body");
    }

    #[test]
    fn parse_headers_joins_folded_lines() {
        let headers = "Subject: hello\r\n world\r\nFrom: a@b.com";
        let parsed = parse_headers(headers);
        assert_eq!(
            parsed[0],
            ("subject".to_string(), "hello world".to_string())
        );
        assert_eq!(parsed[1], ("from".to_string(), "a@b.com".to_string()));
    }

    #[test]
    fn strip_html_removes_tags() {
        assert_eq!(strip_html("<p>buy <b>now</b></p>"), "buy now");
    }

    #[test]
    fn subject_tokens_are_namespaced() {
        let m = msg(&[("Subject", "free money")], "");
        let tokens = tokenize(&m);
        assert!(tokens.contains(&"subj:free".to_string()));
        assert!(tokens.contains(&"subj:money".to_string()));
    }

    #[test]
    fn base64_blob_lines_are_skipped() {
        // A realistic base64 attachment line should be detected as a blob...
        let b64 = "VGhpcyBpcyBhIHRlc3QgYmFzZTY0IGF0dGFjaG1lbnQgcGF5bG9hZCBsaW5lIHRoYXQ=";
        assert!(is_high_entropy_blob(b64));
        // ...while normal prose (with spaces/punctuation) is not.
        assert!(!is_high_entropy_blob(
            "Hi Bob, are we still on for lunch tomorrow at noon?"
        ));

        // A message whose body is mostly an encoded attachment should only
        // yield tokens from the readable text, not thousands of b64 fragments.
        let m = msg(
            &[("Subject", "invoice attached")],
            &format!("Please see the attached invoice document\n{b64}\n{b64}\n{b64}"),
        );
        let tokens = tokenize(&m);
        assert!(tokens.contains(&"attached".to_string()));
        assert!(tokens.contains(&"invoice".to_string()));
        assert!(
            tokens.len() < 30,
            "blob fragments leaked into tokens: {} tokens",
            tokens.len()
        );
    }

    #[test]
    fn body_tokens_are_plain() {
        let m = msg(&[("Subject", "hi")], "meeting tomorrow");
        let tokens = tokenize(&m);
        assert!(tokens.contains(&"meeting".to_string()));
        assert!(tokens.contains(&"tomorrow".to_string()));
    }

    #[test]
    fn untrained_model_scores_neutral_and_unsure() {
        let model = SpamModel::default();
        let m = msg(&[("Subject", "anything")], "whatever");
        assert_eq!(model.score(&m), 0.5);
        let (_, verdict) = model.classify(&m, 0.9, 0.2, 5);
        assert_eq!(verdict, Verdict::Unsure);
    }

    #[test]
    fn trained_model_separates_spam_from_ham() {
        let mut model = SpamModel::default();
        for _ in 0..10 {
            model.train(
                &msg(
                    &[("Subject", "cheap viagra")],
                    "buy cheap pills now discount",
                ),
                Label::Spam,
            );
            model.train(
                &msg(
                    &[("Subject", "project update")],
                    "lets meet about the roadmap",
                ),
                Label::Ham,
            );
        }

        let spammy = model.score(&msg(
            &[("Subject", "cheap pills")],
            "discount viagra buy now",
        ));
        let hammy = model.score(&msg(
            &[("Subject", "roadmap meeting")],
            "lets discuss the project update",
        ));

        assert!(spammy > 0.8, "spammy score was {}", spammy);
        assert!(hammy < 0.2, "hammy score was {}", hammy);
    }

    #[test]
    fn cold_start_gate_blocks_verdict_until_min_per_class() {
        let mut model = SpamModel::default();
        // Train enough spam to be confident, but only one ham message.
        for _ in 0..10 {
            model.train(&msg(&[("Subject", "viagra")], "buy now cheap"), Label::Spam);
        }
        model.train(&msg(&[("Subject", "hello")], "regular note"), Label::Ham);

        let m = msg(&[("Subject", "viagra")], "buy now cheap");
        // Score is high...
        assert!(model.score(&m) > 0.8);
        // ...but the verdict is gated because ham_messages (1) < min_per_class (5).
        let (_, verdict) = model.classify(&m, 0.9, 0.2, 5);
        assert_eq!(verdict, Verdict::Unsure);
        assert!(!model.is_trained(5));
    }

    #[test]
    fn uncertain_band_yields_unsure() {
        let mut model = SpamModel::default();
        for _ in 0..10 {
            model.train(
                &msg(&[("Subject", "spam")], "spammy words here"),
                Label::Spam,
            );
            model.train(&msg(&[("Subject", "ham")], "hammy words here"), Label::Ham);
        }
        // A message with no trained signal scores 0.5 -> between thresholds.
        let neutral = msg(&[("Subject", "zzz")], "qqq");
        let (score, verdict) = model.classify(&neutral, 0.9, 0.2, 5);
        assert_eq!(score, 0.5);
        assert_eq!(verdict, Verdict::Unsure);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = crate::testing::tempdir().unwrap();
        let path = dir.path().join("model.json");

        let mut model = SpamModel::default();
        model.train(&msg(&[("Subject", "viagra")], "buy now"), Label::Spam);
        model.train(&msg(&[("Subject", "hi")], "meeting"), Label::Ham);
        model.save(&path).unwrap();

        let loaded = SpamModel::load(&path);
        assert_eq!(loaded.spam_messages(), 1);
        assert_eq!(loaded.ham_messages(), 1);
        // Scores should match the in-memory model.
        let m = msg(&[("Subject", "viagra")], "buy now");
        assert_eq!(model.score(&m), loaded.score(&m));
    }

    #[test]
    fn load_missing_file_returns_empty_model() {
        let model = SpamModel::load(Path::new("/nonexistent/td-mail/spam-model.json"));
        assert_eq!(model.spam_messages(), 0);
        assert_eq!(model.ham_messages(), 0);
    }

    #[test]
    fn chi2_prob_bounds() {
        // Zero statistic -> certain (tail probability 1).
        assert_eq!(chi2_prob(0.0, 4), 1.0);
        // Larger statistic -> smaller tail probability, always within [0, 1].
        let p = chi2_prob(10.0, 4);
        assert!(p > 0.0 && p < 1.0, "p was {}", p);
    }

    #[test]
    fn verdict_as_str() {
        assert_eq!(Verdict::Spam.as_str(), "spam");
        assert_eq!(Verdict::Ham.as_str(), "ham");
        assert_eq!(Verdict::Unsure.as_str(), "unsure");
    }
}
