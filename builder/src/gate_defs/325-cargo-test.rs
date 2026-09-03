//! cargo-test — the td Rust engine's fast checks: `cargo clippy` (the coding-rules
//! lint) THEN `cargo test` (the unit tests), run DIRECTLY on the dependency-free
//! engine crates (offline, toolchain-only). The loop-latency brainstorm's "push
//! logic down into fast unit tests" first step, now also the enforcement point for
//! the AGENTS.md Rust coding rules.
//! 
//! The crates are not listed here. This gate asks `td-builder gate-crates` for
//! the same roster `affected.rs` derives from the tree, because the list it used
//! to keep — a lock check, a clippy line, a test line and a closing sentence,
//! each spelled out per crate — had drifted three crates behind it: td-jail,
//! td-portal and td-profiler were in none of them, so this gate silently did not
//! lint or test three shipped crates. A crate joins by existing, and declares
//! any deviation in its own `[package.metadata.td-gate]` block.
//!
//! clippy leg (AGENTS.md → "Rust code"): the engine workspace (builder, recipes,
//! and the shared std-only engine lib) and every roster crate must lint
//! clean under the `[lints]` table each Cargo.toml declares at `deny` — NO panicking
//! surface (unwrap/expect/panic!/unreachable!/todo!/unimplemented!), `.get(i)` over
//! panicking `xs[i]`, and `unsafe` confined to the raw-syscall layer (builder's is
//! sys.rs; td-kexec, td-netd, td-init, td-login, td-svc, and td-compositor carry the
//! six recorded target-side exceptions — td-init's, td-login's, td-svc's, and
//! td-compositor's own confinement tests
//! additionally pin their scoped-allow counts and syscall rosters, and td-login's the
//! ORDER its three credential syscalls are issued in, none of which the compiler
//! checks).
//! Existing code is grandfathered (per-file `#![allow]` in modules; per-item `#[allow]` on the
//! crate root's own fns/impls — a crate-root inner `#![allow]` is crate-GLOBAL and
//! would silently exempt everything), so a denied lint reds ONLY on NEW code. Also
//! the one-way "no crates" guard: builder/recipes/engine share ONE workspace-root
//! Cargo.lock and stay dependency-free — asserted two ways: one `[[package]]`
//! entry per workspace member AND no external `source = ` line (path members
//! carry none). The member COUNT is now derived from the root manifest's
//! `members` list rather than pinned at 3, so adding a path member no longer
//! reds here; that it is a reviewed change is enforced by review, and by
//! `the_workspace_lock_count_follows_the_members_list`. The `source = ` half
//! went the other way: the old script applied it to the root lock alone, and
//! it now runs over every lock in the roster. Every
//! roster crate keeps a 1-package lock, checked the same two ways over the
//! derived list by `gate-crates locks`. Most are TARGET-built programs — the
//! shipped userland, from the boot shim and installer up to the compositor and
//! session broker; `td-builder gate-crates names` prints the current set, and
//! each crate's own manifest says what it is. (This paragraph used to
//! enumerate their roles, and had already drifted past the three crates whose
//! absence this gate was fixed for.) They are
//! not engine code, but are pure std and compile offline, so
//! they lint/test here with the engine crates. A crate that declares
//! `clippy-all-targets` is linted
//! `--all-targets`, so its tests are held to the coding rules too — each takes
//! the AGENTS.md `#[cfg(test)]` panic exemption through its own `clippy.toml`
//! (`allow-*-in-tests`), which scopes it to tests rather than to a file. td-svc's
//! unsafe surface is one `kill(2)`, which safe std has
//! no route to at all; DESIGN.md records that and why every OTHER capability it
//! needs is reachable through safe std. td-review is the one
//! HOST-side crate here (in neither bootstrap graph, but pure std and offline), and
//! only its `--bins` tests run — declared as `gate-test-args`, separately from the
//! `test-args` the host preflight uses, because the two legs deliberately run
//! different suites: its integration tests need a `git`, which the
//! sandbox toolchain has none of, so they run in the host `cargo-test` preflight.
//! The network tools
//! (fetch/feed/subst) carry the vendored
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
//! stays in the full `check`.
//! This gate IS the per-PR engine check: the `cargo-test` preflight of
//! `td-builder affected-checks` runs `cargo test --frozen` on the dev host's own
//! rust — no store image, no hosted CI (GitHub is a backup remote only). The
//! deep from-source gates stay on the dev-machine full `td-builder check` (the
//! §7.2 step-2 landing gate).

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
	"$td" gate-crates locks || exit $?; \
rustpath=`"$td" provision-rust` || exit $?; \
ccpath=`"$td" provision-cc` || exit $?; \
scratch="$PWD/.cargo-test-scratch"; \
rm -rf "$scratch"; mkdir -p "$scratch/home" "$scratch/target"; \
log="$scratch/out.log"; \
cmds=`"$td" gate-crates cargo-cmds` || exit $?; \
names=`"$td" gate-crates names` || exit $?; \
PATH="$rustpath:$ccpath:$PATH" \
CARGO_HOME="$scratch/home" CARGO_TARGET_DIR="$scratch/target" \
	  sh -c "set -e
$cmds" 2>&1 | tee "$log"; \
	"$td" text cargo-test-ok "$log" || \
	  { echo "ERROR: cargo test reported no passing tests (vacuous run?)" >&2; exit 1; }; \
rm -rf "$scratch"; \
echo "PASS: cargo-test — the engine workspace (builder + recipes + engine) and $names are dependency-free and lint clean; their unit tests pass (guix-free toolchain)."

"##,
    }
}
