section: side
status: claimed
handle: claude-opus-8ea90a
date: 2026-06-28
title: affected-checks-rs
notes: plan/affected-checks-rs.md
summary: rust-migration C1 — port tools/affected-checks.sh (1284 LOC, the biggest single shell file) to a `td-builder affected-checks` engine subcommand (builder/src/affected.rs), proven byte-equivalent to the shell oracle. Durable native-Rust self-test (the ported run_self_test, runs on every PR via the required cargo-test job / check-engine smoke) + a removable differential oracle (Rust `--path` output diffed byte-for-byte against the live tools/affected-checks.sh, "own then diverge" — directive 4). The shell script stays the live tool/oracle this PR; the thin-shim cutover is a deliberate follow-up (it would make the dispatcher depend on a built td-builder, a separate ergonomic decision).
