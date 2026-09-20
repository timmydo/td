//! In-tree "local source" staging shared by td-recipe-eval (which stages a
//! recipe's declared `local_source` + `local_source_trees` to intern the
//! seed) and td-builder (which independently re-derives the SAME staged
//! bytes from the repository root named by `TD_AUTO_REPO_ROOT`, to verify a
//! `--auto` map/db entry for a local-source-roster key without trusting a
//! caller-supplied digest — re #469 local-source-roster split).
//!
//! Keeping the exclusion rule and the staging shape in exactly ONE place is
//! what lets the two sides agree on a NAR hash without maintaining two
//! copies of the same list: a divergence here would silently make one
//! side's "identity" a different set of bytes than the other's. The actual
//! NAR hashing stays td-builder's own (`nar.rs`'s confined syscall surface,
//! which this dependency-free, `unsafe`-forbidding crate cannot carry); both
//! sides reach it either through `td-builder` (recipes, by subprocess) or
//! in-process (builder, calling its own `nar_hash`).

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};

/// Entries excluded from local-source staging at every depth: build output,
/// VCS metadata, and design documents (`DEVELOPMENT.md` "Local-source
/// staging excludes ..."). Excluded from BOTH the staged tree and its
/// content address — a design document must not be a build input.
pub fn excluded_entry(name: &OsStr) -> bool {
    matches!(name.to_str(), Some("target") | Some(".git") | Some("DESIGN.md"))
}

/// Resolve and validate a repo-relative local-source path against `root`: a
/// plain relative path (no `..`, `.`, or absolute component) that, once
/// symlinks are resolved, stays under the root. Returns the CANONICAL path,
/// so a caller that copies it copies the validated bytes.
pub fn resolve_tree(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() {
        return Err("local source path is empty".into());
    }
    let relp = Path::new(rel);
    for comp in relp.components() {
        if !matches!(comp, Component::Normal(_)) {
            return Err(format!(
                "local source `{rel}' must be a plain repo-relative path \
                 (no `..', `.', or absolute root)"
            ));
        }
    }
    let dir = root.join(relp);
    if !dir.is_dir() {
        return Err(format!(
            "local source `{rel}' is not a directory ({})",
            dir.display()
        ));
    }
    // Defense beyond the lexical `..` check: a symlinked path COMPONENT could
    // still resolve outside the checkout and smuggle ambient (non-committed)
    // bytes into the interned/re-derived tree, breaking the in-tree
    // provenance boundary. Canonicalize both and require the source stays
    // under the repo root; the caller then copies this resolved path.
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize repo root {}: {e}", root.display()))?;
    let canon_dir = dir
        .canonicalize()
        .map_err(|e| format!("canonicalize local source {}: {e}", dir.display()))?;
    if !canon_dir.starts_with(&canon_root) {
        return Err(format!(
            "local source `{rel}' resolves outside the checkout ({}) — a symlinked \
             component must not escape the repo (#469 in-tree provenance)",
            canon_dir.display()
        ));
    }
    // `starts_with` alone admits the degenerate case where a symlinked
    // component resolves to the root ITSELF (not merely somewhere under it):
    // a declared local source must be a proper subdirectory, never the whole
    // checkout (which would stage its own `.git`/build-output exclusions
    // aside but otherwise hash the entire repository as one "local source").
    if canon_dir == canon_root {
        return Err(format!(
            "local source `{rel}' resolves to the repository root itself — a declared \
             local source must be a proper subdirectory, not the checkout root \
             (#469 in-tree provenance)"
        ));
    }
    Ok(canon_dir)
}

fn remove_path_if_exists(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            let removed = if meta.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            };
            removed.map_err(|e| format!("remove {}: {e}", path.display()))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect {}: {e}", path.display())),
    }
}

/// Copy a validated local-source tree with staging exclusions to `dst`.
/// Preserve symlinks and executable bits in sorted entry order — the exact
/// shape a NAR hash of the result reads back. Other untracked or modified
/// files enter the content address; there is no git-tracked-file filter (a
/// git subprocess is not something this dependency-free surface can use).
pub fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let target = fs::read_link(src)?;
        let _ = fs::remove_file(dst);
        symlink(target, dst)?;
        return Ok(());
    }
    if ftype.is_dir() {
        fs::create_dir_all(dst)?;
        let mut children = Vec::new();
        for entry in fs::read_dir(src)? {
            children.push(entry?.path());
        }
        children.sort();
        for child in children {
            let Some(name) = child.file_name() else {
                continue;
            };
            if excluded_entry(name) {
                continue;
            }
            copy_tree(&child, &dst.join(name))?;
        }
        fs::set_permissions(dst, meta.permissions())?;
        return Ok(());
    }
    if ftype.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        fs::set_permissions(dst, meta.permissions())?;
    }
    Ok(())
}

/// Stage a local source (main tree `rel` plus sibling `trees`) under `dest`,
/// exactly as its recipe declares it: with no siblings, `dest` IS the copied
/// main tree; with siblings, `dest` holds every tree — the main one
/// included — each under its own basename, so a relative path between trees
/// resolves as it does in the checkout. Every tree is resolved and named
/// before anything is copied, so a refused roster leaves no half-staged
/// directory behind. Two trees with one basename are refused rather than
/// merged.
pub fn stage(
    root: &Path,
    key: &str,
    rel: &str,
    trees: &[String],
    dest: &Path,
) -> Result<PathBuf, String> {
    let dir = resolve_tree(root, rel)?;
    if trees.is_empty() {
        remove_path_if_exists(dest)?;
        copy_tree(&dir, dest)
            .map_err(|e| format!("copy local source {} for `{key}': {e}", dir.display()))?;
        return Ok(dest.to_path_buf());
    }
    let mut resolved: Vec<(String, PathBuf)> = vec![(rel.to_string(), dir)];
    for tree in trees {
        resolved.push((tree.clone(), resolve_tree(root, tree)?));
    }
    let mut staged: Vec<(PathBuf, String)> = Vec::new();
    for (tree_rel, tree_dir) in resolved {
        // Named by the declared path, not the canonical one: the crate's
        // relative paths (`../engine`, `../../td-boot/src`) are written
        // against the checkout's names, which a symlinked tree would not
        // keep.
        let name = Path::new(&tree_rel)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .ok_or_else(|| format!("local source tree `{tree_rel}' for `{key}' has no name"))?;
        if staged.iter().any(|(_, seen)| *seen == name) {
            return Err(format!(
                "local source `{key}' stages two trees named `{name}' — sibling trees \
                 must have distinct basenames"
            ));
        }
        staged.push((tree_dir, name));
    }
    remove_path_if_exists(dest)?;
    fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    for (tree_dir, name) in &staged {
        copy_tree(tree_dir, &dest.join(name)).map_err(|e| {
            format!(
                "copy local source tree {} for `{key}': {e}",
                tree_dir.display()
            )
        })?;
    }
    Ok(dest.to_path_buf())
}

/// One `seed/local-source-roster.txt` row: the key, its declared main path,
/// and its sibling trees in declared order (empty when the recipe stages no
/// siblings).
pub type RosterRow<'a> = (&'a str, &'a str, Vec<&'a str>);

/// Parse a `seed/local-source-roster.txt` table: `key path` or `key path
/// tree,tree,...` per line, `#` comments. The ONE parser td-recipe-eval and
/// td-builder both call (re #469 local-source-roster split, moved here so a
/// divergence between the two copies cannot silently parse the same text
/// two different ways) — a malformed row is a hard error, like
/// `seed-digests.txt`: this table names the paths a consumer is about to
/// stage and hash, so it is a trust anchor, never best-effort.
pub fn parse_roster(text: &str) -> Result<Vec<RosterRow<'_>>, String> {
    let mut rows: Vec<RosterRow<'_>> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let key = it.next();
        let path = it.next();
        let trees = it.next();
        let extra = it.next();
        match (key, path, trees, extra) {
            (Some(key), Some(path), trees, None) if !path.contains(',') => {
                if rows.iter().any(|(k, _, _)| *k == key) {
                    return Err(format!(
                        "seed/local-source-roster.txt line {}: duplicate key `{key}' — each \
                         local source is declared exactly once (re #469)",
                        n + 1
                    ));
                }
                let trees: Vec<&str> = match trees {
                    None => Vec::new(),
                    Some(t) => {
                        let parts: Vec<&str> = t.split(',').collect();
                        if parts.iter().any(|p| p.is_empty()) {
                            return Err(format!(
                                "seed/local-source-roster.txt line {}: empty sibling-tree \
                                 element in `{t}' — no leading, trailing, or doubled comma \
                                 (omit the field entirely for no siblings)",
                                n + 1
                            ));
                        }
                        parts
                    }
                };
                rows.push((key, path, trees));
            }
            _ => {
                return Err(format!(
                    "seed/local-source-roster.txt line {}: malformed row `{line}' (want \
                     `key path' or `key path tree,tree,...')",
                    n + 1
                ))
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roster_accepts_rows_with_and_without_siblings() {
        let rows = parse_roster("# c\nk1 path1\n\nk2 path2 a,b,c\n").unwrap();
        assert_eq!(
            rows,
            vec![("k1", "path1", vec![]), ("k2", "path2", vec!["a", "b", "c"])]
        );
    }

    #[test]
    fn parse_roster_rejects_garbage() {
        assert!(parse_roster("k1\n").is_err(), "missing path must red");
        assert!(parse_roster("k1 path1 a,b extra\n").is_err(), "extra field must red");
        assert!(parse_roster("k1 path1\nk1 path2\n").is_err(), "a duplicate key must red");
        assert!(parse_roster("k1 path1 \n").is_ok(), "trailing whitespace is not a field");
        assert!(parse_roster("k1 pa,th1\n").is_err(), "a comma in the path is not a plain path");
        assert_eq!(
            parse_roster("k1 path1 \n").unwrap(),
            vec![("k1", "path1", vec![])]
        );
        assert!(
            parse_roster("k1 path1 a,,b\n").is_err(),
            "a doubled comma is an empty sibling element, not a valid tree name"
        );
        assert!(
            parse_roster("k1 path1 a,\n").is_err(),
            "a trailing comma is an empty sibling element"
        );
        assert!(
            parse_roster("k1 path1 ,a\n").is_err(),
            "a leading comma is an empty sibling element"
        );
    }

    #[test]
    fn excluded_entry_matches_only_the_three_names() {
        for name in ["target", ".git", "DESIGN.md"] {
            assert!(excluded_entry(OsStr::new(name)), "{name}");
        }
        for name in ["targets", ".gitignore", "design.md", "src"] {
            assert!(!excluded_entry(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn resolve_tree_refuses_escape_and_non_relative_paths() {
        let tmp = std::env::temp_dir().join(format!(
            "td-engine-local-source-resolve-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        fs::create_dir_all(root.join("app")).unwrap();
        let outside = tmp.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("escape")).unwrap();

        assert!(resolve_tree(&root, "app").is_ok());
        assert!(resolve_tree(&root, "").is_err());
        assert!(resolve_tree(&root, "missing").is_err());
        assert!(resolve_tree(&root, "../root/app").is_err());
        assert!(resolve_tree(&root, "/app").is_err());
        let err = resolve_tree(&root, "escape").unwrap_err();
        assert!(err.contains("resolves outside the checkout"), "{err}");
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn resolve_tree_refuses_a_symlink_that_resolves_to_the_root_itself() {
        let tmp = std::env::temp_dir().join(format!(
            "td-engine-local-source-resolve-root-loophole-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        fs::create_dir_all(&root).unwrap();
        // A declared path whose only component is a symlink back to the repo
        // root itself: `starts_with` alone would admit this (the root starts
        // with the root), silently turning a `local_source` declaration into
        // "the whole checkout".
        symlink(&root, root.join("whole-repo")).unwrap();

        let err = resolve_tree(&root, "whole-repo").unwrap_err();
        assert!(err.contains("repository root itself"), "{err}");
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn stage_without_siblings_copies_the_tree_itself_minus_exclusions() {
        let tmp = std::env::temp_dir().join(format!(
            "td-engine-local-source-stage-alone-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::create_dir_all(root.join("app/target")).unwrap();
        fs::create_dir_all(root.join("app/.git")).unwrap();
        fs::write(root.join("app/src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(root.join("app/DESIGN.md"), b"design").unwrap();
        fs::write(root.join("app/target/junk"), b"x").unwrap();
        fs::write(root.join("app/.git/HEAD"), b"ref\n").unwrap();

        let dest = tmp.join("staged");
        let staged = stage(&root, "app-source", "app", &[], &dest).unwrap();
        assert_eq!(staged, dest);
        assert!(dest.join("src/main.rs").is_file());
        assert!(!dest.join("target").exists());
        assert!(!dest.join(".git").exists());
        assert!(!dest.join("DESIGN.md").exists());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn stage_with_siblings_names_each_tree_by_its_own_basename() {
        let tmp = std::env::temp_dir().join(format!(
            "td-engine-local-source-stage-siblings-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("root");
        fs::create_dir_all(root.join("app/src")).unwrap();
        fs::create_dir_all(root.join("lib/src")).unwrap();
        fs::create_dir_all(root.join("other/lib")).unwrap();
        fs::write(root.join("app/src/main.rs"), b"fn main() {}\n").unwrap();
        fs::write(root.join("lib/src/lib.rs"), b"pub fn f() {}\n").unwrap();

        let dest = tmp.join("staged");
        let siblings = vec!["lib".to_string()];
        let staged = stage(&root, "app-source", "app", &siblings, &dest).unwrap();
        assert_eq!(staged, dest);
        assert!(dest.join("app/src/main.rs").is_file());
        assert!(dest.join("lib/src/lib.rs").is_file());

        // Two trees with the same basename are refused, and leave the
        // previous staging untouched.
        let listing_before: Vec<_> = fs::read_dir(&dest).unwrap().collect();
        let dup = vec!["lib".to_string(), "other/lib".to_string()];
        let err = stage(&root, "app-source", "app", &dup, &dest).unwrap_err();
        assert!(err.contains("two trees named `lib'"), "{err}");
        assert_eq!(
            fs::read_dir(&dest).unwrap().count(),
            listing_before.len(),
            "a refused roster must not touch the existing staging"
        );
        fs::remove_dir_all(&tmp).unwrap();
    }
}
