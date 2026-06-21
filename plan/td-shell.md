# td-shell — `td-builder shell`, td's own `guix shell`

Handle: claude-fable-db65ca · branch: worktree-td-shell

## Goal

`td shell PKG... -- CMD...` brings the named packages into CMD's environment and
runs CMD — td's own `guix shell` (the default, non-`-C` form). v1 is the minimal
useful surface; the acceptance test is `td shell hello -- hello`.

## Design ("own, then diverge")

- **Resolution (guix oracle, retired last §5):** each PKG is resolved to its store
  output(s) by `guix build PKG` — the same name→derivation→output package layer
  guix shell uses. This is the ONLY guix dependency in the command (`run_shell` in
  builder/src/main.rs); swapping that one block for a td-native package db makes
  `shell` guix-free.
- **Environment composition (td's own, DURABLE):** td prepends each resolved
  output's `bin`/`sbin` to PATH itself (package bins FIRST so the package wins —
  load-bearing), then the inherited PATH (guix shell's non-pure default), and runs
  CMD directly. No guix process is in the exec path.
- `shell PKG...` with no `--` drops into an interactive `$SHELL` (fallback /bin/sh).
- Not in v1 (later): `-C` container form (host-sandbox already exists for the loop),
  manifests (`-m`), `--pure`, search paths beyond PATH, profile union.

## Gate (mk/gates/370-td-shell.mk → tests/td-shell.sh)

The td-builder under test is the guix-free STAGE0 (tests/stage0-builder.sh,
cargo-compiled from the current builder/ source) → **no new `guix build -e
'(@ (system td-builder) ...)'` packager site**, so guix-surface stays at 26.

- A [DURABLE behavioral] `td shell hello -- hello` prints exactly "Hello, world!".
- B [DURABLE structural] the hello on the composed PATH is a real `/gnu/store`
  binary that itself runs and greets — the package injected a runnable hello.
- C [DURABLE discriminate] without the package, `td shell -- hello` FAILS in the
  same env where with it succeeds (load-bearing); a bogus package name fails at
  resolution.
- D [REMOVABLE oracle] td resolves hello to `$(guix build hello)/bin/hello`, and
  `td shell` byte-equals `guix shell` — the guix differential, DELETED (not
  rewritten) when guix retires; A–C are what remain.

## Verified-red (2026-06-20)

- VR1 — Leg A behavioral: dropped the package bins from the composed PATH
  (`path = String::new()` instead of `prefix_dirs.join(":")`) → `td shell hello --
  hello` exits nonzero ("td shell hello -- hello exited nonzero"). Reverted.
- VR2 — Leg C discrimination: swallowed `guix build`'s non-zero exit (`continue`
  instead of returning Err) → `td shell no-such-package-xyzzy` succeeds, gate reds
  ("resolution is a no-op"). Reverted.
- VR3 — Leg D oracle: perturbed the expected path to `$oracle/bin/WRONG` → the
  equality assertion fires and reds (non-vacuous). Reverted.

Note: rapid back-to-back stage0 recompiles into the dedicated
`.td-build-cache/td-shell` base can leave a half-written placement; `rm -rf` it
and re-run if stage0-builder reports "could not place a stage0 td-builder".

## Status

- Implementation + gate green via `./check.sh td-shell` and direct
  `sh tests/td-shell.sh` (2026-06-20). Verified-red done (VR1–VR3 above).
- Next: landing readiness (`tools/affected-checks.sh --committed-only --run`),
  draft PR, flip the record to `done` on land.
