# toolchain-input-addressed — working notes (claude-opus-b3b7ea, 2026-06-27)

Task **2a** from the post-#199 todo: *"Input-addressed toolchain — express the /td/store
toolchain as a drv/recipe output (stable key) or a pinned td-toolchain.lock"*. Why:
**prereq for td-subst chaining** — the content-addressed path varies today.

## The problem

Gates 398/400/402/.../412 build the toolchain from the seed and intern it with
`store-add-recursive`, which content-addresses by the tree's recursive NAR hash:
`/td/store/<nar-hash>-<name>`. The modern toolchain is **not byte-reproducible** (cc1
build stamp, `ar`/install mtimes — see [[td-gcc-mesboot-494]], [[td-toolchain-store-native]]),
so that path **varies build-to-build**. A td-subst consumer ([[td-subst-track]]) asks the
server for a specific `output_path` (the narinfo `StorePath` must equal it) — it cannot do
that if the path is unknowable until after a 90-min rebuild. Task 3 (byte-repro) would fix
this too, but 2a is the cheaper unblock: **input-address** the toolchain so its path is a
pure function of its declared inputs, independent of build nondeterminism.

## What landed in this PR (the stable-key mechanism)

Chose the **pinned `td-toolchain.lock`** route (the table's stated alternative to a full
guix-style drv graph; a real `.drv` for the shell-script-driven toolchain build is a much
larger lift and unnecessary for a stable key).

- `tests/td-toolchain.lock` — declares the toolchain's COMPLETE pinned input set: 24
  source tarballs + 7 vendored boot patches (the whole bootstrap chain gate 412 consumes)
  + 3 components (gcc-14.3.0/binutils-2.44/glibc-2.41) + `recipe-rev`. Pins mirror
  seed/sources/*.lock + seed/patches/* (generated from them; the gate asserts no drift).
- `builder/src/store.rs`: `input_addressed_path(key, name)` = `make_store_path("output:out",
  key, name)`; `ToolchainLock::{parse,key,path_for}` — `key()` is sha256 over an
  order-independent canonical serialization of every declared input (+ name + recipe-rev).
- `builder/src/main.rs`: `toolchain-key LOCK`, `toolchain-path LOCK [NAME]`, and the
  placement primitive `store-add-input-addressed NAME KEY SRC STORE-DIR OUT-DB` (mirrors
  store-add-recursive; path digest = KEY, but registers the tree's REAL NAR hash + size, so
  closure/verify are unchanged — naming and content-integrity are orthogonal, the daemon's
  `output:` semantics).
- gate `toolchain-input-addressed` (mk/gates/414 + tests/) — HEAVY (stage0 + a rootless
  userns, like store-native-profile), durable + td-native end to end (no guix oracle).

## Sub-task ladder (all green)

1. store.rs helpers + unit tests — `cargo test store::` 6/6 green.
2. CLI subcommands — smoke-tested: key deterministic, paths distinct, bad component rejected.
3. td-toolchain.lock generated from seed pins (all shas resolved).
4. store-add-input-addressed — content-independence verified: two builds of different bytes
   into separate stores -> SAME path, with REAL differing NAR hashes recorded.
5. gate script — all legs pass on host (incl. the real store-ns round-trip).

## Verified-red evidence

- **content-indep (crux)**: swap `store-add-input-addressed` -> `store-add-recursive` for
  the two adds -> `FAIL: [content-indep] input-addressed path moved with content`.
- **load-bearing**: make the lock perturbation a no-op -> `FAIL: [load-bearing] perturbing
  an input pin did NOT change the path`.
- **pinned-sync**: corrupt one input pin in the lock -> `FAIL: [pinned-sync] glibc-2.41...
  lock pin ... != seed pin ...`.
- Unit: `toolchain_lock_key_is_order_independent_and_load_bearing`,
  `input_addressed_path_is_a_function_of_key_and_name_only`, `toolchain_lock_rejects_malformed`.

## Durable vs oracle

Every leg is DURABLE — there is NO guix in the room (td-native addressing end to end), so
nothing here is removable migration-oracle scaffolding. The behavioral leg (a real binary
runs from an input-addressed /td/store path, /gnu/store absent) and the content-independence
+ load-bearing legs all hold with guix retired.

## Deferred to 2b (not this PR)

Wiring the **literal** toolchain to the stable path — swap gate 412's
`store-add-recursive glibc-2.41 ...` to `store-add-input-addressed glibc-2.41 $(toolchain-key
...) ...` (a one-line change using this PR's primitive). That belongs in **2b** ("Register
toolchain -> td-subst + serve it"), where building the toolchain (~90 min, from-seed) is
inherent — registering + serving the built tree at its now-stable path is the same step.
Modifying the 90-min daily-suite gate here would force a full from-seed validation that is
not the per-PR contract, for no extra stable-key coverage (the 414 gate already proves the
producer->run round-trip on a real binary).

## Next (2b/2c)

- 2b: build the toolchain at the input-addressed path (412 swap), `subst publish` it, serve.
- 2c: cold-worktree / iteration `subst` fetch of the toolchain by its lock-computed path
  (loop stays from-seed; opt-in for dev/CI-image prep). See [[td-subst-track]].
