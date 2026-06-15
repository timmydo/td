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
  #:use-module (guix gexp)               ;#:configure-flags is a G-EXPRESSION
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

;; A recipe's declared upstream URI is either a single URL string or — for a
;; package with mirror fallbacks, like pkg-config — a JSON array of URLs
;; (guile-json yields a vector). `url-fetch` accepts a string OR a list of
;; strings; the source derivation (hence the package's whole derivation) is
;; byte-identical to the corpus oracle only when the URI SHAPE matches, so a
;; declared list passes through as a list (a single string is unchanged, so
;; hello/nano lower exactly as before).
(define (recipe-uri source)
  (let ((u (field source "uri")))
    (if (vector? u) (vector->list u) u)))

;; A recipe's declared #:configure-flags (a JSON array of literal strings;
;; guile-json yields a vector). `gnu-build-system` reads #:configure-flags as a
;; G-EXPRESSION wrapping a quoted list (the corpus packages write
;; `#:configure-flags #~'( … )`), and that quoted list is spliced verbatim into
;; the build expression — so to converge on a corpus package that sets flags the
;; bridge must reconstruct exactly that gexp shape (`#~(quote #$flags)`). Omitted
;; or empty ⇒ the EMPTY argument list, i.e. the default `gnu-build-system`
;; arguments — byte-identical to specifying none — so a recipe that declares no
;; flags (hello, nano) lowers unchanged (the `corpus`/`corpus-deps` oracles are
;; untouched, directive 3).
;; A recipe's declared build PHASES (DESIGN §7.1 move-off-Guile §5; the phase
;; frontier for nano's own inputs, whose recipes patch source files in custom
;; phases). A phase is structured DATA in the TS surface — position/anchor/name +
;; a list of `substitute*` substitutions — that the bridge LOWERS to the same
;; `(modify-phases %standard-phases …)` gexp the corpus package writes by hand;
;; building it programmatically yields the byte-identical build expression (so the
;; reconstructed package converges on the corpus oracle). `gnu-build-system` /
;; `(guix build utils)` (substitute*/which/modify-phases) stay the build-time
;; toolchain (retired LAST, §5); only the phase DATA comes from the TS surface.
;;
;; A substitution's replacement is either a literal string or `{which: PROG}`
;; (the `(which PROG)` that resolves a program on PATH at build time — a common
;; patch idiom). `returnTrue` appends a trailing `#t` to the phase body, matching
;; packages whose phase ends in `#t`.
(define (subst-replacement->gexp to)
  (cond
   ((string? to) to)
   ((and (pair? to) (assoc "which" to)) #~(which #$(assoc-ref to "which")))
   (else (error "td-recipe: unsupported substitution replacement" to))))

(define (substitution->gexp s)
  (let ((file (field s "file"))
        (from (field s "from"))
        (to   (subst-replacement->gexp (field s "to"))))
    #~(substitute* #$file ((#$from) #$to))))

(define (phase->gexp p)
  (let* ((pos    (field p "position"))
         (anchor (string->symbol (field p "anchor")))
         (name   (string->symbol (field p "name")))
         (subs   (map substitution->gexp (vector->list (field p "substitutions"))))
         (lam    (if (eq? (field/default p "returnTrue" #f) #t)
                     #~(lambda _ #$@subs #t)
                     #~(lambda _ #$@subs))))
    (cond
     ((string=? pos "before") #~(add-before (quote #$anchor) (quote #$name) #$lam))
     ((string=? pos "after")  #~(add-after  (quote #$anchor) (quote #$name) #$lam))
     (else (error "td-recipe: phase position must be \"before\" or \"after\"" pos)))))

;; The recipe's #:phases gexp, or #f when none are declared (so a recipe without
;; phases lowers exactly as before — the existing oracles are untouched).
(define (recipe-phases alist)
  (let ((ps (vector->list (field/default alist "phases" #()))))
    (and (pair? ps)
         #~(modify-phases %standard-phases #$@(map phase->gexp ps)))))

(define (recipe-arguments alist)
  (let ((flags  (vector->list (field/default alist "configureFlags" #())))
        (phases (recipe-phases alist)))
    (append
     (if (null? flags) '() (list #:configure-flags #~(quote #$flags)))
     (if phases (list #:phases phases) '()))))

;; A recipe's declared package OUTPUTS (a JSON array of names; guile-json yields a
;; vector). Many corpus packages split off extra outputs — `debug`, `static`,
;; `doc` (e.g. nano's own inputs ncurses + gettext-minimal carry a `doc`) — and an
;; extra output enters the build derivation (the output list + the build
;; expression's strip/debug handling), so to converge on such a package the bridge
;; must declare the same outputs. Omitted ⇒ the default `("out")`, byte-identical
;; to specifying no outputs, so a single-output recipe (hello, nano, pkg-config)
;; lowers unchanged (those oracles are untouched, directive 3).
(define (recipe-outputs alist)
  (let ((outs (vector->list (field/default alist "outputs" #("out")))))
    (if (null? outs) '("out") outs)))

(define (json-recipe->package json-string)
  "Reconstruct a Guix package from a TS-authored recipe emitted as JSON by the boa
evaluator.  Only the build-derivation-determining coordinates come from the
recipe (name, version, source uri+sha256 — a single URL or a mirror LIST, build
system, any #:configure-flags, any extra outputs, any custom #:phases, and the
names of any build inputs); the human-readable metadata is placeholder (it does
not enter the derivation), so the reconstructed package converges on the corpus
oracle's build by construction."
  (let* ((a      (json-string->scm json-string))
         (name   (field a "name"))
         (version (field a "version"))
         (source (field a "source"))
         (hash   (field source "sha256"))
         (bs     (field a "buildSystem"))
         (inputs (field/default a "inputs" #())))
    (package
      (name name)
      (version version)
      (source (origin
                (method url-fetch)
                (uri (recipe-uri source))
                (sha256 (base32 hash))))
      (build-system (build-system-for bs))
      (arguments (recipe-arguments a))
      (outputs (recipe-outputs a))
      (inputs (resolve-inputs inputs))
      ;; Metadata does not enter the derivation (proven by convergence on the
      ;; oracle); kept minimal and recipe-derived.
      (synopsis name)
      (description name)
      (home-page "")
      (license license:gpl3+))))
