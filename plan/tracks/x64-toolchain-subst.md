section: side
status: claimed
handle: claude-opus-cedce1
date: 2026-06-28
title: x64-toolchain-subst
notes: plan/x64-toolchain-subst.md
summary: Make x86_64 the canonical /td/store toolchain the check builds actually SUBSTITUTE; confine i686 to the bootstrap intermediate (human 2026-06-28). #219 keyed the x86_64 toolchain (lock + addressing gate 418) but with a static-bash FIXTURE, and #213 wired the publisher into ci/daily-full-suite.sh — so the gap is: tie the lock-keyed path to the REAL cross-built x86_64 bytes. The i686→x86_64 split stays at the gcc-14 path (#201's existing cross point), not an earlier gcc-4.x split. PR1 (this PR): gate 414 (x86_64-cross-fns.sh) interns the REAL cross-built x86_64 glibc 2.41 at the input-addressed path from #219's tests/td-toolchain-x86_64.lock and RUNS a dynamic x86_64 binary off it (real bytes at a stable, fetchable path, distinct from i686). PR2 DONE upstream (#213 publisher + ~/.td/subst). PR3: wire resolve-toolchain.sh into the x86_64-consuming gates (fetch by default, fall back on miss). PR4: corpus userland built x86_64 + demote the i686 final toolchain. Ladder in plan/x64-toolchain-subst.md.
