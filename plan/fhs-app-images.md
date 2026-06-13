# Track: fhs-app-images (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** graduated from the DESIGN §6 parking lot to the §7.1 roadmap; re-scoped
by M9 (FHS belongs to *app* images — the base stays a minimal store-based container
host).
**Scope authority:** DESIGN §7.1.

## Goal

Produce Guix-built OCI **app** images presenting a traditional FHS layout
(`/usr/bin`, `/lib`, …) instead of the `/gnu/store` symlink farm, so foreign
software/expectations work inside app containers. The base image is explicitly NOT
flattened.

## Acceptance

An FHS app image builds reproducibly (`guix build --check`), runs on the booted base
via the existing container-host rung (entrypoint honored), and a behavioral
assertion proves the FHS property (e.g. the app binary really resolves at
`/usr/bin/...` inside the container). Verified-red required.

## Constraints

- Guix's native store-based image remains the reproducibility oracle (M5); FHS
  flattening layers on top — diff against it where applicable (§2.5).
- Offline loop, as always. (FSDG relaxed to a non-goal 2026-06-11 — DESIGN §5.)

## Working state

**Agent:** claude-fable-aed5c2 (claimed 2026-06-13). Draft PR #17.

### Mechanism (verified against pinned guix `(guix scripts pack)`, commit 520785e)

`docker-image` accepts `#:symlinks` — a list of `(SOURCE -> TARGET)` tuples
(`->` literal), SOURCE absolute in-image, TARGET relative to the profile. The
docker path's `symlink->directives` (pack.scm ~561) materializes SOURCE's parent
dir + a symlink SOURCE → `<profile-store-path>/TARGET` INTO the image layer. So
`#:symlinks '(("/usr/bin/hello" -> "bin/hello"))` yields `/usr/bin/hello` →
`<profile>/bin/hello` inside the unpacked rootfs; the profile closure is already
in the layer, so it resolves. This is the FHS-presentation vehicle (same one
`guix pack -S /usr/bin/env=bin/env` uses) — store closure + FHS entry points,
which is what lets foreign software that hardcodes `/usr/bin/...` and
`/lib64/ld-linux-x86-64.so.2` work.

### Reproducibility dependency on #16 (container-tar-repro)

The FHS image is built by the same `(docker-image)` whose OUTER tar is
readdir-ordered (non-reproducible cross-filesystem) — the exact bug PR #16 fixes
with its `deterministic-docker-image` re-pack wrapper in `tests/container.scm`.
This track MUST build its FHS image through that wrapper. Plan: land on top of
#16 — rebase onto origin/main after #16 merges, then implement. (#16 is approved
+ auto-merge armed.)

### Approach: extend the `container` rung, do NOT add a new heavy rung

The `container` rung already boots the base ONCE and runs N app images on it
(positive + 2 negatives + cgroup). The FHS app is "another OCI app image run on
the booted base" — it belongs in that same boot. Adding a 4th scenario reuses the
VM (no ~140s second boot) and fits the rung's role. Touches: `tests/container.scm`
(new FHS image+bundle + scenario) and the `container:` recipe (add the FHS
artifacts to the `--check` set). The Makefile edit is small but is still a shared-
spine touch — land carefully, expect rebases.

### Sub-task ladder (each a green commit; verified-red recorded here)

- **S1 — FHS image builds + reproducible.** Add `td-app-fhs-image` (hello, with
  `#:symlinks '(("/usr/bin/hello" -> "bin/hello"))`, packed via #16's
  `deterministic-docker-image`) + `td-app-fhs-bundle`. Wire into the `container:`
  recipe's `--check` set. Test: `guix build --check` reproducible (prime directive
  1) + an artifact-content assertion that `/usr/bin/hello` is present in the image
  layer (the manifest-check pattern). Verified-red: drop the symlink → /usr/bin/hello
  absent → artifact assertion reds.
- **S2 — FHS path runs on the booted base (the §7.1 acceptance floor).** The FHS
  image's OWN declared entrypoint is the absolute `/usr/bin/hello`; bundle reads it
  into args.json; crun execs it on the booted base → prints "Hello, world!", exit
  0. Self-discriminating: running the SAME `/usr/bin/hello` arg against the PLAIN
  store-layout rootfs (`td-app-image`, no /usr/bin) FAILS. Same arg, different
  rootfs, different outcome ⇒ the FHS image specifically provides /usr/bin/hello.
  Verified-red: point the FHS symlink elsewhere / run plain rootfs → positive reds.
- **S3 — foreign-interpreter binary works ONLY under FHS (the deeper goal).**
  Strengthening toward "foreign software/expectations work". A binary whose ELF
  interpreter is the FHS path `/lib64/ld-linux-x86-64.so.2` runs in the FHS image
  (loader symlinked from glibc) and FAILS in the plain image (no /lib64/ld-linux).
  Proves FHS makes foreign-expecting software runnable, not just that we added one
  symlink. Heavier (needs a gcc-toolchain-built foreign binary derivation) — pursue
  if it lands cleanly; S2 is the defensible landing point if S3 proves too costly.

### Verified-red evidence

(record per sub-task as it goes green)
