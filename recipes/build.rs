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

use std::collections::{BTreeMap, BTreeSet};
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
    declares_out_of_line_module, embed_literals, has_computed_embed, has_embed_marker,
    normalize_join, production_code, sibling_reads, strip_comments, td_dirs_embedded,
    td_dirs_named, td_dirs_named_beside_embeds, td_files_embedded,
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
    let mut recipe_texts: BTreeMap<String, String> = BTreeMap::new();
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
        recipe_texts.insert(stem.to_string(), text);
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

    let evaluator = evaluator_reads(repo, &recipes)?;
    let (file_readers, wide_readers) =
        recipe_file_readers(repo, &recipes, &recipe_texts, &evaluator)?;
    writeln!(
        out,
        "/// Each repository file a recipe's evaluation reads directly, with the"
    )?;
    writeln!(
        out,
        "/// recipes that read it: a recipe file, read by its own recipe and the"
    )?;
    writeln!(
        out,
        "/// recipes whose code names its module, or a file a recipe embeds."
    )?;
    writeln!(
        out,
        "pub fn recipe_file_readers() -> &'static [(&'static str, &'static [&'static str])] {{"
    )?;
    writeln!(out, "    &[")?;
    for (path, stems) in &file_readers {
        let stems: Vec<String> = stems.iter().map(|s| format!("{s:?}")).collect();
        writeln!(out, "        ({path:?}, &[{}]),", stems.join(", "))?;
    }
    writeln!(out, "    ]")?;
    writeln!(out, "}}")?;
    writeln!(
        out,
        "/// The recipes whose code may read any recipe file (a glob, a group, a"
    )?;
    writeln!(out, "/// catalog lookup by name). Sorted.")?;
    writeln!(
        out,
        "pub fn recipe_wide_readers() -> &'static [&'static str] {{"
    )?;
    let list: Vec<String> = wide_readers.iter().map(|s| format!("{s:?}")).collect();
    writeln!(out, "    &[{}]", list.join(", "))?;
    writeln!(out, "}}")?;
    writeln!(
        out,
        "/// The files under `recipes/` the crate's shared modules and evaluator"
    )?;
    writeln!(
        out,
        "/// read in production code, which reach every check. Sorted."
    )?;
    writeln!(
        out,
        "pub fn recipe_evaluator_reads() -> &'static [&'static str] {{"
    )?;
    let list: Vec<String> = evaluator.files.iter().map(|s| format!("{s:?}")).collect();
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

/// Who reads each file a recipe's evaluation reads under `recipes/`, and
/// which recipes may read any recipe file. A recipe file is read by its own
/// recipe and by every recipe whose production code names its module
/// (`embed_scan::sibling_reads`, a `catalog` alias of one included), since
/// that recipe's derivation can change with it; a file a recipe embeds
/// under `RECIPE_DIR`, `recipes/src/fixtures`, `recipes/src/probes` or
/// `recipes/locks` is read by that recipe. A recipe whose code reads in a
/// way this cannot follow (a glob, an alias, a computed embed path) may
/// read any. Anything else under `recipes/` — the shared modules, the
/// evaluator — is in no entry, which the check reach takes as reaching
/// every check; so is a file the crate's other sources read
/// (`EvaluatorReads`, emitted as `recipe_evaluator_reads`), though its
/// entry stays here for the module readers the reach follows through it.
type FileReaders = BTreeMap<String, BTreeSet<String>>;

/// Where a file a recipe embeds may lie and still key only that recipe.
const RECIPE_EMBED_DIRS: &[&str] = &[
    RECIPE_DIR,
    "recipes/src/fixtures/",
    "recipes/src/probes/",
    "recipes/locks/",
];

fn recipe_file_readers(
    repo: &Path,
    recipes: &[(String, String, Vec<String>)],
    texts: &BTreeMap<String, String>,
    evaluator: &EvaluatorReads,
) -> Result<(FileReaders, Vec<String>), Box<dyn Error>> {
    let mut modules: Vec<&str> = recipes.iter().map(|(_, m, _)| m.as_str()).collect();
    modules.extend(evaluator.aliases.keys().map(String::as_str));
    let mut readers: FileReaders = BTreeMap::new();
    let mut wide: Vec<String> = Vec::new();
    let mut read = |path: String, stem: &str| {
        readers.entry(path).or_default().insert(stem.to_string());
    };
    for (stem, module, _) in recipes {
        let text = texts
            .get(stem)
            .ok_or_else(|| format!("no text read for src/recipes/{stem}.rs"))?;
        let own = format!("{RECIPE_DIR}{stem}.rs");
        read(own.clone(), stem);
        // The recipe file, then each Rust file it mounts under `recipes/`,
        // transitively: a mounted module's code is the recipe's.
        let mut pending: Vec<(String, String)> = vec![(own.clone(), text.clone())];
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut any_wide = false;
        while let Some((path, text)) = pending.pop() {
            if !seen.insert(path.clone()) {
                continue;
            }
            let (reads, any) = sibling_reads(&text, &modules);
            let code = production_code(&text);
            any_wide |= any || has_computed_embed(&code);
            for name in &reads {
                let target = evaluator.aliases.get(name).unwrap_or(name);
                if target == module {
                    continue;
                }
                let (read_stem, _, _) = recipes
                    .iter()
                    .find(|(_, m, _)| m == target)
                    .ok_or_else(|| format!("{stem}: no recipe for module {target}"))?;
                read(format!("{RECIPE_DIR}{read_stem}.rs"), stem);
            }
            let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
            for (_, literal) in embed_literals(&code) {
                let Some(embedded) = normalize_join(dir, &literal) else {
                    continue;
                };
                if !RECIPE_EMBED_DIRS.iter().any(|d| embedded.starts_with(d)) || embedded == own {
                    continue;
                }
                if embedded.ends_with(".rs") {
                    match fs::read_to_string(repo.join(&embedded)) {
                        Ok(text) => pending.push((embedded.clone(), text)),
                        // A module this cannot read is one it cannot follow.
                        Err(_) => any_wide = true,
                    }
                }
                read(embedded, stem);
            }
        }
        if any_wide {
            wide.push(stem.clone());
        }
    }
    Ok((readers, wide))
}

/// Where the recipe files live, repository-relative.
const RECIPE_DIR: &str = "recipes/src/recipes/";

/// What the crate's sources other than the recipe files (the shared
/// modules, the evaluator) read of the recipes, in production code.
struct EvaluatorReads {
    /// Files they read under `recipes/`: a recipe module named after
    /// `registry::`, a recipe looked up by a literal name, and a file they
    /// embed. Such a file can change what any check asserts, so it keys and
    /// reaches every check.
    files: BTreeSet<String>,
    /// A recipe module re-exported under another name (`pub use
    /// registry::x as y;`), by that name, with the module it is.
    aliases: BTreeMap<String, String>,
}

/// Watched through `walk_sources`.
fn evaluator_reads(
    repo: &Path,
    recipes: &[(String, String, Vec<String>)],
) -> Result<EvaluatorReads, Box<dyn Error>> {
    let mut sources = Vec::new();
    walk_sources(&repo.join("recipes/src"), &mut sources)?;
    let mut files = BTreeSet::new();
    let mut aliases = BTreeMap::new();
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let ident = |s: &str| {
        let s = s.strip_prefix("r#").unwrap_or(s);
        s.get(..s.find(|c: char| !ident_char(c)).unwrap_or(s.len()))
            .unwrap_or("")
            .to_string()
    };
    for source in sources {
        let rel = source
            .strip_prefix(repo)?
            .to_str()
            .ok_or("non-UTF-8 path under recipes/src")?
            .to_string();
        if rel.starts_with(RECIPE_DIR) || !rel.ends_with(".rs") {
            continue;
        }
        let code = production_code(&fs::read_to_string(&source)?);
        for (prefix, literal) in [("registry::", false), ("lookup(\"", true)] {
            let mut from = 0usize;
            while let Some(i) = code.get(from..).and_then(|r| r.find(prefix)) {
                from = from.saturating_add(i).saturating_add(prefix.len());
                let rest = code.get(from..).unwrap_or("").trim_start();
                // A group (`registry::{x, y as z}`) names each of its items.
                let items: Vec<&str> = match rest.strip_prefix('{') {
                    Some(group) if !literal => group
                        .get(..group.find('}').unwrap_or(group.len()))
                        .unwrap_or("")
                        .split(',')
                        .map(str::trim)
                        .collect(),
                    _ => vec![rest],
                };
                for item in items {
                    let name = if literal {
                        item.get(..item.find('"').unwrap_or(0))
                            .unwrap_or("")
                            .to_string()
                    } else {
                        ident(item)
                    };
                    let hit = recipes.iter().find(|(stem, module, _)| {
                        if literal {
                            *stem == name
                        } else {
                            *module == name
                        }
                    });
                    let Some((stem, module, _)) = hit else {
                        continue;
                    };
                    files.insert(format!("{RECIPE_DIR}{stem}.rs"));
                    let item = item.strip_prefix("r#").unwrap_or(item);
                    let after = item.get(name.len()..).unwrap_or("").trim_start();
                    if let Some(alias) = after.strip_prefix("as ").filter(|_| !literal) {
                        aliases.insert(ident(alias.trim_start()), module.clone());
                    }
                }
            }
        }
        let dir = rel.rsplit_once('/').map_or("", |(d, _)| d);
        for (_, literal) in embed_literals(&code) {
            if let Some(path) = normalize_join(dir, &literal) {
                if path.starts_with("recipes/") {
                    files.insert(path);
                }
            }
        }
    }
    Ok(EvaluatorReads { files, aliases })
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
