section: side
status: done
title: build-plan
handle: claude-fable-ca5b4f
date: 2026-06-18
notes: plan/build-plan.md
summary: topo build-PLAN that chains td-built outputs into downstream builds (move-off-Guile §5, the input-recipes/own-builder follow-on) — a typed lock (NAME PATH CLASS; seed|source|td-recipe-output|crate, backward-compatible), a multi-db closure (td.db ∪ guix db) and a td-store staging path so a downstream recipe consumes a td-BUILT dependency, not a guix store path. Demo: pcre2 → grep — grep's assembled .drv references td's pcre2 output, NOT guix's pcre2 (build-plan gate).
