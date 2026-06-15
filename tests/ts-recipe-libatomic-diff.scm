;; tests/ts-recipe-libatomic-diff.scm — input-recipes differential: the MULTI-OUTPUT
;; recipe rung (DESIGN §7.1 move-off-Guile §5; the "reconstruct individual input
;; recipes" frontier).
;;
;; Where tests/ts-recipe-pkgconfig-diff.scm proves the configure-flags + multi-URI
;; capability, this proves the recipe-DSL `outputs` field: libatomic-ops
;; (recipe-libatomic-ops.ts) splits a `debug` output off `out`, reconstructed from
;; upstream coordinates lowers to the SAME derivation as the pinned Guix corpus's
;; own `libatomic-ops` (the §2.5 oracle). libatomic-ops is the cleanest multi-output
;; demonstrator: NO configure-flags, NO custom phases, so the extra output is the
;; only thing beyond a leaf recipe. It is the prerequisite capability for nano's
;; DIRECT inputs ncurses + gettext-minimal (both carry a `doc` output).
;;
;; SELF-DISCRIMINATING, and discriminating on the OUTPUTS axis specifically:
;;   (a) CONVERGE          — libatomic-ops (out + debug) lowers to the corpus oracle
;;       drv (store-path-equal ⇒ NAR-hash-equal).
;;   (b) DISCRIMINATE-src  — a perturbed recipe (one wrong byte in the source hash)
;;       lowers to a DIFFERENT drv (the differential can never rot vacuous).
;;   (c) OUTPUTS LOAD-BEARING — the SAME recipe with `outputs` STRIPPED (so it
;;       defaults to a single `out`) lowers to a DIFFERENT drv: the declared extra
;;       output is load-bearing, not decorative — a bridge that dropped it would
;;       diverge from the oracle.
;;   (d) OUTPUT-SET        — the lowered derivation actually declares BOTH outputs
;;       (out + debug), i.e. the recipe's outputs really entered the derivation.
;;
;; Derivation-level and build-free (`#:graft? #f`). The matching BUILD + `--check`
;; (prime directive 1) + NAR-equality is the rest of the `corpus-libatomic` gate.
;; Run as a repl SCRIPT so the process exit status is the test result.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package libatomic-ops)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus-libatomic gate must pass the emitted \
recipe JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

;; Re-serialize the recipe JSON with its "outputs" field removed (leg (c)).
(define (strip-outputs json)
  (scm->json-string (alist-delete "outputs" (json-string->scm json))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (pkg-drv pkg) (package-derivation store pkg #:graft? #f))
  (define (drv-name pkg) (derivation-file-name (pkg-drv pkg)))

  (let* ((recipe-json (env-json "TD_RECIPE_LIBATOMIC_JSON"))
         (oracle    (drv-name (specification->package "libatomic-ops")))
         (cand-drv  (pkg-drv (json-recipe->package recipe-json)))
         (candidate (derivation-file-name cand-drv))
         (perturbed (drv-name (json-recipe->package
                               (env-json "TD_RECIPE_LIBATOMIC_PERTURBED_JSON"))))
         (nooutputs (drv-name (json-recipe->package (strip-outputs recipe-json))))
         (cand-outs (sort (map car (derivation-outputs cand-drv)) string<?))
         (converge?     (string=? oracle candidate))
         (disc-src?     (not (string=? oracle perturbed)))
         (disc-outs?    (not (string=? oracle nooutputs)))
         (has-both?     (equal? cand-outs '("debug" "out"))))

    (format #t "~%== input-recipes differential: a MULTI-OUTPUT recipe (libatomic-ops) vs. the Guix corpus ==~%")
    (format #t "  oracle    (corpus libatomic-ops)     : ~a~%" oracle)
    (format #t "  candidate (recipe-libatomic-ops.ts)  : ~a~%" candidate)
    (format #t "  perturbed (wrong source hash)        : ~a~%" perturbed)
    (format #t "  no-outputs (outputs stripped)        : ~a~%" nooutputs)
    (format #t "  candidate output names               : ~s~%" cand-outs)
    (format #t "~%  (a) converge        (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate-src (perturbed != oracle) : ~a~%" disc-src?)
    (format #t "  (c) outputs load-bearing (no-outputs != oracle): ~a~%" disc-outs?)
    (format #t "  (d) output set is (out debug)              : ~a~%~%" has-both?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored multi-output recipe does NOT reproduce the \
corpus oracle's derivation — the reconstructed libatomic-ops diverges from the \
package it claims to be.~%")
      (exit 1))
     ((not disc-src?)
      (format #t "FAIL: differential is vacuous — a perturbed recipe (wrong source \
hash) did NOT change the derivation.~%")
      (exit 1))
     ((not disc-outs?)
      (format #t "FAIL: the declared outputs are NOT load-bearing — stripping them \
left the derivation unchanged, so the bridge is ignoring the outputs field (or the \
package is single-output).~%")
      (exit 1))
     ((not has-both?)
      (format #t "FAIL: the lowered derivation does not declare both outputs \
(got ~s) — the recipe's extra output did not enter the derivation.~%" cand-outs)
      (exit 1))
     (else
      (format #t "PASS: a TS-authored multi-output recipe (libatomic-ops, out + \
debug) lowers store-path-identical to the Guix corpus oracle; a perturbed source \
diverges, the declared outputs are load-bearing (stripping them diverges), and the \
lowered derivation declares both out and debug.~%")
      (exit 0)))))
