//! td-builder — td's own builder (DESIGN §7.1 side-track; plan/td-builder.md).
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

mod build;
mod build_daemon;
mod daemon;
mod drv;
mod json;
mod lock;
mod nar;
mod sandbox;
mod scan;
mod sha256;
mod store;
mod store_db;
mod store_db_read;
mod sys;

use std::path::Path;
use std::process::{Command, ExitCode};

/// Adapter: stream Write into the hasher.
struct HashWriter(sha256::Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn nar_hash_path(path: &Path) -> Result<String, std::io::Error> {
    let mut w = HashWriter(sha256::Sha256::new());
    nar::write_nar(&mut w, path)?;
    Ok(format!("sha256:{}", sha256::to_base16(&w.0.finalize())))
}

fn nar_hash(path: &str) -> Result<String, std::io::Error> {
    nar_hash_path(Path::new(path))
}

/// Adapter: hash AND count the NAR bytes in one serialization pass (the seed
/// manifest needs both the NAR hash and the NAR size — the daemon's `narSize`).
struct HashSizeWriter {
    hasher: sha256::Sha256,
    size: u64,
}

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
fn nar_hash_size_path(path: &Path) -> Result<(String, u64), std::io::Error> {
    let mut w = HashSizeWriter { hasher: sha256::Sha256::new(), size: 0 };
    nar::write_nar(&mut w, path)?;
    Ok((format!("sha256:{}", sha256::to_base16(&w.hasher.finalize())), w.size))
}

/// The `path` column (index 1) of a read `ValidPaths` row, or "" if absent.
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

/// Execute DRV in a userns sandbox against CLOSURE (the staged input store paths,
/// one per line) and write a registration record — `path` / `nar-hash` /
/// `nar-size` / `reference`* / `deriver` per output — to SCRATCH/registration,
/// printing `OUT=<name> <path>` per output. The reference candidates are the
/// closure plus the drv's own outputs (self-references), the daemon's candidate
/// shape. Returns the per-output registration facts (for `realize` to write a td
/// store-db). Shared by `build` (CLOSURE handed in as a file) and `realize`
/// (CLOSURE computed by td itself from the store DB's Refs graph).
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
    let mut record = String::new();
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
        record.push_str(&format!("path {store_path}\n"));
        record.push_str(&format!("nar-hash {hash}\n"));
        record.push_str(&format!("nar-size {size}\n"));
        for r in &refs {
            record.push_str(&format!("reference {r}\n"));
        }
        record.push_str(&format!("deriver {deriver}\n\n"));
        println!("OUT={name} {store_path}");
        regs.push(OutputReg {
            store_path: store_path.clone(),
            nar_hash: hash,
            nar_size: size,
            refs,
            deriver: deriver.clone(),
        });
    }
    std::fs::write(scratch.join("registration"), record).map_err(|e| e.to_string())?;
    Ok(regs)
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
fn cached_realization(
    parsed: &drv::Derivation,
    scratch: &Path,
) -> Result<Option<Vec<OutputReg>>, String> {
    let reg = match std::fs::read_to_string(scratch.join("registration")) {
        Ok(s) => s,
        Err(_) => return Ok(None), // never built here
    };
    // Parse the registration blocks (`path`/`nar-hash`/`nar-size`/`reference`*/`deriver`).
    let mut recs: std::collections::HashMap<String, OutputReg> = std::collections::HashMap::new();
    let mut cur: Option<OutputReg> = None;
    let commit = |cur: &mut Option<OutputReg>,
                  recs: &mut std::collections::HashMap<String, OutputReg>| {
        if let Some(r) = cur.take() {
            recs.insert(r.store_path.clone(), r);
        }
    };
    for line in reg.lines() {
        if let Some(p) = line.strip_prefix("path ") {
            commit(&mut cur, &mut recs);
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
    commit(&mut cur, &mut recs);

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

/// Realize DRV with NO guix-daemon: compute the input closure ITSELF (td's SQLite
/// reader over STORE-DB's Refs graph — the `guix gc -R` the daemon did), build it in
/// the userns sandbox (build_and_register), and register the output(s) into a td
/// store-db at SCRATCH/td.db. Returns the per-output records. Shared by `realize` and
/// `build-recipe`. SRC_OVERRIDE, when set, supplies the recipe source from a td-owned
/// store instead of the daemon store (no `guix repl` interning). STORE_DBS is the set of
/// store-dbs the closure is computed over (a single guix db for `realize`/`build-recipe`;
/// build-plan passes [guix-db, …prior steps' td.dbs] so a downstream build's closure spans
/// both). BUILDER_OVERRIDE, when set, supplies the drv's `builder` from a td-owned store (a
/// td-bootstrapped stage0, not the guix-built td-builder) — the builder entry binds from the
/// builder DB and its direct refs' TRANSITIVE closures come from STORE_DBS. TD_STORE, when
/// set, names td's own store dir holding td-BUILT deps: a closure path whose tree lives
/// under TD_STORE/<base> is emitted `canonical\ton-disk` so the sandbox binds it FROM THERE
/// (the build-plan chaining edge) — the same on-disk encoding SRC_OVERRIDE uses.
fn realize_drv(
    drv_path: &str,
    store_dbs: &[String],
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
    // Compute the input closure over the MERGED graph of every store-db (td's own
    // dbs for td-built deps + guix's db for the transitive seeds). With a single db
    // this is exactly `db.closure`; build-plan passes [guix-db, …td.dbs] so a
    // downstream build's closure spans both.
    if store_dbs.is_empty() {
        return Err("realize: no store DB given".to_string());
    }
    let datas: Vec<Vec<u8>> = store_dbs
        .iter()
        .map(|p| std::fs::read(p).map_err(|e| format!("read store db {p}: {e}")))
        .collect::<Result<_, _>>()?;
    let dbs: Vec<store_db_read::Db> = datas
        .into_iter()
        .map(store_db_read::Db::open)
        .collect::<Result<_, _>>()?;
    let db_refs: Vec<&store_db_read::Db> = dbs.iter().collect();
    // Each td-OWNED interned tree (the recipe source AND the vendored-crate tree) has its
    // own DB — the daemon DB has no row for it. Open them paired with their override so a
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
    // + its DIRECT refs there); the daemon DB has no row for the builder itself.
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
        // root from the MERGED multi-db graph.
        match (builder_override, &builder_db) {
            // The td-placed builder: builder DB gives {builder} ∪ its DIRECT refs;
            // the builder entry binds from on_disk (canonical\ton-disk), and each
            // direct ref's TRANSITIVE closure is read from the merged store-db graph
            // (the pinned toolchain lives there — glibc/gcc-lib + their deps).
            (Some(ov), Some(bdb)) if r == &ov.canonical => {
                for p in bdb.closure(r)? {
                    if p == ov.canonical {
                        closure.insert(format!("{p}\t{}", ov.on_disk));
                    } else {
                        for q in store_db_read::closure_multi(&db_refs, &p)? {
                            closure.insert(q);
                        }
                    }
                }
            }
            _ => {
                for p in store_db_read::closure_multi(&db_refs, r)? {
                    closure.insert(p);
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
                if let Some(base) = e.strip_prefix(store::store_dir().as_str()).and_then(|s| s.strip_prefix('/')) {
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
        "td-builder: realize computed the input closure ITSELF — {} paths from {} store-db(s) (its own reader, no guix gc / no daemon)",
        closure.len(),
        store_dbs.len()
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
fn build_recipe(
    recipe_json: &str,
    lock_file: &str,
    scratch: &Path,
    store_dbs: &[String],
    src_store: Option<(&str, &str)>,
    vendor_store: Option<(&str, &str, &str)>,
    builder_store: Option<(&str, &str, &str)>,
    td_store: Option<&Path>,
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
    eprintln!("td-builder: build-recipe assembled {drv_path} (no guix (derivation), no Guile)");
    // td realizes it (no guix-daemon). With a td-owned source store, the source is
    // staged from td's own store + closure read from the td DB (no daemon interning);
    // with a td-owned builder, the drv's builder is staged from td's store + its
    // closure spans the builder DB ∪ the seed DB (no guix-built builder, brick 2);
    // td_store carries any td-BUILT deps (build-plan) for the multi-db closure's staging.
    let regs = realize_drv(
        &drv_file.to_string_lossy(),
        store_dbs,
        scratch,
        &src_overrides,
        builder_override.as_ref(),
        td_store,
    )?;
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
/// Shared by `build-recipe` (which then realizes it daemon-free) and `assemble-recipe`
/// (assemble-only, so a SEPARATE process — the build daemon — realizes the td-assembled
/// drv). Splitting assembly from realization is what lets td's own daemon, not a `guix
/// repl`-emitted drv, be the build's input (own-builder-daemon §5).
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
        other => return Err(format!("recipe: unknown buildSystem `{other}' (known: gnu, rust, cmake)")),
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
    inputs.sort();
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
/// is the edge the per-package locks could not express: `corpus-no-guix` builds
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
fn build_plan(plan_file: &str, guix_db: &str, scratch: &Path) -> Result<(), String> {
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

        // Closure spans guix's db (seeds) + every prior step's td.db (td deps).
        let mut dbs: Vec<String> = Vec::with_capacity(td_dbs.len() + 1);
        dbs.push(guix_db.to_string());
        dbs.extend(td_dbs.iter().cloned());
        let regs = build_recipe(
            &recipe_text,
            &resolved_lock.to_string_lossy(),
            &step_scratch,
            &dbs,
            None,            // src_store: build-plan locks carry resolved paths
            None,            // vendor_store: build-plan deps are not vendored-crate trees
            None,            // builder_store: build-plan uses the guix-built builder
            Some(&tdstore),  // td_store: stage td-built deps from the shared td-store
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

/// A recipe's declared inputs — the JSON `inputs` array (absent → none).
fn auto_inputs(recipe_dir: &str, name: &str) -> Result<Vec<String>, String> {
    let p = format!("{recipe_dir}/{name}.json");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("read recipe {p}: {e}"))?;
    let alist = json::parse(&text).map_err(|e| format!("recipe JSON {p}: {e}"))?;
    Ok(alist
        .get("inputs")
        .and_then(json::Json::as_arr)
        .map(|a| a.iter().filter_map(json::Json::as_str).map(str::to_string).collect())
        .unwrap_or_default())
}

/// An input is OWNED (td reconstructs it) iff both its recipe JSON and base lock exist;
/// otherwise it is an external seed (the toolchain, retired last) and stays guix-supplied.
fn auto_is_owned(recipe_dir: &str, lock_dir: &str, name: &str) -> bool {
    Path::new(&format!("{recipe_dir}/{name}.json")).exists()
        && Path::new(&format!("{lock_dir}/{name}-no-guix.lock")).exists()
}

/// Post-order DFS over the OWNED-input subgraph: appends each recipe AFTER its owned
/// deps → a topo order (deps first). Cycles error.
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
        match owned_deps.iter().find(|d| auto_entry_is_dep(first, path, d)) {
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
/// Usage: build-plan --auto TARGET RECIPE-DIR LOCK-DIR GUIX-DB SCRATCH
fn build_plan_auto(
    target: &str,
    recipe_dir: &str,
    lock_dir: &str,
    guix_db: &str,
    scratch: &Path,
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
    build_plan(&plan_path.to_string_lossy(), guix_db, scratch)
}

/// Emit a recipe's JSON from its `.ts` with td's OWN TS front-end — `tsgo` (the
/// native TypeScript compiler, NO node) transpiles TS→JS, `td-ts-eval` (the boa
/// evaluator) evaluates it to the recipe JSON. No guix, no Guile — the same two
/// steps as `tests/ts-emit.sh`. Tools come from the env (td-built, placed by the
/// caller): TD_TSGO (dir holding `lib/tsc`), TD_TS_EVAL (the binary), TD_TSDIR (the
/// dialect dir holding `td-spec.d.ts`).
fn emit_recipe_json(recipe_ts: &str) -> Result<String, String> {
    let tsgo = std::env::var("TD_TSGO")
        .map_err(|_| "TD_TSGO must point at td's tsgo dir (with lib/tsc) to emit a recipe".to_string())?;
    let ts_eval = std::env::var("TD_TS_EVAL")
        .map_err(|_| "TD_TS_EVAL must point at td's td-ts-eval binary".to_string())?;
    let tsdir = std::env::var("TD_TSDIR").unwrap_or_else(|_| "tests/ts".to_string());
    let work = std::env::temp_dir().join(format!("td-shell-emit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    // tsgo transpile — same pinned flags as tests/ts-emit.sh, so the JS is the golden emit.
    let tsc = format!("{tsgo}/lib/tsc");
    let ok = Command::new(&tsc)
        .args(["--strict", "--target", "es2020", "--lib", "es2020", "--newLine", "lf", "--removeComments", "--outDir"])
        .arg(&work)
        .arg(format!("{tsdir}/td-spec.d.ts"))
        .arg(recipe_ts)
        .status()
        .map_err(|e| format!("spawn {tsc}: {e}"))?
        .success();
    if !ok {
        return Err(format!("tsgo tsc failed transpiling {recipe_ts}"));
    }
    let stem = Path::new(recipe_ts)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("bad recipe path {recipe_ts}"))?;
    let js = work.join(format!("{stem}.js"));
    let js_bytes = std::fs::read(&js).map_err(|e| format!("tsc produced no JS ({}): {e}", js.display()))?;
    // td-ts-eval: evaluate the JS to the recipe JSON on stdout.
    let mut child = Command::new(&ts_eval)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn td-ts-eval ({ts_eval}): {e}"))?;
    use std::io::Write;
    child
        .stdin
        .take()
        .ok_or("td-ts-eval stdin")?
        .write_all(&js_bytes)
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&work);
    if !out.status.success() {
        return Err(format!(
            "td-ts-eval failed on {recipe_ts}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("td-ts-eval output not UTF-8: {e}"))
}

/// td-builder shell — run a command with td-BUILT packages on PATH. td's own
/// `guix shell`, but with NO guix anywhere: each PKG is resolved to a td RECIPE and
/// BUILT by td-builder itself (the recipe → `td-builder build-recipe`, whose
/// content-addressed cache makes this build-on-demand + cached), then td composes
/// the command's PATH from the td store OUTPUT and execs. There is no `guix`
/// process in the resolve/build/exec path; an unknown package errors ("no td recipe
/// for PKG"), it does NOT fall back to guix. The package that lands on PATH is td's
/// own build at td's own store path. (Boundary, North Star step 1: the build still
/// links the pinned toolchain SEED from the lock — guix-built today, the frozen seed
/// tarball next, CLAUDE.md "North star" step 2 — but no guix runs here.)
///
/// Config (env): TD_SHELL_RECIPES (dir of `recipe-<pkg>.ts`, default `tests/ts`),
/// TD_SHELL_LOCKS (dir of `<pkg>-no-guix.lock`, default `tests`), TD_TSGO/TD_TS_EVAL/
/// TD_TSDIR (td's TS front-end, to emit the recipe), TD_SHELL_STORE_DB (store DB for
/// closure staging, default `/var/guix/db/db.sqlite`), TD_SHELL_CACHE (build cache
/// root, default `$HOME/.cache/td-shell`), TD_BUILDER_PATH/STORE/DB (optional stage0
/// builder override, so the build's builder is td-placed too).
///
/// Usage: shell PKG... [-- CMD ARGS...]
///   PKG...      td package names (a recipe must exist; no guix fallback)
///   -- CMD...   the command to run in the composed env; omitted → interactive $SHELL
fn run_shell(rest: &[String]) -> Result<std::process::ExitStatus, String> {
    // Everything before the first `--` is a package name; after it, the command.
    let sep = rest.iter().position(|a| a == "--");
    let (pkgs, cmd): (&[String], &[String]) = match sep {
        Some(i) => (&rest[..i], &rest[i + 1..]),
        None => (rest, &[]),
    };

    let env_or = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let recipe_dir = env_or("TD_SHELL_RECIPES", "tests/ts");
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

    // Build each named package with td-builder itself — no guix — and collect the
    // td store output's bin/sbin dirs to put on PATH.
    let mut prefix_dirs: Vec<String> = Vec::new();
    for pkg in pkgs {
        // Resolve PKG to a td recipe. No recipe ⇒ loud error, NOT a guix fallback.
        let recipe_ts = format!("{recipe_dir}/recipe-{pkg}.ts");
        if !Path::new(&recipe_ts).is_file() {
            return Err(format!(
                "no td recipe for `{pkg}' ({recipe_ts} not found) — td shell builds td packages, it does not fall back to guix"
            ));
        }
        let lock = format!("{lock_dir}/{pkg}-no-guix.lock");
        if !Path::new(&lock).is_file() {
            return Err(format!("no lock for `{pkg}' ({lock} not found)"));
        }
        // Emit the recipe JSON (td's tsgo + td-ts-eval — no guix), stage it in the
        // per-package cache dir that build-recipe also keys its build cache on.
        let recipe_json = emit_recipe_json(&recipe_ts)?;
        let sd = format!("{cache}/{pkg}");
        std::fs::create_dir_all(&sd).map_err(|e| e.to_string())?;
        let json_file = format!("{sd}/recipe.json");
        std::fs::write(&json_file, &recipe_json).map_err(|e| e.to_string())?;
        // BUILD it via the build-recipe subcommand (its content-addressed cache makes
        // an unchanged recipe a HIT — build-on-demand + cached). A subprocess keeps the
        // build's chatter off the command's stdout, and rides the inherited
        // TD_BUILDER_* override so the builder is the td-placed stage0 too.
        let out = Command::new(&self_exe)
            .args(["build-recipe", &json_file, &lock, &sd, &store_db])
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
fn build_profile(profile_dir: &str, pkgs: &[String]) -> Result<usize, String> {
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
                if dst.exists() {
                    return Err(format!(
                        "profile collision: `{sub}/{}' is provided by more than one package (last: {pkg})",
                        ent.file_name().to_string_lossy()
                    ));
                }
                // Absolute symlink INTO the store (so the profile is a thin view).
                symlink(ent.path(), &dst)
                    .map_err(|e| format!("symlink {} -> {}: {e}", dst.display(), ent.path().display()))?;
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        // S1 sentinel — the rung's run leg greps for this exact line.
        None => {
            println!("td-builder {} ok", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
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
                let socket = std::env::var("TD_DAEMON_SOCKET")
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
                let socket = std::env::var("TD_DAEMON_SOCKET")
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
        // td-store-db: compute the GC-reachable CLOSURE of a path from td's OWN store
        // DB — the daemon's GC "mark" set (`guix gc -R ROOT`), in pure Rust. Reads the
        // DB with td's own reader (`store_db_read`) and walks the Refs graph from ROOT;
        // no daemon. Usage:
        //   store-closure DB ROOT
        // Prints the reachable store paths, sorted (ROOT included).
        Some("store-closure") if args.len() == 4 => {
            let (db_path, root) = (&args[2], &args[3]);
            let run = || -> Result<Vec<String>, String> {
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
                let db = store_db_read::Db::open(bytes)?;
                db.closure(root)
            };
            match run() {
                Ok(paths) => {
                    for p in paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: store-closure {db_path} {root}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // seed-manifest: emit the MANIFEST for a seed closure — the capture half of the
        // frozen seed tarball (North-Star step 2). For the GC closure of ROOT… over DB's
        // Refs graph, print one line per member: `<path> <nar-hash> <nar-size> <ref,ref,…>`
        // (direct refs sorted; `-` if none), all from td's OWN reader + NAR serializer (no
        // daemon). The capture tool tars the same closure; `seed-unpack` restores + registers
        // from this manifest. Usage: seed-manifest DB ROOT...
        Some("seed-manifest") if args.len() >= 4 => {
            let db_path = &args[2];
            let roots = &args[3..];
            let run = || -> Result<Vec<String>, String> {
                let bytes = std::fs::read(db_path).map_err(|e| e.to_string())?;
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
                    let mut rs: Vec<String> = refs.get(p).cloned().unwrap_or_default();
                    rs.sort();
                    rs.dedup();
                    let refstr = if rs.is_empty() { "-".to_string() } else { rs.join(",") };
                    lines.push(format!("{p} {hash} {size} {refstr}"));
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
        // bootstrap brick 2: PLACE a tree WITH references into a td-owned store,
        // content-addressed — the builder analog of store-add-recursive (which REFUSES a
        // referenced tree). td restores the tree into STORE-DIR, computes its
        // content-addressed `source` path from the recursive NAR, SCANS its references
        // against the SEED db's ValidPaths (the pinned toolchain — the glibc/gcc-lib the
        // stage0 builder links), and registers the path + those refs in OUT-DB (each ref a
        // scaffolding ValidPaths row so the Refs join resolves — store-add-referenced's
        // external-ref shape). This lets the loop use a td-BOOTSTRAPPED builder (stage0,
        // NEVER produced by guix) as a recipe's builder-of-record: build-recipe reads its
        // closure as OUT-DB.closure(path) (the builder + its DIRECT refs) ∪ the seed db
        // (those refs' transitive closures). No daemon, no guix. Usage:
        //   store-add-builder NAME TREE STORE-DIR OUT-DB SEED-DB    (prints the store path)
        Some("store-add-builder") if args.len() == 7 => {
            let (name, tree, store_dir, out_db, seed_db) =
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
                // Scan the restored tree for references AGAINST the seed db's valid paths
                // (the pinned toolchain closure) — the builder's actual store deps. The
                // path itself is a candidate so a self-reference is detected. Extra
                // never-matching candidates cannot add references (scan.rs candidate note).
                let seed = store_db_read::Db::open(
                    std::fs::read(seed_db).map_err(|e| format!("read seed db {seed_db}: {e}"))?,
                )?;
                let mut candidates: Vec<String> = seed
                    .table("ValidPaths")?
                    .into_iter()
                    .filter_map(|(_, cols)| match cols.get(1) {
                        Some(store_db_read::Value::Text(p)) => Some(p.clone()),
                        _ => None,
                    })
                    .collect();
                candidates.push(path.clone());
                let mut s = scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
                nar::write_nar(&mut s, &disk).map_err(|e| e.to_string())?;
                let (hash, size, mut refs) = s.finish();
                refs.sort();
                refs.dedup();
                // Register: id 1 = the builder (full record), each external reference a
                // scaffolding ValidPaths row (path only) so the Refs ids resolve. So
                // OUT-DB.closure(path) returns the builder + its DIRECT refs; realize then
                // spans those refs' transitive closures from the seed db.
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
                let socket = std::env::var("TD_DAEMON_SOCKET")
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
        // registration record (plan/td-builder.md Q3) under
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
        // guix-daemon is no longer in the realize path — it is only the differential
        // oracle (prime directive 4). Usage:
        //   realize DRV STORE-DB SCRATCH
        Some("realize") if args.len() == 5 => {
            let (drv_path, store_db, scratch) = (&args[2], &args[3], &args[4]);
            let run = || -> Result<(), String> {
                realize_drv(drv_path, std::slice::from_ref(store_db), Path::new(scratch), &[], None, None)
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
        // Usage:  daemon SOCKET STORE-DB SCRATCH-BASE
        Some("daemon") if args.len() == 5 => {
            let (socket, store_db, scratch) = (&args[2], &args[3], &args[4]);
            let bp = std::env::var("TD_BUILDER_PATH").ok();
            let bs = std::env::var("TD_BUILDER_STORE").ok();
            let bd = std::env::var("TD_BUILDER_DB").ok();
            let builder_override = match (&bp, &bs, &bd) {
                (Some(canonical), Some(store_dir), Some(db)) => {
                    let base = canonical.rsplit('/').next().unwrap_or(canonical);
                    Some(BuilderOverride {
                        canonical: canonical.clone(),
                        on_disk: format!("{store_dir}/{base}"),
                        db: db.clone(),
                    })
                }
                (None, None, None) => None,
                _ => {
                    eprintln!(
                        "td-builder: daemon: TD_BUILDER_PATH/TD_BUILDER_STORE/TD_BUILDER_DB must be set together"
                    );
                    return ExitCode::FAILURE;
                }
            };
            let realize = |drv: &str, scr_base: &Path| -> Result<(String, String), String> {
                // Parse the drv so its (content-addressed) first-output basename can
                // key a STABLE per-drv scratch — the same drv always lands in the same
                // dir, so a prior valid realization there is a cache HIT.
                let content = std::fs::read(drv).map_err(|e| format!("read {drv}: {e}"))?;
                let parsed =
                    drv::parse(&content).map_err(|e| format!("parse drv {drv}: {e}"))?;
                let first_out = parsed
                    .outputs
                    .first()
                    .ok_or_else(|| format!("{drv}: derivation has no outputs"))?;
                let key = first_out
                    .path
                    .rsplit('/')
                    .next()
                    .ok_or_else(|| format!("{}: not a store path", first_out.path))?;
                let scr = scr_base.join(key);
                let mk = |regs: &[OutputReg]| -> Result<(String, String), String> {
                    let first =
                        regs.first().ok_or_else(|| "realize produced no outputs".to_string())?;
                    let canon = first.store_path.clone();
                    let base = canon
                        .strip_prefix("/gnu/store/")
                        .ok_or_else(|| format!("{canon}: not a store path"))?;
                    let host = scr.join("newstore").join(base);
                    Ok((canon, host.to_string_lossy().into_owned()))
                };
                // guix-daemon parity: don't rebuild a valid output. If this exact drv
                // was already realized here AND its output is still present + NAR-verifies,
                // serve it from cache.
                if let Some(regs) = cached_realization(&parsed, &scr)? {
                    eprintln!("td-builder: daemon CACHE HIT for {drv} — output already valid under {}, not rebuilding", scr.display());
                    return mk(&regs);
                }
                eprintln!("td-builder: daemon CACHE MISS for {drv} — realizing");
                let regs = realize_drv(
                    drv,
                    std::slice::from_ref(store_db),
                    &scr,
                    &[],
                    builder_override.as_ref(),
                    None,
                )?;
                mk(&regs)
            };
            match build_daemon::serve(socket, realize, Path::new(scratch)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: daemon: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder daemon-request — the in-process client for `daemon` (so a
        // caller needs no nc/socat): connect to SOCKET, send DRV, print the daemon's
        // single-line response and exit 0 only on "OK …". Usage:
        //   daemon-request SOCKET DRV
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
        // daemon-resident source path, as before. Usage:
        //   build-recipe RECIPE-JSON-FILE LOCK SCRATCH STORE-DB [SRC-STORE-DIR SRC-DB]
        Some("build-recipe") if args.len() == 6 || args.len() == 8 || args.len() == 11 => {
            let (recipe_file, lock, scratch, store_db) =
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
            // closure is computed from the seed DB and every seed input binds from the
            // unpacked store (TD_SEED_STORE/<base>) — so STORE-DB (/var/guix) and the live
            // /gnu/store are out of the build path. Set together; the build is otherwise
            // identical (same drv, same output).
            let seed_store = std::env::var("TD_SEED_STORE").ok();
            let seed_db = std::env::var("TD_SEED_DB").ok();
            let run = || -> Result<(), String> {
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
                let (store_dbs, td_store): (Vec<String>, Option<&Path>) =
                    match (&seed_store, &seed_db) {
                        (Some(s), Some(d)) => (vec![d.clone()], Some(Path::new(s))),
                        (None, None) => (vec![store_db.clone()], None),
                        _ => return Err("TD_SEED_STORE/TD_SEED_DB must be set together".into()),
                    };
                let recipe_json =
                    std::fs::read_to_string(recipe_file).map_err(|e| e.to_string())?;
                build_recipe(
                    &recipe_json,
                    lock,
                    Path::new(scratch),
                    &store_dbs,
                    src_store,
                    vendor_store,
                    builder_store,
                    td_store,
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
        // spans GUIX-DB ∪ the prior steps' td.dbs, staged from a shared td-store.
        // Usage: build-plan PLAN GUIX-DB SCRATCH
        Some("build-plan") if args.len() == 5 => {
            let (plan_file, guix_db, scratch) = (&args[2], &args[3], &args[4]);
            match build_plan(plan_file, guix_db, Path::new(scratch)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build-plan {plan_file}: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // td-builder build-plan --auto — GENERATE the plan from the recipe GRAPH (no
        // hand-written plan/manifest): topo-sort TARGET's owned-input closure, mark each
        // owned-input dep `td-recipe-output`, and run it. An input is owned iff
        // RECIPE-DIR/<name>.json and LOCK-DIR/<name>-no-guix.lock both exist.
        // Usage: build-plan --auto TARGET RECIPE-DIR LOCK-DIR GUIX-DB SCRATCH
        Some("build-plan") if args.len() == 8 && args[2] == "--auto" => {
            let (target, recipe_dir, lock_dir, guix_db, scratch) =
                (&args[3], &args[4], &args[5], &args[6], &args[7]);
            match build_plan_auto(target, recipe_dir, lock_dir, guix_db, Path::new(scratch)) {
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
        Some("check") if args.len() == 5 => {
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
        Some("profile") if args.len() >= 4 => match build_profile(&args[2], &args[3..]) {
            Ok(n) => {
                eprintln!("td-builder: profile {} — linked {n} entr{}", args[2], if n == 1 { "y" } else { "ies" });
                println!("{}", args[2]);
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("td-builder: profile: {e}");
                ExitCode::FAILURE
            }
        },
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
        // loop-sandbox: the DEV-SHELL — run a command inside td's own hermetic
        // container (pivot into a fresh root exposing the WHOLE /gnu/store (ro),
        // the daemon socket /var/guix, /proc, /dev; host-guix on PATH; its own
        // loopback-only netns), toward replacing `guix shell -C`. With
        // `--expose-cwd` it adds the FULL loop env (worktree + cgroups + guix
        // cache, caller PATH + TD_CHECK_* preserved, chdir into the cwd) so a real
        // rung runs as under `guix shell -C`. Usage:
        //   host-sandbox [--expose-cwd] -- CMD ARGS...
        Some("host-sandbox") if args.len() >= 4 => {
            let mut i = 2usize;
            let mut expose_cwd = false;
            while i < args.len() && args[i] != "--" {
                match args[i].as_str() {
                    "--expose-cwd" => expose_cwd = true,
                    other => {
                        eprintln!("td-builder: host-sandbox: unknown flag `{other}'");
                        return ExitCode::from(2);
                    }
                }
                i += 1;
            }
            if i >= args.len() || i + 1 >= args.len() {
                eprintln!("usage: td-builder host-sandbox [--expose-cwd] -- CMD ARGS...");
                return ExitCode::from(2);
            }
            let cmd = args[i + 1].clone();
            let cmd_args: Vec<String> = args[i + 2..].to_vec();
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
                let mut binds = vec![
                    sandbox::Bind { src: "/gnu/store".to_string(), dest: None, readonly: true, ro_optional: false },
                    sandbox::Bind { src: "/var/guix".to_string(), dest: None, readonly: false, ro_optional: false },
                ];
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
                    }
                    let cache = format!("{home}/.cache/guix");
                    if Path::new(&cache).is_dir() {
                        binds.push(sandbox::Bind { src: cache, dest: None, readonly: false, ro_optional: false });
                    }
                    path_env = std::env::var("PATH").unwrap_or_default();
                    workdir = cwd;
                    for (k, v) in std::env::vars() {
                        if k.starts_with("TD_CHECK_") {
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
        // input-resolution: RESOLVE recipe input names -> store paths from a PINNED
        // lock (one `NAME<whitespace>STORE-PATH` per line, `#` comments allowed) —
        // the lookup system/td-build.scm does via Guile's `specification->package`
        // -> `package-derivation` -> output path. Additive equivalence FIRST (the
        // gate pattern of loop-sandbox/td-check): the `resolve` rung proves
        // td-builder's lock resolution EQUALS Guile's live resolution (the oracle);
        // the build is unchanged. The lock is a pinned artifact (regenerated on a
        // channel bump, like DIGESTS.md); the RESOLVER that computes it stays Guile,
        // retired package-by-package later (§5: toolchain retired last). Usage:
        //   resolve LOCKFILE NAME...
        // Prints one resolved store path per NAME, in order; errors if a NAME is
        // absent (so a recipe input the pinned lock does not cover fails loudly).
        Some("resolve") if args.len() >= 4 => {
            let lockfile = &args[2];
            let names = &args[3..];
            let run = || -> Result<Vec<String>, String> {
                let text =
                    std::fs::read_to_string(lockfile).map_err(|e| format!("{lockfile}: {e}"))?;
                let mut map = std::collections::HashMap::new();
                for (i, line) in text.lines().enumerate() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    let mut it = line.splitn(2, char::is_whitespace);
                    let name = it.next().unwrap_or_default().trim();
                    let path = it.next().unwrap_or_default().trim();
                    if name.is_empty() || path.is_empty() {
                        return Err(format!("{lockfile}:{}: malformed lock line", i + 1));
                    }
                    if map.insert(name.to_string(), path.to_string()).is_some() {
                        return Err(format!("{lockfile}:{}: duplicate name `{name}'", i + 1));
                    }
                }
                names
                    .iter()
                    .map(|n| {
                        map.get(n)
                            .cloned()
                            .ok_or_else(|| format!("name `{n}' not in lock {lockfile}"))
                    })
                    .collect()
            };
            match run() {
                Ok(paths) => {
                    for p in paths {
                        println!("{p}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("td-builder: resolve: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        // corpus-independence: run AS a derivation's builder, executing the
        // autotools phases in Rust (replaces gnu-build-system). Reads the build
        // environment from env vars (out, TD_SRC, TD_INPUTS, TD_CONFIGURE_FLAGS)
        // that system/td-build.scm sets on the derivation.
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
        // env-driven derivation-builder contract (system/td-build.scm).
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
        // env-driven derivation-builder contract (system/td-build.scm).
        Some("cmake-build") if args.len() == 2 => match build::run_cmake() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("td-builder: cmake-build: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("usage: td-builder            # print the S1 sentinel");
            eprintln!("       td-builder nar-hash PATH");
            eprintln!("       td-builder drv-parse FILE.drv");
            eprintln!("       td-builder build FILE.drv CLOSURE-FILE SCRATCH-DIR");
            eprintln!("       td-builder check FILE.drv CLOSURE-FILE SCRATCH-DIR");
            eprintln!("       td-builder store-register STORE-PATH DERIVER CANDIDATES-FILE OUT-DB");
            eprintln!("       td-builder store-query DB info|references");
            eprintln!("       td-builder store-closure DB ROOT");
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
}
