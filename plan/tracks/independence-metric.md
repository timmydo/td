section: side
status: done
title: independence-metric
handle: claude-opus-f4c9c8
date: 2026-06-15
pr: 63
notes: plan/independence-metric.md
summary: measure td's build-time independence from guix — a deterministic, snapshot-checked census of how many derivations in a target's build closure td reconstructs store-path-equal (owned recipes) vs are guix-supplied. New cheap `guix-dependence` gate over two targets (owned-corpus union + shipped system/td.scm).
