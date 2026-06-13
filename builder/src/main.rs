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
mod drv;
mod nar;
mod sandbox;
mod scan;
mod sha256;
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

fn nar_hash(path: &str) -> Result<String, std::io::Error> {
    let mut w = HashWriter(sha256::Sha256::new());
    nar::write_nar(&mut w, Path::new(path))?;
    Ok(format!("sha256:{}", sha256::to_base16(&w.0.finalize())))
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
                    record.push_str(&format!("deriver {drv_path}\n\n"));
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
            eprintln!("       td-builder autotools-build   # as a derivation builder");
            ExitCode::from(2)
        }
    }
}
