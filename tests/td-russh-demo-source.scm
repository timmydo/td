;; tests/td-russh-demo-source.scm — intern the tests/russh-demo crate tree and print
;; its store path as `SRC=<path>`, for the rust-russh gate's lock. Source PREP only
;; (the daemon registers the tree so `td-builder realize` can stage it); the build
;; itself is td's Rust (build-recipe / run_rust), no Guile. Mirrors
;; tests/td-vendor-demo-source.scm. `target`/`.cargo` excluded so a stray build dir
;; can never perturb the source hash.
(use-modules (guix)
             (guix store)
             (guix monads))

(define %russh-demo-source
  (local-file "russh-demo" "td-russh-demo-src"
              #:recursive? #t
              #:select? (lambda (file stat)
                          (not (member (basename file) '("target" ".cargo"))))))

(with-store store
  (format #t "SRC=~a~%" (run-with-store store (lower-object %russh-demo-source))))
