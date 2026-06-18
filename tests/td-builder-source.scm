;; tests/td-builder-source.scm — intern the CURRENT builder/ tree and print its
;; store path as `SRC=<path>`, for the rust-build gate's self-host lock.
;;
;; This is the ONE bit of SOURCE PREP the self-host still does through guix: the
;; daemon must register the live tree in its store db so `td-builder realize` can
;; stage it (the source changes every edit, so it can't be a pinned lock line). It
;; does NOT construct the build — the .drv is assembled by td (store::assemble_drv)
;; and realized daemon-free (realize_drv) via `td-builder build-recipe`. It is the
;; same `%builder-source` local-file the td-builder package uses, so one source
;; lowers to one path. Analogous to guix realizing nano's source tarball in the
;; nano-no-guix PREP; the build PATH itself stays guix/Guile-free (§5 seed).
(use-modules (system td-builder)
             (guix)
             (guix store)
             (guix monads))

(with-store store
  (format #t "SRC=~a~%" (run-with-store store (lower-object %builder-source))))
