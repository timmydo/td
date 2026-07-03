;; tests/td-system-lock.scm — CAPTURE tool for tests/td-system.lock (the pinned
;; lowering of the FROZEN td system). This is the Guile system lowering DEMOTED
;; from a per-gate loop step to a channel-bump regen tool: the loop's gates no
;; longer run `guix repl` to resolve the shipped system; they consume the lock
;; this tool writes, with td-builder doing the closure computation itself
;; (`resolve` + `store-closure-scan` — no guix process, no /var/guix/db read).
;;
;; It lowers the frozen oracle `td-system` (system/td.scm) exactly as
;; `guix system build` does (operating-system-derivation), REALIZES it (so the
;; consume side finds every closure member on disk), and prints the complete
;; lock file — header + two entries:
;;
;;   td-system      <out>   the system's output store path (the closure root)
;;   td-system-drv  <drv>   the derivation that produces it (provenance)
;;
;; Regenerate ON A CHANNEL BUMP (an exclusive landing, like DIGESTS.md), on a
;; guix-provisioned host at the pinned channel:
;;
;;   guix time-machine -C channels.scm -- repl -L . tests/td-system-lock.scm \
;;     > tests/td-system.lock
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (system td)
             (ice-9 format))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (let* ((drv (run-with-store store (operating-system-derivation td-system)))
         (out (derivation->output-path drv)))
    (build-derivations store (list drv))
    (format #t "# tests/td-system.lock — PINNED lowering of the FROZEN td system (system/td.scm~%")
    (format #t "# td-system) at the pinned channel (channels.scm): the store path the system~%")
    (format #t "# lowers to and the derivation that produces it. Consumed by the loop with NO~%")
    (format #t "# guix process: `td-builder resolve` reads it and `td-builder store-closure-scan`~%")
    (format #t "# computes the runtime closure from the pinned root by content-scan (gates~%")
    (format #t "# 120-oci / 135-oci-load). PINNED ARTIFACT — regenerate on a channel bump~%")
    (format #t "# (an exclusive landing, like DIGESTS.md):~%")
    (format #t "#   guix time-machine -C channels.scm -- repl -L . tests/td-system-lock.scm > tests/td-system.lock~%")
    (format #t "td-system ~a~%" out)
    (format #t "td-system-drv ~a~%" (derivation-file-name drv))))
