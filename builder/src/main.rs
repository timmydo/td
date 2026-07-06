//! td-builder — td's own builder (DESIGN §7.1 side-track).
//!
//! Goal of the track: a td-owned Rust binary that executes a `.drv` in a
//! user-namespace sandbox and registers the output, proven behaviorally
//! equivalent to the pinned `guix-daemon` (prime directive 4 — the daemon is
//! the oracle; never replace without a differential).
//!
//! Grown rung by rung, each with its own daemon differential:
//!   • S1 — toolchain probe: the bare invocation prints a stable sentinel the
//!     `td-builder` rung greps (proves the COMPILED BINARY ran — stronger than
//!     "cargo build exited 0");
//!   • S2 — `nar-hash PATH`: NAR serializer + SHA-256, bit-for-bit equal to
//!     the daemon's recorded hash (the rung's S2 leg diffs them);
//!   • S3 — an ATerm `.drv` parser + a userns build sandbox + store
//!     registration;
//!   • S4 — the daemon-vs-td-builder store differential, as a check.sh rung.

mod affected;
mod bootstrap;
mod build;
mod build_daemon;
mod check_loop;
mod daemon;
mod drv;
mod elf;
// The comment-splice static guard (#300) is exercised only by its own `#[test]`
// (the cargo-test tier) — gate it to test builds so it adds no dead-code surface
// to the release binary or the clippy pass.
#[cfg(test)]
mod gate_lint;
mod gate_bodies;
mod gate_inputs;
mod gate_timing;
mod gates;
mod json;
mod lock;
mod nar;
mod oci;
mod sandbox;
mod scan;
mod sha256;
mod store;
mod store_db;
mod store_db_read;
mod sys;
mod toolchain_x86_64;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Adapter: stream Write into the hasher.
struct HashWriter(sha256::Sha256);

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nar_hash_path(path: &Path) -> Result<String, std::io::Error> {
    let mut w = HashWriter(sha256::Sha256::new());
    nar::write_nar(&mut w, path)?;
    Ok(format!("sha256:{}", sha256::to_base16(&w.0.finalize())))
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nar_hash(path: &str) -> Result<String, std::io::Error> {
    nar_hash_path(Path::new(path))
}

/// Adapter: hash AND count the NAR bytes in one serialization pass (the seed
/// manifest needs both the NAR hash and the NAR size — the daemon's `narSize`).
struct HashSizeWriter {
    hasher: sha256::Sha256,
    size: u64,
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
impl std::io::Write for HashSizeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        self.size += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The (NAR hash, NAR size) of a path — one serialization pass.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nar_hash_size_path(path: &Path) -> Result<(String, u64), std::io::Error> {
    let mut w = HashSizeWriter { hasher: sha256::Sha256::new(), size: 0 };
    nar::write_nar(&mut w, path)?;
    Ok((format!("sha256:{}", sha256::to_base16(&w.hasher.finalize())), w.size))
}

// --- substitute server: export half (store-coupled, dependency-free) ---
// Write a serve-able directory for a store closure: a td-native `<basename>.narinfo` per
// member + `nar/<narhash-hex>.nar`. This is the dual of `seed-manifest`/`seed-unpack` —
// the seed pair captures a closure into ONE tarball + manifest; the substitute export
// serves each path on its OWN, addressable by basename, so a consumer can fetch just the
// paths it lacks. The networked `subst/` binary signs + serves this dir and the consumer
// verifies + restores it (with `nar-restore`); this half stays in the dependency-free
// engine because it needs the store DB + NAR serializer, not crypto/HTTP. Same reader +
// `write_nar` as seed-manifest, so the served bytes match the daemon's.

/// One member of a substitute export.
struct SubstMember {
    store_path: String,           // logical path, e.g. /gnu/store/<hash>-name
    physical: std::path::PathBuf, // where to read it on disk (== store_path on live store)
    refs: Vec<String>,            // direct references (logical store paths)
}

/// The basename (`<hash>-name`) of a store path.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn store_basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Render a td-native narinfo (minimal, line-oriented). References are recorded as
/// basenames so the record is store-location independent; the consumer rebases them onto
/// its own store dir. The signature line (`Sig:`) is appended later by the signer.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn narinfo_text(
    store_path: &str,
    narhash: &str,
    narsize: u64,
    narfile: &str,
    ref_basenames: &[String],
) -> String {
    format!(
        "StorePath: {store_path}\nNarHash: {narhash}\nNarSize: {narsize}\nNarFile: {narfile}\nReferences: {}\n",
        ref_basenames.join(" ")
    )
}

/// Write a serve-able substitute directory for MEMBERS into OUTDIR. Returns the basenames
/// written. Each member yields `OUTDIR/<basename>.narinfo` + `OUTDIR/nar/<narhash>.nar`.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn subst_export(outdir: &Path, members: &[SubstMember]) -> std::io::Result<Vec<String>> {
    let nardir = outdir.join("nar");
    std::fs::create_dir_all(&nardir)?;
    let mut written = Vec::new();
    for m in members {
        let (narhash, narsize) = nar_hash_size_path(&m.physical)?;
        let hex = narhash.strip_prefix("sha256:").unwrap_or(&narhash);
        let narfile = format!("nar/{hex}.nar");
        let mut f = std::fs::File::create(nardir.join(format!("{hex}.nar")))?;
        nar::write_nar(&mut f, &m.physical)?;
        drop(f);
        let base = store_basename(&m.store_path);
        let refbases: Vec<String> =
            m.refs.iter().map(|r| store_basename(r).to_string()).collect();
        let text = narinfo_text(&m.store_path, &narhash, narsize, &narfile, &refbases);
        std::fs::write(outdir.join(format!("{base}.narinfo")), text)?;
        written.push(base.to_string());
    }
    Ok(written)
}

/// Build the `SubstMember` list to export for ROOTS — paths + their direct refs read from DB,
/// each member's bytes taken from `STORE_DIR/<basename>`. With `walk_closure`, ROOTS expands to
/// its full closure over DB's Refs graph (a whole-closure mirror). Without, EXACTLY the roots
/// are exported — the per-output granularity the substitute consumer uses: `try_substitute`
/// fetches a drv's own outputs one at a time (their deps assumed already present), so a
/// publisher of a single build output need not stage that output's whole closure into STORE_DIR
/// (its external refs live elsewhere). The narinfo still lists each path's refs as basenames
/// either way, so the consumer can scan-verify the restored bytes.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn subst_export_members(
    db: &store_db_read::Db,
    store_dir: &str,
    roots: &[String],
    walk_closure: bool,
) -> Result<Vec<SubstMember>, String> {
    let mut paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in roots {
        // db.closure errors if the root is not in the DB; in paths-only mode we keep that
        // existence check but take only the root itself (not its refs).
        let c = db.closure(r)?;
        if walk_closure {
            for p in c {
                paths.insert(p);
            }
        } else {
            paths.insert(r.clone());
        }
    }
    let refs = db.refs_by_path()?;
    Ok(paths
        .iter()
        .map(|p| {
            let mut rs = refs.get(p).cloned().unwrap_or_default();
            rs.sort();
            rs.dedup();
            let base = p.rsplit('/').next().unwrap_or(p);
            SubstMember {
                store_path: p.clone(),
                physical: Path::new(store_dir).join(base),
                refs: rs,
            }
        })
        .collect())
}

/// The fixed logical store path under which the guix-less-runner harness ships as a single
/// whole-tree substitute (issue #314). Unlike the lock-keyed toolchain CLOSURES (whose name a
/// consumer recomputes from the lock), the /td/store harness — the busybox+make set, the staged
/// C toolchain, the /td/store/ld loader, and the `rel`/`toolchain` metadata — is a
/// content-addressed BUILD OUTPUT with no derivable lock name, so it ships as ONE nar of the
/// whole `.td-build-cache/harness` tree under this fixed name. Integrity is the signed NarHash;
/// trust is the pinned ed25519 key (tests/td-subst.pub). A forger without the private key cannot
/// mint a valid `td-harness.narinfo`, so the worst a store-writer can do is a signed DOWNGRADE
/// to an older td-published harness — acceptable for an optimization the consumer fails CLOSED on.
const HARNESS_SUBST_STORE_PATH: &str = "/td/store/td-harness";

/// Export the harness tree at `harness_dir` (the `.td-build-cache/harness` layout: `store/` +
/// `rel` + `toolchain`) as a single substitute: one nar of the WHOLE tree + a `td-harness.narinfo`
/// (StorePath == `HARNESS_SUBST_STORE_PATH`, no References), written under `outdir`. No store DB is
/// needed — the harness is content-addressed by its NarHash, not by a lock. The daily signs the
/// narinfo (tools/publish-harness-subst.sh) and the guix-less runner fetches+verifies+restores it
/// (tools/resolve-harness.sh). Returns the written basenames (exactly one: `td-harness`).
fn harness_subst_export(outdir: &Path, harness_dir: &Path) -> Result<Vec<String>, String> {
    if !harness_dir.join("store").is_dir() || !harness_dir.join("rel").is_file() {
        return Err(format!(
            "{} is not a harness tree (expected store/ + rel)",
            harness_dir.display()
        ));
    }
    let member = SubstMember {
        store_path: HARNESS_SUBST_STORE_PATH.to_string(),
        physical: harness_dir.to_path_buf(),
        refs: Vec::new(),
    };
    subst_export(outdir, std::slice::from_ref(&member)).map_err(|e| e.to_string())
}

/// The `path` column (index 1) of a read `ValidPaths` row, or "" if absent.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn path_at(cols: &[store_db_read::Value]) -> &str {
    match cols.get(1) {
        Some(store_db_read::Value::Text(p)) => p,
        _ => "",
    }
}

/// Recreate the tree at `src` under `dst` as a canonical store entry — the
/// daemon's addToStore canonicalization, for the properties NAR (hence the
/// content-addressed store path) actually captures: the tree STRUCTURE, file
/// CONTENTS, the file EXECUTABLE bit, and SYMLINK targets. NAR omits directory
/// permissions, the read/write permission bits, and mtimes, so those are not
/// reproduced (dirs are left writable so the scratch copy can be cleaned up);
/// regular files get the canonical `0555`/`0444` by their source exec bit, which
/// is the one perm NAR encodes. Mirrors `(guix serialization) write-file`.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn copy_canonical(src: &Path, dst: &Path) -> Result<(), String> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let md = std::fs::symlink_metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let ft = md.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src).map_err(|e| format!("{}: {e}", src.display()))?;
        symlink(&target, dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    } else if ft.is_dir() {
        std::fs::create_dir(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        for entry in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_canonical(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        // Regular file: copy contents and set the canonical mode by the source's
        // executable bit (the only permission NAR distinguishes). Key off OWNER-exec
        // (`0o100`) — exactly what the daemon's canonicaliser (S_IXUSR) and td's own
        // NAR serializer (`nar.rs`) use, so the restored tree's NAR matches the source's.
        let content = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
        std::fs::write(dst, &content).map_err(|e| format!("{}: {e}", dst.display()))?;
        let exec = md.permissions().mode() & 0o100 != 0;
        let mode = if exec { 0o555 } else { 0o444 };
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    Ok(())
}

/// Resolve `guix` on the current PATH to its real (symlink-followed) location
/// and return the directory holding it — the bin dir check.sh prepends to PATH
/// (under the exposed /gnu/store). None if `guix` is not on PATH.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn host_guix_bin_dir() -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':').filter(|s| !s.is_empty()) {
        let cand = Path::new(dir).join("guix");
        if cand.is_file() {
            let real = std::fs::canonicalize(&cand).ok()?;
            return real.parent().map(|p| p.to_string_lossy().into_owned());
        }
    }
    None
}

/// One built output's post-build registration facts — the daemon's per-path
/// record (the `build`/`realize` NAR scan computes these).
#[derive(Clone)]
struct OutputReg {
    store_path: String,
    nar_hash: String,
    nar_size: u64,
    refs: Vec<String>,
    deriver: String,
}

/// Write a td store-db (the daemon's `ValidPaths`/`Refs` authority, via the
/// zero-dep `store_db` writer) registering the just-built OUTPUTS — td OWNS the
/// store record of its own build, not just a text file. Each output is fully
/// registered (path/hash/registrationTime/deriver/narSize) with ids 1..N; its
/// references resolve to another output's id or to a scaffolding `ValidPaths` row
/// (path only) — the same shape `store-add-referenced` writes. registrationTime is
/// a fixed sentinel (excluded from the daemon differential, as in `store-register`).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn write_output_db(regs: &[OutputReg], out_db: &Path) -> Result<(), String> {
    use std::collections::BTreeMap;
    use store_db::{Table, Value};
    let out_id: BTreeMap<&str, i64> = regs
        .iter()
        .enumerate()
        .map(|(i, r)| (r.store_path.as_str(), i as i64 + 1))
        .collect();
    // External references (not themselves outputs) get ids after the outputs, in
    // first-seen order — stable, so the db is deterministic.
    let mut ext_order: Vec<String> = Vec::new();
    let mut ext_id: BTreeMap<String, i64> = BTreeMap::new();
    let mut next = regs.len() as i64 + 1;
    for r in regs {
        for rf in &r.refs {
            if !out_id.contains_key(rf.as_str()) && !ext_id.contains_key(rf) {
                ext_id.insert(rf.clone(), next);
                ext_order.push(rf.clone());
                next += 1;
            }
        }
    }
    let id_of = |p: &str| -> i64 {
        *out_id
            .get(p)
            .or_else(|| ext_id.get(p))
            .expect("reference id assigned above")
    };
    let mut valid: Vec<(i64, Vec<Value>)> = Vec::new();
    for (i, r) in regs.iter().enumerate() {
        valid.push((
            i as i64 + 1,
            vec![
                Value::Null, // id (integer primary key) — rowid is the id
                Value::Text(r.store_path.clone()),
                Value::Text(r.nar_hash.clone()),
                Value::Int(1), // registrationTime (sentinel; excluded from diffs)
                Value::Text(r.deriver.clone()),
                Value::Int(r.nar_size as i64),
            ],
        ));
    }
    for p in &ext_order {
        valid.push((
            ext_id[p],
            vec![
                Value::Null,
                Value::Text(p.clone()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ));
    }
    let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut rid = 1i64;
    for (i, r) in regs.iter().enumerate() {
        for rf in &r.refs {
            ref_rows.push((rid, vec![Value::Int(i as i64 + 1), Value::Int(id_of(rf))]));
            rid += 1;
        }
    }
    let tables = [
        Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: valid,
        },
        Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: ref_rows,
        },
    ];
    std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
    Ok(())
}

/// MERGE built outputs into a PERSISTENT store DB — the accumulating dual of
/// `write_output_db`'s clobber. Given the EXISTING db bytes (None for the first
/// commit) and the NEW outputs, union their `ValidPaths` + `Refs` into one db and
/// return the serialized bytes. This is what makes a td store *persistent*: a build
/// adds its result to a store that already holds prior builds' results, instead of
/// every build writing a fresh single-output db. The daemon's accumulating
/// `ValidPaths`/`Refs` authority across SEPARATE builds, in pure Rust.
///
/// The store PATH is the identity:
///   - re-committing the same output is IDEMPOTENT (one row; the bytes are
///     byte-deterministic, so a re-merge of the same set reproduces them exactly);
///   - a path first seen only as another output's *reference* is a SCAFFOLD row
///     (path, no hash) and is UPGRADED in place to a full row when a later commit
///     registers it for real;
///   - rowids are assigned in sorted-path order, so the db is deterministic
///     regardless of commit order.
/// Mirrors `store-gc-sweep`'s renumber-and-remap-Refs rewrite (its additive dual):
/// reads with the td reader, writes with the td writer, no daemon, no sqlite engine.
/// Scope is the GC/closure authority — `ValidPaths` + `Refs`; a persistent commit DB
/// does not carry `DerivationOutputs` (as `store-gc-sweep`'s swept DB does not: the
/// drv→output mapping is rebuilt by registration, not by accumulation).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn merge_regs(existing: Option<&[u8]>, new_regs: &[OutputReg]) -> Result<Vec<u8>, String> {
    use std::collections::{BTreeMap, BTreeSet};
    use store_db::{Table, Value as WV};
    use store_db_read::{Db, Value as RV};
    // One accumulated record per path: full fields when known, else scaffold (None).
    struct Rec {
        hash: Option<String>,
        deriver: Option<String>,
        size: Option<u64>,
        refs: BTreeSet<String>,
    }
    let mut recs: BTreeMap<String, Rec> = BTreeMap::new();
    fn ensure<'a>(recs: &'a mut BTreeMap<String, Rec>, p: &str) -> &'a mut Rec {
        recs.entry(p.to_string()).or_insert_with(|| Rec {
            hash: None,
            deriver: None,
            size: None,
            refs: BTreeSet::new(),
        })
    }
    // 1) Fold in the existing db (if this is not the first commit).
    if let Some(bytes) = existing {
        let db = Db::open(bytes.to_vec())?;
        let mut path_of: BTreeMap<i64, String> = BTreeMap::new();
        for (rowid, cols) in db.table("ValidPaths")? {
            let path = match cols.get(1) {
                Some(RV::Text(p)) => p.clone(),
                _ => continue,
            };
            path_of.insert(rowid, path.clone());
            let r = ensure(&mut recs, &path);
            if let Some(RV::Text(h)) = cols.get(2) {
                if !h.is_empty() {
                    r.hash = Some(h.clone());
                }
            }
            if let Some(RV::Text(d)) = cols.get(4) {
                if !d.is_empty() {
                    r.deriver = Some(d.clone());
                }
            }
            if let Some(RV::Int(s)) = cols.get(5) {
                r.size = Some(*s as u64);
            }
        }
        for (_rid, cols) in db.table("Refs")? {
            if let (Some(RV::Int(a)), Some(RV::Int(b))) = (cols.first(), cols.get(1)) {
                if let (Some(ap), Some(bp)) = (path_of.get(a), path_of.get(b)) {
                    let (ap, bp) = (ap.clone(), bp.clone());
                    ensure(&mut recs, &bp); // a referenced path is at least a scaffold
                    ensure(&mut recs, &ap).refs.insert(bp);
                }
            }
        }
    }
    // 2) Union the new outputs — each a full row; its refs are at least scaffolds.
    for reg in new_regs {
        {
            let r = ensure(&mut recs, &reg.store_path);
            r.hash = Some(reg.nar_hash.clone());
            r.deriver = Some(reg.deriver.clone());
            r.size = Some(reg.nar_size);
        }
        for rf in &reg.refs {
            ensure(&mut recs, rf);
            ensure(&mut recs, &reg.store_path).refs.insert(rf.clone());
        }
    }
    // 3) Assign rowids in sorted-path order (BTreeMap iterates sorted → deterministic).
    // `id_of` resolves a reference's TARGET path to its rowid; a row's OWN id is just
    // its (sorted) position, so the loop uses the enumerate index for that directly.
    let id_of: BTreeMap<&str, i64> = recs
        .keys()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i as i64 + 1))
        .collect();
    let mut valid: Vec<(i64, Vec<WV>)> = Vec::with_capacity(recs.len());
    let mut ref_rows: Vec<(i64, Vec<WV>)> = Vec::new();
    let mut rid = 1i64;
    for (i, (p, r)) in recs.iter().enumerate() {
        let myid = i as i64 + 1;
        // registrationTime is the same fixed sentinel write_output_db uses (excluded
        // from the daemon differential); a scaffold (no hash) keeps it NULL too.
        let (regtime, deriver, size) = match &r.hash {
            Some(_) => (
                WV::Int(1),
                r.deriver.clone().map(WV::Text).unwrap_or(WV::Null),
                r.size.map(|s| WV::Int(s as i64)).unwrap_or(WV::Null),
            ),
            None => (WV::Null, WV::Null, WV::Null),
        };
        valid.push((
            myid,
            vec![
                WV::Null, // id (integer primary key) — rowid is the id
                WV::Text(p.clone()),
                r.hash.clone().map(WV::Text).unwrap_or(WV::Null),
                regtime,
                deriver,
                size,
            ],
        ));
        for rf in &r.refs {
            ref_rows.push((rid, vec![WV::Int(myid), WV::Int(id_of[rf.as_str()])]));
            rid += 1;
        }
    }
    let tables = [
        Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: valid,
        },
        Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: ref_rows,
        },
    ];
    Ok(store_db::write_db(&tables))
}

/// Read-modify-write `merge_regs` against an on-disk persistent DB: load DEST-DB if
/// it exists (a missing file = the first commit), union the NEW outputs in, write it
/// back. The store dir's bytes are interned by the caller (`store-commit`).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn merge_output_db(dest_db: &Path, new_regs: &[OutputReg]) -> Result<(), String> {
    let existing = match std::fs::read(dest_db) {
        Ok(b) => Some(b),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read {}: {e}", dest_db.display())),
    };
    let bytes = merge_regs(existing.as_deref(), new_regs)?;
    std::fs::write(dest_db, bytes).map_err(|e| format!("write {}: {e}", dest_db.display()))
}

/// Execute DRV in a userns sandbox against CLOSURE (the staged input store paths,
/// one per line) and write a registration record — `path` / `nar-hash` /
/// `nar-size` / `reference`* / `deriver` per output — to SCRATCH/registration,
/// printing `OUT=<name> <path>` per output. The reference candidates are the
/// closure plus the drv's own outputs (self-references), the daemon's candidate
/// shape. Returns the per-output registration facts (for `realize` to write a td
/// store-db). Shared by `build` (CLOSURE handed in as a file) and `realize`
/// (CLOSURE computed by td itself from the store DB's Refs graph).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_and_register(
    drv_path: &str,
    closure: &[String],
    scratch: &Path,
) -> Result<Vec<OutputReg>, String> {
    let bytes = std::fs::read(drv_path).map_err(|e| e.to_string())?;
    let parsed = drv::parse(&bytes).map_err(|e| e.to_string())?;
    // The deriver recorded is the .drv's OWN store path. For a store-path input
    // that is drv_path; for an emitted .drv handed in from outside the store,
    // compute its content-addressed store path so the registration matches the
    // daemon's recorded deriver.
    let deriver = if drv_path.starts_with(store::store_dir().as_str()) {
        drv_path.to_string()
    } else {
        let out0 = parsed
            .outputs
            .first()
            .ok_or_else(|| "derivation has no outputs".to_string())?;
        let drv_name = format!(
            "{}.drv",
            store::name_from_store_path(&out0.path)
                .ok_or_else(|| "output is not a store path".to_string())?
        );
        let mut refs: Vec<String> = parsed.input_drvs.iter().map(|(p, _)| p.clone()).collect();
        refs.extend(parsed.input_srcs.iter().cloned());
        store::drv_store_path(&drv_name, &bytes, &refs)
    };
    let outputs =
        sandbox::build(&parsed, drv_path, closure, scratch).map_err(|e| e.to_string())?;
    // Reference candidates: the staged closure plus the drv's own outputs
    // (self-references), the daemon's candidate shape. A closure entry may carry an
    // on-disk override (`CANONICAL\tON-DISK`); reference scanning matches the
    // CANONICAL store paths, so take the canonical half.
    let mut candidates: Vec<String> = closure
        .iter()
        .map(|e| sandbox::split_closure_entry(e).0.to_string())
        .collect();
    candidates.extend(parsed.outputs.iter().map(|o| o.path.clone()));
    let mut regs: Vec<OutputReg> = Vec::new();
    for (name, host) in &outputs {
        let store_path = &parsed
            .outputs
            .iter()
            .find(|o| &o.name == name)
            .expect("output came from this drv")
            .path;
        let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, host).map_err(|e| e.to_string())?;
        let (hash, size, refs) = scanner.finish();
        println!("OUT={name} {store_path}");
        regs.push(OutputReg {
            store_path: store_path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs,
            deriver: deriver.clone(),
        });
    }
    std::fs::write(scratch.join("registration"), registration_text(&regs))
        .map_err(|e| e.to_string())?;
    Ok(regs)
}

/// Serialize per-output registration records into a SCRATCH/registration blob — the
/// inverse of `parse_registration_blocks` (`parse(registration_text(regs)) == regs`).
/// One `path`/`nar-hash`/`nar-size`/`reference`*/`deriver` block per output, blank-line
/// separated. Written by `build_and_register` after a real build and by a
/// persistent-store read-back (so a fresh scratch that reused a prior build's output
/// still carries the same registration a real build would have).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn registration_text(regs: &[OutputReg]) -> String {
    let mut record = String::new();
    for r in regs {
        record.push_str(&format!("path {}\n", r.store_path));
        record.push_str(&format!("nar-hash {}\n", r.nar_hash));
        record.push_str(&format!("nar-size {}\n", r.nar_size));
        for rf in &r.refs {
            record.push_str(&format!("reference {rf}\n"));
        }
        record.push_str(&format!("deriver {}\n\n", r.deriver));
    }
    record
}

/// Intern a finished build SCRATCH (its `registration` + `newstore/<base>` trees) into a
/// PERSISTENT store and MERGE its registration into the accumulating DB — the build-into
/// half of an incremental store. Idempotent (a content path already present is a no-op).
/// Shared by the `store-commit` subcommand and build-recipe's persistent-store build-into.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn commit_scratch_to_store(scratch: &Path, store_dir: &str, db: &Path) -> Result<Vec<String>, String> {
    let reg = std::fs::read_to_string(scratch.join("registration")).map_err(|e| {
        format!("read {}/registration: {e} (build into this scratch first)", scratch.display())
    })?;
    let regs = parse_registration_blocks(&reg);
    if regs.is_empty() {
        return Err("no outputs in the registration to commit".to_string());
    }
    std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
    let newstore = scratch.join("newstore");
    let mut committed = Vec::with_capacity(regs.len());
    for r in &regs {
        let base = r
            .store_path
            .rsplit('/')
            .next()
            .filter(|_| store::name_from_store_path(&r.store_path).is_some())
            .ok_or_else(|| format!("output {} is not a store path", r.store_path))?;
        let src = newstore.join(base);
        if !src.exists() {
            return Err(format!("output tree missing under {}", src.display()));
        }
        let dest = Path::new(store_dir).join(base);
        // The store path is content-addressed, so an entry already present is the same
        // bytes — committing is idempotent (skip the copy).
        if !dest.exists() {
            copy_canonical(&src, &dest)?;
        }
        committed.push(r.store_path.clone());
    }
    merge_output_db(db, &regs)?;
    Ok(committed)
}

/// Persistent-store build cache — like `cached_realization`, but keyed on a PERSISTENT
/// store (dir + accumulating DB) that survives ACROSS invocations (the incremental
/// /td/store). If EVERY output of PARSED is a full valid path in PERSIST_DB whose tree
/// under PERSIST_STORE re-serializes to the recorded NAR hash, a PRIOR invocation already
/// built it: stage each output tree into SCRATCH/newstore, and return the read-back regs
/// (the caller writes SCRATCH/registration + td.db from them) — so the build is SKIPPED.
/// Any missing/mismatched output ⇒ None (rebuild), and any tree staged so far is unwound.
/// The daemon's valid-path skip, sourced across process boundaries from an on-disk store.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn persistent_realization(
    parsed: &drv::Derivation,
    persist_store: &str,
    persist_db: &Path,
    scratch: &Path,
) -> Result<Option<Vec<OutputReg>>, String> {
    use store_db_read::{Db, Value as RV};
    let bytes = match std::fs::read(persist_db) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {}: {e}", persist_db.display())),
    };
    let db = Db::open(bytes)?;
    // Fully-registered paths (hash present) → (hash, narSize, deriver).
    let mut full: std::collections::HashMap<String, (String, u64, String)> =
        std::collections::HashMap::new();
    for (_rid, cols) in db.table("ValidPaths")? {
        if let (Some(RV::Text(p)), Some(RV::Text(h))) = (cols.get(1), cols.get(2)) {
            if h.is_empty() {
                continue;
            }
            let size = match cols.get(5) {
                Some(RV::Int(s)) => *s as u64,
                _ => 0,
            };
            let deriver = match cols.get(4) {
                Some(RV::Text(d)) => d.clone(),
                _ => String::new(),
            };
            full.insert(p.clone(), (h.clone(), size, deriver));
        }
    }
    let refs_map = db.refs_by_path()?;
    let newstore = scratch.join("newstore");
    std::fs::create_dir_all(&newstore).map_err(|e| e.to_string())?;
    let mut out: Vec<OutputReg> = Vec::with_capacity(parsed.outputs.len());
    let mut staged: Vec<std::path::PathBuf> = Vec::new();
    // A partial hit (some outputs found, then a miss) must not leave half the outputs
    // staged in newstore (the rebuild would build ON them) — unwind before returning None.
    fn unwind(staged: &[std::path::PathBuf]) {
        for d in staged {
            let _ = std::fs::remove_dir_all(d);
        }
    }
    for o in &parsed.outputs {
        let (hash, size, deriver) = match full.get(&o.path) {
            Some(x) => x.clone(),
            None => {
                unwind(&staged);
                return Ok(None);
            }
        };
        let base = match o.path.rsplit('/').next() {
            Some(b) => b,
            None => {
                unwind(&staged);
                return Ok(None);
            }
        };
        let src = Path::new(persist_store).join(base);
        if !src.exists() {
            unwind(&staged);
            return Ok(None);
        }
        let refs: Vec<String> = refs_map.get(&o.path).cloned().unwrap_or_default();
        // Integrity: the persistent tree must re-serialize to the recorded hash — a
        // corrupt/partial persistent entry is a MISS (rebuild), never trusted.
        let mut scanner = scan::Scanner::new(&refs).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, &src).map_err(|e| e.to_string())?;
        let (got, _, _) = scanner.finish();
        if got != hash {
            unwind(&staged);
            return Ok(None);
        }
        let dest = newstore.join(base);
        if dest.exists() {
            let _ = std::fs::remove_dir_all(&dest);
        }
        copy_canonical(&src, &dest)?;
        staged.push(dest);
        out.push(OutputReg {
            store_path: o.path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs,
            deriver,
        });
    }
    Ok(Some(out))
}

/// Parse a SCRATCH/registration blob into per-output records. The blob is the
/// `path`/`nar-hash`/`nar-size`/`reference`*/`deriver` blocks `build_and_register`
/// writes — one block per output, a `path ` line opening each. Order is preserved.
/// Shared by `cached_realization` (the build cache) and `store-commit` (interning a
/// finished build into the persistent store), so both read the registration the same way.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn parse_registration_blocks(text: &str) -> Vec<OutputReg> {
    let mut recs: Vec<OutputReg> = Vec::new();
    let mut cur: Option<OutputReg> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("path ") {
            if let Some(r) = cur.take() {
                recs.push(r);
            }
            cur = Some(OutputReg {
                store_path: p.to_string(),
                nar_hash: String::new(),
                nar_size: 0,
                refs: Vec::new(),
                deriver: String::new(),
            });
        } else if let (Some(r), Some(h)) = (cur.as_mut(), line.strip_prefix("nar-hash ")) {
            r.nar_hash = h.to_string();
        } else if let (Some(r), Some(s)) = (cur.as_mut(), line.strip_prefix("nar-size ")) {
            r.nar_size = s.parse().unwrap_or(0);
        } else if let (Some(r), Some(rf)) = (cur.as_mut(), line.strip_prefix("reference ")) {
            r.refs.push(rf.to_string());
        } else if let (Some(r), Some(d)) = (cur.as_mut(), line.strip_prefix("deriver ")) {
            r.deriver = d.to_string();
        }
    }
    if let Some(r) = cur {
        recs.push(r);
    }
    recs
}

/// Content-addressed build cache. The assembled `.drv` path is DETERMINISTIC (its
/// hash covers the inputs + builder + env), so if every output of PARSED is already
/// present under SCRATCH/newstore AND recorded in SCRATCH/registration with a NAR
/// hash that RE-VERIFIES, the build was already done — same drv ⇒ same result, the
/// guix-daemon valid-path skip. Returns the recorded outputs to reuse, or None to
/// (re)build. Re-hashing the cached output (cheap vs a rebuild) guards a corrupted /
/// partially-deleted entry. This is consulted ONLY by `build-recipe`; the
/// reproducibility `check` is a separate command that force-rebuilds, so reuse here
/// never weakens the repro proof.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn cached_realization(
    parsed: &drv::Derivation,
    scratch: &Path,
) -> Result<Option<Vec<OutputReg>>, String> {
    let reg = match std::fs::read_to_string(scratch.join("registration")) {
        Ok(s) => s,
        Err(_) => return Ok(None), // never built here
    };
    let recs: std::collections::HashMap<String, OutputReg> = parse_registration_blocks(&reg)
        .into_iter()
        .map(|r| (r.store_path.clone(), r))
        .collect();

    let newstore = scratch.join("newstore");
    let mut out: Vec<OutputReg> = Vec::with_capacity(parsed.outputs.len());
    for o in &parsed.outputs {
        let rec = match recs.get(&o.path) {
            Some(r) if !r.nar_hash.is_empty() => r.clone(),
            _ => return Ok(None),
        };
        let base = o.path.rsplit('/').next().unwrap_or("");
        let physical = newstore.join(base);
        if !physical.exists() {
            return Ok(None);
        }
        let mut scanner = scan::Scanner::new(&rec.refs).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, &physical).map_err(|e| e.to_string())?;
        let (hash, _, _) = scanner.finish();
        if hash != rec.nar_hash {
            return Ok(None); // corrupt/partial cache entry — rebuild
        }
        out.push(rec);
    }
    Ok(Some(out))
}

/// Read a `Key: value` field from a td-native narinfo body.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn narinfo_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(": ")))
}

/// Restore a SUBSTITUTE output from a fetched (already signature-verified) narinfo + NAR:
/// unpack the NAR into NEWSTORE/<base> (nar::read_nar), then re-serialize it and require
/// the NAR hash to equal the narinfo's NarHash. That equality is the DURABLE leg — a
/// substitute is only accepted if the bytes it restores to are the bytes the publisher
/// signed (and, since td builds are reproducible, those are the bytes a local build would
/// produce). Returns the output's registration record (refs detected by the same scanner
/// build_and_register uses, so the store-db registration is identical to a real build's).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn restore_substitute(
    narinfo: &str,
    narfile: &Path,
    output_path: &str,
    newstore: &Path,
    deriver: &str,
) -> Result<OutputReg, String> {
    // The narinfo signature attests only that the publisher signed THIS StorePath — not
    // that it is the output we asked for. A validly-signed narinfo for some OTHER path,
    // served under this output's basename, must NOT be accepted as this output (it would
    // register another path's bytes as our derivation's result). Bind the signed StorePath
    // to the requested output_path before trusting any of its bytes.
    let signed_path = narinfo_field(narinfo, "StorePath").ok_or("narinfo: no StorePath")?;
    if signed_path != output_path {
        return Err(format!(
            "substitute StorePath does not match the requested output\n  want {output_path}\n  got  {signed_path}"
        ));
    }
    let want_hash = narinfo_field(narinfo, "NarHash").ok_or("narinfo: no NarHash")?;
    // References are recorded as basenames; rebase onto the active store dir for scanning.
    let store_dir = store::store_dir();
    let full_refs: Vec<String> = narinfo_field(narinfo, "References")
        .unwrap_or("")
        .split_whitespace()
        .map(|b| format!("{store_dir}/{b}"))
        .collect();
    let base = output_path.rsplit('/').next().unwrap_or(output_path);
    let dest = newstore.join(base);
    let _ = std::fs::remove_dir_all(&dest);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Restore + verify inside a closure so that ANY failure — a NAR parse/write error part
    // way through `read_nar`, or a NarHash that does not match the signed one — removes the
    // partially-written tree before returning. A half-restored output left under newstore
    // would otherwise contaminate the build fallback (which writes its fresh outputs there)
    // or a later restore attempt.
    let restored = (|| -> Result<(String, u64, Vec<String>), String> {
        let mut r = std::io::BufReader::new(
            std::fs::File::open(narfile).map_err(|e| format!("open {}: {e}", narfile.display()))?,
        );
        nar::read_nar(&mut r, &dest)
            .map_err(|e| format!("restore nar -> {}: {e}", dest.display()))?;
        // Re-serialize the restored tree exactly as build_and_register does (scanner over the
        // reference candidates), and require the hash to match what the publisher signed.
        let mut scanner = scan::Scanner::new(&full_refs).map_err(|e| e.to_string())?;
        nar::write_nar(&mut scanner, &dest).map_err(|e| e.to_string())?;
        let (hash, size, refs) = scanner.finish();
        if hash != want_hash {
            return Err(format!(
                "restored substitute NAR hash != signed NarHash for {output_path}\n  want {want_hash}\n  got  {hash}"
            ));
        }
        Ok((hash, size, refs))
    })();
    let (hash, size, refs) = match restored {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e);
        }
    };
    Ok(OutputReg {
        store_path: output_path.to_string(),
        nar_hash: hash,
        nar_size: size,
        refs,
        deriver: deriver.to_string(),
    })
}

/// SUBSTITUTE-OR-BUILD: before realizing DRV, try to fetch every output from a configured
/// substitute server instead of building it. Returns Some(regs) only if EVERY output is
/// fetched + signature-verified + restores to its signed NarHash; otherwise None (→ build).
///
/// OFF unless `TD_SUBST_URL` is set — the verification loop never sets it, so the loop's
/// behavior is unchanged (directive 1: the loop always builds from source + --check). It
/// is opt-in for `td install` / CI image prep / a cold worktree. td-builder is
/// dependency-free, so the network + ed25519 work is shelled out to the `td-subst` binary
/// (`TD_SUBST_BIN`, default `td-subst`); td-builder only restores (nar::read_nar) + verifies
/// the hash + registers.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn try_substitute(
    parsed: &drv::Derivation,
    drv_path: &str,
    scratch: &Path,
) -> Result<Option<Vec<OutputReg>>, String> {
    let url = match std::env::var("TD_SUBST_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => return Ok(None),
    };
    let pubkey = std::env::var("TD_SUBST_PUBKEY")
        .map_err(|_| "TD_SUBST_URL is set but TD_SUBST_PUBKEY is not".to_string())?;
    let subst_bin = std::env::var("TD_SUBST_BIN").unwrap_or_else(|_| "td-subst".to_string());
    let newstore = scratch.join("newstore");
    std::fs::create_dir_all(&newstore).map_err(|e| e.to_string())?;
    let fetchdir = scratch.join("subst-fetch");
    let _ = std::fs::remove_dir_all(&fetchdir);
    std::fs::create_dir_all(&fetchdir).map_err(|e| e.to_string())?;

    // Substitution is ALL-OR-NOTHING across the drv's outputs: a later output that misses or
    // fails to verify must leave NO restored tree behind, because the build fallback writes its
    // fresh outputs into the SAME newstore (a multi-output drv would otherwise build on top of a
    // half-substituted sibling). Track every base we restore so we can roll the whole set back.
    let mut restored_bases: Vec<String> = Vec::new();
    let rollback = |bases: &[String]| {
        for b in bases {
            let _ = std::fs::remove_dir_all(newstore.join(b));
        }
    };

    let mut record = String::new();
    let mut regs: Vec<OutputReg> = Vec::new();
    for o in &parsed.outputs {
        let base = o.path.rsplit('/').next().unwrap_or(&o.path);
        // Shell out: td-subst fetch URL NAME OUTDIR PUBKEY — verifies the signature +
        // NarHash and writes <base>.narinfo + the nar into fetchdir, or exits non-zero.
        let out = std::process::Command::new(&subst_bin)
            .args(["fetch", &url, base, &fetchdir.to_string_lossy(), &pubkey])
            .output()
            .map_err(|e| format!("spawn {subst_bin}: {e}"))?;
        if !out.status.success() {
            // Not available / failed verification → fall back to building (NOT an error).
            eprintln!(
                "td-builder: no verified substitute for {base} ({}); building",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            rollback(&restored_bases);
            return Ok(None);
        }
        let narinfo = match std::fs::read_to_string(fetchdir.join(format!("{base}.narinfo"))) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("td-builder: substitute narinfo for {base} unreadable ({e}); building");
                rollback(&restored_bases);
                return Ok(None);
            }
        };
        let narfile = match narinfo_field(&narinfo, "NarFile") {
            Some(f) => f,
            None => {
                eprintln!("td-builder: substitute narinfo for {base} has no NarFile; building");
                rollback(&restored_bases);
                return Ok(None);
            }
        };
        // A restore failure (StorePath mismatch, NAR parse error, or NarHash != the signed one)
        // means this substitute is not trustworthy — but that is NOT a hard error: fall back to
        // building from source (directive 1: a source build is always available, so a flaky or
        // hostile substitute server can never BREAK a build that would otherwise succeed).
        // restore_substitute cleans its own partial tree; we roll back the earlier outputs.
        let reg = match restore_substitute(
            &narinfo,
            &fetchdir.join(narfile),
            &o.path,
            &newstore,
            drv_path,
        ) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("td-builder: substitute for {base} failed verification ({e}); building");
                rollback(&restored_bases);
                return Ok(None);
            }
        };
        restored_bases.push(base.to_string());
        record.push_str(&format!("path {}\n", reg.store_path));
        record.push_str(&format!("nar-hash {}\n", reg.nar_hash));
        record.push_str(&format!("nar-size {}\n", reg.nar_size));
        for r in &reg.refs {
            record.push_str(&format!("reference {r}\n"));
        }
        record.push_str(&format!("deriver {}\n\n", reg.deriver));
        regs.push(reg);
    }
    // All outputs restored + verified: only NOW emit the OUT= lines (a mid-loop fallback must
    // not print OUT= for an output the build will re-emit), then write the same registration +
    // td.db a real build writes, so a later cached_realization hits and a downstream build-plan
    // step can resolve this output's closure.
    for (o, reg) in parsed.outputs.iter().zip(&regs) {
        println!("OUT={} {}", o.name, reg.store_path);
    }
    std::fs::write(scratch.join("registration"), record).map_err(|e| e.to_string())?;
    write_output_db(&regs, &scratch.join("td.db"))?;
    Ok(Some(regs))
}

/// A td-OWNED source store handed to `realize`/`build-recipe`: the `canonical`
/// source path is NOT in the daemon DB (td interned it itself, gate 285's
/// store-add-recursive), so its no-reference closure is read from the td `db`, and
/// it is staged by binding from `on_disk` (the td store dir) rather than its
/// canonical `/gnu/store/<base>` (which the daemon never created). Retires the
/// `guix repl … lower-object %builder-source` source PREP (move-off-Guile §5).
struct SrcOverride {
    canonical: String,
    on_disk: String,
    db: String,
}

/// A td-OWNED builder handed to `build-recipe` (bootstrap brick 2): the `canonical`
/// builder path is NOT in the daemon DB — td placed a stage0 td-builder there itself
/// (store-add-builder), a binary guix NEVER produced. Unlike `SrcOverride` the builder
/// HAS references (the glibc/gcc-lib it links), so its closure spans two DBs: its
/// DIRECT refs come from the builder `db` (store-add-builder registered them), and each
/// such ref's TRANSITIVE closure from the daemon/seed `store_db` (the pinned toolchain
/// lives there). The `canonical` entry is staged by binding from `on_disk` (the td
/// store dir). Lets the loop BUILD with stage0 as the builder-of-record (move-off-Guile
/// §5 "build the seed with td").
struct BuilderOverride {
    canonical: String,
    on_disk: String,
    db: String,
}

/// The machine-wide concurrent-build budget for the build daemon: `TD_BUILD_JOBS` if set,
/// else `min(nproc*3/4, MemAvailableGiB / 2)` clamped to ≥1. This is the ONE cap that all
/// agents' submissions to the single shared daemon share, so it must bound the whole box
/// (leaving ~1/4 of cores + memory headroom for interactive work and the not-yet-daemon-
/// managed heavy gates) — never a per-check slice, which N agents would multiply.
fn daemon_budget() -> usize {
    if let Ok(v) = std::env::var("TD_BUILD_JOBS") {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n >= 1 {
                return n;
            }
        }
    }
    let nproc = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cpu_cap = (nproc * 3 / 4).max(1);
    match mem_available_gib() {
        Some(g) => cpu_cap.min(((g / 2.0) as usize).max(1)),
        None => cpu_cap,
    }
}

/// MemAvailable from /proc/meminfo, in GiB (None if unreadable).
fn mem_available_gib() -> Option<f64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: f64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb / 1024.0 / 1024.0);
        }
    }
    None
}

/// The optional td-owned builder override from TD_BUILDER_PATH/STORE/DB (all three set
/// together, or none) — the stage0 td-builder that a corpus drv names as its builder.
/// Shared by the daemon and its spawned per-build children (which re-read the same env).
fn builder_override_from_env() -> Result<Option<BuilderOverride>, String> {
    let bp = std::env::var("TD_BUILDER_PATH").ok();
    let bs = std::env::var("TD_BUILDER_STORE").ok();
    let bd = std::env::var("TD_BUILDER_DB").ok();
    match (&bp, &bs, &bd) {
        (Some(canonical), Some(store_dir), Some(db)) => {
            let base = canonical.rsplit('/').next().unwrap_or(canonical);
            Ok(Some(BuilderOverride {
                canonical: canonical.clone(),
                on_disk: format!("{store_dir}/{base}"),
                db: db.clone(),
            }))
        }
        (None, None, None) => Ok(None),
        _ => {
            Err("TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together".to_string())
        }
    }
}

/// The content-addressed first-output basename of `drv` — the STABLE per-drv scratch/dedup
/// key (the same drv always keys the same dir, so a valid prior realization is a cache hit).
fn drv_scratch_key(drv: &str) -> Result<String, String> {
    let content = std::fs::read(drv).map_err(|e| format!("read {drv}: {e}"))?;
    let parsed = drv::parse(&content).map_err(|e| format!("parse drv {drv}: {e}"))?;
    let first = parsed
        .outputs
        .first()
        .ok_or_else(|| format!("{drv}: derivation has no outputs"))?;
    first
        .path
        .rsplit('/')
        .next()
        .map(str::to_string)
        .ok_or_else(|| format!("{}: not a store path", first.path))
}

/// Host path of `canon`'s output tree under a keyed scratch dir (`<scr>/newstore/<base>`).
fn daemon_host_path(scr: &Path, canon: &str) -> Result<String, String> {
    let base = canon
        .strip_prefix("/gnu/store/")
        .ok_or_else(|| format!("{canon}: not a store path"))?;
    Ok(scr.join("newstore").join(base).to_string_lossy().into_owned())
}

/// Is every one of `canons`' output trees present under `dir` (a keyed build scratch)?
/// The daemon's CHECK verb uses this to decide whether it can reuse a prior build as one
/// of the two independent reproducibility builds (all present ⇒ reuse, one rebuild) or
/// must do a second fresh build (any missing ⇒ the bare-CHECK fallback). Empty ⇒ false, so
/// a drv with no outputs never spuriously "reuses" a vacuous baseline.
fn output_trees_present(dir: &Path, canons: &[String]) -> bool {
    !canons.is_empty()
        && canons.iter().all(|canon| {
            daemon_host_path(dir, canon)
                .map(|p| Path::new(&p).exists())
                .unwrap_or(false)
        })
}

/// Realize ONE drv into a content-addressed keyed scratch under `scratch_base`, with
/// guix-daemon-parity cache reuse (a valid prior output is not rebuilt). Returns
/// (canonical store path, host output path). Run in a child process by `daemon-build`.
fn daemon_realize_one(
    drv: &str,
    seed_dir: &str,
    scratch_base: &Path,
) -> Result<(String, String, bool), String> {
    let ov = builder_override_from_env()?;
    let content = std::fs::read(drv).map_err(|e| format!("read {drv}: {e}"))?;
    let parsed = drv::parse(&content).map_err(|e| format!("parse drv {drv}: {e}"))?;
    let key = drv_scratch_key(drv)?;
    let scr = scratch_base.join(&key);
    let mk = |regs: &[OutputReg]| -> Result<(String, String), String> {
        let first = regs
            .first()
            .ok_or_else(|| "realize produced no outputs".to_string())?;
        let canon = first.store_path.clone();
        let host = daemon_host_path(&scr, &canon)?;
        Ok((canon, host))
    };
    if let Some(regs) = cached_realization(&parsed, &scr)? {
        eprintln!(
            "td-builder: daemon CACHE HIT for {drv} — output already valid under {}, not rebuilding",
            scr.display()
        );
        let (c, h) = mk(&regs)?;
        return Ok((c, h, true));
    }
    eprintln!("td-builder: daemon CACHE MISS for {drv} — realizing");
    let seed_dirs = [seed_dir.to_string()];
    // The daemon scans the live store dir: entries are canonical where they sit.
    let regs = realize_drv(drv, &seed_dirs, &store::store_dir(), &[], &scr, &[], ov.as_ref(), None)?;
    let (c, h) = mk(&regs)?;
    Ok((c, h, false))
}

/// Reproducibility check of ONE drv (the daemon's `CHECK` verb): compare two INDEPENDENT
/// realizations of the drv by per-output NAR hash. Returns the first output's (canonical,
/// host) on success, an Err naming the divergence otherwise. Run in a child process by
/// `daemon-check` so the repro rebuild ALSO counts against the budget.
///
/// The proof needs two independent builds; it does NOT need two *fresh* ones. The `daemon-build`
/// verb already realized this drv into `scratch_base/<key>` — the artifact the client consumes —
/// so this reuses THAT as the first build and rebuilds only ONCE here: two genuine builds total,
/// not three (this verb used to discard the built artifact and realize twice more, tripling the
/// single-threaded build cost that dominates `build-recipes`). In the loop substitutes are off,
/// so the build verb's output is a real local build; comparing a fresh rebuild against it is a
/// full two-independent-build reproducibility test (and additionally catches cross-run drift).
/// When no prior build output is present (a bare `CHECK` issued with no preceding build), it
/// falls back to a second fresh build, so the verb stays correct on its own.
fn daemon_check_one(
    drv: &str,
    seed_dir: &str,
    scratch_base: &Path,
) -> Result<(String, String), String> {
    let ov = builder_override_from_env()?;
    let seed_dirs = [seed_dir.to_string()];
    let key = drv_scratch_key(drv)?;
    let scr = scratch_base.join(format!("{key}-chk"));
    let _ = std::fs::remove_dir_all(&scr); // the rebuild here must be fresh, never a cache reuse
    let r1 = scr.join("r1");
    let regs1 = realize_drv(drv, &seed_dirs, &store::store_dir(), &[], &r1, &[], ov.as_ref(), None)?;
    // Baseline for the comparison: the build verb's already-realized output at
    // scratch_base/<key> when every output tree is present there (the loop's normal path,
    // ⇒ 2 builds total), else a SECOND fresh build (bare-CHECK fallback ⇒ the original 3).
    let built = scratch_base.join(&key);
    let canons: Vec<String> = regs1.iter().map(|r| r.store_path.clone()).collect();
    let base_dir = if output_trees_present(&built, &canons) {
        built
    } else {
        let r2 = scr.join("r2");
        let _ = realize_drv(drv, &seed_dirs, &store::store_dir(), &[], &r2, &[], ov.as_ref(), None)?;
        r2
    };
    for reg in &regs1 {
        let canon = &reg.store_path;
        let h1 = nar_hash(&daemon_host_path(&r1, canon)?).map_err(|e| e.to_string())?;
        let h2 = nar_hash(&daemon_host_path(&base_dir, canon)?).map_err(|e| e.to_string())?;
        if h1 != h2 {
            return Err(format!("NON-REPRODUCIBLE {canon}: {h1} != {h2}"));
        }
    }
    let first = regs1
        .first()
        .ok_or_else(|| "realize produced no outputs".to_string())?;
    let canon = first.store_path.clone();
    let host = daemon_host_path(&r1, &canon)?;
    Ok((canon, host))
}

/// Build the content-scan candidate index over one or more on-disk store DIRECTORIES —
/// the guix/seed store bytes — with NO store DB and NO guix daemon. Returns the candidate
/// CANONICAL paths (`<CANONICAL_PREFIX>/<basename>`, what a reference literally present in
/// the bytes resolves to) plus a canonical→on-disk map (where those bytes actually live, so
/// a seed staged under a td-store dir is NAR-read from there). Dedup is by 32-char hash part
/// keeping the SHORTEST basename (the canonical entry, not a `.chroot`/`.check` sibling), and
/// `.lock` aux files are skipped — the daemon's own candidate criterion. An absent dir is
/// skipped (a caller may pass an optional td-store dir). This is the hoisted candidate set a
/// `scan::Scanner` matches against (store-closure-scan / #260): building it ONCE and
/// `reset()`-ing between paths keeps a whole-live-store walk O(bytes), not O(candidates).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn scan_candidate_index(
    store_dirs: &[String],
    canonical_prefix: &str,
) -> Result<(Vec<String>, std::collections::HashMap<String, String>), String> {
    use std::collections::HashMap;
    // hash part -> (basename, on-disk dir); shortest basename wins.
    let mut by_hash: HashMap<String, (String, String)> = HashMap::new();
    for dir in store_dirs {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(_) => continue, // an absent optional store dir contributes nothing
        };
        for entry in rd {
            let entry = entry.map_err(|e| format!("{dir}: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".lock") {
                continue;
            }
            match name.split('-').next() {
                Some(p) if p.len() == 32 => {
                    let keep = match by_hash.get(p) {
                        Some((cur, _)) => name.len() < cur.len(),
                        None => true,
                    };
                    if keep {
                        by_hash.insert(p.to_string(), (name.clone(), dir.clone()));
                    }
                }
                _ => continue,
            }
        }
    }
    let mut candidates = Vec::with_capacity(by_hash.len());
    let mut on_disk = HashMap::with_capacity(by_hash.len());
    for (_h, (name, dir)) in by_hash {
        let canonical = format!("{canonical_prefix}/{name}");
        candidates.push(canonical.clone());
        on_disk.insert(canonical, format!("{dir}/{name}"));
    }
    Ok((candidates, on_disk))
}

/// Re-key candidate-index entries onto their TRUE canonical store paths (#292). A seed
/// staging dir mixes entries whose canonical homes DIFFER — guix-captured bytes live at
/// `/gnu/store`, td-built copies (a chained /td/store toolchain) at `/td/store` — but
/// `scan_candidate_index` can only stamp ONE prefix on all of them. OVERRIDES carries the
/// hash-keyed truth the caller does know: the drv's own roots (the lock is authoritative
/// for its entries' canonicals) and every td-OWNED store DB registration. Without this,
/// a root whose prefix differs from the stamped one misses the on-disk map, is never
/// content-scanned, and silently drops its whole transitive runtime closure (gate 377:
/// coreutils' gmp vanished and `expr` died on libgmp.so.10).
///
/// PRECONDITION: an entry whose true canonical differs from the stamped seed prefix must
/// be visible as a drv root or via a TD_EXTRA_DBS registration, or it keeps the stamp.
/// Callers satisfy this by construction — every td-built tree is created WITH its OUT-DB
/// (store-add-recursive/store-add-builder/write_output_db), and the paths that stage one
/// into a seed dir (gate 377's toolchain pair, the td-shell native store) pass that DB in
/// TD_EXTRA_DBS and/or name the tree as a lock root. Don't stage an unregistered td-built
/// tree into a seed dir.
fn recanonicalize_candidates(
    candidates: &mut [String],
    on_disk: &mut std::collections::HashMap<String, String>,
    overrides: &std::collections::HashMap<String, String>,
) {
    for c in candidates.iter_mut() {
        let Some(h) = store::hash_from_store_path(c) else { continue };
        let Some(true_canonical) = overrides.get(h) else { continue };
        if true_canonical == c {
            continue;
        }
        if let Some(od) = on_disk.remove(c) {
            on_disk.insert(true_canonical.clone(), od);
        }
        *c = true_canonical.clone();
    }
}

/// Compute the runtime closure of ROOTS with NO guix store DB: BFS to fixpoint, each path's
/// references found by NAR-scanning its bytes (`scan::Scanner` against the seed candidate
/// index) UNIONed with the direct references any td-OWNED store DB registered for it
/// (EXTRA_REFS — build-plan's td.dbs, whose td-built dep bytes live OUTSIDE the scanned seed
/// dirs). Content-scan is the daemon's scanForReferences — equal to `guix gc -R` for an
/// output root (gate 290, store-gc); a union with a byte-scan superset never DROPS a real
/// reference (the only unsafe direction is under-staging). SCANNER carries the candidate
/// index built ONCE; it is `reset()` between paths, so this is O(bytes scanned), not
/// O(candidates × paths). Returns the reachable canonical paths (ROOTS included).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn scan_closure_hybrid(
    scanner: &mut scan::Scanner,
    on_disk: &std::collections::HashMap<String, String>,
    extra_refs: &std::collections::HashMap<String, Vec<String>>,
    roots: &[String],
) -> Result<std::collections::BTreeSet<String>, String> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = roots.to_vec();
    while let Some(p) = stack.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        let mut refs: Vec<String> = Vec::new();
        // Seed bytes (this path lives in a scanned store dir): content-scan its NAR.
        if let Some(od) = on_disk.get(&p) {
            scanner.reset();
            nar::write_nar(scanner, Path::new(od))
                .map_err(|e| format!("nar {p} (at {od}): {e}"))?;
            refs.extend(scanner.refs());
        }
        // A td-OWNED store DB's DIRECT refs (a td-built dep staged outside the seed dirs).
        if let Some(rs) = extra_refs.get(&p) {
            refs.extend(rs.iter().cloned());
        }
        for r in refs {
            if !seen.contains(&r) {
                stack.push(r);
            }
        }
    }
    Ok(seen)
}

/// Merge the DIRECT-reference graph of one or more td-OWNED store DBs (build-plan's td.dbs /
/// TD_EXTRA_DBS) into a single `path -> direct refs` map, for `scan_closure_hybrid`. These
/// DBs are td's OWN registration (never `/var/guix`); they carry a td-built dep whose bytes
/// live outside the content-scanned seed dirs, so its refs are read from the DB it wrote.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn merge_extra_refs(
    extra_dbs: &[String],
) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
    let mut extra_refs: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for dbp in extra_dbs {
        let data = std::fs::read(dbp).map_err(|e| format!("read store db {dbp}: {e}"))?;
        let db = store_db_read::Db::open(data)?;
        for (from, tos) in db.refs_by_path()? {
            extra_refs.entry(from).or_default().extend(tos);
        }
    }
    Ok(extra_refs)
}

/// Realize DRV with NO guix-daemon and NO guix store DB: compute the input closure ITSELF by
/// CONTENT-SCANNING the seed store dir(s) (the daemon's scanForReferences / `guix gc -R`,
/// gate 290) — no `/var/guix/db` read — build it in the userns sandbox (build_and_register),
/// and register the output(s) into a td store-db at SCRATCH/td.db. Returns the per-output
/// records. Shared by `realize`, `build-recipe` and the build daemon. SRC_OVERRIDE, when set,
/// supplies the recipe source from a td-owned store instead of the daemon store (no `guix
/// repl` interning). SEED_STORE_DIRS is the set of store DIRECTORIES the seed/toolchain
/// closure is content-scanned over (`/gnu/store`, or the unpacked seed store); EXTRA_DBS is
/// the set of td-OWNED store DBs whose td-built deps live outside those dirs (build-plan
/// passes the prior steps' td.dbs so a downstream build's closure spans both). BUILDER_OVERRIDE,
/// when set, supplies the drv's `builder` from a td-owned store (a td-bootstrapped stage0, not
/// the guix-built td-builder) — the builder entry binds from the builder DB and its direct
/// refs' TRANSITIVE closures come from the seed content-scan. TD_STORE, when set, names td's
/// own store dir holding td-BUILT deps: a closure path whose tree lives under TD_STORE/<base>
/// is emitted `canonical\ton-disk` so the sandbox binds it FROM THERE (the build-plan chaining
/// edge) — the same on-disk encoding SRC_OVERRIDE uses. SEED_CANONICAL_PREFIX is the canonical
/// home of the seed dirs' entries — `/gnu/store` for a guix-captured seed/warm-seed staging
/// dir, the live `store::store_dir()` when scanning the active store itself; per-entry truth
/// (a td-built copy inside a guix seed dir, or vice versa) is restored from the drv roots +
/// td-owned DBs by `recanonicalize_candidates` (#292).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn realize_drv(
    drv_path: &str,
    seed_store_dirs: &[String],
    seed_canonical_prefix: &str,
    extra_dbs: &[String],
    scratch: &Path,
    src_overrides: &[SrcOverride],
    builder_override: Option<&BuilderOverride>,
    td_store: Option<&Path>,
) -> Result<Vec<OutputReg>, String> {
    let bytes = std::fs::read(drv_path).map_err(|e| e.to_string())?;
    let parsed = drv::parse(&bytes).map_err(|e| e.to_string())?;
    // Input ROOTS: the drv's source inputs, plus each input derivation's requested
    // output paths (resolved by reading that input .drv).
    let mut roots: Vec<String> = parsed.input_srcs.clone();
    for (idrv, outnames) in &parsed.input_drvs {
        let ib = std::fs::read(idrv).map_err(|e| format!("read input drv {idrv}: {e}"))?;
        let ip = drv::parse(&ib).map_err(|e| format!("parse input drv {idrv}: {e}"))?;
        for on in outnames {
            let o = ip
                .outputs
                .iter()
                .find(|o| &o.name == on)
                .ok_or_else(|| format!("input drv {idrv} has no output `{on}'"))?;
            roots.push(o.path.clone());
        }
    }
    // Compute the input closure with NO guix store DB: CONTENT-SCAN the seed store dir(s)
    // for the seed/toolchain roots (scanForReferences == `guix gc -R` for an output root,
    // gate 290), UNIONed with the direct refs any td-OWNED store DB registers (build-plan's
    // td.dbs — a td-built dep staged outside the seed dirs). The candidate index (canonical
    // paths + a canonical→on-disk map) is built ONCE; the Scanner is reset() between roots.
    if seed_store_dirs.is_empty() {
        return Err("realize: no seed store dir given".to_string());
    }
    let extra_refs = merge_extra_refs(extra_dbs)?;
    // TRUE-canonical overrides for the index, keyed by store hash (#292): td-owned DB
    // registrations first, then the drv's own roots (the drv/lock is the stronger
    // authority where both name the same hash). Every other seed entry keeps
    // SEED_CANONICAL_PREFIX.
    let mut canonical_overrides: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for p in extra_refs.keys() {
        if let Some(h) = store::hash_from_store_path(p) {
            canonical_overrides.insert(h.to_string(), p.clone());
        }
    }
    for r in &roots {
        if let Some(h) = store::hash_from_store_path(r) {
            canonical_overrides.insert(h.to_string(), r.clone());
        }
    }
    let (mut candidates, mut on_disk) =
        scan_candidate_index(seed_store_dirs, seed_canonical_prefix)?;
    recanonicalize_candidates(&mut candidates, &mut on_disk, &canonical_overrides);
    let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
    // Each td-OWNED interned tree (the recipe source AND the vendored-crate tree) has its
    // own DB — the seed store has no row for it. Open them paired with their override so a
    // root can be matched to its store + db. Both are no-reference content-addressed trees
    // (store-add-recursive), so they share the SrcOverride handling.
    let src_dbs: Vec<(&SrcOverride, store_db_read::Db)> = src_overrides
        .iter()
        .map(|ov| {
            let data =
                std::fs::read(&ov.db).map_err(|e| format!("read source db {}: {e}", ov.db))?;
            Ok::<_, String>((ov, store_db_read::Db::open(data)?))
        })
        .collect::<Result<_, _>>()?;
    // The td-owned builder likewise has its own DB (store-add-builder wrote the builder
    // + its DIRECT refs there); the seed store has no row for the builder itself.
    let builder_db = match builder_override {
        Some(ov) => Some(store_db_read::Db::open(
            std::fs::read(&ov.db).map_err(|e| format!("read builder db {}: {e}", ov.db))?,
        )?),
        None => None,
    };
    let mut closure: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for r in &roots {
        // A td-interned tree (the recipe source OR the vendored-crate tree): no-ref
        // closure from its OWN db, the entry bound FROM on_disk (canonical\ton-disk).
        if let Some(entry) = src_dbs.iter().find(|e| r == &e.0.canonical) {
            let (ov, sdb) = (entry.0, &entry.1);
            for p in sdb.closure(r)? {
                let line = if p == ov.canonical {
                    format!("{p}\t{}", ov.on_disk)
                } else {
                    p
                };
                closure.insert(line);
            }
            continue;
        }
        // The td-placed builder gets its closure from the builder DB; every other
        // root from the seed content-scan (∪ any td-owned extra dbs).
        match (builder_override, &builder_db) {
            // The td-placed builder: builder DB gives {builder} ∪ its DIRECT refs;
            // the builder entry binds from on_disk (canonical\ton-disk), and each
            // direct ref's TRANSITIVE closure is CONTENT-SCANNED from the seed store
            // (the pinned toolchain lives there — glibc/gcc-lib + their deps).
            (Some(ov), Some(bdb)) if r == &ov.canonical => {
                for p in bdb.closure(r)? {
                    if p == ov.canonical {
                        closure.insert(format!("{p}\t{}", ov.on_disk));
                    } else {
                        for q in scan_closure_hybrid(
                            &mut scanner,
                            &on_disk,
                            &extra_refs,
                            std::slice::from_ref(&p),
                        )? {
                            closure.insert(q);
                        }
                    }
                }
            }
            _ => {
                for q in scan_closure_hybrid(
                    &mut scanner,
                    &on_disk,
                    &extra_refs,
                    std::slice::from_ref(r),
                )? {
                    closure.insert(q);
                }
            }
        }
    }
    // A td-BUILT dep's files live under TD_STORE/<base>, not /gnu/store. Re-key those
    // closure entries to `canonical\ton-disk` so the sandbox binds them FROM td's store
    // (split_closure_entry) — the build-plan chaining edge, the same on-disk encoding
    // SRC_OVERRIDE uses. Bare guix-seed entries and already-overridden entries (the
    // SRC_OVERRIDE source) pass through unchanged. The on-disk half rides through
    // closure.txt, so a later `td-builder check` of this drv stages the dep with no
    // extra argument.
    let closure: Vec<String> = closure
        .into_iter()
        .map(|e| {
            if e.contains('\t') {
                return e;
            }
            if let Some(ts) = td_store {
                // Re-key by BASENAME (store hashes are unique), so a seed staging-store that holds BOTH
                // /gnu/store deps AND /td/store td-built deps (e.g. a chained /td/store toolchain, brick 8)
                // binds every input from the seed regardless of its canonical prefix — not only paths under
                // the active store_dir(). Bare entries whose basename isn't in the seed pass through.
                let base = e.rsplit('/').next().unwrap_or(e.as_str());
                if !base.is_empty() {
                    let on_disk = ts.join(base);
                    if on_disk.exists() {
                        return format!("{e}\t{}", on_disk.display());
                    }
                }
            }
            e
        })
        .collect();
    eprintln!(
        "td-builder: realize computed the input closure ITSELF — {} paths by CONTENT-SCANNING {} seed store dir(s) (+ {} td-owned db(s)); no /var/guix/db, no guix gc, no daemon",
        closure.len(),
        seed_store_dirs.len(),
        extra_dbs.len()
    );
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    std::fs::write(scratch.join("closure.txt"), closure.join("\n")).map_err(|e| e.to_string())?;
    let regs = build_and_register(drv_path, &closure, scratch)?;
    // td OWNS the store record of its build: write a td store-db registering the
    // realized output(s) — the daemon's post-build registration, in pure Rust.
    write_output_db(&regs, &scratch.join("td.db"))?;
    eprintln!(
        "td-builder: realize registered {} output(s) into td's store-db {}",
        regs.len(),
        scratch.join("td.db").display()
    );
    Ok(regs)
}

/// The td-builder store path of the RUNNING binary (…/td-builder-<v>), stripped of
/// the trailing `/bin/td-builder` — so a recipe built by td references the very
/// builder that built it, with no Guile resolution.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn self_store_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let s = exe.to_string_lossy();
    let bin = s
        .strip_suffix("/bin/td-builder")
        .ok_or_else(|| format!("td-builder is not at <store>/bin/td-builder: {s}"))?;
    Ok(bin.to_string())
}

/// build-recipe: build a TS-authored recipe with NO Guile and NO guix-daemon in the
/// path. Reads the recipe JSON (produced Guile-free by ts-eval), resolves EVERY input
/// from LOCK (`NAME <path>`, no specification->package) — the source is keyed
/// `<name>-source`, the td-builder builder is the running binary, every other lock
/// entry is a build input — assembles the `.drv` itself (store::assemble_drv, the
/// inputs as input-SOURCES), and realizes it (realize_drv over STORE-DB). The
/// toolchain + lock are the guix-built SEED (§5, retired last); nothing in the
/// build path is guix/Guile. The recipe's `buildSystem` selects the phase runner —
/// `"gnu"` → `autotools-build` (configureFlags/phases), `"rust"` → `rust-build`
/// (cargo; installs the recipe's `bins`). Usage: build-recipe RECIPE-JSON LOCK
/// SCRATCH STORE-DB [SRC-STORE-DIR SRC-DB]
///
/// SRC-STORE-DIR + SRC-DB (optional) make the `<name>-source` a td-OWNED source: td
/// interned the tree ITSELF (store-add-recursive) into SRC-STORE-DIR + SRC-DB, so the
/// source is staged from there and its closure read from SRC-DB — no `guix repl …
/// lower-object` daemon interning in the source PREP (move-off-Guile §5). Omitted →
/// the source is a daemon-resident store path, exactly as before.
///
/// BUILDER_STORE (optional, `(canonical, store_dir, db)`) makes the drv's `builder` a
/// td-OWNED stage0 td-builder (store-add-builder placed it at `canonical`, restored
/// under `store_dir`, refs in `db`) instead of the running guix-built binary — the
/// loop then BUILDS with a binary guix never produced (bootstrap brick 2). Omitted →
/// the builder is `self_store_path()` (the guix-built td-builder), exactly as before.
///
/// STORE_DBS (the closure's store-db set) and TD_STORE (td's own store dir for td-BUILT
/// deps) thread straight through to realize_drv — build-plan passes the multi-db set +
/// td-store so a downstream step consumes an upstream step's td-built output.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_recipe(
    recipe_json: &str,
    lock_file: &str,
    scratch: &Path,
    seed_store_dirs: &[String],
    seed_canonical_prefix: &str,
    extra_dbs: &[String],
    src_store: Option<(&str, &str)>,
    vendor_store: Option<(&str, &str, &str)>,
    builder_store: Option<(&str, &str, &str)>,
    td_store: Option<&Path>,
    persist: Option<(&str, &str)>,
) -> Result<Vec<OutputReg>, String> {
    // A td-OWNED builder (optional, bootstrap brick 2): the drv's `builder` is a stage0
    // td-builder td placed at `canonical` (store-add-builder), restored under store_dir,
    // refs in db — a binary guix never produced. The on-disk tree is the canonical
    // basename under store_dir. Omitted → the running guix-built binary.
    let builder_override = builder_store.map(|(canonical, store_dir, db)| {
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        BuilderOverride {
            canonical: canonical.to_string(),
            on_disk: format!("{store_dir}/{base}"),
            db: db.to_string(),
        }
    });
    // The builder store path: the td-placed stage0 (override) or, by default, the
    // running binary (self_store_path — the guix-built td-builder).
    let builder_path = match &builder_override {
        Some(ov) => ov.canonical.clone(),
        None => self_store_path()?,
    };
    // td assembles the .drv ITSELF (pure Rust, no guix (derivation …), no Guile, no
    // daemon) and writes it to SCRATCH — the SAME assembly `assemble-recipe` uses, so a
    // separate process (the build daemon) realizes a byte-identical td-assembled drv.
    let (drv_path, drv_file, parsed, source) = assemble_recipe_drv(
        recipe_json,
        lock_file,
        scratch,
        &builder_path,
        vendor_store.map(|(canonical, _, _)| canonical),
    )?;
    // A td-OWNED source store (optional): the `<name>-source` path was interned by td
    // itself into SRC-STORE-DIR + SRC-DB, so realize stages it from there + reads its
    // closure from SRC-DB — no daemon interning. The on-disk tree is the canonical
    // basename under SRC-STORE-DIR (store-add-recursive restored it there).
    let src_override = src_store.map(|(store_dir, db)| {
        let base = source.rsplit('/').next().unwrap_or(&source);
        SrcOverride {
            canonical: source.clone(),
            on_disk: format!("{store_dir}/{base}"),
            db: db.to_string(),
        }
    });
    // A td-OWNED vendored-crate tree (optional, the guix-free crate path): td interned the
    // crate SET itself (store-add-recursive) into VENDOR-STORE-DIR + VENDOR-DB — a no-ref
    // content-addressed tree, staged + its closure read from there exactly like the source,
    // with NO daemon and NO `/gnu/store` crate path. run_rust vendors from it (TD_VENDOR_DIR).
    let vendor_override = vendor_store.map(|(canonical, store_dir, db)| {
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        SrcOverride {
            canonical: canonical.to_string(),
            on_disk: format!("{store_dir}/{base}"),
            db: db.to_string(),
        }
    });
    // Both no-ref td-interned trees go to realize_drv as src-overrides.
    let src_overrides: Vec<SrcOverride> =
        src_override.into_iter().chain(vendor_override).collect();
    // Content-addressed build cache: if SCRATCH already holds a valid realization of
    // this exact (deterministic) drv, reuse it — skip the build. The gate points
    // SCRATCH at a persistent cache, so an unchanged recipe is a cache HIT and only a
    // CHANGED recipe (⇒ different drv hash ⇒ different output path, a miss) rebuilds.
    if let Some(regs) = cached_realization(&parsed, scratch)? {
        eprintln!(
            "td-builder: build-recipe CACHE HIT for {drv_path} — {} output(s) already realized + NAR-verified under {}; skipping the build",
            regs.len(),
            scratch.display()
        );
        for (o, r) in parsed.outputs.iter().zip(&regs) {
            println!("OUT={} {}", o.name, r.store_path);
        }
        // Re-write the td store-db even on a hit (deterministic from regs): a
        // downstream build-plan step reads this step's td.db to resolve the closure
        // of a td-built dependency, so it must exist whether or not we rebuilt.
        write_output_db(&regs, &scratch.join("td.db"))?;
        println!("CACHE=hit");
        return Ok(regs);
    }
    // PERSISTENT-STORE skip (opt-in, TD_PERSIST_STORE/TD_PERSIST_DB): an incremental
    // store that survives ACROSS invocations (the /td/store the loop builds into). If
    // this exact (deterministic) drv's output is already a valid path there — a PRIOR
    // invocation built it — and its tree re-verifies, read it back instead of rebuilding.
    // The daemon's valid-path skip, backed by an on-disk store across process boundaries.
    if let Some((ps, pd)) = persist {
        if let Some(regs) = persistent_realization(&parsed, ps, Path::new(pd), scratch)? {
            eprintln!(
                "td-builder: build-recipe PERSISTENT-STORE HIT for {drv_path} — {} output(s) already valid under {ps}; skipping the build",
                regs.len()
            );
            for (o, r) in parsed.outputs.iter().zip(&regs) {
                println!("OUT={} {}", o.name, r.store_path);
            }
            // A fresh scratch reusing a prior build's output still needs the registration
            // + td.db a real build writes (downstream staging / a later store-commit).
            std::fs::write(scratch.join("registration"), registration_text(&regs))
                .map_err(|e| e.to_string())?;
            write_output_db(&regs, &scratch.join("td.db"))?;
            println!("CACHE=persist");
            return Ok(regs);
        }
    }
    // SUBSTITUTE-OR-BUILD (opt-in, TD_SUBST_URL): fetch the outputs from a substitute
    // server instead of building. OFF for the verification loop — it never sets the env,
    // so this is a no-op there (directive 1: the loop always builds from source + --check).
    if let Some(regs) = try_substitute(&parsed, &drv_path, scratch)? {
        eprintln!(
            "td-builder: build-recipe SUBSTITUTED {} output(s) for {drv_path} (verified signature + NarHash); skipping the build",
            regs.len()
        );
        println!("CACHE=subst");
        return Ok(regs);
    }
    eprintln!("td-builder: build-recipe assembled {drv_path} (no guix (derivation), no Guile)");
    // td realizes it (no guix-daemon). With a td-owned source store, the source is
    // staged from td's own store + closure read from the td DB (no daemon interning);
    // with a td-owned builder, the drv's builder is staged from td's store + its
    // closure spans the builder DB ∪ the seed DB (no guix-built builder, brick 2);
    // td_store carries any td-BUILT deps (build-plan) for the multi-db closure's staging.
    let regs = realize_drv(
        &drv_file.to_string_lossy(),
        seed_store_dirs,
        seed_canonical_prefix,
        extra_dbs,
        scratch,
        &src_overrides,
        builder_override.as_ref(),
        td_store,
    )?;
    // PERSISTENT-STORE build-into: commit the freshly-built output(s) into the
    // incremental store so a LATER invocation reads them back (the skip above) —
    // build-into / read-back across builds, no daemon.
    if let Some((ps, pd)) = persist {
        commit_scratch_to_store(scratch, ps, Path::new(pd))?;
        eprintln!(
            "td-builder: build-recipe committed {} output(s) into the persistent store {ps}",
            regs.len()
        );
    }
    println!("CACHE=miss");
    Ok(regs)
}

/// Assemble a recipe's `.drv` with NO Guile and NO realize. Parses RECIPE-JSON, resolves
/// every input from LOCK (no specification->package), builds the drv spec (inputs as
/// input-SOURCES; BUILDER_PATH's `/bin/td-builder` is the drv's builder), assembles it
/// with `store::assemble_drv` (pure Rust, no guix (derivation …)), and writes it to
/// SCRATCH/<name>-<version>.drv — WITHOUT building it. Returns (canonical drv store path,
/// the written `.drv` file, the parsed derivation, the `<name>-source` path).
///
/// Substitute the /td/store gcc-toolchain `tc` for the lock's guix gcc-toolchain input(s) in
/// `inputs` (corpus-toolchain-default). A gcc-toolchain input is identified by its store-path
/// PACKAGE NAME (the part after the `<hash>-` store prefix) being `gcc-toolchain-…` — so
/// `<hash>-gcc-toolchain-15.2.0` matches but a bare `<hash>-gcc-14.3.0`, or an unrelated package
/// that merely embeds the segment interior (e.g. `<hash>-libfoo-gcc-toolchain-helper`), does NOT.
/// Only the toolchain input is swapped; every other build input + the order are untouched. Returns
/// true iff at least one input was substituted (callers no-op silently when none — see the override
/// site). A multi-match dedup is the caller's (`inputs.dedup()` after sort).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn substitute_gcc_toolchain(inputs: &mut [String], tc: &str) -> bool {
    let mut swapped = false;
    for p in inputs.iter_mut() {
        let base = p.rsplit('/').next().unwrap_or(p);
        // store basename = `<nix-base32 hash>-<package name>`; match the gcc-toolchain PACKAGE,
        // anchored at the name (split at the first `-`), not an interior substring.
        let is_toolchain =
            base.split_once('-').is_some_and(|(_hash, name)| name.starts_with("gcc-toolchain-"));
        if is_toolchain {
            *p = tc.to_string();
            swapped = true;
        }
    }
    swapped
}

/// Shared by `build-recipe` (which then realizes it daemon-free) and `assemble-recipe`
/// (assemble-only, so a SEPARATE process — the build daemon — realizes the td-assembled
/// drv). Splitting assembly from realization is what lets td's own daemon, not a `guix
/// repl`-emitted drv, be the build's input (own-builder-daemon §5).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn assemble_recipe_drv(
    recipe_json: &str,
    lock_file: &str,
    scratch: &Path,
    builder_path: &str,
    vendor_dir: Option<&str>,
) -> Result<(String, std::path::PathBuf, drv::Derivation, String), String> {
    let alist = json::parse(recipe_json).map_err(|e| format!("recipe JSON: {e}"))?;
    let name = alist.get("name").and_then(json::Json::as_str).ok_or("recipe: no name")?;
    let version = alist.get("version").and_then(json::Json::as_str).ok_or("recipe: no version")?;
    let full = format!("{name}-{version}");
    // The build system selects the td-builder phase runner. "gnu" (default) is the
    // autotools path; "rust" is the cargo path (build::run_rust), used to SELF-HOST
    // td-builder itself off Guile-construction + the daemon.
    let build_system = alist.get("buildSystem").and_then(json::Json::as_str).unwrap_or("gnu");
    let phase_runner = match build_system {
        "gnu" => "autotools-build",
        "rust" => "rust-build",
        // cmake: td's own cmake phase runner (build::run_cmake), the cmake-build-system
        // replacement — out-of-source cmake configure -> make -> make install in Rust.
        "cmake" => "cmake-build",
        // stage0: the seed executor (build::run_stage0, #378) — place the pinned
        // stage0-posix tree writable and exec its kaem interpreter; no build inputs.
        "stage0" => "stage0-build",
        // mesboot: the bootstrap-RUNG executor (build::run_mesboot, #378 slices
        // 2+3) — the recipe's typed steps run in the sandbox over staged inputs.
        "mesboot" => "mesboot-build",
        other => return Err(format!("recipe: unknown buildSystem `{other}' (known: gnu, rust, cmake, stage0, mesboot)")),
    };
    // configure flags + phases (both optional) -> JSON array string. A configure
    // flag may itself contain whitespace (e.g. `CFLAGS=-O2 -g -Wno-foo`), so the
    // list is carried as JSON — each element stays ONE ./configure argument — the
    // same drv-safe encoding TD_PHASES uses. (Space-joining shattered such flags.)
    let cflags = match alist.get("configureFlags") {
        Some(c) => c.to_json_string(),
        None => String::new(),
    };
    let phases = match alist.get("phases") {
        Some(p) => p.to_json_string(),
        None => String::new(),
    };
    // Resolve EVERY input from the lock (no Guile), via the typed lock parser
    // (`NAME PATH [CLASS]`, backward-compatible with 2-field locks). The `source`
    // entry is TD_SRC; a `crate` entry is a vendored Rust dependency
    // (TD_VENDOR_CRATES); a `seed` or `td-recipe-output` entry is a build input
    // (TD_INPUTS). Each input is also an input-src. A `td-recipe-output` entry's
    // PATH is td's own dep build when build-plan substituted it, or the guix
    // oracle when this lock is consumed standalone — either way it is just an
    // input here.
    let lock_text =
        std::fs::read_to_string(lock_file).map_err(|e| format!("read lock {lock_file}: {e}"))?;
    let src_key = format!("{name}-source");
    let entries = lock::parse(&lock_text, &src_key)?;
    let mut source = String::new();
    let mut inputs: Vec<String> = Vec::new();
    // Vendored Rust deps: `crate`-class entries are the dependency closure (from
    // Cargo.lock), handed to the rust phase runner as TD_VENDOR_CRATES rather than
    // as toolchain inputs. A gnu recipe has none, so its spec is unchanged.
    let mut vendor: Vec<String> = Vec::new();
    for e in &entries {
        match e.class {
            lock::Class::Source => source = e.path.clone(),
            lock::Class::Crate => vendor.push(e.path.clone()),
            lock::Class::Seed | lock::Class::TdRecipeOutput => inputs.push(e.path.clone()),
        }
    }
    if source.is_empty() {
        return Err(format!("lock has no `{src_key}' entry (the recipe source)"));
    }
    // Default corpus toolchain (corpus-toolchain-default): when TD_GCC_TOOLCHAIN names a /td/store
    // gcc-toolchain-shaped tree, SUBSTITUTE it for the lock's guix `gcc-toolchain-15.2.0` input — so the
    // corpus package is compiled by td's OWN /td/store toolchain (no guix gcc-toolchain bytes) instead of
    // guix's. The override path is staged as an input-src + reaches TD_INPUTS like any other input below;
    // its closure must be in the caller's store-dbs (the corpus gate interns the toolchain + threads its
    // db, exactly as the inline lock-rewrite did). Equivalent to rewriting the lock's gcc-toolchain line,
    // but done in the engine so it can be the DEFAULT for the corpus build path, not per-gate shell.
    // A no-swap (a lock with no gcc-toolchain — e.g. a pure-source package) is a SILENT no-op, NOT an
    // error: TD_GCC_TOOLCHAIN must be safe to set corpus-wide as the default. A package that wrongly
    // still pulls guix's toolchain is caught downstream by the gate's [no-guix-toolchain] assertion.
    if let Ok(tc) = std::env::var("TD_GCC_TOOLCHAIN") {
        if !tc.is_empty() {
            substitute_gcc_toolchain(&mut inputs, &tc);
        }
    }
    inputs.sort();
    // Dedup: the override collapses any (today single, but defensively >1) gcc-toolchain inputs to the
    // same path; the input-src loop + TD_INPUTS below must not carry it twice.
    inputs.dedup();
    vendor.sort();
    let builder = format!("{builder_path}/bin/td-builder");
    // Assemble the .drv spec: inputs as input-SOURCES (already-realized seed paths,
    // no input-derivations — so this diverges from guix's nano, by design).
    let mut spec = String::new();
    spec.push_str(&format!("name {full}\n"));
    spec.push_str("system x86_64-linux\n");
    spec.push_str(&format!("builder {builder}\n"));
    spec.push_str(&format!("arg {phase_runner}\n"));
    spec.push_str(&format!("input-src {source}\n"));
    spec.push_str(&format!("input-src {builder_path}\n"));
    for p in &inputs {
        spec.push_str(&format!("input-src {p}\n"));
    }
    // Vendored crates are also staged into the build (input-srcs); a gnu recipe has
    // none, so this adds nothing to its spec.
    for p in &vendor {
        spec.push_str(&format!("input-src {p}\n"));
    }
    // The td-OWNED vendored-crate TREE (guix-free crate path): one interned dir of
    // `*.crate`, staged as an input-src; run_rust vendors from it (TD_VENDOR_DIR set below).
    if let Some(vd) = vendor_dir {
        spec.push_str(&format!("input-src {vd}\n"));
    }
    spec.push_str(&format!("env TD_SRC={source}\n"));
    spec.push_str(&format!("env TD_INPUTS={}\n", inputs.join(":")));
    match build_system {
        // gnu: the autotools phase runner reads the configure flags + custom phases.
        "gnu" => {
            spec.push_str(&format!("env TD_CONFIGURE_FLAGS={cflags}\n"));
            spec.push_str(&format!("env TD_PHASES={phases}\n"));
        }
        // cmake: the cmake phase runner reads the extra `cmake` flags (TD_CONFIGURE_FLAGS);
        // the autotools `substitute*` phase interpreter (TD_PHASES) does not apply here.
        "cmake" => {
            spec.push_str(&format!("env TD_CONFIGURE_FLAGS={cflags}\n"));
        }
        // stage0: sealed — source + builder are the WHOLE closure. Any other build
        // material (inputs, crates/vendor tree) or unrunnable field (configureFlags/
        // phases — run_stage0 reads only TD_SRC/out) is a hard error, never ignored.
        "stage0" => {
            if !inputs.is_empty() {
                return Err(format!(
                    "recipe: buildSystem \"stage0\" takes no build inputs (the seed needs nothing), but the lock carries {}",
                    inputs.join(" ")
                ));
            }
            if !vendor.is_empty() || vendor_dir.is_some() {
                return Err(
                    "recipe: buildSystem \"stage0\" takes no vendored crates — a crate-class lock entry or vendor tree would stage a store path into the sealed seed sandbox".into(),
                );
            }
            if alist.get("configureFlags").is_some() || alist.get("phases").is_some() {
                return Err(
                    "recipe: buildSystem \"stage0\" supports no configureFlags/phases — the seed runner would silently ignore them, so declaring them is an error".into(),
                );
            }
        }
        // mesboot: the bootstrap-rung step executor (#378 slices 2+3). The typed
        // steps ride as JSON; {in:NAME} templates resolve through TD_INPUT_MAP
        // (lock entry name -> canonical store path, source entry included).
        // configureFlags/phases have no runner here — hard error, never ignored.
        "mesboot" => {
            let steps = alist
                .get("steps")
                .ok_or("recipe: buildSystem \"mesboot\" requires `steps'")?;
            if alist.get("configureFlags").is_some() || alist.get("phases").is_some() {
                return Err(
                    "recipe: buildSystem \"mesboot\" supports no configureFlags/phases — rungs declare typed `steps'".into(),
                );
            }
            spec.push_str(&format!("env TD_STEPS={}\n", steps.to_json_string()));
            let map = json::Json::Obj(
                entries
                    .iter()
                    .map(|e| (e.name.clone(), json::Json::Str(e.path.clone())))
                    .collect(),
            );
            spec.push_str(&format!("env TD_INPUT_MAP={}\n", map.to_json_string()));
        }
        // rust: the cargo phase runner installs the named binaries (TD_RUST_BINS) and,
        // if any vendored deps were locked, resolves them offline (TD_VENDOR_CRATES).
        "rust" => {
            let bins: Vec<&str> = alist
                .get("bins")
                .and_then(json::Json::as_arr)
                .map(|a| a.iter().filter_map(json::Json::as_str).collect())
                .unwrap_or_default();
            if bins.is_empty() {
                return Err("recipe: buildSystem \"rust\" requires a non-empty `bins'".into());
            }
            spec.push_str(&format!("env TD_RUST_BINS={}\n", bins.join(" ")));
            if !vendor.is_empty() {
                spec.push_str(&format!("env TD_VENDOR_CRATES={}\n", vendor.join(":")));
            }
            // td's OWN guix-free crate set: one interned dir of `*.crate` (run_rust reads
            // every crate from it). No `/gnu/store` crate path, no guix-daemon FOD.
            if let Some(vd) = vendor_dir {
                spec.push_str(&format!("env TD_VENDOR_DIR={vd}\n"));
            }
            // Native /td/store toolchain link mode (#258): the sandbox clears the env, so run_rust
            // only sees the drv's `env` lines — forward the caller's TD_RUST_STORE_* into the drv
            // (mirroring the TD_GCC_TOOLCHAIN input override above). When TD_RUST_STORE_INTERP is set
            // the ripgrep cutover is linking against the native /td/store gcc (a PLAIN gcc, no
            // ld-wrapper): run_rust bakes the interp/RUNPATH/-B explicitly so the built `rg` resolves
            // its libc/libgcc_s from /td/store at run time. The values are the /td/store glibc paths,
            // fixed for the run, so the drv (and its double-build `check`) stay deterministic. Unset
            // ⇒ no env lines emitted ⇒ the guix ld-wrapper path, unchanged.
            for k in ["TD_RUST_STORE_INTERP", "TD_RUST_STORE_RPATH", "TD_RUST_STORE_BDIR"] {
                if let Ok(v) = std::env::var(k) {
                    if !v.is_empty() {
                        spec.push_str(&format!("env {k}={v}\n"));
                    }
                }
            }
            // Optional cargo feature selection (both default-absent ⇒ a plain
            // `cargo build` with the crate's defaults, unchanged). `noDefaultFeatures`
            // drops the crate's default features — e.g. fd's `use-jemalloc`, whose
            // jemalloc-sys runs a C ./configure the scrubbed build-env can't satisfy;
            // `features` adds back the wanted ones (e.g. "completions").
            if alist.get("noDefaultFeatures").is_some_and(json::Json::is_true) {
                spec.push_str("env TD_CARGO_NO_DEFAULT=1\n");
            }
            if let Some(feats) = alist.get("features").and_then(json::Json::as_arr) {
                let fl: Vec<&str> = feats.iter().filter_map(json::Json::as_str).collect();
                if !fl.is_empty() {
                    spec.push_str(&format!("env TD_CARGO_FEATURES={}\n", fl.join(",")));
                }
            }
        }
        _ => unreachable!("buildSystem already validated"),
    }
    // td assembles the .drv (pure Rust, no guix (derivation …), no daemon).
    let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
    let (drv_path, content) = store::assemble_drv(&spec, &read)?;
    let parsed = drv::parse(content.as_bytes()).map_err(|e| format!("parse assembled drv: {e}"))?;
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let drv_file = scratch.join(format!("{full}.drv"));
    std::fs::write(&drv_file, &content).map_err(|e| e.to_string())?;
    Ok((drv_path, drv_file, parsed, source))
}

/// build-plan: realize a TOPO-ordered chain of recipes where a downstream step
/// consumes an UPSTREAM step's td-BUILT output instead of a guix store path. This
/// is the edge the per-package locks could not express: `recipe-checks` builds
/// grep's own derivation Guile-free but still links GUIX's pcre2; here grep links
/// the pcre2 td just built.
///
/// PLAN is line-based — `step RECIPE-JSON LOCK` per step, in dependency order. For
/// each step: any `td-recipe-output` lock entry is SUBSTITUTED with the matching
/// earlier step's output path (matched by the entry NAME == the producing recipe's
/// `name`); the recipe is built with `build_recipe` over a closure that spans
/// GUIX-DB (the transitive seeds) ∪ every prior step's `td.db` (the td-built deps),
/// staging those deps from a shared TD-STORE the steps populate. The output of each
/// step is copied into TD-STORE and its store path recorded for downstream steps.
///
/// Usage: build-plan PLAN GUIX-DB SCRATCH
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
/// The optional td-OWNED stage0 builder override from TD_BUILDER_PATH/STORE/DB — all
/// three set together (a `store-add-builder` placement) → the drv's builder is that
/// td-placed stage0, staged from its own store + db; none set → the running binary
/// (self_store_path). Any partial set is a loud error. Returns owned strings; borrow
/// them into the `(&str, &str, &str)` build_recipe/build_plan expects at the call site.
fn builder_store_env() -> Result<Option<(String, String, String)>, String> {
    match (
        std::env::var("TD_BUILDER_PATH").ok(),
        std::env::var("TD_BUILDER_STORE").ok(),
        std::env::var("TD_BUILDER_DB").ok(),
    ) {
        (Some(p), Some(s), Some(d)) => Ok(Some((p, s, d))),
        (None, None, None) => Ok(None),
        _ => Err("TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together".into()),
    }
}

fn build_plan(
    plan_file: &str,
    guix_store: &str,
    scratch: &Path,
    builder_store: Option<(&str, &str, &str)>,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    let plan = std::fs::read_to_string(plan_file)
        .map_err(|e| format!("read plan {plan_file}: {e}"))?;
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    // The shared td-store: each step copies its output here, and a downstream step
    // stages a td-built dep FROM here — realize_drv re-keys a closure entry whose tree
    // lives under <tdstore>/<base> to `canonical\ton-disk`, so the sandbox binds it from
    // td's store (split_closure_entry) instead of the daemon's /gnu/store. The on-disk
    // half rides through closure.txt, so a later `td-builder check` needs no extra state.
    let tdstore = scratch.join("tdstore");
    std::fs::create_dir_all(&tdstore).map_err(|e| e.to_string())?;

    // recipe name -> its (single) output store path; and each step's td.db, fed
    // into the closure of later steps so a td-built dep resolves.
    let mut built: BTreeMap<String, String> = BTreeMap::new();
    let mut td_dbs: Vec<String> = Vec::new();

    for raw in plan.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let toks: Vec<&str> = line.split_whitespace().collect();
        let (recipe_json, lock_file) = match toks.as_slice() {
            ["step", r, l] => (*r, *l),
            _ => return Err(format!("malformed plan line (want `step RECIPE-JSON LOCK'): {line}")),
        };
        let recipe_text = std::fs::read_to_string(recipe_json)
            .map_err(|e| format!("read recipe {recipe_json}: {e}"))?;
        let alist = json::parse(&recipe_text).map_err(|e| format!("recipe JSON {recipe_json}: {e}"))?;
        let name = alist
            .get("name")
            .and_then(json::Json::as_str)
            .ok_or_else(|| format!("recipe {recipe_json}: no name"))?;
        let step_scratch = scratch.join(name);
        std::fs::create_dir_all(&step_scratch).map_err(|e| e.to_string())?;

        // Substitute td-recipe-output entries with the producing step's output.
        let src_key = format!("{name}-source");
        let lock_text = std::fs::read_to_string(lock_file)
            .map_err(|e| format!("read lock {lock_file}: {e}"))?;
        let entries = lock::parse(&lock_text, &src_key)?;
        let mut resolved = String::new();
        let mut substituted: Vec<String> = Vec::new();
        for e in &entries {
            let path = if e.class == lock::Class::TdRecipeOutput {
                let p = built.get(&e.name).ok_or_else(|| {
                    format!("step `{name}': lock entry `{}' is td-recipe-output but no earlier step built it (plan out of topo order?)", e.name)
                })?;
                substituted.push(format!("{}={}", e.name, p));
                p.clone()
            } else {
                e.path.clone()
            };
            // Re-emit 2-field; build_recipe re-infers the class. A substituted td
            // path infers `seed` → an input-src, exactly the intent (it IS now a
            // realized input — just td's, not guix's).
            resolved.push_str(&format!("{} {}\n", e.name, path));
        }
        let resolved_lock = step_scratch.join("resolved.lock");
        std::fs::write(&resolved_lock, &resolved).map_err(|e| e.to_string())?;
        if substituted.is_empty() {
            eprintln!("td-builder: build-plan step `{name}': no td-built deps to substitute");
        } else {
            eprintln!("td-builder: build-plan step `{name}': substituted td outputs -> {}", substituted.join(" "));
        }

        // Closure content-scans guix's seed store (seeds) + reads every prior step's td.db
        // (td deps, whose bytes live in the shared td-store, outside the seed dir).
        let seed_dirs = [guix_store.to_string()];
        let regs = build_recipe(
            &recipe_text,
            &resolved_lock.to_string_lossy(),
            &step_scratch,
            &seed_dirs,
            store::STORE_DIR, // the guix seed store's canonical home
            &td_dbs,
            None,            // src_store: build-plan locks carry resolved paths
            None,            // vendor_store: build-plan deps are not vendored-crate trees
            builder_store,   // builder_store: the td-placed stage0 (TD_BUILDER_*), or None → self
            Some(&tdstore),  // td_store: stage td-built deps from the shared td-store
            None,            // persist: build-plan owns its own in-run td-store
        )?;
        // Single-output recipes (the gnu corpus): the dep is regs[0].
        let out = regs
            .first()
            .ok_or_else(|| format!("step `{name}': build produced no output"))?;
        let base = out
            .store_path
            .rsplit('/')
            .next()
            .ok_or_else(|| format!("step `{name}': output is not a store path"))?;
        // Copy the step's output into the shared td-store so a downstream step can
        // stage it (a real dir — sandbox::build bind-mounts it, so no symlink).
        let physical = step_scratch.join("newstore").join(base);
        let dest = tdstore.join(base);
        if !dest.exists() {
            copy_canonical(&physical, &dest)?;
        }
        built.insert(name.to_string(), out.store_path.clone());
        td_dbs.push(step_scratch.join("td.db").to_string_lossy().into_owned());
        println!("STEP {name} {}", out.store_path);
        eprintln!(
            "td-builder: build-plan step `{name}': out {} (staged into td-store {})",
            out.store_path,
            tdstore.display()
        );
    }
    eprintln!("td-builder: build-plan complete — {} step(s)", built.len());
    Ok(())
}

/// A recipe's declared inputs — the JSON `inputs` array UNION `nativeInputs`
/// (#378 staged builders: a rung's compiler is a prior rung's output; --auto
/// chains both edge kinds identically).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_inputs(recipe_dir: &str, name: &str) -> Result<Vec<String>, String> {
    let p = format!("{recipe_dir}/{name}.json");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("read recipe {p}: {e}"))?;
    let alist = json::parse(&text).map_err(|e| format!("recipe JSON {p}: {e}"))?;
    let mut xs: Vec<String> = Vec::new();
    for key in ["inputs", "nativeInputs"] {
        if let Some(a) = alist.get(key).and_then(json::Json::as_arr) {
            xs.extend(a.iter().filter_map(json::Json::as_str).map(str::to_string));
        }
    }
    Ok(xs)
}

/// An input is OWNED (td reconstructs it) iff both its recipe JSON and base lock exist;
/// otherwise it is an external seed (the toolchain, retired last) and stays guix-supplied.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_is_owned(recipe_dir: &str, lock_dir: &str, name: &str) -> bool {
    Path::new(&format!("{recipe_dir}/{name}.json")).exists()
        && Path::new(&format!("{lock_dir}/{name}-no-guix.lock")).exists()
}

/// Post-order DFS over the OWNED-input subgraph: appends each recipe AFTER its owned
/// deps → a topo order (deps first). Cycles error.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_topo(
    recipe_dir: &str,
    lock_dir: &str,
    name: &str,
    order: &mut Vec<String>,
    seen: &mut std::collections::BTreeSet<String>,
    stack: &mut Vec<String>,
) -> Result<(), String> {
    if seen.contains(name) {
        return Ok(());
    }
    if stack.iter().any(|s| s == name) {
        return Err(format!("--auto: dependency cycle through `{name}'"));
    }
    stack.push(name.to_string());
    for inp in auto_inputs(recipe_dir, name)? {
        if auto_is_owned(recipe_dir, lock_dir, &inp) {
            auto_topo(recipe_dir, lock_dir, &inp, order, seen, stack)?;
        }
    }
    stack.pop();
    seen.insert(name.to_string());
    order.push(name.to_string());
    Ok(())
}

/// A lock entry (first field + store path) names dep D iff the field is bare `D` or the
/// path basename is `<hash>-D-<version>` (32-char base32 hash + `-`). Handles both lock
/// conventions: declared inputs written bare (grep's `pcre2`) and hash-named entries.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_entry_is_dep(first: &str, path: &str, dep: &str) -> bool {
    if first == dep {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.len() > 33 && base.as_bytes().get(32) == Some(&b'-') {
        let rest = &base[33..];
        return rest == dep || rest.starts_with(&format!("{dep}-"));
    }
    false
}

/// Derive a chained lock from BASE_LOCK_TEXT by re-keying each OWNED-input dep to
/// `D <path> td-recipe-output` (so build_plan substitutes td's build of D); non-owned
/// lines pass through. Every owned dep must appear in the lock — else the recipe
/// declares an input its lock doesn't carry, and we refuse rather than drop the edge.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn auto_chained_lock(base_lock_text: &str, owned_deps: &[String]) -> Result<String, String> {
    let mut out = String::new();
    let mut marked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for line in base_lock_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let mut toks = trimmed.split_whitespace();
        let first = toks.next().unwrap_or("");
        let path = toks.next().unwrap_or("");
        let class = toks.next().unwrap_or("");
        // A `source`-class line is the recipe's OWN source, NEVER a rung dependency — so it
        // must pass through unchanged. Without this, a rung that reuses another rung's source
        // TARBALL (e.g. the x86_64 cross rungs build the i686 gcc-14/binutils-2.44 source and
        // ALSO depend on those rungs) mis-fires auto_entry_is_dep: the source path basename
        // `<hash>-binutils-244-source` matches the dep `binutils-244` via its `starts_with`
        // prefix rule, re-keying the source line away and leaving the recipe with no source.
        let dep = if class == "source" {
            None
        } else {
            owned_deps.iter().find(|d| auto_entry_is_dep(first, path, d))
        };
        match dep {
            Some(d) => {
                out.push_str(&format!("{d} {path} td-recipe-output\n"));
                marked.insert(d.clone());
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    for d in owned_deps {
        if !marked.contains(d) {
            return Err(format!("--auto: owned input `{d}' not found in the lock"));
        }
    }
    Ok(out)
}

/// build-plan --auto: GENERATE the plan from the recipe GRAPH, then run it. Given a
/// TARGET recipe spec, recursively resolve every declared input that is itself an owned
/// recipe (RECIPE-DIR/<name>.json + LOCK-DIR/<name>-no-guix.lock both exist), topo-sort,
/// emit a per-recipe chained lock marking those owned deps `td-recipe-output`, and feed
/// the generated plan to build_plan. No hand-written plan or manifest — a recipe's edges
/// chain automatically as the owned set grows.
///
/// Usage: build-plan --auto TARGET RECIPE-DIR LOCK-DIR GUIX-STORE SCRATCH
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_plan_auto(
    target: &str,
    recipe_dir: &str,
    lock_dir: &str,
    guix_store: &str,
    scratch: &Path,
    builder_store: Option<(&str, &str, &str)>,
) -> Result<(), String> {
    if !auto_is_owned(recipe_dir, lock_dir, target) {
        return Err(format!(
            "--auto target `{target}': need {recipe_dir}/{target}.json and {lock_dir}/{target}-no-guix.lock"
        ));
    }
    std::fs::create_dir_all(scratch).map_err(|e| e.to_string())?;
    let mut order: Vec<String> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    auto_topo(recipe_dir, lock_dir, target, &mut order, &mut seen, &mut stack)?;
    eprintln!(
        "td-builder: build-plan --auto {target}: derived a {}-step plan from the recipe graph: {}",
        order.len(),
        order.join(" -> ")
    );
    let mut plan = String::new();
    for name in &order {
        let owned: Vec<String> = auto_inputs(recipe_dir, name)?
            .into_iter()
            .filter(|i| auto_is_owned(recipe_dir, lock_dir, i))
            .collect();
        let base = std::fs::read_to_string(format!("{lock_dir}/{name}-no-guix.lock"))
            .map_err(|e| format!("read lock for {name}: {e}"))?;
        let chained = auto_chained_lock(&base, &owned)?;
        let lock_path = scratch.join(format!("{name}-auto.lock"));
        std::fs::write(&lock_path, &chained).map_err(|e| e.to_string())?;
        plan.push_str(&format!(
            "step {recipe_dir}/{name}.json {}\n",
            lock_path.to_string_lossy()
        ));
    }
    let plan_path = scratch.join("auto.plan");
    std::fs::write(&plan_path, &plan).map_err(|e| e.to_string())?;
    build_plan(&plan_path.to_string_lossy(), guix_store, scratch, builder_store)
}

/// Emit PKG's recipe JSON from td's Rust catalog via `td-recipe-eval emit` — the
/// dependency-free evaluator (recipes/), set in TD_RECIPE_EVAL by the caller (placed,
/// td-built). This REPLACES the old `tsgo`+`td-ts-eval` `.ts` emit (the TypeScript
/// recipe surface was deleted in rust-recipe-surface, #224); `tests/recipe-emit.sh`
/// is the shell sibling of this call. td-recipe-eval `die`s with a non-zero exit on an
/// unknown stem, which we surface as the loud "no td recipe for PKG" error — td shell
/// resolves PKG to a td recipe or fails; it never falls back to guix.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn emit_recipe_json(pkg: &str) -> Result<String, String> {
    let eval = std::env::var("TD_RECIPE_EVAL").map_err(|_| {
        "TD_RECIPE_EVAL must point at td's td-recipe-eval binary (the Rust recipe catalog evaluator)"
            .to_string()
    })?;
    let out = Command::new(&eval)
        .args(["emit", pkg])
        .output()
        .map_err(|e| format!("spawn td-recipe-eval ({eval}): {e}"))?;
    if !out.status.success() {
        // Unknown stem (or any emit failure) ⇒ loud error, NOT a guix fallback. Keep the
        // "no td recipe for" phrasing the td-shell gate's load-bearing leg asserts on.
        return Err(format!(
            "no td recipe for `{pkg}' — td shell builds td packages (the recipes/ catalog via td-recipe-eval), it does not fall back to guix: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("td-recipe-eval output not UTF-8: {e}"))
}

/// The pre-provisioned NATIVE `/td/store` toolchain `td shell` builds the Rust userland with,
/// handed in via the `TD_SHELL_NATIVE_*` environment (gate `td-shell-userland` / host-prep
/// stages it: fetch-or-build the native x86_64 gcc/binutils/glibc + relinked rust, stage a
/// combined seed+native store, and expose it). When present, a vendored rust build (ripgrep,
/// fd, …) links this toolchain — never the guix rust/gcc-toolchain: the seed lock is retargeted
/// (`native_seed_lock_body`), the combined store is the build's STORE-DIR + `TD_SEED_STORE`, the
/// native store's db rides `TD_EXTRA_DBS`, and `TD_RUST_STORE_{INTERP,RPATH,BDIR}` put run_rust
/// in native link mode. Absence is fine for a plain seed package (hello); a *vendored rust* build
/// with no native toolchain provisioned is a hard error (no guix-rust fallback — the cutover).
struct NativeToolchain {
    /// The combined seed+native store dir (guix build seed + the `/td/store` toolchain trees):
    /// the build's STORE-DIR and `TD_SEED_STORE`.
    store: String,
    /// A placeholder `TD_SEED_DB` companion (the engine content-scans `store`; this is the legacy
    /// set-together companion, never `/var/guix/db`).
    seed_db: String,
    /// The native toolchain's own td store db (its `/td/store` outputs + refs) → `TD_EXTRA_DBS`.
    extra_dbs: String,
    /// Native link mode: the `/td/store` glibc loader, RUNPATH, and `-B` dir baked by run_rust.
    interp: String,
    rpath: String,
    bdir: String,
    /// The native toolchain lock lines (`…-x86_64-store-native /td/store/<rel> seed`, one per
    /// line) appended to the retargeted seed lock.
    lock_lines: String,
}

impl NativeToolchain {
    /// Read the `TD_SHELL_NATIVE_*` env. `Ok(None)` when unset (no native toolchain provisioned);
    /// `Err` when partially set (a provisioning bug we surface loudly rather than silently
    /// falling back to guix). `TD_SHELL_NATIVE_LOCK` names a file with the native lock lines.
    fn from_env() -> Result<Option<NativeToolchain>, String> {
        let store = match std::env::var("TD_SHELL_NATIVE_STORE") {
            Ok(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };
        let get = |k: &str| -> Result<String, String> {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("TD_SHELL_NATIVE_STORE is set but {k} is not (native-toolchain provisioning is incomplete)"))
        };
        let seed_db = get("TD_SHELL_NATIVE_DB")?;
        let extra_dbs = get("TD_SHELL_NATIVE_EXTRA_DBS")?;
        let interp = get("TD_SHELL_NATIVE_INTERP")?;
        let rpath = get("TD_SHELL_NATIVE_RPATH")?;
        let bdir = get("TD_SHELL_NATIVE_BDIR")?;
        let lock_file = get("TD_SHELL_NATIVE_LOCK")?;
        let lock_lines = std::fs::read_to_string(&lock_file)
            .map_err(|e| format!("read TD_SHELL_NATIVE_LOCK {lock_file}: {e}"))?;
        Ok(Some(NativeToolchain {
            store,
            seed_db,
            extra_dbs,
            interp,
            rpath,
            bdir,
            lock_lines,
        }))
    }
}

/// td-builder shell — run a command with td-BUILT packages on PATH. td's own
/// `guix shell`, but with NO guix anywhere: each PKG is resolved to a td RECIPE and
/// BUILT by td-builder itself (the recipe → `td-builder build-recipe`, whose
/// content-addressed cache makes this build-on-demand + cached), then td composes
/// the command's PATH from the td store OUTPUT and execs. There is no `guix`
/// process in the resolve/build/exec path; an unknown package errors ("no td recipe
/// for PKG"), it does NOT fall back to guix. The package that lands on PATH is td's
/// own build at td's own store path. A vendored rust build (ripgrep, fd, …) links the
/// NATIVE `/td/store` toolchain provisioned via `TD_SHELL_NATIVE_*` (see `NativeToolchain`),
/// never the guix rust/gcc-toolchain — that path is retired for `td shell`; a plain seed
/// package (hello) still links the pinned build seed from its `<pkg>-no-guix.lock`.
///
/// Config (env): TD_RECIPE_EVAL (td's Rust recipe-catalog evaluator, to emit the
/// recipe), TD_SHELL_LOCKS (dir of `<pkg>.lock` / `<pkg>-no-guix.lock`, default `tests`),
/// TD_SHELL_STORE_DB (store DB for the plain seed-package path, default `/var/guix/db/db.sqlite`),
/// TD_SHELL_NATIVE_* (the pre-provisioned native `/td/store` toolchain for vendored rust builds —
/// `NativeToolchain::from_env`), TD_SHELL_CACHE (build cache root, default `$HOME/.cache/td-shell`),
/// TD_BUILDER_PATH/STORE/DB (optional stage0 builder override, so the build's builder
/// is td-placed too).
///
/// Usage: shell PKG... [-- CMD ARGS...]
///   PKG...      td package names (a recipe must exist; no guix fallback)
///   -- CMD...   the command to run in the composed env; omitted → interactive $SHELL
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn run_shell(rest: &[String]) -> Result<std::process::ExitStatus, String> {
    // Everything before the first `--` is a package name; after it, the command.
    let sep = rest.iter().position(|a| a == "--");
    let (pkgs, cmd): (&[String], &[String]) = match sep {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[]),
    };

    let env_or = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let lock_dir = env_or("TD_SHELL_LOCKS", "tests");
    let store_db = env_or("TD_SHELL_STORE_DB", "/var/guix/db/db.sqlite");
    let cache = match std::env::var("TD_SHELL_CACHE") {
        Ok(c) => c,
        Err(_) => format!(
            "{}/.cache/td-shell",
            std::env::var("HOME").map_err(|_| "set TD_SHELL_CACHE or HOME".to_string())?
        ),
    };
    let self_exe = std::env::current_exe()
        .map_err(|e| format!("locate td-builder: {e}"))?
        .to_string_lossy()
        .into_owned();
    // The pre-provisioned NATIVE /td/store toolchain (TD_SHELL_NATIVE_*), if any. When present, a
    // vendored rust build links it instead of the guix rust/gcc-toolchain (the `td shell` cutover).
    let native = NativeToolchain::from_env()?;

    // Build each named package with td-builder itself — no guix — and collect the
    // td store output's bin/sbin dirs to put on PATH.
    let mut prefix_dirs: Vec<String> = Vec::new();
    for pkg in pkgs {
        // Resolve PKG to a td recipe via the Rust catalog (td-recipe-eval) and emit its
        // JSON FIRST — an unknown PKG ⇒ loud "no td recipe" error, NOT a guix fallback.
        // (Resolve before the lock check so an unknown package reports "no td recipe", the
        // load-bearing leg the td-shell gate asserts; a known pkg then needs its lock.)
        let recipe_json = emit_recipe_json(pkg)?;
        // Stage the recipe JSON in the per-package cache dir that build-recipe also keys
        // its build cache on.
        let sd = format!("{cache}/{pkg}");
        std::fs::create_dir_all(&sd).map_err(|e| e.to_string())?;
        let json_file = format!("{sd}/recipe.json");
        std::fs::write(&json_file, &recipe_json).map_err(|e| e.to_string())?;
        // Assemble the build-recipe argv. A rust userland recipe (ripgrep/fd/…) needs its
        // whole crate closure provisioned GUIX-FREE: td interns the warmed source + crate
        // set and feeds build-recipe's 11-arg form (TD_VENDOR_DIR), exactly as the
        // crate-free corpus gates do — but here from the real `td shell` product command,
        // not a bespoke harness. A seed package (hello) has no warmed closure ⇒ the plain
        // 4-arg path on its `<pkg>-no-guix.lock`.
        let mut bargs: Vec<String> = vec!["build-recipe".into(), json_file.clone()];
        // A vendored rust build links the native /td/store toolchain when it is provisioned; a
        // plain seed package (hello) never does. Track it so the build-recipe subprocess gets the
        // native env (TD_SEED_STORE/TD_EXTRA_DBS/TD_RUST_STORE_*) only for the vendored-rust case.
        let mut used_native = false;
        match provision_rust_inputs(pkg, &lock_dir, &sd, &self_exe)? {
            Some((seedlock, extra)) => {
                match &native {
                    // CUTOVER: retarget the seed lock onto the native /td/store toolchain (drop the
                    // guix rust/gcc-toolchain lines) and build against the combined seed+native
                    // store — never the guix rust/gcc-toolchain. This is the `td shell` product
                    // command building the Rust userland with td's OWN toolchain.
                    Some(nt) => {
                        let body = std::fs::read_to_string(&seedlock)
                            .map_err(|e| format!("read seed lock {seedlock}: {e}"))?;
                        let retargeted = native_seed_lock_body(&body, &nt.lock_lines);
                        std::fs::write(&seedlock, &retargeted)
                            .map_err(|e| format!("write seed lock {seedlock}: {e}"))?;
                        bargs.push(seedlock);
                        bargs.push(sd.clone());
                        bargs.push(nt.store.clone());
                        used_native = true;
                    }
                    // No native toolchain provisioned ⇒ this vendored rust build has no toolchain.
                    // The guix rust/gcc-toolchain path is RETIRED for `td shell` (the cutover): fail
                    // loudly rather than silently fall back to guix.
                    None => {
                        return Err(format!(
                            "build `{pkg}': a vendored rust build needs the native /td/store toolchain, \
                             but TD_SHELL_NATIVE_STORE is not set. Provision it (gate `td-shell-userland' \
                             or host-prep stages the native gcc/binutils/glibc + relinked rust). \
                             The guix rust/gcc-toolchain path is retired for `td shell'."
                        ));
                    }
                }
                bargs.extend(extra);
            }
            None => {
                // PKG needs a lock (its pinned toolchain seed). No lock ⇒ loud error.
                let lock = format!("{lock_dir}/{pkg}-no-guix.lock");
                if !Path::new(&lock).is_file() {
                    return Err(format!("no lock for `{pkg}' ({lock} not found)"));
                }
                bargs.push(lock);
                bargs.push(sd.clone());
                bargs.push(store_db.clone());
            }
        }
        // BUILD it via the build-recipe subcommand (its content-addressed cache makes
        // an unchanged recipe a HIT — build-on-demand + cached). A subprocess keeps the
        // build's chatter off the command's stdout, and rides the inherited
        // TD_BUILDER_* override so the builder is the td-placed stage0 too.
        let mut build = Command::new(&self_exe);
        build.args(&bargs);
        if used_native {
            // Native link mode, exactly as `tests/rust-x86_64-userland-store-native.sh` sets it:
            // the combined store is content-scanned for the closure, the native toolchain's own db
            // adds its /td/store refs, and run_rust bakes the /td/store interp/RUNPATH/-B.
            if let Some(nt) = &native {
                build
                    .env("TD_SEED_STORE", &nt.store)
                    .env("TD_SEED_DB", &nt.seed_db)
                    .env("TD_EXTRA_DBS", &nt.extra_dbs)
                    .env("TD_RUST_STORE_INTERP", &nt.interp)
                    .env("TD_RUST_STORE_RPATH", &nt.rpath)
                    .env("TD_RUST_STORE_BDIR", &nt.bdir);
            }
        }
        let out = build
            .output()
            .map_err(|e| format!("build `{pkg}': spawn td-builder build-recipe: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "build `{pkg}' failed:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        // build-recipe prints `OUT=out <canonical-store-path>`; the realized tree is
        // staged at <sd>/newstore/<basename> (a td path distinct from guix's).
        let outline = String::from_utf8_lossy(&out.stdout);
        let canonical = outline
            .lines()
            .find_map(|l| l.strip_prefix("OUT=out "))
            .ok_or_else(|| format!("build `{pkg}': build-recipe reported no `out' output"))?
            .trim();
        let base = canonical.rsplit('/').next().unwrap_or(canonical);
        let outdir = format!("{sd}/newstore/{base}");
        let mut any = false;
        for sub in ["bin", "sbin"] {
            let dir = format!("{outdir}/{sub}");
            if Path::new(&dir).is_dir() {
                prefix_dirs.push(dir);
                any = true;
            }
        }
        if !any {
            return Err(format!("build `{pkg}': td output {outdir} has no bin/sbin"));
        }
    }

    // Compose the child PATH ourselves: the td package bins FIRST (so the
    // package's binary wins — the package is load-bearing), then the inherited
    // PATH (guix shell's non-pure default). td builds this string; no guix
    // process is between us and the command.
    let inherited = std::env::var("PATH").unwrap_or_default();
    let mut path = prefix_dirs.join(":");
    if !inherited.is_empty() {
        if !path.is_empty() {
            path.push(':');
        }
        path.push_str(&inherited);
    }

    // Explicit `-- CMD…`, else drop into an interactive $SHELL (fallback /bin/sh).
    let shell;
    let (prog, prog_args): (&str, &[String]) = if let Some((first, args)) = cmd.split_first() {
        (first.as_str(), args)
    } else {
        shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        (shell.as_str(), &[])
    };

    Command::new(prog)
        .args(prog_args)
        .env("PATH", &path)
        .status()
        .map_err(|e| format!("run `{prog}': {e}"))
}

/// Intern a source TREE into a td-OWNED store with td's OWN recursive add-to-store
/// (`store-add-recursive`) — no `guix repl`, no guix-daemon. The shell sibling is
/// `tests/intern-src.sh`. Returns the content-addressed `source` store path td computed
/// from the tree's recursive NAR sha256 and restored under `store_dir` (+ `db`).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn run_store_add(
    self_exe: &str,
    name: &str,
    tree: &Path,
    store_dir: &Path,
    db: &Path,
) -> Result<String, String> {
    std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
    let out = Command::new(self_exe)
        .args([
            "store-add-recursive",
            name,
            &tree.to_string_lossy(),
            &store_dir.to_string_lossy(),
            &db.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("spawn store-add-recursive {name}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "store-add-recursive {name} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return Err(format!("store-add-recursive {name} produced no path"));
    }
    Ok(path)
}

/// Build the seed-lock body for a vendored rust build: the package lock with any `.crate`
/// FOD line and any stale `<sourcekey> …` line dropped, then the td-interned source pinned
/// as `<sourcekey> <src_canonical>`. Pure (no I/O) so the line filtering is unit-tested
/// directly — the same transform `tests/crate-free-build.sh` does with grep/echo.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn seed_lock_body(lock_body: &str, sourcekey: &str, src_canonical: &str) -> String {
    let keypfx = format!("{sourcekey} ");
    let mut seed = String::new();
    for line in lock_body.lines() {
        if line.contains(".crate ") || line.starts_with(&keypfx) {
            continue;
        }
        seed.push_str(line);
        seed.push('\n');
    }
    seed.push_str(&format!("{sourcekey} {src_canonical}\n"));
    seed
}

/// Retarget a rust seed lock onto the NATIVE `/td/store` toolchain: drop every guix rust /
/// gcc-toolchain seed line (a `/gnu/store/…-rust-…` or `…-gcc-toolchain-…` entry), keep the
/// retired-last build seed (coreutils/bash/tar/gzip) and the interned-source line, then append
/// `native_lines` (the `/td/store` gcc/binutils/glibc + relinked-rust lock lines the gate
/// pre-provisioned). Pure (no I/O) so it is unit-tested directly — the same transform
/// `tests/rust-x86_64-userland-store-native.sh` does with `grep -vE -- '-rust-|-gcc-toolchain-'`.
/// This is the `td shell` cutover: the product command builds the Rust userland with td's OWN
/// toolchain, never the guix rust/gcc-toolchain.
fn native_seed_lock_body(seed_body: &str, native_lines: &str) -> String {
    let mut out = String::new();
    for line in seed_body.lines() {
        // Only a `/gnu/store/` toolchain line is dropped; the native `/td/store` lines and the
        // non-toolchain seed (coreutils/bash/tar/gzip, no `-rust-`/`-gcc-toolchain-`) survive.
        let is_guix_toolchain = line.contains("/gnu/store/")
            && (line.contains("-rust-") || line.contains("-gcc-toolchain-"));
        if is_guix_toolchain {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let native = native_lines.trim_end_matches('\n');
    if !native.is_empty() {
        out.push_str(native);
        out.push('\n');
    }
    out
}

/// Provision a rust recipe's crate closure for `td shell`, GUIX-FREE — the product-command
/// counterpart of the `tests/crate-free-build.sh` corpus harness, so `td shell ripgrep -- rg
/// …` builds the REAL shipped userland the way a user types it (not a bespoke gate script).
///
/// Source of the crates: a warmed tree at `$TD_SHELL_VENDOR_ROOT/<pkg>/{src/<one>,vendor}`
/// (host PREP via `td-feed warm crate`, the cargo-proxy having verified each `.crate`
/// sha256 == the crates.io sparse-index cksum — the upstream pin, NOT a guix artifact).
/// This:
///   - clean-copies the source tree (dropping `target`/`vendor`/`.cargo` so a stray local
///     build cannot perturb the source hash) and interns it with `store-add-recursive`,
///   - interns the crate SET the same way (a no-ref content-addressed tree),
///   - writes a seed lock = the package's `<pkg>.lock` minus any `.crate`/source-key line,
///     plus `<pkg>-source <interned-src>`,
/// and returns `(seed-lock-path, [src-store, src-db, vendor-canonical, vendor-store,
/// vendor-db])` — the extra positional args build-recipe's 11-arg form takes.
///
/// Returns `Ok(None)` when no warmed closure exists for PKG (`TD_SHELL_VENDOR_ROOT` unset,
/// or no `<pkg>/vendor` under it) ⇒ the caller uses the plain seed-package path (e.g. hello).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn provision_rust_inputs(
    pkg: &str,
    lock_dir: &str,
    sd: &str,
    self_exe: &str,
) -> Result<Option<(String, [String; 5])>, String> {
    let vendor_root = match std::env::var("TD_SHELL_VENDOR_ROOT") {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(None),
    };
    let pkg_root = Path::new(&vendor_root).join(pkg);
    let vendor = pkg_root.join("vendor");
    let src_parent = pkg_root.join("src");
    // No warmed crate closure for this package here ⇒ not a vendored rust build.
    if !vendor.is_dir() || !src_parent.is_dir() {
        return Ok(None);
    }

    // The single extracted source tree under src/ (e.g. ripgrep-14.1.1, fd-find-10.2.0):
    // glob it so td shell needs no per-package crate-name table.
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&src_parent)
        .map_err(|e| format!("read {}: {e}", src_parent.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();
    let srctree = match subdirs.as_slice() {
        [one] => one.clone(),
        [] => {
            return Err(format!(
                "warmed crate closure for `{pkg}' has no source tree under {} — re-run `td-feed warm crate'",
                src_parent.display()
            ))
        }
        _ => {
            return Err(format!(
                "warmed crate closure for `{pkg}' has multiple source trees under {} (expected exactly one)",
                src_parent.display()
            ))
        }
    };
    if !srctree.join("Cargo.toml").is_file() {
        return Err(format!(
            "source tree {} ships no Cargo.toml — re-run `td-feed warm crate'",
            srctree.display()
        ));
    }
    let ncrate = std::fs::read_dir(&vendor)
        .map_err(|e| format!("read {}: {e}", vendor.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "crate"))
        .count();
    if ncrate == 0 {
        return Err(format!(
            "no `.crate' files under {} — re-run `td-feed warm crate'",
            vendor.display()
        ));
    }

    let work = Path::new(sd);
    // --- intern the source tree (clean-copy dropping the build dirs) ---
    let clean = work.join("srcclean");
    let _ = std::fs::remove_dir_all(&clean);
    std::fs::create_dir_all(&clean).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(&srctree).map_err(|e| format!("read {}: {e}", srctree.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n == "target" || n == "vendor" || n == ".cargo" {
            continue;
        }
        copy_canonical(&entry.path(), &clean.join(&name))?;
    }
    let src_store = work.join("srcstore");
    let src_db = work.join("src.db");
    let _ = std::fs::remove_dir_all(&src_store);
    let _ = std::fs::remove_file(&src_db);
    let src_canonical = run_store_add(self_exe, &format!("{pkg}-src"), &clean, &src_store, &src_db)?;

    // --- intern the crate set ---
    let vendor_store = work.join("vendorstore");
    let vendor_db = work.join("vendor.db");
    let _ = std::fs::remove_dir_all(&vendor_store);
    let _ = std::fs::remove_file(&vendor_db);
    let vendor_canonical =
        run_store_add(self_exe, &format!("{pkg}-vendor"), &vendor, &vendor_store, &vendor_db)?;

    // --- seed lock: the package lock minus crate/source-key lines, + the interned source ---
    let lock = format!("{lock_dir}/{pkg}.lock");
    if !Path::new(&lock).is_file() {
        return Err(format!("no lock for `{pkg}' ({lock} not found)"));
    }
    let body = std::fs::read_to_string(&lock).map_err(|e| format!("read {lock}: {e}"))?;
    let sourcekey = format!("{pkg}-source");
    let seed = seed_lock_body(&body, &sourcekey, &src_canonical);
    let seedlock = work.join("seed.lock");
    std::fs::write(&seedlock, &seed).map_err(|e| format!("write {}: {e}", seedlock.display()))?;

    Ok(Some((
        seedlock.to_string_lossy().into_owned(),
        [
            src_store.to_string_lossy().into_owned(),
            src_db.to_string_lossy().into_owned(),
            vendor_canonical,
            vendor_store.to_string_lossy().into_owned(),
            vendor_db.to_string_lossy().into_owned(),
        ],
    )))
}

/// td-builder profile — build a PROFILE: a symlink tree unioning the `bin`/`sbin` of a
/// set of installed package outputs, the user-package-manager profile layer (like a guix
/// profile / nix env). PROFILE-DIR is rebuilt fresh; for each PKG-OUT (a store output dir,
/// e.g. `~/.td/store/<hash>-hello`), every entry under `bin`/`sbin` is symlinked into
/// PROFILE-DIR/{bin,sbin}, pointing at the absolute store path. A user puts PROFILE-DIR/bin
/// on PATH (or symlinks `~/bin/<tool>` → PROFILE-DIR/bin/<tool>). A name provided by two
/// packages is a COLLISION (error — explicit, like guix). The symlinks resolve to the
/// store, so the profile is a thin, GC-friendly view that swaps atomically when rebuilt.
///
/// Usage: profile PROFILE-DIR PKG-OUT...
/// Union the bin/sbin of each package output into a symlink-tree profile. When
/// `store_native_prefix` is `Some(prefix)`, the symlink TARGETS are the LOGICAL store paths
/// (`<prefix>/<basename(pkg)>/<sub>/<entry>`) rather than the physical PKG-OUT path passed in
/// — so the profile resolves inside a store-ns own-root where `prefix` (e.g. `/td/store`) is
/// the bound store but the physical scratch dir is absent. `None` keeps the thin-view behavior
/// (link straight at PKG-OUT as given). Enumeration always reads the physical PKG-OUT dir.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn build_profile(
    profile_dir: &str,
    pkgs: &[String],
    store_native_prefix: Option<&str>,
) -> Result<usize, String> {
    use std::os::unix::fs::symlink;
    let pdir = Path::new(profile_dir);
    // Rebuild fresh (idempotent) — a profile is a derived view, not state.
    if pdir.exists() {
        std::fs::remove_dir_all(pdir).map_err(|e| format!("clear {profile_dir}: {e}"))?;
    }
    std::fs::create_dir_all(pdir).map_err(|e| e.to_string())?;
    let mut linked = 0usize;
    for pkg in pkgs {
        let pkgp = Path::new(pkg);
        if !pkgp.is_dir() {
            return Err(format!("package output `{pkg}' is not a directory"));
        }
        for sub in ["bin", "sbin"] {
            let src_dir = pkgp.join(sub);
            if !src_dir.is_dir() {
                continue;
            }
            let dst_dir = pdir.join(sub);
            std::fs::create_dir_all(&dst_dir).map_err(|e| e.to_string())?;
            let mut entries: Vec<_> = std::fs::read_dir(&src_dir)
                .map_err(|e| format!("read {}: {e}", src_dir.display()))?
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            entries.sort_by_key(|e| e.file_name());
            for ent in entries {
                let dst = dst_dir.join(ent.file_name());
                // symlink_metadata (lexists), not exists(): a store-native link is a LOGICAL
                // path that dangles on the host, so exists() would follow it and miss the clash.
                if dst.symlink_metadata().is_ok() {
                    return Err(format!(
                        "profile collision: `{sub}/{}' is provided by more than one package (last: {pkg})",
                        ent.file_name().to_string_lossy()
                    ));
                }
                // Absolute symlink INTO the store (so the profile is a thin view). In
                // store-native mode, retarget to the LOGICAL store path so it resolves in
                // the own-root; otherwise link straight at the physical PKG-OUT entry.
                let target = match store_native_prefix {
                    Some(prefix) => {
                        let base = pkgp
                            .file_name()
                            .ok_or_else(|| format!("package `{pkg}' has no basename"))?;
                        Path::new(prefix).join(base).join(sub).join(ent.file_name())
                    }
                    None => ent.path(),
                };
                symlink(&target, &dst)
                    .map_err(|e| format!("symlink {} -> {}: {e}", dst.display(), target.display()))?;
                linked += 1;
            }
        }
    }
    if linked == 0 {
        return Err("no bin/sbin entries in any package — refusing to write an empty profile".into());
    }
    Ok(linked)
}

/// Replace every occurrence of `from` with `to` (SAME length — size-preserving, so ELF
/// offsets/section sizes are untouched and the substitution is binary-safe). Used to
/// relocate `/gnu/store` → `/td//store` (both 10 bytes).
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn replace_bytes_same_len(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    assert_eq!(from.len(), to.len(), "relocation substitution must be size-preserving");
    let mut out = data.to_vec();
    let mut i = 0;
    while i + from.len() <= out.len() {
        if &out[i..i + from.len()] == from {
            out[i..i + from.len()].copy_from_slice(to);
            i += from.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Rewrite `/gnu/store` → `/td//store` in every file's CONTENT and every symlink TARGET
/// under `dir`. Size-preserving (10→10 bytes; `/td//store` is the kernel-collapsed form of
/// `/td/store`), so RUNPATH/interpreter in `.dynstr`, embedded paths in `.rodata`, and
/// scripts are all handled by one byte substitution — no ELF surgery. Returns the count.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn relocate_tree(dir: &Path, from: &[u8], to: &[u8]) -> Result<usize, String> {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let md = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let ft = md.file_type();
    let mut n = 0;
    if ft.is_symlink() {
        // Store-path symlink targets are ASCII; rewrite the prefix textually.
        let target = std::fs::read_link(dir).map_err(|e| e.to_string())?;
        let ts = target.to_string_lossy();
        let from_s = std::str::from_utf8(from).unwrap();
        let to_s = std::str::from_utf8(to).unwrap();
        if ts.contains(from_s) {
            let new = ts.replace(from_s, to_s);
            std::fs::remove_file(dir).map_err(|e| e.to_string())?;
            symlink(&new, dir).map_err(|e| e.to_string())?;
            n += 1;
        }
    } else if ft.is_dir() {
        for e in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            n += relocate_tree(&e.map_err(|e| e.to_string())?.path(), from, to)?;
        }
    } else if ft.is_file() {
        let data = std::fs::read(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if data.windows(from.len()).any(|w| w == from) {
            let mode = md.permissions().mode();
            let mut w = md.permissions();
            w.set_mode(mode | 0o200); // make writable for the rewrite
            std::fs::set_permissions(dir, w).ok();
            std::fs::write(dir, replace_bytes_same_len(&data, from, to))
                .map_err(|e| format!("{}: {e}", dir.display()))?;
            let mut back = std::fs::metadata(dir).map_err(|e| e.to_string())?.permissions();
            back.set_mode(mode); // restore the canonical (read-only) mode
            std::fs::set_permissions(dir, back).ok();
            n += 1;
        }
    }
    Ok(n)
}

/// Relocate ROOT's closure (over STORE-DB) from guix's `/gnu/store` into DEST-DIR as a
/// td-prefixed store: copy each closure member to `DEST-DIR/<base>` and rewrite every
/// `/gnu/store` reference to `/td//store` (the length-preserving form of `/td/store`, so
/// the substitution is binary-safe — no patchelf). DEST-DIR bound at `/td/store` (store-ns)
/// IS the relocated store; binaries' `/td//store` refs resolve into it, with NO `/gnu/store`.
/// The one-time break from guix (seed captured once, relocated, then td builds/runs from
/// `/td/store`). Usage: store-relocate STORE-DB ROOT DEST-DIR
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn relocate_closure(store_db: &str, root: &str, dest: &str) -> Result<(usize, usize), String> {
    let bytes = std::fs::read(store_db).map_err(|e| format!("read store db {store_db}: {e}"))?;
    let db = store_db_read::Db::open(bytes)?;
    let closure = db.closure(root)?;
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let (from, to): (&[u8], &[u8]) = (b"/gnu/store", b"/td//store");
    let mut files = 0usize;
    for p in &closure {
        let base = p.rsplit('/').next().ok_or_else(|| format!("{p}: not a store path"))?;
        let dst = Path::new(dest).join(base);
        if !dst.exists() {
            copy_canonical(Path::new(p), &dst)?;
            files += relocate_tree(&dst, from, to)?;
        }
    }
    Ok((closure.len(), files))
}

/// The nice value build work runs at, from `TD_BUILD_NICE` (default 10). Parsed
/// from the raw env value so the policy is unit-testable without touching real
/// process state. Clamped to the kernel's -20..=19 range; a missing/garbage value
/// falls back to the default.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn parse_build_nice(raw: Option<String>) -> i32 {
    raw.and_then(|v| v.trim().parse::<i32>().ok()).unwrap_or(10).clamp(-20, 19)
}

/// Raise THIS process's niceness so the compilers/`make` it spawns (which inherit
/// the value at fork) yield CPU to anything interactive sharing the host — a
/// desktop/compositor stays responsive during a build storm. Best-effort and
/// increase-only: the kernel rejects an unprivileged DEcrease with EPERM, which
/// just means we were already at least this nice, so we ignore the result. Purely
/// a scheduling knob — build OUTPUT (and thus reproducibility) is unaffected.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn nice_self_for_builds() {
    let _ = sys::set_self_priority(parse_build_nice(std::env::var("TD_BUILD_NICE").ok()));
}

/// Parse an `oci-image`/`oci-image-closure` CONFIG-JSON ({"repoTag","env","entrypoint",
/// "cmd"}, all optional; repoTag defaults to td:latest) into an `oci::ImageConfig`.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn image_config_from_json(cj: &json::Json) -> oci::ImageConfig {
    let strs = |key: &str| -> Vec<String> {
        cj.get(key)
            .and_then(json::Json::as_arr)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    oci::ImageConfig {
        repo_tag: cj.get("repoTag").and_then(json::Json::as_str).unwrap_or("td:latest").to_string(),
        env: strs("env"),
        entrypoint: strs("entrypoint"),
        cmd: strs("cmd"),
    }
}

/// Parsed `host-sandbox` invocation (the loop container). Pure data so the flag
/// grammar is unit-testable without touching namespaces.
#[derive(Debug)]
struct HostSandboxArgs {
    expose_cwd: bool,
    /// `--store-from DIR`: bind DIR (an unpacked store, e.g. a captured seed or the
    /// `/td/store` harness) instead of the host `/gnu/store`.
    store_from: Option<String>,
    /// `--store-at DEST`: the in-sandbox mount point for `--store-from`. Defaults to
    /// `/gnu/store` (a guix-captured seed's binaries hardcode that interpreter path);
    /// pass `/td/store` for td's own store-native harness (interp relinked to
    /// `/td/store/ld`). Only meaningful with `--store-from`; when DEST != `/gnu/store`
    /// the host `/gnu/store` is NOT bound at all — the guix-byte-free VM substrate.
    store_at: Option<String>,
    /// `--no-daemon`: do not bind `/var/guix` (no guix-daemon socket / GC roots).
    no_daemon: bool,
    cmd: String,
    cmd_args: Vec<String>,
}

/// Parse the full `td-builder host-sandbox …` argv (args[0]=prog, args[1]=subcommand,
/// flags…, `--`, CMD, CMD-ARGS…). Returns the parsed form or a user-facing message.
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn parse_host_sandbox_args(args: &[String]) -> Result<HostSandboxArgs, String> {
    let mut i = 2usize;
    let mut expose_cwd = false;
    let mut store_from: Option<String> = None;
    let mut store_at: Option<String> = None;
    let mut no_daemon = false;
    while i < args.len() && args[i] != "--" {
        match args[i].as_str() {
            "--expose-cwd" => expose_cwd = true,
            "--no-daemon" => no_daemon = true,
            "--store-from" => {
                i += 1;
                if i >= args.len() || args[i] == "--" {
                    return Err("--store-from needs a DIR".to_string());
                }
                store_from = Some(args[i].clone());
            }
            "--store-at" => {
                i += 1;
                if i >= args.len() || args[i] == "--" {
                    return Err("--store-at needs a DIR".to_string());
                }
                store_at = Some(args[i].clone());
            }
            other => return Err(format!("unknown flag `{other}'")),
        }
        i += 1;
    }
    if store_at.is_some() && store_from.is_none() {
        return Err("--store-at requires --store-from".to_string());
    }
    // args[i] is now "--" (or we ran off the end); the command follows it.
    if i >= args.len() || i + 1 >= args.len() {
        return Err("usage: td-builder host-sandbox [--expose-cwd] [--store-from DIR [--store-at DEST]] [--no-daemon] -- CMD ARGS...".to_string());
    }
    Ok(HostSandboxArgs {
        expose_cwd,
        store_from,
        store_at,
        no_daemon,
        cmd: args[i + 1].clone(),
        cmd_args: args[i + 2..].to_vec(),
    })
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Builds run nicer than the loop's other work so a shared desktop stays smooth.
    // Scope to the build-executing subcommands; their spawned compilers inherit it.
    if matches!(args.get(1).map(String::as_str), Some("build" | "realize" | "autotools-build")) {
        nice_self_for_builds();
    }
    match args.get(1).map(String::as_str) {
        // S1 sentinel — the rung's run leg greps for this exact line.
        None => {
            println!("td-builder {} ok", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        // affected-checks — port of tools/affected-checks.sh (rust-migration C1):
        // map the branch diff to a right-sized check set + the waive/escalate
        // decision. Run from the repo root. See builder/src/affected.rs.
        Some("affected-checks") => affected::main(&args[2..]),
        // gate-run — td's OWN gate runner: the loop scheduler that replaced `make`
        // on the spine. The gates are compiled in (src/gate_defs/*.rs registry);
        // runs the requested tier/gates with cheap-serial + heavy-parallel
        // ordering, a MACHINE-WIDE flock slot pool (TD_CHECK_SLOTS, shared across
        // every concurrent check on the box), and data-driven longest-first heavy
        // order. Run from the repo root, inside the loop sandbox (`td-builder
        // check` execs it there). See builder/src/gates.rs.
        Some("gate-run") => gates::cli(args.get(2..).unwrap_or(&[])),
        // gate-body <name> — run one NATIVE (typed-Rust) gate body (#318 axis 3).
        // The runner execs this in place of `bash -c <script>` for a gate whose
        // GateDef.script is empty; see builder/src/gate_bodies.rs.
        Some("gate-body") if args.len() == 3 => gate_bodies::cli(&args[2]),
        // check [GOAL...] — the loop's HOST PRELUDE (the old shell check.sh,
        // ported): guards, stage0 + toolchain provisioning, warms, the shared
        // daemon, then the sandboxed gate-run. check.sh is now a guix-free cargo
        // bootstrap shim that execs this. (The drv reproducibility double-build
        // that used to share this verb is `check-drv` now — no argument sniffing.)
        Some("check") => check_loop::cli(args.get(2..).unwrap_or(&[])),
        // check-rung HARNESS [ARGS...] — dev-iteration helper: run a cached-chain
        // bootstrap harness inside the loop sandbox (was tools/check-rung.sh).
        Some("check-rung") => check_loop::check_rung_cli(args.get(2..).unwrap_or(&[])),
        // bootstrap-recipe <name> | --list — run a structured source-bootstrap rung
        // (the tests/bootstrap-*.sh drivers as typed Rust data; see bootstrap.rs).
        Some("bootstrap-recipe") => bootstrap::cli(&args),
        // toolchain-recipe <name> — build a /td/store toolchain rung as a structured Rust
        // recipe (the tests/x86_64-cross-fns.sh drivers as typed Rust; see toolchain_x86_64.rs).
        Some("toolchain-recipe") => toolchain_x86_64::cli(&args),
        Some("nar-hash") if args.len() == 3 => match nar_hash(&args[2]) {
            Ok(h) => {
                println!("{h}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("td-builder: nar-hash {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // The inverse of nar-hash's serializer: restore NARFILE onto DEST (which must
        // not already exist). The read side of the codec the substitute consumer uses to
        // unpack a fetched NAR; strict — a truncated/garbled archive errors, never a
        // partial tree.
        Some("nar-restore") if args.len() == 4 => {
            let (narfile, dest) = (&args[2], &args[3]);
            let run = || -> std::io::Result<()> {
                let mut r = std::io::BufReader::new(std::fs::File::open(narfile)?);
                nar::read_nar(&mut r, Path::new(dest))
            };
            match run() {
                Ok(()) => {
                    println!("td-builder: restored {narfile} -> {dest}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: nar-restore {narfile} {dest}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // oci-image: pack a PREPARED rootfs directory into a deterministic, uncompressed
        // docker-archive (OCI image) — td-native, no guix/Guile (system-image-native brick
        // 1). CONFIG-JSON is {"repoTag","env":[],"entrypoint":[],"cmd":[]} (all optional;
        // repoTag defaults to td:latest). Usage: oci-image ROOTFS-DIR CONFIG-JSON OUT.tar
        Some("oci-image") if args.len() == 5 => {
            let (rootfs, config_file, out_file) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                let cfg_text = std::fs::read_to_string(config_file)
                    .map_err(|e| format!("read {config_file}: {e}"))?;
                let cj = json::parse(&cfg_text).map_err(|e| format!("config JSON: {e}"))?;
                let cfg = image_config_from_json(&cj);
                let mut w =
                    std::fs::File::create(out_file).map_err(|e| format!("create {out_file}: {e}"))?;
                oci::write_docker_archive(&mut w, Path::new(rootfs), &cfg)
                    .map_err(|e| format!("write docker-archive: {e}"))?;
                Ok(())
            };
            match run() {
                Ok(()) => {
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: oci-image: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // oci-image-closure: the td-native replacement for `guix system image -t docker`.
        // Compute the store CLOSURE of ROOT… by CONTENT-SCANNING STORE-DIR (no /var/guix/db,
        // no guix process — scanForReferences == `guix gc -R` for an output root, gate 290),
        // lay each member at its STORE-DIR location into a single layer, and pack the
        // docker-archive. TD_STORE (env; the same td-owned-store concept realize_drv threads
        // as its `td_store` PARAMETER — build-plan passes it programmatically; this subcommand
        // is the only env reader of the name), when set,
        // names td's OWN store dir holding td-BUILT trees (the shared daemon cache): its
        // entries join the candidate index CANONICALIZED at STORE-DIR, are content-scanned
        // where their bytes lie, and are packed at their canonical names — so a td-built
        // root packs next to the guix-seed deps physically in STORE-DIR.
        // Usage: oci-image-closure STORE-DIR CONFIG-JSON OUT.tar ROOT...
        Some("oci-image-closure") if args.len() >= 6 => {
            let (store_dir, config_file, out_file) = (&args[2], &args[3], &args[4]);
            let roots = &args[5..];
            let run = || -> Result<usize, String> {
                let mut store_dirs = vec![store_dir.clone()];
                if let Some(ts) = std::env::var("TD_STORE").ok().filter(|s| !s.is_empty()) {
                    store_dirs.push(ts);
                }
                let (candidates, mut on_disk) = scan_candidate_index(&store_dirs, store_dir)?;
                let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                let empty = std::collections::HashMap::new();
                let mut closure_set: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                for r in roots {
                    closure_set.extend(scan_closure_hybrid(
                        &mut scanner,
                        &on_disk,
                        &empty,
                        std::slice::from_ref(r),
                    )?);
                }
                // Pack each canonical member from where its bytes really live. A member in
                // NO scanned dir is a hole in the image — fail loud, never ship a
                // silently-incomplete closure.
                let mut members: Vec<(String, String)> = Vec::with_capacity(closure_set.len());
                for c in closure_set {
                    let od = on_disk.remove(&c).ok_or_else(|| {
                        format!(
                            "closure member {c} is on disk in none of the scanned store dir(s) {}",
                            store_dirs.join(", ")
                        )
                    })?;
                    members.push((c, od));
                }
                let n = members.len();
                let cfg_text = std::fs::read_to_string(config_file)
                    .map_err(|e| format!("read {config_file}: {e}"))?;
                let cj = json::parse(&cfg_text).map_err(|e| format!("config JSON: {e}"))?;
                let cfg = image_config_from_json(&cj);
                let mut w =
                    std::fs::File::create(out_file).map_err(|e| format!("create {out_file}: {e}"))?;
                oci::write_docker_archive_from_closure(
                    &mut w,
                    Path::new(store_dir),
                    &members,
                    &cfg,
                )
                .map_err(|e| format!("write docker-archive: {e}"))?;
                Ok(n)
            };
            match run() {
                Ok(n) => {
                    eprintln!("td-builder: oci-image-closure: packed {n} store paths");
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: oci-image-closure: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // oci-image-paths: pack a PRE-RESOLVED store closure into a docker-archive — like
        // oci-image-closure, but the closure is read from a PATHS FILE (one store path per
        // line) instead of walking /var/guix/db. Lets a caller keep closure RESOLUTION
        // wherever it already is (e.g. a guix-resolved input-resolution step, retired last)
        // while the image CONSTRUCTION is td-native, and td reads no guix private state.
        // Usage: oci-image-paths PATHS-FILE STORE-DIR CONFIG-JSON OUT.tar
        Some("oci-image-paths") if args.len() == 6 => {
            let (paths_file, store_dir, config_file, out_file) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<usize, String> {
                let text = std::fs::read_to_string(paths_file)
                    .map_err(|e| format!("read paths {paths_file}: {e}"))?;
                let mut closure: Vec<String> = text
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect();
                closure.sort();
                closure.dedup();
                if closure.is_empty() {
                    return Err(format!("no store paths in {paths_file}"));
                }
                let n = closure.len();
                let cfg_text = std::fs::read_to_string(config_file)
                    .map_err(|e| format!("read {config_file}: {e}"))?;
                let cj = json::parse(&cfg_text).map_err(|e| format!("config JSON: {e}"))?;
                let cfg = image_config_from_json(&cj);
                let mut w =
                    std::fs::File::create(out_file).map_err(|e| format!("create {out_file}: {e}"))?;
                oci::write_docker_archive_from_store_paths(
                    &mut w,
                    Path::new(store_dir),
                    &closure,
                    &cfg,
                )
                .map_err(|e| format!("write docker-archive: {e}"))?;
                Ok(n)
            };
            match run() {
                Ok(n) => {
                    eprintln!("td-builder: oci-image-paths: packed {n} store paths");
                    println!("{out_file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: oci-image-paths: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // subst-export: write a serve-able substitute directory for the closure of ROOT…
        // over DB's Refs graph — a `<basename>.narinfo` + `nar/<narhash>.nar` per member —
        // the store-coupled, dependency-free half of the substitute server. STORE-DIR is the
        // directory holding each path FLAT as `<basename>` — `/gnu/store` for the live store,
        // or a build's `newstore` (the same flat layout build_and_register / store-add-text
        // write). The networked subst/ binary signs + serves OUTDIR. Usage:
        //   subst-export DB STORE-DIR OUTDIR ROOT...
        Some("subst-export") if args.len() >= 6 => {
            // Optional leading `--paths`: export EXACTLY the roots (no closure walk) — the
            // per-output granularity the substitute consumer fetches. Default = whole closure.
            let paths_only = args.get(2).map(|s| s.as_str()) == Some("--paths");
            let off = if paths_only { 3 } else { 2 };
            let run = || -> Result<Vec<String>, String> {
                if args.len() < off + 4 {
                    return Err("usage: subst-export [--paths] DB STORE-DIR OUTDIR ROOT...".into());
                }
                let (db_path, store_dir, outdir) = (&args[off], &args[off + 1], &args[off + 2]);
                let roots = &args[off + 3..];
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                let members = subst_export_members(&db, store_dir, roots, !paths_only)?;
                subst_export(Path::new(outdir), &members).map_err(|e| e.to_string())
            };
            match run() {
                Ok(written) => {
                    println!(
                        "td-builder: subst-export wrote {} narinfo(s) + nars -> {}",
                        written.len(),
                        args.get(off + 2).map(|s| s.as_str()).unwrap_or("")
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: subst-export: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // harness-subst-export OUTDIR HARNESS-DIR — ship the whole /td/store harness tree
        // (.td-build-cache/harness: store/ + rel + toolchain) to a guix-less runner as ONE nar +
        // a fixed-name `td-harness.narinfo` (issue #314). The daily signs it
        // (tools/publish-harness-subst.sh); a runner with an empty `.td-build-cache/harness`
        // fetches+verifies+restores it (tools/resolve-harness.sh) and runs check-harness.
        Some("harness-subst-export") if args.len() == 4 => {
            let (outdir, harness_dir) = (&args[2], &args[3]);
            match harness_subst_export(Path::new(outdir), Path::new(harness_dir)) {
                Ok(written) => {
                    println!(
                        "td-builder: harness-subst-export wrote {} narinfo(s) + nar -> {outdir}",
                        written.len()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: harness-subst-export: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // S3a — parse the ATerm drv and print the canonical dump.
        Some("drv-parse") if args.len() == 3 => match std::fs::read(&args[2]) {
            Ok(bytes) => match drv::parse(&bytes) {
                Ok(d) => {
                    print!("{}", drv::dump(&d));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-parse {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("td-builder: drv-parse {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // evaluator-as-library (sub-task 2): round-trip a `.drv` — parse then
        // re-serialize — and exit 0 only if byte-identical to the file. Proves the
        // ATerm serializer matches the daemon's writer on a real derivation.
        Some("drv-roundtrip") if args.len() == 3 => match std::fs::read(&args[2]) {
            Ok(bytes) => match drv::parse(&bytes) {
                Ok(d) => {
                    let re = drv::serialize(&d);
                    if re.as_bytes() == bytes.as_slice() {
                        println!("OK {}", args[2]);
                        ExitCode::SUCCESS
                    } else {
                        eprintln!("DIFFER: re-serialized {} is not byte-identical", args[2]);
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("td-builder: drv-roundtrip {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            },
            Err(e) => {
                eprintln!("td-builder: drv-roundtrip {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // evaluator-as-library (sub-task 3): compute a `.drv`'s OWN store path
        // from its content + references (inputDrvs ∪ inputSrcs), the daemon's
        // makeTextPath. Prints the computed path; the rung compares it to the real
        // one. Proves nix-base32 + make-store-path match guix.
        Some("drv-path") if args.len() == 3 => {
            let file = &args[2];
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let name = store::name_from_store_path(file)
                    .ok_or_else(|| format!("{file} is not a store path"))?;
                let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                refs.extend(d.input_srcs.iter().cloned());
                Ok(store::drv_store_path(&name, &bytes, &refs))
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-path {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // drv-refs: print a `.drv`'s DIRECT references — the store paths folded into its own
        // content-addressed path (inputDrvs ∪ inputSrcs), the exact set `drv-path`/the daemon's
        // makeTextPath uses. Parsed from the `.drv` bytes (drv::parse), so it is guix-free and
        // needs no store DB / no `guix gc --references`. One path per line, sorted+deduped —
        // the reference list `store-add-referenced` folds back in. Usage: drv-refs FILE
        Some("drv-refs") if args.len() == 3 => {
            let file = &args[2];
            let run = || -> Result<Vec<String>, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                refs.extend(d.input_srcs.iter().cloned());
                refs.sort();
                refs.dedup();
                Ok(refs)
            };
            match run() {
                Ok(refs) => {
                    for r in &refs {
                        println!("{r}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-refs {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // evaluator-as-library (sub-task 4): compute output `out`'s store path via
        // the recursive hashDerivationModulo. Prints the computed path; the rung
        // compares it to the real one. Proves the modulo recursion matches guix.
        Some("drv-outpath") if args.len() == 3 => {
            let file = &args[2];
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let drv_name = store::name_from_store_path(file)
                    .and_then(|n| n.strip_suffix(".drv").map(str::to_string))
                    .ok_or_else(|| format!("{file} is not a .drv store path"))?;
                store::output_path(&d, &drv_name, "out", &read)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-outpath {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // evaluator-as-library (sub-task 5): CONSTRUCT the `.drv` from its skeleton
        // — recompute every output path + the `.drv`'s own store path + serialize —
        // and verify byte-identical (path AND content) to guix's. This is the
        // §6-named differential: identical `.drv` both ways, with guix the oracle.
        Some("drv-emit") if args.len() == 3 => {
            let file = &args[2];
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<(), String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let drv_name = store::name_from_store_path(file)
                    .and_then(|n| n.strip_suffix(".drv").map(str::to_string))
                    .ok_or_else(|| format!("{file} is not a .drv store path"))?;
                let (path, content) = store::construct_drv(&d, &drv_name, &read)?;
                let path_ok = path == *file;
                let content_ok = content.as_bytes() == bytes.as_slice();
                if path_ok && content_ok {
                    Ok(())
                } else {
                    Err(format!(
                        "DIFFER: store path {} (computed {path}); content {}",
                        if path_ok { "matches" } else { "MISMATCH" },
                        if content_ok { "matches" } else { "MISMATCH" },
                    ))
                }
            };
            match run() {
                Ok(()) => {
                    println!("OK {file}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-emit {file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-drv-build (sub-task 2): like drv-emit, but WRITE the constructed `.drv`
        // to OUT (so the td-builder executor can build it). Prints the computed store
        // path. The end-to-end rung then builds OUT in the td-builder sandbox.
        Some("drv-emit-to") if args.len() == 4 => {
            let (oracle, out_file) = (&args[2], &args[3]);
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(oracle).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let drv_name = store::name_from_store_path(oracle)
                    .and_then(|n| n.strip_suffix(".drv").map(str::to_string))
                    .ok_or_else(|| format!("{oracle} is not a .drv store path"))?;
                let (path, content) = store::construct_drv(&d, &drv_name, &read)?;
                std::fs::write(out_file, content.as_bytes()).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-emit-to {oracle}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-drv-add: CONSTRUCT the `.drv` (#22) and REGISTER it in the store via
        // the daemon's addTextToStore — no guile `(derivation …)`. Asserts the
        // path the daemon returns equals td's own computed path, and prints it.
        // The socket is TD_DAEMON_SOCKET or the default.
        Some("drv-add") if args.len() == 3 => {
            let oracle = &args[2];
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(oracle).map_err(|e| e.to_string())?;
                let d = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let name = store::name_from_store_path(oracle)
                    .ok_or_else(|| format!("{oracle} is not a store path"))?;
                let drv_name = name
                    .strip_suffix(".drv")
                    .ok_or_else(|| format!("{oracle} is not a .drv"))?;
                let (computed, content) = store::construct_drv(&d, drv_name, &read)?;
                let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                refs.extend(d.input_srcs.iter().cloned());
                // The GUIX daemon's worker-protocol socket — deliberately NOT
                // TD_DAEMON_SOCKET: that env var names td's OWN build daemon (a line
                // protocol) since the machine-wide limiter landed, and check.sh exports
                // it loop-wide, so reading it here dialed the wrong daemon and spoke
                // binary worker-protocol at a line reader — the newline never came and
                // the td daemon's accept thread blocked forever (the machine-wide wedge
                // the daemon's read-timeout now bounds; this is the caller-side fix).
                let socket = std::env::var("TD_GUIX_DAEMON_SOCKET")
                    .unwrap_or_else(|_| daemon::DEFAULT_SOCKET.to_string());
                let mut dm = daemon::Daemon::connect(&socket)
                    .map_err(|e| format!("connect {socket}: {e}"))?;
                let added = dm
                    .add_text_to_store(&name, content.as_bytes(), &refs)
                    .map_err(|e| e.to_string())?;
                if added != computed {
                    return Err(format!(
                        "daemon registered {added} but td computed {computed}"
                    ));
                }
                Ok(added)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-add {oracle}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-drv-add: generic addTextToStore of FILE's bytes under NAME (no
        // references) — prints the daemon-returned store path. The rung uses it
        // with a UNIQUE name to prove the daemon actually WRITES td's bytes (a
        // novel path), not just returns a pre-existing one.
        Some("store-add") if args.len() == 4 => {
            let (name, file) = (&args[2], &args[3]);
            let run = || -> Result<String, String> {
                let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
                // The GUIX daemon's worker-protocol socket — deliberately NOT
                // TD_DAEMON_SOCKET: that env var names td's OWN build daemon (a line
                // protocol) since the machine-wide limiter landed, and check.sh exports
                // it loop-wide, so reading it here dialed the wrong daemon and spoke
                // binary worker-protocol at a line reader — the newline never came and
                // the td daemon's accept thread blocked forever (the machine-wide wedge
                // the daemon's read-timeout now bounds; this is the caller-side fix).
                let socket = std::env::var("TD_GUIX_DAEMON_SOCKET")
                    .unwrap_or_else(|_| daemon::DEFAULT_SOCKET.to_string());
                let mut dm = daemon::Daemon::connect(&socket)
                    .map_err(|e| format!("connect {socket}: {e}"))?;
                dm.add_text_to_store(name, &bytes, &[]).map_err(|e| e.to_string())
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: WRITE the store SQLite DB ourselves — the daemon's
        // `ValidPaths`/`Refs`/`DerivationOutputs` authority. td computes the
        // registration (NAR hash + size + reference scan, the same machinery `build`
        // uses) AND writes the SQLite file format directly (store_db, zero-dep) — the
        // real replacement of the daemon's libsqlite, no `sqlite3` engine. Usage:
        //   store-register STORE-PATH DERIVER CANDIDATES-FILE OUT-DB
        // CANDIDATES-FILE is STORE-PATH's full closure (`guix gc -R`). td registers
        // EVERY path in it — each fully scanned (real hash/size/refs) — plus all the
        // inter-path Refs and the deriver→output mapping. Only the deriver (a `.drv`,
        // not a closure member) is a scaffolding row so DerivationOutputs.drv resolves.
        // STORE-PATH carries its deriver; per-path derivers for the rest are the
        // daemon's input-resolution (a later increment). registrationTime is the
        // daemon's "now" — a fixed sentinel here, excluded from the differential.
        Some("store-register") if args.len() == 6 => {
            let (store_path, deriver, candidates_file, out_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<(), String> {
                use store_db::{Table, Value};
                // CANDIDATES-FILE is the artifact's full closure (`guix gc -R PATH`):
                // td registers EVERY path in it, each fully scanned — no placeholders.
                let closure: Vec<String> = std::fs::read_to_string(candidates_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                // Stable ids (= b-tree rowids), assigned ascending: the artifact = 1,
                // the deriver = 2, then the other closure paths in file order = 3.. .
                // Every reference is a closure member. The deriver `.drv` is ALWAYS id 2
                // (so DerivationOutputs.drv resolves); `others` excludes it so it is never
                // registered twice — the duplicate `ValidPaths` row that occurs when the
                // deriver IS itself a closure member (e.g. the rootless `img_drv`, which
                // is in its own `gc -R` set because it is bound into the staged store).
                let deriver_in_closure = closure.iter().any(|p| p.as_str() == deriver.as_str());
                let others: Vec<String> = closure
                    .iter()
                    .filter(|p| p.as_str() != store_path.as_str() && p.as_str() != deriver.as_str())
                    .cloned()
                    .collect();
                let id_of = |p: &str| -> Result<i64, String> {
                    if p == store_path.as_str() {
                        Ok(1)
                    } else if p == deriver.as_str() {
                        Ok(2)
                    } else {
                        others
                            .iter()
                            .position(|o| o.as_str() == p)
                            .map(|i| 3 + i as i64)
                            .ok_or_else(|| format!("reference `{p}' is not in the closure"))
                    }
                };
                // Scan one path; return its (hash, size, references) — the `build`
                // machinery, references found among the closure.
                let scan_path = |p: &str| -> Result<(String, u64, Vec<String>), String> {
                    let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                    nar::write_nar(&mut s, Path::new(p)).map_err(|e| e.to_string())?;
                    Ok(s.finish())
                };

                // ValidPaths rows in ascending rowid order; Refs accumulated per path.
                let mut valid: Vec<(i64, Vec<Value>)> = Vec::with_capacity(closure.len() + 1);
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut ref_rowid = 1i64;

                // id 1: the artifact, fully registered, with its deriver.
                let (a_hash, a_size, a_refs) = scan_path(store_path)?;
                valid.push((
                    1,
                    vec![
                        Value::Null, // id (integer primary key) — rowid is the id
                        Value::Text(store_path.to_string()),
                        Value::Text(a_hash),
                        Value::Int(1), // registrationTime (sentinel; excluded)
                        Value::Text(deriver.to_string()),
                        Value::Int(a_size as i64),
                    ],
                ));
                for r in &a_refs {
                    ref_rows.push((ref_rowid, vec![Value::Int(1), Value::Int(id_of(r)?)]));
                    ref_rowid += 1;
                }
                // id 2: the deriver. When it IS a closure member (the rootless case —
                // `img_drv` is bound into the staged store and the nested daemon reads it
                // to rebuild it for --check), register it FULLY (real hash/size/refs) so
                // the daemon accepts it as a valid path. Otherwise a scaffolding row (path
                // only) that exists solely so DerivationOutputs.drv resolves; the daemon
                // need not see the `.drv` as a valid built path in that case. Either way
                // id 2, so DerivationOutputs is unchanged.
                if deriver_in_closure {
                    let (d_hash, d_size, d_refs) = scan_path(deriver)?;
                    valid.push((
                        2,
                        vec![
                            Value::Null,
                            Value::Text(deriver.to_string()),
                            Value::Text(d_hash),
                            Value::Int(1),
                            Value::Null, // a `.drv` has no deriver of its own
                            Value::Int(d_size as i64),
                        ],
                    ));
                    for r in &d_refs {
                        ref_rows.push((ref_rowid, vec![Value::Int(2), Value::Int(id_of(r)?)]));
                        ref_rowid += 1;
                    }
                } else {
                    valid.push((
                        2,
                        vec![
                            Value::Null,
                            Value::Text(deriver.to_string()),
                            Value::Null,
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ],
                    ));
                }
                // ids 3..: the other closure paths, each fully registered (deriver NULL
                // — per-path derivers are the daemon's input-resolution, a later
                // increment; the differential is td's computed hash/size/refs).
                for p in &others {
                    let (hash, size, refs) = scan_path(p)?;
                    valid.push((
                        id_of(p)?,
                        vec![
                            Value::Null,
                            Value::Text(p.to_string()),
                            Value::Text(hash),
                            Value::Int(1),
                            Value::Null,
                            Value::Int(size as i64),
                        ],
                    ));
                    for r in &refs {
                        ref_rows.push((ref_rowid, vec![Value::Int(id_of(p)?), Value::Int(id_of(r)?)]));
                        ref_rowid += 1;
                    }
                }
                // DerivationOutputs: the deriver (id 2) → "out" → the artifact.
                let drv_out = vec![(
                    1i64,
                    vec![
                        Value::Int(2),
                        Value::Text("out".to_string()),
                        Value::Text(store_path.to_string()),
                    ],
                )];

                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                    Table {
                        name: "DerivationOutputs",
                        sql: "CREATE TABLE DerivationOutputs (drv integer, id text, path text)",
                        rows: drv_out,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: store-register {store_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: READ td's own store DB ourselves — the daemon's store-query
        // role, in pure Rust. `store_db_read` parses the SQLite file format that
        // `store-register` writes (no `sqlite3` engine, no daemon, in td's own
        // store-query path). Usage:
        //   store-query DB info        -> "path|hash|narSize" per fully-registered path
        //   store-query DB references  -> "referrer|reference" for the full Refs relation
        // Both sorted, so a set-comparison against the daemon oracle is order-free.
        Some("store-query") if args.len() == 4 => {
            let (db_path, mode) = (&args[2], &args[3]);
            let run = || -> Result<Vec<String>, String> {
                use store_db_read::{Db, Value};
                let text = |v: &Value| match v {
                    Value::Text(s) => Some(s.clone()),
                    _ => None,
                };
                let int = |v: &Value| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                };
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = Db::open(bytes)?;
                let mut out = match mode.as_str() {
                    // ValidPaths(id, path, hash, registrationTime, deriver, narSize):
                    // the path|hash|narSize of every fully-registered path (hash NOT NULL;
                    // a scaffolding row leaves hash/size NULL and is skipped).
                    "info" => {
                        let mut lines = Vec::new();
                        for (_rowid, cols) in db.table("ValidPaths")? {
                            match (text(&cols[1]), text(&cols[2]), int(&cols[5])) {
                                (Some(path), Some(hash), Some(size)) => {
                                    lines.push(format!("{path}|{hash}|{size}"));
                                }
                                _ => {}
                            }
                        }
                        lines
                    }
                    // Resolve Refs(referrer, reference) ids -> paths via the ValidPaths
                    // rowid (= the integer-primary-key id).
                    "references" => {
                        let mut path_of = std::collections::HashMap::new();
                        for (rowid, cols) in db.table("ValidPaths")? {
                            if let Some(p) = text(&cols[1]) {
                                path_of.insert(rowid, p);
                            }
                        }
                        let resolve = |id: i64| -> Result<String, String> {
                            path_of
                                .get(&id)
                                .cloned()
                                .ok_or_else(|| format!("Refs id {id} has no ValidPaths row"))
                        };
                        let mut lines = Vec::new();
                        for (_rowid, cols) in db.table("Refs")? {
                            match (int(&cols[0]), int(&cols[1])) {
                                (Some(a), Some(b)) => {
                                    lines.push(format!("{}|{}", resolve(a)?, resolve(b)?));
                                }
                                _ => return Err("Refs row has non-integer columns".to_string()),
                            }
                        }
                        lines
                    }
                    other => {
                        return Err(format!("unknown query mode `{other}' (info|references)"))
                    }
                };
                out.sort();
                Ok(out)
            };
            match run() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-query {db_path} {mode}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: compute the GC-reachable CLOSURE of ROOT(s) by CONTENT-SCANNING a
        // store — the daemon's scanForReferences (scan.rs) recursed to fixpoint — with NO
        // store DB and NO guix process. STORE-DIR's entries are the candidate set and each
        // ROOT's NAR (read from STORE-DIR) is scanned for the candidates it references,
        // transitively; output paths are STORE-DIR/<basename>. This re-derives a closure
        // from the BYTES — the same set `store-closure` walks from the store DB (and the
        // same set `guix gc -R`/`--requisites` returns), computed without any DB or daemon.
        // STORE-DIR is EITHER a self-contained td-owned store (e.g. an unpacked seed) OR the
        // live /gnu/store: the candidate index is built once and reused across the walk
        // (see Scanner::reset), so even a ~500k-entry live store is fast. A match is a
        // 32-char hash literally present in the bytes — exactly the daemon's own reference
        // criterion, so scanning the live store cannot report a reference guix would not
        // (the store-closure-live gate proves == `guix gc -R`). STORE-DIR may be a
        // COMMA-SEPARATED list DIR1,DIR2,…: the candidate index then spans every listed
        // dir (a path's bytes are read from whichever dir holds them — matching is by 32-char
        // hash, not by prefix, so a member found under a non-canonical dir still resolves),
        // while the FIRST dir is the canonical prefix the ROOT paths use. This closes a
        // subject whose output tree lives in one store (e.g. a build scratch's `newstore`)
        // and whose deps live in another (the seed /gnu/store) in a SINGLE scan. Usage:
        //   store-closure-scan STORE-DIR[,EXTRA-DIR...] ROOT [ROOT...]
        // Prints the reachable store paths (canonical under the first dir), sorted (ROOTs incl).
        Some("store-closure-scan") if args.len() >= 4 => {
            let store_dirs: Vec<String> = args[2]
                .split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            // The first (canonical) dir names the ROOT/candidate prefix; the rest are extra
            // byte sources merged into the index. A degenerate all-separator arg (e.g. ",")
            // yields no dirs → an empty prefix and an empty candidate set (the scan then just
            // echoes the roots) rather than a panic — callers pass real dirs.
            let canonical_prefix = store_dirs.first().cloned().unwrap_or_default();
            let roots: Vec<String> = args[3..].to_vec();
            let run = || -> Result<Vec<String>, String> {
                // Candidates = the store-path entries under every listed dir, keyed by 32-char
                // hash; the canonical prefix is the FIRST dir (a single-dir list keeps the
                // original "the dir IS the canonical location" behavior). BFS over CONTENT-
                // scanned refs to fixpoint (no store DB, no extra dbs): the shared
                // `scan_candidate_index` + `scan_closure_hybrid` — the same content-scan
                // realize_drv uses. Index built ONCE, reset() between paths, so even a
                // ~500k-entry live store is fast.
                let (candidates, on_disk) =
                    scan_candidate_index(&store_dirs, &canonical_prefix)?;
                let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                let empty = std::collections::HashMap::new();
                let seen = scan_closure_hybrid(&mut scanner, &on_disk, &empty, &roots)?;
                Ok(seen.into_iter().collect())
            };
            match run() {
                Ok(paths) => {
                    for p in &paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-closure-scan {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: compute the GC-reachable CLOSURE of one or more paths from td's
        // OWN store DB — the daemon's GC "mark" set (`guix gc -R ROOT` / the union
        // `guix gc --requisites ROOT…`), in pure Rust. Reads the DB with td's own
        // reader (`store_db_read`) and walks the Refs graph from each ROOT; no daemon.
        // Multiple ROOTs parse the DB once and union their closures. Usage:
        //   store-closure DB ROOT [ROOT...]
        // Prints the reachable store paths, sorted and deduped (every ROOT included).
        Some("store-closure") if args.len() >= 4 => {
            let db_path = &args[2];
            let roots: Vec<String> = args[3..].to_vec();
            let run = || -> Result<Vec<String>, String> {
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                db.closure_roots(&roots)
            };
            match run() {
                Ok(paths) => {
                    for p in paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-closure {db_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // seed-manifest: emit the MANIFEST for a seed closure — the capture half of the
        // frozen seed tarball (North-Star step 2). For the GC closure of ROOT…, print one
        // line per member: `<path> <nar-hash> <nar-size> <ref,ref,…>` (direct refs sorted;
        // `-` if none), all from td's OWN reader + NAR serializer (no daemon). The capture
        // tool tars the same closure; `seed-unpack` restores + registers from this manifest.
        //
        // SOURCE (arg 1) is EITHER a store DB FILE — closure + direct refs read from its Refs
        // graph (td's `store_db_read`) — OR a store DIRECTORY, in which case the closure and
        // every member's direct refs are computed by CONTENT-SCANNING the store bytes
        // (`scan_candidate_index` + `scan_closure_hybrid`, the same content-scan realize_drv
        // uses, == `guix gc -R`; gate 290) with NO store DB read at all. The dir form lets a
        // seed be captured with ZERO reads of guix's PRIVATE /var/guix/db (directive 8) — the
        // caller points it at the store dir the bytes live in (e.g. /gnu/store). Auto-detected
        // by whether SOURCE is a directory (a DB is always a file). Usage:
        //   seed-manifest DB-FILE-OR-STORE-DIR ROOT...
        Some("seed-manifest") if args.len() >= 4 => {
            let src = &args[2];
            let roots = &args[3..];
            // A closure member's `<path> <nar-hash> <nar-size> <refs>` line — refs sorted +
            // deduped, `-` when none (both branches emit the identical format).
            let manifest_line = |p: &str, hash: &str, size: u64, refs: &[String]| -> String {
                let mut rs: Vec<String> = refs.to_vec();
                rs.sort();
                rs.dedup();
                let refstr = if rs.is_empty() { "-".to_string() } else { rs.join(",") };
                format!("{p} {hash} {size} {refstr}")
            };
            let run = || -> Result<Vec<String>, String> {
                // STORE-DIR form: compute the closure + each member's direct refs by
                // content-scanning the store bytes — no store DB, no /var/guix/db, no daemon.
                if Path::new(src).is_dir() {
                    let store_dirs = std::slice::from_ref(src);
                    let (candidates, on_disk) = scan_candidate_index(store_dirs, src)?;
                    let mut scanner = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                    let empty = std::collections::HashMap::new();
                    // BFS the runtime closure over content-scanned refs (== guix gc -R).
                    let closure_set =
                        scan_closure_hybrid(&mut scanner, &on_disk, &empty, roots)?;
                    // Refs restricted to the (ref-closed) closure — a member's real direct
                    // refs are all closure members, so scanning against the closure set finds
                    // exactly them and drops nothing (superset-safe; matches the DB form).
                    let closure: Vec<String> = closure_set.iter().cloned().collect();
                    let mut lines = Vec::with_capacity(closure.len());
                    for p in &closure {
                        let od = on_disk.get(p).map(String::as_str).unwrap_or(p.as_str());
                        let mut s =
                            scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                        nar::write_nar(&mut s, Path::new(od))
                            .map_err(|e| format!("nar of {p} (at {od}): {e}"))?;
                        // finish() gives (nar-hash, nar-size, sorted refs) in the ONE pass —
                        // same "sha256:<base16>" hash + narSize the DB form's nar_hash_size_path emits.
                        let (hash, size, refs) = s.finish();
                        lines.push(manifest_line(p, &hash, size, &refs));
                    }
                    return Ok(lines);
                }
                // STORE-DB form: closure + direct refs from the DB's Refs graph.
                let bytes = std::fs::read(src).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                let mut closure = std::collections::BTreeSet::new();
                for r in roots {
                    for p in db.closure(r)? {
                        closure.insert(p);
                    }
                }
                let refs = db.refs_by_path()?;
                let mut lines = Vec::new();
                for p in &closure {
                    let (hash, size) =
                        nar_hash_size_path(Path::new(p)).map_err(|e| format!("nar of {p}: {e}"))?;
                    let rs: Vec<String> = refs.get(p).cloned().unwrap_or_default();
                    lines.push(manifest_line(p, &hash, size, &rs));
                }
                Ok(lines)
            };
            match run() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: seed-manifest: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // seed-unpack: RESTORE a frozen seed tarball into a td-owned store + register it
        // from the manifest — North-Star step 2, so the loop has the toolchain seed with NO
        // guix install. Extracts TARBALL into DEST-STORE (the canonical `/gnu/store/<base>`
        // trees land at `DEST-STORE/gnu/store/<base>`), VERIFIES each restored tree's NAR
        // hash equals the manifest (the seed survived the tarball, byte-for-byte), and writes
        // DEST-DB (ValidPaths + Refs) FROM the manifest — no re-scan (the live /gnu/store is
        // read-only in the loop), no daemon. Usage:
        //   seed-unpack TARBALL MANIFEST DEST-STORE DEST-DB
        Some("seed-unpack") if args.len() == 6 => {
            let (tarball, manifest, dest_store, dest_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<usize, String> {
                use store_db::{Table, Value};
                // Parse the manifest: `<path> <nar-hash> <nar-size> <ref,ref,…>`.
                let text = std::fs::read_to_string(manifest)
                    .map_err(|e| format!("read manifest {manifest}: {e}"))?;
                struct Entry {
                    path: String,
                    hash: String,
                    size: u64,
                    refs: Vec<String>,
                }
                let mut entries: Vec<Entry> = Vec::new();
                for (i, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let f: Vec<&str> = line.split(' ').collect();
                    if f.len() != 4 {
                        return Err(format!("manifest:{}: want `PATH HASH SIZE REFS', got `{line}'", i + 1));
                    }
                    let refs = if f[3] == "-" {
                        Vec::new()
                    } else {
                        f[3].split(',').map(str::to_string).collect()
                    };
                    entries.push(Entry {
                        path: f[0].to_string(),
                        hash: f[1].to_string(),
                        size: f[2].parse().map_err(|_| format!("manifest:{}: bad size", i + 1))?,
                        refs,
                    });
                }
                if entries.is_empty() {
                    return Err("manifest is empty".into());
                }
                // Extract the tar into DEST-STORE (its members are `gnu/store/<base>`).
                std::fs::create_dir_all(dest_store).map_err(|e| e.to_string())?;
                let ok = Command::new("tar")
                    .args(["xf", tarball, "-C", dest_store])
                    .status()
                    .map_err(|e| format!("spawn tar: {e}"))?
                    .success();
                if !ok {
                    return Err(format!("tar xf {tarball} -C {dest_store} failed"));
                }
                // Verify every restored tree is NAR-identical to the manifest.
                for e in &entries {
                    let on_disk = format!("{dest_store}{}", e.path); // DEST-STORE + /gnu/store/<base>
                    let got = nar_hash_path(Path::new(&on_disk))
                        .map_err(|err| format!("nar-hash {on_disk}: {err}"))?;
                    if got != e.hash {
                        return Err(format!(
                            "NAR mismatch after restore for {} (restored={got} manifest={})",
                            e.path, e.hash
                        ));
                    }
                }
                // Register DEST-DB from the manifest: rowids in manifest order, Refs by id.
                let id_of: std::collections::HashMap<&str, i64> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (e.path.as_str(), i as i64 + 1))
                    .collect();
                let mut valid: Vec<(i64, Vec<Value>)> = Vec::with_capacity(entries.len());
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut ref_rowid = 1i64;
                for e in &entries {
                    let id = id_of[e.path.as_str()];
                    valid.push((
                        id,
                        vec![
                            Value::Null,
                            Value::Text(e.path.clone()),
                            Value::Text(e.hash.clone()),
                            Value::Int(1), // registrationTime sentinel
                            Value::Null,   // deriver: a seed has none
                            Value::Int(e.size as i64),
                        ],
                    ));
                    for r in &e.refs {
                        let rid = *id_of.get(r.as_str()).ok_or_else(|| {
                            format!("reference `{r}' of {} is not in the manifest", e.path)
                        })?;
                        ref_rows.push((ref_rowid, vec![Value::Int(id), Value::Int(rid)]));
                        ref_rowid += 1;
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(dest_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(entries.len())
            };
            match run() {
                Ok(n) => {
                    eprintln!("td-builder: seed-unpack restored + registered {n} seed paths (NAR-verified, no daemon)");
                    println!("{n}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: seed-unpack: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: ADD a path to a td-OWNED store ourselves — the daemon's
        // addToStore (the WRITE side), in pure Rust. td computes the addTextToStore
        // path (`make_text_path`), WRITES the content into STORE-DIR as a canonical
        // store file (a regular, read-only 0444 file), and REGISTERS it in a td store
        // DB (`store_db`) — no daemon in the write path. NAR (hence the store path's
        // identity) ignores mtime and the read/write permission bits, so the
        // registration is metadata-independent. Usage:
        //   store-add-text NAME CONTENT-FILE STORE-DIR OUT-DB
        // Prints the store path. Flat/text case, no references — the recursive
        // directory case (canonical tree restore) is a later increment.
        Some("store-add-text") if args.len() == 6 => {
            let (name, content_file, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<String, String> {
                use std::os::unix::fs::PermissionsExt;
                use store_db::{Table, Value};
                let content = std::fs::read(content_file).map_err(|e| e.to_string())?;
                // td computes the addTextToStore path itself (no references).
                let path = store::make_text_path(name, &content, &[]);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Write the content into the td-owned store as a canonical store file:
                // a regular, world-readable, read-only (0444) file.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                std::fs::write(&disk, &content).map_err(|e| e.to_string())?;
                let mut perm =
                    std::fs::metadata(&disk).map_err(|e| e.to_string())?.permissions();
                perm.set_mode(0o444);
                std::fs::set_permissions(&disk, perm).map_err(|e| e.to_string())?;
                // Register it: NAR-hash + size of the file td just wrote (the `build`
                // machinery), references scanned among the single-path closure.
                let closure = vec![path.clone()];
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                let valid = vec![(
                    1i64,
                    vec![
                        Value::Null, // id (integer primary key) — rowid is the id
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1), // registrationTime (sentinel; excluded)
                        Value::Null,   // deriver — a source add has none
                        Value::Int(size as i64),
                    ],
                )];
                // A flat text add references nothing but (possibly) itself.
                let mut ref_rows = Vec::new();
                let mut rid = 1i64;
                for r in &refs {
                    if r == &path {
                        ref_rows.push((rid, vec![Value::Int(1), Value::Int(1)]));
                        rid += 1;
                    } else {
                        return Err(format!("unexpected reference {r} in a flat text add"));
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-text {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: ADD a DIRECTORY TREE to a td-OWNED store ourselves — the
        // RECURSIVE addToStore (the general write side), in pure Rust. td computes the
        // content-addressed `source` path from the recursive NAR hash
        // (`make_store_path("source", sha256(NAR), name)` — the daemon's
        // makeFixedOutputPath for recursive-sha256, no references), CANONICALLY restores
        // the tree into a td-owned store dir (`copy_canonical`: structure + contents +
        // exec bit + symlinks, the NAR-relevant properties), and REGISTERS it in a td
        // store DB (`store_db`). No daemon in the write path. Usage:
        //   store-add-recursive NAME SRC STORE-DIR OUT-DB
        // Prints the store path. No-reference sources (this increment); referenced
        // sources are a later increment.
        Some("store-add-recursive") if args.len() == 6 => {
            let (name, src, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                // Content-addressed path from the source tree's recursive NAR sha256.
                let nar = nar_hash(src).map_err(|e| e.to_string())?;
                let hex = nar
                    .strip_prefix("sha256:")
                    .ok_or_else(|| format!("nar-hash returned `{nar}', expected sha256:<hex>"))?;
                let path = store::make_store_path("source", hex, name);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Canonically restore the tree into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(src), &disk)?;
                // Register: NAR hash + size of the tree td restored (the `build`
                // machinery), references scanned among the single-path closure.
                let closure = vec![path.clone()];
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                if !refs.is_empty() && refs != [path.clone()] {
                    return Err(format!(
                        "source {name} has references {refs:?}; referenced sources are a later increment"
                    ));
                }
                let valid = vec![(
                    1i64,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null, // deriver — a source add has none
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows = Vec::new();
                let mut rid = 1i64;
                for r in &refs {
                    if r == &path {
                        ref_rows.push((rid, vec![Value::Int(1), Value::Int(1)]));
                        rid += 1;
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-recursive {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // INPUT-ADDRESSED add: like store-add-recursive, but the store path's digest is
        // KEY (a hash of the artifact's DECLARED INPUTS — `toolchain-key`), NOT the tree's
        // recursive NAR hash. So a NON-byte-reproducible tree (the modern toolchain: cc1
        // stamp, ar/install mtimes) lands at a STABLE path: identical across rebuilds, and
        // computable from the lock BEFORE the build (the prereq for td-subst chain-caching).
        // The tree is still REGISTERED with its real NAR hash + size (naming and content-
        // integrity are orthogonal — the daemon's `output:` semantics), so closure/verify
        // are unchanged. Usage:
        //   store-add-input-addressed NAME KEY SRC STORE-DIR OUT-DB    (prints the path)
        Some("store-add-input-addressed") if args.len() == 7 => {
            let (name, key, src, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                // Input-addressed path: digest = KEY (declared inputs), not the content.
                let path = store::input_addressed_path(key, name);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed (bad KEY/NAME?)"))?
                    .to_string();
                // Canonically restore the tree into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(src), &disk)?;
                // Register the REAL NAR hash + size of the placed tree (self-references
                // among the single-path closure — the store-add-recursive registration).
                let closure = vec![path.clone()];
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                let valid = vec![(
                    1i64,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null, // deriver — set by the producer's drv when there is one
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows = Vec::new();
                let mut rid = 1i64;
                for r in &refs {
                    if r == &path {
                        ref_rows.push((rid, vec![Value::Int(1), Value::Int(1)]));
                        rid += 1;
                    }
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-input-addressed {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // bootstrap brick 2: PLACE a tree WITH references into a td-owned store,
        // content-addressed — the builder analog of store-add-recursive (which REFUSES a
        // referenced tree). td restores the tree into STORE-DIR, computes its
        // content-addressed `source` path from the recursive NAR, SCANS its references
        // against the SEED STORE DIRECTORY's entries (readdir via scan_candidate_index —
        // the pinned toolchain store holding the glibc/gcc-lib the stage0 builder links;
        // NO guix db read, #313), and registers the path + those refs in OUT-DB (each ref
        // a scaffolding ValidPaths row so the Refs join resolves — store-add-referenced's
        // external-ref shape). An ABSENT seed dir contributes no candidates: a guix-less
        // host's rustup/system-cc stage0 embeds no store paths, so the cold start records
        // an empty reference set. This lets the loop use a td-BOOTSTRAPPED builder (stage0,
        // NEVER produced by guix) as a recipe's builder-of-record: build-recipe reads its
        // closure as OUT-DB.closure(path) (the builder + its DIRECT refs) ∪ the seed
        // content-scan (those refs' transitive closures). No daemon, no guix. Usage:
        //   store-add-builder NAME TREE STORE-DIR OUT-DB SEED-STORE-DIR  (prints the store path)
        Some("store-add-builder") if args.len() == 7 => {
            let (name, tree, store_dir, out_db, seed_store) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                // Content-addressed path from the tree's recursive NAR sha256 (same as
                // store-add-recursive — a `source`-type path).
                let nar = nar_hash(tree).map_err(|e| e.to_string())?;
                let hex = nar
                    .strip_prefix("sha256:")
                    .ok_or_else(|| format!("nar-hash returned `{nar}', expected sha256:<hex>"))?;
                let path = store::make_store_path("source", hex, name);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Canonically restore the tree into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(tree), &disk)?;
                // Scan the restored tree for references AGAINST the seed store DIRECTORY's
                // entries (the pinned toolchain store) — the builder's actual store deps,
                // with NO read of guix's private db (#313: a guix-less host cold-starts).
                // An ABSENT seed dir is legitimate (a guix-less host has no /gnu/store, so
                // the stage0 embeds no store refs and the placement records an empty ref
                // set). But a PRESENT-but-unreadable seed dir (a typo'd path, a regular
                // file, an EACCES mount) must FAIL LOUDLY, not be silently treated as
                // empty — a refless placement would poison the builder's closure and
                // surface only as an opaque exec/link failure at build time. So
                // distinguish NotFound (benign, no candidates) from any other read_dir
                // error here, restoring the loud failure the old sqlite seed read gave;
                // scan_candidate_index itself swallows both as "contributes nothing".
                match std::fs::read_dir(seed_store) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("seed store {seed_store}: {e}")),
                }
                // The path itself is a candidate so a self-reference is detected. Extra
                // never-matching candidates cannot add references (scan.rs candidate note).
                let (mut candidates, _on_disk) =
                    scan_candidate_index(std::slice::from_ref(seed_store), seed_store)?;
                candidates.push(path.clone());
                let mut s = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, mut refs) = s.finish();
                refs.sort();
                refs.dedup();
                // Register: id 1 = the builder (full record), each external reference a
                // scaffolding ValidPaths row (path only) so the Refs ids resolve. So
                // OUT-DB.closure(path) returns the builder + its DIRECT refs; realize then
                // spans those refs' transitive closures from the seed content-scan.
                let mut valid: Vec<(i64, Vec<Value>)> = vec![(
                    1,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null,
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut edge = 1i64;
                let mut next_id = 2i64;
                for r in &refs {
                    let target = if r == &path {
                        1 // a self-reference resolves to id 1
                    } else {
                        valid.push((
                            next_id,
                            vec![
                                Value::Null,
                                Value::Text(r.clone()),
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ],
                        ));
                        let id = next_id;
                        next_id += 1;
                        id
                    };
                    ref_rows.push((edge, vec![Value::Int(1), Value::Int(target)]));
                    edge += 1;
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-builder {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: ADD a path WITH REFERENCES to a td-owned store — the daemon's
        // addToStore/addTextToStore WITH a references set, in pure Rust. td computes the
        // content-addressed path with the references folded into the type
        // (`make_text_path`: `text:<sorted refs>` — the daemon's makeTextPath/makeType),
        // WRITES the content into a td-owned store dir (canonical 0444 file), and
        // REGISTERS it with its `Refs` to the referenced paths (each a scaffolding
        // ValidPaths row so the join resolves). No daemon. The canonical referenced
        // content-addressed item is a `.drv` (referenced by its input drvs/srcs). Usage:
        //   store-add-referenced NAME CONTENT-FILE REFS-FILE STORE-DIR OUT-DB
        // REFS-FILE lists the references (one store path per line). Prints the store path.
        Some("store-add-referenced") if args.len() == 7 => {
            let (name, content_file, refs_file, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use std::os::unix::fs::PermissionsExt;
                use store_db::{Table, Value};
                let content = std::fs::read(content_file).map_err(|e| e.to_string())?;
                let mut refs: Vec<String> = std::fs::read_to_string(refs_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                refs.sort();
                refs.dedup();
                // td computes the path with the references in the type (makeTextPath).
                let path = store::make_text_path(name, &content, &refs);
                let base = path
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(&path).is_some())
                    .ok_or_else(|| format!("computed path {path} is malformed"))?
                    .to_string();
                // Write the content as a canonical (0444) store file.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                std::fs::write(&disk, &content).map_err(|e| e.to_string())?;
                let mut perm =
                    std::fs::metadata(&disk).map_err(|e| e.to_string())?.permissions();
                perm.set_mode(0o444);
                std::fs::set_permissions(&disk, perm).map_err(|e| e.to_string())?;
                // NAR hash + size of what td wrote (for the registration record).
                let mut s = scan::Scanner::new(&[path.clone()]).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, _) = s.finish();
                // Register: id 1 = the path (full), with its declared references; each
                // reference is a scaffolding ValidPaths row (path only) so Refs resolves.
                let mut valid: Vec<(i64, Vec<Value>)> = vec![(
                    1,
                    vec![
                        Value::Null,
                        Value::Text(path.clone()),
                        Value::Text(hash),
                        Value::Int(1),
                        Value::Null,
                        Value::Int(size as i64),
                    ],
                )];
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut edge = 1i64;
                let mut next_id = 2i64;
                for r in &refs {
                    let target = if r == &path {
                        1 // a self-reference resolves to id 1
                    } else {
                        valid.push((
                            next_id,
                            vec![
                                Value::Null,
                                Value::Text(r.clone()),
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ],
                        ));
                        let id = next_id;
                        next_id += 1;
                        id
                    };
                    ref_rows.push((edge, vec![Value::Int(1), Value::Int(target)]));
                    edge += 1;
                }
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(path)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-referenced {name}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: a td STORE BACKEND for a BUILD OUTPUT — place a built output's
        // TREE into a td-owned store at its output path and FULLY REGISTER it (the
        // daemon's post-build registration: hash + narSize + deriver + the output's
        // references + the drv->output mapping), in pure Rust, no daemon. The result is
        // a td-owned store that HOLDS the build result and is served by td's own tools
        // (store-query / store-verify / store-closure). Usage:
        //   store-add-output OUTPUT DERIVER CLOSURE-FILE STORE-DIR OUT-DB
        // CLOSURE-FILE is OUTPUT's runtime closure (`guix gc -R`), used to scan
        // references. The output's tree is placed; its references are scaffolding rows.
        Some("store-add-output") if args.len() == 7 => {
            let (output, deriver, closure_file, store_dir, out_db) =
                (&args[2], &args[3], &args[4], &args[5], &args[6]);
            let run = || -> Result<String, String> {
                use store_db::{Table, Value};
                let closure: Vec<String> = std::fs::read_to_string(closure_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                let base = output
                    .rsplit('/')
                    .next()
                    .filter(|_| store::name_from_store_path(output).is_some())
                    .ok_or_else(|| format!("output {output} is not a store path"))?
                    .to_string();
                // Place the output TREE canonically into the td-owned store.
                std::fs::create_dir_all(store_dir).map_err(|e| e.to_string())?;
                let disk = Path::new(store_dir).join(&base);
                copy_canonical(Path::new(output), &disk)?;
                // Scan the PLACED tree for its registration (hash + size + references
                // among the closure) — the `build` machinery.
                let mut s = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, refs) = s.finish();
                // Register: id 1 = the OUTPUT (full, with its deriver); id 2 = the
                // deriver scaffold (so DerivationOutputs.drv resolves); ids 3.. = the
                // references (scaffold, path only). Refs: output -> each reference.
                let mut valid: Vec<(i64, Vec<Value>)> = vec![
                    (
                        1,
                        vec![
                            Value::Null,
                            Value::Text(output.to_string()),
                            Value::Text(hash),
                            Value::Int(1),
                            Value::Text(deriver.to_string()),
                            Value::Int(size as i64),
                        ],
                    ),
                    (
                        2,
                        vec![
                            Value::Null,
                            Value::Text(deriver.to_string()),
                            Value::Null,
                            Value::Null,
                            Value::Null,
                            Value::Null,
                        ],
                    ),
                ];
                let mut ref_rows: Vec<(i64, Vec<Value>)> = Vec::new();
                let mut edge = 1i64;
                let mut next_id = 3i64;
                for r in &refs {
                    let target = if r == output {
                        1 // self-reference -> id 1
                    } else {
                        valid.push((
                            next_id,
                            vec![
                                Value::Null,
                                Value::Text(r.clone()),
                                Value::Null,
                                Value::Null,
                                Value::Null,
                                Value::Null,
                            ],
                        ));
                        let id = next_id;
                        next_id += 1;
                        id
                    };
                    ref_rows.push((edge, vec![Value::Int(1), Value::Int(target)]));
                    edge += 1;
                }
                let drv_out = vec![(
                    1i64,
                    vec![Value::Int(2), Value::Text("out".to_string()), Value::Text(output.to_string())],
                )];
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: valid,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: ref_rows,
                    },
                    Table {
                        name: "DerivationOutputs",
                        sql: "CREATE TABLE DerivationOutputs (drv integer, id text, path text)",
                        rows: drv_out,
                    },
                ];
                std::fs::write(out_db, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(output.to_string())
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-add-output {output}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: COMMIT a finished build into a PERSISTENT td store — the
        // build-into half of an accumulating store+DB that survives across separate
        // `td-builder` invocations (vs the per-build scratch a one-shot build leaves).
        // Given a build SCRATCH (its `registration` + `newstore/<base>` trees, as
        // `build-recipe`/`realize` write), INTERN each output tree into STORE-DIR at
        // its basename (idempotent — a content path already present is a no-op) and
        // MERGE its registration into DB (the accumulating `ValidPaths`/`Refs`, via
        // `merge_output_db`) instead of clobbering. A later, SEPARATE invocation then
        // reads those outputs back out of STORE-DIR + DB (store-query/store-verify/
        // store-closure) — build-into / read-back across builds, no daemon. Usage:
        //   store-commit STORE-DIR DB SCRATCH
        Some("store-commit") if args.len() == 5 => {
            let (store_dir, db_path, scratch) = (&args[2], &args[3], &args[4]);
            let run =
                || commit_scratch_to_store(Path::new(scratch), store_dir, Path::new(db_path));
            match run() {
                Ok(paths) => {
                    for p in paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-commit: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // toolchain stable key: print the INPUT key of a td-toolchain.lock — sha256 over
        // its declared inputs (sources + patches + name + recipe-rev), order-independent.
        // The input-addressed toolchain path (toolchain-path) is named by this key, so it
        // is stable across non-reproducible rebuilds. Usage: toolchain-key LOCK
        Some("toolchain-key") if args.len() == 3 => {
            match std::fs::read_to_string(&args[2])
                .map_err(|e| e.to_string())
                .and_then(|c| store::ToolchainLock::parse(&c))
            {
                Ok(lock) => {
                    println!("{}", lock.key());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: toolchain-key {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // toolchain stable path: print the INPUT-ADDRESSED store path for a component of
        // the toolchain (or the toolchain itself when NAME is omitted), under the active
        // store_dir() (set TD_STORE_DIR=/td/store for the /td/store path). This is the
        // path the producer interns the built tree at (store-add-input-addressed) and the
        // path a td-subst consumer computes from the lock BEFORE fetching — the 2a stable key.
        // Usage: toolchain-path LOCK [NAME]
        Some("toolchain-path") if args.len() == 3 || args.len() == 4 => {
            let name = args.get(3).map(String::as_str);
            match std::fs::read_to_string(&args[2])
                .map_err(|e| e.to_string())
                .and_then(|c| store::ToolchainLock::parse(&c))
            {
                Ok(lock) => {
                    if let Some(n) = name {
                        if !lock.components.iter().any(|c| c == n) && n != lock.name {
                            eprintln!(
                                "td-builder: toolchain-path: `{n}` is not a component of {} (have: {})",
                                lock.name,
                                lock.components.join(", ")
                            );
                            return ExitCode::FAILURE;
                        }
                    }
                    println!("{}", lock.path_for(name));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: toolchain-path {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: VERIFY a td store's integrity ourselves — the daemon's
        // `guix gc --verify --check-contents`, in pure Rust. Reads the recorded
        // registration from a td store DB (`store_db_read`, #36), re-NAR-hashes each
        // registered path at STORE-ROOT/<basename>, and reports any path whose content
        // no longer matches its recorded `hash` (corruption / disk-rot). No daemon.
        // Usage:
        //   store-verify DB STORE-ROOT
        // STORE-ROOT holds the path bytes (e.g. /gnu/store, or a td-owned store dir).
        // Exit 0 if every registered path verifies; exit 1 (listing the mismatches) if
        // any content differs from its recorded hash.
        Some("store-verify") if args.len() == 4 => {
            let (db_path, store_root) = (&args[2], &args[3]);
            let run = || -> Result<Vec<String>, String> {
                use store_db_read::{Db, Value};
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = Db::open(bytes)?;
                let mut mismatches = Vec::new();
                let mut checked = 0u64;
                for (_rowid, cols) in db.table("ValidPaths")? {
                    // Only paths with a recorded hash (skip scaffolding rows).
                    let (path, recorded) = match (cols.get(1), cols.get(2)) {
                        (Some(Value::Text(p)), Some(Value::Text(h))) => (p, h),
                        _ => continue,
                    };
                    let base = path
                        .rsplit('/')
                        .next()
                        .ok_or_else(|| format!("malformed path {path}"))?;
                    let location = Path::new(store_root).join(base);
                    let got = nar_hash_path(&location).map_err(|e| format!("{}: {e}", location.display()))?;
                    checked += 1;
                    if &got != recorded {
                        mismatches.push(format!("{path}: recorded {recorded} got {got}"));
                    }
                }
                if checked == 0 {
                    Err("no registered paths with a recorded hash to verify".to_string())
                } else if mismatches.is_empty() {
                    Ok(vec![format!("verified {checked} paths")])
                } else {
                    Err(format!(
                        "{} of {checked} paths FAILED verification:\n{}",
                        mismatches.len(),
                        mismatches.join("\n")
                    ))
                }
            };
            match run() {
                Ok(lines) => {
                    for l in lines {
                        println!("{l}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-verify: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-store-db: the destructive GC SWEEP — the other half of GC (after the
        // mark/liveness `store-closure`, #39), in pure Rust. Given a td-owned store DIR,
        // its DB, and a GC ROOT, td computes the live set (closure of ROOT over Refs),
        // DELETES every registered content path NOT reachable from ROOT from STORE-DIR,
        // and rewrites the DB to the live set (ValidPaths + Refs renumbered). No daemon.
        // Boundary: operates ONLY on the given (td-owned) STORE-DIR/DB — NEVER the host
        // store. Usage:
        //   store-gc-sweep STORE-DIR DB ROOT
        // Prints how many paths were swept / remain.
        Some("store-gc-sweep") if args.len() == 5 => {
            let (store_dir, db_path, root) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<String, String> {
                use std::collections::{HashMap, HashSet};
                use store_db::{Table, Value as WV};
                use store_db_read::Value as RV;
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                let live: HashSet<String> = db.closure(root)?.into_iter().collect();
                let valid = db.table("ValidPaths")?;
                let refs = db.table("Refs")?;
                // old rowid -> path (to remap Refs after renumbering).
                let mut path_of: HashMap<i64, String> = HashMap::new();
                for (rid, cols) in &valid {
                    if let Some(RV::Text(p)) = cols.get(1) {
                        path_of.insert(*rid, p.clone());
                    }
                }
                // A registered content path = a row WITH a recorded hash (skip the
                // deriver scaffold). Keep the live ones; DELETE the dead ones' files.
                let mut survivors: Vec<&Vec<RV>> = Vec::new();
                let mut deleted = 0u64;
                for (_rid, cols) in &valid {
                    let path = match (cols.get(1), cols.get(2)) {
                        (Some(RV::Text(p)), Some(RV::Text(_))) => p,
                        _ => continue, // no hash -> scaffolding, not a content path
                    };
                    if live.contains(path) {
                        survivors.push(cols);
                    } else if let Some(base) = path.rsplit('/').next() {
                        let entry = Path::new(store_dir).join(base);
                        if entry.exists() {
                            if entry.is_dir() {
                                std::fs::remove_dir_all(&entry)
                                    .map_err(|e| format!("{}: {e}", entry.display()))?;
                            } else {
                                std::fs::remove_file(&entry)
                                    .map_err(|e| format!("{}: {e}", entry.display()))?;
                            }
                            deleted += 1;
                        }
                    }
                }
                // Renumber survivors 1..k by path; remap Refs among them.
                survivors.sort_by(|a, b| path_at(a).cmp(path_at(b)));
                let mut newid: HashMap<String, i64> = HashMap::new();
                let mut vrows: Vec<(i64, Vec<WV>)> = Vec::new();
                for (i, cols) in survivors.iter().enumerate() {
                    let nid = i as i64 + 1;
                    let path = path_at(cols).to_string();
                    newid.insert(path.clone(), nid);
                    let conv = |v: Option<&RV>| -> WV {
                        match v {
                            Some(RV::Int(n)) => WV::Int(*n),
                            Some(RV::Text(s)) => WV::Text(s.clone()),
                            _ => WV::Null,
                        }
                    };
                    vrows.push((
                        nid,
                        vec![
                            WV::Null,
                            WV::Text(path),
                            conv(cols.get(2)), // hash
                            conv(cols.get(3)), // registrationTime
                            conv(cols.get(4)), // deriver
                            conv(cols.get(5)), // narSize
                        ],
                    ));
                }
                let mut rrows: Vec<(i64, Vec<WV>)> = Vec::new();
                let mut rid = 1i64;
                for (_r, cols) in &refs {
                    let (a, b) = match (cols.first(), cols.get(1)) {
                        (Some(RV::Int(a)), Some(RV::Int(b))) => (*a, *b),
                        _ => continue,
                    };
                    if let (Some(pa), Some(pb)) = (path_of.get(&a), path_of.get(&b)) {
                        if let (Some(&na), Some(&nb)) = (newid.get(pa), newid.get(pb)) {
                            rrows.push((rid, vec![WV::Int(na), WV::Int(nb)]));
                            rid += 1;
                        }
                    }
                }
                // The swept DB carries the live ValidPaths + Refs only; the deriver
                // scaffold and DerivationOutputs are intentionally not carried (a swept
                // store is content + references — the build-derivation mapping is rebuilt
                // by registration, not GC).
                let tables = [
                    Table {
                        name: "ValidPaths",
                        sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                        rows: vrows,
                    },
                    Table {
                        name: "Refs",
                        sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                        rows: rrows,
                    },
                ];
                std::fs::write(db_path, store_db::write_db(&tables)).map_err(|e| e.to_string())?;
                Ok(format!("swept {deleted} dead paths, {} live remain", newid.len()))
            };
            match run() {
                Ok(msg) => {
                    println!("{msg}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-gc-sweep: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-drv-assemble: ASSEMBLE the `.drv` from a raw SPEC (Guile resolved the
        // inputs and emitted it WITHOUT `(derivation …)`) and REGISTER it via the
        // daemon — so the build derivation enters the store with no guile
        // `(derivation …)` at all. Asserts the daemon returns td's own computed
        // path, and prints it.
        Some("drv-assemble") if args.len() == 3 => {
            let spec_file = &args[2];
            let read = |p: &str| std::fs::read(p).map_err(|e| e.to_string());
            let run = || -> Result<String, String> {
                let spec = std::fs::read_to_string(spec_file).map_err(|e| e.to_string())?;
                let (computed, content) = store::assemble_drv(&spec, &read)?;
                let d = drv::parse(content.as_bytes()).map_err(|e| e.to_string())?;
                let name = store::name_from_store_path(&computed)
                    .ok_or_else(|| format!("computed path {computed} is malformed"))?;
                let mut refs: Vec<String> = d.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                refs.extend(d.input_srcs.iter().cloned());
                // The GUIX daemon's worker-protocol socket — deliberately NOT
                // TD_DAEMON_SOCKET: that env var names td's OWN build daemon (a line
                // protocol) since the machine-wide limiter landed, and check.sh exports
                // it loop-wide, so reading it here dialed the wrong daemon and spoke
                // binary worker-protocol at a line reader — the newline never came and
                // the td daemon's accept thread blocked forever (the machine-wide wedge
                // the daemon's read-timeout now bounds; this is the caller-side fix).
                let socket = std::env::var("TD_GUIX_DAEMON_SOCKET")
                    .unwrap_or_else(|_| daemon::DEFAULT_SOCKET.to_string());
                let mut dm = daemon::Daemon::connect(&socket)
                    .map_err(|e| format!("connect {socket}: {e}"))?;
                let added = dm
                    .add_text_to_store(&name, content.as_bytes(), &refs)
                    .map_err(|e| e.to_string())?;
                if added != computed {
                    return Err(format!(
                        "daemon registered {added} but td computed {computed}"
                    ));
                }
                Ok(added)
            };
            match run() {
                Ok(path) => {
                    println!("{path}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: drv-assemble {spec_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // S3b/S3c — execute the drv in the userns sandbox and register the
        // outputs. CLOSURE is a file listing every store path the build may
        // see, one per line; writes land under SCRATCH/newstore and the v1
        // registration record under
        // SCRATCH/registration.
        Some("build") if args.len() == 5 => {
            let (drv_path, closure_file, scratch) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                let closure: Vec<String> = std::fs::read_to_string(closure_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                build_and_register(drv_path, &closure, Path::new(scratch)).map(|_| ())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build {drv_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder realize: REALIZE a derivation with NO guix-daemon in the path.
        // td computes the build's input closure ITSELF — its own SQLite reader
        // (store_db_read) over the store DB's `Refs` graph, the job `guix gc -R`
        // (the daemon) used to do (gate td-drv-build, line "stage the input
        // closure"). It then executes the build in its userns sandbox and writes
        // the registration record (via build_and_register). STORE-DB supplies the
        // reference graph of the already-realized inputs — the Guix toolchain,
        // retired LAST (§5) — so td realizes only the TOP derivation; reading guix's
        // live /var/guix/db/db.sqlite with td's OWN reader is "own, then diverge"
        // (the store is shared; the reader is td's, no daemon process). The
        // guix-daemon is no longer in the realize path, and neither is its store DB:
        // the input closure is CONTENT-SCANNED from the seed STORE-DIR (no /var/guix/db).
        // Usage:
        //   realize DRV STORE-DIR SCRATCH
        Some("realize") if args.len() == 5 => {
            let (drv_path, store_dir, scratch) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                // Honor the optional td-OWNED stage0 builder override (TD_BUILDER_PATH/STORE/DB)
                // exactly as the daemon's realize path does — a td-ASSEMBLED drv (e.g. corpus
                // hello) names the stage0 td-builder as its builder, which is NOT in /gnu/store,
                // so without the override the content-scanned closure can't stage it. Unset env
                // ⇒ None ⇒ identical to the prior behavior. The override rides into closure.txt,
                // so a later `td-builder build`/`check` of this drv stages the builder too.
                let ov = builder_override_from_env()?;
                realize_drv(drv_path, std::slice::from_ref(store_dir), &store::store_dir(), &[], Path::new(scratch), &[], ov.as_ref(), None)
                    .map(|_| ())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: realize {drv_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder daemon — td's OWN persistent build daemon: a long-running
        // process that realizes derivations served over a Unix socket, instead of
        // guix-daemon (own-builder-daemon track). Each request realizes via the
        // exact `realize_drv` path (same sandbox/NEWPID, daemon-free) into a
        // CONTENT-ADDRESSED per-drv scratch dir under SCRATCH-BASE — keyed by the
        // drv's (content-addressed) output basename, so a repeat request for the
        // SAME drv reuses + NAR-verifies a valid prior realization (cached_realization)
        // instead of rebuilding (guix-daemon parity: a valid path is not rebuilt).
        // The response carries the realized output's canonical store path + its
        // host-side path under that scratch.
        //
        // Optional td-OWNED builder (same convention as build-recipe, bootstrap brick 2):
        // with TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB set together, a drv whose
        // builder is td's stage0 td-builder (a binary guix never produced) is realized by
        // staging that builder from TD_BUILDER_STORE with its direct refs from
        // TD_BUILDER_DB — so the daemon builds with td's OWN builder, not the guix-built
        // one, and needs no new `guix build -e' packager site. The override is matched by
        // PATH (only the drv root equal to its canonical is re-keyed), so it is a harmless
        // no-op for a drv that does not name the stage0 (e.g. the guile probes of gate 358).
        // Usage:  daemon SOCKET STORE-DIR SCRATCH-BASE
        Some("daemon") if args.len() == 5 => {
            let (socket, seed_dir, scratch) = (args[2].clone(), args[3].clone(), args[4].clone());
            // Fail fast on a half-set builder override (the children re-read the same env).
            if let Err(e) = builder_override_from_env() {
                eprintln!("td-builder: daemon: {e}");
                return ExitCode::FAILURE;
            }
            let _ = std::fs::create_dir_all(&scratch);
            let budget = daemon_budget();
            let exe = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("td-builder: daemon: current_exe: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Per-key dedup: concurrent requests for the SAME output serialize on one lock,
            // so the drv builds once (the 2nd cache-hits) and two builds never race the same
            // content-addressed scratch — the guix-daemon "a valid path is built once"
            // property, preserved across concurrency and across agents (one shared daemon).
            let keymap: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>> =
                std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
            let handle = move |req: &str| -> Result<String, String> {
                // Request grammar: "<drv> [SEED-DIR BP BS BD]" (build) or "CHECK <drv> [SEED-DIR
                // BP BS BD]" (reproducibility). The optional trailing fields are the SEED store
                // DIR (content-scanned for the input closure — #267 retired the /var/guix/db read)
                // and the td-owned builder override (TD_BUILDER_PATH/STORE/DB). Both are carried
                // PER REQUEST because ONE shared daemon serves many worktrees: each declares the
                // seed store dir its inputs come from and the stage0 builder its drv names (bound
                // at identical absolute paths in every sandbox, so the daemon on the host opens
                // exactly what the submitter names). Absent → the child uses the daemon's own
                // start-time seed dir + inherited env (gates 358/359 pass a bare drv).
                let mut toks = req.split_whitespace();
                let first = toks.next().ok_or_else(|| "empty request".to_string())?;
                let (sub, drv) = if first == "CHECK" {
                    ("daemon-check", toks.next().ok_or_else(|| "CHECK: missing drv".to_string())?)
                } else {
                    ("daemon-build", first)
                };
                let rest: Vec<&str> = toks.collect();
                let (seed_dir_req, override_env): (String, Vec<(&str, &str)>) = match rest.as_slice() {
                    [sdb, bp, bs, bd] => (
                        (*sdb).to_string(),
                        vec![
                            ("TD_BUILDER_PATH", *bp),
                            ("TD_BUILDER_STORE", *bs),
                            ("TD_BUILDER_DB", *bd),
                        ],
                    ),
                    [] => (seed_dir.clone(), Vec::new()),
                    _ => {
                        return Err(format!(
                            "malformed request (expected DRV [SEED-DIR BUILDER_PATH BUILDER_STORE BUILDER_DB]): {req}"
                        ))
                    }
                };
                let key = drv_scratch_key(drv)?;
                let keylock = {
                    let mut m = keymap.lock().unwrap();
                    m.entry(key)
                        .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
                        .clone()
                };
                let _kg = keylock.lock().unwrap();
                // Each build runs in its OWN child td-builder process (Command = the safe
                // fork+exec): an in-process fork on a daemon thread is unsound (sandbox::build
                // mutates the process CWD + forks with heavy pre-exec work). The child's stderr
                // is inherited so the daemon log keeps the CACHE HIT/MISS lines (gate
                // daemon-recipe greps them).
                let mut cmd = Command::new(&exe);
                cmd.arg(sub)
                    .arg(drv)
                    .arg(&seed_dir_req)
                    .arg(&scratch)
                    .stderr(std::process::Stdio::inherit());
                for (k, v) in &override_env {
                    cmd.env(k, v);
                }
                let out = cmd
                    .output()
                    .map_err(|e| format!("spawn {sub} for {drv}: {e}"))?;
                if !out.status.success() {
                    return Err(format!("{sub} failed for {drv} (see daemon log)"));
                }
                let stdout = String::from_utf8_lossy(&out.stdout);
                stdout
                    .lines()
                    .find_map(|l| l.strip_prefix("OK "))
                    .map(str::to_string)
                    .ok_or_else(|| format!("{sub}: no OK line for {drv}"))
            };
            // Reserve free memory before admitting a build — the global OOM guard on this
            // swapless host, shared by every daemon via /proc/meminfo (bounds machine-wide
            // memory even when per-binary daemons fragment the concurrency budget).
            let min_free_gib = std::env::var("TD_MIN_FREE_GIB")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(4.0);
            match build_daemon::serve(&socket, budget, min_free_gib, handle) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: daemon: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder daemon-build / daemon-check — the per-build CHILD processes the daemon
        // spawns (one build per process = safe fork + full isolation). daemon-build realizes
        // one drv into a content-addressed keyed scratch (guix-daemon-parity cache reuse) and
        // prints `OK <canonical> <host>`; daemon-check reproducibility-double-builds it and
        // prints `OK repro <canonical> <host>`. Both read the td-owned builder from
        // TD_BUILDER_* (inherited from the daemon). Usage: daemon-build|daemon-check DRV STORE-DB SCRATCH-BASE
        Some("daemon-build") if args.len() == 5 => {
            match daemon_realize_one(&args[2], &args[3], Path::new(&args[4])) {
                Ok((canon, host, hit)) => {
                    println!("OK {canon} {host} {}", if hit { "hit" } else { "built" });
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-build {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        Some("daemon-check") if args.len() == 5 => {
            match daemon_check_one(&args[2], &args[3], Path::new(&args[4])) {
                Ok((canon, host)) => {
                    println!("OK repro {canon} {host}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-check {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder daemon-request — the in-process client for `daemon` (so a
        // caller needs no nc/socat): connect to SOCKET, send REQUEST, print the daemon's
        // single-line response and exit 0 only on "OK …". REQUEST is a drv path (build),
        // "CHECK <drv>" (reproducibility double-build), or "SHUTDOWN". Usage:
        //   daemon-request SOCKET REQUEST
        Some("daemon-request") if args.len() == 4 => {
            let (socket, drv) = (&args[2], &args[3]);
            match build_daemon::request(socket, drv) {
                Ok(resp) => {
                    println!("{resp}");
                    if resp.starts_with("OK ") {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("td-builder: daemon-request: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder assemble-recipe — ASSEMBLE a recipe's `.drv` with NO Guile and
        // NO realize (own-builder-daemon §5): read RECIPE-JSON + LOCK and assemble the
        // `.drv` (store::assemble_drv) to SCRATCH/<name>-<version>.drv, WITHOUT building
        // it — so a SEPARATE process (the persistent build daemon) realizes the
        // td-assembled drv. The drv's builder is td's stage0 td-builder when
        // TD_BUILDER_PATH is set (matching the daemon's TD_BUILDER_* override), else the
        // running binary (self_store_path). Prints `DRV=<file>` then one
        // `OUT=<name> <store-path>` per output. Usage:
        //   assemble-recipe RECIPE-JSON-FILE LOCK SCRATCH
        Some("assemble-recipe") if args.len() == 5 => {
            let (recipe_file, lock, scratch) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                let recipe_json =
                    std::fs::read_to_string(recipe_file).map_err(|e| e.to_string())?;
                let builder_path = match std::env::var("TD_BUILDER_PATH").ok() {
                    Some(p) => p,
                    None => self_store_path()?,
                };
                let (drv_path, drv_file, parsed, _source) =
                    assemble_recipe_drv(&recipe_json, lock, Path::new(scratch), &builder_path, None)?;
                eprintln!(
                    "td-builder: assemble-recipe assembled {drv_path} (no guix (derivation), no Guile, no realize)"
                );
                println!("DRV={}", drv_file.display());
                for o in &parsed.outputs {
                    println!("OUT={} {}", o.name, o.path);
                }
                Ok(())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: assemble-recipe {recipe_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder build-recipe — build a TS recipe with NO Guile and NO
        // guix-daemon in the path: read the recipe JSON (ts-eval produced it,
        // Guile-free), resolve every input from the pinned LOCK (no
        // specification->package), assemble the .drv itself, and realize it. The
        // toolchain + lock are the guix-built SEED (§5, retired last). The optional
        // trailing SRC-STORE-DIR + SRC-DB make the `<name>-source` a td-OWNED source
        // (interned by td's store-add-recursive, no `guix repl`); omitted → a
        // daemon-resident source path, as before. STORE-DIR is the seed store DIRECTORY
        // whose bytes the input closure is CONTENT-SCANNED over (no /var/guix/db). Usage:
        //   build-recipe RECIPE-JSON-FILE LOCK SCRATCH STORE-DIR [SRC-STORE-DIR SRC-DB]
        Some("build-recipe") if args.len() == 6 || args.len() == 8 || args.len() == 11 => {
            let (recipe_file, lock, scratch, store_dir) =
                (&args[2], &args[3], &args[4], &args[5]);
            let src_store = if args.len() >= 8 {
                Some((args[6].as_str(), args[7].as_str()))
            } else {
                None
            };
            // Optional td-OWNED vendored-crate tree (the guix-free crate path): td interned
            // the crate SET itself (store-add-recursive). VENDOR-CANONICAL is its store path,
            // VENDOR-STORE the td store dir, VENDOR-DB its db. run_rust vendors from it
            // (TD_VENDOR_DIR) — no `/gnu/store` crate, no guix-daemon FOD.
            //   build-recipe RECIPE LOCK SCRATCH STORE-DB [SRC-STORE SRC-DB [VENDOR-CANONICAL VENDOR-STORE VENDOR-DB]]
            let vendor_store = if args.len() == 11 {
                Some((args[8].as_str(), args[9].as_str(), args[10].as_str()))
            } else {
                None
            };
            // Optional td-OWNED builder (bootstrap brick 2): all three env vars set
            // together → the drv's builder is a td-placed stage0 (store-add-builder),
            // not the running guix-built binary. TD_BUILDER_PATH is its canonical store
            // path, TD_BUILDER_STORE the td store dir it was restored under, and
            // TD_BUILDER_DB the db registering it + its direct refs.
            let bp = std::env::var("TD_BUILDER_PATH").ok();
            let bs = std::env::var("TD_BUILDER_STORE").ok();
            let bd = std::env::var("TD_BUILDER_DB").ok();
            // North-Star step 2: build from the UNPACKED SEED, not a host guix. With
            // TD_SEED_STORE + TD_SEED_DB set (a `td-builder seed-unpack` output), the input
            // closure is CONTENT-SCANNED from the unpacked seed store and every seed input
            // binds from it (TD_SEED_STORE/<base>) — so STORE-DIR and the live /gnu/store are
            // out of the build path. Set together; the build is otherwise identical (same drv,
            // same output). (TD_SEED_DB is the legacy DB companion; the content-scan of
            // TD_SEED_STORE now supplies the closure, so it is no longer read.)
            let seed_store = std::env::var("TD_SEED_STORE").ok();
            let seed_db = std::env::var("TD_SEED_DB").ok();
            // Optional PERSISTENT store (the incremental /td/store the loop builds into):
            // set TD_PERSIST_STORE + TD_PERSIST_DB together and the build reads an
            // already-built output back from there (skip) or, on a miss, commits its fresh
            // output into it (build-into) — build-into / read-back across invocations.
            let persist_store = std::env::var("TD_PERSIST_STORE").ok().filter(|s| !s.is_empty());
            let persist_db = std::env::var("TD_PERSIST_DB").ok().filter(|s| !s.is_empty());
            let run = || -> Result<(), String> {
                let persist = match (&persist_store, &persist_db) {
                    (Some(s), Some(d)) => Some((s.as_str(), d.as_str())),
                    (None, None) => None,
                    _ => return Err("TD_PERSIST_STORE/TD_PERSIST_DB must be set together".into()),
                };
                let builder_store = match (&bp, &bs, &bd) {
                    (Some(p), Some(s), Some(d)) => Some((p.as_str(), s.as_str(), d.as_str())),
                    (None, None, None) => None,
                    _ => {
                        return Err(
                            "TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together"
                                .into(),
                        )
                    }
                };
                // The seed staging dir's entries are guix-captured bytes whose canonical
                // home is /gnu/store even when the BUILD targets TD_STORE_DIR=/td/store
                // (#292 — gate 377's collapse); td-built copies inside it are restored to
                // their /td/store canonicals from the roots + TD_EXTRA_DBS. Without a seed,
                // the scanned dir IS the live store, canonical where it sits.
                let (seed_store_dirs, seed_prefix, td_store): (Vec<String>, &str, Option<&Path>) =
                    match (&seed_store, &seed_db) {
                        (Some(s), Some(_d)) => (vec![s.clone()], store::STORE_DIR, Some(Path::new(s))),
                        (None, None) => (vec![store_dir.clone()], store_dir.as_str(), None),
                        _ => return Err("TD_SEED_STORE/TD_SEED_DB must be set together".into()),
                    };
                // TD_EXTRA_DBS (colon-separated) merges ADDITIONAL td-OWNED store DBs alongside the
                // content-scanned seed dir — e.g. a td-BUILT toolchain's own db (its /td/store outputs
                // + refs) chained beside the guix seed, so a corpus recipe builds with the /td/store
                // toolchain (brick 8). Those deps' bytes live OUTSIDE the seed dir, so their refs come
                // from the db they wrote; the FILES are staged from td_store/<base> like any td dep.
                // This only ADDS closure edges. Empty/unset → pure seed content-scan.
                let mut extra_dbs: Vec<String> = Vec::new();
                if let Ok(extra) = std::env::var("TD_EXTRA_DBS") {
                    for d in extra.split(':').filter(|s| !s.is_empty()) {
                        extra_dbs.push(d.to_string());
                    }
                }
                let recipe_json =
                    std::fs::read_to_string(recipe_file).map_err(|e| e.to_string())?;
                build_recipe(
                    &recipe_json,
                    lock,
                    Path::new(scratch),
                    &seed_store_dirs,
                    seed_prefix,
                    &extra_dbs,
                    src_store,
                    vendor_store,
                    builder_store,
                    td_store,
                    persist,
                )
                .map(|_| ())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build-recipe {recipe_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder build-plan — realize a TOPO chain of recipes where a
        // downstream step consumes an UPSTREAM step's td-BUILT output instead of a
        // guix store path (a `td-recipe-output` lock entry). PLAN is `step
        // RECIPE-JSON LOCK` per line, in dependency order; the closure of each step
        // content-scans GUIX-STORE ∪ the prior steps' td.dbs, staged from a shared td-store.
        // Usage: build-plan PLAN GUIX-STORE SCRATCH
        Some("build-plan") if args.len() == 5 => {
            let (plan_file, guix_store, scratch) = (&args[2], &args[3], &args[4]);
            let bov = match builder_store_env() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("td-builder: build-plan {plan_file}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let builder_store = bov.as_ref().map(|(p, s, d)| (p.as_str(), s.as_str(), d.as_str()));
            match build_plan(plan_file, guix_store, Path::new(scratch), builder_store) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build-plan {plan_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder build-plan --auto — GENERATE the plan from the recipe GRAPH (no
        // hand-written plan or manifest): topo-sort TARGET's owned-input closure, mark each
        // owned-input dep `td-recipe-output`, and run it. An input is owned iff
        // RECIPE-DIR/<name>.json and LOCK-DIR/<name>-no-guix.lock both exist.
        // Usage: build-plan --auto TARGET RECIPE-DIR LOCK-DIR GUIX-STORE SCRATCH
        Some("build-plan") if args.len() == 8 && args[2] == "--auto" => {
            let (target, recipe_dir, lock_dir, guix_store, scratch) =
                (&args[3], &args[4], &args[5], &args[6], &args[7]);
            let bov = match builder_store_env() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("td-builder: build-plan --auto {target}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let builder_store = bov.as_ref().map(|(p, s, d)| (p.as_str(), s.as_str(), d.as_str()));
            match build_plan_auto(target, recipe_dir, lock_dir, guix_store, Path::new(scratch), builder_store) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build-plan --auto {target}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-check: td OWNS the reproducibility oracle. Execute the SAME .drv
        // TWICE in two independent userns sandbox runs and compare the per-output
        // NAR hashes — td's own `guix build --check`, with no daemon in the
        // verdict. CLOSURE lists every store path the build may see (one per
        // line); the two builds land under SCRATCH/r1 and SCRATCH/r2. Exits 0 if
        // every output is bit-for-bit identical across the two builds, 3 if any
        // output diverges (NON-REPRODUCIBLE — a FAILING test per prime directive 1).
        // The drv reproducibility double-build. RENAMED from `check` when the
        // loop entry (`td-builder check [GOAL...]`) took that verb — every
        // caller (tests/*.sh + the gate bodies) was switched in the same change,
        // so there is no argument-shape sniffing between the two features.
        Some("check-drv") if args.len() == 5 => {
            let (drv_path, closure_file, scratch) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<bool, String> {
                let bytes = std::fs::read(drv_path).map_err(|e| e.to_string())?;
                let parsed = drv::parse(&bytes).map_err(|e| e.to_string())?;
                let closure: Vec<String> = std::fs::read_to_string(closure_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                let scratch1 = Path::new(scratch).join("r1");
                let scratch2 = Path::new(scratch).join("r2");
                // Two independent builds of the same derivation.
                let out1 = sandbox::build(&parsed, drv_path, &closure, &scratch1)
                    .map_err(|e| e.to_string())?;
                let out2 = sandbox::build(&parsed, drv_path, &closure, &scratch2)
                    .map_err(|e| e.to_string())?;
                let mut reproducible = true;
                for (name, host1) in &out1 {
                    let host2 = &out2
                        .iter()
                        .find(|(n, _)| n == name)
                        .ok_or_else(|| format!("output `{name}' missing from the second build"))?
                        .1;
                    let store_path = &parsed
                        .outputs
                        .iter()
                        .find(|o| &o.name == name)
                        .expect("output came from this drv")
                        .path;
                    let h1 = nar_hash_path(host1).map_err(|e| e.to_string())?;
                    let h2 = nar_hash_path(host2).map_err(|e| e.to_string())?;
                    if h1 == h2 {
                        println!("CHECK {name} {store_path} {h1} reproducible");
                    } else {
                        println!("CHECK {name} {store_path} {h1} != {h2} NON-REPRODUCIBLE");
                        reproducible = false;
                    }
                }
                Ok(reproducible)
            };
            match run() {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => {
                    eprintln!("td-builder: check {drv_path}: NOT reproducible");
                    ExitCode::from(3)
                }
                Err(e) => {
                    eprintln!("td-builder: check {drv_path}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder shell — td's own `guix shell` (NOT a container; the default,
        // non-`-C` form): resolve the named packages, compose the command's PATH
        // from their outputs, run it. Resolution stays on the guix package oracle
        // for v1; the env composition + exec are td's own (see run_shell). The
        // durable assertion is behavioral — the command actually runs with the
        // package on PATH. Usage:
        //   shell PKG... [-- CMD ARGS...]
        Some("shell") => match run_shell(&args[2..]) {
            Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
            Err(e) => {
                eprintln!("td-builder: shell: {e}");
                ExitCode::FAILURE
            }
        },
        // td-builder profile — build a profile symlink tree (the user-package-manager
        // profile layer): union the bin/sbin of the given package outputs into PROFILE-DIR.
        // See build_profile. Usage: profile PROFILE-DIR PKG-OUT...
        // profile [--store-native] PROFILE-DIR PKG-OUT… — union the packages' bin/sbin into a
        // symlink-tree profile. With --store-native the links target the LOGICAL store
        // (store::store_dir(), e.g. /td/store) so the profile resolves in a store-ns own-root.
        Some("profile") if args.len() >= 4 => {
            let (store_native, rest): (bool, &[String]) = if args[2] == "--store-native" {
                (true, &args[3..])
            } else {
                (false, &args[2..])
            };
            if rest.len() < 2 {
                eprintln!("usage: td-builder profile [--store-native] PROFILE-DIR PKG-OUT...");
                ExitCode::FAILURE
            } else {
                let sd = store::store_dir();
                let prefix = if store_native { Some(sd.as_str()) } else { None };
                match build_profile(&rest[0], &rest[1..], prefix) {
                    Ok(n) => {
                        eprintln!(
                            "td-builder: profile {} — linked {n} entr{}{}",
                            rest[0],
                            if n == 1 { "y" } else { "ies" },
                            if store_native { " (store-native)" } else { "" }
                        );
                        println!("{}", rest[0]);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("td-builder: profile: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
        // td-builder store-relocate — relocate ROOT's closure from guix's /gnu/store into
        // DEST-DIR as a td /td/store seed (size-preserving /gnu/store -> /td//store rewrite;
        // see relocate_closure). user-pm Phase 2: the one-time break from guix. Usage:
        //   store-relocate STORE-DB ROOT DEST-DIR
        Some("store-relocate") if args.len() == 5 => {
            match relocate_closure(&args[2], &args[3], &args[4]) {
                Ok((paths, files)) => {
                    eprintln!(
                        "td-builder: store-relocate — {paths} store path(s) -> {}, rewrote {files} file(s)/symlink(s) /gnu/store -> /td//store",
                        args[4]
                    );
                    println!("{}", args[4]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-relocate: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // elf-interp FILE — print FILE's program interpreter (PT_INTERP), or nothing for a
        // shared object. td's OWN ELF reader (no patchelf / no guix tool).
        Some("elf-interp") if args.len() == 3 => match elf::read_interp(Path::new(&args[2])) {
            Ok(Some(i)) => {
                println!("{i}");
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::SUCCESS, // no interpreter (e.g. a .so)
            Err(e) => {
                eprintln!("td-builder: elf-interp {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // elf-set-interp FILE NEW — rewrite FILE's PT_INTERP to NEW: in place when it fits
        // the existing slot, else GROWN (string appended at EOF, mapped by repurposing the
        // PT_NOTE segment into a covering PT_LOAD — see elf::set_interp), so a full hashed
        // /td/store/<hash>-glibc.../ld loader path fits. The one patchelf feature the
        // rust-store-native relink needs, owned by td in Rust so the build path adds NO guix tool.
        Some("elf-set-interp") if args.len() == 4 => {
            match elf::set_interp(Path::new(&args[2]), &args[3]) {
                Ok(()) => {
                    eprintln!("td-builder: elf-set-interp {} -> {}", args[2], args[3]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: elf-set-interp {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // elf-rpath FILE — print FILE's run-path (DT_RUNPATH, else legacy DT_RPATH), or
        // nothing for a static binary / one with no run-path. td's OWN ELF reader.
        Some("elf-rpath") if args.len() == 3 => match elf::read_rpath(Path::new(&args[2])) {
            Ok(Some(r)) => {
                println!("{r}");
                ExitCode::SUCCESS
            }
            Ok(None) => ExitCode::SUCCESS, // no run-path
            Err(e) => {
                eprintln!("td-builder: elf-rpath {}: {e}", args[2]);
                ExitCode::FAILURE
            }
        },
        // elf-set-rpath FILE NEW — rewrite FILE's DT_RPATH/DT_RUNPATH to NEW in place (must
        // fit the existing .dynstr slot). Makes a toolchain binary self-sufficient — bake an
        // absolute /td/store run-path so it finds its shared libc with no LD_LIBRARY_PATH
        // wrapper. The second patchelf feature td owns in Rust (no guix tool on the path).
        Some("elf-set-rpath") if args.len() == 4 => {
            match elf::set_rpath(Path::new(&args[2]), &args[3]) {
                Ok(()) => {
                    eprintln!("td-builder: elf-set-rpath {} -> {}", args[2], args[3]);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: elf-set-rpath {}: {e}", args[2]);
                    ExitCode::FAILURE
                }
            }
        }
        // loop-sandbox: the DEV-SHELL — run a command inside td's own hermetic
        // container (pivot into a fresh root exposing the WHOLE /gnu/store (ro),
        // the daemon socket /var/guix, /proc, /dev; host-guix on PATH; its own
        // loopback-only netns), toward replacing `guix shell -C`. With
        // `--expose-cwd` it adds the FULL loop env (worktree + cgroups + guix
        // cache, caller PATH + TD_SUBST_*/TD_DAEMON_* preserved, chdir into the
        // cwd) so a real rung runs as under `guix shell -C`.
        //
        // GUIX-LESS provisioning (host-sandbox-stage0 inc2 — the daily-suite VM):
        //   --store-from DIR : bind DIR (an UNPACKED SEED store, e.g.
        //                      <seed>/store/gnu/store) at /gnu/store INSIDE the
        //                      sandbox instead of the host /gnu/store, so the loop
        //                      toolchain resolves from the seed and the host store
        //                      is absent — the substrate for a VM with no guix.
        //   --no-daemon      : do NOT bind /var/guix (no guix-daemon socket/GC
        //                      roots). The build path uses td-builder's own build
        //                      jail (its own newstore), not the daemon, so the
        //                      shell needs no /var/guix.
        //   --store-at DEST  : where --store-from is mounted INSIDE (default
        //                      /gnu/store). Pass /td/store for td's own store-native
        //                      harness (busybox/make/td-builder relinked to
        //                      /td/store/ld); then the host /gnu/store is NOT bound at
        //                      all — the guix-byte-free loop substrate.
        // Without these flags the binds are byte-identical to before.
        // Usage:
        //   host-sandbox [--expose-cwd] [--store-from DIR [--store-at DEST]] [--no-daemon] -- CMD ARGS...
        Some("host-sandbox") if args.len() >= 4 => {
            let parsed = match parse_host_sandbox_args(&args) {
                Ok(p) => p,
                Err(msg) => {
                    eprintln!("td-builder: host-sandbox: {msg}");
                    return ExitCode::from(2);
                }
            };
            let HostSandboxArgs { expose_cwd, store_from, store_at, no_daemon, cmd, cmd_args } =
                parsed;
            let run = || -> Result<std::process::ExitStatus, String> {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/home/td".to_string());
                // The base exposure set: the whole store (ro) and the daemon
                // socket + GC roots (rw). /dev is NOT bound — host_shell builds a
                // minimal synthetic /dev (standard char devices + shm + pts + fd
                // links, like `guix shell -C`) instead of leaking the host device
                // tree (kmsg/kvm/disks/input/GPUs). /proc is NOT bound either —
                // host_shell mounts a FRESH procfs reflecting the sandbox's own
                // PID namespace, so the host /proc never leaks in and nested
                // containers see a private /proc.
                //
                // The store bind: by default the host /gnu/store; with
                // --store-from DIR the UNPACKED store DIR, mounted at DEST
                // (--store-at, default /gnu/store) so its binaries' hardcoded
                // interpreters resolve. With --store-at /td/store (td's own
                // store-native harness) the host /gnu/store is then absent — the
                // guix-byte-free loop substrate. --no-daemon drops /var/guix.
                let mut binds = Vec::new();
                match store_from.as_deref() {
                    Some(dir) => binds.push(sandbox::Bind {
                        src: dir.to_string(),
                        dest: Some(store_at.clone().unwrap_or_else(|| "/gnu/store".to_string())),
                        readonly: true,
                        ro_optional: false,
                    }),
                    None => binds.push(sandbox::Bind {
                        src: "/gnu/store".to_string(),
                        dest: None,
                        readonly: true,
                        ro_optional: false,
                    }),
                }
                if !no_daemon {
                    binds.push(sandbox::Bind {
                        src: "/var/guix".to_string(),
                        dest: None,
                        readonly: false,
                        ro_optional: false,
                    });
                }
                let mut tmpfs = vec!["/tmp".to_string()];
                let mut path_env = String::new();
                let mut workdir = String::new();
                let mut extra_env: Vec<(String, String)> = Vec::new();
                if expose_cwd {
                    let cwd = std::env::current_dir()
                        .map_err(|e| e.to_string())?
                        .to_string_lossy()
                        .into_owned();
                    // Worktree (rw, like guix shell -C's shared cwd), the host
                    // cgroup hierarchy (ro, for crun), and the guix lowering cache
                    // (rw, check.sh --shares it). HOME is a dir on the writable
                    // root tmpfs (created by these binds), so no HOME tmpfs.
                    binds.push(sandbox::Bind { src: cwd.clone(), dest: None, readonly: false, ro_optional: false });
                    if Path::new("/sys/fs/cgroup").is_dir() {
                        // ro is defense-in-depth (crun probes the hierarchy with
                        // --cgroup-manager=disabled, never writing it). A child
                        // userns can't remount-ro the host-owned cgroup2 on some
                        // kernels (EPERM, e.g. the azure CI runner); there the bind
                        // is DETACHED (fail-closed), never left writable — see
                        // Bind::ro_optional. The crun gates that need it run only
                        // locally, where the ro-remount succeeds.
                        binds.push(sandbox::Bind {
                            src: "/sys/fs/cgroup".to_string(),
                            dest: None,
                            readonly: true,
                            ro_optional: true,
                        });
                        // The delegated per-run cgroup dir (issue #328): bound
                        // RW OVER the ro hierarchy so gate-run (inside) can
                        // create per-gate child cgroups + set memory.max. Only
                        // when `td-builder check` probed a delegation; the rest
                        // of the hierarchy stays ro (the crun-probe posture).
                        if let Ok(cg) = std::env::var("TD_CHECK_CGROUP") {
                            if !cg.is_empty() && Path::new(&cg).is_dir() {
                                binds.push(sandbox::Bind {
                                    src: cg,
                                    dest: None,
                                    readonly: false,
                                    ro_optional: false,
                                });
                            }
                        }
                    }
                    let cache = format!("{home}/.cache/guix");
                    if Path::new(&cache).is_dir() {
                        binds.push(sandbox::Bind { src: cache, dest: None, readonly: false, ro_optional: false });
                    }
                    // The persistent signed substitute store (~/.td/subst, populated by the daily) —
                    // READ-ONLY: the loop FETCHES the lock-keyed toolchain closure from it
                    // (x64-toolchain-subst) over its own loopback netns instead of rebuilding ~98 min
                    // from seed, and never writes it. Like the host /gnu/store + guix cache, it is a
                    // declared, exposed input — no network egress (resolve-toolchain serves loopback).
                    let subst = format!("{home}/.td/subst");
                    if Path::new(&subst).is_dir() {
                        binds.push(sandbox::Bind { src: subst, dest: None, readonly: true, ro_optional: false });
                    }
                    // The ONE shared build daemon's socket + output store (~/.td/build-daemon,
                    // started on the host by the `td-builder check` prelude). The corpus build
                    // (inside this sandbox) SUBMITS drvs to it over the socket and reads its
                    // output back, so it must be visible at the SAME absolute path in every
                    // check sandbox — RW (connect to the socket; read the store). Bound only
                    // when present; a cold machine without a running daemon simply lacks it.
                    let bdd = format!("{home}/.td/build-daemon");
                    if Path::new(&bdd).is_dir() {
                        binds.push(sandbox::Bind { src: bdd.clone(), dest: None, readonly: false, ro_optional: false });
                    }
                    // The #317 warm chain-brick cache: when the operator points
                    // TD_CHECK_CHAIN_CACHE at a CUSTOM host path (the default lives
                    // under ~/.td/build-daemon, bound above), bind it RW so warm
                    // bricks actually persist — unbound, the override would silently
                    // write to the sandbox's ephemeral root and vanish on teardown.
                    if let Ok(cc) = std::env::var("TD_CHECK_CHAIN_CACHE") {
                        if !cc.is_empty() && !Path::new(&cc).starts_with(&bdd) {
                            let _ = std::fs::create_dir_all(&cc);
                            if Path::new(&cc).is_dir() {
                                binds.push(sandbox::Bind {
                                    src: cc,
                                    dest: None,
                                    readonly: false,
                                    ro_optional: false,
                                });
                            }
                        }
                    }
                    path_env = std::env::var("PATH").unwrap_or_default();
                    workdir = cwd;
                    for (k, v) in std::env::vars() {
                        // TD_SUBST_* = the host-provisioned
                        // substitute resolver knobs (TD_SUBST_BIN/STORE/PUBKEY) the toolchain gates
                        // read to FETCH the lock-keyed closure instead of building from seed;
                        // TD_DAEMON_* = the shared build daemon's socket (TD_DAEMON_SOCKET) the
                        // corpus build submits to. (TD_CHECK_CHAIN_CACHE — the #317 warm
                        // chain-brick knob, including its set-and-empty force-cold form —
                        // rides the TD_CHECK_ prefix.)
                        if k.starts_with("TD_CHECK_")
                            || k.starts_with("TD_SUBST_")
                            || k.starts_with("TD_DAEMON_")
                        {
                            extra_env.push((k, v));
                        }
                    }
                } else {
                    let guix_bin = host_guix_bin_dir().unwrap_or_default();
                    if !guix_bin.is_empty() {
                        path_env = format!("{guix_bin}:/run/current-system/profile/bin");
                    }
                    tmpfs.push(home.clone());
                }
                let scratch = std::env::temp_dir()
                    .join(format!("td-host-sandbox-{}-{}", sys::getuid(), std::process::id()));
                let _ = std::fs::remove_dir_all(&scratch);
                std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
                let result = sandbox::host_shell(
                    &cmd, &cmd_args, &binds, &tmpfs, &path_env, &home, &workdir, &extra_env,
                    &scratch,
                )
                .map_err(|e| e.to_string());
                // Remove the scratch tree (the sandbox's mounts lived in the
                // child's now-gone mount namespace, so only an empty dir remains
                // here). Previously leaked one dir per run.
                let _ = std::fs::remove_dir_all(&scratch);
                result
            };
            match run() {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
                Err(e) => {
                    eprintln!("td-builder: host-sandbox: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder store-ns — OWN ROOT, td's OWN store at /td/store, NO guix (user-pm
        // Phase 0; human 2026-06-21: break from guix's /gnu/store, not mixed with the local
        // guix install). Enter a user namespace pivoted into a minimal td-owned root
        // (host_shell's fresh-tmpfs root + minimal /dev + private /proc), bind STORE-DIR at
        // `/td/store`, and bind NOTHING from /gnu/store or /var/guix — so inside, `/td/store`
        // IS the store (= STORE-DIR) and the host `/gnu/store` + guix install are ABSENT. A
        // static binary in STORE-DIR runs by absolute path; dynamic content needs the seed
        // relocated to /td/store (Phase 2). Rootless (no daemon, no root), unmixed from guix.
        //   store-ns STORE-DIR -- CMD ARGS...
        Some("store-ns") if args.len() >= 5 && args[3] == "--" => {
            let store_dir = args[2].clone();
            let cmd = args[4].clone();
            let cmd_args: Vec<String> = args[5..].to_vec();
            let run = || -> Result<std::process::ExitStatus, String> {
                if !Path::new(&store_dir).is_dir() {
                    return Err(format!("store dir `{store_dir}' does not exist"));
                }
                // The ONLY bind: the user store at td's prefix. No /gnu/store, no /var/guix.
                let binds = vec![sandbox::Bind {
                    src: store_dir,
                    dest: Some("/td/store".to_string()),
                    readonly: true,
                    ro_optional: false,
                }];
                let tmpfs = vec!["/tmp".to_string()];
                let home = "/tmp".to_string();
                let path_env = "/td/store/bin".to_string();
                let scratch = std::env::temp_dir()
                    .join(format!("td-store-ns-{}-{}", sys::getuid(), std::process::id()));
                let _ = std::fs::remove_dir_all(&scratch);
                std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
                let result = sandbox::host_shell(
                    &cmd, &cmd_args, &binds, &tmpfs, &path_env, &home, "", &[], &scratch,
                )
                .map_err(|e| e.to_string());
                let _ = std::fs::remove_dir_all(&scratch);
                result
            };
            match run() {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
                Err(e) => {
                    eprintln!("td-builder: store-ns: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // (the `resolve` subcommand — a Guile-oracle lock resolver — retired with
        // the guix Guile-lowering gates; native input resolution lives in gate_inputs.rs.)
        // corpus-independence: run AS a derivation's builder, executing the
        // autotools phases in Rust (replaces gnu-build-system). Reads the build
        // environment from env vars (out, TD_SRC, TD_INPUTS, TD_CONFIGURE_FLAGS)
        // that the td-native derivation contract sets on the derivation.
        Some("autotools-build") if args.len() == 2 => match build::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: autotools-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's OWN Rust/cargo build system (the cargo-build-system replacement):
        // builds the TD_SRC crate with `cargo build --offline` and installs
        // TD_RUST_BINS into $out/bin. Sibling of autotools-build; same
        // env-driven derivation-builder contract.
        Some("rust-build") if args.len() == 2 => match build::run_rust() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: rust-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's OWN cmake build system (the cmake-build-system replacement): runs an
        // out-of-source `cmake` configure -> make -> make install over the TD_SRC
        // tree, installing into $out. Sibling of autotools-build/rust-build; same
        // env-driven derivation-builder contract.
        Some("cmake-build") if args.len() == 2 => match build::run_cmake() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: cmake-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's stage0-posix SEED build system (#378): see build::run_stage0.
        // Sibling of autotools-build/rust-build/cmake-build; same env contract.
        Some("stage0-build") if args.len() == 2 => match build::run_stage0() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: stage0-build: {e}");
                ExitCode::FAILURE
            }
        },
        // td's bootstrap-RUNG step executor (#378 slices 2+3): see
        // build::run_mesboot. Same env-driven derivation-builder contract.
        Some("mesboot-build") if args.len() == 2 => match build::run_mesboot() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: mesboot-build: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: td-builder            # print the S1 sentinel");
            eprintln!("       td-builder check [GOAL...]             # the loop: host prelude + sandboxed gate ladder");
            eprintln!("       td-builder gate-run [-j N] [GOAL...]   # the in-sandbox gate scheduler (src/gate_defs/)");
            eprintln!("       td-builder check-rung HARNESS [ARG...] # dev: run a harness inside the loop sandbox");
            eprintln!("       td-builder nar-hash PATH");
            eprintln!("       td-builder nar-restore NARFILE DEST");
            eprintln!("       td-builder subst-export DB STORE-DIR OUTDIR ROOT...");
            eprintln!("       td-builder drv-parse FILE.drv");
            eprintln!("       td-builder drv-refs FILE.drv");
            eprintln!("       td-builder build FILE.drv CLOSURE-FILE SCRATCH-DIR");
            eprintln!("       td-builder check-drv FILE.drv CLOSURE-FILE SCRATCH-DIR");
            eprintln!("       td-builder store-register STORE-PATH DERIVER CANDIDATES-FILE OUT-DB");
            eprintln!("       td-builder store-query DB info|references");
            eprintln!("       td-builder store-closure DB ROOT");
            eprintln!("       td-builder store-closure-scan STORE-DIR[,EXTRA-DIR...] ROOT...");
            eprintln!("       td-builder store-add-text NAME CONTENT-FILE STORE-DIR OUT-DB");
            eprintln!("       td-builder store-add-recursive NAME SRC STORE-DIR OUT-DB");
            eprintln!("       td-builder store-add-referenced NAME CONTENT-FILE REFS-FILE STORE-DIR OUT-DB");
            eprintln!("       td-builder store-add-output OUTPUT DERIVER CLOSURE-FILE STORE-DIR OUT-DB");
            eprintln!("       td-builder store-verify DB STORE-ROOT");
            eprintln!("       td-builder store-gc-sweep STORE-DIR DB ROOT");
            eprintln!("       td-builder resolve LOCKFILE NAME...");
            eprintln!("       td-builder realize FILE.drv STORE-DB SCRATCH-DIR");
            eprintln!("       td-builder build-recipe RECIPE-JSON LOCK SCRATCH-DIR STORE-DB [SRC-STORE-DIR SRC-DB]");
            eprintln!("       td-builder build-plan PLAN GUIX-DB SCRATCH-DIR");
            eprintln!("       td-builder autotools-build   # as a derivation builder");
            eprintln!("       td-builder rust-build        # as a derivation builder (cargo)");
            eprintln!("       td-builder cmake-build       # as a derivation builder (cmake)");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The daemon CHECK verb reuses the build already realized as ONE of the two
    /// independent reproducibility builds — but ONLY when every output tree is actually
    /// present under the build scratch; a missing output must force the fresh-rebuild
    /// fallback (never a comparison against an absent baseline). This pins that decision.
    #[test]
    fn output_trees_present_gates_the_repro_build_reuse() {
        let dir = std::env::temp_dir().join(format!("td-repro-baseline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = "/gnu/store/aaaaaaaaaaaaaaaa-hello-2.12".to_string();
        let b = "/gnu/store/bbbbbbbbbbbbbbbb-hello-2.12-lib".to_string();
        let touch = |canon: &str| {
            let p = daemon_host_path(&dir, canon).unwrap();
            let p = std::path::Path::new(&p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"out").unwrap();
        };
        // Empty ⇒ false: a drv with no outputs must never reuse a vacuous baseline.
        assert!(!output_trees_present(&dir, &[]), "empty canon set must not reuse");
        // Missing ⇒ false (the fallback trigger — VERIFIED-RED for the reuse guard).
        assert!(!output_trees_present(&dir, &[a.clone()]), "absent output must force a rebuild");
        // Present ⇒ true (the loop's normal 2-build path).
        touch(&a);
        assert!(output_trees_present(&dir, &[a.clone()]), "present output must be reusable");
        // Multi-output: reuse only when EVERY output is present.
        assert!(!output_trees_present(&dir, &[a.clone(), b.clone()]), "one missing output must force a rebuild");
        touch(&b);
        assert!(output_trees_present(&dir, &[a, b]), "all outputs present must be reusable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // `td shell` rust crate provisioning: the seed lock = the package lock with the crate
    // FODs and any stale source-key line dropped, then the td-interned source pinned. This
    // is the transform that lets `td shell ripgrep -- rg …` build the real userland from
    // its guix-free crate closure (build-recipe's TD_VENDOR_DIR form).
    #[test]
    fn seed_lock_body_drops_crates_and_pins_interned_source() {
        let lock = "\
# a comment line, kept verbatim
rust /gnu/store/aaa-rust-1.0
coreutils /gnu/store/bbb-coreutils-9.1
ripgrep-source /gnu/store/old-stale-source
serde-1.0.crate /gnu/store/ccc-serde-1.0.crate
memchr-2.7.crate /gnu/store/ddd-memchr.crate
";
        let seed = seed_lock_body(lock, "ripgrep-source", "/gnu/store/zzz-ripgrep-src");
        // The crate FOD lines are gone (no daemon FOD, the guix-free crate path).
        assert!(!seed.contains(".crate "), "crate FOD lines must be dropped:\n{seed}");
        // The stale source-key line is replaced, not duplicated.
        assert_eq!(
            seed.matches("ripgrep-source ").count(),
            1,
            "exactly one source-key line:\n{seed}"
        );
        assert!(!seed.contains("old-stale-source"), "stale source pin removed:\n{seed}");
        assert!(
            seed.contains("ripgrep-source /gnu/store/zzz-ripgrep-src\n"),
            "interned source pinned:\n{seed}"
        );
        // The toolchain seed lines (and comments) survive untouched.
        assert!(seed.contains("# a comment line, kept verbatim\n"), "comment kept:\n{seed}");
        assert!(seed.contains("rust /gnu/store/aaa-rust-1.0\n"), "rust seed kept:\n{seed}");
        assert!(seed.contains("coreutils /gnu/store/bbb-coreutils-9.1\n"), "coreutils kept:\n{seed}");
    }

    // native_seed_lock_body is the `td shell` cutover transform: retarget a seed lock onto the
    // native /td/store toolchain — drop the guix rust/gcc-toolchain lines, keep the retired-last
    // build seed + the source pin, append the native toolchain lines. It mirrors what
    // `tests/rust-x86_64-userland-store-native.sh` does with `grep -vE -- '-rust-|-gcc-toolchain-'`.
    #[test]
    fn native_seed_lock_body_retargets_onto_the_td_store_toolchain() {
        // A realistic post-provision seed lock: guix rust + cargo + gcc-toolchain, the retired-last
        // build seed (coreutils/bash/tar/gzip), and the interned source.
        let seed = "\
xxx-rust-1.93.0 /gnu/store/xxx-rust-1.93.0
yyy-rust-1.93.0-cargo /gnu/store/yyy-rust-1.93.0-cargo
zzz-gcc-toolchain-15.2.0 /gnu/store/zzz-gcc-toolchain-15.2.0
bbb-coreutils-9.1 /gnu/store/bbb-coreutils-9.1
ppp-bash-5.2.37 /gnu/store/ppp-bash-5.2.37
vvv-tar-1.35 /gnu/store/vvv-tar-1.35
ccc-gzip-1.14 /gnu/store/ccc-gzip-1.14
ripgrep-source /gnu/store/interned-ripgrep-src
";
        let native = "\
rust-1.96.0-x86_64-store-native /td/store/qnkl-rust-1.96.0-x86_64-store-native seed
gcc-14.3.0-x86_64-native /td/store/ng-gcc-14.3.0-x86_64-native seed
binutils-2.44-x86_64-native /td/store/nb-binutils-2.44-x86_64-native seed
glibc-2.41-x86_64 /td/store/gl-glibc-2.41-x86_64 seed
";
        let out = native_seed_lock_body(seed, native);

        // The guix rust / cargo / gcc-toolchain lines are GONE — the cutover, checked at the lock.
        assert!(
            !out.lines().any(|l| l.contains("/gnu/store/") && (l.contains("-rust-") || l.contains("-gcc-toolchain-"))),
            "no guix rust/gcc-toolchain line may survive:\n{out}"
        );
        // The retired-last build seed and the source pin are KEPT verbatim.
        for keep in [
            "bbb-coreutils-9.1 /gnu/store/bbb-coreutils-9.1",
            "ppp-bash-5.2.37 /gnu/store/ppp-bash-5.2.37",
            "vvv-tar-1.35 /gnu/store/vvv-tar-1.35",
            "ccc-gzip-1.14 /gnu/store/ccc-gzip-1.14",
            "ripgrep-source /gnu/store/interned-ripgrep-src",
        ] {
            assert!(out.contains(&format!("{keep}\n")), "must keep `{keep}':\n{out}");
        }
        // The native /td/store toolchain lines are appended, each exactly once.
        for nl in native.lines() {
            assert_eq!(out.matches(nl).count(), 1, "native line `{nl}' appended once:\n{out}");
        }
        // No trailing blank line / double newline surprises: the body ends with a single newline.
        assert!(out.ends_with("glibc-2.41-x86_64 /td/store/gl-glibc-2.41-x86_64 seed\n"), "ends clean:\n{out}");
    }

    // A native line containing the substring `-rust-`/`-gcc-toolchain-` (e.g. a hypothetical
    // `…-rust-…` native component) must NOT be dropped: the drop only fires on `/gnu/store/`
    // lines, so appended `/td/store` native lines always survive.
    #[test]
    fn native_seed_lock_body_keeps_td_store_lines_even_if_named_rust() {
        let seed = "aaa-rust-1.93.0 /gnu/store/aaa-rust-1.93.0\nbbb-coreutils-9.1 /gnu/store/bbb-coreutils-9.1\n";
        let native = "some-rust-thing /td/store/hh-some-rust-thing seed\n";
        let out = native_seed_lock_body(seed, native);
        assert!(!out.contains("/gnu/store/aaa-rust-1.93.0"), "guix rust dropped:\n{out}");
        assert!(out.contains("some-rust-thing /td/store/hh-some-rust-thing seed\n"), "td-store native rust line kept:\n{out}");
        assert!(out.contains("bbb-coreutils-9.1 /gnu/store/bbb-coreutils-9.1\n"), "coreutils kept:\n{out}");
    }

    // Empty native lines (defensive): the transform still drops guix toolchain lines and does not
    // append a stray blank line.
    #[test]
    fn native_seed_lock_body_tolerates_empty_native_lines() {
        let seed = "aaa-rust-1.0 /gnu/store/aaa-rust-1.0\nbbb-coreutils-9.1 /gnu/store/bbb-coreutils-9.1\n";
        let out = native_seed_lock_body(seed, "");
        assert!(!out.contains("-rust-"), "guix rust dropped:\n{out}");
        assert!(out.contains("bbb-coreutils-9.1 /gnu/store/bbb-coreutils-9.1\n"), "coreutils kept:\n{out}");
        assert!(!out.contains("\n\n"), "no stray blank line:\n{out}");
    }

    // ---- persistent accumulating store DB (merge_regs) ----------------------
    // These are the durable, daemon-free proof that a td store ACCUMULATES across
    // builds: merge_regs takes the existing db bytes + new outputs and returns a db
    // that holds BOTH, by store path. (The heavy `store-persist` gate exercises the
    // same path end-to-end with a real build across separate invocations.)

    fn reg(path: &str, hash: &str, refs: &[&str]) -> OutputReg {
        OutputReg {
            store_path: path.to_string(),
            nar_hash: hash.to_string(),
            nar_size: 42,
            refs: refs.iter().map(|s| s.to_string()).collect(),
            deriver: format!("{path}.drv"),
        }
    }
    // A path's full ValidPaths row (rowid + hash) or None if it is only a scaffold.
    fn full_row(db: &store_db_read::Db, path: &str) -> Option<(i64, String)> {
        for (rowid, cols) in db.table("ValidPaths").unwrap() {
            if let (Some(store_db_read::Value::Text(p)), Some(store_db_read::Value::Text(h))) =
                (cols.get(1), cols.get(2))
            {
                if p == path && !h.is_empty() {
                    return Some((rowid, h.clone()));
                }
            }
        }
        None
    }
    fn sorted_closure(db: &store_db_read::Db, root: &str) -> Vec<String> {
        let mut c = db.closure(root).unwrap();
        c.sort();
        c
    }

    const A: &str = "/gnu/store/00000000000000000000000000000000-a";
    const B: &str = "/gnu/store/11111111111111111111111111111111-b";
    const X: &str = "/gnu/store/22222222222222222222222222222222-x";

    #[test]
    fn merge_into_empty_registers_output_and_scaffold_ref() {
        // First commit (no existing db): A (full) referencing X (scaffold).
        let bytes = merge_regs(None, &[reg(A, "hashA", &[X])]).unwrap();
        let db = store_db_read::Db::open(bytes).unwrap();
        assert_eq!(full_row(&db, A).map(|r| r.1), Some("hashA".to_string()));
        assert!(full_row(&db, X).is_none(), "a bare reference is a scaffold (no hash)");
        assert_eq!(sorted_closure(&db, A), vec![A.to_string(), X.to_string()]);
    }

    #[test]
    fn merge_accumulates_across_commits_without_clobbering() {
        // Commit A, then commit B (referencing A) into the SAME db: BOTH survive.
        // This is the accumulation property a fresh-write (clobber) lacks — the
        // verified-red is exactly "make merge ignore `existing` → A vanishes here".
        let db1 = merge_regs(None, &[reg(A, "hashA", &[])]).unwrap();
        let db2 = merge_regs(Some(&db1), &[reg(B, "hashB", &[A])]).unwrap();
        let db = store_db_read::Db::open(db2).unwrap();
        assert_eq!(full_row(&db, A).map(|r| r.1), Some("hashA".to_string()), "A NOT clobbered by B's commit");
        assert_eq!(full_row(&db, B).map(|r| r.1), Some("hashB".to_string()));
        // B's closure spans the earlier-committed A (read-back across commits).
        assert_eq!(sorted_closure(&db, B), vec![A.to_string(), B.to_string()]);
    }

    #[test]
    fn merge_is_idempotent_and_byte_deterministic() {
        // Re-committing the same set reproduces the bytes exactly (sorted rowids),
        // so a redundant commit is a safe no-op on the db.
        let once = merge_regs(None, &[reg(A, "hashA", &[X]), reg(B, "hashB", &[A])]).unwrap();
        let twice = merge_regs(Some(&once), &[reg(A, "hashA", &[X]), reg(B, "hashB", &[A])]).unwrap();
        assert_eq!(once, twice, "re-merging the same outputs must be byte-identical");
        // Commit ORDER must not matter either (rowids assigned by sorted path).
        let other = merge_regs(None, &[reg(B, "hashB", &[A]), reg(A, "hashA", &[X])]).unwrap();
        assert_eq!(once, other, "merge result is independent of commit order");
    }

    #[test]
    fn merge_upgrades_scaffold_to_full_row() {
        // A appears first only as B's reference (scaffold), then is committed for
        // real: its row gains the hash in place (no duplicate path row).
        let db1 = merge_regs(None, &[reg(B, "hashB", &[A])]).unwrap();
        assert!(full_row(&store_db_read::Db::open(db1.clone()).unwrap(), A).is_none());
        let db2 = merge_regs(Some(&db1), &[reg(A, "hashA", &[])]).unwrap();
        let db = store_db_read::Db::open(db2).unwrap();
        assert_eq!(full_row(&db, A).map(|r| r.1), Some("hashA".to_string()), "scaffold A upgraded to full");
        let a_rows = db
            .table("ValidPaths")
            .unwrap()
            .iter()
            .filter(|(_, c)| matches!(c.get(1), Some(store_db_read::Value::Text(p)) if p == A))
            .count();
        assert_eq!(a_rows, 1, "A is a single row, not duplicated");
        // B's edge to A is preserved through the upgrade.
        assert_eq!(sorted_closure(&db, B), vec![A.to_string(), B.to_string()]);
    }

    #[test]
    fn registration_text_round_trips_through_parse() {
        // registration_text is the inverse of parse_registration_blocks — a
        // persistent-store read-back writes it so a fresh scratch carries the same
        // registration a real build would (incl. an empty deriver, e.g. a source).
        let regs = vec![
            reg(A, "sha256:aa", &[X]),
            OutputReg {
                store_path: B.to_string(),
                nar_hash: "sha256:bb".to_string(),
                nar_size: 7,
                refs: vec![],
                deriver: String::new(),
            },
        ];
        let parsed = parse_registration_blocks(&registration_text(&regs));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].store_path, A);
        assert_eq!(parsed[0].nar_hash, "sha256:aa");
        assert_eq!(parsed[0].nar_size, 42);
        assert_eq!(parsed[0].refs, vec![X.to_string()]);
        assert_eq!(parsed[0].deriver, format!("{A}.drv"));
        assert_eq!(parsed[1].store_path, B);
        assert_eq!(parsed[1].deriver, "", "an empty deriver round-trips");
    }

    fn one_output_drv(out_path: &str) -> drv::Derivation {
        drv::Derivation {
            outputs: vec![drv::Output {
                name: "out".to_string(),
                path: out_path.to_string(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            platform: String::new(),
            builder: String::new(),
            args: vec![],
            env: vec![],
        }
    }

    #[test]
    fn persistent_realization_hits_stages_and_rejects_miss_or_corrupt() {
        // The cross-invocation SKIP: an output already valid in a persistent store (DB +
        // a tree that re-verifies) is read back (staged into scratch/newstore); an unknown
        // output or a tampered tree is a MISS (rebuild), and a corrupt miss stages nothing.
        let tmp = std::env::temp_dir().join(format!("td-persist-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let store = tmp.join("store");
        let base = "00000000000000000000000000000abc-persist-demo-1";
        let path = format!("/td/store/{base}");
        let tree = store.join(base);
        std::fs::create_dir_all(tree.join("bin")).unwrap();
        std::fs::write(tree.join("bin/run"), b"#!/bin/sh\necho hi\n").unwrap();
        // Register with the hash the checker computes (Scanner over the tree, no refs).
        let mut sc = scan::Scanner::new(&[]).unwrap();
        nar::write_nar(&mut sc, &tree).unwrap();
        let (hash, size, _) = sc.finish();
        let reg = OutputReg {
            store_path: path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs: vec![],
            deriver: format!("{path}.drv"),
        };
        let db = tmp.join("db");
        std::fs::write(&db, merge_regs(None, &[reg]).unwrap()).unwrap();
        let sd = store.to_str().unwrap();

        // HIT: staged into scratch/newstore + the reg returned.
        let s1 = tmp.join("s-hit");
        std::fs::create_dir_all(&s1).unwrap();
        let regs = persistent_realization(&one_output_drv(&path), sd, &db, &s1)
            .unwrap()
            .expect("expected a persistent-store HIT");
        assert_eq!(regs[0].store_path, path);
        assert!(s1.join("newstore").join(base).join("bin/run").exists(), "output tree staged into newstore");

        // MISS: an output path not registered in the persistent DB.
        let s2 = tmp.join("s-miss");
        std::fs::create_dir_all(&s2).unwrap();
        let miss = persistent_realization(
            &one_output_drv("/td/store/11111111111111111111111111111111-other-1"),
            sd,
            &db,
            &s2,
        )
        .unwrap();
        assert!(miss.is_none(), "an unregistered output must be a MISS");

        // CORRUPT: the tree no longer matches the registered hash → MISS, nothing staged.
        std::fs::write(tree.join("bin/run"), b"tampered\n").unwrap();
        let s3 = tmp.join("s-corrupt");
        std::fs::create_dir_all(&s3).unwrap();
        let corrupt = persistent_realization(&one_output_drv(&path), sd, &db, &s3).unwrap();
        assert!(corrupt.is_none(), "a tree that no longer matches its hash must be a MISS");
        assert!(!s3.join("newstore").join(base).exists(), "a corrupt miss must not leave a staged tree");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_registration_blocks_reads_multi_output() {
        let blob = "path /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-o\n\
                    nar-hash sha256:deadbeef\nnar-size 7\n\
                    reference /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep\n\
                    deriver /gnu/store/cccccccccccccccccccccccccccccccc-o.drv\n";
        let regs = parse_registration_blocks(blob);
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].store_path, "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-o");
        assert_eq!(regs[0].nar_hash, "sha256:deadbeef");
        assert_eq!(regs[0].nar_size, 7);
        assert_eq!(regs[0].refs, vec!["/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-dep".to_string()]);
        assert_eq!(regs[0].deriver, "/gnu/store/cccccccccccccccccccccccccccccccc-o.drv");
    }

    // TD_BUILD_NICE policy: default 10 when unset/garbage, honor a valid value,
    // clamp to the kernel's -20..=19 nice range. Pure parse, no process state.
    #[test]
    fn build_nice_target_parses_and_clamps() {
        assert_eq!(parse_build_nice(None), 10, "unset -> default 10");
        assert_eq!(parse_build_nice(Some("garbage".into())), 10, "garbage -> default");
        assert_eq!(parse_build_nice(Some("".into())), 10, "empty -> default");
        assert_eq!(parse_build_nice(Some(" 15 ".into())), 15, "trimmed valid value");
        assert_eq!(parse_build_nice(Some("0".into())), 0, "0 is honored (opt out)");
        assert_eq!(parse_build_nice(Some("99".into())), 19, "clamp above max");
        assert_eq!(parse_build_nice(Some("-99".into())), -20, "clamp below min");
    }

    // host-sandbox flag grammar (the loop container). The `--store-at` flag (inc2c)
    // lets the harness be bound at /td/store instead of the hardcoded /gnu/store.
    fn hs(argv: &[&str]) -> Result<HostSandboxArgs, String> {
        let v: Vec<String> = std::iter::once("td-builder".to_string())
            .chain(std::iter::once("host-sandbox".to_string()))
            .chain(argv.iter().map(|s| s.to_string()))
            .collect();
        parse_host_sandbox_args(&v)
    }

    #[test]
    fn host_sandbox_store_at_for_td_store_harness() {
        // The inc2c path: bind td's own harness at /td/store.
        let p = hs(&["--store-from", "/h/store", "--store-at", "/td/store", "--no-daemon",
                     "--", "/td/store/bin/busybox", "sh", "-c", "true"])
            .expect("valid");
        assert_eq!(p.store_from.as_deref(), Some("/h/store"));
        assert_eq!(p.store_at.as_deref(), Some("/td/store"));
        assert!(p.no_daemon, "--no-daemon parsed");
        assert!(!p.expose_cwd);
        assert_eq!(p.cmd, "/td/store/bin/busybox");
        assert_eq!(p.cmd_args, vec!["sh", "-c", "true"]);
    }

    #[test]
    fn host_sandbox_back_compat_no_store_at() {
        // inc2a back-compat: --store-from alone still means "mount at /gnu/store"
        // (store_at None — the handler defaults the dest), and the daemon/cwd flags
        // parse as before. Asserting store_at==None keeps the default wired here.
        let p = hs(&["--expose-cwd", "--store-from", "/seed", "--", "make", "check"])
            .expect("valid");
        assert_eq!(p.store_from.as_deref(), Some("/seed"));
        assert_eq!(p.store_at, None, "no --store-at -> handler binds at /gnu/store");
        assert!(p.expose_cwd);
        assert!(!p.no_daemon);
        assert_eq!(p.cmd, "make");
        assert_eq!(p.cmd_args, vec!["check"]);
    }

    #[test]
    fn host_sandbox_store_at_requires_store_from() {
        // --store-at is meaningless without something to mount: reject it loudly
        // rather than silently bind the host /gnu/store at the wrong place.
        let e = hs(&["--store-at", "/td/store", "--", "true"]).unwrap_err();
        assert!(e.contains("--store-at requires --store-from"), "got: {e}");
    }

    #[test]
    fn host_sandbox_flag_errors() {
        assert!(hs(&["--store-from", "--", "true"]).unwrap_err().contains("--store-from needs a DIR"));
        assert!(hs(&["--store-at", "--", "true"]).unwrap_err().contains("--store-at needs a DIR"));
        assert!(hs(&["--bogus", "--", "true"]).unwrap_err().contains("unknown flag"));
        // a `--` with no command after it is a usage error (no vacuous empty cmd).
        assert!(hs(&["--expose-cwd", "--"]).unwrap_err().contains("usage:"));
    }

    // build_profile --store-native: enumerate the PHYSICAL package dir but point the
    // symlinks at the LOGICAL store path, so the profile resolves in a store-ns own-root.
    #[test]
    fn profile_store_native_links_logical_paths() {
        let dir = std::env::temp_dir().join(format!("prof-sn-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let pkg = dir.join("aaaa-hello");
        std::fs::create_dir_all(pkg.join("bin")).unwrap();
        std::fs::write(pkg.join("bin").join("hello"), b"#!/x\n").unwrap();
        let prof = dir.join("profile");

        // store-native: link target is <prefix>/<basename(pkg)>/bin/hello, NOT the physical pkg.
        let n = build_profile(
            prof.to_str().unwrap(),
            std::slice::from_ref(&pkg.to_string_lossy().into_owned()),
            Some("/td/store"),
        )
        .unwrap();
        assert_eq!(n, 1);
        let link = std::fs::read_link(prof.join("bin").join("hello")).unwrap();
        assert_eq!(link, Path::new("/td/store/aaaa-hello/bin/hello"));

        // default (None): link straight at the physical PKG-OUT entry.
        let n2 = build_profile(
            prof.to_str().unwrap(),
            std::slice::from_ref(&pkg.to_string_lossy().into_owned()),
            None,
        )
        .unwrap();
        assert_eq!(n2, 1);
        let link2 = std::fs::read_link(prof.join("bin").join("hello")).unwrap();
        assert_eq!(link2, pkg.join("bin").join("hello"));

        std::fs::remove_dir_all(&dir).ok();
    }

    // build_profile still refuses a name collision across packages (store-native or not).
    #[test]
    fn profile_rejects_collision() {
        let dir = std::env::temp_dir().join(format!("prof-col-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for p in ["aaaa-a", "bbbb-b"] {
            std::fs::create_dir_all(dir.join(p).join("bin")).unwrap();
            std::fs::write(dir.join(p).join("bin").join("dup"), b"x").unwrap();
        }
        let pkgs = vec![
            dir.join("aaaa-a").to_string_lossy().into_owned(),
            dir.join("bbbb-b").to_string_lossy().into_owned(),
        ];
        let err = build_profile(dir.join("profile").to_str().unwrap(), &pkgs, Some("/td/store"))
            .unwrap_err();
        assert!(err.contains("collision"), "unexpected: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // --auto: a lock entry names a dep whether it's written bare (declared inputs like
    // grep's `pcre2`) or hash-named (`<hash>-D-<version>`); a non-matching toolchain
    // entry and a near-miss (`ncursesw` vs `ncurses`) do NOT match.
    #[test]
    fn auto_entry_is_dep_matches_bare_and_hash_forms() {
        let h = "agdqkcaybihqgjiwq9s9kz5mqsxwdjdv"; // 32-char base32 hash
        assert!(auto_entry_is_dep("pcre2", "/gnu/store/x-pcre2-10.42", "pcre2")); // bare
        assert!(auto_entry_is_dep(
            &format!("{h}-ncurses-6.2"),
            &format!("/gnu/store/{h}-ncurses-6.2"),
            "ncurses"
        )); // hash-named
        assert!(auto_entry_is_dep(
            &format!("{h}-gettext-minimal-0.23.1"),
            &format!("/gnu/store/{h}-gettext-minimal-0.23.1"),
            "gettext-minimal"
        )); // dep name contains a dash
        assert!(!auto_entry_is_dep(
            &format!("{h}-ncursesw-6.2"),
            &format!("/gnu/store/{h}-ncursesw-6.2"),
            "ncurses"
        )); // near-miss must NOT match
        assert!(!auto_entry_is_dep(
            &format!("{h}-coreutils-9.1"),
            &format!("/gnu/store/{h}-coreutils-9.1"),
            "ncurses"
        )); // toolchain entry
    }

    // --auto: a `source`-class line is the recipe's OWN source, never a rung dep, so it must
    // NOT be re-keyed even when its path basename prefix-matches an owned dep. Regression for
    // the x86_64 cross rungs: binutils-x86-64 builds the `binutils-244-source` tarball AND
    // depends on the `binutils-244` rung — the source path `<hash>-binutils-244-source` else
    // mis-matches `binutils-244` and the recipe loses its source.
    #[test]
    fn auto_chained_lock_never_rekeys_the_source_line() {
        let h = "agdqkcaybihqgjiwq9s9kz5mqsxwdjdv";
        let base = format!(
            "binutils-x86-64-source /td/store/{h}-binutils-244-source source\n\
             binutils-244 /td/store/pending-binutils-244\n"
        );
        let out = auto_chained_lock(&base, &["binutils-244".to_string()]).unwrap();
        // the source line passes through unchanged (still `source`-class), and the rung dep
        // is the line that gets re-keyed to td-recipe-output.
        assert!(
            out.contains(&format!(
                "binutils-x86-64-source /td/store/{h}-binutils-244-source source"
            )),
            "source line was re-keyed: {out}"
        );
        assert!(
            out.contains("binutils-244 /td/store/pending-binutils-244 td-recipe-output"),
            "rung dep not re-keyed: {out}"
        );
    }

    // --auto: topo-sort follows the recipe JSONs' `inputs`, ordering deps before
    // dependents, recursing only through OWNED inputs (those with a recipe JSON + lock);
    // a non-owned input (toolchain seed) is not a node.
    #[test]
    fn auto_topo_orders_deps_before_dependents() {
        let d = std::env::temp_dir().join(format!("td-auto-topo-{}", std::process::id()));
        let rj = d.join("rj");
        let ld = d.join("ld");
        std::fs::create_dir_all(&rj).unwrap();
        std::fs::create_dir_all(&ld).unwrap();
        let put = |name: &str, json: &str| {
            std::fs::write(rj.join(format!("{name}.json")), json).unwrap();
            std::fs::write(ld.join(format!("{name}-no-guix.lock")), "x\n").unwrap();
        };
        put("bash", r#"{"name":"bash","inputs":["readline","ncurses","gcc-toolchain"]}"#);
        put("readline", r#"{"name":"readline","inputs":["ncurses"]}"#);
        put("ncurses", r#"{"name":"ncurses"}"#);
        // gcc-toolchain has no recipe JSON / lock → not owned → not a node.
        let (rjs, lds) = (rj.to_string_lossy().to_string(), ld.to_string_lossy().to_string());
        let mut order = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut stack = Vec::new();
        auto_topo(&rjs, &lds, "bash", &mut order, &mut seen, &mut stack).unwrap();
        assert_eq!(order, vec!["ncurses", "readline", "bash"]);
        std::fs::remove_dir_all(&d).ok();
    }

    // --auto: deriving the chained lock re-keys each owned dep to bare-name +
    // td-recipe-output (so build_plan substitutes by recipe name), passes every other
    // line through unchanged, and errors if a declared owned dep isn't in the lock.
    #[test]
    fn auto_chained_lock_marks_owned_deps_only() {
        let h = "agdqkcaybihqgjiwq9s9kz5mqsxwdjdv"; // 32-char base32 store hash
        let base = format!(
            "{h}-coreutils-9.1 /gnu/store/{h}-coreutils-9.1\n\
             {h}-ncurses-6.2 /gnu/store/{h}-ncurses-6.2\n\
             pcre2 /gnu/store/{h}-pcre2-10.42\n\
             bash-source /gnu/store/{h}-bash-5.2.tar.gz\n"
        );
        let got = auto_chained_lock(&base, &["ncurses".into(), "pcre2".into()]).unwrap();
        assert!(got.contains(&format!("ncurses /gnu/store/{h}-ncurses-6.2 td-recipe-output")));
        assert!(got.contains(&format!("pcre2 /gnu/store/{h}-pcre2-10.42 td-recipe-output")));
        assert!(got.contains(&format!("{h}-coreutils-9.1 /gnu/store/{h}-coreutils-9.1\n"))); // seed untouched
        assert!(got.contains(&format!("bash-source /gnu/store/{h}-bash-5.2.tar.gz\n"))); // source untouched
        // a declared owned dep absent from the lock is an error (don't drop the edge).
        assert!(auto_chained_lock(&base, &["readline".into()]).is_err());
    }

    // subst-export writes, for each member, a narinfo with the right StorePath/References
    // and a nar that RESTORES (read_nar) to the original tree with the recorded NarHash —
    // the durable round-trip of the substitute server's store-coupled half, no DB/network.
    #[test]
    fn subst_export_writes_narinfos_and_restorable_nars() {
        let base = std::env::temp_dir().join(format!("td-subst-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Two synthetic store paths; "app" references "lib".
        let lib = "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib";
        let app = "/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app";
        let phys_lib = base.join("phys/lib");
        std::fs::create_dir_all(&phys_lib).unwrap();
        std::fs::write(phys_lib.join("libfoo"), b"lib bytes\n").unwrap();
        let phys_app = base.join("phys/app");
        std::fs::create_dir_all(&phys_app).unwrap();
        std::fs::write(phys_app.join("run"), b"app\n").unwrap();

        let members = vec![
            SubstMember { store_path: lib.into(), physical: phys_lib.clone(), refs: vec![] },
            SubstMember { store_path: app.into(), physical: phys_app.clone(), refs: vec![lib.into()] },
        ];
        let outdir = base.join("out");
        let written = subst_export(&outdir, &members).unwrap();
        assert_eq!(written.len(), 2);

        // The app narinfo carries the right StorePath and records the ref as a BASENAME.
        let ni = std::fs::read_to_string(outdir.join("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app.narinfo")).unwrap();
        assert!(ni.contains(&format!("StorePath: {app}\n")), "narinfo: {ni}");
        assert!(ni.contains("References: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib\n"), "narinfo: {ni}");
        let narhash = ni.lines().find_map(|l| l.strip_prefix("NarHash: ")).unwrap();
        let narfile = ni.lines().find_map(|l| l.strip_prefix("NarFile: ")).unwrap();
        // The recorded NarHash is the TRUE nar hash of the source path.
        assert_eq!(narhash, nar_hash_size_path(&phys_app).unwrap().0);
        // The served nar RESTORES to the original tree (durable round-trip).
        let restored = base.join("restored-app");
        let mut r = std::io::BufReader::new(std::fs::File::open(outdir.join(narfile)).unwrap());
        nar::read_nar(&mut r, &restored).unwrap();
        assert_eq!(std::fs::read(restored.join("run")).unwrap(), b"app\n");

        std::fs::remove_dir_all(&base).unwrap();
    }

    // harness_subst_export (#314): the WHOLE harness tree — a store/ with multiple entries
    // AND loose files (the /td/store/ld loader), plus the rel + toolchain metadata — ships as
    // ONE fixed-name nar and restores byte-for-byte. This is the tree-set variant the toolchain
    // per-path export can't express (ld is not a `<hash>-name` store path).
    #[test]
    fn harness_subst_export_ships_the_whole_tree_under_a_fixed_name() {
        let base = std::env::temp_dir().join(format!("td-harness-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let hdir = base.join("harness");
        // A harness-shaped fixture: store/<rel>/bin/busybox (exec), a loose store/ld loader,
        // plus the rel + toolchain manifest the check-harness loop reads.
        let rel = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-userland-x86_64-store-native";
        let bind = hdir.join("store").join(rel).join("bin");
        std::fs::create_dir_all(&bind).unwrap();
        std::fs::write(bind.join("busybox"), b"#!/bin/sh\necho hi\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bind.join("busybox"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(hdir.join("store").join("ld"), b"loader bytes\n").unwrap();
        std::fs::write(hdir.join("rel"), format!("{rel}\n")).unwrap();
        std::fs::write(hdir.join("toolchain"), b"HT_TARGET=x86_64-pc-linux-gnu\nHT_GCC=g\n").unwrap();

        let outdir = base.join("out");
        let written = harness_subst_export(&outdir, &hdir).unwrap();
        assert_eq!(written, vec!["td-harness".to_string()]);
        let ni = std::fs::read_to_string(outdir.join("td-harness.narinfo")).unwrap();
        assert!(ni.contains("StorePath: /td/store/td-harness\n"), "narinfo: {ni}");
        assert!(ni.contains("References: \n"), "harness has no refs: {ni}");
        let narfile = ni.lines().find_map(|l| l.strip_prefix("NarFile: ")).unwrap();

        // The served nar RESTORES the WHOLE tree — the store subdir (its entry + the loose ld)
        // and both metadata files — byte-for-byte, exec bit preserved on the binary.
        let restored = base.join("restored");
        let mut r = std::io::BufReader::new(std::fs::File::open(outdir.join(narfile)).unwrap());
        nar::read_nar(&mut r, &restored).unwrap();
        assert_eq!(std::fs::read(restored.join("store").join(rel).join("bin/busybox")).unwrap(),
                   b"#!/bin/sh\necho hi\n");
        assert_eq!(std::fs::read(restored.join("store").join("ld")).unwrap(), b"loader bytes\n");
        assert_eq!(std::fs::read_to_string(restored.join("rel")).unwrap(), format!("{rel}\n"));
        assert!(std::fs::read_to_string(restored.join("toolchain")).unwrap().contains("HT_GCC=g"));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(restored.join("store").join(rel).join("bin/busybox"))
                .unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "exec bit not preserved through the harness nar");
        }

        // A non-harness dir (no store/ or rel) is rejected — the producer never ships junk.
        assert!(harness_subst_export(&outdir, &base).is_err());

        std::fs::remove_dir_all(&base).unwrap();
    }

    // subst-export `--paths` exports EXACTLY the named roots (no closure walk) — the
    // per-output granularity the substitute consumer fetches — while the default closure
    // mode pulls in the external refs. A build output's refs (glibc, …) are recorded in the
    // build db but NOT staged in its newstore, so a per-output publish must skip them.
    #[test]
    fn subst_export_members_paths_only_exports_roots_not_their_closure() {
        let dir = std::env::temp_dir().join(format!("td-subst-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let app = "/gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-app".to_string();
        let lib = "/gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-lib".to_string();
        // app references lib (an external dep: in the build db with a null hash, not staged).
        let regs = vec![OutputReg {
            store_path: app.clone(),
            nar_hash: "sha256:00".into(),
            nar_size: 1,
            refs: vec![lib.clone()],
            deriver: String::new(),
        }];
        let db_path = dir.join("td.db");
        write_output_db(&regs, &db_path).unwrap();
        let db = store_db_read::Db::open(std::fs::read(&db_path).unwrap()).unwrap();
        let roots = vec![app.clone()];

        // Default (closure) mode pulls in the external ref — a whole-closure mirror.
        let full = subst_export_members(&db, "/store", &roots, true).unwrap();
        let fp: std::collections::BTreeSet<&str> =
            full.iter().map(|m| m.store_path.as_str()).collect();
        assert!(
            fp.contains(app.as_str()) && fp.contains(lib.as_str()),
            "closure mode must include the external ref: {fp:?}"
        );

        // Paths-only exports EXACTLY the root, but still lists its refs in the narinfo so the
        // consumer can scan-verify the restored bytes (deps assumed already present).
        let only = subst_export_members(&db, "/store", &roots, false).unwrap();
        assert_eq!(only.len(), 1, "paths-only must not pull in the closure");
        assert_eq!(only[0].store_path, app);
        assert_eq!(only[0].refs, vec![lib.clone()]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // restore_substitute: a fetched narinfo + nar (as subst-export produces) restores to
    // the original tree, the OutputReg's NarHash equals the signed one, and a corrupted nar
    // is REJECTED (the durable equality leg — a substitute is only accepted if it restores
    // to the bytes the publisher signed). The consumer's core, no network/DB.
    #[test]
    fn restore_substitute_round_trips_and_rejects_corruption() {
        let base = std::env::temp_dir().join(format!("td-subst-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store_path = "/td/store/cccccccccccccccccccccccccccccccc-app";
        let app_base = "cccccccccccccccccccccccccccccccc-app";
        let phys = base.join("phys/app");
        std::fs::create_dir_all(&phys).unwrap();
        std::fs::write(phys.join("run"), b"app payload\n").unwrap();

        // Export it (the server side) → a narinfo + nar, exactly what `fetch` would write.
        let served = base.join("served");
        subst_export(&served, &[SubstMember { store_path: store_path.into(), physical: phys.clone(), refs: vec![] }]).unwrap();
        let ni = std::fs::read_to_string(served.join(format!("{app_base}.narinfo"))).unwrap();
        let narfile = served.join(narinfo_field(&ni, "NarFile").unwrap());

        // Restore it (the consumer side) into a fresh newstore.
        let newstore = base.join("newstore");
        let reg = restore_substitute(&ni, &narfile, store_path, &newstore, "x.drv").unwrap();
        assert_eq!(reg.store_path, store_path);
        assert_eq!(reg.nar_hash, narinfo_field(&ni, "NarHash").unwrap());
        assert_eq!(std::fs::read(newstore.join(app_base).join("run")).unwrap(), b"app payload\n");

        // Self-discrimination (wrong output): the narinfo is a perfectly valid,
        // hash-consistent export of `store_path`, but we ask restore to treat it as a
        // DIFFERENT output. A signed narinfo for one path must not be accepted as
        // another (the StorePath-binding check) even though every byte verifies.
        let other_path = "/td/store/dddddddddddddddddddddddddddddddd-other";
        assert!(
            restore_substitute(&ni, &narfile, other_path, &newstore, "x.drv").is_err(),
            "restore accepted a narinfo whose signed StorePath != the requested output"
        );

        // Self-discrimination: corrupt the nar's file CONTENTS (structure intact, so
        // read_nar still parses) → restore must reject on the NarHash check specifically.
        let mut bytes = std::fs::read(&narfile).unwrap();
        let pos = bytes.windows(3).position(|w| w == b"app").expect("payload in nar");
        bytes[pos] ^= 0xff;
        std::fs::write(&narfile, &bytes).unwrap();
        assert!(
            restore_substitute(&ni, &narfile, store_path, &newstore, "x.drv").is_err(),
            "restore accepted a nar whose contents do not match the signed NarHash"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    // A substitute whose NAR is structurally TRUNCATED (read_nar creates the dest dir + starts
    // the file, then hits EOF mid-contents) must be rejected AND leave NO partial tree under
    // newstore. This is the cleanup-on-failure leg: a half-restored output left behind would let
    // the build fallback write its fresh outputs on top of it (contaminating a multi-output drv).
    // The old code only cleaned on a NarHash mismatch, not on a parse/write error.
    #[test]
    fn restore_substitute_cleans_partial_tree_on_parse_error() {
        let base = std::env::temp_dir().join(format!("td-subst-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store_path = "/td/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-app";
        let app_base = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-app";
        let phys = base.join("phys/app");
        std::fs::create_dir_all(&phys).unwrap();
        std::fs::write(phys.join("run"), b"app payload\n").unwrap();

        let served = base.join("served");
        subst_export(&served, &[SubstMember { store_path: store_path.into(), physical: phys.clone(), refs: vec![] }]).unwrap();
        let ni = std::fs::read_to_string(served.join(format!("{app_base}.narinfo"))).unwrap();
        let narfile = served.join(narinfo_field(&ni, "NarFile").unwrap());

        // Truncate inside the file contents: read_nar creates dest + the `run` file, then EOFs
        // part way through copy_n — a partial tree exists at the moment the error is returned.
        let bytes = std::fs::read(&narfile).unwrap();
        let pos = bytes.windows(3).position(|w| w == b"app").expect("payload in nar");
        let truncated = base.join("truncated.nar");
        std::fs::write(&truncated, &bytes[..pos + 4]).unwrap();

        let newstore = base.join("newstore");
        assert!(
            restore_substitute(&ni, &truncated, store_path, &newstore, "x.drv").is_err(),
            "a truncated NAR must be rejected"
        );
        assert!(
            !newstore.join(app_base).exists(),
            "a rejected (parse-error) substitute must leave no partial tree under newstore"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    // The build cache hits only on a present + NAR-verified output, and misses on a
    // corrupted, deleted, or never-recorded one — so a CHANGED recipe (different drv ⇒
    // different output path, never recorded) always rebuilds, and a corrupted cache
    // entry rebuilds rather than serving garbage.
    #[test]
    fn cached_realization_hits_only_on_a_present_and_nar_verified_output() {
        let base = std::env::temp_dir().join(format!("td-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scratch = base.join("b");
        let store_path = "/gnu/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-thing-1.0";
        let outdir = scratch.join("newstore").join("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-thing-1.0");
        std::fs::create_dir_all(&outdir).unwrap();
        std::fs::write(outdir.join("data"), b"hello cache").unwrap();
        // Real NAR hash of the output (same scan/nar the registration is written with).
        let mut sc = scan::Scanner::new(&[]).unwrap();
        nar::write_nar(&mut sc, &outdir).unwrap();
        let (hash, size, _) = sc.finish();
        let drv = drv::Derivation {
            outputs: vec![drv::Output {
                name: "out".into(),
                path: store_path.into(),
                hash_algo: String::new(),
                hash: String::new(),
            }],
            input_drvs: vec![],
            input_srcs: vec![],
            platform: String::new(),
            builder: String::new(),
            args: vec![],
            env: vec![],
        };
        let write_reg = |h: &str| {
            std::fs::write(
                scratch.join("registration"),
                format!("path {store_path}\nnar-hash {h}\nnar-size {size}\nderiver x.drv\n\n"),
            )
            .unwrap();
        };

        // (a) present + matching hash recorded -> HIT.
        write_reg(&hash);
        assert!(cached_realization(&drv, &scratch).unwrap().is_some(), "valid entry must hit");

        // (b) recorded hash wrong (output content changed under us) -> MISS.
        write_reg("sha256:deadbeef");
        assert!(cached_realization(&drv, &scratch).unwrap().is_none(), "hash mismatch must miss");

        // (c) output directory gone -> MISS.
        write_reg(&hash);
        std::fs::remove_dir_all(&outdir).unwrap();
        assert!(cached_realization(&drv, &scratch).unwrap().is_none(), "absent output must miss");

        // (d) never built here (no registration) -> MISS.
        std::fs::remove_file(scratch.join("registration")).unwrap();
        assert!(cached_realization(&drv, &scratch).unwrap().is_none(), "no registration must miss");

        let _ = std::fs::remove_dir_all(&base);
    }

    // `copy_canonical` must reproduce a tree byte-identically by NAR — exercising
    // the properties NAR captures that the `store-add-tree` rung's source tree does
    // not have: an EXECUTABLE file and a SYMLINK (plus a subdir + a plain file).
    #[test]
    fn copy_canonical_is_nar_identical_with_exec_and_symlink() {
        let base = std::env::temp_dir().join(format!("td-cc-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/run.sh"), b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(src.join("sub/run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        // A GROUP-exec-only file (0o654): NAR keys off OWNER-exec, so this must be
        // restored NON-executable — a regression guard for the `& 0o100` (not `0o111`)
        // exec test, matching nar.rs / the daemon.
        std::fs::write(src.join("group-exec"), b"data").unwrap();
        std::fs::set_permissions(src.join("group-exec"), std::fs::Permissions::from_mode(0o654))
            .unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();

        copy_canonical(&src, &dst).unwrap();

        // Structure + contents + exec bit + symlink target all preserved ⇒ same NAR.
        assert_eq!(
            nar_hash_path(&src).unwrap(),
            nar_hash_path(&dst).unwrap(),
            "canonical copy is NAR-identical to the source"
        );
        // The executable bit (the one perm NAR distinguishes) is preserved.
        let mode = std::fs::metadata(dst.join("sub/run.sh")).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111, "exec bit preserved on dst");
        // The symlink is recreated as a symlink, not followed.
        assert!(std::fs::symlink_metadata(dst.join("link")).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn gcc_toolchain_substitution_swaps_only_the_toolchain_input() {
        // corpus-toolchain-default: TD_GCC_TOOLCHAIN swaps the guix gcc-toolchain input for a
        // /td/store toolchain, leaving every other build input (glibc, make, coreutils, the source)
        // untouched, order-preserved.
        let tc = "/td/store/abc123-gcc-toolchain-tdstore";
        let mut inputs = vec![
            "/gnu/store/aaa-glibc-2.41".to_string(),
            "/gnu/store/bbb-gcc-toolchain-15.2.0".to_string(),
            "/gnu/store/ccc-make-4.4.1".to_string(),
        ];
        assert!(super::substitute_gcc_toolchain(&mut inputs, tc), "should report a swap");
        assert_eq!(
            inputs,
            vec![
                "/gnu/store/aaa-glibc-2.41".to_string(),
                tc.to_string(),
                "/gnu/store/ccc-make-4.4.1".to_string(),
            ],
            "only the gcc-toolchain input is swapped; others + order preserved"
        );
        // Near-miss basenames must NOT be swapped: a bare gcc (the package name is `gcc-…`, not
        // `gcc-toolchain-…`), and an unrelated package that merely embeds `-gcc-toolchain-` INTERIOR
        // (the name is `libfoo-…`, so the anchored match at the package name excludes it).
        let mut other = vec![
            "/gnu/store/ddd-gcc-14.3.0".to_string(),
            "/gnu/store/eee-libfoo-gcc-toolchain-helper".to_string(),
        ];
        assert!(
            !super::substitute_gcc_toolchain(&mut other, tc),
            "bare gcc + interior-substring packages are not toolchain inputs"
        );
        assert_eq!(
            other,
            vec![
                "/gnu/store/ddd-gcc-14.3.0".to_string(),
                "/gnu/store/eee-libfoo-gcc-toolchain-helper".to_string(),
            ],
            "unchanged on no-op"
        );
    }

    // Exercise the override through the REAL engine path: assemble_recipe_drv reads TD_GCC_TOOLCHAIN and
    // substitutes it for the lock's guix gcc-toolchain when it assembles the .drv — the code the corpus
    // build path runs (not just the helper). Asserts the produced drv's TD_INPUTS + input-srcs reflect the
    // swap, and that the default (env unset) is unchanged. This is the reusable-mechanism analog of the
    // per-gate lock-rewrite: a build-recipe with TD_GCC_TOOLCHAIN set compiles with td's /td/store toolchain.
    #[test]
    fn assemble_recipe_drv_honors_td_gcc_toolchain() {
        let dir = std::env::temp_dir().join(format!("td-gcctc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("hello.lock");
        // A minimal recipe lock: source + guix gcc-toolchain + glibc + make (2-field seed inputs).
        std::fs::write(
            &lock,
            "hello-source /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello-2.12.2.tar.gz source\n\
             /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-toolchain-15.2.0 /gnu/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-toolchain-15.2.0\n\
             /gnu/store/cccccccccccccccccccccccccccccccc-glibc-2.41 /gnu/store/cccccccccccccccccccccccccccccccc-glibc-2.41\n\
             /gnu/store/dddddddddddddddddddddddddddddddd-make-4.4.1 /gnu/store/dddddddddddddddddddddddddddddddd-make-4.4.1\n",
        )
        .unwrap();
        let recipe = r#"{"name":"hello","version":"2.12.2","buildSystem":"gnu"}"#;
        let lockp = lock.to_str().unwrap();
        let builder = "/gnu/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-td-builder-0.1.0";
        let tc = "/td/store/ffffffffffffffffffffffffffffffff-gcc-toolchain-tdstore";
        let td_inputs = |drv: &drv::Derivation| {
            drv.env.iter().find(|(k, _)| k == "TD_INPUTS").map(|(_, v)| v.clone()).unwrap()
        };

        // WITH the override: the guix gcc-toolchain is swapped for the /td/store toolchain.
        std::env::set_var("TD_GCC_TOOLCHAIN", tc);
        let (_p, _f, drv, _s) = assemble_recipe_drv(recipe, lockp, &dir, builder, None).unwrap();
        std::env::remove_var("TD_GCC_TOOLCHAIN");
        let ti = td_inputs(&drv);
        assert!(ti.contains(tc), "TD_INPUTS carries the /td/store toolchain: {ti}");
        assert!(!ti.contains("gcc-toolchain-15.2.0"), "guix gcc-toolchain swapped OUT of TD_INPUTS: {ti}");
        assert!(ti.contains("-glibc-2.41") && ti.contains("-make-4.4.1"), "other inputs untouched: {ti}");
        // The swapped path is an input-src too (staged into the build), not just an env value.
        assert!(drv.input_srcs.iter().any(|s| s == tc), "override is an input-src");
        assert!(
            !drv.input_srcs.iter().any(|s| s.contains("gcc-toolchain-15.2.0")),
            "guix gcc-toolchain is not an input-src (dropped from the drv closure)"
        );

        // WITHOUT the override (default): unchanged — the guix gcc-toolchain stays.
        let (_p, _f, drv0, _s) = assemble_recipe_drv(recipe, lockp, &dir, builder, None).unwrap();
        let ti0 = td_inputs(&drv0);
        assert!(ti0.contains("gcc-toolchain-15.2.0"), "default keeps the guix gcc-toolchain: {ti0}");
        assert!(!ti0.contains(tc), "default has no /td/store toolchain: {ti0}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // #258 ripgrep cutover: the native /td/store gcc is a PLAIN gcc (no ld-wrapper), so run_rust must
    // bake the interp/RUNPATH/-B explicitly — but the build sandbox CLEARS the env, so those must ride
    // in the drv's `env` lines. assemble_recipe_drv forwards the caller's TD_RUST_STORE_* into a rust
    // drv (and ONLY a rust drv; only when set), so run_rust receives them. Exercise the real engine
    // path and assert the drv env carries them, and that the default (unset) emits none.
    #[test]
    fn assemble_recipe_drv_forwards_td_rust_store_env() {
        let dir = std::env::temp_dir().join(format!("td-ruststore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("ripgrep.lock");
        std::fs::write(
            &lock,
            "ripgrep-source /gnu/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ripgrep-14.1.1.tar.gz source\n\
             /td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-14.3.0-x86_64-native /td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-gcc-14.3.0-x86_64-native\n\
             /td/store/cccccccccccccccccccccccccccccccc-rust-1.96.0-x86_64-store-native /td/store/cccccccccccccccccccccccccccccccc-rust-1.96.0-x86_64-store-native\n",
        )
        .unwrap();
        let recipe = r#"{"name":"ripgrep","version":"14.1.1","buildSystem":"rust","bins":["rg"]}"#;
        let lockp = lock.to_str().unwrap();
        let builder = "/gnu/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-td-builder-0.1.0";
        let env_of = |drv: &drv::Derivation, k: &str| {
            drv.env.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone())
        };
        let interp = "/td/store/cccccccccccccccccccccccccccccccc-glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2";
        let rpath = "/td/store/cccccccccccccccccccccccccccccccc-glibc-2.41-x86_64/lib";
        let bdir = rpath;

        // WITH the vars set: the rust drv carries them so run_rust can bake interp/RUNPATH/-B.
        std::env::set_var("TD_RUST_STORE_INTERP", interp);
        std::env::set_var("TD_RUST_STORE_RPATH", rpath);
        std::env::set_var("TD_RUST_STORE_BDIR", bdir);
        let (_p, _f, drv, _s) = assemble_recipe_drv(recipe, lockp, &dir, builder, None).unwrap();
        std::env::remove_var("TD_RUST_STORE_INTERP");
        std::env::remove_var("TD_RUST_STORE_RPATH");
        std::env::remove_var("TD_RUST_STORE_BDIR");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_INTERP").as_deref(), Some(interp), "interp forwarded to the drv env");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_RPATH").as_deref(), Some(rpath), "rpath forwarded");
        assert_eq!(env_of(&drv, "TD_RUST_STORE_BDIR").as_deref(), Some(bdir), "bdir forwarded");

        // WITHOUT the vars (default): none emitted ⇒ the guix ld-wrapper path, unchanged.
        let (_p, _f, drv0, _s) = assemble_recipe_drv(recipe, lockp, &dir, builder, None).unwrap();
        assert!(env_of(&drv0, "TD_RUST_STORE_INTERP").is_none(), "no interp in the drv env by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_RPATH").is_none(), "no rpath by default");
        assert!(env_of(&drv0, "TD_RUST_STORE_BDIR").is_none(), "no bdir by default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- content-scan closure (retire /var/guix/db): scan_candidate_index + scan_closure_hybrid ----
    // The daemon-DB-free input-closure computation realize_drv now uses. A store DIR is
    // content-scanned for the seed roots (candidate index keyed by CANONICAL path, bytes
    // NAR-read from on-disk), UNIONed with any td-OWNED store DB's direct refs (build-plan's
    // td.dbs — a td-built dep whose bytes live OUTSIDE the scanned dir). Covers: the canonical
    // vs on-disk mapping, the `.lock` aux-file skip, a content-scanned transitive closure, and
    // the hybrid extra-refs edge (a root with no on-disk bytes resolved via the extra-db map).
    #[test]
    fn content_scan_closure_spans_seed_dir_and_extra_dbs() {
        use std::collections::HashMap;
        // 32-char nix-base32 hash parts (alphabet omits e,o,u,t).
        let glibc_h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let gcc_h = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let tddep_h = "cccccccccccccccccccccccccccccccc";
        let dir = std::env::temp_dir().join(format!("td-cscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let od = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let canon = |name: &str| format!("/gnu/store/{name}");
        // glibc: a leaf with no store references. gcc: references glibc (its 32-char hash is
        // literally in the bytes, the daemon's own reference criterion). A `.lock` aux file
        // shares glibc's hash and MUST be skipped by the candidate index.
        std::fs::write(od(&format!("{glibc_h}-glibc-2.41")), b"a leaf, no store references here\n").unwrap();
        std::fs::write(
            od(&format!("{gcc_h}-gcc-14")),
            format!("gcc links libc at /gnu/store/{glibc_h}-glibc-2.41/lib\n").as_bytes(),
        )
        .unwrap();
        std::fs::write(od(&format!("{glibc_h}-glibc-2.41.lock")), b"").unwrap();

        let dirs = [dir.to_string_lossy().into_owned()];
        let (candidates, on_disk) = scan_candidate_index(&dirs, "/gnu/store").unwrap();
        // Two candidates (the .lock aux file is skipped), keyed by CANONICAL path.
        assert_eq!(candidates.len(), 2, "candidates (lock aux file skipped): {candidates:?}");
        assert!(candidates.contains(&canon(&format!("{glibc_h}-glibc-2.41"))));
        assert!(candidates.contains(&canon(&format!("{gcc_h}-gcc-14"))));
        // Canonical path maps to the ON-DISK bytes (here dir == canonical prefix's stand-in).
        assert_eq!(on_disk[&canon(&format!("{glibc_h}-glibc-2.41"))], od(&format!("{glibc_h}-glibc-2.41")));

        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let empty: HashMap<String, Vec<String>> = HashMap::new();

        // Pure content-scan from the gcc root: BFS finds glibc via gcc's bytes.
        let cl = scan_closure_hybrid(&mut scanner, &on_disk, &empty, &[canon(&format!("{gcc_h}-gcc-14"))]).unwrap();
        let cl: Vec<String> = cl.into_iter().collect();
        assert_eq!(
            cl,
            vec![canon(&format!("{glibc_h}-glibc-2.41")), canon(&format!("{gcc_h}-gcc-14"))],
            "content-scan closure of gcc must be {{gcc, glibc}}"
        );

        // Hybrid: a td-built dep whose bytes live OUTSIDE the scanned dir. Its refs come from
        // the extra-db map (td.db), then that ref (gcc) is content-scanned into glibc.
        let mut extra: HashMap<String, Vec<String>> = HashMap::new();
        extra.insert(canon(&format!("{tddep_h}-mylib-1")), vec![canon(&format!("{gcc_h}-gcc-14"))]);
        let hy = scan_closure_hybrid(&mut scanner, &on_disk, &extra, &[canon(&format!("{tddep_h}-mylib-1"))]).unwrap();
        let hy: Vec<String> = hy.into_iter().collect();
        assert_eq!(
            hy,
            vec![
                canon(&format!("{glibc_h}-glibc-2.41")),
                canon(&format!("{gcc_h}-gcc-14")),
                canon(&format!("{tddep_h}-mylib-1")),
            ],
            "hybrid closure must span the td-dep (extra db) + its content-scanned seed refs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- multi-store store-closure-scan: candidate index spans several dirs -------------
    // The store-closure-scan primitive R3 uses to close a td-built subject whose OUTPUT tree
    // lives in a build scratch's `newstore` while its deps live in the seed store: the
    // candidate index spans BOTH dirs, the FIRST dir is the canonical prefix the roots use,
    // and — because matching is by 32-char HASH, not by prefix — a member whose bytes sit
    // under the non-canonical dir still resolves. This mirrors scan_candidate_index(&[seed,
    // newstore], seed) exactly as the `store-closure-scan seed,newstore ROOT` arm calls it.
    #[test]
    fn multi_store_scan_spans_seed_and_newstore() {
        use std::collections::HashMap;
        let glibc_h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hello_h = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let base = std::env::temp_dir().join(format!("td-multiscan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed"); // the canonical /gnu/store stand-in (deps live here)
        let newstore = base.join("newstore"); // a build scratch's newstore (the output only)
        std::fs::create_dir_all(&seed).unwrap();
        std::fs::create_dir_all(&newstore).unwrap();
        // glibc is a leaf in the seed; hello is the SUBJECT output, present ONLY in newstore,
        // and its bytes reference glibc by hash (the daemon's own reference criterion).
        std::fs::write(seed.join(format!("{glibc_h}-glibc-2.41")), b"a libc leaf\n").unwrap();
        std::fs::write(
            newstore.join(format!("{hello_h}-hello-2.12.2")),
            format!("hello links /gnu/store/{glibc_h}-glibc-2.41/lib/libc.so\n").as_bytes(),
        )
        .unwrap();

        let seed_s = seed.to_string_lossy().into_owned();
        let newstore_s = newstore.to_string_lossy().into_owned();
        let canon = |name: &str| format!("/gnu/store/{name}");
        // The FIRST dir is the canonical prefix; both dirs are byte sources.
        let (candidates, on_disk) =
            scan_candidate_index(&[seed_s.clone(), newstore_s.clone()], "/gnu/store").unwrap();
        // hello's canonical path uses the /gnu/store prefix, but its BYTES come from newstore.
        let hello_c = canon(&format!("{hello_h}-hello-2.12.2"));
        let glibc_c = canon(&format!("{glibc_h}-glibc-2.41"));
        assert!(candidates.contains(&hello_c) && candidates.contains(&glibc_c));
        assert_eq!(on_disk[&hello_c], newstore.join(format!("{hello_h}-hello-2.12.2")).to_string_lossy());
        assert_eq!(on_disk[&glibc_c], seed.join(format!("{glibc_h}-glibc-2.41")).to_string_lossy());

        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        // Closing from the subject root pulls glibc out of the OTHER store dir, by hash.
        let mut cl: Vec<String> =
            scan_closure_hybrid(&mut scanner, &on_disk, &empty, &[hello_c.clone()]).unwrap().into_iter().collect();
        cl.sort();
        assert_eq!(cl, vec![glibc_c, hello_c], "multi-store closure must span both stores");
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- #292: roots whose canonical prefix differs from the candidate index's ----------
    // Gate 377 (store-persist) builds at TD_STORE_DIR=/td/store from a lock whose seed roots
    // are /gnu/store paths. The walk only content-scans a path whose CANONICAL form is an
    // index key — so a /gnu/store root against a single-prefix-canonicalized index collapsed
    // to "roots only" and dropped every transitive runtime dep (coreutils → gmp: expr died
    // on libgmp.so.10). VERIFIED-RED: composed the pre-fix way (prefix = the active
    // /td/store, no overrides), the gmp assertion below fails with closure == roots. This
    // composes index + overrides + walk exactly as realize_drv now does: /gnu/store as the
    // seed dirs' canonical home, per-hash TRUE canonicals restored from the drv roots and
    // the td-owned extra DBs (recanonicalize_candidates).
    #[test]
    fn cross_prefix_roots_keep_transitive_deps() {
        use std::collections::HashMap;
        let cu_h = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // coreutils (a /gnu/store lock root)
        let gmp_h = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"; // gmp (transitive: only in coreutils' bytes)
        let tc_h = "cccccccccccccccccccccccccccccccc"; // td-built toolchain (a /td/store root)
        let gl_h = "dddddddddddddddddddddddddddddddd"; // td-built glibc (a /td/store root, td-db-registered)
        let dir = std::env::temp_dir().join(format!("td-xprefix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wr = |name: &str, bytes: String| std::fs::write(dir.join(name), bytes).unwrap();
        // The warm-seed staging dir mixes guix-captured entries and copied-in td-built ones.
        wr(&format!("{cu_h}-coreutils-9.1"), format!("expr RPATHs /gnu/store/{gmp_h}-gmp-6.3.0/lib\n"));
        wr(&format!("{gmp_h}-gmp-6.3.0"), "a guix-built leaf\n".to_string());
        wr(&format!("{tc_h}-gcc-toolchain-tdstore"), format!("wrapper: -Wl,--dynamic-linker /td/store/{gl_h}-glibc-2.41/lib/ld-linux.so.2\n"));
        wr(&format!("{gl_h}-glibc-2.41"), "a td-built leaf\n".to_string());
        let dirs = [dir.to_string_lossy().into_owned()];
        // The gate's roots: guix seed entries at /gnu/store + the td-built toolchain pair
        // at /td/store (the substituted lock lines).
        let cu_c = format!("/gnu/store/{cu_h}-coreutils-9.1");
        let tc_c = format!("/td/store/{tc_h}-gcc-toolchain-tdstore");
        let gl_c = format!("/td/store/{gl_h}-glibc-2.41");
        let roots = [cu_c.clone(), tc_c.clone(), gl_c.clone()];
        // realize_drv's composition: /gnu/store is the seed dirs' canonical home; the
        // roots + the td-owned DB registrations (glibc rides bgdb in the gate) override
        // per hash; everything else keeps the seed prefix.
        let mut overrides: HashMap<String, String> = HashMap::new();
        overrides.insert(gl_h.to_string(), gl_c.clone()); // TD_EXTRA_DBS registration
        for r in &roots {
            overrides.insert(store::hash_from_store_path(r).unwrap().to_string(), r.clone());
        }
        let (mut candidates, mut on_disk) = scan_candidate_index(&dirs, "/gnu/store").unwrap();
        recanonicalize_candidates(&mut candidates, &mut on_disk, &overrides);
        let mut scanner = scan::Scanner::new(&candidates).unwrap();
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        let cl = scan_closure_hybrid(&mut scanner, &on_disk, &empty, &roots).unwrap();
        let gmp_c = format!("/gnu/store/{gmp_h}-gmp-6.3.0");
        assert!(
            cl.contains(&gmp_c),
            "transitive runtime dep gmp dropped from the closure (#292): {cl:?}"
        );
        // The toolchain's byte-scanned glibc ref must resolve to the td-built glibc at its
        // TRUE /td/store canonical — a phantom /gnu/store twin would poison the output
        // reference scan's candidate set (duplicate hash, last-in wins).
        assert!(cl.contains(&gl_c), "td-built glibc missing: {cl:?}");
        assert!(
            !cl.contains(&format!("/gnu/store/{gl_h}-glibc-2.41")),
            "td-built glibc duplicated under /gnu/store: {cl:?}"
        );
        // All four members, each at exactly its true canonical.
        assert_eq!(cl.len(), 4, "closure must be exactly the 4 true-canonical members: {cl:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
