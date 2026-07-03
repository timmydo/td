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
;; lock file — header + these entries:
;;
;;   td-system              <out>  the system's output store path (the closure root)
;;   td-system-drv          <drv>  the derivation that produces it (provenance for
;;       the later lowering slices — e.g. the td-builder S4 / rootless clusters
;;       need the deriver; no gate consumes it yet, and it is unverified)
;;   input-sha256-<file>    <hex>  sha256 of each repo-local LOWERING INPUT —
;;       channels.scm (the pin every (gnu …) module resolves through) and the
;;       system declaration modules td-system loads (system/td.scm,
;;       system/td-hardening.scm). The consume side (td_system_closure,
;;       tests/td-system-lib.sh) re-hashes the LIVE files every gate run and reds on
;;       any mismatch, so a channel bump OR an oracle re-baseline cannot leave a
;;       stale pinned root passing silently — staleness detection is
;;       input-anchored, not a heuristic.
;;   closure-count / closure-sha256   appended by tools/td-system-lock-regen.sh
;;       (NOT printed here — they come from `td-builder store-closure-scan`, the
;;       same scan the gates run): the pinned membership of the runtime closure,
;;       so a truncated (partially GC'd/imported store) or grown gate-time scan
;;       reds instead of packing different image bytes per host. The count is a
;;       human diagnostic; the hash is the assertion.
;;
;; This tool prints the lock BODY ONLY — regenerate via the driver, which also
;; appends the closure-count/closure-sha256 pins (td's own scan) and swaps the
;; file in ATOMICALLY (a failed capture can never truncate the good lock):
;;
;;   sh tools/td-system-lock-regen.sh
;;
;; Run ON A CHANNEL BUMP or a td-system/td-hardening re-baseline (an exclusive
;; landing, like DIGESTS.md), host-side on a guix-provisioned machine.
;;
;; Run as a repl SCRIPT (`guix repl FILE`), never piped via STDIN: guix repl
;; reading from STDIN always exits 0 (it swallows the script's status — the
;; documented trap the eval gate avoids), while script mode propagates a load
;; error as a non-zero exit the regen driver's `set -e` honors.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (guix base16)
             (gcrypt hash)
             (gnu)
             (gnu system)
             (system td)
             (ice-9 format))

;; The repo-local files the lowering is a function of (beyond the channel pin
;; itself): the system declaration and the module it loads. (system td) imports
;; only (system td-hardening) from the repo; everything else comes from the
;; pinned channel, which channels.scm covers.
(define lowering-inputs
  '("channels.scm" "system/td.scm" "system/td-hardening.scm"))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading — guix repl
  ;; ignores GUIX_BUILD_OPTIONS, so these must be set here, not in the env (the
  ;; same wiring check.sh documents for the loop's own repl invocations).
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (let* ((drv (run-with-store store (operating-system-derivation td-system)))
         (out (derivation->output-path drv)))
    (build-derivations store (list drv))
    (display "\
# tests/td-system.lock — PINNED lowering of the FROZEN td system (system/td.scm
# td-system) at the pinned channel (channels.scm): the store path the system
# lowers to, the derivation that produces it, the sha256 of each lowering INPUT,
# and the count/sha256 of the scanned runtime closure (see
# tests/td-system-lock.scm, the capture tool, which documents each entry).
# Consumed by the loop with NO guix process (td_system_closure in
# tests/td-system-lib.sh; gates 120-oci / 135-oci-load). PINNED ARTIFACT —
# regenerate on a channel bump or td-system re-baseline (atomic driver):
#   sh tools/td-system-lock-regen.sh
")
    (format #t "td-system ~a~%" out)
    (format #t "td-system-drv ~a~%" (derivation-file-name drv))
    (for-each (lambda (f)
                (format #t "input-sha256-~a ~a~%"
                        f (bytevector->base16-string (file-sha256 f))))
              lowering-inputs)))
