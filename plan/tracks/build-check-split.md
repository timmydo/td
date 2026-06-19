section: side
status: claimed
title: build-check-split
handle: claude-fable-2715d4
date: 2026-06-18
notes: plan/build-check-split.md
summary: separate "build everything" from "the checks" — a parallel `build-recipes` phase realizes + reproducibility-checks all 21 package recipes fanned out across cores (into the shared .td-build-cache/pkg) BEFORE the build gates assert; the gates then cache-hit + memo-skip the double-build and only run their durable behavioral/oracle assertions. Fixes the cold/builder-change loop being single-threaded (serial-within-gate at -j2). Stacked on the build-recipe cache (#90).
