;; tests/td-builder-s3-drvs.scm — S3 drvs + oracle facts for the td-builder
;; rung's build differential (plan/td-builder.md S3). Emits, for the Makefile
;; recipe to parse:
;;
;;   DIFF_DRV=...      the differential drv file (deterministic output)
;;   DIFF_OUT=...      its output path
;;   DIFF_HASH=...     the daemon's RECORDED NAR sha256 (base16) of that
;;                     output — the oracle hash, from its own DB
;;   DIFF_NARSIZE=...  the daemon's recorded NAR size
;;   DIFF_DERIVER=...  the daemon's recorded deriver
;;   DIFF_REF=...      one line per recorded reference, sorted
;;   DIFF_INPUT=...    each direct input's output path of the diff drv
;;   PROBE_DRV/PROBE_OUT/PROBE_INPUT — the isolation probe, as in
;;                     tests/rootless-drvs.scm
;;
;; The DIFF drv is the S3 differential subject: bit-deterministic, and its
;; output embeds store paths so the references-scan assert discriminates —
;; an input reference (#$%dep, both as symlink target and file contents) AND
;; a self-reference (#$output), plus an executable bit (the NAR-visible mode
;; bit). The daemon builds it HERE (build-derivations, like the S2 nar
;; script) so query-path-info returns the oracle's recorded facts; td-builder
;; rebuilds the same drv in its scratch store and must register equal fields
;; at the same path.
;;
;; The PROBE drv is rootless-drvs.scm's instrument, reused under the track
;; file's probe-vs-oracle caveat: its output records /proc/self/{uid,gid}_map
;; and is therefore namespace-dependent BY DESIGN — td-side build only,
;; never a differential subject, lives only in the discarded scratch store.

(use-modules (guix) (guix gexp) (guix derivations) (guix base16)
             (srfi srfi-1) (ice-9 match))

(define (input-output-paths drv)
  (append-map (lambda (input)
                (let ((idrv (derivation-input-derivation input)))
                  (map (lambda (out)
                         (derivation->output-path idrv out))
                       (derivation-input-sub-derivations input))))
              (derivation-inputs drv)))

(define %dep
  (computed-file "td-s3-dep"
    #~(call-with-output-file #$output
        (lambda (port) (display "td-s3 dep payload\n" port)))))

(define %diff
  (computed-file "td-s3-diff"
    #~(begin
        (mkdir #$output)
        ;; The input reference, twice over: a symlink target and file
        ;; contents (the scanner must see both via the NAR dump).
        (symlink #$%dep (string-append #$output "/dep-link"))
        (call-with-output-file (string-append #$output "/ref")
          (lambda (port) (display #$%dep port)))
        ;; The self-reference.
        (call-with-output-file (string-append #$output "/self")
          (lambda (port) (display #$output port)))
        ;; The NAR-visible executable bit.
        (let ((exe (string-append #$output "/run")))
          (call-with-output-file exe
            (lambda (port) (display "#!/bin/sh\n" port)))
          (chmod exe #o755)))))

(define %probe
  (computed-file "td-s3-isolation-probe"
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
         '("uid_map" "gid_map")))))

(with-store store
  ;; Offline contract, as in every sibling: no substitutes, no offloading.
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((diff-drv (run-with-store store (lower-object %diff)))
        (probe-drv (run-with-store store (lower-object %probe))))
    ;; Oracle-build the diff drv so the daemon records its facts.
    (build-derivations store (list diff-drv))
    (let* ((out (derivation->output-path diff-drv))
           (info (query-path-info store out)))
      (format #t "DIFF_DRV=~a~%" (derivation-file-name diff-drv))
      (format #t "DIFF_OUT=~a~%" out)
      (format #t "DIFF_HASH=~a~%"
              (bytevector->base16-string (path-info-hash info)))
      (format #t "DIFF_NARSIZE=~a~%" (path-info-nar-size info))
      (format #t "DIFF_DERIVER=~a~%" (path-info-deriver info))
      (for-each (lambda (r) (format #t "DIFF_REF=~a~%" r))
                (sort (path-info-references info) string<?))
      (for-each (lambda (p) (format #t "DIFF_INPUT=~a~%" p))
                (input-output-paths diff-drv)))
    (format #t "PROBE_DRV=~a~%" (derivation-file-name probe-drv))
    (format #t "PROBE_OUT=~a~%" (derivation->output-path probe-drv))
    (for-each (lambda (p) (format #t "PROBE_INPUT=~a~%" p))
              (input-output-paths probe-drv))))
