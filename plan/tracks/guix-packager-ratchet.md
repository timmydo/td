section: side
status: claimed
title: guix-packager-ratchet
handle: claude-fable-a94246
date: 2026-06-20
notes: plan/guix-packager-ratchet.md
summary: move-off-Guile §5 ENFORCEMENT — make "no NEW guix-as-packager usage" a tracked, one-way-ratcheted invariant so the arc can't silently regress. New cheap gate `guix-surface` (tests/guix-surface.sh + snapshot tests/guix-surface.expected) statically scans the loop's orchestration sources (Makefile, mk/gates/*.mk, tests/*.sh) for `guix build -e '(@ (system M) PKG)'`, classifies each ref by reading system/M.scm (a `(package …)` define = PACKAGER; an `(origin …)`/fetch define = an allowed FETCHER seed), and records the sorted PACKAGER sites. FAIL if the set grows (a regression needing sign-off + a deliberate .expected edit); PASS when it only shrinks (re-baseline to lock). Purely additive (directive 3): removes/loosens nothing. Baseline = 47 sites (td-builder/td-ts-eval/td-typescript), retired by their own tracks. + CLAUDE.md directive 7 and DESIGN §5: new seeds are td-placed fixed-output fetches, never guix `(build-system …)` packages.
