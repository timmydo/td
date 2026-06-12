;; tests/td-builder-s4-drv.scm — S4 oracle facts for the td-builder rung's
;; system-image differential (plan/td-builder.md S4). Run via `guix repl`
;; with TD_IMAGE_DRV naming the qcow2 image drv (the `build` rung's subject),
;; AFTER the recipe has realized it through the root daemon, so
;; query-path-info returns the facts the ORACLE recorded in its own DB.
;; Emits, for the Makefile recipe to parse:
;;
;;   IMG_DRV=...      the image drv file
;;   IMG_OUT=...      its output path (the oracle artifact)
;;   IMG_HASH=...     the daemon's RECORDED NAR sha256 (base16)
;;   IMG_NARSIZE=...  the daemon's recorded NAR size
;;   IMG_DERIVER=...  the daemon's recorded deriver
;;   IMG_REF=...      one line per recorded reference, sorted (may be none —
;;                    the assert compares the SETS, empty included)
;;   IMG_INPUT=...    each direct input's output path of the image drv
;;
;; This is the acceptance subject: td-builder rebuilds the SAME drv in its
;; scratch store and must register equal fields at the same path. The shape
;; mirrors tests/td-builder-s3-drvs.scm (trivial subject) and
;; tests/rootless-drvs.scm (same image drv, rootless-daemon side).

(use-modules (guix) (guix derivations) (guix base16)
             (srfi srfi-1) (ice-9 match))

(define (input-output-paths drv)
  (append-map (lambda (input)
                (let ((idrv (derivation-input-derivation input)))
                  (map (lambda (out)
                         (derivation->output-path idrv out))
                       (derivation-input-sub-derivations input))))
              (derivation-inputs drv)))

(with-store store
  ;; Offline contract, as in every sibling: no substitutes, no offloading.
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let* ((img-drv (read-derivation-from-file (getenv "TD_IMAGE_DRV")))
         (out (derivation->output-path img-drv))
         (info (query-path-info store out)))
    (format #t "IMG_DRV=~a~%" (derivation-file-name img-drv))
    (format #t "IMG_OUT=~a~%" out)
    (format #t "IMG_HASH=~a~%"
            (bytevector->base16-string (path-info-hash info)))
    (format #t "IMG_NARSIZE=~a~%" (path-info-nar-size info))
    (format #t "IMG_DERIVER=~a~%" (path-info-deriver info))
    (for-each (lambda (r) (format #t "IMG_REF=~a~%" r))
              (sort (path-info-references info) string<?))
    (for-each (lambda (p) (format #t "IMG_INPUT=~a~%" p))
              (input-output-paths img-drv))))
