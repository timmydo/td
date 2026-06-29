section: side
status: done
handle: claude-opus-8ea90a
date: 2026-06-28
pr: 226
title: affected-checks-rs
notes: plan/affected-checks-rs.md
summary: rust-migration C1 — port tools/affected-checks.sh (1284 LOC, the biggest single shell file) to the `td-builder affected-checks` engine subcommand (builder/src/affected.rs), proven byte-equivalent to the shell oracle (development differential over 180+ paths), then DELETE the shell script and rewire callers/docs to invoke the subcommand directly (human 2026-06-28; td-builder resolved prebuilt→cargo, guix-free). Durable guards: run_self_test ported to native Rust #[test] (dynamic mapping over the real mk/gates+tests tree) + a frozen full-render byte-equality #[test] — both run every PR via the required cargo-test job / check-engine smoke. The removable shell differential is retired with its oracle (directive 4).
