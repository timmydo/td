;; ci/channel-instance-drv.scm — lower the pinned channel instance (the exact
;; guix `guix time-machine -C channels.scm` runs) and print its derivation and
;; output path. The CI store image imports this closure so the runner's
;; time-machine is the same warm, offline no-op it is on a dev box whose host
;; guix matches the pin. With a warm ~/.cache/guix this never touches the
;; network (latest-channel-instances resolves the pinned commit from the
;; cached checkout).
(use-modules (guix) (guix store) (guix channels) (guix derivations))

(with-store store
  (let* ((channels (load (string-append (getcwd) "/channels.scm")))
         (instances (latest-channel-instances store channels)))
    (let ((drv (run-with-store store (channel-instances->derivation instances))))
      (format #t "CHANNEL_DRV=~a~%" (derivation-file-name drv))
      (format #t "CHANNEL_OUT=~a~%" (derivation->output-path drv "out")))))
