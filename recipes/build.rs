//! Generates the recipe registry from `src/recipes/*.rs` (github issue #295).
//!
//! Each recipe is one self-registering file: the FILE NAME (minus `.rs`) is the
//! catalog stem, and the file exports `pub fn recipe() -> Recipe`. This script
//! globs the directory and writes `$OUT_DIR/registry.rs` — the module
//! declarations plus the stem-sorted `all()` table — which `src/catalog.rs`
//! includes. Adding a recipe therefore touches only its new file: no Rust
//! source line is shared, the mk/gates/ "one file per entry" property, so
//! parallel recipe PRs don't collide on a central table.
//!
//! Deterministic by construction: the registry is sorted by stem, never
//! `read_dir` order. Pure `std` — the crate stays dependency-free.
//!
//! Also fingerprints the evaluator's OWN sources into
//! `TD_EVALUATOR_SOURCE_FINGERPRINT` for the check verdict key, so the key
//! names the logic that runs rather than whatever the tree holds when a check
//! starts (see `evaluator_source_fingerprint`).

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

// SHA-256 straight from the engine's source file: a build script cannot use
// the crate it builds for, and a build-dependency edge would compile the
// engine twice. The module is std-only and self-contained by its own contract.
#[path = "../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

// The two scans over this crate's sources, kept in the library so its tests
// cover them: a build script has no tests of its own.
#[path = "src/embed_scan.rs"]
mod embed_scan;
use embed_scan::{
    declares_out_of_line_module, has_embed_marker, strip_comments, td_dirs_embedded, td_dirs_named,
    td_dirs_named_beside_embeds, td_files_embedded,
};

fn main() -> Result<(), Box<dyn Error>> {
    // The directory path retriggers on file ADDS/REMOVES (dir mtime); an EDIT to
    // an existing file does NOT change the dir mtime, so each recipe file is
    // also declared below (bit us live in #378: a stale td-recipe-eval emitted a
    // recipe's OLD nativeInputs after an in-place edit).
    println!("cargo:rerun-if-changed=src/recipes");

    let manifest_dir = env::var("CARGO_MANIFEST_DIR")?;
    let recipes_dir = PathBuf::from(&manifest_dir).join("src/recipes");
    // (stem, module, named dirs) triples. The module name is the stem with '-'
    // mapped to '_', emitted as a raw identifier (`r#...`) so stems that
    // collide with Rust keywords (`true`, `move`, `loop`, ...) still compile.
    // The named dirs are the `td-*` directories the file spells, which is how
    // a recipe embeds crate sources (`include_str!("../../../td-sh/...")`).
    let mut recipes: Vec<(String, String, Vec<String>)> = Vec::new();
    for entry in fs::read_dir(&recipes_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().ok_or("non-UTF-8 filename in src/recipes")?;
        if entry.file_type()?.is_dir() {
            // A nested recipe would be silently absent from the catalog — and the
            // census manifest can't catch that (it is regenerated FROM the
            // catalog) — so directories are a hard error, never skipped.
            return Err(format!(
                "src/recipes/{name} is a directory — the recipe layout is FLAT, \
                 one <stem>.rs per recipe directly in src/recipes/"
            )
            .into());
        }
        if name.starts_with('.') {
            continue; // editor droppings (e.g. an emacs `.#foo.rs` lock link)
        }
        let Some(stem) = name.strip_suffix(".rs") else {
            continue;
        };
        // Per-file edit tracking (see the header note — the dir mtime misses edits).
        println!("cargo:rerun-if-changed=src/recipes/{name}");
        // The stem is the catalog key and doubles as a module name, so keep it
        // to the charset every existing key uses and reject a digit lead.
        let ok = !stem.is_empty()
            && stem
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !stem.starts_with(|c: char| c.is_ascii_digit());
        if !ok {
            return Err(format!(
                "src/recipes/{name}: stem must be [a-z0-9-]+, not digit-led \
                 (it becomes the catalog key and, with '-'->'_', a module name)"
            )
            .into());
        }
        let module = stem.replace('-', "_");
        if matches!(module.as_str(), "crate" | "self" | "super") {
            return Err(format!(
                "src/recipes/{name}: '{module}' cannot be a module name even as a \
                 raw identifier — rename the file"
            )
            .into());
        }
        let text = fs::read_to_string(entry.path())?;
        recipes.push((stem.to_string(), module, td_dirs_named(&text)));
    }
    recipes.sort();
    if recipes.is_empty() {
        return Err("src/recipes is empty — the catalog would be vacuous".into());
    }
    // The library's shared modules — everything under `src/` but the recipe
    // files and the evaluator's own `src/bin/` — embed crate sources too
    // (`lib.rs` places td-boot's protocol and td-profiler's contract by
    // `#[path]`), and any recipe may use one, so what they EMBED is every
    // recipe's. Only what they embed: a shared module multiplies into every
    // recipe, and `ladder.rs`'s doc comments cite the compositor sources
    // whose markers it duplicates, which would make every check a reader of
    // td-compositor. A recipe file keeps the wide rule, where a stray name
    // widens one recipe's reach and not all of them. The evaluator's own
    // sources embed crate files only in their test modules, which
    // `catalog::named_dirs_tests` pins. And a crate a shared module names in
    // code without embedding it is an error here, not a narrowed scope: it
    // is a read the embed scan would miss, or prose that belongs in a
    // comment.
    //
    // What a shared module embeds is kept by FILE, apart from the per-recipe
    // directories: a change elsewhere in td-compositor does not touch the one
    // timezone file every recipe compiles in. An embed in a `#[cfg(test)]`
    // module is not compiled into the evaluator at all and is left out. An
    // embedded file holding any embed marker or an out-of-line module reads
    // more than its own bytes, so it stands for its whole crate directory, as
    // does one that is not a file here, and so does a crate the module names
    // in production code outside an embed literal it resolved.
    let repo = PathBuf::from(&manifest_dir);
    let repo = repo
        .parent()
        .ok_or("recipes crate has no parent directory")?;
    let mut crate_dirs: Vec<String> = Vec::new();
    for entry in fs::read_dir(repo)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("td-") && entry.path().join("Cargo.toml").is_file() {
            crate_dirs.push(name);
        }
    }
    let mut shared_files: Vec<String> = Vec::new();
    let src = PathBuf::from(&manifest_dir).join("src");
    let mut pending = vec![src.clone()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type()?.is_dir() {
                if dir == src && (name == "recipes" || name == "bin") {
                    continue;
                }
                pending.push(path);
            } else if name.to_string_lossy().ends_with(".rs") {
                let text = fs::read_to_string(&path)?;
                let embedded = td_dirs_embedded(&text);
                for named in td_dirs_named(&strip_comments(&text)) {
                    if crate_dirs.contains(&named) && !embedded.contains(&named) {
                        return Err(format!(
                            "{}: names `{named}/` outside a comment without embedding it; a \
                             shared module reads a crate only through a `#[path]` or \
                             `include_*!` literal, which is what the recipe-checks scope sees",
                            path.display()
                        )
                        .into());
                    }
                }
                let module_dir = path
                    .parent()
                    .and_then(|p| p.strip_prefix(repo).ok())
                    .and_then(Path::to_str)
                    .ok_or("a shared module outside the repository or with a non-UTF-8 path")?;
                for file in td_files_embedded(module_dir, &text) {
                    let entry = shared_file_entry(repo, &file)?;
                    if !shared_files.contains(&entry) {
                        shared_files.push(entry);
                    }
                }
                for dir in td_dirs_named_beside_embeds(&text) {
                    if crate_dirs.contains(&dir) && !shared_files.contains(&dir) {
                        shared_files.push(dir);
                    }
                }
            }
        }
    }
    shared_files.sort();

    let mut out = String::new();
    writeln!(
        out,
        "// @generated by build.rs from src/recipes/*.rs — do not edit."
    )?;
    for (stem, module, _) in &recipes {
        let path = format!("{manifest_dir}/src/recipes/{stem}.rs");
        writeln!(out, "#[path = {path:?}]")?;
        writeln!(out, "pub mod r#{module};")?;
    }
    writeln!(
        out,
        "pub fn all() -> Vec<(&'static str, crate::types::Recipe)> {{"
    )?;
    writeln!(out, "    vec![")?;
    for (stem, module, _) in &recipes {
        writeln!(out, "        ({stem:?}, r#{module}::recipe()),")?;
    }
    writeln!(out, "    ]")?;
    writeln!(out, "}}")?;
    writeln!(
        out,
        "/// The `td-*` directories each recipe's own file names. Sorted."
    )?;
    writeln!(
        out,
        "pub fn named_dirs() -> &'static [(&'static str, &'static [&'static str])] {{"
    )?;
    writeln!(out, "    &[")?;
    for (stem, _, dirs) in &recipes {
        let list: Vec<String> = dirs.iter().map(|d| format!("{d:?}")).collect();
        writeln!(out, "        ({stem:?}, &[{}]),", list.join(", "))?;
    }
    writeln!(out, "    ]")?;
    writeln!(out, "}}")?;
    writeln!(
        out,
        "/// What the shared modules compile in from `td-*` crates, which every"
    )?;
    writeln!(
        out,
        "/// recipe reads: repository-relative files, or a whole crate directory. Sorted."
    )?;
    writeln!(out, "pub fn shared_embeds() -> &'static [&'static str] {{")?;
    let list: Vec<String> = shared_files.iter().map(|f| format!("{f:?}")).collect();
    writeln!(out, "    &[{}]", list.join(", "))?;
    writeln!(out, "}}")?;

    let (fingerprint, fingerprinted) =
        evaluator_source_fingerprint(Path::new(&manifest_dir), &shared_files)?;
    writeln!(
        out,
        "/// The repository-relative files `TD_EVALUATOR_SOURCE_FINGERPRINT` covers."
    )?;
    writeln!(out, "#[cfg(test)]")?;
    writeln!(
        out,
        "pub fn evaluator_fingerprint_files() -> &'static [&'static str] {{"
    )?;
    let list: Vec<String> = fingerprinted.iter().map(|f| format!("{f:?}")).collect();
    writeln!(out, "    &[{}]", list.join(", "))?;
    writeln!(out, "}}")?;

    let out_path = PathBuf::from(env::var("OUT_DIR")?).join("registry.rs");
    fs::write(&out_path, out)?;

    println!("cargo:rustc-env=TD_EVALUATOR_SOURCE_FINGERPRINT={fingerprint}");
    Ok(())
}

/// The `shared_embeds` entry for `file`, a repository-relative path a shared
/// module embeds: the file itself where it is a file here whose text holds
/// no embed marker, literal or not, and declares no out-of-line module, and
/// its `td-*` crate directory otherwise. Watched, so an edit that adds such
/// a read re-runs this script.
fn shared_file_entry(repo: &Path, file: &str) -> Result<String, Box<dyn Error>> {
    let top = file.split('/').next().unwrap_or(file).to_string();
    let path = repo.join(file);
    if !path.is_file() {
        return Ok(top);
    }
    println!("cargo:rerun-if-changed={}", path.display());
    // Bytes that are not text (an `include_bytes!` payload) read nothing
    // further; any text may be Rust an `include!` compiles.
    let Ok(text) = String::from_utf8(fs::read(&path)?) else {
        return Ok(file.to_string());
    };
    let code = strip_comments(&text);
    if has_embed_marker(&code) || declares_out_of_line_module(&code) {
        return Ok(top);
    }
    Ok(file.to_string())
}

/// sha256 over the evaluator's own sources — everything under this crate's
/// `src/`, its `build.rs` and manifest, the engine's `src/` and manifest, the
/// workspace manifest and lock, the builder preparation implementation and
/// its script entry point, and the crate files the shared modules compile in
/// (a whole-directory entry by its `src/`) — as (path, file digest) pairs in
/// path order, returned with those paths. The check verdict key holds it in
/// place of reading those trees at run time: a key read from the tree names
/// the tree at that moment, and a check whose assertions were compiled from
/// older sources could record a pass under a newer key. Every file is
/// declared to cargo, and every directory for its adds and removes, so an
/// edit reruns this script and re-keys. A hidden entry — an editor's swap
/// file, its lock link, a scratch directory — is skipped, file or directory
/// alike; any other symlink is an error, since the walk does not follow one
/// and skipping it would fingerprint less than the compiler reads.
fn evaluator_source_fingerprint(
    manifest_dir: &Path,
    shared_embeds: &[String],
) -> Result<(String, Vec<String>), Box<dyn Error>> {
    let root = manifest_dir
        .parent()
        .ok_or("recipes crate has no parent directory")?;
    let mut files: Vec<PathBuf> = [
        "recipes/build.rs",
        "recipes/Cargo.toml",
        "engine/Cargo.toml",
        "Cargo.toml",
        "Cargo.lock",
        "tests/recipe-eval-tool.sh",
        "builder/src/stage0.rs",
        // Mounted into the evaluator by path (src/bin/td-recipe-eval.rs):
        // the screen oracles draw the compositor's chrome text with them.
        "td-ui/src/atlas.rs",
        "td-ui/src/coverage.rs",
        "td-ui/src/face.rs",
        "td-ui/src/face_file.rs",
        "td-ui/src/sfnt.rs",
        "td-compositor/src/font.rs",
        "td-compositor/src/font_data.rs",
    ]
    .iter()
    .map(|rel| root.join(rel))
    .collect();
    for dir in ["recipes/src", "engine/src"] {
        walk_sources(&root.join(dir), &mut files)?;
    }
    // An entry is a file path, or a bare crate directory whose sources an
    // embedded file reads further (`shared_file_entry`).
    for entry in shared_embeds {
        if entry.contains('/') {
            files.push(root.join(entry));
        } else {
            walk_sources(&root.join(entry).join("src"), &mut files)?;
        }
    }
    files.sort();
    files.dedup();
    let mut h = sha256::Sha256::new();
    let mut listed = Vec::with_capacity(files.len());
    for file in &files {
        let rel = file
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF-8 path under the evaluator sources")?;
        println!("cargo:rerun-if-changed={}", file.display());
        let digest = sha256::sha256_file(file)
            .map_err(|e| format!("fingerprint {}: {e}", file.display()))?;
        h.update(rel.as_bytes());
        h.update(b"\0");
        h.update(digest.as_bytes());
        h.update(b"\n");
        listed.push(rel.to_string());
    }
    Ok((sha256::to_base16(&h.finalize()), listed))
}

fn walk_sources(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in fs::read_dir(dir).map_err(|e| format!("list {}: {e}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        // Hidden before symlink: an emacs lock is a hidden symlink.
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(format!("{}: symlink under the evaluator sources", path.display()).into());
        }
        if kind.is_dir() {
            walk_sources(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
