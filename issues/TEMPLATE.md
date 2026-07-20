---
title:
labels: []
blocked-by: none
---

## What

<!-- The capability or fix, stated as what a user or gate observes — not the
     mechanism. One or two sentences. -->

## Entry points

<!-- Files, commands, and gates to start from. Enough that a fresh agent can
     begin without re-deriving the context. -->

## Done

<!-- The behavioral done criterion: what test, through what real entry point,
     asserts what behavior ("Test the feature, not the possibility"). "X builds"
     or "X exists" is not a Done. -->

## Collisions

<!-- The files/gates/areas this work touches. Name any exclusive-landing files
     (builder/src/gates.rs, builder/src/check_loop.rs) and any shared
     regenerated baselines (e.g. a generated Cargo.lock — regenerate on rebase,
     never hand-merge). Claimable only while disjoint from every active
     issue-* branch's territory (git ls-remote --heads origin 'issue-*'). -->
