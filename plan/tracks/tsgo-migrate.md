section: side
status: claimed
title: tsgo-migrate
handle: claude-fable-300f35
date: 2026-06-20
notes: plan/tsgo-migrate.md
summary: move-off-Guile §5 / drop node — replace node + the JS tsc (td-typescript) in the TS spec front-end with the TypeScript 7 NATIVE compiler (tsgo, `typescript@7.0.1-rc`'s `@typescript/typescript-linux-x64` — a static Go binary, no node/V8). New `td-tsgo` package (url-fetch the pinned native tarball + copy lib/); ts-emit.sh + ts-check.sh + the gates run `$td-tsgo/lib/tsc` directly. PROVEN drop-in: node-free, type-checks identically (rejects spec-bad-fstype's "ext3" with TS2322 — gate 195's load-bearing control), and emits BYTE-IDENTICAL JS to node-tsc (golden + corpus JSON unchanged). Removes `guix build node` (the V8 runtime) from every ts-using gate; node + td-typescript → one static tsgo binary.
