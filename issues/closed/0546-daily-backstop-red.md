---
title: Daily backstop RED
labels: [daily-red]
blocked-by: none
---

<!-- Imported from GitHub issue #546 (closed) at the migration off GitHub Issues.
     Historical archive; the go-forward daily-red format is in ci/daily-backstop.md. -->

The scheduled daily backstop is RED (`td-builder daily` exit 3).

Verdict:
```
commit=dbd077a
date=2026-07-19T12:09:10+00:00
env_error=0
env_error_msg=
heavy=red
heavy_rc=1
heavy_fail=td-builder check: FATAL: loop userland build failed (td-recipe-eval build-run busybox-x86-64):
system=red
system_rc=1
system_fail=td-builder check: FATAL: loop userland build failed (td-recipe-eval build-run busybox-x86-64):
harness=green
harness_rc=0
harness_env_error=0
harness_fail=
```

Healing agent: triage `git log $(cat .td-last-green 2>/dev/null || echo origin/main~10)..origin/main`, reproduce the failing gate (the `*_fail` field names it) to confirm it is a real regression (not host contention), and form a fix-or-revert (no auto-merge — the integrator lands it). Revert helper: `ci/revert-suspect.sh --ref <sha>`. Close this item when the fix/revert lands.
