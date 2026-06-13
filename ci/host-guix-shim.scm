;; ci/host-guix-shim.scm — build the runner's "host guix": a store item
;; shaped like a dev box's system guix package (bin/guix + bin/guix-daemon as
;; REAL files), aggregated from the imported channel instance.
;;
;; Why: check.sh and the rootless rung derive container-visible paths from
;; `readlink -f $(command -v guix)`: its dirname becomes the PATH entry
;; inside the hermetic container (check.sh), and its dirname^2 a package
;; whose closure `guix gc -R` stages (Makefile rootless recipe, which also
;; needs guix-daemon next to guix). The imported channel-instance profile
;; cannot serve: its bin/guix is a symlink to the bare ...-guix-command FILE,
;; so both deriveds collapse to /gnu/store itself (second live CI run failed
;; exactly so). COPYING (not symlinking) the instance's guix-command and
;; guix-daemon scripts into one bin/ reproduces the dev-box shape, and
;; bin/guix still `guix describe`s the PIN (it IS the channel instance's
;; command, byte-identical content).
;;
;; This uses the low-level (derivation ...) API on purpose: the two scripts
;; must enter the derivation as #:sources — LITERAL store paths — so the
;; output reference scanner recognises their embedded references (guile,
;; module directories) in the copied bytes and records them. The gexp route
;; (local-file) re-interns the files under fresh names, the originals never
;; become inputs, and the shim comes out with an EMPTY reference set — its
;; gc closure would stage a store where the copied shebangs dangle (caught
;; by local verification before this ever reached CI).
;;
;; Usage: CHANNEL_OUT=/gnu/store/...-profile guix repl ci/host-guix-shim.scm
;; Builds locally (substitutes/offload off) and prints HOST_GUIX=<out path>.

(use-modules (guix store) (guix derivations) (ice-9 rdelim))

(define channel-out
  (or (getenv "CHANNEL_OUT") (error "CHANNEL_OUT is unset")))

(define (instance-binary name)
  ;; The profile's bin/NAME is a single-hop symlink to the real store file.
  (readlink (string-append channel-out "/bin/" name)))

(define guix-cmd (instance-binary "guix"))
(define guix-daemon (instance-binary "guix-daemon"))

;; Builder interpreter: the guile from the daemon wrapper's own shebang —
;; guaranteed valid in the imported store (it is a reference of the
;; instance's closure).
(define guile
  (call-with-input-file guix-daemon
    (lambda (port)
      (let ((shebang (read-line port)))
        (unless (string-prefix? "#!" shebang)
          (error "no shebang in" guix-daemon))
        (car (string-split (substring shebang 2) #\space))))))

(define (store-item path)
  ;; #:sources wants store item ROOTS; guile's path reaches inside its item.
  (let ((rest (substring path (string-length "/gnu/store/"))))
    (string-append "/gnu/store/" (car (string-split rest #\/)))))

(define builder-exp
  `(let* ((out (getenv "out"))
          (bin (string-append out "/bin")))
     (mkdir out)
     (mkdir bin)
     (copy-file ,guix-cmd (string-append bin "/guix"))
     (copy-file ,guix-daemon (string-append bin "/guix-daemon"))
     (chmod (string-append bin "/guix") #o555)
     (chmod (string-append bin "/guix-daemon") #o555)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((drv (derivation store "host-guix" guile
                         (list "--no-auto-compile" "-c"
                               (object->string builder-exp))
                         #:sources (list (store-item guile)
                                         guix-cmd guix-daemon)
                         #:local-build? #t)))
    (build-derivations store (list drv))
    (format #t "HOST_GUIX=~a~%" (derivation->output-path drv "out"))))
