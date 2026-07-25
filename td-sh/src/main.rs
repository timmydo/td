//! td-sh — td's target-built POSIX `/bin/sh`, intended to replace busybox
//! `sh`/`ash` as the shipped shell (system-x86-64) once it can pass conformance.
//!
//! STATUS: STUB. This is the exit-0 skeleton. It parses and executes NOTHING; it
//! ignores its arguments and exits 0. That is deliberate: it establishes the
//! build recipe, the static-ELF shape, and — with the conformance harness in
//! this crate's `lib.rs` — a RED baseline in which every spec case fails against
//! it. Subsequent PRs grow the tokenizer, parser, expander, and builtins until
//! the Oils spec corpus (resolved to the dash/ash goldens) and, later, the
//! busybox `ash_test` parity gate go green. Only then is td-sh wired into the
//! image as `/bin/sh`; until it passes, the shipped shell stays busybox.
//!
//! Like td-kexec, the SHIPPED binary is this single file compiled directly by
//! rustc (`recipes/src/recipes/td-sh.rs`), statically linked (`+crt-static`)
//! into an ET_EXEC with an empty runtime closure so it can run in stage-1 init
//! before the dynamic uutils glibc closure is reachable (the boot-critical
//! static-`sh` constraint enforced by system-x86-64). Because the recipe
//! compiles THIS file alone, `main.rs` must stay self-contained: the conformance
//! harness lives in `lib.rs` and is host-side tooling, never compiled in here.
//!
//! std + zero external crates (AGENTS.md "std, not no_std"): a POSIX sh is a thin
//! orchestration layer over exactly the OS services `std` wraps, and `std` is the
//! pinned toolchain, not a cargo dependency. Where `std` won't expose a needed
//! syscall (e.g. job-control `setpgid`/`tcsetpgrp`), a future confined raw-syscall
//! module mirrors `builder/src/sys.rs` — no external crate is introduced.
#![deny(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    // Stub: a real /bin/sh reads argv (`-c CMD`, a script path, or interactive)
    // and runs the program. Until the interpreter exists, exit 0 unconditionally.
    // This is what makes every conformance case RED — they assert real output and
    // statuses that an exit-0 no-op cannot produce.
    ExitCode::SUCCESS
}
