section: side
status: done
title: fast-check
handle: claude-fable-aeb054
date: 2026-06-16
notes: plan/fast-check.md
summary: (PR #64) memoized td-builder check's reproducibility double-build in the shared recipe-gate helper (tests/td-check-repro.sh), keyed on the drv hash with the check-memo guards (env-id, bounded TTL, TD_CHECK_FULL bypass, fail-closed, on-hit daemon-DB re-assertion). Warm full ./check.sh 1458s→236s (6.2×); covers all recipe gates incl. gettext + td-build-phases. A changed/forged/forced verdict always re-runs the real double-build (verified-red).
