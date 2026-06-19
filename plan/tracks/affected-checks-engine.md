section: side
status: done
title: affected-checks-engine
handle: claude-opus-c30fea
date: 2026-06-19
pr: 100
notes: plan/affected-checks-engine.md
summary: affected-checks escalates builder/src (the td-builder build engine) to the full ./check.sh instead of waiving on cargo-test + td-builder alone — closes the false-green gap where a shared-engine change skips the corpus / source-interning / bootstrap / rust-build / build-plan gates that all link that engine.
