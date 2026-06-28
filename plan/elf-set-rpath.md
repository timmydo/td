# elf-set-rpath — td's own in-place DT_RPATH/DT_RUNPATH rewriter

Task 5 of the post-modern-toolchain follow-ups: "elf.rs DT_RPATH rewrite
(set_rpath) — make toolchain binaries self-sufficient; drops the ar/ranlib
LD_LIBRARY_PATH wrappers (cleanup)". Symmetric companion to the PT_INTERP
rewriter landed for [[rust-store-native]] (#196).

## What landed

`builder/src/elf.rs`:
- `read_rpath(path)` — returns the run-path: `DT_RUNPATH` (loader-preferred),
  else legacy `DT_RPATH`; `None` for a static binary or one with no run-path.
- `set_rpath(path, new)` — rewrites every `DT_RPATH`/`DT_RUNPATH` entry's
  `.dynstr` string IN PLACE. The new string (plus NUL) must fit the existing
  slot, NUL-padded; validated across all slots BEFORE any write so a too-long
  path is refused atomically (file left intact). Errors loudly when there is no
  run-path to rewrite (adding one needs growing `.dynamic`/`.dynstr` — the
  out-of-scope add-a-segment dance, same boundary as `set_interp`).
- Refactor: shared `phdr_table` + `segment_slot` helpers (interp_slot now reuses
  them); `vaddr_to_off` maps DT_STRTAB's vaddr → file offset via PT_LOAD; new
  ELF64 dynamic-section constants (DT_STRTAB/RPATH/RUNPATH, Elf64_Dyn layout).

`builder/src/main.rs`: CLI `elf-rpath FILE` / `elf-set-rpath FILE NEW`, mirroring
`elf-interp` / `elf-set-interp`.

## Scope / honesty

- ELFCLASS64-LE only — the SAME boundary `set_interp` already declares (the only
  class td produces/consumes for the x86_64 path). Applies to rust (x86_64) and
  the in-flight x86_64 toolchain ([[td-x86-64-toolchain]]).
- The CURRENT i686 `/td/store` toolchain `ar`/`ranlib` are 32-bit ELF, so this
  64-bit rewriter does NOT itself remove their wrappers — this PR lands the
  reusable primitive; the wrapper removal follows on the x86_64 toolchain.

## Verification

- Gate: `cargo-test` (check-engine smoke tier — builder/src changes validate
  there per DESIGN §7.2). 4 new unit tests over synthesized dynamic ELFs.
- Verified-red: broke the byte-write (→ set-shorter red), the fit-check (→
  refuse-too-long red), and the read offset (→ all read-using legs red); each
  reverted to green.
- Differential (manual, not a gate): vs `readelf -d` on a real x86_64 ELF built
  with `-Wl,-rpath` — read byte-equal incl. multi-entry path, rewrite round-trips
  and the readelf oracle agrees, too-long refused with file unchanged.
