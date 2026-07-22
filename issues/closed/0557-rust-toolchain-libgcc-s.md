---
title: rust-toolchain: satisfy rustc's forced -lgcc_s against the static-only native gcc
labels: [bug, bootstrap, rust]
blocked-by: none
---

## What

A cold `system-x86-64` (or `rust-toolchain`) build fails in the Rust stage-2
bootstrap: linking the `src/bootstrap` build-script executables fails with

```
/td/store/…-binutils-x86-64-self-2.44/bin/ld: cannot find -lgcc_s: No such file or directory
```

`gcc-x86-64-self` is `--disable-shared --enable-static`, so no `libgcc_s.so`
exists (by design — the compiler closure keeps no shared-GCC edge). But rustc's
stock `x86_64-unknown-linux-gnu` link emits `-Wl,-Bdynamic -lgcc_s` explicitly,
and it does so *even under `-nodefaultlibs`* (which the failing link uses), so
the `wb/cc` wrapper's `-static-libgcc` cannot suppress or cover it — `-lgcc_s`
is the sole libgcc on that link and has no file to resolve to.

## Entry points

- `recipes/src/recipes/gcc-x86-64-self.rs` — `--disable-shared --enable-static`
  (lines 114-115) is why no `libgcc_s.so` exists; the fix ships a `libgcc_s.so`
  linker-script shim from this rung's own libgcc dir so every consumer resolves
  the forced `-lgcc_s` with no `-L` of its own.
- Consumers that link through this gcc and force `-lgcc_s`: the `rust-toolchain`
  stage-2 bootstrap (`rust-toolchain.rs` `wb/cc`, #533), the rust-bridge smoke
  (`recipes/src/bin/td_recipe_eval/checks/rust_toolchain.rs:75`), and downstream
  `Recipe::rust` builds (e.g. `uutils.rs`).
- Reproduce: `td-recipe-eval qemu-boot-system` (or `run system-x86-64`) from
  cold, or build the `rust-toolchain` rung directly.

## Done

`rust-toolchain` builds through stage-2 from cold and the Rust-bridge boot path
(`qemu-boot-system`) reaches its markers; the shipped compiler closure still has
no shared `libgcc_s.so` dependency (the shim resolves `-lgcc_s` to the STATIC
unwinder).

## Collisions

- `recipes/src/recipes/gcc-x86-64-self.rs` only. Disjoint from every active
  `issue-*` branch (none touch rust/gcc/toolchain). No shared regenerated
  baselines. Note: changing this deep rung rebuilds everything above it
  (cmake → rust-toolchain → uutils → system) on the daily.
