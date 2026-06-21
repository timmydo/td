section: side
status: claimed
handle: claude-fable-db65ca
date: 2026-06-21
title: seed-tarball
notes: plan/seed-tarball.md
summary: North-Star step 2 (CLAUDE.md) — serve the toolchain SEED from a frozen binary tarball, NOT a host guix, so the loop builds with NO guix install. Capture the seed closure (the locks' toolchain inputs + their closure) ONCE from a guix host into a pinned, content-addressed tarball + manifest (path -> refs + nar-hash); `td-builder seed-unpack` restores it into a td store + DB from the manifest (no daemon, no /gnu/store write); `realize_drv`'s existing multi-DB staging then feeds builds from the seed store. PR1 (this track): capture tool + `seed-unpack` + a ROUND-TRIP gate (the seed survives the tarball into a fresh td store, NAR-identical + closure-complete). PR2: build hello from the unpacked seed with /var/guix + the live /gnu/store closure OUT of the path (the real "no guix install" demo). Follows [[guix-free-seed]] step 1 (td shell builds td packages, no guix process).
