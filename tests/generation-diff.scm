;; tests/generation-diff.scm — M10.1: each generation gets a distinct, selectable root.
;;
;; The M10 crux (M10-design.md "How a generation flows", P1): a generation must
;; boot its OWN root, not the shared `td-root`. If every generation mounted the
;; one fixed label, every GRUB entry would boot the SAME filesystem and rollback
;; would be a no-op. The typed `generation` field derives a distinct,
;; bootloader-selectable root label per generation; this rung pins that at the
;; derivation/record level (the full boot+rollback is M10.3). Self-discriminating,
;; like typed-diff.scm — it asserts every direction so a vacuous pass can't hide:
;;
;;   (a) CONVERGE   — generation #f (the default) keeps the system byte-identical
;;                    to the frozen oracle, whose root is still the shared `td-root`.
;;   (b) DISTINCT   — two different generations lower to DIFFERENT root labels AND
;;                    DIFFERENT system derivations: they cannot share a filesystem.
;;   (c) NOT-SHARED — a generation's root label is NOT `td-root` and IS the actual
;;                    root file-system device the boot path mounts, so the
;;                    bootloader selects THAT generation's root, not the base one.
;;
;; Run as a script so the process exit status is the result (see typed-diff.scm).
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (gnu system file-systems)
             (srfi srfi-1)
             (ice-9 format)
             (system td)
             (system td-typed))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading — `guix
  ;; repl` ignores GUIX_BUILD_OPTIONS, so set it here (see typed-diff.scm).
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (system-drv os)
    (derivation-file-name
     (run-with-store store (operating-system-derivation os))))

  ;; The td root fs is the only one whose device is a <file-system-label>
  ;; (%base-file-systems use paths / "none"); robust under any perturbation.
  (define (root-label os)
    (let ((fs (find (lambda (fs) (file-system-label? (file-system-device fs)))
                    (operating-system-file-systems os))))
      (and fs (file-system-label->string (file-system-device fs)))))

  (let* ((oracle      (system-drv td-system))
         (default-os  (td-config->operating-system (td-config)))
         (gen1-os     (td-config->operating-system (td-config #:generation 1)))
         (gen2-os     (td-config->operating-system (td-config #:generation 2)))
         (default-drv (system-drv default-os))
         (default-root (root-label default-os))
         (gen1-root   (root-label gen1-os))
         (gen2-root   (root-label gen2-os))
         (gen1-drv    (system-drv gen1-os))
         (gen2-drv    (system-drv gen2-os))
         ;; (a) the default (no generation) still IS the oracle, shared td-root
         (converge?       (string=? oracle default-drv))
         (default-shared? (and default-root (string=? default-root "td-root")))
         ;; (b) two generations differ in both root label and system drv
         (distinct-label? (and gen1-root gen2-root
                               (not (string=? gen1-root gen2-root))))
         (distinct-drv?   (not (string=? gen1-drv gen2-drv)))
         ;; (c) a generation's root is NOT the shared base root
         (not-shared?     (and gen1-root (not (string=? gen1-root "td-root")))))

    (format #t "~%== M10.1 generation root: distinct, selectable, non-shared ==~%")
    (format #t "  oracle system drv      : ~a~%" oracle)
    (format #t "  default (gen #f) root  : ~a~%" default-root)
    (format #t "  generation 1 root      : ~a~%" gen1-root)
    (format #t "  generation 2 root      : ~a~%" gen2-root)
    (format #t "~%  (a) converge   (default == oracle)       : ~a~%" converge?)
    (format #t "      default root is shared td-root         : ~a~%" default-shared?)
    (format #t "  (b) distinct   (gen1 root != gen2 root)    : ~a~%" distinct-label?)
    (format #t "      distinct   (gen1 drv  != gen2 drv)     : ~a~%" distinct-drv?)
    (format #t "  (c) not-shared (gen1 root != td-root)      : ~a~%~%" not-shared?)

    (cond
     ((not (and converge? default-shared?))
      (format #t "FAIL: the generation mechanism disturbed the DEFAULT system — \
generation #f must still lower to the oracle's shared td-root.~%")
      (exit 1))
     ((not (and distinct-label? distinct-drv?))
      (format #t "FAIL: two different generations did NOT get distinct roots — \
they would boot the same filesystem and rollback would be a no-op.~%")
      (exit 1))
     ((not not-shared?)
      (format #t "FAIL: a generation's root is still the shared td-root — it is \
not a distinct, selectable per-generation root.~%")
      (exit 1))
     (else
      (format #t "PASS: generation #f converges to the shared-root oracle, while \
each generation gets a distinct, non-shared root label that is the actual root \
device the boot path mounts — so the bootloader selects that generation's root.~%")
      (exit 0)))))
