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
mod daemon;
mod drv;
mod nar;
mod sandbox;
mod scan;
mod sha256;
mod store;
mod store_db;
mod store_db_read;
mod sys;

use std::path::Path;
use std::process::ExitCode;

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
                // the deriver = 2 (a scaffolding row — a `.drv`, not a closure member,
                // so DerivationOutputs.drv resolves), then the other closure paths in
                // file order = 3.. . Every reference is a closure member.
                let others: Vec<String> = closure
                    .iter()
                    .filter(|p| p.as_str() != store_path.as_str())
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
                // id 2: the deriver scaffolding row (path only).
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
                let bytes = std::fs::read(drv_path).map_err(|e| e.to_string())?;
                let parsed = drv::parse(&bytes).map_err(|e| e.to_string())?;
                // The deriver recorded is the .drv's OWN store path. For a
                // store-path input that is drv_path; for an emitted .drv handed in
                // from outside the store (td-drv-build builds the file td wrote),
                // compute its content-addressed store path so the registration
                // matches the daemon's recorded deriver.
                let deriver = if drv_path.starts_with(store::STORE_DIR) {
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
                    let mut refs: Vec<String> =
                        parsed.input_drvs.iter().map(|(p, _)| p.clone()).collect();
                    refs.extend(parsed.input_srcs.iter().cloned());
                    store::drv_store_path(&drv_name, &bytes, &refs)
                };
                let closure: Vec<String> = std::fs::read_to_string(closure_file)
                    .map_err(|e| e.to_string())?
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                let outputs = sandbox::build(&parsed, drv_path, &closure, Path::new(scratch))
                    .map_err(|e| e.to_string())?;
                // Reference candidates: the staged closure plus the drv's own
                // outputs (self-references), the daemon's candidate shape.
                let mut candidates = closure.clone();
                candidates.extend(parsed.outputs.iter().map(|o| o.path.clone()));
                let mut record = String::new();
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
                }
                std::fs::write(Path::new(scratch).join("registration"), record)
                    .map_err(|e| e.to_string())?;
                Ok(())
            };
            match run() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("td-builder: build {drv_path}: {e}");
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
                // The base exposure set: the whole store (ro), the daemon socket
                // + GC roots (rw), /proc, and the host device tree.
                let mut binds = vec![
                    sandbox::Bind { src: "/gnu/store".to_string(), readonly: true },
                    sandbox::Bind { src: "/var/guix".to_string(), readonly: false },
                    sandbox::Bind { src: "/proc".to_string(), readonly: false },
                    sandbox::Bind { src: "/dev".to_string(), readonly: false },
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
                    binds.push(sandbox::Bind { src: cwd.clone(), readonly: false });
                    if Path::new("/sys/fs/cgroup").is_dir() {
                        binds.push(sandbox::Bind {
                            src: "/sys/fs/cgroup".to_string(),
                            readonly: true,
                        });
                    }
                    let cache = format!("{home}/.cache/guix");
                    if Path::new(&cache).is_dir() {
                        binds.push(sandbox::Bind { src: cache, readonly: false });
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
                sandbox::host_shell(
                    &cmd, &cmd_args, &binds, &tmpfs, &path_env, &home, &workdir, &extra_env,
                    &scratch,
                )
                .map_err(|e| e.to_string())
            };
            match run() {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
                Err(e) => {
                    eprintln!("td-builder: host-sandbox: {e}");
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
            eprintln!("       td-builder autotools-build   # as a derivation builder");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
