# Track: td-builder (side-track)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** approved 2026-06-11 (§4.3 gate 1) — the first Guix-component
replacement under the §2.5 discipline. Opens the own-builder era the
offline-isolation closure deferred daemon-side isolation to.
**Scope authority:** DESIGN §7.1.

## Goal

A td-owned builder — a Rust binary — that executes a `.drv` in a
user-namespace sandbox and registers the output, proven behaviorally
equivalent to the pinned `guix-daemon` (prime directive 4: the daemon is the
oracle; never replace without a differential).

## Acceptance (from DESIGN §7.1)

The daemon-vs-td-builder store differential, run as a self-discriminating
rung: the same drvs — a trivial gexp drv, an environment-sensitive divergence
probe, and the system image drv (the `build` rung's qcow2) — built both ways
yield NAR-hash-equal outputs at identical store paths. Verified-red by a
deliberate builder defect the rung catches (e.g. wrong NAR serialization, a
leaky sandbox, a references mis-registration).

**Probe-vs-oracle caveat** (from the rootless track's verified-red A): a
uid_map-reading probe GENUINELY diverges between the root daemon
(`0 0 4294967295`) and any userns builder (`30001 30001 1`) — reusing the
rootless probe verbatim against the root daemon yields a permanent, by-design
red. Either restrict the divergence probe to env details that legitimately
must match the chosen oracle configuration (getuid, /dev set, env scrubbing,
hostname — see open question 4), or use the rootless-configured daemon as the
oracle side (legitimate: the `rootless` rung already proves it store-path
equal to the root daemon, so oracle authority transfers).

## Settled decisions

- **Vehicle: Rust** (human, 2026-06-11). Building the toolchain from source is
  a non-goal right now — the host store may be warmed with substitutes for the
  pinned channel's Rust closure (DESIGN §5). The loop itself stays
  offline/no-substitutes. S1 still verifies the warm toolchain actually
  compiles td-builder *inside* the check.sh sandbox before any rung depends on
  it.
- **Harness reuse.** `tests/rootless.sh`'s mechanics — sqlite `.backup` DB
  snapshot, staged store rbind at `/gnu/store`, validity guards (oracle output
  valid, probe output invalid), uid_map isolation probe, kept-output +
  diffoscope diagnostics — are the rung's skeleton; the rebuild side swaps
  from "pinned daemon, unprivileged" to td-builder.
- **Crate sourcing:** crates come through the pinned channel (hermeticity —
  pinned inputs, offline loop). Free-ness is no longer required: the FSDG
  posture was relaxed to a non-goal (human, 2026-06-11 — DESIGN §5).

## Open questions (decide here, in order)

1. **Interface seam: daemon protocol vs CLI.** DECIDED 2026-06-11
   (claude-fable-a03d13): **staged — CLI first.** Probed
   `nix/libstore/worker-protocol.hh` at the pin: 34 worker ops; a `guix`
   client expects the query surface (path info, references, valid paths …) to
   work, so protocol-first means implementing most of a store before the
   first build differential can run. The S2–S4 differentials drive td-builder
   directly (subcommand CLI); protocol compatibility is re-examined when the
   §6 loop-convergence follow-on graduates.
2. **NAR serialization + hashing.** RESOLVED by S2 (2026-06-11): own
   serializer + hand-rolled SHA-256, proven bit-for-bit equal to the daemon's
   recorded `info.hash`; verified-red ×3.
3. **DB registration.** DECIDED 2026-06-11 (claude-fable-696a4e): **td-native
   registration record** — td-builder writes its own v1 record per output
   (store path, sha256 NAR hash, NAR size, sorted references, deriver), and
   the rung compares those FIELDS against the daemon's DB
   (`query-path-info`). Writing the daemon's sqlite rows is deferred until
   something needs to READ them (Q1's staged-CLI decision means no `guix`
   client consumes td's registrations yet; revisit at the §6
   loop-convergence follow-on). Equality of the recorded fields is the
   differential either way — exactly what Q3 demanded.
4. **Sandbox parity.** DECIDED for S3 scope 2026-06-11 (claude-fable-696a4e),
   from `nix/libstore/build.cc` at the pin: replicate the hash-visible
   contract — env exactly `PATH=/path-not-set`, `HOME=/homeless-shelter`,
   `NIX_STORE=/gnu/store`, `NIX_BUILD_CORES`, the drv's env, then
   `NIX_BUILD_TOP`/`TMPDIR`/`TEMPDIR`/`TMP`/`TEMP`/`PWD` all
   `/tmp/guix-build-<drvname>-0` where `<drvname>` = storePathToName of the
   DRV path, i.e. it KEEPS the `.drv` suffix (`…/guix-build-foo-1.0.drv-0`),
   cwd there; userns mapping `30001 <host-uid> 1` / `30000 <host-gid> 1`
   with `setgroups deny` (defaultGuestUID/GID, initializeUserNamespace).
   td's replication mechanism (not build.cc facts — guest-visible state is
   what must match): fresh tmpfs `/tmp`, staged store rbind at `/gnu/store`
   (the rootless harness's mechanics, per "Settled decisions").
   `NIX_BUILD_CORES` is fixed at "1" (libstore's default at the pin;
   client-overridable daemon-side, not hash-visible for the S3 subjects —
   revisit if a differential subject ever embeds it). Namespaces at S3:
   NEWUSER|NEWNS|NEWNET|NEWIPC|NEWUTS (the immediate-effect set — NEWNET
   makes non-fixed-output builds offline by construction). Deferred to S4
   (the system-image drv will honestly red if they matter): NEWPID + fresh
   /proc, pivot_root full chroot layout (/dev set, /etc), seccomp,
   fixed-output slirp network, store-file canonicalization (mtime=1, perm
   stripping — NOT NAR-hash-visible; only the executable bit is).

## Sub-task ladder (draft — refine as probes land)

- [x] **S1 toolchain probe** — the pinned channel's Rust toolchain (warmed on
  the host) compiles a hello-world td-builder inside the check.sh sandbox,
  offline. Records closure size and compile time (loop-latency budget, §1.3).
  DONE 2026-06-11: implemented by claude-fable-49b6d6 (PR #2, no guix there);
  guix-loop validation + review round by claude-fable-a03d13 (see Working
  state — "S1 guix-loop validation").
- [x] **S2 NAR differential** — td's NAR serializer hashes a store item
  bit-for-bit equal to the daemon's recorded hash; verified-red by a
  perturbation (e.g. ordering or padding defect). DONE 2026-06-11
  (claude-fable-a03d13): `nar-hash` agrees with the daemon's DB across the
  full-coverage fixture + td-builder's own output; verified-red ×3 (ordering,
  padding, executable-flag — see Working state "S2 — NAR differential").
- [ ] **S3 drv parse + trivial build** — parse the ATerm `.drv`, execute the
  trivial probe drv in a userns sandbox, register the output; NAR-hash-equal
  to the oracle, at the same store path.
- [ ] **S4 the rung** — full acceptance differential including the system
  image drv, plumbed into HEAVY_RUNGS (exclusive landing: `Makefile`, maybe
  `check.sh` sandbox packages).
- [ ] **S5 verified-red** — deliberate builder defects (NAR, sandbox,
  references) each red the rung; evidence recorded here.
- [ ] **S6 land** — §7.2 protocol; release the claim in `PLAN.md`.

## Working state

**Claimed:** claude-fable-49b6d6, 2026-06-11. Starting with S1 (toolchain
probe) — the prerequisite every later sub-task depends on. Open question 1
(daemon protocol vs CLI) is NOT yet decided; it does not block S1, which only
needs the crate to compile and run.

### S1 — toolchain probe (implemented by claude-fable-49b6d6; validated below)

What landed in this increment:

- `builder/` — the Rust crate. `src/main.rs` is a hello-world skeleton that
  prints a stable sentinel `td-builder <version> ok`; `Cargo.toml` declares a
  zero-dependency binary crate (offline by construction — nothing to vendor);
  `Cargo.lock` is committed (pinned, deterministic, no resolver network step);
  `.gitignore` keeps `target/` out of the tree.
- `system/td-builder.scm` — the `td-builder` Guix package, built with the
  pinned channel's `cargo-build-system` + rust toolchain. Source is a
  `local-file` of `../builder` that excludes `target/`/`.cargo/`, so a stray
  local `cargo build` cannot perturb the derivation hash. `#:cargo-inputs '()`,
  `#:tests? #f` (the rung supplies the behavioral assertion by RUNNING the
  binary, not an empty unit suite).
- `tests/td-builder-drv.scm` — two-step lower-then-realise driver: prints
  `DRV=…`; the build and honest pass/fail happen in `guix build`.
- `Makefile` — new `td-builder` rung in `HEAVY_RUNGS` (rung count 21→22): lower
  → build offline → `guix build --check` (reproducibility; re-runs the compile)
  → RUN the binary and assert its sentinel → record `guix size` + wall-clock.
- `tests/eval.scm` — loads `(system td-builder)` so the fast `eval` rung catches
  a syntax/binding error in the new module sub-second.

Shared-spine note (DESIGN §7.3 exclusive landing): this touches `Makefile` and
`tests/eval.scm`. Announced here; landing expects others to rebase.

### Verified-red (S1)

The `td-builder` rung has two assertion legs; both were driven red against the
SAME defects the rung is built to catch. Full guix-loop evidence must be
captured on a guix-capable host (the implementing container had no
guix/guix-daemon — only the rung's *crate-level* legs could be exercised there):

- **A — compile leg (syntax defect).** Appended `fn broken( {` to
  `src/main.rs`; `cargo build --release --offline` failed
  (`error: this file contains an unclosed delimiter` / `could not compile`).
  In the rung this is the `guix build "$drv"` step going red. RESTORED → green.
- **B — run leg (wrong sentinel).** Changed the format string `… ok` → `… NOPE`;
  the binary built but printed `td-builder 0.1.0 NOPE`, so the rung's
  `grep -Eq '^td-builder [0-9.]+ ok$'` assertion fails. RESTORED → green
  (`td-builder 0.1.0 ok`).

### S2 — NAR differential (DONE 2026-06-11, claude-fable-a03d13)

Oracle semantics confirmed at the pin (`guix/serialization.scm`
`write-file`/`filter/sort-directory-entries`): directory entries sorted
`string<?` (codepoint order), `.`/`..` removed; strings framed u64-LE length +
UTF-8 bytes + zero-pad to 8; regular files are `executable` iff
`mode & 0o100`; symlink target via `readlink`, written verbatim.

Implementation decisions:
- **Zero-dep stays:** SHA-256 hand-rolled in the crate (`src/sha256.rs`) with
  FIPS vectors as unit tests — `#:tests?` flips to `#t` (the S1 review
  reminder). Hashing here is an integrity computation whose correctness the
  differential itself proves against the daemon; not a security boundary.
- `src/nar.rs` streams the serialization into the hasher (no buffering of
  whole files); `td-builder nar-hash PATH` prints `sha256:<base16>`. Bare
  invocation keeps printing the S1 sentinel (that rung leg is unchanged).
- **Oracle pairs** come from the daemon's own DB via `query-path-info`
  (`tests/td-builder-nar.scm` prints `NAR=<path> <base16>`): (a) a constructed
  fixture (computed-file) covering every node type — regular, empty regular,
  executable, symlink, nested dir, empty dir — plus sort-stress names
  (`B` vs `a` vs `a-b`: codepoint order, catches case-insensitive or
  locale sorts) and a >8-byte/odd-length content for padding; (b) td-builder's
  own output (a real store item). The rung's S2 leg compares td vs daemon for
  every pair.
- Verified-red — driven 2026-06-11, all via `./check.sh td-builder`, each
  exit ≠ 0 (note the layering: the crate's unit tests are a FIRST line that
  reds the BUILD leg; the differential leg catches what they miss):
  - **ordering defect** (`entries.reverse()` after sort): rung red at the
    build leg — the crate's own sort unit test fails inside `guix build`.
  - **padding defect** (pad-to-4 instead of 8): rung red at the build leg —
    the framing unit test fails.
  - **executable-flag defect** (`if false` on the mode test — a defect NO
    unit test covers): rung red at the S2 DIFFERENTIAL leg itself:
    `FAIL: NAR hash mismatch for …-td-nar-fixture` —
    td `sha256:1209ce1a…` vs daemon `sha256:4d21079b…` — proving the
    daemon-recorded-hash comparison discriminates independently of the unit
    suite.

### S1 guix-loop validation (claude-fable-a03d13, 2026-06-11 — takeover of PR #2)

Everything the section above owed, delivered on a guix host:

- **Store warm-in (DESIGN §5):** cargo-build-system pins rust 1.93 (not the
  channel's default `rust` 1.91) — warmed the exact td-builder closure with
  `guix build -L . -e '(@ (system td-builder) td-builder)'` pinned to the
  OFFICIAL substitute servers only (`--substitute-urls="https://bordeaux.guix.gnu.org
  https://ci.guix.gnu.org"` — never the host daemon's default list, which
  includes nonguix; FSDG posture — since relaxed to a non-goal, DESIGN §5).
  Cold-store lowering fails honestly: the
  no-substitutes source-build path dies on an upstream boost-patch hash
  mismatch — the rung's warm-store precondition is real, not cosmetic.
- **GREEN:** `./check.sh td-builder` passes — lower, offline build,
  `guix build --check` bit-for-bit, sentinel run. `eval` green with
  `(system td-builder)` loaded.
- **RED A through the rung** (not just cargo): `fn broken( {` appended →
  `make: *** [Makefile:567: td-builder] Error 1` (build leg). RESTORED.
- **RED B through the rung:** sentinel `ok` → `NOPE` → the defective binary
  compiles AND passes `--check`, and the rung still reds at the run leg
  (`FAIL: the compiled td-builder did not print its sentinel`) — the legs
  discriminate independently. RESTORED.
- **Review round** (sub-agent, full contract review of dd1ea2f): one
  should-fix — the `#:select?` substring match admitted an EMPTY `target/`
  directory into the source nar, so a stray local `cargo build` perturbed the
  drv hash, contrary to the stated guarantee. Fixed to a basename match and
  proven by drv differential: clean tree vs tree-with-`target/` lower to the
  SAME drv (`qlip8v4p…`) under the fix, and to DIFFERENT drvs (`ff7d8071…`)
  under the old predicate — red and green for the fix itself. Nits applied:
  `.gitignore` also excludes `.cargo`; run-leg FAIL message mentions the
  nonzero-exit cause; drv script imports spelled out like its siblings.
- **S2 reminder (review):** the package sets `#:tests? #f` (legitimate at S1 —
  empty suite, the rung runs the binary); when S2 adds real unit tests this
  MUST flip to `#t` or they silently never run.

### Measurement log (§1.3 — from the real loop run, 2026-06-11)

| metric | value | notes |
|--------|-------|-------|
| td-builder closure size | 74.5 MiB | `guix size` in-rung (rust runtime closure) |
| first compile wall-clock | ~1s (crate, warm toolchain) | the `--check` recompile re-runs it every loop |
| `./check.sh td-builder` wall | ~14s | incl. the ~12s serial cheap chain; rung proper ~2s + --check recompile |
| crate-level compile (rustup, not the rung) | ~9.3s | local sanity only; NOT the loop number |

### S3 — drv parse + trivial build (claude-fable-696a4e, takeover 2026-06-11)

Takeover: a03d13's session ended after landing S2 with no open PR; claim
republished per the PR protocol (PR #4). Q3 and Q4(S3 scope) decided above.

S3 sub-ladder (each step: verified-red before trusting green, then commit):

- [x] **S3a parser** — `src/drv.rs`, recursive-descent ATerm
  `Derive([outputs],[inputDrvs],[inputSrcs],system,builder,[args],[env])`;
  `td-builder drv-parse FILE` prints a canonical dump; unit tests cover
  escapes (`\"`, `\\`, `\n`) and a real pinned-channel drv shape.
  DONE 2026-06-11: grammar transcribed from parseDerivation/parseString at
  the pin; fail-closed deviations only (trailing bytes, non-UTF-8 refused).
  Verified-red ORGANICALLY: the first endOfList transcription (error on a
  list's first element instead of no-consume-continue) turned 5 of the new
  unit tests red before the fix — the tests discriminate. Dump validated
  against the real td-builder-0.1.0.drv; `./check.sh td-builder` green
  (tests run inside `guix build`, `--check` reproducible).
- [x] **S3b sandbox build** — `td-builder build DRV CLOSURE SCRATCH`:
  unshare(NEWUSER|NEWNS|NEWNET|NEWIPC|NEWUTS) via raw x86_64 syscalls
  (zero-dep stays — precedent: the hand-rolled SHA-256; the differential
  proves behavior), uid/gid map per Q4, staged closure rbind at /gnu/store,
  tmpfs /tmp with the exact build dir, env per Q4, exec the drv's builder.
  Isolation probe drv (uid_map recorder, the rootless rung's pattern) builds
  td-side ONLY — its output is namespace-dependent by design so it can never
  be a differential subject (the track-file caveat).
- [x] **S3c registration + differential leg** — reference scanning (search
  output bytes for candidate store-path hash parts, the daemon's algorithm),
  v1 registration record per Q3; `tests/td-builder-s3-drvs.scm` prints the
  daemon-built diff-drv oracle facts (path, recorded NAR hash, references);
  the rung's S3 leg asserts: store path string-equal, NAR hash equal to the
  daemon's RECORDED hash, references set equal, probe uid_map a single
  non-zero-first-entry line.

The differential drv is deterministic and carries a runtime reference (its
output embeds an input store path) so the references-scan assert can
discriminate; the probe drv stays separate (see the probe-vs-oracle caveat).

S3 GREEN 2026-06-11: `./check.sh td-builder` — the userns rebuild of the
diff drv registers the daemon's exact facts (path, NAR hash recorded AND
independently re-hashed, size 1048, refs = input + self, deriver) at the
same store path; probe uid_map `30001 <host> 1`. S2's S1/S2 legs untouched.

Verified-red (S3) — each driven 2026-06-11 via `./check.sh td-builder`,
each exiting non-zero at a DISTINCT assert, then restored:

- **A — sandbox uid defect** (`GUEST_UID 30001 -> 0` in sandbox.rs): probe
  leg red — uid_map read `0 1001 1`, FAIL names the expected
  `30001 <host> 1` shape (the assert pins build.cc's defaultGuestUID after
  the review-round strengthening; a merely non-zero wrong uid would also
  red).
- **B — registration defect** (record `nar-size + 1` in main.rs): FAIL
  `NAR size mismatch — registration '1049' vs daemon '1048'`.
- **C — references mis-registration** (outputs dropped from the candidate
  set in main.rs — the defect Q3's differential exists for): FAIL prints
  both sets — daemon `{dep, self}`, td `{dep}` — the self-reference is the
  discriminator.
- (S3a's parser red was organic: the first endOfList transcription turned 5
  unit tests red. S2's NAR reds ×3 cover the serialization leg the
  independent re-hash assert shares.)
