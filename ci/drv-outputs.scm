;; ci/drv-outputs.scm — read derivation file names (one per line) from the
;; file named by $DRVLIST and print every output path that is currently VALID
;; in the store. Used by ci/build-ci-image.sh: a warm store that just ran a
;; green check has, by construction, exactly the valid outputs the check
;; needs, so "valid outputs of the check's drv closure" IS the warm-store
;; build closure to export (drv files alone carry recipes, not the built
;; inputs a `guix build --check` rebuild consumes).
(use-modules (guix) (guix store) (guix derivations) (ice-9 rdelim))

(with-store store
  (call-with-input-file (getenv "DRVLIST")
    (lambda (port)
      (let loop ((line (read-line port)))
        (unless (eof-object? line)
          (let ((drv (read-derivation-from-file line)))
            (for-each (lambda (out)
                        (let ((p (derivation->output-path drv (car out))))
                          (when (valid-path? store p)
                            (format #t "~a~%" p))))
                      (derivation-outputs drv)))
          (loop (read-line port)))))))
