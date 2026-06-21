# plan/own-builder-daemon.md — stand up td's OWN builder daemon (move-off-Guile §5)

Track: **own-builder-daemon**. Goal: the loop realizes derivations with td, not
guix-daemon. The guts exist (td-builder executes drvs [td-drv-build], td-store-db
owns the SQLite store, rootless builds NAR-equal); this track wires them into the
realize path and, eventually, a daemon the loop uses by default — the home of the
parked **offline-isolation** daemon-network work (rescoped to "the own-builder era").
Handle: claude-fable-2715d4.

## Increment 1 (PR #69 — td-realize): realize without guix-daemon

`td-builder realize DRV STORE-DB SCRATCH` (builder/src/main.rs): parse DRV → resolve
input ROOTS (input-srcs + each input drv's output paths, read from that .drv) →
compute the closure ITSELF via `store_db_read::Db::closure` over STORE-DB's Refs graph
(the `guix gc -R` the daemon did, now td's reader) → build in the userns sandbox →
register (shared `build_and_register`, extracted from `build`). Reading guix's live
`/var/guix/db/db.sqlite` with td's OWN reader is "own, then diverge" (shared store,
td's reader, no daemon process). guix-daemon is no longer in the realize path — only
the differential oracle.

Gate `td-realize` (355), subject = td-build hello drv:
- DURABLE: td computed the closure itself (non-empty); the realized hello runs.
- MIGRATION ORACLE (removable when guix retires): output path/NAR/size/deriver ==
  the daemon's build of the same drv.

Verified-red (closure step is load-bearing): (A) `realize` against a bogus store-db
errors ("not a SQLite 3 database") — it genuinely reads the db, not a no-op; (B)
`build` with an EMPTY closure fails to even spawn the builder ("No such file or
directory") — the userns sandbox RESTRICTS to the staged closure, so computing it
correctly matters. Both confirm the realize path is not vacuous.

## Increment 2 (PR #70 — td-offline): offline-isolation for td's builder

The parked **offline-isolation** work (rescoped 2026-06-11 to "the own-builder era")
resumed. `offline` (185) proves a non-fixed-output build can't reach the network under
GUIX-DAEMON; `td-offline` (360) proves td's OWN builder does the same. `td-builder
realize` runs the existing DRV_SANDBOX probe (tests/offline-drv.scm) in td's
userns+NEWNET sandbox — realize SUCCEEDING ⇒ the build saw only `lo` and egress failed
(DURABLE, the probe asserts it; no guix oracle). Discrimination control: a
userns+netns given a dummy non-lo interface, where the same /proc/net/dev check DOES
see it (lo,dummy0) — so the isolation assertion is load-bearing, not vacuous (the probe
would red the build if td's builder ever leaked an interface). All-durable.

Note: inside the offline loop the OUTER loop-sandbox netns is already loopback-only, so
this gate guards td's builder END-TO-END (it produces lo-only builds) + that the check
discriminates; a fully differential "td's NEWNET strips a parent interface" proof waits
for a network-present daemon harness.

## Increment 3 (PR #71 — td-realize-store): realize a real recipe + own the store record

`realize` is now a COMPLETE daemon op — build AND own the store record — on a
REAL-dependency recipe. builder/src/main.rs: `build_and_register` returns per-output
records; `realize` writes a td store-db (new `write_output_db`, store_db / pure Rust)
registering the built output (the daemon's post-build registration, no daemon).
Subject = gettext-minimal (inputs libunistring/libxml2/ncurses + flags + makeFlag +
two phases — a real dep graph, not just the toolchain). Gate `td-realize-store` (365):
DURABLE behavioral — the realized gettext-tools run (msgfmt + xgettext); DURABLE
structural — store-query reads the output record back from td's db (write → read
round-trip); MIGRATION ORACLE — that record (path|hash|narSize) equals the daemon's
ValidPaths row.

Verified-red / load-bearing: the round-trip reds on an empty/missing db (store-query
finds no record), and the daemon differential is a value comparison that reds on a
wrong hash/size — both new assertions are non-vacuous.

## Increment 4 (PR #72 — td-loop-build): the loop CONSUMES td's build

"A builder the loop uses instead of guix-daemon." The realize gates so far built via td
then ran the daemon's byte-identical /gnu/store copy; `td-loop-build` (370) has the loop
RUN the artifact FROM td's OWN store output (the realize scratch store) — the consumed
binary is td's build, not guix-daemon's. Subject = gettext-minimal (real deps): the loop
runs `msgfmt` from `…/newstore/<out>/bin/msgfmt` (a path under td's scratch store, NOT
/gnu/store; DURABLE) → 0.23.1. MIGRATION ORACLE: td's own-store output is NAR-identical
to the daemon's build of the same drv. guix-daemon builds only the inputs (toolchain,
retired last) + the oracle copy; td builds + serves the recipe the loop consumes.
Gate-only (realize already does the work — this proves the loop USES it). Load-bearing:
the path-under-scratch check distinguishes td's output from /gnu/store; the NAR oracle is
a value comparison.

## Capstone (PR #74 — nano-no-guix): nano builds with NO guix/Guile in its build path

The convergence of this track + retire-resolver, on a human directive ("build nano with
no guix dependencies, 1 PR"). New Rust `td-builder build-recipe RECIPE-JSON LOCK SCRATCH
STORE-DB` (builder/src/main.rs) + a JSON serializer (json.rs `to_json_string`): reads the
recipe JSON (ts-eval/boa produces it Guile-free), resolves EVERY input from the pinned
lock (tests/nano-no-guix.lock — toolchain + deps + source, no specification->package),
assembles the .drv itself (store::assemble_drv, inputs as input-SOURCES — so it diverges
from guix's nano), and realizes it (realize_drv, no guix-daemon). `realize_drv` +
`self_store_path` extracted/added; the `realize` arm now calls `realize_drv`.

Gate `nano-no-guix` (380): PREP (guix realizes the SEED — toolchain+deps+source from the
lock, retired last); BUILD runs with **guix/Guile SCRUBBED FROM PATH** (the structural
proof the path needs neither) → STRUCTURAL; nano runs 8.7.1 from td's own output →
DURABLE; guix's nano runs 8.7.1 at a DISTINCT path → MIGRATION ORACLE (own, then
diverge). Verified: with guix/Guile off PATH the build SUCCEEDS (positive proof); a
guix-shelling build would red there; a wrong/missing lock entry reds resolution.

The bar (human-aligned): no guix/Guile in nano's BUILD PATH; the toolchain + lock are the
guix-built SEED (bootstrapping the toolchain is §5 retired-LAST, a separate effort).

## Increment 5 (self-hermetic build sandbox — reassigned to claude-fable-9e6e71)

Track reassigned from the dormant claude-fable-2715d4 (no PR since the #74 capstone;
record last touched #69) with the maintainer's go-ahead.

`sandbox::build` unshared NEWUSER|NEWNS|NEWNET|NEWIPC|NEWUTS and overlaid
newstore→/gnu/store + a tmpfs /tmp, but did NOT pivot_root — so the build inherited
the INVOKING root (/etc, /home, /usr, /var/guix … all visible) and was hermetic ONLY
because the outer host-sandbox had already hidden the host. That hidden precondition
("build is only hermetic inside the loop container") bites the own-builder-daemon
direction, where build runs without the outer wrapper.

Now build pivot_roots into a MINIMAL fresh-tmpfs root: staged /gnu/store (rbind, so the
per-item input binds + the output dir ride through), a writable /tmp with the build dir,
/dev + /proc rbind'd from the invoking namespace, and a minimal /etc (passwd+group for
getpwuid/getgrgid) — nothing else of the host fs.

- /dev is rbind'd WHOLE, not rebuilt node-by-node: re-binding a device onto a fresh
  unprivileged-userns tmpfs strips device access (the first attempt red'd td-realize with
  `/dev/null: Permission denied` in hello's configure). The rbind preserves host_shell's
  already-minimal /dev in the loop. A standalone daemon (no outer host_shell) will want
  its own minimal-/dev builder — noted in Next.
- NEWPID + a fresh /proc reflecting the build's own pid namespace stay parity work; /proc
  is rbind'd for now (filesystem hermeticity, the finding's concern, doesn't need it).

Gate `build-hermetic` (356): a probe drv whose guile builder ERRORS if /var/guix (the
daemon db/socket/gc-roots — bound rw into the loop container, never wanted in a build) is
reachable; `td-builder realize` succeeds ONLY because build pivots it away. DURABLE /
behavioral, no guix oracle leg (it asserts the daemon state is ABSENT from the build).

Verified-red (recorded): with `sandbox::build` reverted to the no-pivot main version,
`./check.sh build-hermetic` FAILS — `LEAK: /var/guix reachable inside td's build sandbox`
→ the probe builder exits 1 → realize errors → gate red (exit 2). So the gate is
non-vacuous and the pivot is load-bearing. Restored → green. td-realize still PASSES
byte-identically to the daemon (55-path closure), proving the minimal root is sufficient
and the minimal /etc does not perturb output.

## Increment 6 (build pid-namespace parity — folded into the build-hermetic gate)

`sandbox::build` unshared NEWUSER|NEWNS|NEWNET|NEWIPC|NEWUTS but NOT NEWPID, and
rbind'd /proc from the invoking namespace — so a build saw the loop's whole process
tree (the guix daemon, other concurrent builds, their /proc/<pid>/environ) and could
signal it. Now build reaches full host_shell / `guix shell -C` parity: NEWPID rides
in the SAME unshare as NEWUSER (so the PID ns is owned by the new user ns), then a
fork lands the builder at PID 1 of its own pid namespace and a FRESH procfs is
mounted reflecting THAT namespace. Two-level PDEATHSIG (spawned-child + PID 1)
reaps an orphaned build if the realize process dies, and the kernel tears down the
whole ns when PID 1 exits. Mirrors the proven host_shell mechanism.

Test: rather than a new gate (which would add a `guix build -e (system td-builder)`
packager site — wait, 356 builds via stage0, so a NEW gate copying that pattern grew
the surface; folding avoids it entirely) the pid-ns assertion is FOLDED into the
existing `build-hermetic` probe/gate (356), which already builds td-builder via the
stage0 bootstrap (rebuilt from builder/src on a fingerprint change, so my change is
exercised). The probe now asserts BOTH (a) /var/guix absent (fs hermeticity, needs
pivot_root — increment 5) and (b) the launching `td-builder' process is INVISIBLE in
/proc (pid-ns isolation, needs NEWPID — increment 6). Adding an assertion strengthens
the gate (free); no new gate, no surface growth.

Discriminator choice: `(getpid)==1` is NOT usable — under guix-daemon's own build
chroot the builder runs as PID ~18 (the `separate-from-pid1` phase), so the probe's
guix-daemon materialization build would fail it. A /proc pid-COUNT threshold is also
unreliable: the loop runs inside host_shell's own pid ns, so without NEWPID a
standalone build sees only ~6 loop processes — too close to any threshold. The
robust signal is the LAUNCHER's visibility: the `td-builder' realize process is in
/proc iff the build shares the loop's pid namespace; absent iff the build has its
own. Holds under guix-daemon (no td-builder there) and td-with-NEWPID; fails under
td-without-NEWPID.

DURABLE / behavioral, no guix oracle leg (it asserts the loop's process tree is
ABSENT from the build).

Verified-red (recorded, 2026-06-20):
- Minimal perturbation (drop only `| sys::CLONE_NEWPID`, keep the fresh-procfs
  mount): `./check.sh build-hermetic` FAILS at the procfs mount itself —
  `spawning builder …/guile: Operation not permitted` — because a fresh procfs
  cannot be mounted for a pid namespace whose owning user namespace isn't the
  build's. Proves NEWPID and the fresh /proc are coupled and load-bearing, but it
  errors BEFORE the probe runs.
- Clean perturbation (revert sandbox.rs to the pre-increment-6 state: no NEWPID +
  /proc rbind'd, via `git checkout origin/main -- builder/src/sandbox.rs`): the
  build RUNS and the probe itself reds — `LEAK: the launching td-builder is visible
  in /proc … comms= ("make" "td-builder" "guile" "bash")`. So the new assertion is
  non-vacuous and the pid-ns isolation is load-bearing. Note only 4 loop processes
  were visible — a /proc pid-count threshold would have been unreliable; the
  launcher-name discriminator is correct. Restored → green (both legs pass).

## Increment 7 (the persistent build daemon — the track's end-goal capability)

`td-builder daemon SOCKET STORE-DB SCRATCH` (new `build_daemon::serve`) is a
long-running process that realizes derivations served over a Unix socket — the
loop's builder instead of guix-daemon. It wraps the EXACT `realize_drv` path (same
userns sandbox + NEWPID from increment 6, daemon-free); the daemon only adds
persistence + a line protocol (`<drv>\n` → `OK <canonical> <host-out>\n` | `ERR
…\n`, `SHUTDOWN\n` for a clean stop). `td-builder daemon-request SOCKET DRV`
(`build_daemon::request`) is the in-process client, so a gate needs no nc/socat
(absent from the loop sandbox — as are awk and seq; the gate is pure shell + grep
+ sed + a counter loop). Serial accept loop, fresh per-request scratch dir.

Gate `build-daemon` (358): one long-running daemon serves TWO DISTINCT realize
requests (probes a + b) over a single socket — persistence, the property a
one-shot `realize` lacks — each producing td's OWN output (under the scratch store,
NOT /gnu/store) with the expected marker, the realize daemon-free. All-DURABLE
behavioral, no guix oracle leg.

Why minimal-/dev was deferred (it was the listed "next"): the minimal-/dev builder
only runs when /dev is a real devtmpfs (a standalone daemon, no outer host_shell);
every gate runs INSIDE host_shell, which already presents a minimal tmpfs /dev, so
that path never executes in the loop and there is no red to see — untestable until
a standalone/daemon harness exists. The maintainer chose the persistent daemon as
the next increment instead; it becomes that standalone context.

Verified-red (recorded, 2026-06-21): add `break;` after the first request's
response in `build_daemon::serve` (serve exactly one request). `./check.sh
build-daemon`: request A succeeds (realize, OUT=probe-a, registered), request B
reds — `daemon-request: connect …/sock: Connection refused (os error 111)` →
`FAIL: request B (PERSISTENCE …)` → gate red. So the "one daemon serves multiple
requests" assertion is non-vacuous and persistence is load-bearing. Restored → green.

## Next

- A minimal-/dev builder for the standalone (no outer host_shell) daemon case
  (now has a context — the daemon — to be tested in).
- The loop CONSUMING the daemon by default (a daemon the realize gates talk to,
  instead of each spawning a one-shot realize) — the consumer swap.
- A network-present daemon harness → fully differential offline-isolation.
- Toolchain retired LAST (§5).
