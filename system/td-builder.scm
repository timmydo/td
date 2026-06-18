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
  #:export (td-builder
            ;; Exported so (system td-build) can build td-builder via td's OWN
            ;; rust-build runner from the SAME source the cargo-build-system
            ;; package uses — the self-hosting rust-build differential builds one
            ;; source two ways (guix cargo-build-system vs td's cargo runner).
            %builder-source))

;; The crate source: the ../builder tree (Cargo.toml, Cargo.lock, src/). A
;; relative local-file resolves against THIS file's directory, so it points at
;; the repo's top-level builder/. Exclude any local cargo build output (target/,
;; a stray .cargo) so a developer who ran cargo by hand cannot perturb the
;; derivation's hash — only the committed source decides it. Match the BASENAME,
;; not a "/target/" substring: select? sees each directory entry's path WITHOUT
;; a trailing slash, so a substring match would admit the (then-empty) target/
;; directory itself into the nar and the hash would differ between a clean tree
;; and one where cargo ran (review finding; probed against the pinned guix's
;; (guix serialization) write-file).
(define %builder-source
  (local-file "../builder" "td-builder-src"
              #:recursive? #t
              #:select? (lambda (file stat)
                          (not (member (basename file) '("target" ".cargo"))))))

(define td-builder
  (package
    (name "td-builder")
    (version "0.1.0")
    (source %builder-source)
    (build-system cargo-build-system)
    (arguments
     (list
      ;; No dependencies — nothing to vendor, so the build is offline by
      ;; construction (S2 keeps it that way: SHA-256 is hand-rolled in the
      ;; crate). Unit tests are real as of S2 (FIPS vectors, NAR framing/sort)
      ;; and run in the build — the S1 review round's reminder honored.
      #:cargo-inputs '()
      #:cargo-development-inputs '()
      #:tests? #t))
    (synopsis "td's own builder (S2: NAR serializer + SHA-256)")
    (description
     "td-builder is td's own builder: a Rust binary that will execute a
@code{.drv} in a user-namespace sandbox and register the output, proven
behaviorally equivalent to the pinned @code{guix-daemon}.  As of S2 it
compiles offline and reproducibly inside the check.sh sandbox (S1) and
serializes store items to NAR with a hash bit-for-bit equal to the daemon's
recorded one (@code{td-builder nar-hash}, the rung's S2 differential).")
    (home-page "https://github.com/timmydo/td")
    (license license:gpl3+)))
