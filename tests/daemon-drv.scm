;; tests/daemon-drv.scm — emit TWO distinct PROBE derivations for the build-daemon
;; gate (mk/gates/358, own-builder-daemon increment 7). Each probe's build just
;; writes a distinct marker file to its output, so the gate can confirm td's
;; persistent build daemon actually realized THAT request (not a replay) by reading
;; the marker back from the daemon-served output path. The daemon builds the same
;; derivations the daemon-free `realize` path would — the gate exercises serving
;; them over a Unix socket, one long-running daemon handling both.
;;
;; The daemon here materializes the guile closure inputs + proves the probes are
;; well-formed; the discriminating environment is td's daemon, exercised by the gate.
;;
;; Emits: DRV_A/OUT_A and DRV_B/OUT_B (the .drv file names + output paths).
(use-modules (guix) (guix gexp) (guix monads) (guix derivations))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (define (probe tag)
    (run-with-store store
      (gexp->derivation (string-append "td-daemon-probe-" tag)
        #~(begin
            (mkdir #$output)
            (call-with-output-file (string-append #$output "/marker")
              (lambda (p) (display #$(string-append "td-daemon-built:" tag) p)))))))
  (let ((a (probe "a")) (b (probe "b")))
    (build-derivations store (list a b))
    (format #t "DRV_A=~a~%OUT_A=~a~%DRV_B=~a~%OUT_B=~a~%"
            (derivation-file-name a) (derivation->output-path a)
            (derivation-file-name b) (derivation->output-path b))))
