# x86_64-toolchain-as-path — fix the cross gcc's assembler/linker discovery on the FETCH path

Handle: claude-opus-aedee9

## The bug (gate 414, landed #223)

The x86_64 toolchain FETCH short-circuit (`tests/bootstrap-x86_64-toolchain-store-native.sh` +
`tests/x86_64-subst-lib.sh`) lets the loop SKIP the ~98-min from-seed cross build by fetching the
lock-keyed closure `{binutils-2.44, gcc-14.3.0, glibc-2.41}` and running the UNIFIED
`x86_64_verify_closure` against it. On a real cold fetch the cross gcc could not compile:

```
verify_closure: closure cross gcc could not compile an x86_64 C program
FAIL: the x86_64 closure toolchain did not compile+run an x86_64 program → 42
```

Root cause (reproduced on the host, no rebuild):

- The cross gcc 14.3.0 is configured `--with-as="$xbu/bin/$XTARGET-as"` / `--with-ld=...`
  (`tests/x86_64-cross-fns.sh` lines 103, 193), where `$xbu` is the binutils BUILD mktemp dir.
  After a cold fetch that `/tmp/tmp.XXXX/...` path is gone, so gcc falls back to searching `PATH`
  for a *plain* `as`.
- The closure binutils (`$XBU/bin`) ships only the target-PREFIXED `x86_64-pc-linux-gnu-{as,ld}` —
  **no plain `as`/`ld`**. So with the gate's `PATH=$XBU/bin`, gcc finds nothing → fails.

Why the gate was green on main: the from-seed (MISS/build) path's `verify_closure` ran in the SAME
run, where the build's binutils scratch dir still existed, so gcc's baked `--with-as` resolved. The
**fetch (HIT) path had never actually run** — the substitute store had never been pre-populated — so
the "unified" verify only ever certified the build branch. Manually populating `~/.td/subst` from the
#223 dev-worktree export surfaced it.

## The fix

`x86_64_bundle_tooldir GCCTREE` (in `tests/x86_64-subst-lib.sh`), called from the gate driver
BEFORE `x86_64_build_closure` interns the gcc: install plain `as`/`ld` into the cross gcc's own
tooldir `$GCCTREE/$XTARGET/bin` — the dir gcc searches for the assembler/linker relative to argv[0]
(`gcc -print-prog-name=as`). **RELATIVE** symlinks to the sibling binutils lock path
(`../../../<binutils-lock-base>/bin/$XTARGET-{as,ld}`) so they resolve in every context the closure
is unpacked as siblings: the host-side verify compile (`$cstore`), the store-ns own-root (`/td/store`
bind), and a fetched consumer. td's nar preserves symlinks, so the published nar carries them.

Plus a DURABLE `[self-contained]` structural guard in `x86_64_verify_closure`: assert the cross gcc
carries `as`/`ld` in its tooldir; reds on BOTH paths if `x86_64_bundle_tooldir` is dropped (so the
build path can no longer mask a fetch-only break behind its lingering scratch dir). The compile is the
behavioral proof they WORK; the guard pins WHY.

## Verified-red (host, no rebuild — exact `verify_closure` compile, `PATH=$XBU/bin` only)

- WITHOUT the tooldir bundle: `EXIT=1` (reproduces the gate FAIL).
- WITH `$XGCC2/$XTARGET/bin/{as,ld}` → `../../../<bu>/bin/$XTARGET-{as,ld}`: `EXIT=0`, ELF64,
  interp `/td/store/<glibc-lock>/lib/ld-linux-x86-64.so.2`. `gcc -print-prog-name=as` → absolute
  tooldir path.
- NAR round-trip preserves symlinks (the restored gcc tree already carries working relative
  symlinks, e.g. `libstdc++.so → libstdc++.so.6.0.33`).

## Loop / waiver

Human waived the full from-seed re-validation for this bugfix (the skip path is already broken on
main, so landing the fix is low-risk). `tools/affected-checks.sh`: `bash -n` ✓, full `./check.sh`
waived; selected `./check.sh bootstrap-x86_64-toolchain-store-native` (the ~98-min from-seed gate)
NOT run per the waiver. The fix is validated by the host verified-red above + a fetch-path gate run
against a re-bundled closure (see notes).
