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
- Refactor: a single `Elf` accessor carrying `is64` does every class-dependent
  read (`phdr_table`, `segment_slot`, `vaddr_to_off`); `interp_slot` and
  `rpath_slots` both go through it, unifying #200's inline 32/64 dispatch.
  `vaddr_to_off` maps DT_STRTAB's vaddr → file offset via PT_LOAD.

`builder/src/main.rs`: CLI `elf-rpath FILE` / `elf-set-rpath FILE NEW`, mirroring
`elf-interp` / `elf-set-interp`.

## Scope

- BOTH classes — ELFCLASS32-LE (i686, the bootstrap toolchain `ar`/`ranlib`) AND
  ELFCLASS64-LE (x86-64, rust/userland). #200 made `set_interp` both-class for
  the i686 toolchain; this matches it for the run-path, so the actual i686
  `ar`/`ranlib` are rewritable and their `LD_LIBRARY_PATH` wrappers can be
  dropped. In-place only (same growth boundary as `set_interp`); no other
  class/endianness.

## Verification

- Gate: `cargo-test` (check-engine smoke tier — builder/src changes validate
  there per DESIGN §7.2). 5 new unit tests over synthesized 32- AND 64-bit
  dynamic ELFs.
- Verified-red: broke the byte-write (→ set-shorter red), the fit-check (→
  refuse-too-long red), the read offset (→ all read-using legs red), and the
  ELF32 dynamic-entry stride (→ the i686 round-trip red); each reverted to green.
- Differential (manual, not a gate): vs `readelf -d` on a real x86_64 ELF built
  with `-Wl,-rpath` — read byte-equal incl. multi-entry path, rewrite round-trips
  and the readelf oracle agrees, too-long refused with file unchanged.
