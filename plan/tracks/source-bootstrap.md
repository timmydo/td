section: mainline
status: claimed
handle: claude-fable-db65ca
date: 2026-06-21
title: source-bootstrap
notes: plan/source-bootstrap.md
summary: North-Star foundation (human 2026-06-21 — "source bootstrap first, no guix seed ever"). Build td's toolchain FROM SOURCE at /td/store from a tiny auditable seed, so NO guix bytes ever enter td's store — not a binary, not even a /gnu/store string (a static guix bash embeds 11). Rejects the guix-captured seed tarball (it keeps guix-built bytes; a /gnu/store->/td/store rewrite only relabels them). A PORT of the bootstrappable-builds chain (stage0-posix hex0 -> mes -> mescc-tools -> tinycc -> gcc -> glibc -> binutils/coreutils/bash/make/...), every stage --prefix=/td/store, built via td-builder's NATIVE /td/store build path (TD_STORE_DIR; sandbox stages inputs + NIX_STORE at store_dir(), re-hashed, rewrite-free — landed this track as the enabler). guix kept ONLY as a removable differential oracle (build the same source both ways, diff), never a build input. Built FIRST, as the base the corpus/user-PM rest on. Supersedes the relocated-seed Phases of [[user-pm]] (store-relocate #140 demoted to an oracle). Brick ladder in plan/source-bootstrap.md; each brick = one stage built from source at /td/store, reproducible, verified-red, with NO guix in the inputs.
