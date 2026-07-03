---
name: Work item
about: A claimable unit of work for the backlog (AGENTS.md "Parallel work")
title: ""
labels: []
---

## What

<!-- The capability or fix, stated as what a user or gate observes — not the
     mechanism. One or two sentences. -->

## Entry points

<!-- Files, commands, and gates to start from. Enough that a fresh agent can
     begin without re-deriving the context. -->

## Done

<!-- The behavioral done criterion: what test, through what real entry point,
     asserts what behavior (AGENTS.md "Test the feature, not the possibility"
     and "Definition of done"). "X builds" or "X exists" is not a Done. -->

## Collisions

<!-- The files/gates/areas this work touches. Name any exclusive-landing files
     (check.sh, Makefile, channels.scm) and any shared regenerated baselines
     (e.g. tests/guix-surface-shrink.expected, tests/guix-dependence.expected —
     regenerate on rebase, never hand-merge). Claimable only while disjoint
     from every open PR's territory. -->

## Blocked by

<!-- Issue/PR numbers that must clear first, or "none". A blocker is cleared
     when the referenced issue closes as completed or the referenced PR merges
     (a not-planned or unmerged close doesn't clear it) — there is no label to
     maintain. -->
