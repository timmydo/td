;; system/td-recipe.scm — the generic recipe lowering bridge (DESIGN §7.1
;; corpus-independence, Phase 2 of the §5 move-off-Guile goal).
;;
;; The CORPUS axis. A package recipe is AUTHORED in the TypeScript surface
;; (tests/ts/recipe-*.ts: name/version/source/build-system), transpiled by tsc and
;; evaluated by the boa evaluator (td-ts-eval), which emits it as JSON. This module
;; is the Guile side: it mechanically LOWERS that JSON to a Guix `package` — it does
;; NOT author a recipe. The recipe DATA lives in TypeScript; this is the
;; retire-last Guile lowering target (§5: Guile/gexps remain underneath as the
;; lowering layer until surface AND corpus are off them, then retired last).
;;
;; The TOP recipe is reconstructed from the TS-supplied upstream coordinates, NOT
;; looked up in `(gnu packages …)`, with the Guix corpus as the differential
;; ORACLE (§2.5 / prime directive 4 — proven equal by the `corpus`/`corpus-deps`
;; rungs, never asserted). What it still leans on — `gnu-build-system`, the
;; toolchain, and INPUT RESOLUTION — is Guix infrastructure retired LAST (§5: seed
;; external, no full-source bootstrap). A declared build INPUT is resolved to a
;; corpus package by name (`specification->package`): input resolution stays
;; Guix's for now (DESIGN §5 / the corpus-independence scope boundary — "input
;; resolution stays Guix's, toolchain retired last"); only the top recipe's own
;; coordinates come from the TS surface.
;;
;; HERMETICITY. The source becomes a DECLARED fixed-output `url-fetch` (the
;; TS-supplied uri + sha256) — the same narrowed offline contract every other td
;; source uses; the build is offline against the warm toolchain.

(define-module (system td-recipe)
  #:use-module (guix packages)
  #:use-module (guix download)
  #:use-module (guix build-system gnu)
  #:use-module ((guix licenses) #:prefix license:)
  #:use-module (gnu packages)            ;input resolution only (specification->package)
  #:use-module (json)
  #:export (json-recipe->package))

;; Dispatch a recipe's declared build system to the Guix build-system object. The
;; TS dialect's `BuildSystem` union (tests/ts/td-spec.d.ts) mirrors this set, so an
;; unsupported value is rejected at type-check time; this is the runtime backstop.
(define (build-system-for name)
  (cond
   ((string=? name "gnu") gnu-build-system)
   (else (error "td-recipe: unsupported build system" name))))

;; guile-json's json-string->scm yields alists with STRING keys; assoc-ref uses
;; equal?, so string lookups work. Fail loudly on a missing field (a malformed
;; recipe must not silently lower to something else).
(define (field alist key)
  (let ((p (assoc key alist)))
    (unless p (error "td-recipe: recipe JSON missing field" key))
    (cdr p)))

;; Optional field — recipes that declare no inputs (e.g. recipe-hello.ts) lower
;; exactly as before, so the `corpus` rung's convergence is unchanged.
(define (field/default alist key default)
  (let ((p (assoc key alist)))
    (if p (cdr p) default)))

;; A recipe's declared build INPUTS arrive as a JSON array of corpus package
;; names (guile-json yields a vector). Resolve each to a corpus package by name —
;; input resolution stays Guix's (DESIGN §5, retired LAST); the labels Guix
;; derives for new-style inputs are the package names, matching the corpus
;; oracle's, so a recipe that names a package's real inputs converges on it.
(define (resolve-inputs names)
  (map (lambda (n)
         (unless (string? n)
           (error "td-recipe: recipe input is not a package name string" n))
         (specification->package n))
       (vector->list names)))

(define (json-recipe->package json-string)
  "Reconstruct a Guix package from a TS-authored recipe emitted as JSON by the boa
evaluator.  Only the build-derivation-determining coordinates come from the
recipe (name, version, source uri+sha256, build system, and the names of any
build inputs); the human-readable metadata is placeholder (it does not enter the
derivation), so the reconstructed package converges on the corpus oracle's build
by construction."
  (let* ((a      (json-string->scm json-string))
         (name   (field a "name"))
         (version (field a "version"))
         (source (field a "source"))
         (uri    (field source "uri"))
         (hash   (field source "sha256"))
         (bs     (field a "buildSystem"))
         (inputs (field/default a "inputs" #())))
    (package
      (name name)
      (version version)
      (source (origin
                (method url-fetch)
                (uri uri)
                (sha256 (base32 hash))))
      (build-system (build-system-for bs))
      (inputs (resolve-inputs inputs))
      ;; Metadata does not enter the derivation (proven by convergence on the
      ;; oracle); kept minimal and recipe-derived.
      (synopsis name)
      (description name)
      (home-page "")
      (license license:gpl3+))))
