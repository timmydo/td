section: side
status: claimed
title: fast-check
handle: claude-fable-aeb054
date: 2026-06-15
notes: plan/fast-check.md
summary: halve the warm inner-loop — memoize td-builder check's reproducibility double-build in the shared recipe-gate helper (tests/td-check-repro.sh), keyed on the drv hash with the check-memo guards (env-id, bounded TTL, TD_CHECK_FULL bypass, fail-closed); covers all present + future recipe gates. A non-reproducible or changed recipe is always a MISS (verified-red intact).
