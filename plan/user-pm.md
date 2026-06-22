# user-pm — td as a user package manager, on td's OWN store (/td/store)

Handle: claude-fable-db65ca · branch: td-profile → td-store-ns

## Vision (human, 2026-06-21)

Use td as a **user package manager**, the way `guix profile` / `guix home` / nix-home / brew
work — but on **td's OWN store, breaking from guix**:

- td's store is **`/td/store`**, NOT `/gnu/store`. A clean, independent store.
- **It is NOT mixed with the local guix install.** The guix install at `/gnu/store` stays;
  td neither reads it, writes it, nor uses the guix daemon. To test locally we **own our own
  root** — td runs in a user namespace pivoted into a td-owned root where the store is
  `/td/store` and `/gnu/store` does not exist.
- A **rootless builder** any user can set up (no daemon, no root, no system guix): the
  `td-builder` binary + the seed.
- Build into a persistent **`/td/store`**, expose a **profile** (`profile/bin/xyz → /td/store`)
  on PATH, link `~/bin`.
- **Port a `guix home` config to TypeScript** (`td-home.ts`) and use td instead of guix.

The build *engine* exists (this session): `build-recipe` builds a recipe guix-free into a
store, registers, GCs; a name resolves → build → run (`td shell`); the seed removes the guix
install from the build path. What remains is (1) moving the store to `/td/store` (a real
break), and (2) the store/profile/home/UX layer on top.

## The decision: re-prefix to /td/store, own root (human, 2026-06-21)

Earlier I offered two options for the store location — (a) **namespace-bind** `~/.td/store`
over `/gnu/store` (keep guix's prefix, rootless), or (b) **re-prefix** to a td-owned path. The
human chose **(b), `/td/store`, breaking from guix now.** So:

- `STORE_DIR` moves from `/gnu/store` to **`/td/store`**. td's paths become
  `/td/store/<hash>-<name>`, distinct from guix's — a separate store, not mixed with the
  local guix install.
- **Own our own root.** `/td/store` is a root-level path. To create + use it locally without
  touching the host `/` (which holds `/gnu/store` + guix), td enters a **user namespace** and
  **pivots into a td-owned root** that has `/td/store` and **no `/gnu/store`**. (Same userns +
  pivot_root + bind-store-at-path machinery `sandbox::build` already uses — it binds its staged
  store at the store dir inside a `CLONE_NEWUSER|NEWNS` unshare; here the dir is `/td/store` and
  the root is td's own.)

## The hard part: changing the store prefix

`STORE_DIR` is in **every content hash** (`builder/src/store.rs`:
`{ty}:sha256:{inner}:{STORE_DIR}:{name}`) and in binaries' **RUNPATH/interpreter**. Moving
`/gnu/store` → `/td/store` is therefore NOT a rename — it:

1. **re-hashes every path** (new `/td/store/<newhash>-<name>`), and
2. needs the toolchain **seed to exist at `/td/store`** with its binaries pointing at
   `/td/store` (RUNPATH/interp), since td builds inherit the seed's prefix.

The seed (the guix toolchain, `/gnu/store`-prefixed) must be **relocated** to `/td/store`:
rewrite `/gnu/store → /td/store` in each tree (ELF RUNPATH/interp the patchelf way; store-path
strings in scripts/`.rodata` via length-aware substitution) **and re-derive the content-addressed
paths** (the recursive re-hash, like `nix store make-content-addressed`). This is the
foundational, genuinely-hard piece — bounded, but a real effort, and the thing that actually
makes td's store independent of guix. (Re-deriving the toolchain *from source* at `/td/store` —
Mes-style — is the alternative, bigger.)

## Phased approach (so each step is a green increment)

- **Phase 0 — own the root** [THIS STEP, `td-store-ns`]: `td-builder store-ns STORE-DIR -- CMD`
  — userns + pivot into a minimal td-owned root with STORE-DIR bound at `/td/store` and **no
  `/gnu/store`**; run CMD. Gate: place a **static** binary (`bash-static`, already in the seed —
  no RUNPATH, so it sidesteps relocation) at `/td/store/<base>`, enter the store-ns, run it, and
  assert **`/gnu/store` is ABSENT** inside (isolated from the guix install). Proves the
  `/td/store` own-root works and is unmixed from guix — the scaffolding everything else runs in.
- **Phase 1 — `STORE_DIR` configurable** [DONE]: `store::store_dir()` reads `TD_STORE_DIR`
  (default `/gnu/store`); the prefix is threaded into the hash (`make_store_path_in`) + the
  recognise sites (`main.rs`). Re-prefixing **re-hashes** — `/td/store` is a DISTINCT store,
  not a rename (unit test `re_prefix_changes_the_path_and_the_hash`). Default unchanged, so
  every existing gate is untouched. The additive enabler builds target `/td/store` through.
- **Phase 2 — seed relocation to `/td/store`** (the hard core): a `td seed-relocate` that turns
  the `/gnu/store` seed into a `/td/store` seed (re-hash + patch). Then a *dynamic* binary runs
  from `/td/store` — and a build with `TD_STORE_DIR=/td/store` produces native `/td/store` content.
- **Phase 3 — build the corpus at `/td/store`** from the relocated seed (td's content natively
  `/td/store`, no guix anywhere).
- **Phase 4+ — the user-PM UX** on top: persistent `/td/store`, profile (done), `td
  install/remove/list`, `td-home.ts`, generations/rollback.

## User-PM layer (on top of the /td/store base)

1. **`td-builder profile`** [DONE, #138] — union packages' bin/sbin into a symlink-tree profile.
2. **`td install / remove / list`** — build-recipe into `/td/store` → manifest → rebuild profile.
3. **`td-home.ts`** — declarative TS config (reuse the ts-frontend: tsgo + td-ts-eval) listing
   packages + env; `td home switch` builds the closure + a new profile generation. The user
   analog of `system/td.scm`.
4. **Profile generations + rollback** — `profile` → numbered generation; `td rollback` repoints.
   Reuse the M10 generation discipline at the user level.

## Status

- 2026-06-21: step 1 (profile) done (#138). The store moves to `/td/store` (human decision);
  Phase 0 (own-root `store-ns`) in progress on `td-store-ns`. Phases 1–3 (configurable prefix,
  seed relocation, corpus at /td/store) are the foundational break; the UX layer follows.

## Verified-red

- profile (#138): VR1 — `build_profile` symlinks to a WRONG target → "profile/bin/hello did
  not greet"; VR2 — drop the collision check → "collision not reported". Reverted.
- store-ns (Phase 0): VR — make store-ns ALSO bind `/gnu/store` → "GNU-PRESENT" → the
  "/gnu/store is PRESENT — mixed with the guix install!" leg reds (the unmixed-from-guix
  assertion is real, not just /gnu/store happening to be absent). Reverted.
