# Agent development and landing workflow

This is the mutation-only companion to `AGENTS.md`. Read it completely before
changing the tree. The root file holds the product and coding invariants every
task needs; this file holds the operational detail needed only to create a
landing.

# Branches and rolling workstreams: land on green

Multiple agents work this repository concurrently, so work in your own git
worktree. There is no GitHub PR, Issues, or Actions UI and no branch
protection. GitHub and the sr.ht mirror are backup remotes, but shared
`origin` is the handoff: the integrator reviews and lands from a separate
clone, so a pushed branch is the PR and there is nothing else to notify.

## Worktree location

Create agent-owned development worktrees under `~/src/td-worktrees/<branch>`.
Keep `~/src/td` as the primary checkout; do not create development worktrees
as `~/src/td-*` siblings or inside the primary checkout. This is the path
to pass to `git worktree add`, including when working through Codex CLI.
For example, from the primary checkout after fetching `origin`:

```text
git worktree add -b example-change ~/src/td-worktrees/example-change origin/main
```

Tool-managed integration and review checkouts keep their documented
locations. Disposable agent scratch stays inside the owning worktree as
specified in `AGENTS.md`.

Existing worktrees need not move during active work. To consolidate an idle
checkout, first check for running agents, builds, services and other process
use, uncommitted or untracked files, Git operations and worktree locks.
Leave active or uncertain checkouts in place. Use `git worktree move` without
force for a clean, inactive checkout; it preserves the branch and private
Git history while updating Git's worktree registration. Resume sessions
from the new path; external tools may still remember the old path. Moving
a checkout does not reclaim its build-cache space. Use the landed-worktree
cleanup below when removal is appropriate, and preserve retained review
traces during any cleanup.

## Choose the branch lifetime

Name every branch for the work it carries. Use a normal descriptive branch for
a one-off change. Reserve the `-rolling` suffix for a long-lived stacked
workstream that continues after the integrator lands some or all of its current
commits, for example `ui-rolling`, `td-sh-rolling`, or `td-txt-rolling`. Do not
add an allocation number. The suffix tells the integrator's sweep to preserve
the branch and worktree across their own landings.

## One commit is one increment

Each commit stands alone, stays green, and carries its own review record. The
integrator may land the first commits of a branch and stop, so never depend on
a later commit to repair an earlier one. Do not combine several increments to
save review work: the commit is the review and checkpoint unit.

## Host kernel

The builder requires Linux 5.12 or newer with `mount_setattr` available to
its namespace sandbox. The source-bootstrap ladder also executes i386 Mes
and GNU intermediates, so x86-64 build hosts and installed td kernels need
`CONFIG_IA32_EMULATION=y` with compatibility execution enabled at boot
and `CONFIG_COMPAT_32BIT_TIME=y` for the early glibc futex and clock
syscalls. Enabling ELF execution alone is insufficient; x32 is not required.
Read-only binds add restrictions without clearing locked source flags; td's writable `/var` is mounted nosuid,nodev. A blocked
operation is a sandbox setup failure, with no writable-bind fallback. The
value-pinned syscall boundary is recorded in `UNSAFE.md`.

## Ready

`td-builder ready` is the pre-push gate. It runs
`affected-checks --committed-only --run` and verifies that every commit not on
the base carries the review record described below.

```text
td-builder ready
td-builder ready --record-only
```

`--record-only` scans records but does not run builds; its successful output
says `checks NOT run` and is not permission to push. The same agent that
finishes an increment carries it through the full ready gate.

`ready` resolves the committed check selection locally. When it selects no
preflights or check targets, it finishes locally with the normal review-record
and clean-tree validation, without joining the shared check host's queue.
Nonempty selections still run through that host. This uses the affected-path
mapping, not a blanket Markdown exemption: documentation that selects a check
still runs it. `ready` remains required for documentation updates.

The repository-root entry scripts are `./start`, `./build-qcow` and
`./build-iso`, which build the system through the Cargo runner, `./test-iso`,
which boots a retained ISO with a private QEMU disk, `./install-fonts`,
which installs the pinned outline face for td programs run on this host
(`td-builder install-fonts`), and `./install-apps`, which builds the desktop
programs in release mode and installs them for this host's user (`td-builder
install-apps`, APPLICATIONS.md §X.7). `tests/start.sh` and `tests/host-run.sh`
prove their bootstrap.

`ready` runs the selected checks once over the branch tip. It does not prove
that an intermediate commit is green, so keep every commit independently
passing as it is made.

A gate that already passed on the same content is not run again: `ready`
gives its `td-builder check` `--resume`, whose journal
(`.td-build-cache/gate-verdicts/`) keys each pass on HEAD's tree, the
dirty diff and untracked files, the recipe-checks scope, and the bytes of
the td-builder whose gate bodies ran it. HEAD's tree, not HEAD: no gate
reads history or a message, so a run stopped to amend a commit message
and started again runs only the gates it had not passed. A gate that
runs first drops its pass under every key, so a rerun that fails or is
stopped leaves nothing to resume, and a pass with unprovisioned checks
inside it is not journaled. The cargo preflights (each crate's tests and
clippy, the workspace's, the format check) keep the same journal per
command in `.td-build-cache/preflight-verdicts/`, and say which they
reused. Their key adds what picks and configures the toolchain: the
`rustc` cargo would run, `cargo`, `clippy`, `rustfmt` and `cc` versions,
every `CARGO_*` and `RUST*` variable but those that set only
parallelism or output, `TD_RUST_HOME`, `TD_CC_HOME` and the memory cap,
every cargo config cargo merges from above the repository or in
`CARGO_HOME`, the test runner's bytes, and which `initdb` (the native
mail leg's PostgreSQL) and `dav1d` (td-photo's decode oracles) are first
on `PATH` with what their `--version` prints, or that there is none; a
pass is recorded only if
the content is unchanged when it finishes. A run holds its journal
throughout, so a second run in the worktree waits. A gate does not
recheck the content, so do not edit a worktree under a running `ready`.
The journals do not see the rest of the host; `TD_CHECK_FULL` runs every
gate and preflight, forgetting each pass as it goes.

The recipe-checks gate answers a check from its verdict memo when that check
passed on this host before and nothing it reads has changed since: the
closure's recipe definitions with the sources they embed — the closure of
the owning recipe and of any recipe its runner declares it also builds
(`CheckRunner::extra_builds`; a check run refuses to build any other) —
the seed patches, committed cargo locks and local-source trees, the
builder's engine sources — what a build can execute, with the seed digest
table AND the local-source roster it compiles in, and not its routing,
check loop, gates, isolated crypto-crate build or host commands
(`install-apps`, `install-fonts`), which no check runs
(`engine_set::HOST_ONLY`) — and the evaluator's own
sources, with the script that builds it for the gate and the crate files
its shared modules compile in (`catalog::shared_embeds`), each as
fingerprinted when its binary was built (`td-builder engine-fingerprint`
prints the builder's).
A recipe file, and a file under `recipes/src/recipes` only recipes
embed, is not in that fingerprint: it keys the checks whose closure holds
a recipe that reads it, by that recipe's source digest (its own file,
what it embeds, and the files of the recipes whose modules it names,
transitively), so a uutils bump re-keys only the check that builds
uutils. A recipe file the evaluator's own code reads stays in the
fingerprint and re-keys every check, as fixtures and probes do. The boot
harnesses — `checks/qemu_boot` and the host-command modules beside it,
and the td-ui and td-compositor font files only the screen oracles mount
(`HOST_CHECK_SOURCES` in `recipes/build.rs`) — are not in it: no recipe
check runs them, which a test holds by name, so an edit there re-keys the
integration oracles, whose key holds them as `host-evaluator`, and no
recipe check; a shared embed that would compile one in fails the build.
The repo's cargo config is in that key; the host toolchain that compiles
either binary is not. The gate says how many checks it answered that way
and counts them apart from the ones it ran.

A check that runs says why in its log, as `[memo] CHECK runs (key K):
...`: the key's components that changed since the pass it recorded last
(`changed recipe uutils, changed sources uutils, changed lock ...`), or
that it has none on record, or that `TD_CHECK_FULL` is set. Every check
run, memoized or not, appends a line to
`~/.td/build-daemon/check-history.jsonl` — outcome, wall time, key, and
that reason — shared by every worktree and kept by `clear-store`;
`td-recipe-eval check-history [CHECK...]` sums it per check, costliest
first, to say where check time goes. A run that fails before the check
starts (the ladder lock, the memo dir) writes no record. A check that
runs also prints `[time] CHECK: setup, key, lock, build, test` seconds
and records them; the summary shows the last run's. A small check's
time is mostly `build`: re-planning and staging its whole closure from
stage0, already-built rungs included.

A branch's `ready` leaves to main the recipe checks only main's churn
re-keys. `affected-checks --run` sets `TD_CHECK_DEFER_ENGINE`, and a
check whose key differs from a pass on record here only in its
`builder-engine` or `evaluator` component, both of which every check's
key holds, does not run: it prints `[td-check-memo:deferred]` with the
components that changed, the gate counts and names it as deferred to
main beside its memo answers, and the history records it as `deferred`,
not as a run. Every pass of the check is weighed, not only the
newest, since worktrees on other bases record passes of the same check in
the shared memo; only a well-formed pass whose components hash to the
key it is filed under counts. A change to anything else the check reads
— its script, a recipe in its closure or the sources it embeds, a lock,
a local source, the seed patches — runs it, as does a check with no
such pass. The evaluator's own sources key a check in two
parts: `evaluator-checks`, what is a check's own content though every
key holds it — the Rust check bodies under
`recipes/src/bin/td_recipe_eval/checks/`, the source and OSTree pins
(which a recipe's build JSON names only by key), a recipe file the
evaluator's code reads, and a crate file the shared modules compile in,
such as the TZif reader tzdata#1 asserts — which never defers, and
`evaluator`, the rest of its code, which may. The builder-engine
component also hides the seed tables td-builder compiles in, among them
`seed/bootstrap-root.txt`, the root every post-cut check builds on, so
a branch whose diff touches `seed/` defers nothing, and the selection
says so; the same changes arriving from main are main's to run. So a
branch that touches only the builder engine, the evaluator's shared
code or the engine crate runs none of the checks they alone reach, and
one that changes a recipe or a check's body still runs the checks that
reach it whatever main did to the engine since. Weighing passes records
nothing; the pass that defers becomes the check's recorded root, as a
memo hit's does, so it outlives the collection below while this
worktree's last run needed it. main-integration runs every recipe
check undeferred (below); on the check host's machine its passes are
what the next branch compares with, while a runner on a host of its own
still covers main but leaves the branches' memo to their own runs. The
flag is part of the
verdict-journal key, so a deferring pass is never what a full `--resume`
finds; `TD_CHECK_FULL` runs every check here, deferral included, and
nothing but `affected-checks --run` sets the flag. The cost is that an
engine or evaluator change that breaks a recipe check is found on main,
after it lands; run `td-builder check recipe-checks` before pushing one
that expects to.

The gate starts its checks longest first, by each check's median executed
time in that history (`check-history --durations`), with a check that has
none starting before every recorded one: the gate ends when its longest
check does. It runs one check per 4 GiB of its own grant, and on the check
host it adds workers that each borrow 4 GiB of tokens no request holds,
only while a new request's base grant and one gate grant stay free, so
another worktree's check is always admitted and runs its first gate; its
further concurrent gates may wait behind a borrowed check. The borrowers
are as many as the pool can admit beside this request's own grants and
that reserve, and the CPUs hold at the jobs a 4 GiB check gets: two on a
32-token host, none when another worktree's check holds the slack. A
borrower takes a check only once it holds its tokens, and the gate
publishes what it borrows to the file gate-run's tree-memory watchdog adds
to the gate's budget (`TD_GATE_BORROWED_BYTES_FILE`); without that file it
borrows nothing.

The memo does not see the host — its qemu, kernel, or toolchain — so after
such a change, or when a recorded pass is in doubt, run everything:

```text
TD_CHECK_FULL=1 td-builder ready
```

The variable set to any value, the way `td-builder check --resume` reads it
to rerun gates it has a passing record for; it also runs the checks a
branch run would defer to main. A forced rerun forgets the
recorded pass before it runs, so a failure leaves nothing to answer from.
`td-recipe-eval clear-store` drops the memos with the rest of the ladder work
dir.

## Reclaiming the shared build cache

The ladder work dir under `~/.td/build-daemon/ladder-shared-v1` keeps every
output every worktree has built. Collection keeps what each worktree's last
runs needed and nothing else; there is no age window. Every build records,
under `gc-roots/`, per worktree and per target, the store basename of each
rung as its `STEP` line names it, so a build stopped or failed part way
keeps what it finished for its retry; a build that completes replaces the
record's outputs with exactly the rungs it named. A build-run memo hit records that
memo and the trees it names, and a check's verdict memo is recorded under
the check when a pass writes it, a memo hit reads it, or a branch run
defers to it. A target the worktree builds again therefore stops rooting
what it no longer names; one it never builds again stays rooted as last
built. A record that would say what it already says is not rewritten.

Normal builds, checks (including verdict memo hits) and boot preparation
attempt that collection before taking the shared ladder lock, whenever a
record has changed since the last completed sweep. It takes the ladder
exclusively without waiting; a live build defers it to a later invocation,
and failed or deferred sweeps do not advance the shared completion stamp.
Cleanup failures warn and leave the requested operation running; automatic
reports go to stderr. No host scheduler is needed. Until some record names
an output, nothing in the cache is collected. This is not a byte cap: what
the worktrees' last runs needed can still fill the disk, and cleanup runs
only when td is invoked.

A record's worktree is gone only when git has removed it: the directory
is absent, and so is git's administrative directory for it (recorded with
the roots) from a `.git/worktrees/` that is itself visible, or that
directory's name now belongs to a worktree elsewhere. A worktree
deleted by hand stays live until `git worktree prune` removes that entry,
a main checkout is never gone, and a sweep that cannot see the repository
(the check sandbox sees only its own worktree) judges nothing gone. A gone
worktree's record still roots until a live worktree's record of the same
build or check is at least as new or already names all it names, and of several
gone ones only the newest does, so removing a worktree does not collect
what its successor is about to reuse. So a key no live worktree builds
again keeps its last gone record, one per key; the bootstrap root's
ladder is one key across re-pins for that reason. Records that are never
judged gone (a main checkout, a root that is not a git checkout, the old
path of a `git worktree move`) root until their record is replaced;
removing their directory under `gc-roots/` releases them. A record from
a newer td-recipe-eval stops every sweep until this one is as new.

To see what a sweep would reclaim, or run one on demand:

```text
td-recipe-eval gc-store --dry-run
td-recipe-eval gc-store
```

`gc-store` keeps the closure of every recorded output over the cache's
reference graph — what a recorded output references stays with it — with
the receipts whose outputs all stay and the memos a record names, and
removes the other rows, trees, receipts and memos. The seed store is not
touched.

It holds the ladder exclusively, as `clear-store` does: it waits for the
running builds, checks and boots that hold it to finish and blocks new ones
while it runs, so run it when builds are quiet. A check whose verdict memo
hits never takes the ladder, and its read of that memo may overlap the memo
reaping; nothing worse than a record naming a removed memo comes of it. A
missing root costs a rebuild, never a wrong build: the cache is
content-addressed, and a missing rung cold-climbs on its next use.

Beside the ladder's records, `td-shell-cache/` keeps what the
rust-toolchain check's `td shell` proof built (ripgrep, fd, uutils), so a
rerun over an unchanged package is a content-addressed hit, committed into
that run's own store as a build would be, rather than a ten-minute
rebuild; a miss rebuilds on an emptied `newstore`, never beside the build
it replaces. The check holds it exclusively while it runs, saying so when
it waits, and empties it when the toolchain the packages link changes, so
it holds one toolchain's builds; two worktrees on different toolchains
empty it in turn. `gc-store` leaves it and `clear-store` drops it.

Fetched and host-generated seeds are pinned by the seed digest table the
evaluator compiles in (`seed/seed-digests.txt`); regenerate it after a pin,
seed patch, or stage0 source change and commit it with the change:

```text
td-recipe-eval seed-digests > seed/seed-digests.txt
```

The ladder below the gcc-14 cut is pinned the same way, by
`seed/bootstrap-root.txt`: the cut's sixteen exports
(`bootstrap_root::CUT`), every store item in their reference closure by
basename, NAR hash and references, the builder ABI they were built under,
and a digest over the ladder's recipes and the seeds it stages. A graph
whose targets are not ladder rungs stops at the exports and stages them
from the pinned root, so a builder ABI bump rebuilds only what lies above
the cut. The first such build on a machine materializes the root: it
builds the ladder from stage0 under the pinned ABI, which on a warm
machine is a cache hit, and admits the result only if every item
reproduces the pin. Nothing is downloaded or published (principle 5).

A pin builds the ladder under its own ABI token, `<abi>-root`, never the
compiled `store::BUILDER_ABI`: a graph cut at the root types its exports
AuditedSeed where a full climb types them RecipeOutput, the reuse key
binds that origin, and the two must therefore never share an output
path. The root keeps its token across later ABI bumps; the first pin
records plain `4`, which the bump to 5 retired. A ladder-side check (a
rung's own test, such as gcc-10-bridge-test) still climbs from stage0 at
the compiled ABI, so each bump re-climbs the ladder for it once. A bump
that changes what the ladder's rungs produce makes a cold machine's
admission fail with the differing items named; re-pin then, and use
`bootstrap-root check` to see the drift. An admitted root is re-verified
once per run and re-admitted if it no longer matches. Re-pinning leaves
the old root's items in the seed store; only `clear-store` reclaims
them.

Changing a ladder recipe, or a seed it stages, makes the pin stale; the
`cargo-test` preflight reds until it is re-pinned, and a cut build refuses
to start. Re-pin, review the diff, and commit it with the change:

```text
td-recipe-eval bootstrap-root pin
```

`td-recipe-eval bootstrap-root check` is the on-demand stage0 proof and
not part of `check`: it rebuilds the ladder from stage0 into a private,
empty build cache under the pinned ABI and reports every item that does
not reproduce. `bootstrap-root status` says whether the root is pinned,
current and admitted here; `TD_BOOTSTRAP_FROM_STAGE0=1` makes one
invocation build every graph from stage0. Building a ladder rung, or a
recipe that builds from one below the cut, always climbs from stage0.

A recipe built from the checkout's own trees (a `local_source`, with any
sibling `local_source_trees`) is pinned differently: DECLARATION-only, by
`seed/local-source-roster.txt`, which the evaluator and td-builder both
compile in. That table records only which key stages which paths — never a
content hash — because a local source's bytes are the checkout itself, so a
committed hash would go stale on every ordinary edit to a staged tree (105
commits once touched `seed/seed-digests.txt` this way; one row alone was
rewritten 29 times). Instead, both the evaluator (interning a seed) and
td-builder (independently re-deriving a `--auto` map/db entry's identity,
straight from the repository root, using the identical exclusion rule and
staging shape) re-derive a local source's identity LIVE from the working
tree on every run. An ordinary edit inside a staged tree therefore needs no
table regeneration and moves no row; only adding, removing, or renaming a
`local_source`/`local_source_trees` declaration does, and the
`local-source-roster` preflight reds naming the stale key until the table is
regenerated:

```text
td-recipe-eval local-source-roster > seed/local-source-roster.txt
```

A key must never appear in both tables; td-builder refuses that outright,
naming the key, on every path that gates one (the keyed seed db, the
separate local-source seed db below, and a `--auto` map entry alike). The
recipe catalog declares the local-source roster, including each recipe's
sibling trees.

A local source's registration lives in its OWN per-run, disposable seed db
— never in the shared db keyed by `seed/seed-digests.txt`'s digest. That
keyed db is authenticated wholesale for every worktree still on the same
digest table; since a local source's identity does not move with that
table, an interned local-source row there would poison the shared db on
the very next ordinary edit to its staged tree, for every later
`build-plan`, on every worktree sharing the table (found in review — the
kind of bug this whole split exists to prevent). td-builder therefore
refuses a local-source-roster row outright if it ever finds one in the
keyed db, naming the key, and the runner interns a local source only into
its own run's separate db, removed once that run's `build-plan --auto` has
read it.

The root `TD_AUTO_REPO_ROOT` names for a `--auto` re-derivation is anchored
to the checkout td-builder was built from — its own committed roster must
be byte-identical to the one compiled in — but this is a sanity check on an
already-trusted input (the same pre-existing convention
`provision_auto_vendor` uses for a rust step's committed `Cargo.lock`), not
a cryptographic proof: it catches a stale td-builder or a directory that is
plainly not the repository, not a purpose-built directory carrying a copy
of the roster.

Local-source staging excludes entries named `target`, `.git`, or
`DESIGN.md` at every depth, including sibling trees — for BOTH the
evaluator's intern and td-builder's re-derivation, from the one shared
`td_engine::local_source` implementation. This policy is unchanged by the
digest/roster split. Those entries are absent from the staged build input,
its content address, and the recipe-check memo fingerprint. Design
documents must not be consumed by these builds. Other files, including
README documents and licenses, remain pinned inputs, and the checkout's
dirty and untracked files are included exactly as tracked ones are — a
build reads what is on disk now, not the last commit. Editing `DESIGN.md`
alone selects no checks; the profiler design is the exception and retains
its runtime-contract checks.

When every changed path lies under `td-*` crates or `recipes/`, `ready`
also scopes the recipe-checks gate: the changed paths travel to the gate in
`TD_CHECK_SCOPE`, and the gate runs only the checks whose closure builds a
recipe that embeds or stages their crates, naming the checks it did not
run. That can be none: a crate some recipe embeds or stages but no recipe
check's closure builds passes the gate with every check named as
unreached. The few crate files the recipes crate's shared modules compile
in (`catalog::shared_embeds`, such as td-boot's protocol and
td-compositor's timezone rules) reach every check, and only those files
do: the rest of their crates reach what stages them. A path with
whitespace, or a diff too large for one variable, sends its crates
instead. The crates that read a changed crate are not added: a recipe
can build a reader only by staging what it reads, so the evaluator's own
closure walk already reaches its checks, while the cargo narrowing's
readers also include test-only and prose readers, and readers no recipe
builds at all. A scope none of whose crates any recipe reads runs every
check and says so.

A file under `builder/src/`, or `builder/Cargo.toml`, runs the
builder/recipes/engine workspace legs, not every crate's, unless it is
part of how crate legs are built or checked or runs code only they
reach: `affected.rs`, the crypto cargo driver, its policy and vendor
preparation, the native compositor test driver, the check spine, the
in-sandbox cargo gate, and the test runner's trusted-root path
(`run_capped.rs`, `test_root.rs`, `sandbox.rs`, `sys.rs`); the list is
`CRATE_LEG_SOURCES`. Those still take every crate, as does any other
builder path. Beside a crate the narrowed file still owes its workspace
legs, and a builder path leaves the recipe-checks run unscoped either
way.

Recipe sources scope the same way, beside crate paths or alone. A recipe
file reaches its own recipe and the recipes whose code names its module,
a file a recipe embeds (a `.mk`, a patch, a fixture) reaches that recipe,
and `recipes/locks/<dir>/` reaches the recipes whose cargo lock lies
there; each reached recipe then reaches the recipes naming its module, in
turn. The gate runs the checks whose closure, including the builds their
runner declares, holds a reached recipe. So a uutils bump runs the one
check that builds uutils, not every check above the toolchain. Anything
else under `recipes/` — the shared modules, the evaluator, `build.rs` — is
compiled into every recipe and runs every check. The gate prints, for
each check it runs, `reach: CHECK: WHY`, naming the recipe and path that
reached it, so a scope that ran more than expected explains itself. A
scoped pass is journaled under its scope, so
`td-builder check --resume` on the same tree does not take it for a full
one. `TD_CHECK_FULL` runs every check in full, scope or not, and
`td-builder check recipe-checks` on its own has no scope.

A flat recipe definition at `recipes/src/recipes/<stem>.rs` also runs
the builder/recipes/engine workspace legs and repository formatting,
without unchanged standalone crate legs. Its stem starts with a lowercase
ASCII letter and contains only lowercase ASCII letters, digits and `-`;
`crate`, `self` and `super` remain unrecognized. Recipe-checks still follows
the recipe-source scope above and validates the affected target builds.
Beside a crate change, the definition retains the workspace legs and that crate's reader
closure, including for a crate normally exempt from workspace checks.
Shared recipe machinery, nested paths and unrecognized filenames retain
the full Cargo preflight. If any standalone crate names the recipe-definition
directory or recipe directory fragments outside line comments in its scanned
sources, or outside `#` comments in its manifest, the full Cargo preflight
remains required; the roster reader graph does not model that cross-boundary
read. Every manifest-declared target file
is inspected, including paths spelled through `..`, custom build scripts,
and single-quoted target paths.

The evaluator's own tree, `recipes/src/bin/td-recipe-eval.rs` and
`recipes/src/bin/td_recipe_eval/`, is a workspace binary as well: a
change there runs the workspace legs and repository formatting, plus
the legs of each standalone crate that names `recipes/src/bin` or
`td_recipe_eval` (or may reach a recipe directory through fragments,
as above) and that crate's reader closure. Today that crate is td-vm,
whose boot-profile test includes evaluator checks. Recipe-checks still
treats an evaluator path as a miss and lists every check.

The system-level qemu oracles are a separate tier, `td-builder check
integration`, which `check` alone does not include. It runs on the host,
never in the gate sandbox: it warms the system image's inputs, then runs
`qemu-boot-system`, `qemu-deploy-rollback`, `qemu-boot-live` and
`qemu-install-system`, each with a banner saying what it proves, its
outcome and wall time, recorded in the check history as
`integration:STEP` (`td-recipe-eval check-history integration`). On its
own it runs only those steps; beside gate goals (`td-builder check check
integration`), it runs after the gates pass and says so when they do
not. Every boot runs on KVM alone, which needs the run to open
`/dev/kvm` (membership in the `kvm` group, from a login started after
joining it): an oracle never falls back to TCG, several times slower,
and without KVM it is a host gap. A q35 firmware boot splits the
irqchip under KVM (`checks/accel.rs`): with it in the host kernel, OVMF
hung polling AHCI on QEMU 10.2.1.
`TD_QEMU_ACCEL=tcg` emulates on purpose, keyed apart. It needs the
host's qemu, and OVMF for the last two (found beside qemu or in
/usr/share/OVMF, or named by
`TD_QEMU_EFI_CODE` and `TD_QEMU_EFI_VARS`). An oracle the host cannot
run is an unprovisioned skip; when none could run the tier exits 69,
which is not a pass. An oracle that passed before
with every input it boots unchanged (the same components as a recipe
check's key over its recipes, beside its name, its accelerator list and
`TD_QEMU_BOOT_TIMEOUT_SECS`) answers from its memo and boots nothing,
as `td-recipe-eval oracle-memo ORACLE` reports; when all of them do, the
warm is skipped too. Each is asked again when its step is reached, its
pass is forgotten before it boots, and `TD_CHECK_FULL=1` boots them all,
so a doubted pass that fails is gone, though a host without KVM, which
boots nothing, keeps it; an oracle whose question failed boots without
forgetting or recording. The memo does not see the host's qemu,
firmware or KVM; the key is read when asked and again when a pass
is recorded, not between.

It belongs to main. On a provisioned host (qemu, OVMF, and `/dev/kvm`
for the user), `td-builder main-integration run`, started from any
checkout of the repository, fetches `origin/main` five minutes after
each pass and runs that head's own `td-builder check recipe-checks
integration` in a detached worktree of its own, under
`~/.local/state/td/main-integration` (or `TD_MAIN_INTEGRATION_DIR`):
every recipe check, unscoped and with the deferral flag scrubbed, then
the oracles once the gates pass. A failed recipe check reds the head
and the oracles do not boot. The runner invokes each head's builder
with the tier arguments and scrubbed environment of the runner's own
build, so a landing that changes those in `main_integration.rs` takes
effect only from a runner built with it: stop the runner; if main's
newest head already has a verdict from it, run `run --once --again`;
then start `run` again. Heads that land during a run are not
queued: the next run takes the newest, so a burst of landings costs one
run. Each head's verdict and log are kept there; a red run prints the
commits since the last green one as suspects. A run killed by a signal
records nothing and runs again; any other failure, the check host's
included, is red, since an exit of 1 cannot say whose. `td-builder
main-integration status` says what the newest fetched main has (pass,
fail, host-gap, running or none yet) and exits 0, 1 or 69 as `run
--once` does, or 3 while it has no verdict. A red one is healed as any
red gate on main is (`ci/revert-suspect.sh`), and `run --once --again`
re-runs the newest head once a red's cause is understood. One runner
holds the state at a time; its hour of qemu takes the check host's
memory like any check, so a host of its own keeps it out of the agents'
way. No branch's
`ready` runs it, so an hour of qemu never holds a landing: the
selection prints that the tier is deferred to main, and names in its
notes a change to the boot path — the code, manifest, lock or build
script of td-boot, td-btrfs, td-firstboot, td-init, td-install,
td-install-qemu-test, td-json, td-kexec, td-login, td-protector, td-sh,
td-svc or td-tpm, or the recipe of the same name; the linux-x86-64 or
system-x86-64 recipe; the
oracles' code; or `builder/src/integration.rs`. The cost is that a
boot-breaking change is found on main, after it lands. A branch that
expects to touch the boot can run the tier by hand with `td-builder
check integration` before pushing.

When `ready` passes, push the branch:

```text
git push -u origin <branch>
```

The push submits it for landing. A request to change, build, or fix authorizes
this handoff; stop at a local branch only when the user explicitly asks for
local or draft work.

## Land

A single integrator lands from another clone with `td-review`, which opens a
td-ui window on the session's Wayland compositor: `/bin/td-review` in a
td image, or `~/.local/bin/td-review` on a host after `./install-apps`
(rerun it after a pull to replace an older copy). Its git has no terminal
to ask on. On a host with td-pinentry beside it, as `./install-apps`
places it, and a `WAYLAND_DISPLAY`, ssh's passphrase and git's password
prompts open td-pinentry's window, unless the environment already names
an askpass program or git's `core.askPass` answers git's; a gpg
passphrase does once `~/.gnupg/gpg-agent.conf` names it as
`pinentry-program` (td-pinentry/DESIGN.md). It lists remote
branches and their review records, `r` replays the branch's commits onto main,
`p` pushes, and `w` sweeps fully landed worktrees. Rebase landing preserves
each commit, subject, body, and review record. The post-push branch and worktree
sweeps remove fully landed ordinary branches and skip `-rolling` workstreams.
Started with `--choose-repo` instead of in a work tree, it first opens a
chooser: the repositories its window opened before, most recent first
(saved in `~/.config/td-review/repositories`, which no application view
can write), then a folder browser from `~/src`, where Return only
enters a folder and Ctrl+Return opens the one in view. The folder
chosen must be a work tree's top through its own `.git`, so a
repository in a folder above is never taken for it, and no `GIT_DIR`
or like variable may override it. Git then runs in it as it would
under `-C`, repository configuration included.

## Reclaiming landed worktrees

The builder runs local worktree reclamation after a successful fetch/run
cycle of `main-integration run` using `origin`, at most once a day. It
keeps seven days of idle worktrees. Run the same cleanup independently,
including on a machine that runs no integration loop:

```text
td-builder worktree-gc --idle-days 7 --dry-run
td-builder worktree-gc --idle-days 7
```

The command fetches `origin/main` and checks exact HEAD ancestry: every
commit reachable from HEAD must be on main. Patch-equivalent replay or
squash landings are deliberately retained. Unlanded private HEAD reflog
and worktree-ref commits retain the tree unless they also survive in the
retained branch's global reflog; ordinary amendments remain recoverable
there after removing the worktree. It keeps primary and invoking
checkouts, main, rolling workstreams, locked worktrees, and any tree with
uncommitted or untracked files or a Git operation in progress. Local and
remote branch refs and their reflogs are retained. `--idle-days 0` disables
the age check; `--dry-run` still fetches but removes no trees.
Idle age is the newest
mtime or ctime anywhere in the checkout (ignored build output included)
or its private Git metadata. Reads alone do not restart the age, and
GC's own directory reads do not make a tree active.

Before removal it inspects every visible thread belonging to the
worktree owner (any real, effective, saved or filesystem UID), checking
cwd, root, executable, open descriptors and file mappings for paths
under the checkout or its private Git directory. Other users' processes
are outside this maintenance check. An unreadable owner process retains
the tree. A procfs mount using `hidepid` is refused because it may omit
processes entirely. Run from the host PID namespace. The integration
runner Git-locks its own detached cache tree. It repeats the checks and
lock listing before `git worktree remove`, without force. This is
maintenance of trusted local checkouts during a quiet period; an
uncooperative writer can start after the last check. A Git lock is the
way to reserve an otherwise idle tree. A repository-wide file lock
prevents overlapping builder GC passes.

## Rebase a rolling workstream

Periodically, and after the integrator lands part of the stack, rebase the
rolling workstream onto the current base:

```text
git fetch origin && git rebase origin/main
```

Commits whose patches landed drop out; unfinished commits replay. The remote
workstream still contains the pre-landing copies, so the next push is normally
non-fast-forward. Before replacing them, compare stable patch IDs on the
landed and workstream copies; equal IDs prove that the discarded copies are
the work that landed. Then use:

```text
git push --force-with-lease
```

The lease is mandatory: it refuses if somebody else moved the remote branch
after the last fetch.

## Parking

If work must stop mid-increment, put the resume point in a `Next:` block in
the last commit message, above the review trailers. A fresh worktree can read
it from `git log`; an untracked plan file cannot be the handoff.

Never use `git stash` in this repository. `refs/stash` is repository-global,
not worktree-local.

# Stopping a run

To abandon a long check run, ask for it:

```text
td-builder stop
```

It stops the `ready`, `check`, `affected-checks --run` or `gate-run` that THIS
worktree started, and no other: the run writes a record inside the worktree,
so a `stop` cannot name a run whose record it cannot see. It signals through
the audited path, so the kill audit says who asked and why.

Do not reach for `pkill -f`. Every worktree invokes the same binary path and a
command line carries no cwd, so no pattern distinguishes your run from a
parallel agent's: `pkill -f td-builder` ends all of them AND the shared check
host, and the agents you did not mean to interrupt are left with swept gates
and nothing in the kill audit naming a cause. That has happened.

`stop` waits to see each run go before reporting it stopped, polling them all
together, and exits non-zero for anything it could not account for — a run
still alive after the wait, or a record it could not read — while still
stopping everything else it found. Run it again to re-signal a run that has
not gone. That non-zero exit is what makes `stop && ready` refuse to start
over a client that is still running; nothing to stop is success on its own.

What it confirms is that the recorded process has ended. A recorded run is one
the check host took, so that process owns no build tree of its own; the hosted
tree is the host's, and comes down on the host's client-went-away cancellation
shortly after. `stop` does not wait for that.

`ready` records its run when it hands selected checks to the shared host.
Its initial local selection and record scan, including an empty-selection
completion, have no run record for `stop` to name.

Short forms are not runs and are not recorded: `ready --record-only`, a bare
`affected-checks`, `gate-run --list`. Nor are `build` and `realize`, which the
check host does not take — signal a pid you recorded yourself. And
`check-host-stop` is a different thing again: it stops the shared check host,
which every worktree uses.

# When something td-builder ran was killed

Every signal td-builder sends to another process is recorded in one place:
`~/.td/kill-audit/log`. One line per signal gives the time, the sending
td-builder and its verb, the signal, the target pid or process group, the
reason the sender had, whether the kernel accepted the signal, and the
target's command line as it stood just before. The gate-run watchdog, the
build watchdog, the build daemon, the per-user check host, the check-loop
warm step and the sandbox-reaping gate all write it, so a gate, build or
check that died looks there first. The same line goes to the sender's stderr,
which for the check host is `check-host-v2.log` in its runtime directory,
`/run/user/<uid>/td-builder` where that exists; each new host rotates it and
keeps one `.prev` generation, so the audit file is the durable copy. It is
appended to and never rotated; truncate it when it has served. It is the
user's own file, writable by anything running as the user, gate code inside
the check sandbox included: a record, not tamper evidence.

The directory is td-builder's own and is bound read-write into the check
sandbox, so the gate runner inside records to the host's file. td-builder
creates `~/.td/kill-audit` but never `~/.td` itself: inside a build sandbox
HOME is a stand-in with no `~/.td`, so the build watchdog's line reaches the
build log through stderr and nothing is written into the build.

A line there is proof; its absence is only evidence. The file cannot show a
death td-builder did not signal: the kernel's OOM killer, a
`PR_SET_PDEATHSIG` cascade when a supervisor dies, the `RLIMIT_DATA` ceiling
under `run-capped` (the process fails to allocate rather than being
signalled), a signal from a terminal or another program, or a line lost
because `~/.td` was absent or unwritable. The one td control-plane program
not yet covered is `td-recipe-eval`, whose QEMU and check-runner teardown
still kill without a record.

# Formatting

Every tracked Rust file is in rustfmt's default style for its manifest's
edition; there is no `rustfmt.toml`. The `cargo-test` and `net-test`
preflights run the check over the whole repository whatever the branch
touched, `cargo-test` beside its other groups and `net-test` ahead of its
tests:

```text
td-builder gate-crates fmt --all
td-builder gate-crates fmt --all --write
```

The second formats instead of checking. Prefer it to `cargo fmt`, which
formats only the module tree it finds from each target: the recipes and the
gate definitions compile through `build.rs`-generated `include!` and
`#[path]` modules, so `cargo fmt` never reaches them. The check reads the
file list from git and refuses, by name, a tracked Rust file outside the
workspace members, the `td-*` crates and td-net; a new tree of Rust is added
to `format_roots` in `builder/src/affected.rs`.

rustfmt is not always idempotent. If `--write` leaves a difference, run it
again. Source-pin tests match the formatted text, so a reformatted line they
read needs its pin moved with it.

The check needs `rustfmt` on `PATH`, which the host toolchain is expected
to provide; a minimal rustup profile does not. td's own Rust toolchain
does not ship it yet, so on a td system the two preflights fail,
saying so, until it does. rustfmt also reads a `rustfmt.toml` above the
repository or in the user's configuration directory, as `cargo fmt` does;
keep none there.

# Test binaries run under a memory ceiling

Namespace workloads inherit only their configured standard descriptors.
The shared host/build/check namespace boundary marks descriptors above
stderr close-on-exec and closes the reaper's copies after its final fork.
A private success-byte/EOF handshake holds workload exec until those copies
are gone, preventing recovery of a descriptor through `/proc/1/fd`.
An invoking shell or persistent daemon's extra descriptor therefore cannot
reach a recipe test. Setup errors name the descriptor-cleanup operation;
the exec-error pipe remains usable until exec, so a missing program still
reports its original error. This uses Linux `close_range` with
`CLOSE_RANGE_CLOEXEC`, available since Linux 5.11 (the mount boundary already
requires Linux 5.12).

`.cargo/config.toml` points cargo's `runner` at `td-builder run-capped`, so
every cargo TEST binary runs under a per-process `RLIMIT_DATA` ceiling: 2 GiB
for `td-builder`/`td-recipe`/`td-engine`, 1 GiB for every other crate. A test
that allocates without bound now reds itself instead of driving the machine
into swap.

This means `cargo test`, `cargo run` and `cargo bench` need
`target/release/td-builder` to exist. Build it first — the command AGENTS.md
already documents:

```text
cargo build --release --manifest-path builder/Cargo.toml
```

`cargo build` never invokes a runner, so that bootstraps cleanly. A missing
runner fails loudly, naming the path and `No such file or directory`.

The runner caps only cargo test artifacts, identified by their `-C metadata`
suffix. `cargo run` binaries have no such suffix and are exec'd unchanged,
which is what keeps `td-builder check` and each gate's own larger allowance
untouched.

`TD_RUN_CAPPED_MIB=<mib>` raises or lowers the ceiling for one run. It cannot
remove it: `0` is refused. If a crate genuinely needs more, raise its entry
rather than teaching people to switch the ceiling off.

Doctests do not reach the runner and run uncapped — cargo wires a runner into
rustdoc only when cross-compiling.

Which builder gets used matters, because cargo searches ancestor directories
for `.cargo/config.toml`. Older worktrees may still live under the primary
checkout at `.claude/worktrees/*`. A worktree whose branch carries this file
uses its own copy and its own `target/release/td-builder`. An older nested
worktree without this file — or an unrelated crate under the td tree —
finds the primary checkout's config instead and execs its builder, so a
`cargo clean` there makes its tests fail with `No such file or directory`.
Build the release binary in whichever checkout supplies the config.

## Host compiler preparation for reviews

Repository reviews inherit parent PATH directories that resolve into the
jail's mounted system trees, including Guix/Nix profile directories. Symlinks
in other PATH directories also survive when their executable targets are in
those trees and the symlink is the parent PATH's first match for that command.
The jail exposes these aliases through a read-only directory; it does not
mount the user's private bin directory or copy executables.

Provide ordinary host compiler commands before launching a review. If the
host provides `gcc` but lacks `cc`, a `~/.local/bin/cc` symlink to the current
profile's `gcc` is sufficient; similarly `c++` may point to `g++`. Prefer the
stable profile path over a store generation that garbage collection can
remove. PATH-based host control-plane provisioning retains that tool PATH so
binutils need not live beside a compiler alias. An explicit `TD_CC_HOME`
still restricts provisioning to that toolchain's `bin` directory. When the
selected `cc` and the provisioned PATH's `gcc` resolve to the same executable
whose basename is `gcc`, host build preparation invokes the discovered `gcc`
path. Aliases for shared dispatchers such as ccache retain `cc`, since their
invoked name can select different underlying compilers. Rust selects linker
behavior from the driver name: a `cc` alias enables bundled LLD, which can
bypass Guix's GNU linker runtime-path setup and produce executables that need
`LD_LIBRARY_PATH`. Naming that same driver `gcc` retains the host's setup,
including for children that clear their environment. A distinct `cc` remains
selected; no compiler version or store path is pinned. The reviewer
does not set CC or rewrite linker configuration, and
td-agent has no private compiler forwarder. The controller preserves validated
parent compiler selections and header/library search paths. C and Rust
compile/link probes run in the real review jail before paid completions.
Missing tools or incompatible environments are logged limitations; metadata
resolution alone does not prove that native dependencies can build.
The target artifact graph retains its own declared tool inputs. See
`APPLICATIONS.md` §C and `td-agent/DESIGN.md` §2 for the exact boundaries.

A td system is not a full host for these preparations: its `/bin/cc` is
td-link, which links Rust and compiles no C, and it ships no `gcc`. Rust
builds and tests link through it, but what needs host C fails there:
`gate-crates crypto-cargo` (AWS-LC) with that diagnosis, and `net-test`
(ring, `CC=gcc`) and the td-update, td-vm and td-vm-guest test fixtures
that link with `-Clinker=gcc` (GNU ld, not bundled LLD) for want of a
`gcc`; run `ready` on a host with a C toolchain.

## Native compositor fixture builds

A crate with `native-compositor-tests = true` may additionally declare one
ASCII `native-compositor-fixture-feature` name in its gate metadata. The
native gate first tests the default binary, then uses a fresh separate
target directory for that feature's library tests (a crate without a
library: its binary's unit tests), ignored
`native_compositor::fixture::` process cases, and strict all-target Clippy.
Both process legs require positive passing summaries. This is a test-build
declaration, not a shipping feature or a broader affected-path mapping.
The runner removes ambient `TD_EDITOR_TEST_FILE_BARRIER` and
`TD_EDITOR_TEST_QUEUE_BARRIER`; only the owned process fixture may give
those scheduling endpoints to its child editor.

## Trusted filesystem roots for permission tests

A standalone `td-*` roster crate may declare `trusted-test-root = true` in its
`[package.metadata.td-gate]`. Both derived cargo-test command lists force
`TD_TEST_TRUSTED_ROOT=1` for that invocation; Clippy, other crates and ordinary
`cargo run` are unchanged. To exercise the same fixture directly, use
`TD_TEST_TRUSTED_ROOT=1 cargo test --manifest-path CRATE/Cargo.toml`.
The root workspace's builder/recipes/engine command is not a roster-crate
invocation and does not acquire this setting from member metadata.

The existing `run-capped` runner applies its memory ceiling first, then runs
each test artifact through `sandbox::host_shell` with caller-owned mode-1777
root and private `/tmp`. For a crate under `/tmp`, the fixture binds the
nearest enclosing worktree marked by a regular `.git` file or directory,
so tests can read sibling sources while keeping their original cwd. With
no worktree marker, it binds only the cwd. Symlink or special-file Git
markers fail setup; errors identify the marker path. The marker is not
parsed to add Git administrative directory mounts; existing ambient mounts
may expose those directories. This matches the editor's trusted-owner plus
sticky ancestor policy; components requiring mode-0755 root or forbidding
all shared write bits use the explicit `TD_TEST_TRUSTED_ROOT=private` mode.
That mode makes only the disposable namespace root mode 0755 and its `/tmp`
mode 0700, after checking both belong to the non-root caller. The native
PostgreSQL mail fixtures use this mode for protected connection files and
service directories. Host directory permissions remain unchanged. An internal
builder supervisor
is namespace PID 1 and starts the test as an ordinary child, preserving its
default signal dispositions. The extra user/mount/PID namespaces are nested
inside the check host's process-lifetime containment; parent-death handling
and memory limits remain in force. Ordinary exit codes propagate; signal
deaths propagate as nonzero `128 + signal` exit codes, not as a signaled
outer process. Setup failure fails the test invocation, never skips it or
falls back to an uncontained execution. The opt-in is read only for test
artifacts; ordinary `cargo run` is unaffected even if the variable is set.

This is an ownership fixture, not a new filesystem security sandbox. Ambient
resolvable top-level paths remain bound with their existing access; `/proc` and `/dev`
use the existing host-sandbox private/minimal implementations. The working
directory and executable remain available, including when located below
`/tmp`. `/tmp` itself cannot be the working directory; the host's `/oldroot`
is omitted because that name is reserved for pivot cleanup. Dangling
top-level symlinks are logged and omitted; other lookup failures refuse.
Resolvable top-level symlink aliases become mountpoints, not symlinks, so
this fixture is not an exact replica of host pathname identity.
The helper requires UTF-8 paths/environment and replaces
`TMPDIR` with `/tmp`; externally prepared temporary files and display sockets
under the old `/tmp` are not retained unless inside the bound working tree.
This includes contents of `HOME`, `CARGO_HOME` and `XDG_RUNTIME_DIR` beneath
the old `/tmp`. Environment strings are retained, not silently rewritten
to a different directory. Network and UTS namespaces are also private.
IPC gets a private namespace when the kernel provides IPC facilities; their
absence must be established from a freshly mounted procfs as specified in
`UNSAFE.md`. Host network services, abstract Unix sockets, SysV IPC and
hostname are not shared. Pathname sockets in retained bound paths remain
filesystem objects.
The inner process does not inherit the opt-in variable, preventing recursive
wrapping through another runner invocation. Other ancestor ownership is not
rewritten, and no production permission predicate gains a test exception.

This allows the editor's permission tests to run under the check host's identity
map: host-root-owned directories otherwise appear as an unmapped overflow
UID. Such an owner remains untrusted by production code. A real container
must supply an identifiable trusted path or the optional endpoint refuses.

## Native compositor test platform

A roster crate declaring `native-compositor-tests = true` in its gate
metadata adds a native process-test command to both the host preflight and
gate 325. It requires the discovered `td-compositor` crate. The command is
attributed to the consumer for affected-check narrowing: an editor-only
change builds its compositor test tool without selecting the compositor's
own suites or bringing the compositor into the recipe-check scope, which
names the editor alone. For the cargo narrowing, compositor changes select
declared native-test consumers as readers even without shared source files.

`td-builder gate-crates native-compositor --manifest-path CRATE/Cargo.toml`
checks that declaration, builds the repository's compositor offline into a
fresh owned directory under `target/`, then runs the consumer's ignored
`control_process` cases filtered by `native_compositor::`, with two test
threads. Ordinary tests and optional Weston cases keep their own commands.
The cases drive the compositor through one shared harness, the
`td-test-compositor` crate, which a consumer names under
`[dev-dependencies]`: it launches the tool, reads its readiness line and
speaks its control socket (layout, input with receipts, observation,
capture, clipboard). Cargo resolves a dev-dependency into the consumer's
lock, so a consumer's recipe stages that tree beside its others.
The tool's absolute UTF-8 path is forced through Cargo configuration as
`TD_TEST_COMPOSITOR`; ambient values cannot substitute another executable.
`trusted-test-root` is retained for this test command. The tool build's
explicit target directory is independent of `CARGO_TARGET_DIR`; the tests
still honor the gate's target directory. Ambient cross-target configuration
that does not produce the expected host binary fails, never reuses an old
binary. This is host test preparation, not a target artifact input.

The wrapper requires successful Cargo exit and a final, exact, nonzero
passing libtest summary from this invocation. Embedded diagnostic markers
do not count; a later zero or malformed summary retires earlier evidence.
This parses the test harness's output, not adversarial executable output.
Its streamed stdout is bounded to four MiB and 64 KiB per line; stderr is
inherited. Gate-run supplies its existing wall-clock deadline inside gate
325. Direct and host-preflight invocations have no total elapsed-time
limit. Hosted runs retain check-host memory/client-loss cancellation and
descendant containment; the direct command is not automatically hosted.
On normal completion or error the wrapper reaps its Cargo child and removes
only its owned tool directory. The process fixture owns editor/compositor
cleanup. A hard-killed wrapper can leave its uniquely named scratch
directory behind.

## Native mail read fixture

The td-mta native crypto-cargo test leg also builds the checkout's td-mail
binary and td-net unit-test executable offline. Native source preparation
warms both locked external closures; each build verifies its archived inputs.
Cargo artifact records identify the helper executables inside the selected
target directory. Ambient TD_MTA_TEST_MAIL and TD_MTA_TEST_FETCH cannot replace
them. After ordinary tests, the wrapper runs the exact ignored PostgreSQL read,
plain-text send and foreground-service cases. Each requires a positive passing
libtest summary and exit success. Host preparation must put PostgreSQL 17 on
`PATH`: the directory of the first `initdb` there must also hold `postgres`,
`pg_ctl`, `psql`, `pg_dump` and `pg_restore`, and `postgres --version` must
report release 17. The restore fixture uses the same installation for its
archive round trip. The
wrapper checks this before any test runs and names what is missing; the
fixtures find the same directory on `PATH` themselves, so no variable names
it. The fixtures own disposable clusters; no existing database service or
public endpoint is required.

td-mta/CONFORMANCE.md records what this native client fixture proves. Its
private root certificate enters only the isolated fetch test process behind
cfg(test); shipped fetch trust is unchanged.
Ordinary and portable mail tests leave this host fixture ignored. Helper
processes are killed and reaped by their owning fixture, and all mailbox,
credential, client-cache and download data live under private temporary roots.
This host test preparation does not add a target artifact dependency.

# Code review: two frontier models per commit

Every increment receives two independent reviews of one exact commit:
`opus-latest` and `gpt-sol-latest`, both at **high** effort. Select the
newest available frontier release in each family and record the concrete
versions actually used. An unresolved `latest` alias is not a receipt.
The subscription alternatives below may replace one of these models.

Use a fresh review subagent in the current harness when it can select the
required model and high effort. Give it the repository and full commit id,
ask it to inspect the complete commit, relevant instructions and surrounding
code, and return prioritized findings with file locations. It must not edit
or inherit the acting agent's conclusions.

## Subscription alternatives

To preserve API credits when subscription credits are available, prefer these
user-approved alternatives to a paid td-agent review:

- Codex keeps the independent GPT Sol review and may use a fresh Astra
  subagent instead of the td-agent Opus review.
- Claude keeps the independent Opus review and may use a fresh Fable
  subagent instead of the td-agent GPT Sol review.

Both reviews still inspect the exact commit independently at high effort;
the acting agent's own assessment does not count as either review. Select
the newest available release in the chosen family and record the concrete
model actually used. Use a fresh context without the acting agent's
conclusions. The same reconciliation and confirmation rules apply.

This is standing approval from the repository user until revoked; do not
ask again for each commit. It permits the substitution, not omission of
the second review. If the alternative is unavailable, follow the ordinary
review route unless the user has prohibited paid API calls; in that case
ask before spending API credits.

The current verifier recognizes Opus and GPT Sol receipts. Record Astra or
Fable's concrete identity, high effort, reviewed commit, findings and
dispositions in the commit body, and use the existing waiver for the
replaced slot. Do not mislabel an alternative as Opus or GPT Sol, or put
its unsupported model in a Reviewed-by trailer. Keep the other review's
normal Reviewed-by trailer. For Codex:

```text
Reviewed-by: subagent/gpt-6.1-sol
Review-waiver: opus — Astra substitution to preserve API credits; approved by repository user
Checks: <checks actually run>
```

For Claude, retain the concrete Opus Reviewed-by trailer and use:

```text
Review-waiver: gpt-sol — Fable substitution to preserve API credits; approved by repository user
```

These trailers record this standing human approval; they do not claim the
verifier independently checks the replacement review.

## API review route

When the harness cannot invoke the required model and the subscription
alternative is not used, run the following from the change's worktree:

```text
td-agent review --commit COMMIT_ID --model opus-latest
td-agent review --commit COMMIT_ID --model gpt-sol-latest
```

Run only the command for the model unavailable in the harness. td-agent
reads unpublished commits from the local Git object database, generates the
full diff and built-in review instruction, and prepares an internal checkout
pinned to that commit. Source and Git metadata are read-only; test output
uses private writable scratch. td-agent removes its checkout and scratch on
completion or error. No remote, caller-written prompt or prepared diff is
needed. The command defaults to high effort and resolves the model alias to
its concrete API identity. See `td-agent/DESIGN.md` §2 for confinement, cost
limits and retained traces. A nonzero exit or incomplete report is not a
review. Both transports fill the same reviewer slots; the subscription
alternatives above use an explicit waiver for the replaced slot. There is
no separate CLI review step.

## Review records and follow-up

Store reports under `.td-build-cache/agent-scratch/` in the owned worktree.
Summarize their findings and dispositions in the commit, then delete owned
stdout/stderr report captures and private review copies before finishing.
The full JSONL traces are separate user-owned audit records, stored centrally
in `$XDG_STATE_HOME/td-agent/reviews/` (otherwise
`~/.local/state/td-agent/reviews/`), shared across worktrees and sessions.
The command labels their paths `retained trace` and asks callers to keep them.
Never follow those paths to delete traces as part of report, session or
worktree cleanup. An explicit `--log-dir` must likewise name retained storage
outside disposable session trees. td-agent itself removes its private run
directories under the key directory's `reviews/`; those are workspaces, not
the state directory's retained traces. See `td-agent/DESIGN.md` §2.

Do not alter a reviewer's report to make it agree with the change. The acting
agent reconciles each finding, fixes valid issues and explains rejected ones
with evidence.

Schedule one panel cycle per commit. After fixes, the review subagent alone
confirms the resulting diff. Re-run the panel only for a named escalation:
a new source file, unsafe surface or dependency, a material design change,
or a disagreement that needs the other model's judgment. Record the trigger.
Stop and ask the user if passes stop converging. Review the exact final
source tree; a message-only amendment does not invalidate a source review.

Close the commit message with a review/check trailer block. Both reviewers
may use the same transport. For example:

```text
Reviewed-by: subagent/gpt-6.1-sol
Reviewed-by: td-agent/anthropic/claude-opus-5.5
Checks: <checks actually run>
```

The body records the reviewed commit, concrete models, high effort, findings,
dispositions and confirmation passes. `ready` and `td-review` share the same
record verifier. Historical CLI receipts remain readable, but the current
workflow invokes only harness subagents or `td-agent review`.

Outside the standing subscription approval above, an unavailable model may
be waived only with a reason and a named human's explicit approval. At most
one frontier reviewer may be waived:

```text
Review-waiver: opus — <reason>; approved by <human name>
```

A documentation-only increment may instead use:

```text
Review-waiver: docs-only
Checks: <checks actually run>
```

`ready` verifies that every touched path ends in `.md`; a source or
configuration change cannot use the documentation waiver.

# Commit messages

Commit messages are the durable record because there is no PR description or
web page. The integrator reads the message that lands. Include the rationale,
design decisions, review findings and dispositions, and verified-red evidence
needed to understand the increment later.

Write each commit as the commit that lands. Amend the current increment rather
than stacking a permanent `fix review nits` commit. Hard-wrap body prose at 72
columns; let genuinely unbreakable commands, paths, and diagnostics run long.

If a `Next:` block is needed, it belongs above the closing review trailers.
