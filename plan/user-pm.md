# user-pm — td as a user package manager

Handle: claude-fable-db65ca · branch: td-profile

## Vision (human, 2026-06-21)

Use td as a user package manager: build packages into a persistent store (`~/.td/store`)
and link them into a profile / `~/bin/xyz → store`, the way `guix profile` / nix env /
brew work. The build ENGINE already exists from the seed/`td shell` work (build a recipe
guix-free into a store, register, GC, resolve a name → build → run). What's missing is the
PROFILE layer + the install UX.

## Ladder

1. **`td-builder profile`** [DONE, this PR] — union installed packages' `bin`/`sbin` into a
   symlink-tree profile (`PROFILE-DIR/bin/<tool> → <store>/<hash>-<name>/bin/<tool>`,
   absolute symlink into the store; a name from two packages is a collision). The `profile`
   gate builds hello+which into a persistent store, profiles them, runs `profile/bin/*` + a
   `~/bin` symlink, and rejects a collision.
2. **Persistent store + the relocation decision** — `STORE_DIR = "/gnu/store"` is baked into
   every content hash (`store.rs`) and binaries' RUNPATHs. Two options for `~/.td/store`:
   (a) **namespace-bind** `~/.td/store` over `/gnu/store` per-user (rootless guix/nix; what
   td's sandbox already does) — least disruption, a tiny wrapper enters a userns to run; or
   (b) **re-prefix** the store to `~/.td/store` — fully td-owned paths, no runtime namespace,
   but every hash changes (a different store, re-prefix the seed). Recommend (a) first.
3. **`td install / remove / list`** — orchestrate `build-recipe` (into the persistent store)
   → update a manifest → rebuild the profile. `td shell` already proves resolve+build.
4. **Declarative `td-home.ts`** — reuse the TS front-end so the installed set is config, not
   imperative state (the user analog of `system/td.scm`).
5. **Profile generations + rollback** — reuse the M10 generation machinery at the user level.

## Status

- 2026-06-21: profile subcommand + gate green. Verified-red pending below.
- Next: step 2 (persistent store relocation) — the one piece with real design weight.

## Verified-red (record here)

- [pending] symlink to a wrong target → "run through the profile" reds.
- [pending] drop the collision check → the collision leg reds.
