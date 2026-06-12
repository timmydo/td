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

mod drv;
mod nar;
mod sha256;

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
        _ => {
            eprintln!("usage: td-builder            # print the S1 sentinel");
            eprintln!("       td-builder nar-hash PATH");
            eprintln!("       td-builder drv-parse FILE.drv");
            ExitCode::from(2)
        }
    }
}
