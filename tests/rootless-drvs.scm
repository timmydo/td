;; rootless rung lowering (run via `guix repl` with TD_IMAGE_DRV set).
;;
;; Emits, for the Makefile recipe to parse:
;;   IMG_OUT=...      the target image drv's output path (the oracle artifact)
;;   IMG_INPUT=...    each direct input's output path of the image drv
;;   PROBE_DRV=...    the isolation-probe derivation file
;;   PROBE_OUT=...    its output path (must be INVALID in the host store)
;;   PROBE_INPUT=...  each direct input's output path of the probe drv
;;
;; The probe derivation is DELIBERATELY environment-sensitive: its builder
;; copies /proc/self/uid_map and /proc/self/gid_map into its output, so the
;; output records whether the daemon that built it placed the build in a user
;; namespace (a root daemon's chroot build reads the identity map
;; "0 0 4294967295"; the rootless daemon's CLONE_NEWUSER build reads a
;; single-uid mapping). It is an INSTRUMENT, not an artifact: the rung builds
;; it only with the rootless daemon, never `--check`s it, and its output lives
;; only in the rung's scratch store, which is discarded — it never becomes a
;; valid path in the real store (prime directive 1 is about artifacts we keep).

(use-modules (guix) (guix gexp) (guix derivations) (srfi srfi-1) (ice-9 match))

(define (input-output-paths drv)
  (append-map (lambda (input)
                (let ((idrv (derivation-input-derivation input)))
                  (map (lambda (out)
                         (derivation->output-path idrv out))
                       (derivation-input-sub-derivations input))))
              (derivation-inputs drv)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let* ((img-drv (read-derivation-from-file (getenv "TD_IMAGE_DRV")))
         (probe-drv
          (run-with-store store
            (gexp->derivation "td-rootless-isolation-probe"
              #~(begin
                  (use-modules (ice-9 textual-ports))
                  (mkdir #$output)
                  (for-each
                   (lambda (f)
                     (call-with-output-file (string-append #$output "/" f)
                       (lambda (port)
                         (display (call-with-input-file
                                      (string-append "/proc/self/" f)
                                    get-string-all)
                                  port))))
                   '("uid_map" "gid_map")))))))
    (for-each (match-lambda
                ((name . output)
                 (format #t "IMG_OUT=~a~%" (derivation-output-path output))))
              (derivation-outputs img-drv))
    (for-each (lambda (p) (format #t "IMG_INPUT=~a~%" p))
              (input-output-paths img-drv))
    (format #t "PROBE_DRV=~a~%" (derivation-file-name probe-drv))
    (format #t "PROBE_OUT=~a~%" (derivation->output-path probe-drv))
    (for-each (lambda (p) (format #t "PROBE_INPUT=~a~%" p))
              (input-output-paths probe-drv))))
