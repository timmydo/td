;; tests/td-daemon-instantiate.scm — instantiate a td-ASSEMBLED .drv into the
;; guix daemon store and have the DAEMON realize it. This is the td-artifact
;; bridge (plan/daemon-td-drv.md): td builds its artifacts in its OWN store
;; (daemon-free), but the system IMAGE is daemon-built, so a td-built artifact must
;; become daemon-valid to be referenced by the image. Given a .drv td assembled
;; with the daemon-valid GUIX-built td-builder as its builder (build-recipe WITHOUT
;; the stage0 override), every input-src (crates/source/builder) is daemon-valid, so
;; the daemon can build it — we just have to put the .drv into the daemon store.
;;
;; Usage:  guix repl -- tests/td-daemon-instantiate.scm DRV-FILE
;; Prints the realized, now-daemon-VALID output path(s), one per line.
;;
;; This is the retire-last Guile "lowering bridge" layer — the daemon interaction —
;; reused by the system lowering (td-config->operating-system) to ship td-built
;; userland (move off Guix: the build LOGIC is td's; the daemon only realizes the
;; deterministic .drv td produced).
(use-modules (guix store)
             (guix derivations)
             (ice-9 textual-ports)
             (ice-9 regex)
             (srfi srfi-1)
             (srfi srfi-26)
             (ice-9 match))

(define drv-file
  (match (command-line)
    ((_ f) f)
    (_ (begin (format (current-error-port)
                      "usage: guix repl -- td-daemon-instantiate.scm DRV-FILE~%")
              (exit 2)))))

(define content (call-with-input-file drv-file get-string-all))

;; The derivation's OUTPUT paths — the `("out","/gnu/store/…","","")` tuples. These
;; are produced, not dependencies, so they are NOT references of the .drv text.
(define out-paths
  (map (cut match:substring <> 1)
       (list-matches "\\(\"[a-z0-9_-]+\",\"(/gnu/store/[a-z0-9]{32}-[^\"]+)\"" content)))

;; Every store ROOT the .drv text mentions (`/gnu/store/<32hash>-<name>`, stopping
;; at the next slash/quote/paren/comma — sub-paths like `…/bin/td-builder` are NOT
;; valid store paths and must be normalized to their root, or add-text-to-store reds).
(define roots
  (delete-duplicates
   (map match:substring
        (list-matches "/gnu/store/[a-z0-9]{32}-[^/\",()]+" content))))

;; References of the .drv = its input store roots (everything it mentions minus the
;; outputs it produces). Filtered to those the daemon actually has.
(with-store store
  (let* ((refs  (lset-difference string=? roots out-paths))
         (valid (filter (cut valid-path? store <>) refs))
         (drv-path (add-text-to-store store (basename drv-file) content valid)))
    (format (current-error-port)
            "instantiated ~a (~a/~a refs daemon-valid); building via the daemon…~%"
            drv-path (length valid) (length refs))
    (build-derivations store (list drv-path))
    ;; Authoritative outputs from the now-in-store derivation.
    (let ((drv (read-derivation-from-file drv-path)))
      (for-each (lambda (o)
                  (let ((p (derivation-output-path (cdr o))))
                    (unless (valid-path? store p)
                      (format (current-error-port) "output not valid after build: ~a~%" p)
                      (exit 1))
                    (format #t "~a~%" p)))
                (derivation-outputs drv)))))
