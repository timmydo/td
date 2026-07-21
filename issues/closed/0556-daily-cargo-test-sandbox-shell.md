---
title: cargo-test gate reds in the daily loop sandbox (no bash / no /bin/sh)
labels: [daily-red, test]
blocked-by: none
---

## What

The `cargo-test` gate (heavy + engine pool) reds in the full daily `check`
because 11 engine unit tests panic on ambient tools the minimal loop sandbox
deliberately omits. On any dev host and the per-PR `check-engine` /
`affected-checks` cargo-test job (which run with a full userland) the same tests
pass, so the daily backstop is red while every per-PR gate is green.

Verdict (fresh main `e35b24a`, full `td-builder check`): `cargo-test: FAIL
(71.6s)` — `387 passed; 11 failed`:

- 9 × `build::tests::watchdog_*` panic at `build.rs` `bash_and_env()`
  (`expect("bash on PATH")`): the sandbox PATH is env_clear'd and carries no
  `bash`.
- 2 × `stage0::tests::provision_glibc_static_*` panic with "no static glibc
  found": their fixtures write `#!/bin/sh` fake compilers, and the sandbox has
  no `/bin/sh` to exec them, so `provision_glibc_static`'s cc-probe leg falls
  through.

The sandbox omitting `bzip2` / `/bin/sh` / a general shell is by design
(`check_loop.rs` `check-rung` doc). These 11 tests carry an undeclared
dependency on an ambient Unix userland that only bites in that sandbox.

## Entry points

- `builder/src/build.rs` — `bash_and_env()` test helper + the 9 `watchdog_*`
  tests.
- `builder/src/stage0.rs` — `provision_glibc_static` tests (the `#!/bin/sh`
  fixture compilers).
- `builder/src/gate_defs/325-cargo-test.rs` — the gate; PATH is
  `$rustpath:$ccpath:$PATH`, no `bash`/`/bin/sh` added.
- Reproduce: `td-builder check cargo-test` (sandbox) vs. a host `cargo test`.

## Done

`td-builder check cargo-test` is green in the loop sandbox with all 11 tests
RUNNING (not skipped) — `398 passed; 0 ignored`. The tests get the environment
they need rather than being skipped:

- The `cargo-test` gate declares a `bash` artifact input (a `LockEntry` from
  `tests/td-builder-rust.lock` — the same lock `provision-{rust,cc}` resolve the
  toolchain from). That seed bash 5.2.37 is already a control-plane lock root
  bound read-only in the sandbox; the gate body appends its `bin` to PATH (after
  busybox, so it can't shadow the gate's own `sh`; after a dev host's system bash
  so that still wins). So the 9 `watchdog_*` tests run under a real bash, exactly
  as on a dev host. The
  bash is scoped to THIS gate, NOT the global loop userland, so recipe rungs
  still see no ambient shell (the `check_loop.rs` hermeticity property holds).
- Two `watchdog_*` scripts used `yes`/`seq`, which the loop's busybox userland
  does not ship; rewritten to bash-native equivalents (`printf … {1..N}`,
  `for ((…))`) that emit the identical streams the tests assert on.
- The 2 `provision_glibc_static_*` fixtures wrote `#!/bin/sh` fake compilers;
  their shebang now resolves to a shell that EXISTS (a dev host's `/bin/sh`, or
  busybox `sh` from PATH in the sandbox), so the kernel can exec them.

Verified: authoritative `td-builder check cargo-test` in the real loop sandbox
PASS (39s), builder `398 passed; 0 ignored`, recipes 25+46/0. On a dev host the
same tests run under system bash (`cargo test` 398/398). No production code path
changes (only the gate's declared inputs + PATH prelude, and `#[cfg(test)]` code).

## Collisions

Touches only `#[cfg(test)]` code in `builder/src/build.rs` and
`builder/src/stage0.rs`, plus this issue file. Disjoint from the
`issue-0555-*` branches (which fix the `provision_userland` warm-sources
ordering in `check_loop.rs`, a cold-source-cache red, not `cargo-test`).
