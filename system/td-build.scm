;; system/td-build.scm — construct a derivation built by td's OWN Rust builder
;; instead of gnu-build-system (DESIGN §7.1 corpus-independence; plan/
;; corpus-independence.md "own Rust builder").
;;
;; Where system/td-recipe.scm lowers a TS-authored recipe through
;; `gnu-build-system` (Guile build-system + a Guile builder), this lowers the SAME
;; recipe through a raw `derivation` whose BUILDER is the td-builder binary
;; (`td-builder autotools-build`, builder/src/build.rs). So:
;;   • gnu-build-system is GONE — the phase logic (unpack/configure/make/install)
;;     is td's Rust;
;;   • build-time Guile is GONE — the derivation's builder is a native binary, not
;;     `guile`;
;;   • guix still CONSTRUCTS the .drv (this `derivation` call) — the scope the
;;     human fixed (2026-06-13): replace gnu-build-system, keep guix for .drv
;;     construction.
;; The toolchain inputs (gcc-toolchain, make, …) stay Guix's — retired LAST (§5:
;; seed external, no full-source bootstrap). They are the build environment, not
;; the recipe; the recipe (hello) still comes from the TS surface.
;;
;; The differential oracle is the corpus `hello` (§2.5 / prime directive 4), but
;; an own-builder output has a DIFFERENT store path (different derivation, and
;; hello bakes $out in), so equivalence is proven BEHAVIORALLY (the `td-build`
;; rung runs it), not by NAR hash — the "behaviorally equal where a recipe
;; legitimately differs" case named in the §7.1 corpus-independence entry.

(define-module (system td-build)
  #:use-module (guix)
  #:use-module (guix packages)
  #:use-module (guix derivations)
  #:use-module (guix store)
  #:use-module (guix monads)
  #:use-module (guix gexp)
  #:use-module (guix download)
  #:use-module (gnu packages)
  #:use-module (system td-builder)
  #:use-module (ice-9 format)
  #:use-module (json)                    ;re-serialize the recipe's phases for TD_PHASES
  #:export (%td-build-tool-names
            td-rust-build-derivation
            td-build-components
            write-td-build-spec
            td-rust-selfhost-derivation))

;; The build environment: the Guix toolchain (retired last). gcc-toolchain
;; bundles gcc + glibc + binutils + ld-wrapper with the bin/include/lib layout
;; the Rust set-paths phase expects; the rest are the usual autotools helpers.
(define %td-build-tool-names
  '("gcc-toolchain" "make" "bash" "tar" "gzip" "bzip2" "xz"
    "coreutils" "sed" "grep" "gawk" "findutils" "diffutils" "file" "patch"))

(define (field alist key)
  (let ((p (assoc key alist)))
    (unless p (error "td-build: recipe JSON missing field" key))
    (cdr p)))

;; INPUT RESOLUTION (stays Guix's, retired last — §5): resolve RECIPE-ALIST's
;; source + the toolchain to their derivations/paths, and return the raw
;; components of the build derivation — WITHOUT assembling a `.drv`. Both
;; `td-rust-build-derivation` (the guix-`(derivation …)` oracle) and
;; `write-td-build-spec` (the td-assembled path) start here, so they resolve
;; identical inputs.
(define* (td-build-components store recipe-alist #:key (configure-flags "")
                              (resolved-dep-paths #f))
  (let* ((name    (field recipe-alist "name"))
         (version (field recipe-alist "version"))
         (source  (field recipe-alist "source"))
         ;; The upstream URI is a single URL or — for a mirror-list source like
         ;; pkg-config's — a JSON array (guile-json yields a vector). url-fetch
         ;; accepts a string OR a list; convert a vector to a list (same as
         ;; system/td-recipe.scm's recipe-uri).
         (uri     (let ((u (field source "uri")))
                    (if (vector? u) (vector->list u) u)))
         (hash    (field source "sha256"))
         (full    (string-append name "-" version))
         ;; The upstream source — a declared fixed-output url-fetch, same offline
         ;; contract as everywhere else (no `(gnu packages …)` lookup of hello).
         (src-origin (origin (method url-fetch) (uri uri)
                             (sha256 (base32 hash))))
         (src-drv  (run-with-store store (lower-object src-origin)))
         (src-path (derivation->output-path src-drv))
         ;; The builder: td-builder's Rust binary.
         (tb-drv   (package-derivation store td-builder #:graft? #f))
         (builder  (string-append (derivation->output-path tb-drv) "/bin/td-builder"))
         ;; The toolchain inputs (ungrafted — a behavioral build needs no grafts,
         ;; and it keeps lowering build-free).
         (tool-drvs (map (lambda (spec)
                           (package-derivation store (specification->package spec)
                                               #:graft? #f))
                         %td-build-tool-names))
         (tool-outs (map derivation->output-path tool-drvs))
         ;; The recipe's declared build INPUTS (dependencies) — resolved from the
         ;; corpus by name, exactly as system/td-recipe.scm does (input resolution
         ;; stays Guix's, retired LAST — §5). Their include/lib dirs feed the Rust
         ;; set-paths phase via TD_INPUTS, so td's own builder can build a package
         ;; that links real dependencies. Optional: a leaf recipe (hello) declares
         ;; none and lowers exactly as before (so the hello-based td-drv-* rungs are
         ;; untouched).
         (dep-names (let ((v (assoc-ref recipe-alist "inputs")))
                      (if v (vector->list v) '())))
         ;; INPUT-RESOLUTION SWAP (Inc.2): when RESOLVED-DEP-PATHS is supplied — by
         ;; `td-builder resolve` over the pinned lock, NOT Guile — the recipe's
         ;; declared deps come from td's resolution as input-SOURCES (already-realized
         ;; store paths), so NO `specification->package` runs for the deps. Default
         ;; (#f): resolve from the corpus (Guile, retired LAST §5) as input-derivations.
         ;; The toolchain stays Guile either way (retired even later).
         (swap?    (and resolved-dep-paths (pair? resolved-dep-paths)))
         (dep-drvs (if swap? '()
                       (map (lambda (spec)
                              (package-derivation store (specification->package spec)
                                                  #:graft? #f))
                            dep-names)))
         (dep-outs (if swap? resolved-dep-paths (map derivation->output-path dep-drvs)))
         (dep-srcs (if swap? resolved-dep-paths '()))
         (input-drvs (append (list src-drv tb-drv) tool-drvs dep-drvs))
         ;; The recipe's custom build PHASES, re-serialized to JSON for td's OWN
         ;; phase runner (builder/src/build.rs applies them after unpack — no
         ;; gnu-build-system, no Guile in the build). Empty when the recipe
         ;; declares none, so leaf recipes (hello/nano) are unchanged.
         (phases   (let ((p (assoc-ref recipe-alist "phases")))
                     (if p (scm->json-string p) ""))))
    (list (cons 'name full)
          (cons 'system "x86_64-linux")
          (cons 'builder builder)
          (cons 'args (list "autotools-build"))
          (cons 'input-drvs input-drvs)
          (cons 'input-srcs dep-srcs)
          (cons 'env `(("TD_SRC"             . ,src-path)
                       ("TD_INPUTS"          . ,(string-join (append tool-outs dep-outs) ":"))
                       ("TD_CONFIGURE_FLAGS" . ,configure-flags)
                       ("TD_PHASES"          . ,phases))))))

(define* (td-rust-build-derivation store recipe-alist #:key (configure-flags "")
                                   (resolved-dep-paths #f))
  "Return a derivation that builds RECIPE-ALIST with `td-builder autotools-build`
as the builder, via guix's `(derivation …)`.  With RESOLVED-DEP-PATHS (the
input-resolution swap, Inc.2) the recipe's deps are td-resolved input-sources
rather than Guile-resolved input-derivations."
  (let* ((c (td-build-components store recipe-alist #:configure-flags configure-flags
                                 #:resolved-dep-paths resolved-dep-paths)))
    (derivation store (assq-ref c 'name) (assq-ref c 'builder) (assq-ref c 'args)
                #:inputs (map (lambda (d) (derivation-input d '("out")))
                              (assq-ref c 'input-drvs))
                #:sources (assq-ref c 'input-srcs)
                #:env-vars (assq-ref c 'env)
                #:system (assq-ref c 'system))))

;; Emit the raw build-derivation SPEC (a line-based format the zero-dep Rust
;; assembler parses) — the inputs resolved above, WITHOUT calling `(derivation …)`.
;; td-builder `drv-assemble` turns this into the byte-identical `.drv`, computing
;; output paths + the ordering itself. This is what removes the last guile
;; `(derivation …)` from the build path; input RESOLUTION stays here (§5).
(define* (write-td-build-spec store recipe-alist #:key (configure-flags "")
                              (port (current-output-port)))
  (let ((c (td-build-components store recipe-alist #:configure-flags configure-flags)))
    (format port "name ~a~%" (assq-ref c 'name))
    (format port "system ~a~%" (assq-ref c 'system))
    (format port "builder ~a~%" (assq-ref c 'builder))
    (for-each (lambda (a) (format port "arg ~a~%" a)) (assq-ref c 'args))
    (for-each (lambda (d) (format port "input-drv ~a out~%" (derivation-file-name d)))
              (assq-ref c 'input-drvs))
    (for-each (lambda (kv) (format port "env ~a=~a~%" (car kv) (cdr kv)))
              (assq-ref c 'env))))

;;;
;;; rust-build — td's OWN cargo build path (the cargo-build-system replacement),
;;; proven by SELF-HOSTING: build td-builder ITSELF with td's `rust-build` runner
;;; (builder/src/build.rs `run_rust`). The build LOGIC is td's Rust — no
;;; gnu-build-system, no Guix cargo-build-system in the build — and the rustc/
;;; cargo/gcc seed stays EXTERNAL (§5, retired last), exactly as the autotools
;;; path keeps gcc-toolchain external. The SAME %builder-source the
;;; cargo-build-system `td-builder` package uses is built here a SECOND way, so
;;; one source lowers two ways: the durable legs (the output runs; td-builder
;;; check's double-build agrees it is reproducible) are what we keep; guix's
;;; cargo-build-system build is the removable migration oracle (it legitimately
;;; lands at a DIFFERENT path — different flags — so equivalence is BEHAVIORAL).
;;;

;; The Rust build seed: rust supplies rustc (its `out`) + cargo (its `cargo`
;; output); gcc-toolchain is the linker (Guix's ld-wrapper → RUNPATH) + libc;
;; coreutils/bash are the shell tools `run_rust` invokes (cp/chmod, run_cmd's
;; bash). Retired LAST (§5), same posture as %td-build-tool-names.
(define %td-rust-seed-names '("gcc-toolchain" "coreutils" "bash"))

(define (td-rust-selfhost-derivation store)
  "Return a derivation that builds td-builder from %builder-source via td's OWN
`rust-build` runner.  Its builder is the guix-built td-builder binary (the
bootstrap builder); its output is a td-built td-builder.  Input RESOLUTION (the
rust seed) stays Guix's, retired last (§5)."
  (let* (;; %builder-source is a local-file — it lowers to an interned store PATH
         ;; (added directly, not built), so it is an input-SOURCE, not an
         ;; input-derivation (cf. the url-fetch ORIGIN in td-build-components,
         ;; which DOES lower to a fixed-output derivation).
         (src-path  (run-with-store store (lower-object %builder-source)))
         (tb-drv    (package-derivation store td-builder #:graft? #f))
         (builder   (string-append (derivation->output-path tb-drv) "/bin/td-builder"))
         (rust-drv  (package-derivation store (specification->package "rust") #:graft? #f))
         (rustc-out (derivation->output-path rust-drv "out"))
         (cargo-out (derivation->output-path rust-drv "cargo"))
         (seed-drvs (map (lambda (s)
                           (package-derivation store (specification->package s) #:graft? #f))
                         %td-rust-seed-names))
         (seed-outs (map derivation->output-path seed-drvs))
         (inputs    (string-join (append (list rustc-out cargo-out) seed-outs) ":")))
    (derivation store (string-append "td-builder-rust-" (package-version td-builder))
                builder (list "rust-build")
                #:inputs (cons* (derivation-input tb-drv '("out"))
                                ;; rust contributes BOTH outputs: rustc (out) + cargo.
                                ;; Sub-derivation outputs MUST be in canonical SORTED
                                ;; order ("cargo" < "out") or the daemon recomputes a
                                ;; different drv hash and rejects it ("incorrect output").
                                (derivation-input rust-drv '("cargo" "out"))
                                (map (lambda (d) (derivation-input d '("out"))) seed-drvs))
                #:sources (list src-path)
                #:env-vars `(("TD_SRC"       . ,src-path)
                             ("TD_INPUTS"    . ,inputs)
                             ("TD_RUST_BINS" . "td-builder"))
                #:system "x86_64-linux")))
