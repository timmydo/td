;; system/td-builder.scm — the td-builder package (S1 toolchain probe).
;;
;; Defines the td-builder Rust binary as a Guix package built with the pinned
;; channel's cargo-build-system + rust toolchain (DESIGN §5: the host store may
;; be warmed with substitutes for that closure; the loop itself stays
;; offline/no-substitutes — warm store in, nothing fetched inside).
;;
;; S1's deliverable: this package BUILDS OFFLINE inside the check.sh sandbox and
;; `guix build --check` reproduces it bit-for-bit (prime directive 1 — a
;; non-reproducible builder is a FAILING test). The binary is the hello-world
;; skeleton in ../builder/src/main.rs that the `td-builder` rung runs to prove
;; the toolchain produced a working executable. Later sub-tasks (S2 NAR
;; serializer, S3 drv-parse + userns build, S4 the differential rung) grow the
;; crate; this package definition carries them with no structural change.
;;
;; FSDG: the crate has NO dependencies at S1, so nothing is vendored; any future
;; crate must be free and come through the pinned channel (plan/td-builder.md).

(define-module (system td-builder)
  #:use-module (guix packages)
  #:use-module (guix gexp)
  #:use-module (guix build-system cargo)
  #:use-module ((guix licenses) #:prefix license:)
  #:export (td-builder))

;; The crate source: the ../builder tree (Cargo.toml, Cargo.lock, src/). A
;; relative local-file resolves against THIS file's directory, so it points at
;; the repo's top-level builder/. Exclude any local cargo build output (target/,
;; a stray .cargo) so a developer who ran cargo by hand cannot perturb the
;; derivation's hash — only the committed source decides it.
(define %builder-source
  (local-file "../builder" "td-builder-src"
              #:recursive? #t
              #:select? (lambda (file stat)
                          (not (or (string-contains file "/target/")
                                   (string-contains file "/.cargo/"))))))

(define td-builder
  (package
    (name "td-builder")
    (version "0.1.0")
    (source %builder-source)
    (build-system cargo-build-system)
    (arguments
     (list
      ;; No dependencies at S1 — nothing to vendor, so the build is offline by
      ;; construction. The skeleton has no unit tests yet; the `td-builder` rung
      ;; provides the behavioral assertion (it RUNS the binary), so skip the
      ;; cargo test phase rather than assert on an empty suite.
      #:cargo-inputs '()
      #:cargo-development-inputs '()
      #:tests? #f))
    (synopsis "td's own builder (S1 toolchain probe)")
    (description
     "td-builder is td's own builder: a Rust binary that will execute a
@code{.drv} in a user-namespace sandbox and register the output, proven
behaviorally equivalent to the pinned @code{guix-daemon}.  At the S1 milestone
it is a hello-world skeleton that proves the pinned channel's Rust toolchain
compiles td-builder offline and reproducibly inside the check.sh sandbox.")
    (home-page "https://github.com/timmydo/td")
    (license license:gpl3+)))
