//! The pinned bootstrap root (AGENTS.md, "Target artifact graph").
//!
//! Default builds start at the gcc-14 cut: the ladder rungs below it are a
//! reference-closed set of locally built store items that
//! `seed/bootstrap-root.txt` pins by input-addressed basename, NAR hash and
//! references. Those items keep the paths the ladder built them at, because
//! their bytes embed those paths; so unlike a fetched seed they cannot be
//! content-addressed, and their authority is this compiled table instead.
//!
//! The root db is separate from the keyed seed db: its rows are not
//! self-addressing, and `authenticate_seed_db` would rightly refuse them.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use td_engine::bootstrap_root::{Item, Root};

use crate::{
    commit_tree_checked, lock_store_commit, merge_regs, nar, sandbox, scan, scan_candidate_index,
    store, store_db_read, write_atomic, OutputReg,
};

const MANIFEST: &str = include_str!("../../seed/bootstrap-root.txt");

/// The compiled manifest, parsed once.
pub(crate) fn root() -> Result<&'static Root, String> {
    static ROOT: OnceLock<Result<Root, String>> = OnceLock::new();
    ROOT.get_or_init(|| Root::parse(MANIFEST))
        .as_ref()
        .map_err(Clone::clone)
}

/// The basename the compiled root pins for an export key, if it is one.
pub(crate) fn export(key: &str) -> Result<Option<&'static str>, String> {
    Ok(root()?.export(key))
}

fn canonical(base: &str) -> String {
    format!("{}/{base}", store::store_dir())
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// NAR-hash `physical` and scan it for references among `candidates`
/// (canonical store paths). Returns (hash, size, sorted referenced basenames).
fn scan_item(physical: &Path, candidates: &[String]) -> Result<(String, u64, Vec<String>), String> {
    let mut scanner = scan::Scanner::new(candidates).map_err(|e| e.to_string())?;
    nar::write_nar(&mut scanner, physical)
        .map_err(|e| format!("hash {}: {e}", physical.display()))?;
    let (hash, size, refs) = scanner.finish();
    let mut bases: Vec<String> = refs.iter().map(|r| basename(r).to_string()).collect();
    bases.sort();
    Ok((hash, size, bases))
}

fn sorted(refs: &[String]) -> Vec<String> {
    let mut refs = refs.to_vec();
    refs.sort();
    refs
}

/// The first of `dirs` holding `base`.
fn locate(dirs: &[PathBuf], base: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|d| d.join(base))
        .find(|p| p.symlink_metadata().is_ok())
}

/// `bootstrap-root verify`: the same authentication, uncached, for the
/// evaluator to decide whether an admitted root needs re-admitting.
/// Under the seed store's commit lock, so it never observes a peer's repair
/// half done.
pub(crate) fn verify(dbp: &str, items_dir: &Path) -> Result<(), String> {
    let _lock = lock_store_commit(Path::new(dbp))?;
    authenticate_db_with(root()?, dbp, items_dir)
}

/// AUTHENTICATE a plan's root db against the compiled root.
pub(crate) fn authenticate_db(dbp: &str, items_dir: &Path) -> Result<(), String> {
    static DONE: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let done = DONE.get_or_init(|| Mutex::new(HashSet::new()));
    let key = (dbp.to_string(), items_dir.display().to_string());
    if done.lock().is_ok_and(|set| set.contains(&key)) {
        return Ok(());
    }
    authenticate_db_with(root()?, dbp, items_dir)?;
    if let Ok(mut set) = done.lock() {
        set.insert(key);
    }
    Ok(())
}

/// The rows must be exactly the root's items, each with the pinned hash and
/// references, and each item's bytes under `items_dir` must hash to the pin.
/// A pinned root is all-or-nothing, so unlike the seed dbs a missing row is
/// an error rather than a step that reds later.
fn authenticate_db_with(root: &Root, dbp: &str, items_dir: &Path) -> Result<(), String> {
    let fail = |why: String| format!("bootstrap root db {dbp}: provenance rejected: {why}");
    let data = std::fs::read(dbp).map_err(|e| format!("read bootstrap root db {dbp}: {e}"))?;
    let db = store_db_read::Db::open(data).map_err(fail)?;
    let hashes = db.hashes_by_path().map_err(fail)?;
    let refs = db.refs_by_path().map_err(fail)?;
    for path in hashes.keys() {
        if root.item(basename(path)).is_none() || *path != canonical(basename(path)) {
            return Err(fail(format!(
                "`{path}' is not an item seed/bootstrap-root.txt pins"
            )));
        }
    }
    for item in &root.items {
        let path = canonical(&item.base);
        let hash = hashes
            .get(&path)
            .ok_or_else(|| fail(format!("pinned item `{}' has no row", item.base)))?;
        if *hash != item.nar {
            return Err(fail(format!(
                "`{}' is registered as {hash}, pinned as {}",
                item.base, item.nar
            )));
        }
        let got: Vec<String> = sorted(
            &refs
                .get(&path)
                .map(|r| {
                    r.iter()
                        .map(|p| basename(p).to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        if got != sorted(&item.refs) {
            return Err(fail(format!(
                "`{}' is registered with references {got:?}, pinned as {:?}",
                item.base, item.refs
            )));
        }
        let have = sandbox::nar_hash_of(&items_dir.join(&item.base))
            .map_err(|e| fail(format!("hash {}: {e}", item.base)))?;
        if have != item.nar {
            return Err(fail(format!(
                "`{}' on disk hashes {have}, pinned as {}",
                item.base, item.nar
            )));
        }
    }
    Ok(())
}

/// Admit the compiled root: find every pinned item in `src_dirs` (the build
/// cache holding the ladder's outputs, then the seed store, where an item
/// the cache no longer holds was admitted before), verify its bytes and
/// references, commit it into `seed_store`
/// at its basename, and write `root_db` registering exactly the root.
pub(crate) fn admit(src_dirs: &[PathBuf], seed_store: &Path, root_db: &Path) -> Result<(), String> {
    admit_with(root()?, src_dirs, seed_store, root_db)
}

fn admit_with(
    root: &Root,
    src_dirs: &[PathBuf],
    seed_store: &Path,
    root_db: &Path,
) -> Result<(), String> {
    if !root.is_pinned() {
        return Err("seed/bootstrap-root.txt is unpinned; there is nothing to admit".to_string());
    }
    let candidates: Vec<String> = root.items.iter().map(|i| canonical(&i.base)).collect();
    let mut found: Vec<(PathBuf, OutputReg)> = Vec::with_capacity(root.items.len());
    let mut mismatches: Vec<String> = Vec::new();
    for item in &root.items {
        let Some(src) = locate(src_dirs, &item.base) else {
            mismatches.push(format!("{}: not built", item.base));
            continue;
        };
        let (hash, size, refs) = scan_item(&src, &candidates)?;
        if hash != item.nar || refs != sorted(&item.refs) {
            mismatches.push(format!(
                "{}: built {hash} refs {refs:?}, pinned {} refs {:?}",
                item.base, item.nar, item.refs
            ));
            continue;
        }
        found.push((
            src,
            OutputReg {
                store_path: canonical(&item.base),
                nar_hash: hash,
                nar_size: size,
                refs: refs.iter().map(|r| canonical(r)).collect(),
                deriver: String::new(),
            },
        ));
    }
    if !mismatches.is_empty() {
        return Err(format!(
            "the ladder does not reproduce seed/bootstrap-root.txt ({} of {} items differ):\n  \
             {}\nA builder change since the pin can do this. `td-recipe-eval bootstrap-root \
             check` reports the drift, `bootstrap-root pin` re-pins, and \
             TD_BOOTSTRAP_FROM_STAGE0=1 builds from stage0 meanwhile.",
            mismatches.len(),
            root.items.len(),
            mismatches.join("\n  ")
        ));
    }
    std::fs::create_dir_all(seed_store)
        .map_err(|e| format!("mkdir {}: {e}", seed_store.display()))?;
    // The root db sits beside the keyed seed dbs, so this is the seed
    // store's own commit lock.
    let _lock = lock_store_commit(root_db)?;
    let db = root_db
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 root db path {}", root_db.display()))?;
    // A db that authenticates under the lock was admitted by a peer. One
    // that does not is repaired in place: every source above was just
    // checked against the pin, so a mismatching tree is replaced and the db
    // rewritten atomically, never unlinked under a reader.
    if root_db.is_file() && authenticate_db_with(root, db, seed_store).is_ok() {
        return Ok(());
    }
    // Copied, never hardlinked: the build cache's trees are trusted by reuse
    // without a re-hash, so they must share no inode with anything staged.
    for (src, reg) in &found {
        let dest = seed_store.join(basename(&reg.store_path));
        commit_tree_checked(src, &dest, &reg.nar_hash, false, false)?;
    }
    let regs: Vec<OutputReg> = found.into_iter().map(|(_, reg)| reg).collect();
    let bytes = merge_regs(None, &regs)?;
    write_atomic(root_db, &bytes)
}

/// Measure the reference closure of `exports` (canonical store paths) by
/// content-scanning their bytes, found in `src_dirs`, against every item
/// those dirs hold: the `item` rows `bootstrap-root pin` writes.
///
/// SEED_DIRS are reference candidates but never sources: a root is only
/// ladder outputs, so a reference into the seed store fails as absent
/// rather than going undetected.
pub(crate) fn measure(
    src_dirs: &[PathBuf],
    seed_dirs: &[PathBuf],
    exports: &[String],
) -> Result<Vec<Item>, String> {
    let dirs: Vec<String> = src_dirs.iter().map(|d| d.display().to_string()).collect();
    let scanned: Vec<String> = src_dirs
        .iter()
        .chain(seed_dirs)
        .map(|d| d.display().to_string())
        .collect();
    let (candidates, _) = scan_candidate_index(&scanned, &store::store_dir())?;
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    let mut todo: Vec<String> = exports.iter().map(|p| basename(p).to_string()).collect();
    while let Some(base) = todo.pop() {
        if items.contains_key(&base) {
            continue;
        }
        let physical = locate(src_dirs, &base)
            .ok_or_else(|| format!("{base}: not present in {}", dirs.join(", ")))?;
        let (nar, _size, refs) = scan_item(&physical, &candidates)?;
        todo.extend(refs.iter().filter(|r| !items.contains_key(*r)).cloned());
        items.insert(base.clone(), Item { base, nar, refs });
    }
    Ok(items.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LADDER: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("td-broot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two items, `tool` referencing `lib` by its canonical path, measured
    /// into a root exporting `tool`.
    fn fixture(dir: &Path) -> (Root, String, String) {
        let lib = "0123456789abcdfghijklmnpqrsvwxyz-lib-1".to_string();
        let tool = "0123456789abcdfghijklmnpqrsvwxyy-tool-1".to_string();
        std::fs::create_dir_all(dir.join(&lib)).unwrap();
        std::fs::write(dir.join(&lib).join("libx.so"), b"library bytes").unwrap();
        std::fs::create_dir_all(dir.join(&tool)).unwrap();
        std::fs::write(
            dir.join(&tool).join("tool"),
            format!("#! uses {}/libx.so", canonical(&lib)),
        )
        .unwrap();
        let items = measure(&[dir.to_path_buf()], &[], &[canonical(&tool)]).unwrap();
        let root = Root {
            builder_abi: "4".to_string(),
            ladder: LADDER.to_string(),
            exports: vec![("tool".to_string(), tool.clone())],
            items,
        };
        (Root::parse(&root.render()).unwrap(), lib, tool)
    }

    #[test]
    fn a_reference_into_the_seed_store_is_absent_not_unseen() {
        let base = tmp("seedref");
        let (cache, seeds) = (base.join("cache"), base.join("seeds"));
        let (_root, lib, tool) = fixture(&cache);
        std::fs::create_dir_all(&seeds).unwrap();
        std::fs::rename(cache.join(&lib), seeds.join(&lib)).unwrap();
        let err = measure(&[cache.clone()], &[seeds], &[canonical(&tool)]).unwrap_err();
        assert!(err.contains(&lib) && err.contains("not present"), "{err}");
        let _ = crate::sandbox::remove_scratch_tree(&base);
    }

    #[test]
    fn the_compiled_root_never_shares_the_compiled_abi() {
        // Cut and full graphs would otherwise share output paths under
        // different reuse keys (see store::BUILDER_ABI).
        let root = root().unwrap();
        if root.is_pinned() {
            assert_ne!(root.builder_abi, store::BUILDER_ABI.to_string());
        }
    }

    #[test]
    fn measure_follows_references_and_admit_registers_exactly_the_root() {
        let base = tmp("admit");
        let (root, lib, tool) = fixture(&base.join("cache"));
        assert_eq!(root.items.len(), 2);
        assert_eq!(root.item(&tool).unwrap().refs, vec![lib.clone()]);
        let seed = base.join("seed");
        let db = base.join("root.db");
        admit_with(&root, &[base.join("cache")], &seed, &db).unwrap();
        authenticate_db_with(&root, db.to_str().unwrap(), &seed).unwrap();
        // Idempotent over an already-admitted store.
        admit_with(&root, &[base.join("cache")], &seed, &db).unwrap();
        let _ = crate::sandbox::remove_scratch_tree(&base);
    }

    #[test]
    fn admit_refuses_bytes_the_pin_does_not_describe() {
        let base = tmp("drift");
        let (root, lib, _tool) = fixture(&base.join("cache"));
        std::fs::write(base.join("cache").join(&lib).join("libx.so"), b"rebuilt").unwrap();
        let err = admit_with(
            &root,
            &[base.join("cache")],
            &base.join("seed"),
            &base.join("db"),
        )
        .unwrap_err();
        assert!(
            err.contains("does not reproduce") && err.contains(&lib),
            "{err}"
        );
        assert!(!base.join("db").exists());
        let _ = crate::sandbox::remove_scratch_tree(&base);
    }

    #[test]
    fn authentication_refuses_tampered_bytes_and_foreign_rows() {
        let base = tmp("auth");
        let (root, lib, _tool) = fixture(&base.join("cache"));
        let seed = base.join("seed");
        let db = base.join("root.db");
        admit_with(&root, &[base.join("cache")], &seed, &db).unwrap();
        let dbs = db.to_str().unwrap();

        // A root that pins one more item than the db registers.
        let mut more = root.clone();
        more.items.push(Item {
            base: "0123456789abcdfghijklmnpqrsvwxzz-extra-1".to_string(),
            nar: root.items[0].nar.clone(),
            refs: Vec::new(),
        });
        assert!(authenticate_db_with(&more, dbs, &seed)
            .unwrap_err()
            .contains("has no row"));

        // A db row the root does not pin.
        let mut fewer = root.clone();
        fewer.items.retain(|i| i.base == lib);
        fewer.exports = vec![("lib".to_string(), lib.clone())];
        assert!(authenticate_db_with(&fewer, dbs, &seed)
            .unwrap_err()
            .contains("is not an item"));

        // Bytes changed after admission.
        use std::os::unix::fs::PermissionsExt;
        let file = seed.join(&lib).join("libx.so");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&file, b"tampered").unwrap();
        assert!(authenticate_db_with(&root, dbs, &seed)
            .unwrap_err()
            .contains("on disk hashes"));

        // Re-admission over the present db repairs the rotted item in place.
        admit_with(&root, &[base.join("cache")], &seed, &db).unwrap();
        authenticate_db_with(&root, dbs, &seed).unwrap();
        // And a missing item, with the db still present.
        crate::sandbox::remove_scratch_tree(&seed.join(&lib)).unwrap();
        admit_with(&root, &[base.join("cache")], &seed, &db).unwrap();
        authenticate_db_with(&root, dbs, &seed).unwrap();
        let _ = crate::sandbox::remove_scratch_tree(&base);
    }
}
