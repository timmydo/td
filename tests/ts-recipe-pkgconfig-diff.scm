;; tests/ts-recipe-pkgconfig-diff.scm — input-recipes differential: reconstruct an
;; individual INPUT recipe (DESIGN §7.1 move-off-Guile §5; the "reconstruct
;; individual input recipes" follow-on to the now-done input-resolution track,
;; toolchain retired LAST).
;;
;; Where tests/ts-recipe-diff.scm / ts-recipe-nano-diff.scm prove the TOP package
;; (hello / nano) converges, this proves one of nano's INPUTS converges:
;; pkg-config (recipe-pkg-config.ts) — ncurses's native-input — reconstructed from
;; upstream coordinates lowers to the SAME derivation as the pinned Guix corpus's
;; own `pkg-config` (the §2.5 oracle). pkg-config is the configure-flags + multi-URI
;; rung: it exercises the two recipe-DSL firsts the bridge now carries.
;;
;; SELF-DISCRIMINATING, and discriminating on BOTH new axes specifically:
;;   (a) CONVERGE          — pkg-config lowers to the corpus oracle drv
;;       (store-path-equal ⇒ NAR-hash-equal).
;;   (b) DISCRIMINATE-flag — a perturbed configure flag lowers to a DIFFERENT drv
;;       (the differential can never rot vacuous).
;;   (c) FLAGS LOAD-BEARING — the SAME recipe with `configureFlags` STRIPPED lowers
;;       to a DIFFERENT drv: the declared flags are load-bearing, not decorative —
;;       a bridge that dropped them would diverge from the oracle.
;;   (d) MULTI-URI LOAD-BEARING — the SAME recipe with its source URI list
;;       COLLAPSED to a single URL lowers to a DIFFERENT drv: the mirror-list shape
;;       is load-bearing too.
;;
;; Derivation-level and build-free (`#:graft? #f`). The matching BUILD + `--check`
;; (prime directive 1) + NAR-equality is the rest of the `corpus-pkgconfig` gate.
;; Run as a repl SCRIPT so the process exit status is the test result.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package pkg-config)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus-pkgconfig gate must pass the emitted \
recipe JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

;; Re-serialize the recipe JSON with its "configureFlags" field removed (leg (c)).
(define (strip-flags json)
  (scm->json-string (alist-delete "configureFlags" (json-string->scm json))))

;; Re-serialize the recipe JSON with the source's URI list collapsed to its first
;; URL (leg (d)). guile-json yields a vector for a JSON array.
(define (single-uri json)
  (let* ((a     (json-string->scm json))
         (src   (assoc-ref a "source"))
         (u     (assoc-ref src "uri"))
         (first (if (vector? u) (vector-ref u 0) u))
         (src*  (map (lambda (kv) (if (string=? (car kv) "uri") (cons "uri" first) kv)) src))
         (a*    (map (lambda (kv) (if (string=? (car kv) "source") (cons "source" src*) kv)) a)))
    (scm->json-string a*)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv-name pkg) (derivation-file-name (package-derivation store pkg #:graft? #f)))

  (let* ((recipe-json (env-json "TD_RECIPE_PKGCONFIG_JSON"))
         (oracle    (drv-name (specification->package "pkg-config")))
         (candidate (drv-name (json-recipe->package recipe-json)))
         (perturbed (drv-name (json-recipe->package
                               (env-json "TD_RECIPE_PKGCONFIG_PERTURBED_JSON"))))
         (noflags   (drv-name (json-recipe->package (strip-flags recipe-json))))
         (oneuri    (drv-name (json-recipe->package (single-uri recipe-json))))
         (converge?     (string=? oracle candidate))
         (disc-flag?    (not (string=? oracle perturbed)))
         (disc-noflags? (not (string=? oracle noflags)))
         (disc-oneuri?  (not (string=? oracle oneuri))))

    (format #t "~%== input-recipes differential: a reconstructed INPUT recipe (pkg-config) vs. the Guix corpus ==~%")
    (format #t "  oracle    (corpus pkg-config)        : ~a~%" oracle)
    (format #t "  candidate (recipe-pkg-config.ts)     : ~a~%" candidate)
    (format #t "  perturbed (wrong configure flag)     : ~a~%" perturbed)
    (format #t "  no-flags  (configureFlags stripped)  : ~a~%" noflags)
    (format #t "  one-uri   (URI list -> single URL)   : ~a~%" oneuri)
    (format #t "~%  (a) converge        (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate-flag (perturbed != oracle) : ~a~%" disc-flag?)
    (format #t "  (c) flags load-bearing (no-flags != oracle) : ~a~%" disc-noflags?)
    (format #t "  (d) multi-URI load-bearing (one-uri != oracle): ~a~%~%" disc-oneuri?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored INPUT recipe does NOT reproduce the corpus \
oracle's derivation — the reconstructed pkg-config diverges from the package it \
claims to be.~%")
      (exit 1))
     ((not disc-flag?)
      (format #t "FAIL: differential is vacuous — a perturbed configure flag did \
NOT change the derivation.~%")
      (exit 1))
     ((not disc-noflags?)
      (format #t "FAIL: the declared configureFlags are NOT load-bearing — \
stripping them left the derivation unchanged, so the bridge is ignoring \
configure flags (or the package sets none).~%")
      (exit 1))
     ((not disc-oneuri?)
      (format #t "FAIL: the multi-URI source is NOT load-bearing — collapsing the \
mirror list to a single URL left the derivation unchanged, so the bridge is not \
honouring the declared URI shape.~%")
      (exit 1))
     (else
      (format #t "PASS: a TS-authored recipe for an INPUT package (pkg-config) \
lowers store-path-identical to the Guix corpus oracle; a perturbed configure flag \
diverges, and both the declared configure flags and the multi-URI source are \
load-bearing (stripping/collapsing either diverges).~%")
      (exit 0)))))
