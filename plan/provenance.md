# Provenance: what "off guix" means for td artifacts

This note pins down the provenance model behind the North Star ("remove guix entirely —
no guix *process* AND no guix *bytes*", CLAUDE.md) and how it applies to the things td
ships and runs. It exists because the distinction is subtle and was repeatedly
re-derived; this is the reference.

## Two independent properties

"Off guix" is two separate claims, and an artifact can satisfy one without the other:

1. **No guix process** — nothing on the running/target machine invokes `guix` (no
   `guix build`, no `guix-daemon`, no guix on `PATH`). This is about the *machine*, not
   the bytes.
2. **No guix bytes** — no byte in the artifact originated from guix: not guix-built
   machine code, not a `/gnu/store` string, not a guix-compiled library statically linked
   in. This is about *provenance*, traced through the build.

The daily-suite-on-a-guix-less-VM goal needs **(1) on the VM** unconditionally. **(2)** is
the stronger, "retire guix last" goal; we pursue it per-artifact where feasible.

## What contributes bytes to an artifact (and what does not)

When you build a binary, only some inputs leave bytes in the result:

- **Source code** → compiled in. Provenance = where the source came from.
- **The compiler** (`gcc`/`rustc`) → its code generation *is* the binary's machine code;
  its bundled runtime (`libgcc`, rust `std`) is linked in.
- **The C library** → for a *static* link, `libc.a`/`libm.a` bytes are copied into the
  binary; for a dynamic link, only an interpreter path + `NEEDED` names (the `/gnu/store`
  strings that betray guix provenance).
- **Build-driver tools** (`make`, `sed`, `coreutils`, `bash` running `configure`/recipes)
  → **leave no bytes in the output.** They orchestrate the build; their own provenance is
  irrelevant to the artifact's provenance.

The practical consequence: to make an artifact guix-byte-free you must control its
**source, compiler, and libc** — not the tools that drive the build. You can drive a
guix-byte-free build with guix's own `make`/`sed` and the *output* is still guix-byte-free.

## Two tests

- **Operational (cheap, necessary):** `grep -c /gnu/store <artifact>` is `0`. Catches the
  obvious leak (dynamic interpreters, baked store paths). A guix-built static `bash` fails
  this — it embeds 11 `/gnu/store` strings (measured; CLAUDE.md North Star).
- **Provenance (strong, sufficient):** every byte traces to {upstream source} ∪ {td's own
  from-source toolchain}. This is established by *how it was built*, not by grepping —
  built from a td-fetch-pinned source with the `/td/store` toolchain ⇒ guix-byte-free by
  construction.

## The `/td/store` toolchain is what makes C artifacts guix-byte-free

td's from-source bootstrap (hex0 → mes → tcc → gcc-mesboot → gcc 14.3.0 → glibc 2.41,
binutils 2.44; x86_64 cross) produces a C toolchain at `/td/store` whose own provenance is
a tiny auditable seed — **no guix bytes**. Compiling C with `/td/store` gcc + linking
`/td/store` glibc is therefore guix-byte-free at the source. (`tests/bootstrap-*-store-
native.sh` build it; there is no persistent `/td/store` artifact today — it is rebuilt from
the seed, which is why a guix-byte-free capture must stand one up.)

## Provenance of the captured set (daily-suite harness)

The harness binds three binaries — **dynamically linked against the shared `/td/store`
glibc 2.41** (interp + RUNPATH → `/td/store`, no `/gnu/store`) — into a sandbox that mounts
`/td/store` (`host-sandbox --store-from /td/store --no-daemon`), `/gnu/store` absent. This
is the **same model as `rust-store-native` gate 416**, which already runs `rustc`
dynamically from `/td/store`. (It was briefly a fully-*static* `--no-store` set; switched to
dynamic 2026-06-28 to **reuse the shared `/td/store` glibc rust already needs**, which drops
the static-glibc-2.41 build entirely — the sandbox mounts `/td/store` for td-builder anyway.)

| binary | language | source | compiler | libc (dynamic, from `/td/store`) | guix bytes? |
|--------|----------|--------|----------|------|-------------|
| busybox | C | upstream (td-fetch, sha-pinned) | `/td/store` gcc | `/td/store` glibc 2.41 (shared) | **none** |
| make | C | upstream (td-fetch, sha-pinned) | `/td/store` gcc | `/td/store` glibc 2.41 (shared) | **none** |
| td-builder | Rust | in-tree `builder/` | upstream rustc relinked to `/td/store` (td's `elf.rs`) | `/td/store` glibc 2.41 (shared) | none of guix; rust = upstream |

Notes:

- **busybox / make** reach full provenance-purity: upstream source + td's own toolchain.
- **td-builder** is Rust. The rust toolchain is **upstream** (the released rust binaries —
  *not* guix-compiled rust; any `rustup` toolchain builds it). The intended home is
  **`/td/store`, not `/gnu/store`**: the `rust-store-native` track (gates 410/416, green
  from-seed 2026-06-28) td-fetches the upstream tarball (sha-pinned, no guix) and
  **relinks it to `/td/store`** with td's own ELF interp rewriter (`builder/src/elf.rs`,
  no patchelf), then runs `rustc`/`cargo` from `/td/store` in an own-root with `/gnu/store`
  absent. So td-builder carries **no guix bytes** and need not touch `/gnu/store` at all.
  Two caveats: (a) `tests/td-builder-rust.lock` still points td-builder's *build* at the
  guix-placed rust under `/gnu/store` — switching it to the `/td/store` relinked rust is
  `rust-store-native` **rung 3** (*compiling* with it; *running* it is already proven);
  (b) the rust bytes are an upstream *binary*, not from-source — **from-source rust
  provenance is out of scope for now** (a separate, large bootstrap), but it is upstream,
  not guix.
- The **build-driver** tools that run busybox's/make's `configure` + recipes may be guix's
  (or anything) — they leave no bytes in the output (see above).

## Why guix appears in the capture today (and the retarget)

The current `tools/build-static-*.sh` use guix on the *capture host* for convenience:
`guix build -S <pkg>` for sources, the guix `gcc-toolchain` + `glibc:static` to compile.
That makes the *output* carry guix glibc bytes — fine for "no guix process on the VM",
**not** "no guix bytes". The retarget (in progress) removes guix from the capture:

- **sources** → `td-fetch` upstream tarballs pinned in `seed/sources/*.lock` (the existing
  bootstrap-source mechanism), not `guix build -S`.
- **compiler + libc** → the `/td/store` gcc + the **shared** `/td/store` glibc 2.41
  (dynamic link — no static archives needed), not guix's.

guix then touches the capture only if you *choose* it as a build-driver; nothing it
provides ends up in the shipped binaries.

## Deployment picture

```
 capture host (has the /td/store toolchain)          guix-less VM
 ─────────────────────────────────────────          ─────────────
 td-fetch upstream sources    ─┐                     (ship binaries +
 /td/store gcc + shared glibc  ├─► build set ──────►  the /td/store glibc ──► run daily-suite
 (built from the seed)         ┘   (busybox/make/      closure)               (host-sandbox
 /td/store rust (td-relinked   ┘    td-builder,                                --store-from
   upstream, rust-store-native)     dynamic vs /td/store)                       /td/store
                                                                               --no-daemon)
```

The VM runs the loop with **no guix installed** (property 1). The C half of the set is
**guix-byte-free** (property 2); td-builder is guix-byte-free modulo upstream rust. The set
is dynamic against the shared `/td/store` glibc, so the VM mounts the `/td/store` glibc
closure (the same one rust needs) rather than each binary carrying a static copy.

## Current gaps (tracked by host-sandbox-stage0 inc2)

- No persistent `/td/store` toolchain — must stand one up (or cache it) for the capture; the
  `rust-store-native` machinery (gate 416) already builds the x86_64 `/td/store` gcc + shared
  glibc 2.41 from the seed, so the captured-set build extends that rather than starting fresh.
- The capture builders still target guix; retarget at td-fetch + the `/td/store` gcc +
  **shared** glibc (dynamic link, `--store-from /td/store`) is in flight. (No static glibc
  needed — that requirement is dropped.)
- td-builder's *build* still uses the `/gnu/store` guix rust (`tests/td-builder-rust.lock`);
  the `/td/store` relinked rust already *runs* (rust-store-native gate 416) — switching the
  lock to compile td-builder with it is rust-store-native rung 3.
- Rust is an upstream *binary* (relinked, not from-source); from-source rust is out of scope.
