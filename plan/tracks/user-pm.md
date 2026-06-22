section: side
status: claimed
handle: claude-fable-db65ca
date: 2026-06-21
title: user-pm
notes: plan/user-pm.md
summary: td as a USER package manager (human 2026-06-21) — build packages into a persistent store (~/.td/store) and link them into a profile / ~/bin, the way guix profile / nix env / brew work. The build engine already exists (build-recipe + the seed: build a recipe guix-free into a store, register, GC, resolve a name -> td shell). Missing is the PROFILE layer + install UX. Started: `td-builder profile PROFILE-DIR PKG-OUT...` unions installed packages' bin/sbin into a symlink-tree profile (new `profile` gate: td builds hello+which into a persistent store, profiles them, runs profile/bin/* + a ~/bin symlink, rejects collisions). Ladder: (1) profile subcommand [DONE]; (2) persistent store + relocation decision (namespace-bind ~/.td/store over /gnu/store vs re-prefix — STORE_DIR is baked into every hash); (3) `td install/remove/list` orchestrating build-recipe -> store -> profile; (4) declarative `td-home.ts` manifest (reuse the TS front-end); (5) profile generations/rollback (reuse the M10 generation machinery). Builds on [[seed-tarball]] / [[guix-free-seed]].
