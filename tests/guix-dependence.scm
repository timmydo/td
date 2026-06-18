;; tests/guix-dependence.scm — measure td's BUILD-TIME independence from guix
;; (independence-metric track; the number behind the human's 2026-06-15 question
;; "until td can build all those packages itself, we're just testing guix").
;;
;; A derivation is "td-reproducible" iff td BUILDS it with its OWN Rust builder —
;; i.e. a non-perturbed `tests/ts/recipe-<spec>.ts` exists AND a sibling gate proves
;; it builds via `td-builder build-recipe` (behaviorally + reproducibly, no guix/Guile
;; in the build path): corpus-no-guix for the corpus packages, toolchain-no-guix for
;; the reconstructed toolchain leaves (make/sed/grep/xz/diffutils/patch/file/
;; coreutils/gawk/tar/findutils/bash), corpus-deps-no-guix for the library deps
;; (libsigsegv/libunistring/pcre2 — lever 4). The byte-identity
;; `corpus-*` gates that used to ground this were RETIRED when system/td-recipe.scm was
;; dropped (move-off-Guile §5: own, then diverge — the own-builder path is all-durable,
;; no Guix byte-identity oracle). (The census grounds ownership on the recipe files and
;; asserts each resolves to a real corpus package; it does NOT re-lower the TS recipes
;; here — that proof is the build-recipe gates' job.)
;;
;; EXCLUDED: pkg-config has an authored recipe but no own-builder build yet — its
;; bundled glib hits a C-standard wall under td's build env — so it is NOT counted
;; as td-reproducible (it would overstate independence). Drop the exclusion once
;; corpus-no-guix covers pkg-config.
;;
;; For each TARGET we take the full BUILD CLOSURE — the derivation prerequisite
;; graph (`derivation-prerequisites`; lowering only, NO building) — and classify
;; every derivation td-reproducible vs guix-supplied. Two targets:
;;   corpus-union   — union build closure of all owned recipes (the number that
;;                    MOVES as input-recipes lands more recipes / the toolchain).
;;   shipped-system — (operating-system-derivation td-system) from system/td.scm,
;;                    i.e. the product td actually ships.
;;
;; The emitted report is DETERMINISTIC given the pinned channel. The gate
;; (mk/gates/070) compares it verbatim to tests/guix-dependence.expected; drift
;; is a deliberate re-baseline (the DIGESTS pattern) — landing a recipe raises
;; the number and the snapshot delta shows it in the PR; a pin bump re-baselines
;; it like DIGESTS. Re-baseline with: TD_DEPENDENCE_WRITE=1 guix repl -L . \
;;   tests/guix-dependence.scm
(use-modules (guix store)
             (guix derivations)
             (guix packages)
             (guix monads)
             (gnu packages)                 ;specification->package (the oracle)
             (gnu system)                   ;operating-system-derivation
             (system td)                    ;td-system (the shipped declaration)
             (srfi srfi-1)
             (ice-9 ftw)                    ;scandir
             (ice-9 regex)
             (ice-9 format)
             (ice-9 textual-ports))

(define recipe-dir "tests/ts")
(define expected-file "tests/guix-dependence.expected")
(define channels-file "channels.scm")

;; --- owned set: non-perturbed recipe-<spec>.ts files ------------------------
(define (recipe-file->spec name)
  ;; "recipe-pkg-config.ts" -> "pkg-config"
  (substring name (string-length "recipe-")
             (- (string-length name) (string-length ".ts"))))

;; Authored but NOT yet built by td's own builder (not in corpus-no-guix) — excluded
;; from the td-reproducible census so it does not overstate independence.
(define not-yet-td-built '("pkg-config"))

;; td's OWN programs, authored as recipes for the SELF-HOST (buildSystem "rust"),
;; not guix-corpus reconstructions — they have no `specification->package` oracle
;; by design, so they don't fit this corpus closure census (which is rooted in guix
;; package derivations). Their own-builder proof is the rust-build gate (td builds
;; td-builder itself, reproducibly), not a corpus differential.
(define self-host-specs '("td-builder"))

(define owned-specs
  (sort
   (filter (lambda (s) (not (or (member s not-yet-td-built)
                                (member s self-host-specs))))
           (map recipe-file->spec
                (or (scandir recipe-dir
                             (lambda (n)
                               (and (string-prefix? "recipe-" n)
                                    (string-suffix? ".ts" n)
                                    (not (string-contains n "perturbed")))))
                    '())))
   string<?))

(define pinned-commit
  (let* ((m (string-match "\"([0-9a-f]{40})\""
                          (call-with-input-file channels-file get-string-all))))
    (if m (match:substring m 1) "UNKNOWN")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (when (null? owned-specs)
    (format (current-error-port)
            "guix-dependence: no owned recipes found under ~a — refusing to record a vacuous census~%"
            recipe-dir)
    (exit 2))

  ;; spec -> the .drv td reconstructs (assert each is a real corpus package).
  (define spec+drv
    (map (lambda (spec)
           (let ((pkg (false-if-exception (specification->package spec))))
             (unless pkg
               (format (current-error-port)
                       "guix-dependence: owned recipe spec ~s resolves to no corpus package (oracle missing)~%"
                       spec)
               (exit 2))
             (cons spec (derivation-file-name
                         (package-derivation store pkg #:graft? #f)))))
         owned-specs))
  (define drv->spec (map (lambda (p) (cons (cdr p) (car p))) spec+drv))
  (define owned-drvs (delete-duplicates (map cdr spec+drv)))

  (define (closure-drvs target-drv)
    (lset-union string=?
                (list (derivation-file-name target-drv))
                (map derivation-input-path
                     (derivation-prerequisites target-drv))))

  ;; -> (values count total sorted-specs-present)
  (define (classify u)
    (let* ((total (length u))
           (present (filter (lambda (d) (member d owned-drvs)) u))
           (specs (sort (delete-duplicates
                         (map (lambda (d) (assoc-ref drv->spec d)) present))
                        string<?)))
      (values (length present) total specs)))

  (define corpus-union
    (fold (lambda (spec acc)
            (lset-union string=? acc
                        (closure-drvs (package-derivation
                                       store (specification->package spec)
                                       #:graft? #f))))
          '() owned-specs))
  (define system-closure
    (closure-drvs (run-with-store store (operating-system-derivation td-system))))

  (define (target-line label u)
    (call-with-values (lambda () (classify u))
      (lambda (k n specs)
        (format #f "~a: td-reproducible ~a / ~a (~,2f%) [~a]~%"
                label k n (* 100.0 (/ k n)) (string-join specs " ")))))

  (define report
    (string-append
     "# td build-time guix-dependence census — generated by tests/guix-dependence.scm\n"
     "# td-reproducible = td BUILDS the derivation with its OWN Rust builder (a\n"
     "# non-perturbed tests/ts/recipe-<spec>.ts; proven by the corpus-no-guix /\n"
     "# toolchain-no-guix gates). The\n"
     "# byte-identity corpus-* gates were retired with system/td-recipe.scm. pkg-config\n"
     "# is authored but not yet td-built (not in corpus-no-guix) and is excluded.\n"
     "# Build closure = the derivation prerequisite graph (lowering only, no build).\n"
     "# Deterministic given the pinned channel; a pin bump re-baselines this snapshot.\n"
     (format #f "pin: ~a~%" pinned-commit)
     (format #f "owned-recipes (~a): ~a~%"
             (length owned-specs) (string-join owned-specs " "))
     (target-line "corpus-union" corpus-union)
     (target-line "shipped-system" system-closure)))

  (cond
   ((getenv "TD_DEPENDENCE_WRITE")
    (call-with-output-file expected-file
      (lambda (port) (display report port)))
    (format #t "~a" report)
    (format #t ">> WROTE baseline ~a~%" expected-file)
    (exit 0))
   (else
    (let ((want (if (file-exists? expected-file)
                    (call-with-input-file expected-file get-string-all)
                    "")))
      (format #t "~a" report)
      (cond
       ((string=? report want)
        (format #t ">> PASS: build-time guix-dependence census matches ~a~%" expected-file)
        (exit 0))
       (else
        (format (current-error-port)
                "~%FAIL: census drifted from ~a. If this is intended (a recipe landed, or a pin bump), re-baseline:~%  TD_DEPENDENCE_WRITE=1 guix repl -L . tests/guix-dependence.scm~%~%--- expected ---~%~a~%--- got ---~%~a~%"
                expected-file want report)
        (exit 1)))))))
