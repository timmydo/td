;; system/td-ts.scm — the pinned TypeScript compiler input for the ts-frontend
;; track (DESIGN §7.1 Phase 1 of the §5 move-off-Guile goal).
;;
;; The §7.1 pipeline strips types TS->JS and type-checks the spec. The pinned
;; channel has `node` but NOT `typescript`/`tsc`, and its `rust-swc` 1.2.129
;; ships only a stub `swc_cli` (`swc compile` panics "not implemented"). So tsc
;; is brought in as a hash-pinned input and does BOTH jobs (human, 2026-06-13 —
;; plan/ts-frontend.md "Decision log"): `tsc` is the reference TypeScript
;; compiler, so it type-checks (the load-bearing "reject a bad spec before it
;; runs") AND emits JS (strips types) — run under the packaged `node`.
;;
;; HERMETICITY. The `typescript` npm tarball is a DECLARED fixed-output source
;; (url-fetch + pinned sha256) — the same narrowed offline contract as every
;; other source the loop fetches (tests/typed-diff.scm: substitutes disabled, a
;; cold fixed-output SOURCE fetch by the daemon is still permitted). The package
;; is pure JavaScript (no native addon), so it builds offline with no toolchain
;; and `guix build --check` reproduces it from the pinned bytes alone. Bump the
;; version + hash deliberately, like any other pinned input.
;;
;; FSDG: typescript is Apache-2.0 (free); the relaxed free-software posture
;; (DESIGN §5) would permit a nonfree pinned input anyway, but this one is clean.

(define-module (system td-ts)
  #:use-module (guix packages)
  #:use-module (guix gexp)
  #:use-module (guix download)
  #:use-module (guix build-system copy)
  #:use-module (guix build-system gnu)
  #:use-module (gnu packages rust)
  #:use-module (gnu packages nss)
  #:use-module ((guix licenses) #:prefix license:)
  #:export (td-typescript
            td-ts-eval))

(define td-typescript
  (package
    (name "td-typescript")
    (version "5.5.4")
    (source
     (origin
       (method url-fetch)
       (uri (string-append "https://registry.npmjs.org/typescript/-/typescript-"
                           version ".tgz"))
       (sha256
        (base32 "11l59n9krqkwnhf5dm69h58c9wbiw28cf46gla81saj69lsvd016"))))
    (build-system copy-build-system)
    ;; The npm tarball unpacks to a single `package/` dir, into which the
    ;; gnu/copy unpack phase chdirs; install its `bin/` (the `tsc` entry shim)
    ;; and `lib/` (the compiler itself — bin/tsc requires ../lib/tsc.js, so they
    ;; must stay siblings). Run it as `node <td-typescript>/bin/tsc` — the `ts`
    ;; rung does exactly that.
    (arguments
     '(#:install-plan
       '(("bin" "bin")
         ("lib" "lib"))))
    (synopsis "Pinned TypeScript compiler (tsc) for the ts-frontend spec surface")
    (description
     "td-typescript is the pinned @code{typescript} npm package (the reference
@code{tsc} compiler), brought in as a hash-pinned fixed-output input because the
pinned Guix channel does not package it.  Run under the packaged @code{node}, it
type-checks a td TypeScript spec (rejecting a malformed one before evaluation)
and emits the type-stripped JavaScript the boa evaluator runs (DESIGN §7.1
ts-frontend Phase 1).")
    (home-page "https://www.typescriptlang.org/")
    (license license:asl2.0)))

;;; ---------------------------------------------------------------------------
;;; td-ts-eval — the boa evaluator (DESIGN §7.1 ts-frontend, sub-task 2).
;;;
;;; A small pure-Rust binary (../ts-eval) that evaluates the type-stripped spec
;;; JS in an embedded boa engine (boa_engine, in-process — the charter's
;;; "pure-Rust, in-process" evaluator, vendored as a pinned input per the human
;;; 2026-06-13 decision, since boa is absent from the pinned channel). Before it
;;; runs user code it evaluates a CURATED-GLOBAL prelude that strips
;;; language-level nondeterminism (removes Date, denies Math.random); boa has no
;;; fetch/fs/process/web APIs to begin with, so the global is offline by
;;; construction (plan/ts-frontend.md "Hermetic eval").
;;;
;;; OFFLINE/HERMETIC. boa pulls ~110 crates; rather than committing ~50MB of
;;; vendored sources, the crate tree is materialised by %ts-eval-vendor, a
;;; FIXED-OUTPUT derivation (content-addressed by the pinned Cargo.lock) that
;;; runs `cargo vendor`. Network is permitted ONLY because the output is
;;; hash-pinned — the same rule that lets url-fetch/git-fetch run; once realised
;;; it is warm in the store and the loop never re-fetches (substitutes disabled,
;;; offline). The build itself is fully offline against that vendor. cargo is the
;;; pinned channel's rust 1.93.0, identical to the host's, so the vendor tree is
;;; deterministic and the pin is stable across machines.

;; The ts-eval crate source: ../ts-eval (Cargo.toml, Cargo.lock, src/). Exclude
;; any local build/vendor output so a developer who ran cargo by hand cannot
;; perturb the derivation hash (the td-builder lesson). Match the BASENAME, not a
;; substring.
(define %ts-eval-source
  (local-file "../ts-eval" "td-ts-eval-src"
              #:recursive? #t
              #:select? (lambda (file stat)
                          (not (member (basename file)
                                       '("target" "vendor" ".cargo"))))))

;; Fixed-output vendored crate tree for ts-eval's Cargo.lock. The hash is the
;; nar of the cargo-vendor directory; pin it once (a placeholder build reports
;; the real hash) and bump it deliberately whenever Cargo.lock changes — exactly
;; like any other pinned source.
(define %ts-eval-vendor
  (computed-file
   "td-ts-eval-vendor"
   (with-imported-modules '((guix build utils))
     #~(begin
         (use-modules (guix build utils)
                      (ice-9 textual-ports))
         (setenv "PATH"
                 (string-append (ungexp rust) "/bin:"
                                (ungexp rust "cargo") "/bin"))
         (setenv "CARGO_HOME" (string-append (getcwd) "/cargo-home"))
         ;; cargo's libcurl wants a single CA *bundle*; nss-certs ships only the
         ;; hashed per-cert files, so concatenate them into one and point curl +
         ;; openssl at it (CURL_CA_BUNDLE is libcurl's CAINFO; SSL_CERT_FILE
         ;; covers the openssl backend).
         (let ((bundle (string-append (getcwd) "/ca-bundle.crt")))
           (call-with-output-file bundle
             (lambda (out)
               (for-each
                (lambda (f)
                  (put-string out (call-with-input-file f get-string-all)))
                (find-files (string-append (ungexp nss-certs) "/etc/ssl/certs")
                            "\\.0$"))))
           (setenv "CURL_CA_BUNDLE" bundle)
           (setenv "SSL_CERT_FILE" bundle))
         ;; A writable copy of the manifest (cargo validates src/ exists).
         (copy-recursively (ungexp %ts-eval-source) "crate")
         (for-each make-file-writable
                   (find-files "crate" #:directories? #t))
         (chdir "crate")
         (invoke "cargo" "vendor" "--locked" "--versioned-dirs" (ungexp output))))
   #:options (list #:hash-algo 'sha256
                   #:recursive? #t
                   #:hash (base32 "07kpr4kfxsmf92ddzfja7dwfjkhvvgmmkfj80ryi7zf4xzpcbivh"))))

(define td-ts-eval
  (package
    (name "td-ts-eval")
    (version "0.1.0")
    (source %ts-eval-source)
    (build-system gnu-build-system)
    (native-inputs
     `(("rust" ,rust)
       ("rust:cargo" ,rust "cargo")))
    (arguments
     (list
      #:tests? #f
      #:phases
      #~(modify-phases %standard-phases
          (delete 'configure)
          (delete 'check)
          (replace 'build
            (lambda _
              (setenv "CARGO_HOME" (string-append (getcwd) "/.cargo-home"))
              ;; Offline build against the pinned vendored crates.
              (mkdir-p ".cargo")
              (call-with-output-file ".cargo/config.toml"
                (lambda (port)
                  (format port "[source.crates-io]~%\
replace-with = \"vendored\"~%\
[source.vendored]~%\
directory = \"~a\"~%" #$%ts-eval-vendor)))
              (invoke "cargo" "build" "--release" "--offline" "--locked")))
          (replace 'install
            (lambda _
              (install-file "target/release/td-ts-eval"
                            (string-append #$output "/bin")))))))
    (synopsis "td's boa-based hermetic JS evaluator (ts-frontend Phase 1)")
    (description
     "td-ts-eval evaluates the type-stripped TypeScript spec JS in an embedded
@code{boa_engine} (pure-Rust, in-process).  A curated-global prelude strips
language-level nondeterminism (removes @code{Date}, denies @code{Math.random})
before user code runs; boa ships no @code{fetch}/@code{fs}/@code{process}/web
APIs, so evaluation is offline and deterministic by construction (DESIGN §7.1
ts-frontend Phase 1).")
    (home-page "https://github.com/timmydo/td")
    (license license:gpl3+)))
