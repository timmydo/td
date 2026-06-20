section: mainline
status: claimed
title: own-builder-daemon
handle: claude-fable-9e6e71
date: 2026-06-19
notes: plan/own-builder-daemon.md
summary: Stand up td's OWN builder daemon so the loop realizes derivations without guix-daemon. Increment 1 (landed, #69): `td-builder realize` computes the build's input closure ITSELF (td's SQLite reader over the store db's Refs graph, replacing `guix gc -R`), builds in its userns sandbox, registers the output. Increment 5 (landed, #112): make the build sandbox SELF-hermetic — `sandbox::build` pivot_roots into a minimal root (staged /gnu/store + /tmp + /dev & /proc rbind'd + minimal /etc), so a build can no longer reach the host filesystem even when invoked OUTSIDE the outer host-sandbox; durable gate `build-hermetic` (356) proves a realized probe cannot see /var/guix. Increment 6 (this claim): full pid-namespace parity — `sandbox::build` now unshares NEWPID and forks the builder to PID 1 of its own pid namespace with a FRESH procfs, so a build sees only its own process tree, not the host's (the daemon, other concurrent builds, their /proc/<pid>/environ); durable gate `build-pidns` (357) proves a realized probe is PID 1 with a private /proc. The daemon-byte-identity oracle (td-realize) stays the guardrail. Next: a minimal-/dev builder for the standalone case, the persistent daemon mode.
