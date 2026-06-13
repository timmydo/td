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
;; hello bakes $out in), so equivalence is proven BEHAVIORALLY (the `gnu-build`
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
  #:export (%td-build-tool-names
            td-rust-build-derivation))

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

(define* (td-rust-build-derivation store recipe-alist #:key (configure-flags ""))
  "Return a derivation that builds RECIPE-ALIST (a TS-authored recipe: name,
version, source{uri,sha256}, buildSystem) with `td-builder autotools-build` as the
builder.  The recipe DATA is the TS surface's; this only wires the source +
toolchain inputs and hands them to td's Rust build logic."
  (let* ((name    (field recipe-alist "name"))
         (version (field recipe-alist "version"))
         (source  (field recipe-alist "source"))
         (uri     (field source "uri"))
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
         (input-drvs (cons src-drv (cons tb-drv tool-drvs))))
    (derivation store full builder (list "autotools-build")
                #:inputs (map (lambda (d) (derivation-input d '("out"))) input-drvs)
                #:env-vars `(("TD_SRC"             . ,src-path)
                             ("TD_INPUTS"          . ,(string-join tool-outs ":"))
                             ("TD_CONFIGURE_FLAGS" . ,configure-flags))
                #:system "x86_64-linux")))
