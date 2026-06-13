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
  #:use-module (guix download)
  #:use-module (guix build-system copy)
  #:use-module ((guix licenses) #:prefix license:)
  #:export (td-typescript))

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
