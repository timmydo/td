//! The td recipe lock format. A lock pins every input a `build-recipe` needs to
//! a store path with NO guix `specification->package`. Each non-comment line is
//!
//!   NAME PATH [CLASS]
//!
//! whitespace-separated (store paths never contain spaces). CLASS is one of
//! `source | seed | td-recipe-output | crate` and is OPTIONAL: a 2-field line
//! keeps the historical inference — the recipe's `<name>-source` key → `source`,
//! a NAME ending `.crate` → `crate`, everything else → `seed` — so every
//! pre-typed lock parses byte-for-byte the same and no existing gate that
//! shell-parses a 2-field lock breaks (those locks stay 2-field).
//!
//! The `td-recipe-output` class is the new edge: it names a dependency td BUILDS
//! itself (a `build-plan` step) and SUBSTITUTES into this recipe's inputs. When a
//! lock is consumed standalone (e.g. by `recipe-checks`) its recorded PATH is the
//! guix oracle for that dep; `build-plan` rewrites it to td's own output before
//! the build, which is how a downstream `.drv` comes to reference td's dep, not
//! guix's. To `build-recipe` a `td-recipe-output` is just another build input
//! (an input-src) — only its PATH (oracle vs td) changes.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

/// How a lock entry is consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The recipe's source tree/tarball (TD_SRC).
    Source,
    /// A guix-built toolchain/library seed, retired LAST (§5).
    Seed,
    /// A dependency td builds itself and substitutes (build-plan).
    TdRecipeOutput,
    /// A vendored Rust crate (TD_VENDOR_CRATES).
    Crate,
}

impl Class {
    fn parse(s: &str) -> Result<Class, String> {
        match s {
            "source" => Ok(Class::Source),
            "seed" => Ok(Class::Seed),
            "td-recipe-output" => Ok(Class::TdRecipeOutput),
            "crate" => Ok(Class::Crate),
            other => Err(format!(
                "unknown lock class `{other}' (known: source, seed, td-recipe-output, crate)"
            )),
        }
    }
}

/// One parsed lock entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub class: Class,
}

/// Infer the CLASS of an untyped (2-field) line from its NAME, reproducing the
/// historical build-recipe routing exactly.
fn infer_class(name: &str, source_name: &str) -> Class {
    if name == source_name {
        Class::Source
    } else if name.ends_with(".crate") {
        Class::Crate
    } else {
        Class::Seed
    }
}

/// Parse a lock's text into entries. `source_name` is the recipe's source key
/// (`"<pkg>-source"`), used only to infer the class of untyped lines. Blank lines
/// and `#` comments are skipped.
pub fn parse(text: &str, source_name: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        let (name, path, class) = match toks.as_slice() {
            [n, p] => (*n, *p, infer_class(n, source_name)),
            [n, p, c] => (*n, *p, Class::parse(c)?),
            _ => {
                return Err(format!(
                    "malformed lock line (want `NAME PATH [CLASS]'): {line}"
                ))
            }
        };
        out.push(Entry {
            name: name.to_string(),
            path: path.to_string(),
            class,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn untyped_lines_infer_the_historical_classes() {
        let text = "\
# a comment, skipped

nano-source /gnu/store/aaa-nano-8.7.1.tar.xz
itoa-1.0.11.crate /gnu/store/bbb-itoa.crate
ncurses /gnu/store/ccc-ncurses-6.2
";
        let e = parse(text, "nano-source").unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].class, Class::Source);
        assert_eq!(e[0].path, "/gnu/store/aaa-nano-8.7.1.tar.xz");
        assert_eq!(e[1].class, Class::Crate);
        assert_eq!(e[2].class, Class::Seed);
        assert_eq!(e[2].name, "ncurses");
    }

    #[test]
    fn explicit_class_is_honoured_over_inference() {
        // `pcre2` would infer `seed`, but the explicit class makes it the new edge.
        let text = "pcre2 /gnu/store/ddd-pcre2-10.42 td-recipe-output\n";
        let e = parse(text, "grep-source").unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].class, Class::TdRecipeOutput);
        assert_eq!(e[0].name, "pcre2");
        assert_eq!(e[0].path, "/gnu/store/ddd-pcre2-10.42");
    }

    #[test]
    fn an_explicit_source_or_seed_class_parses() {
        let e = parse(
            "grep-source /gnu/store/x-grep.tar.xz source\nmake /gnu/store/y-make seed\n",
            "grep-source",
        )
        .unwrap();
        assert_eq!(e[0].class, Class::Source);
        assert_eq!(e[1].class, Class::Seed);
    }

    #[test]
    fn an_unknown_class_is_an_error() {
        let err = parse("pcre2 /gnu/store/z-pcre2 frobnicate\n", "grep-source").unwrap_err();
        assert!(err.contains("unknown lock class"), "got: {err}");
    }

    #[test]
    fn a_one_field_line_is_malformed() {
        let err = parse("lonely-token\n", "grep-source").unwrap_err();
        assert!(err.contains("malformed lock line"), "got: {err}");
    }
}
