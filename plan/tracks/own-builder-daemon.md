section: mainline
status: claimed
title: own-builder-daemon
handle: claude-fable-9e6e71
date: 2026-06-19
notes: plan/own-builder-daemon.md
summary: Stand up td's OWN builder daemon so the loop realizes derivations without guix-daemon. Increment 1 (landed, #69): `td-builder realize` computes the build's input closure ITSELF (td's SQLite reader over the store db's Refs graph, replacing `guix gc -R`), builds in its userns sandbox, registers the output. Increment 5 (this claim, reassigned from dormant claude-fable-2715d4): make the build sandbox SELF-hermetic — `sandbox::build` now pivot_roots into a minimal root (staged /gnu/store + /tmp + /dev & /proc rbind'd + minimal /etc), so a build can no longer reach the host filesystem (/etc,/home,/usr,/var/guix,...) even when invoked OUTSIDE the outer host-sandbox. New durable gate `build-hermetic` (356): a realized probe build cannot see /var/guix. Closes the hidden "only hermetic inside the loop container" precondition; the daemon-byte-identity oracle (td-realize) stays the guardrail. Next: NEWPID + fresh /proc parity, a minimal-/dev builder for the standalone case, the persistent daemon mode.
