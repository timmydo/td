# retire-lowering-bridges-2 — drive the `tests/*-drv.scm` count down

Continuation of `retire-lowering-bridges` (which retired the package bridges
`ts-eval-drv.scm` + `td-builder-drv.scm` via `guix build -d -e '(@ (system M) pkg)'`).
That increment only covered subjects that are **packages**. The remaining bridges lower
**monadic / store-function objects** (`td-registry`, `td-placed-tree`,
`td-rollback-tree`, `td-rust-build-derivation`, …), so the simple `-e '(@ (system M) x)'`
form does not apply.

## The byte-identity trick (Form B)

`guix build -e EXPR` (guix/scripts/build.scm `compute-derivation`) dispatches on the
value EXPR evaluates to:

- `procedure?` / `gexp?` / `file-like?` → wrapped in
  `(mbegin %store-monad (set-guile-for-build (default-guile)) …)` then `run-with-store`.
- `derivation?` → **used as-is** (no `set-guile-for-build`).

The bridges call `run-with-store` WITHOUT `set-guile-for-build`, so they pick the lazy
default guile-for-build. Wrapping a monadic value in `(lambda () …)` (the `procedure?`
path) injects `(set-guile-for-build (default-guile))` → a DIFFERENT (still valid) `.drv`
for gexp-based subjects (measured: `td-registry` 3vx3… vs zanim…). To stay
byte-identical, the `-e` expression must return a **`<derivation>` directly**:

```
(let* ((s ((@ (guix store) open-connection))))
  (let ((d <INNER>))               ; INNER uses s, returns a <derivation>
    ((@ (guix store) close-connection) s) d))
```

- INNER for a monadic subject: `((@@ (guix store) run-with-store) s ((@ (system M) proc) ARGS))`
- INNER for a store-fn subject (td-build): `((@ (system td-build) td-rust-build-derivation) s RECIPE)`
- `-d` prints the `.drv`; drop `-d` to build + print the output path.

`run-with-store` is private to `(guix store)` → `(@@ …)`. Use `GUILE_LOAD_PATH=$PWD`,
NOT `-L .`, for `(system td-build)` subjects (`-L .` makes guix scan `.` as a package
path → it tries to compile `ci/*.scm`/`tests/*.scm` and dumps a garbage drv list).

Centralised in `tools/guix-lower.sh`.

## Verified byte-identity (oracle vs Form B, before any edit)

| bridge                | value         | bridge .drv / out (prefix)              | Form B |
|-----------------------|---------------|------------------------------------------|--------|
| registry-drv.scm      | DRV_REGISTRY  | 3vx3xy31…-td-registry.drv                | ✓      |
| rollback-drv.scm      | DRV_TREE      | j8i363s0…-td-placed-tree-mkfs.drv        | ✓      |
| rollback-drv.scm      | DRV_DISK      | g299lj6z…-td-rollback-disk.drv           | ✓      |
| place-drv.scm         | DRV_PLACE     | 6xkxrh06…-td-placed-tree.drv             | ✓      |
| place-drv.scm         | DRV_PRUNE     | 1np6wk1i…-td-placed-tree.drv             | ✓      |
| place-drv.scm         | IMG_1 (out)   | 6ky7n4vd…-td-generation-image-gen-1      | ✓      |
| drv-emit-drv.scm      | DRV           | 1rvs5ijz…-hello-2.12.2.drv               | ✓      |
| drv-emit-drv.scm      | DRV_PERT      | nhbb1nka…-hello-2.12.2.drv               | ✓      |
| td-drv-add-drv.scm    | DRV           | 1rvs5ijz…-hello-2.12.2.drv               | ✓      |

Pure refactor (resolution-equivalent: same .drv, same output, DIGESTS.md unchanged), so
the test is each touched gate stays green; no new assertion ⇒ no verified-red beyond
green (same posture as retire-lowering-bridges).

## Scope / out of scope

In: registry-drv.scm (gates 140 + 145), rollback-drv.scm (100), place-drv.scm (160),
drv-emit-drv.scm (230), td-drv-add-drv.scm (240).

Out (need more than `.drv`/output printing — oracle/input-resolution/probe, retire last):
verify-place-drv.scm (env-param TD_DIGEST + emits a LABEL string), generation-image-drv.scm
(REJECTS_NO_GEN behavioral assert), manifest-image-drv.scm (very verbose expr — maybe a
follow-up), daemon/build-hermetic/offline-drv.scm (inline `#~` gexps — not expressible in
an `-e` string), td-drv-assemble-drv.scm (write-td-build-spec input resolution),
td-drv-build-drv.scm + td-builder-s4-drv.scm (path-info queries).

## Status / evidence

(filled in as gates go green)
