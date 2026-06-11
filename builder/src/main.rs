//! td-builder — td's own builder (DESIGN §7.1 side-track; plan/td-builder.md).
//!
//! Goal of the track: a td-owned Rust binary that executes a `.drv` in a
//! user-namespace sandbox and registers the output, proven behaviorally
//! equivalent to the pinned `guix-daemon` (prime directive 4 — the daemon is
//! the oracle; never replace without a differential).
//!
//! This file is the S1 milestone only: the *toolchain probe*. It is a
//! hello-world skeleton whose sole job is to prove the pinned channel's Rust
//! toolchain compiles td-builder OFFLINE inside the check.sh sandbox and yields
//! a working, reproducible executable. The real builder grows here:
//!   • S2 — a NAR serializer + hasher, bit-for-bit equal to the daemon's;
//!   • S3 — an ATerm `.drv` parser + a userns build sandbox + store registration;
//!   • S4 — the daemon-vs-td-builder store differential, as a check.sh rung.
//!
//! Keeping S1 a genuine hello-world is deliberate: the smallest change that
//! turns one test (the `td-builder` rung) green (CLAUDE.md "Definition of done").

fn main() {
    // A stable sentinel the `td-builder` rung greps for. Printing it proves the
    // COMPILED BINARY ran — a stronger claim than "cargo build exited 0", which
    // a broken runtime could still satisfy. The version comes from Cargo.toml so
    // the sentinel tracks the crate as it grows.
    println!("td-builder {} ok", env!("CARGO_PKG_VERSION"));
}
