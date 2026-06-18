section: side
status: done
title: rust-build
handle: claude-fable-a00773
date: 2026-06-17
pr: 81
notes: plan/rust-build.md
summary: td-builder gained its OWN cargo build path (run_rust + the `rust-build` subcommand) — proven by SELF-HOSTING: td builds td-builder itself from source with no gnu-build-system / no Guix cargo-build-system in the build logic (rustc/gcc seed external, §5). Gate 330-rust-build: structural + durable behavioral (runs, agrees with guix) + durable repro (td double-build) + removable migration oracle; verified-red. Full ./check.sh green. Inc.1 of the track; vendored-deps + a uutils tool are follow-ups.
