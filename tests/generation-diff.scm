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
;;   (c) NOT-SHARED — a generation's root label is NOT `td-root` and IS the data
;;                    device the boot path verifies and mounts (M11: the label
;;                    rides the dm-verity mapped device's source), so the
;;                    bootloader selects THAT generation's root, not the base one.
;;   (d) SEALED     — (M11) generation systems declare the sealed shape — tmpfs
;;                    "/", the store mounted read-only from /dev/mapper/td-root
;;                    (needed-for-boot), dm-verity in the initrd — and the
;;                    DEFAULT system does not (oracle scope, §2.6).
;;
;; Run as a script so the process exit status is the result (see typed-diff.scm).
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (gnu system file-systems)
             (gnu system mapped-devices)
             (srfi srfi-1)
             (ice-9 format)
             (system td)
             (system td-typed)
             (system td-verity))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading — `guix
  ;; repl` ignores GUIX_BUILD_OPTIONS, so set it here (see typed-diff.scm).
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (system-drv os)
    (derivation-file-name
     (run-with-store store (operating-system-derivation os))))

  ;; The root the boot path mounts. For a SHARED-root system the "/"
  ;; file-system's label device. For a GENERATION system (M11) "/" is a
  ;; tmpfs and the OS content comes from the dm-verity mapped device, whose
  ;; SOURCE is the per-generation data-device label — the label moved from
  ;; the root file-system to the mapped device, but it is still the one
  ;; bootloader-selectable, per-generation root identity this rung pins.
  (define (root-label os)
    (or (let ((md (find (lambda (md)
                          (file-system-label? (mapped-device-source md)))
                        (operating-system-mapped-devices os))))
          (and md (file-system-label->string (mapped-device-source md))))
        (let ((fs (find (lambda (fs)
                          (and (string=? "/" (file-system-mount-point fs))
                               (file-system-label?
                                (file-system-device fs))))
                        (operating-system-file-systems os))))
          (and fs (file-system-label->string (file-system-device fs))))))

  ;; M11 (d): the SEALED generation shape — "/" is a tmpfs, the store is
  ;; mounted read-only from the verity target, and the initrd loads
  ;; dm-verity. Structural (record-level), so a compiler that silently
  ;; dropped the sealing goes red here before any VM boots.
  (define (sealed-shape? os)
    (let ((root  (find (lambda (fs)
                         (string=? "/" (file-system-mount-point fs)))
                       (operating-system-file-systems os)))
          (store (find (lambda (fs)
                         (string=? "/gnu/store" (file-system-mount-point fs)))
                       (operating-system-file-systems os))))
      (and root (string=? (file-system-type root) "tmpfs")
           store
           (equal? (file-system-device store)
                   (string-append "/dev/mapper/" %td-verity-target))
           (memq 'read-only (file-system-flags store))
           (file-system-needed-for-boot? store)
           (->bool (member "dm-verity"
                           (operating-system-initrd-modules os))))))

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
         (not-shared?     (and gen1-root (not (string=? gen1-root "td-root"))))
         ;; (d) M11: generation systems are SEALED (tmpfs /, ro verity store,
         ;; dm-verity in the initrd); the default system is NOT (its / is the
         ;; plain labeled root) — self-discriminating in both directions.
         (gen-sealed?     (and (sealed-shape? gen1-os) (sealed-shape? gen2-os)))
         (default-plain?  (not (sealed-shape? default-os))))

    (format #t "~%== M10.1 generation root: distinct, selectable, non-shared ==~%")
    (format #t "  oracle system drv      : ~a~%" oracle)
    (format #t "  default (gen #f) root  : ~a~%" default-root)
    (format #t "  generation 1 root      : ~a~%" gen1-root)
    (format #t "  generation 2 root      : ~a~%" gen2-root)
    (format #t "~%  (a) converge   (default == oracle)       : ~a~%" converge?)
    (format #t "      default root is shared td-root         : ~a~%" default-shared?)
    (format #t "  (b) distinct   (gen1 root != gen2 root)    : ~a~%" distinct-label?)
    (format #t "      distinct   (gen1 drv  != gen2 drv)     : ~a~%" distinct-drv?)
    (format #t "  (c) not-shared (gen1 root != td-root)      : ~a~%" not-shared?)
    (format #t "  (d) sealed     (gens: tmpfs / + ro verity store + dm-verity initrd) : ~a~%" gen-sealed?)
    (format #t "      default stays plain (no sealing)       : ~a~%~%" default-plain?)

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
     ((not gen-sealed?)
      (format #t "FAIL: a generation system is not SEALED (M11) — expected a \
tmpfs /, the store mounted read-only from /dev/mapper/~a (needed-for-boot), \
and dm-verity among the initrd modules.~%" %td-verity-target)
      (exit 1))
     ((not default-plain?)
      (format #t "FAIL: the DEFAULT system acquired the sealed generation \
shape — generation #f must keep the plain shared labeled root (oracle scope, \
DESIGN §2.6).~%")
      (exit 1))
     (else
      (format #t "PASS: generation #f converges to the shared-root oracle, while \
each generation gets a distinct, non-shared root label (M11: carried by the \
dm-verity mapped device) that is the data device the boot path verifies and \
mounts, and the generation shape is SEALED (tmpfs /, read-only verity store, \
dm-verity initrd) while the default stays plain.~%")
      (exit 0)))))
