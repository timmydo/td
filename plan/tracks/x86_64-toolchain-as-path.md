section: side
status: done
handle: claude-opus-aedee9
date: 2026-06-28
title: x86_64-toolchain-as-path
notes: plan/x86_64-toolchain-as-path.md
summary: Bugfix to the x86_64 toolchain FETCH short-circuit (gate 414, landed #223). The cross gcc 14.3.0 was configured --with-as/--with-ld at a build-time mktemp SCRATCH dir; after a cold fetch that path is gone and the closure binutils ships only the target-PREFIXED x86_64-pc-linux-gnu-{as,ld} (no plain as/ld), so the FETCHED gcc could not find the assembler/linker → "could not compile". The skip path had never been exercised end-to-end (the substitute store had never been pre-populated), so the build path's verify_closure passed only because the build's scratch dir lingered in-run, MASKING the fetch-only break. Fix: x86_64_bundle_tooldir installs plain as/ld into the cross gcc's OWN tooldir ($XTARGET/bin) as RELATIVE symlinks to the sibling binutils lock path (resolves on the host verify, in store-ns, and for a fetched consumer) BEFORE interning, so the published nar carries them. + a DURABLE [self-contained] structural guard in verify_closure that reds on BOTH paths if the tooldir as/ld are absent (so the build path can no longer mask a fetch-only break). Surfaced by manually populating ~/.td/subst from the #223 dev-worktree export and running the gate (human waived the full from-seed re-validation: already broken, low risk).
