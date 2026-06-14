;; tests/resolve-lock.scm — the input-resolution ORACLE + lock generator (DESIGN
;; §7.1 move-off-Guile; "retire input resolution", additive equivalence first).
;;
;; Resolve a list of corpus package NAMES to their store output paths exactly as
;; system/td-build.scm does — `specification->package` -> `package-derivation`
;; (#:graft? #f, the "out" output) -> `derivation->output-path`. Prints one
;; `NAME<space>STORE-PATH` line per name (the pinned-lock format `td-builder
;; resolve` consumes).
;;
;; Two uses, ONE source of truth:
;;   • the `resolve` rung runs this to produce the LIVE oracle resolution, then
;;     compares it to `td-builder resolve` over the COMMITTED lock — so a stale
;;     lock (channel bumped, lock not regenerated) reds;
;;   • regenerating the committed lock IS running this and saving stdout.
;;
;; This is the RESOLVER — it stays Guile (the §5 toolchain, retired last); what
;; the increment moves to Rust is the lock CONSUMPTION (`td-builder resolve`).
;;
;; Names come from the command line:  guix repl -L . tests/resolve-lock.scm NAME...
;; (a leading `--` separator, if present, is skipped).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (gnu packages))

(define (names)
  ;; (command-line) = (script-path NAME ...); drop the script and an optional --.
  (let ((rest (cdr (command-line))))
    (if (and (pair? rest) (string=? (car rest) "--")) (cdr rest) rest)))

(let ((ns (names)))
  (when (null? ns)
    (format (current-error-port) "resolve-lock: no names given~%")
    (exit 2))
  (with-store store
    (set-build-options store #:use-substitutes? #f #:offload? #f)
    (for-each
     (lambda (name)
       (let* ((pkg (specification->package name))
              (drv (package-derivation store pkg #:graft? #f))
              (out (derivation->output-path drv)))
         (format #t "~a ~a~%" name out)))
     ns)))
