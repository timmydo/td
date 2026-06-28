section: side
status: done
handle: claude-fable-2d249c
date: 2026-06-28
pr: 221
title: build-resource-caps
notes: plan/build-resource-caps.md
summary: Per-build memory/resource caps so one runaway build can't OOM the host (the human hit this on a shared desktop). Layered, opt-in via env exactly like TD_BUILD_NICE (#212): an always-on setrlimit(RLIMIT_DATA) backstop applied to each build's sandbox child in sandbox.rs::build's pre_exec (works rootless + in CI), PLUS a true RSS cap via cgroup v2 memory.max when the operator delegates a writable cgroup2 dir through TD_BUILD_CGROUP (td uses a delegated cgroup the way a kubelet hands a container one — it does not try to conjure one inside the RO/rootless loop sandbox). Default OFF so the loop can never go spuriously red (a too-tight cap fails a build, never changes its bytes — reproducibility-safe like nice). Granularity is the single derivation build (not the host-sandbox loop container), matching where nice_self_for_builds is scoped. DESIGN gets a forward note that the wanted next step is a global admission scheduler (k8s-lite: per-build memory REQUESTS + admission/bin-packing across all in-flight builds), which needs all builds routed through one arbiter (the build daemon today is serial + not the only path) and a per-drv request field — deliberately out of scope here. Non-colliding: new sys.rs syscalls (prlimit64/mmap) + sandbox.rs build pre_exec edit + a new mk/gates fragment if any; no system/td.scm / check.sh / Makefile edit.
