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
            eprintln!("       td-builder autotools-build   # as a derivation builder");
            ExitCode::from(2)
        }
    }
}
