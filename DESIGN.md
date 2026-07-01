# Design document — td

A functional Linux distribution, built and maintained by AI coding
agents. It aims to be bootstrapped from the rust toolchain, written
in a simple zero-dependency manner, focusing on a rust-based userspace
and using containers for everything else.

---

## 0. North star and scope

**North star:** a content-addressed, reproducible, immutable distro where the store
path doubles as integrity root and OCI digest, with one Rust sandbox stack spanning
build and run, a typed config front-end, and atomic verified generations.

---

## 1. The loop *(this section comes first — nothing else matters until it's settled)*

### 1.1 The single pass/fail command

`./check.sh` is the one command that means green or red. It sets up the hermetic,
offline sandbox — **td's own `td-builder host-sandbox --expose-cwd`, the sole loop
container**  a private PID namespace + `/proc`, its own loopback-only netns,
 and runs` make check` inside it. `make check` runs the gate
ladder, short-circuiting on the first failure; **the drop-in fragments under
`mk/gates/*.mk` (each self-registering into the `CHEAP_GATES`/`HEAVY_GATES` pools the
`check:` target expands) are the authoritative gate list** — documents point here
instead of restating it, and a new gate is a new fragment file, not an edit to a shared
list. Broad shape: config eval → differentials → `guix build --check` →
package-manager behavioral/oracle tests (built tools run, link tests, the
per-package guix differential). 

### 1.2 Agent / container boundary

The AI agent runs **outside** the container. Every build/test command it
issues enters a **fresh** container — td's own `td-builder host-sandbox` (the SOLE loop
container) — so the agent's own environment can't contaminate results and the
reproducibility rung stays honest. Every rung runs there, including `rootless` (its
nested unprivileged builder nests cleanly given td's PID-namespace parity).


## Provenance

rustup -> rust toolchain -> build td tools -> mes bootstrap -> gcc toolchain -> td-built glibc -> retarget rust toolchain to /td/store with gcc toolchain -> build world

