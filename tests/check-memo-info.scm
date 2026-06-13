;; tests/check-memo-info.scm — daemon-DB facts for the check-memo helper
;; (plan/check-memo.md constraint 5: a hit is a cheap assertion, not a
;; no-op). TD_DRVS holds space-separated .drv store paths; for each output
;; of each drv this prints ONE line:
;;
;;   INFO=<drv> <name> <path> <base16 nar sha256> <nar size>   (valid)
;;   INVALID=<drv> <name> <path>                                (not valid)
;;
;; Hash and size come from query-path-info — the daemon's OWN records, the
;; same oracle the td-builder rung compares against (tests/td-builder-nar.scm)
;; — never recomputed here. Run as a repl SCRIPT (not piped) so a thrown
;; error exits nonzero honestly; the helper treats that as a miss, so this
;; script failing can only cause the REAL --check to run (fail closed).
(use-modules (guix store)
             (guix derivations)
             (guix base16)
             (ice-9 format))

(let ((drvs (string-tokenize (or (getenv "TD_DRVS") ""))))
  (when (null? drvs)
    (format (current-error-port) "check-memo-info: TD_DRVS is empty~%")
    (exit 1))
  (with-store store
    ;; Offline contract, as in every sibling: no substitutes, no offloading.
    (set-build-options store #:use-substitutes? #f #:offload? #f)
    (for-each
     (lambda (drv-path)
       (let ((drv (read-derivation-from-file drv-path)))
         (for-each
          (lambda (out)
            (let ((name (car out))
                  (path (derivation-output-path (cdr out))))
              (if (valid-path? store path)
                  (let ((info (query-path-info store path)))
                    (format #t "INFO=~a ~a ~a ~a ~a~%"
                            drv-path name path
                            (bytevector->base16-string (path-info-hash info))
                            (path-info-nar-size info)))
                  (format #t "INVALID=~a ~a ~a~%" drv-path name path))))
          (derivation-outputs drv))))
     drvs)))
