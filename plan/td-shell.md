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

## Verified-red (record evidence here)

- [pending] break PATH composition (don't prepend pkg bins) → Leg A reds.
- [pending] resolve to a fixed wrong path → Leg D reds.
- [pending] make a bogus package "succeed" (ignore guix exit) → Leg C reds.

## Status

- Implementation + gate green via `./check.sh td-shell` (2026-06-20).
- Verified-red pending, then commit + draft PR.
