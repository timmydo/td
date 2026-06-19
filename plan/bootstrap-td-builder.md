# bootstrap-td-builder — build the seed with td, no guix in CREATION (move-off-Guile §5)

**The real goal (human steer, 2026-06-18):** "get rid of guile and guix" means removing
the `guix`/`guile` INVOCATIONS from the loop, not relocating them (see
[[td-move-off-guile-remove-invocations]]). A full `./check.sh` fires ~178 guix calls;
the seed tools are the root: td-builder, td-ts-eval, node, td-typescript are all
`guix build`-produced from Guile package definitions.

The human chose the foundational path: **build the seed with td** — a stage0 td-builder
that bootstraps the self-hosting chain, so guix never has to PRODUCE the first builder.

## The bootstrap circularity

`rust-build` "self-hosts" td-builder via `build-recipe` — but only because a guix-built
td-builder already exists to RUN build-recipe. The first td-builder comes from
`guix build -e '(@ (system td-builder) td-builder)'`. To remove that, we need a stage0
td-builder produced WITHOUT guix.

Enablers found:
- td-builder is **std-only** (builder/Cargo.lock = 1 package), so a guix-free compile
  needs only `rustc`/`cargo` + a gcc linker — no crate vendoring.
- those toolchain paths are already pinned in `tests/td-builder-rust.lock` (the
  guix-built toolchain seed, retired LAST §5).

## Brick 1 (this track): stage0 + proof gate

- `tools/bootstrap-td-builder.sh OUTDIR` — `cargo build --release --offline --frozen`
  under `env -i` with ONLY pinned store tools on PATH (cargo/rust/gcc/coreutils/bash,
  read from the lock as strings; guard: no guix/guile on PATH). No guix, no Guile, no
  daemon. Validated: builds in ~8s, runs (`td-builder 0.1.0 ok`), bit-reproducible.
- `mk/gates/170-bootstrap.mk` — [STRUCTURAL] guix/Guile-free build; [DURABLE behavioral]
  runs sentinel + nar-hash; [DURABLE self-discrimination] perturbed input → different
  hash; [DURABLE intrinsic-reproducibility] two bootstraps bit-identical; [MIGRATION
  ORACLE, removable] behaviorally == guix-built td-builder, distinct binary (stage0
  sha256 b564773…, guix-tb 5fdc614… — same nar-hash e97a8c… on a probe).

## The obstacle for the NEXT brick

`build-recipe` references td-builder (the builder PROCESS) by **store path**
(`current_exe()` → strip `/bin/td-builder`; the assembled drv's `builder` field), and
the sandbox bind-mounts the builder from its real `/gnu/store` path. So making the
loop's builds USE stage0 needs stage0 IN a store the sandbox can bind from — i.e.,
daemon-free placement of the builder (or teaching the sandbox to bind a td-owned
builder path). Two routes for brick 2:
  (a) td-owned store placement of the builder (pure; the "td owns the store" direction),
  (b) daemon places the cargo-built stage0 (creation guix-free, placement via the
      retire-last daemon — transitional).
Then brick 3: demote the guix-built td-builder to oracle-only across the gates.

## Hermeticity — is `env -i` + `$PATH` enough? (No, and here's why it holds)

`env -i` + a pinned PATH is NOT the guarantee on its own: it scrubs the environment and
forces the PINNED rustc/cargo (no ambient toolchain; no host RUSTFLAGS/CC/CARGO_HOME;
HOME/CARGO_HOME → tmp, so no `~/.cargo/config`), but it does not isolate the filesystem
or the network. The isolation comes from WHERE the gate runs:

- **Inside td's loop sandbox** (`td-builder host-sandbox`): a fresh-tmpfs root exposing
  ONLY `/gnu/store` (ro) + the worktree (+ synthetic /dev, fresh /proc) — there is NO
  host `/usr`/`/lib`/`/bin`/`/etc` to leak from — and its OWN loopback-only netns, so
  the build is offline by construction (`--offline --frozen` is belt-and-suspenders).
- **[DURABLE hygiene] leg** proves the PRODUCT is clean: the stage0 binary's ELF
  interpreter and RUNPATH are entirely under `/gnu/store` (store glibc/gcc-lib), never
  host `/usr`/`/lib`. Verified empirically: interp = `…glibc-2.41/…/ld-linux`, RUNPATH =
  store gcc-lib + glibc only. Verified-red: flipping the expected prefix reds the gate
  (exit 2) — the check would catch a host-libc leak.

**Known residual gap (honest):** the loop sandbox exposes the WHOLE store, not just the
declared toolchain closure, so closure-COMPLETENESS is not STRUCTURALLY enforced the way
td's per-build sandbox (build-recipe stages only the closure) enforces it. Bounded today
by: offline netns + pinned PATH + bit-reproducibility + the hygiene leg + behavioral
equivalence to the hermetically-guix-built builder. The structural fix is to run the
stage0 compile inside a STAGED-closure sandbox (only the declared inputs visible, a
missing dep ⇒ build fails) — the convergence point with brick 2 (stage0 produced by a
sandboxed td build).

## Status / evidence

- Manual validation: guix-free compile ✓ (8s), runs ✓, bit-reproducible across 2 builds
  ✓ (sha256 b564773…), behaviorally == guix-tb (same probe nar-hash e97a8c…) ✓, distinct
  binary ✓ (guix-tb 5fdc614…). Pure: `env -i` + only pinned tools (no host /usr/bin).
- `./check.sh bootstrap`: GREEN — all five legs pass in the loop sandbox.
- Verified-red: a non-reproducible bootstrap (append `$RANDOM` to the output) reds the
  intrinsic-reproducibility leg ("two stage0 builds differ … NOT reproducible", exit 2);
  reverted.
- Full `./check.sh`: GREEN (see commit).
