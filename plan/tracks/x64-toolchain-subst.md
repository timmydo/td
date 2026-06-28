section: side
status: claimed
handle: claude-opus-cedce1
date: 2026-06-28
title: x64-toolchain-subst
notes: plan/x64-toolchain-subst.md
summary: Make x86_64 the canonical /td/store toolchain the check builds actually SUBSTITUTE; confine i686 to the bootstrap intermediate (human 2026-06-28). Today the substitute machinery (#207/#209) feeds NO real toolchain bytes — gate 358 serves a static-bash fixture, the only lock is i686, ci/daily-full-suite.sh never calls the publisher, so resolve-toolchain.sh misses 100% and the ~90-min x86_64 cross (#201) rebuilds-and-discards every run. The i686→x86_64 split stays at the gcc-14 path (#201's existing cross point), not an earlier gcc-4.x split. PR1: tests/td-toolchain-x86_64.lock + gate 414 interns the REAL x86_64 glibc 2.41 at the lock-keyed input-addressed path and runs a dynamic x86_64 binary from it (real bytes at a stable, fetchable path, distinct from i686). PR2: real publisher + persistent ~/.td/subst store + wire publish-toolchain-subst.sh into ci/daily-full-suite.sh. PR3: wire resolve-toolchain.sh into the consuming gates (fetch by default, fall back on miss). PR4: corpus userland built x86_64 + demote the i686 final toolchain. Ladder in plan/x64-toolchain-subst.md.
