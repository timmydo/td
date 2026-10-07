//! The push scan's credential shapes (DESIGN.md §9, Pushing): a fixed,
//! deterministic list of what private keys and the tokens of the common
//! forges and clouds look like, matched by hand, with no expression
//! engine. A match is said by its kind alone, never its text.

/// A shape: its prefix, the bytes that may follow it, and how many of
/// them at least, and at most when bounded.
struct Shape {
    kind: &'static str,
    prefix: &'static str,
    body: fn(u8) -> bool,
    least: usize,
    most: Option<usize>,
    /// What the first byte after the prefix must be.
    lead: fn(u8) -> bool,
    /// Whether the prefix, short enough to sit inside ordinary words,
    /// counts only where a word starts.
    alone: bool,
}

const fn token(
    kind: &'static str,
    prefix: &'static str,
    body: fn(u8) -> bool,
    least: usize,
    most: Option<usize>,
) -> Shape {
    Shape {
        kind,
        prefix,
        body,
        least,
        most,
        lead: any,
        alone: false,
    }
}

fn any(_: u8) -> bool {
    true
}

fn alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn dashed(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn dotted(b: u8) -> bool {
    dashed(b) || b == b'.'
}

fn upper(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

fn hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn digit(b: u8) -> bool {
    b.is_ascii_digit()
}

const GITHUB: &str = "a GitHub token";
const GITLAB: &str = "a GitLab token";
const AWS: &str = "an AWS access key";
const GOOGLE: &str = "a Google credential";
const SLACK: &str = "a Slack token";
const OPENAI: &str = "an OpenAI key";
const STRIPE: &str = "a Stripe key";
const KEY: &str = "a private key";

const SHAPES: &[Shape] = &[
    token(GITHUB, "ghp_", alnum, 36, None),
    token(GITHUB, "gho_", alnum, 36, None),
    token(GITHUB, "ghu_", alnum, 36, None),
    token(GITHUB, "ghs_", alnum, 36, None),
    token(GITHUB, "ghr_", alnum, 36, None),
    token(GITHUB, "github_pat_", word, 59, None),
    token(GITLAB, "glpat-", dashed, 20, None),
    token(GITLAB, "glptt-", dashed, 20, None),
    token(GITLAB, "gldt-", dashed, 20, None),
    token(GITLAB, "glrt-", dashed, 20, None),
    Shape {
        alone: true,
        ..token(AWS, "AKIA", upper, 16, Some(16))
    },
    Shape {
        alone: true,
        ..token(AWS, "ASIA", upper, 16, Some(16))
    },
    Shape {
        alone: true,
        ..token(GOOGLE, "AIza", dashed, 35, Some(35))
    },
    token(GOOGLE, "GOCSPX-", dashed, 24, None),
    Shape {
        alone: true,
        ..token(GOOGLE, "ya29.", dotted, 20, None)
    },
    // A placeholder such as `xoxb-your-token` is not one: a Slack
    // token's first part is a number.
    Shape {
        lead: digit,
        ..token(SLACK, "xoxb-", dashed, 10, None)
    },
    Shape {
        lead: digit,
        ..token(SLACK, "xoxp-", dashed, 10, None)
    },
    Shape {
        lead: digit,
        ..token(SLACK, "xoxa-", dashed, 10, None)
    },
    Shape {
        lead: digit,
        ..token(SLACK, "xoxr-", dashed, 10, None)
    },
    Shape {
        lead: digit,
        ..token(SLACK, "xoxs-", dashed, 10, None)
    },
    token("an OpenRouter key", "sk-or-v1-", hex, 64, Some(64)),
    token("an Anthropic key", "sk-ant-", dashed, 32, None),
    token(OPENAI, "sk-proj-", dashed, 32, None),
    token(OPENAI, "sk-svcacct-", dashed, 32, None),
    token(OPENAI, "sk-admin-", dashed, 32, None),
    // The older keys carry `openai` in base64 at their middle.
    token(OPENAI, "T3BlbkFJ", alnum, 0, None),
    token(STRIPE, "sk_live_", alnum, 24, None),
    token(STRIPE, "rk_live_", alnum, 24, None),
    Shape {
        alone: true,
        ..token("an npm token", "npm_", alnum, 36, Some(36))
    },
    token("a PyPI token", "pypi-AgEIcHlwaS5vcmc", dashed, 50, None),
    Shape {
        alone: true,
        ..token("a crates.io token", "cio", alnum, 32, Some(32))
    },
    Shape {
        alone: true,
        ..token("a Hugging Face token", "hf_", alnum, 30, None)
    },
];

/// What a private key's armour begins with, whatever its algorithm, and
/// how far past it its name is looked for.
const KEY_BEGIN: &[u8] = b"-----BEGIN ";
const KEY_NAME: usize = 64;
const KEY_END: &[u8] = b"PRIVATE KEY-----";
const KEY_FIXED: &[&[u8]] = &[
    b"-----BEGIN PGP PRIVATE KEY BLOCK-----",
    b"PuTTY-User-Key-File-",
];

/// The kind of the first credential shape in `line`, if any.
pub fn first(line: &[u8]) -> Option<&'static str> {
    within(line, 0, true)
}

/// As `first`, of the shapes that start at `start` or later, the bytes
/// before it only the context they start in. When `whole` is false the
/// line goes on past `line`'s end, so a shape whose length is bounded
/// and whose bytes run to that end is left for the next piece, which
/// holds it whole.
pub fn within(line: &[u8], start: usize, whole: bool) -> Option<&'static str> {
    if let Some(kind) = armour(line, start) {
        return Some(kind);
    }
    for shape in SHAPES {
        let prefix = shape.prefix.as_bytes();
        let mut from = start;
        while let Some(at) = find(line, prefix, from) {
            from = at.saturating_add(1);
            // A short prefix starts a word: `xAKIA` is not one.
            if shape.alone && at > 0 && line.get(at - 1).copied().is_some_and(word) {
                continue;
            }
            let body = line.get(at + prefix.len()..).unwrap_or_default();
            if body.first().is_some_and(|b| !(shape.lead)(*b)) {
                continue;
            }
            // Counted no further than decides it, so a long run costs
            // no more than a short one.
            let cap = shape.most.unwrap_or(shape.least).saturating_add(1);
            let run = body
                .iter()
                .take(cap)
                .take_while(|b| (shape.body)(**b))
                .count();
            if !whole && run == body.len() && shape.most.is_some_and(|most| run <= most) {
                continue;
            }
            if run >= shape.least && shape.most.is_none_or(|most| run == most) {
                return Some(shape.kind);
            }
        }
    }
    None
}

/// Private-key armour in `line` at `start` or later: what a key file's
/// whole text is scanned for.
pub fn armour(line: &[u8], start: usize) -> Option<&'static str> {
    if KEY_FIXED
        .iter()
        .any(|fixed| find(line, fixed, start).is_some())
    {
        return Some(KEY);
    }
    let mut from = start;
    while let Some(at) = find(line, KEY_BEGIN, from) {
        from = at.saturating_add(1);
        let name: Vec<u8> = line
            .get(at + KEY_BEGIN.len()..)
            .unwrap_or_default()
            .iter()
            .copied()
            .take(KEY_NAME)
            .take_while(|b| b.is_ascii_uppercase() || *b == b' ' || *b == b'-')
            .collect();
        if find(&name, KEY_END, 0).is_some() {
            return Some(KEY);
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| at + from)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Each shape is found wherever it sits in a line, and a near miss
    /// is not: too short, too long where the length is fixed, a short
    /// prefix inside a word, or a placeholder.
    #[test]
    fn credential_shapes_are_found_and_near_misses_are_not() {
        let a = |n: usize| "a".repeat(n);
        let upper = |n: usize| "A".repeat(n);
        let found = [
            (format!("token = \"ghp_{}\"", a(36)), GITHUB),
            (format!("github_pat_{}", a(82)), GITHUB),
            // Escaped in JSON, or in a URL, a token is still one.
            (format!("{{\"t\": \"x\\nghp_{}\"}}", a(36)), GITHUB),
            (format!("?t%3Dgho_{}", a(36)), GITHUB),
            (format!("glpat-{}", a(20)), GITLAB),
            (format!("glrt-{}", a(20)), GITLAB),
            (format!("aws_access_key_id={}{}", "AKIA", upper(16)), AWS),
            (format!("AIza{}", a(35)), GOOGLE),
            (format!("GOCSPX-{}", a(28)), GOOGLE),
            (format!("ya29.{}", a(40)), GOOGLE),
            (format!("xoxb-1234-{}", a(24)), SLACK),
            (format!("sk-or-v1-{}", "0".repeat(64)), "an OpenRouter key"),
            (format!("sk-ant-api03-{}", a(40)), "an Anthropic key"),
            (format!("sk-svcacct-{}", a(40)), OPENAI),
            (format!("sk-{}T3BlbkFJ{}", a(20), a(20)), OPENAI),
            (format!("sk_live_{}", a(24)), STRIPE),
            (format!("npm_{}", a(36)), "an npm token"),
            (format!("hf_{}", a(34)), "a Hugging Face token"),
            ("-----BEGIN OPENSSH PRIVATE KEY-----".into(), KEY),
            ("+-----BEGIN RSA PRIVATE KEY-----".into(), KEY),
            ("-----BEGIN PRIVATE KEY-----".into(), KEY),
            ("-----BEGIN PGP PRIVATE KEY BLOCK-----".into(), KEY),
            ("PuTTY-User-Key-File-3: ssh-ed25519".into(), KEY),
        ];
        for (line, kind) in &found {
            assert_eq!(first(line.as_bytes()), Some(*kind), "{line}");
        }
        let missed = [
            format!("ghp_{}", a(35)),
            format!("xAKIA{}", upper(16)),
            format!("AKIA{}", upper(17)),
            format!("sk-or-v1-{}", "0".repeat(63)),
            format!("thf_{}", a(34)),
            "xoxb-your-token-goes-here".into(),
            "-----BEGIN PUBLIC KEY-----".into(),
            "-----BEGIN CERTIFICATE-----".into(),
            "the ghp_ prefix alone".into(),
            String::new(),
        ];
        for line in &missed {
            assert_eq!(first(line.as_bytes()), None, "{line}");
        }
        // Bytes that are not UTF-8 around one hide nothing.
        let mut binary = vec![0xff, 0x00, 0xfe];
        binary.extend_from_slice(format!("ghp_{}", a(36)).as_bytes());
        binary.push(0x80);
        assert_eq!(first(&binary), Some(GITHUB));
    }

    /// A piece of a longer line leaves a bounded shape that runs to its
    /// end for the next piece, and takes the bytes before `start` as
    /// context only.
    #[test]
    fn a_piece_leaves_what_its_end_cannot_decide() {
        let aws = format!("+ AKIA{}", "A".repeat(16));
        assert_eq!(within(aws.as_bytes(), 0, false), None);
        assert_eq!(within(aws.as_bytes(), 0, true), Some(AWS));
        assert_eq!(within(format!("{aws} ").as_bytes(), 0, false), Some(AWS));
        // An unbounded one is decided once long enough.
        let github = format!("+ ghp_{}", "a".repeat(36));
        assert_eq!(within(github.as_bytes(), 0, false), Some(GITHUB));
        // Before `start`, context: `x` makes the next a word's tail.
        let tail = format!("+xAKIA{} ", "A".repeat(16));
        assert_eq!(within(tail.as_bytes(), 2, true), None);
        assert_eq!(within(tail.as_bytes(), 3, true), None);
        let key = format!("+ ghp_{} -----BEGIN RSA PRIVATE KEY-----", "a".repeat(36));
        assert_eq!(within(key.as_bytes(), 6, true), Some(KEY));
    }

    /// Every shape fits in the overlap a long line's pieces share, so a
    /// shape cut by a piece's end is held whole by the next.
    #[test]
    fn every_shape_fits_in_a_pieces_overlap() {
        for shape in SHAPES {
            let longest = shape.prefix.len() + shape.most.unwrap_or(shape.least) + 1;
            assert!(longest < crate::git::OVERLAP, "{}", shape.prefix);
        }
        assert!(KEY_BEGIN.len() + KEY_NAME < crate::git::OVERLAP);
        for fixed in KEY_FIXED {
            assert!(fixed.len() < crate::git::OVERLAP);
        }
    }

    /// A line made of armour's start, or of a prefix, again and again
    /// costs no more than its length.
    #[test]
    fn a_repeated_prefix_costs_its_length() {
        for unit in ["-----BEGIN ", "-----BEGIN PRIVATE", "AKIA", "ghp_", "-AIza"] {
            // Its runs end one past a multiple of the unit: no run is a whole
            // key.
            let line = unit.repeat(64 * 1024 / unit.len()) + "-";
            let started = std::time::Instant::now();
            assert_eq!(first(line.as_bytes()), None, "{unit}");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "{unit}: {:?}",
                started.elapsed()
            );
        }
    }
}
