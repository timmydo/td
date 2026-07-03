;; tests/eval.scm — the fast fail-fast "config eval" rung (DESIGN.md §1.1 step 1).
;;
;; Load every LOAD-BEARING declaration module so a syntax or binding error is
;; caught in well under a second, before any expensive build. The point of this
;; rung is to go RED on a broken module — so it MUST be run as a script
;; (`guix repl FILE`), whose process exit status reflects an uncaught error. The
;; old rung piped this into `guix repl` via STDIN, which *always exits 0* (it
;; swallows the script's status — the same documented trap the `test` rungs
;; avoid). A broken module therefore passed `eval` green; verified by piping an
;; intentional `(error …)` and observing exit 0. Run as a FILE, the identical
;; error exits 1.
;;
;; Only two system modules remain since the guix-system museum tier was retired
;; (human direction 2026-07-02): td-builder (check.sh's outer-sandbox prelude
;; still realizes it) and td-build (the drv fixtures for the realize/hermetic/
;; daemon gates still lower through it, retired with the remaining fixtures).
(use-modules (system td-builder)
             (system td-build))

(display "eval ok\n")
