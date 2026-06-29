section: side
status: claimed
handle: claude-opus-cedce1
date: 2026-06-28
title: x64-toolchain-subst
notes: plan/x64-toolchain-subst.md
summary: Make x86_64 the canonical /td/store toolchain the check builds actually SUBSTITUTE; confine i686 to the bootstrap intermediate (human 2026-06-28). The i686→x86_64 split stays at the gcc-14 path (#201), not gcc-4.x. PR1 LANDED (#215): gate 414 interns the REAL cross-built x86_64 glibc 2.41 at the input-addressed path from #219's lock and runs a dynamic x86_64 binary off it. PR3 (this PR, human picked the FULL fetch short-circuit): gate 414 builds td-subst from source, PUBLISHES that real x86_64 glibc at its lock-keyed path as a SIGNED substitute, then a consumer FETCHES it (resolve-toolchain.sh: sig + StorePath + NarHash) and RUNS the fetched-not-rebuilt bytes in the own-root → 42, with cold-store/wrong-key/wrong-StorePath self-discrimination (tests/x86_64-subst-lib.sh). KEY FINDING: per-PR fetch-instead-of-build was wired for NO arch (gate 359 = fixture mechanism only). The per-PR full-build-SKIP needs the whole-toolchain CLOSURE fetch (the cross gcc is an i686 binary needing the i686 runtime to compile) → next PR. PR2 (publisher) done upstream (#213). PR4: x86_64 userland + demote the i686 final toolchain. Ladder in plan/x64-toolchain-subst.md.
