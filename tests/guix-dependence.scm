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
;; (libsigsegv/libunistring/pcre2/ncurses/readline — lever 4). The byte-identity
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
;; every derivation td-reproducible vs guix-supplied. One target:
;;   corpus-union   — union build closure of all owned recipes (the number that
;;                    MOVES as input-recipes lands more recipes / the toolchain).
;; (The old `shipped-system` target — (operating-system-derivation td-system) —
;; retired with the guix-system gate tier, human direction 2026-07-02: the guix
;; operating-system is not the product; td ships td-native images of td-built
;; packages, and this census measures exactly that ownership.)
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
             (srfi srfi-1)
             (json)                         ;json-string->scm (read recipes-meta.json)
             (ice-9 regex)
             (ice-9 format)
             (ice-9 textual-ports))

(define expected-file "tests/guix-dependence.expected")
(define channels-file "channels.scm")
(define meta-file "tests/recipes-meta.json")

;; --- owned set: non-perturbed gnu recipes, from td's RUST catalog ------------
;; The recipe surface is declared in Rust now (recipes/); the census reads the
;; committed manifest (`td-recipe-eval meta`, kept in sync by the recipe-rs gate)
;; instead of scanning tests/ts/recipe-*.ts — so this gate stays cheap and
;; rust-free. ONLY `"gnu"` recipes (gnu-build-system reconstructions with a
;; specification->package oracle) are owned; `"rust"`/`"cmake"` self-host tools
;; are excluded by construction (own-builder proof is their own gate); perturbed
;; twins are dropped — no enrollment, no drift.
(define recipe-meta
  ;; vector of alists: (("stem" . s) ("buildSystem" . bs) ("inputs" . #(...)) ("perturbed" . bool))
  (json-string->scm (call-with-input-file meta-file get-string-all)))

(define (meta-stem e) (assoc-ref e "stem"))

;; Authored but NOT yet built by td's own builder (not in corpus-no-guix) — excluded
;; from the td-reproducible census so it does not overstate independence.
(define not-yet-td-built '("pkg-config"))

(define owned-specs
  (sort
   (filter (lambda (s) (not (member s not-yet-td-built)))
           (map meta-stem
                (filter (lambda (e)
                          (and (string=? (assoc-ref e "buildSystem") "gnu")
                               (not (assoc-ref e "perturbed"))))
                        (vector->list recipe-meta))))
   string<?))

;; --- edge ownership (proposal point 1) --------------------------------------
;; td "builds the recipe" (td-reproducible) is necessary but NOT sufficient to own
;; it: the recipe is still guix-dependent if its declared input EDGES are satisfied
;; by guix's store paths. A recipe is EDGE-OWNED when every declared input
;; (recipe-<spec>.ts `inputs: [...]`) that is itself an owned recipe is built FROM td's
;; OWN output. `td-builder build-plan --auto` wires exactly those edges, deriving the
;; chain from the recipe GRAPH; the build-plan gate (mk/gates/365) PROVES each one
;; builds (td's dep output appears in the downstream .drv, not guix's) or reds. So
;; edge-ownership is derived here straight from the graph — no manifest — and a new
;; recipe's edges are credited automatically as the owned set grows. Build-tool /
;; non-owned inputs are the agreed external seed (exempt).
(define (recipe-declared-inputs spec)
  (let ((e (find (lambda (e) (string=? (meta-stem e) spec))
                 (vector->list recipe-meta))))
    (if e (vector->list (assoc-ref e "inputs")) '())))

;; A recipe's declared inputs that are themselves owned recipes — the edges the
;; build-plan gate chains to td outputs. (Non-owned inputs are the external seed.)
(define (owned-input-edges spec)
  (filter (lambda (i) (member i owned-specs)) (recipe-declared-inputs spec)))

;; The owned recipes that HAVE owned input edges — i.e. the ones the build-plan gate
;; chains (the rest are leaves, edge-owned vacuously). Every owned recipe is edge-owned:
;; --auto wires all owned-input edges and mk/gates/365 proves they build.
(define chained-recipes
  (sort (filter (lambda (s) (pair? (owned-input-edges s))) owned-specs) string<?))

(define pinned-commit
  (let* ((m (string-match "\"([0-9a-f]{40})\""
                          (call-with-input-file channels-file get-string-all))))
    (if m (match:substring m 1) "UNKNOWN")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (when (null? owned-specs)
    (format (current-error-port)
            "guix-dependence: no owned recipes found in ~a — refusing to record a vacuous census~%"
            meta-file)
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

  (define (target-line label u)
    (call-with-values (lambda () (classify u))
      (lambda (k n specs)
        (format #f "~a: td-reproducible ~a / ~a (~,2f%) [~a]~%"
                label k n (* 100.0 (/ k n)) (string-join specs " ")))))

  (define edge-report
    ;; Every owned recipe is edge-owned: --auto wires each owned-input edge to a td
    ;; output and mk/gates/365 proves it builds. Derived from the graph — N/N grows
    ;; with the corpus. `chained` lists the recipes with owned input edges (the rest
    ;; are leaves).
    (format #f
            "edge-owned (every owned-input edge built from a td output, proven by mk/gates/365 build-plan): ~a / ~a~%chained (recipes with owned input edges): ~a~%"
            (length owned-specs) (length owned-specs)
            (string-join chained-recipes " ")))

  (define report
    (string-append
     "# td build-time guix-dependence census — generated by tests/guix-dependence.scm\n"
     "# td-reproducible = td BUILDS the derivation with its OWN Rust builder (a\n"
     "# non-perturbed tests/ts/recipe-<spec>.ts; proven by the corpus-no-guix /\n"
     "# toolchain-no-guix gates). The\n"
     "# byte-identity corpus-* gates were retired with system/td-recipe.scm. pkg-config\n"
     "# is authored but not yet td-built (not in corpus-no-guix) and is excluded.\n"
     "# Build closure = the derivation prerequisite graph (lowering only, no build).\n"
     "# edge-owned = td builds the recipe AND every declared input edge that is itself an\n"
     "# owned recipe is built FROM a td output. `td-builder build-plan --auto` wires those\n"
     "# edges from the recipe GRAPH (no manifest); mk/gates/365 proves each chain builds.\n"
     "# Build-tool / non-owned inputs are the external seed (exempt).\n"
     "# Deterministic given the pinned channel; a pin bump re-baselines this snapshot.\n"
     (format #f "pin: ~a~%" pinned-commit)
     (format #f "owned-recipes (~a): ~a~%"
             (length owned-specs) (string-join owned-specs " "))
     (target-line "corpus-union" corpus-union)
     edge-report))

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
