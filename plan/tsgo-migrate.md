# tsgo-migrate — drop node: native TypeScript compiler in the spec front-end (§5)

The TS spec front-end transpiled with `node` + the JS `tsc` (td-typescript). `node` (V8)
was the last guix dependence in ts-emit I'd called "retired-late". TypeScript 7 is a
native (Go) rewrite — `typescript@7.0.1-rc` — so we drop node entirely: the per-platform
native binary (`@typescript/typescript-linux-x64`, a STATICALLY-LINKED Go executable) is
a node-free `tsc`.

## What changed
- `system/td-ts.scm`: `td-tsgo-tarball` — an `origin` (FIXED-OUTPUT FETCH only, sha256
  `0m3x2q…`) of the pinned native tarball. guix is just the FETCHER (the accepted seed
  layer, like the crate `.crate` fetches); **td unpacks it itself** via `tests/tsgo.sh`
  (no guix `(build-system …)` package / copy-build-system). Extracts `package/lib/` (the
  static Go `tsc` + its `lib.*.d.ts`); front-end runs `<extracted>/lib/tsc`, no node/deps.
- `tests/ts-emit.sh` + `tests/ts-check.sh`: transpile/check via `$TD_TSGO/lib/tsc` (no node).
- 13 gates + the Makefile `build-recipes` phase: resolve `td-tsgo` (TD_TSGO) instead of
  `guix build node` + `td-typescript` (TD_NODE/TD_TSC). `tests/ts-eval-tool.sh` +
  `build-pkg.sh`: TD_TSGO.

## Why it's safe (proven before writing)
- **Node-free**: `file` says statically-linked Go ELF; `--version` runs with no node.
- **Type-checks identically**: gate 195's load-bearing control — `spec-bad-fstype.ts`
  (`rootFsType: "ext3"`, outside the union) — is rejected with the SAME `TS2322` on
  `RootFsType`. A stripper (swc/oxc) could NOT do this; tsgo is the full native compiler.
- **Byte-identical emit**: `spec-v0.ts` → JS is byte-for-byte the committed golden
  (`spec-v0.expected.js`), and `recipe-hello.ts` round-trips to the same JSON. So the
  golden is unchanged and every corpus/toolchain build output is identical (cache-hit).

`guix build node` is gone from every ts-using gate; node + the JS td-typescript → one
static tsgo binary. (td-typescript is now unused but left defined; a follow-up can drop
it. td-tsgo is a pinned external seed, like the toolchain/crates, §5.)

## Status
- gate 195 (`ts`): GREEN node-free — TS2322 control fires, golden byte-identical.
- `./check.sh corpus-no-guix`: GREEN — corpus builds on td-tsgo (JSON byte-identical → cache-hit).
- verified-red + full landing check: pending.
