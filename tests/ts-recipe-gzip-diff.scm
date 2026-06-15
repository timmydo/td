;; tests/ts-recipe-gzip-diff.scm — input-recipes differential: a phase that bakes a
;; build STORE PATH into a patched file (DESIGN §7.1 move-off-Guile §5; the phase
;; frontier for nano's own inputs).
;;
;; Where ts-recipe-popt-diff.scm proves a phase with literal / `(which …)`
;; substitutions, this proves the path-reference idiom: gzip (recipe-gzip.ts)
;; rewrites `exec 'gzip'` to `exec <out>/bin/gzip` via a `string-append` with an
;; `(assoc-ref outputs "out")` part, in a `(lambda* (#:key outputs …) …)`, and
;; builds with `#:tests? #f`. Reconstructed from upstream coordinates it lowers to
;; the SAME derivation as the pinned Guix corpus's own `gzip` (the §2.5 oracle).
;; This is the idiom nano's DIRECT inputs (ncurses, gettext-minimal) use to inject
;; store paths in their phases.
;;
;; SELF-DISCRIMINATING:
;;   (a) CONVERGE          — gzip (with the path-ref phase) lowers to the oracle drv.
;;   (b) DISCRIMINATE-src  — a perturbed recipe (one wrong source-hash byte) lowers
;;       to a DIFFERENT drv (never vacuous).
;;   (c) PHASE LOAD-BEARING — the SAME recipe with `phases` STRIPPED lowers to a
;;       DIFFERENT drv: the declared phase (and its store-path baking) is
;;       load-bearing, not decorative.
;;
;; Derivation-level, build-free (`#:graft? #f`). BUILD + `--check` + NAR-equality is
;; the rest of the `corpus-gzip` gate. Run as a repl SCRIPT (exit status = result).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package gzip)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus-gzip gate must pass the emitted \
recipe JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

(define (strip-phases json)
  (scm->json-string (alist-delete "phases" (json-string->scm json))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv-name pkg) (derivation-file-name (package-derivation store pkg #:graft? #f)))

  (let* ((recipe-json (env-json "TD_RECIPE_GZIP_JSON"))
         (oracle    (drv-name (specification->package "gzip")))
         (candidate (drv-name (json-recipe->package recipe-json)))
         (perturbed (drv-name (json-recipe->package
                               (env-json "TD_RECIPE_GZIP_PERTURBED_JSON"))))
         (nophases  (drv-name (json-recipe->package (strip-phases recipe-json))))
         (converge?     (string=? oracle candidate))
         (disc-src?     (not (string=? oracle perturbed)))
         (disc-phases?  (not (string=? oracle nophases))))

    (format #t "~%== input-recipes differential: a phase that bakes a store PATH (gzip) vs. the Guix corpus ==~%")
    (format #t "  oracle    (corpus gzip)              : ~a~%" oracle)
    (format #t "  candidate (recipe-gzip.ts)           : ~a~%" candidate)
    (format #t "  perturbed (wrong source hash)        : ~a~%" perturbed)
    (format #t "  no-phases (phases stripped)          : ~a~%" nophases)
    (format #t "~%  (a) converge        (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate-src (perturbed != oracle) : ~a~%" disc-src?)
    (format #t "  (c) phase load-bearing (no-phases != oracle): ~a~%~%" disc-phases?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored recipe with a store-path-baking phase does \
NOT reproduce the corpus oracle's derivation — the reconstructed gzip diverges \
from the package it claims to be (the generated string-append/lambda* gexp is not \
byte-identical to the corpus phase).~%")
      (exit 1))
     ((not disc-src?)
      (format #t "FAIL: differential is vacuous — a perturbed recipe (wrong source \
hash) did NOT change the derivation.~%")
      (exit 1))
     ((not disc-phases?)
      (format #t "FAIL: the declared phase is NOT load-bearing — stripping `phases` \
left the derivation unchanged.~%")
      (exit 1))
     (else
      (format #t "PASS: a TS-authored recipe with a phase that bakes a build store \
path (gzip: exec <out>/bin/gzip via string-append + assoc-ref outputs, built with \
#:tests? #f) lowers store-path-identical to the Guix corpus oracle; a perturbed \
source diverges, and the phase is load-bearing.~%")
      (exit 0)))))
