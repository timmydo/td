//! cargo-test — the td Rust engine's fast checks: `cargo clippy` (the coding-rules
//! lint) THEN `cargo test` (the unit tests), run DIRECTLY on the dependency-free
//! engine crates (offline, toolchain-only). The loop-latency brainstorm's "push
//! logic down into fast unit tests" first step, now also the enforcement point for
//! the AGENTS.md Rust coding rules.
//! 
//! clippy leg (AGENTS.md → "Rust code"): the engine workspace (builder, recipes,
//! and the shared std-only engine lib) and td-kexec must lint
//! clean under the `[lints]` table each Cargo.toml declares at `deny` — NO panicking
//! surface (unwrap/expect/panic!/unreachable!/todo!/unimplemented!), `.get(i)` over
//! panicking `xs[i]`, and `unsafe` confined to the raw-syscall layer (builder's is
//! sys.rs; td-kexec's is its own kexec_file_load/reboot syscall wrapper — it alone
//! sets `unsafe_code = "allow"`, the recorded AGENTS.md amendment). Existing code
//! is grandfathered (per-file `#![allow]` in modules; per-item `#[allow]` on the
//! crate root's own fns/impls — a crate-root inner `#![allow]` is crate-GLOBAL and
//! would silently exempt everything), so a denied lint reds ONLY on NEW code. Also
//! the one-way "no crates" guard: builder/recipes/engine share ONE workspace-root
//! Cargo.lock and stay dependency-free — asserted two ways so BOTH a registry/git
//! dep AND a new path member are caught: exactly 3 `[[package]]` entries (the known
//! members) AND no external `source = ` line (path members carry none). td-kexec keeps
//! its own 1-package lock. td-kexec is a TARGET guest program,
//! not engine code, but it is pure std and compiles offline, so it lints/tests here
//! with the engine crates. The network tools (fetch/feed/subst) carry the vendored
//! FSDG crates and can't compile offline, so they are NOT linted here; their
//! Cargo.toml still declares the same `[lints]` table so a local `cargo clippy`
//! enforces it.
//! 
//! test leg: the `#[test]`s in builder/src/*.rs (NAR framing, SHA-256 vectors, drv
//! parse/emit, the store-db SQLite encode/decode + reader, scan, sandbox) otherwise
//! run ONLY inside the cargo-build-system package build — a full release rebuild that
//! ~15 heavy gates trigger. Running them here reds a Rust-logic regression in seconds
//! instead of deep in the td-builder/store/drv ladder. recipes/ tests run too —
//! the evaluator's provenance classification and SHA-256 are enforcement code
//! (re #469), and its regressions must red in-loop, not only in CI.
//! 
//! GUIX-FREE toolchain (R1 of the guix-retirement ladder, github issue #274): the Rust +
//! C toolchain is resolved by `td-builder provision-{rust,cc}` (builder/src/stage0.rs) —
//! the SAME guix-free resolvers the stage0 td-builder SEED build uses (a PROVIDED
//! TD_RUST_HOME/TD_CC_HOME, or rustup/system cc on a guix-less host) — NOT a `guix shell`
//! process. No guix is invoked here anymore. The seed-lock toolchain fallback is retired,
//! so a runner with no host cc/rustup and nothing mounted (the loop sandbox) can't
//! provision one — provision-{rust,cc} then exit EXIT_UNPROVISIONED (69) and this gate
//! degrades to a tolerated Unprovisioned/SKIP (below), while a real failure still REDs.
//! 
//! Offline by construction: the provisioned rust bin dir carries rustc + cargo-clippy +
//! clippy-driver, the cargo bin dir carries cargo, and the cc bin dir (gcc-toolchain, rust's
//! default linker driver) is prepended to PATH — all resolved guix-free by `provision-{rust,cc}`
//! (a PROVIDED TD_RUST_HOME/TD_CC_HOME, or rustup/system cc). `cargo clippy/test --frozen`
//! (= --locked --offline) on DEPENDENCY-FREE crates touches no network.
//! Scratch CARGO_HOME/CARGO_TARGET_DIR live in .cargo-test-scratch/ at the repo ROOT — OUTSIDE
//! the crate dirs, so they cannot perturb the td-builder/td-recipe package source hashes.
//! `set -e` inside the shell + pipefail keep a FAILED clippy or test from being greened by the
//! `tee`, and the `test result: ok. <N> passed` (N>=1) assertion rejects a vacuous 0-test run.
//! The build-engine smoke tier (`check-engine`) is JUST this — compile the engine,
//! lint it, and run its unit tests, ~2-4 min, no from-source builds. Anything that
//! builds a package (bootstrap-build/build-plan/td-check/corpus/…) is NOT smoke; it
//! stays in the full `check` / daily backstop.
//! This gate IS the per-PR engine check: the `cargo-test` preflight of
//! `td-builder affected-checks` runs `cargo test --frozen` on the dev host's own
//! rust — no store image, no hosted CI (GitHub is a backup remote only). The
//! deep from-source gates stay on the dev-machine full `td-builder check` (the
//! §7.2 step-2 landing gate) + the nightly daily suite.

use crate::gates::{GateDef, Pool};

pub fn gate() -> GateDef {
    GateDef {
        name: "cargo-test",
        pools: &[Pool::Heavy, Pool::Engine],
        needs: &[],
        build_gate: false,
        specs: &[],
        // The engine's process-supervision unit tests (build::tests::watchdog_*)
        // now spawn a POSIX `sh` resolved from PATH — busybox `sh` (ash) in the
        // loop host-sandbox, the system `/bin/sh` on a dev host — and their
        // scripts use only shell builtins plus `kill`/integer `sleep`, so no seed
        // bash (and no guix lock) is bound here anymore.
        //
        // In the guix-free loop sandbox no toolchain is reachable, so
        // provision-{rust,cc} exit EXIT_UNPROVISIONED (69) (propagated by the
        // `|| exit $?` below) and the gate degrades to a tolerated Unprovisioned
        // SKIP — even as an explicit goal. A real clippy/test failure exits
        // non-69 and still REDs; the blocking host-side cargo-test preflight
        // (affected-checks --run) is the authoritative from-source enforcement.
        non_blocking: false,
        script: r##"
	echo ">> cargo-test: engine crates lint clean (cargo clippy: no panic surface, .get over indexing, unsafe confined) + td-builder unit tests (cargo test) — offline, guix-free toolchain (td-builder provision-{rust,cc})"
	set -euo pipefail; \
	td="${TD_BUILDER_SELF:?gate-run exports TD_BUILDER_SELF}"; \
	w=`"$td" text count-line-exact '[[package]]' Cargo.lock`; \
	test "$w" -eq 3 || { echo "ERROR: the engine workspace root Cargo.lock lists $w packages, expected exactly 3 (td-builder, td-recipe, td-engine); a NEW crate — even an in-repo path member — must be a reviewed change (AGENTS.md 'Rust code'). Update this count deliberately if you are adding one." >&2; exit 1; }; \
	"$td" text not-contains 'source = "' Cargo.lock || { echo "ERROR: the engine workspace is no longer dependency-free (root Cargo.lock carries an external 'source = ' — builder/recipes/engine must carry ZERO external crates; AGENTS.md 'Rust code')" >&2; exit 1; }; \
	n=`"$td" text count-line-exact '[[package]]' td-kexec/Cargo.lock`; \
	test "$n" -eq 1 || { echo "ERROR: td-kexec is no longer dependency-free (Cargo.lock lists $n packages; it must carry ZERO crates — AGENTS.md 'Rust code')" >&2; exit 1; }; \
rustpath=`"$td" provision-rust` || exit $?; \
ccpath=`"$td" provision-cc` || exit $?; \
scratch="$PWD/.cargo-test-scratch"; \
rm -rf "$scratch"; mkdir -p "$scratch/home" "$scratch/target"; \
log="$scratch/out.log"; \
PATH="$rustpath:$ccpath:$PATH" \
CARGO_HOME="$scratch/home" CARGO_TARGET_DIR="$scratch/target" \
	  sh -c 'set -e; \
	    cargo clippy --frozen --workspace; \
	    cargo clippy --frozen --manifest-path td-kexec/Cargo.toml; \
	    cargo test  --frozen --workspace; \
	    cargo test  --frozen --manifest-path td-kexec/Cargo.toml' 2>&1 | tee "$log"; \
	"$td" text cargo-test-ok "$log" || \
	  { echo "ERROR: cargo test reported no passing tests (vacuous run?)" >&2; exit 1; }; \
rm -rf "$scratch"; \
echo "PASS: cargo-test — the engine workspace (builder + recipes + engine) and td-kexec are dependency-free and lint clean; their unit tests pass (guix-free toolchain)."
"##,
    }
}
