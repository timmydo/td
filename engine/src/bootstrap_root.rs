//! The bootstrap root manifest, `seed/bootstrap-root.txt`.
//!
//! Default builds start at the gcc-14 cut instead of stage0: the ladder rungs
//! below it are a reference-closed set of locally built store items, pinned
//! here by their input-addressed basenames and NAR hashes. td-recipe-eval
//! writes the file (`bootstrap-root pin`) and consumes the exports;
//! td-builder admits the items as audited seeds against it. One parser keeps
//! the two sides from reading the same text two ways.
//!
//! A manifest with no `export` rows is UNPINNED: nothing is cut and every
//! build starts from stage0.
//!
//! ```text
//! format 1
//! builder-abi <the TD_BUILDER_ABI the ladder was pinned under>
//! ladder <sha256 over the ladder recipes' canonical JSON>
//! export <recipe stem> <store basename>
//! item <store basename> sha256:<64 hex> <ref basename>,…|-
//! ```

use crate::sha256;

/// One pinned store item: its basename, NAR hash and the basenames it
/// references (itself included when it self-references).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub base: String,
    pub nar: String,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub builder_abi: String,
    pub ladder: String,
    pub exports: Vec<(String, String)>,
    pub items: Vec<Item>,
}

const FILE: &str = "seed/bootstrap-root.txt";

fn valid_base(base: &str) -> bool {
    base.len() > 33
        && base.as_bytes().get(32) == Some(&b'-')
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'+'))
}

fn valid_hex64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_nar(nar: &str) -> bool {
    nar.strip_prefix("sha256:").is_some_and(valid_hex64)
}

fn valid_abi(abi: &str) -> bool {
    !abi.is_empty()
        && abi
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
}

impl Root {
    /// Parse and validate a manifest. Every row is checked: this is a trust
    /// anchor, so a malformed or inconsistent file is an error, never a
    /// partial answer.
    pub fn parse(text: &str) -> Result<Root, String> {
        let mut format = None;
        let mut builder_abi = None;
        let mut ladder = None;
        let mut exports: Vec<(String, String)> = Vec::new();
        let mut items: Vec<Item> = Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = || format!("{FILE} line {}", n + 1);
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["format", value] if format.is_none() => format = Some(*value),
                ["builder-abi", value] if builder_abi.is_none() && valid_abi(value) => {
                    builder_abi = Some((*value).to_string())
                }
                ["ladder", value] if ladder.is_none() && valid_hex64(value) => {
                    ladder = Some((*value).to_string())
                }
                ["export", stem, base] if valid_base(base) && !stem.is_empty() => {
                    if exports.iter().any(|(s, _)| s == stem) {
                        return Err(format!("{}: duplicate export `{stem}'", at()));
                    }
                    exports.push(((*stem).to_string(), (*base).to_string()));
                }
                ["item", base, nar, refs] if valid_base(base) && valid_nar(nar) => {
                    if items.iter().any(|item| item.base == *base) {
                        return Err(format!("{}: duplicate item `{base}'", at()));
                    }
                    let refs: Vec<String> = if *refs == "-" {
                        Vec::new()
                    } else {
                        refs.split(',').map(str::to_string).collect()
                    };
                    if refs.iter().any(|r| !valid_base(r)) {
                        return Err(format!("{}: malformed reference list `{line}'", at()));
                    }
                    items.push(Item {
                        base: (*base).to_string(),
                        nar: (*nar).to_string(),
                        refs,
                    });
                }
                _ => return Err(format!("{}: malformed or repeated row `{line}'", at())),
            }
        }
        if format != Some("1") {
            return Err(format!("{FILE}: want exactly one `format 1' row"));
        }
        let builder_abi = builder_abi.ok_or_else(|| format!("{FILE}: no `builder-abi' row"))?;
        let ladder = ladder.ok_or_else(|| format!("{FILE}: no `ladder' row"))?;
        let root = Root {
            builder_abi,
            ladder,
            exports,
            items,
        };
        for (stem, base) in &root.exports {
            if root.item(base).is_none() {
                return Err(format!(
                    "{FILE}: export `{stem}' names `{base}', which is not a pinned item"
                ));
            }
        }
        for item in &root.items {
            for r in &item.refs {
                if root.item(r).is_none() {
                    return Err(format!(
                        "{FILE}: item `{}' references `{r}', which is not a pinned item — \
                         the root must be closed under references",
                        item.base
                    ));
                }
            }
        }
        Ok(root)
    }

    pub fn is_pinned(&self) -> bool {
        !self.exports.is_empty()
    }

    pub fn item(&self, base: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.base == base)
    }

    pub fn export(&self, stem: &str) -> Option<&str> {
        self.exports
            .iter()
            .find(|(s, _)| s == stem)
            .map(|(_, base)| base.as_str())
    }

    /// The canonical file text: exports and items sorted, references sorted.
    pub fn render(&self) -> String {
        let mut exports = self.exports.clone();
        exports.sort();
        let mut items = self.items.clone();
        items.sort_by(|a, b| a.base.cmp(&b.base));
        let mut out = String::from(
            "# seed/bootstrap-root.txt — the pinned bootstrap root (AGENTS.md, \"Target\n\
             # artifact graph\"). Written by `td-recipe-eval bootstrap-root pin`; do not\n\
             # edit by hand.\n",
        );
        out.push_str("format 1\n");
        out.push_str(&format!("builder-abi {}\n", self.builder_abi));
        out.push_str(&format!("ladder {}\n", self.ladder));
        for (stem, base) in &exports {
            out.push_str(&format!("export {stem} {base}\n"));
        }
        for item in &items {
            let mut refs = item.refs.clone();
            refs.sort();
            let refs = if refs.is_empty() {
                "-".to_string()
            } else {
                refs.join(",")
            };
            out.push_str(&format!("item {} {} {refs}\n", item.base, item.nar));
        }
        out
    }
}

/// The digest of the ladder recipes the root was built from: SHA-256 over
/// `stem TAB canonical-json LF` for each recipe, sorted by stem. A changed
/// ladder recipe moves it, which is the signal that the pin is stale.
pub fn ladder_digest<'a>(recipes: impl IntoIterator<Item = (&'a str, String)>) -> String {
    let mut rows: Vec<(&str, String)> = recipes.into_iter().collect();
    rows.sort();
    let mut text = String::new();
    for (stem, json) in rows {
        text.push_str(stem);
        text.push('\t');
        text.push_str(&json);
        text.push('\n');
    }
    sha256::hex_digest(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0123456789abcdfghijklmnpqrsvwxyz-alpha-1";
    const B: &str = "0123456789abcdfghijklmnpqrsvwxyy-beta-2";
    const NAR: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    const LADDER: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn sample() -> String {
        format!(
            "format 1\nbuilder-abi 4\nladder {LADDER}\nexport alpha {A}\n\
             item {A} {NAR} {A},{B}\nitem {B} {NAR} -\n"
        )
    }

    #[test]
    fn a_closed_manifest_parses_and_round_trips() {
        let root = Root::parse(&sample()).unwrap();
        assert_eq!(root.builder_abi, "4");
        assert_eq!(root.export("alpha"), Some(A));
        assert_eq!(
            root.item(A).unwrap().refs,
            vec![A.to_string(), B.to_string()]
        );
        assert!(root.item(B).unwrap().refs.is_empty());
        let text = root.render();
        assert_eq!(Root::parse(&text).unwrap().render(), text);
    }

    #[test]
    fn an_open_or_malformed_manifest_is_refused() {
        let open = sample().replace(&format!("item {B} {NAR} -\n"), "");
        assert!(Root::parse(&open)
            .unwrap_err()
            .contains("closed under references"));
        let dangling = sample().replace(
            &format!("export alpha {A}"),
            "export alpha 0123456789abcdfghijklmnpqrsvwxzz-missing-3",
        );
        assert!(Root::parse(&dangling).is_err());
        let unpinned = Root::parse(&format!("format 1\nbuilder-abi 4\nladder {LADDER}\n")).unwrap();
        assert!(!unpinned.is_pinned());
        assert!(Root::parse(&sample()).unwrap().is_pinned());
        for bad in [
            sample().replace("format 1", "format 2"),
            sample().replace(&format!("ladder {LADDER}\n"), ""),
            sample().replace("builder-abi 4", "builder-abi 4 5"),
            sample().replace(NAR, "sha256:zz"),
            sample().replace("export alpha", "export alpha extra"),
            format!("{}builder-abi 5\n", sample()),
            format!("{}export alpha {A}\n", sample()),
            format!("{}item {B} {NAR} -\n", sample()),
        ] {
            assert!(Root::parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_ladder_digest_ignores_order_and_sees_content() {
        let a = ladder_digest([("a", "{}".to_string()), ("b", "[]".to_string())]);
        let b = ladder_digest([("b", "[]".to_string()), ("a", "{}".to_string())]);
        assert_eq!(a, b);
        assert_ne!(
            a,
            ladder_digest([("a", "{}".to_string()), ("b", "[1]".to_string())])
        );
    }
}
