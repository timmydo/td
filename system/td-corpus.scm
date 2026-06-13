;; system/td-corpus.scm — td's OWN package recipes (DESIGN §7.1
;; corpus-independence, Phase 2 of the §5 move-off-Guile goal).
;;
;; CORPUS axis (where a package definition comes from), distinct from the SURFACE
;; axis (`ts-frontend`, what language a spec is written in). Today every td
;; artifact reads the pinned Guix corpus `(gnu packages …)`. This module instead
;; RECONSTRUCTS a recipe from upstream coordinates — source URL + sha256 + a build
;; expression — so the package's provenance is td's, not a corpus lookup.
;;
;; DELIBERATELY does NOT import any `(gnu packages …)` module: the recipe stands on
;; its own. What it still leans on — `gnu-build-system` and, transitively, the
;; toolchain (gcc/glibc/make) — is Guix infrastructure that the migration retires
;; LAST (§5: the seed/first toolchain stays external; no full-source bootstrap).
;; What changes here is provenance, nothing else.
;;
;; The differential oracle is the pinned corpus's own `hello` (§2.5 / prime
;; directive 4): the recipe is PROVEN equivalent (tests/corpus-diff.scm lowers both
;; and diffs the derivations; the `corpus` rung builds + `--check`s), never merely
;; asserted. GNU hello is the POC package because in the pinned channel it is
;; maximally trivial — no inputs, no native-inputs, no `arguments` — so a
;; from-scratch recipe lowers to the corpus's exact derivation (plan/
;; corpus-independence.md "Why GNU hello").
;;
;; HERMETICITY. The source is a DECLARED fixed-output url-fetch (pinned sha256) —
;; the same narrowed offline contract every other td source uses (tests/
;; typed-diff.scm: substitutes disabled; a cold fixed-output SOURCE fetch by the
;; daemon is still permitted). hello's build is offline against the warm toolchain.

(define-module (system td-corpus)
  #:use-module (guix packages)
  #:use-module (guix download)
  #:use-module (guix build-system gnu)
  #:use-module ((guix licenses) #:prefix license:)
  #:export (%hello-source-uri
            %hello-source-sha256
            td-hello/source-sha256
            td-hello))

;; Upstream coordinates for GNU hello 2.12.2 — declared by td, read from upstream
;; (mirror://gnu), NOT from `(gnu packages base)`. Bump version+hash deliberately,
;; like any other pinned source.
(define %hello-source-uri
  "mirror://gnu/hello/hello-2.12.2.tar.gz")

(define %hello-source-sha256
  "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js")

;; The recipe, parameterised on the source hash so the differential can perturb
;; the upstream coordinate (a wrong hash ⇒ a different source ⇒ a divergent build)
;; without duplicating the package definition. `sha256` is the ONLY load-bearing
;; provenance input to the build derivation; name/version drive the source URI and
;; output name; synopsis/description/home-page/license are metadata that do not
;; enter the derivation (the recipe converges on the oracle regardless of them).
(define (td-hello/source-sha256 hash)
  (package
    (name "hello")
    (version "2.12.2")
    (source (origin
              (method url-fetch)
              (uri %hello-source-uri)
              (sha256 (base32 hash))))
    (build-system gnu-build-system)
    (synopsis "Hello, GNU world: an example GNU package")
    (description
     "GNU Hello prints the message \"Hello, world!\" and then exits.  It serves
as an example of standard GNU coding and packaging conventions.")
    (home-page "https://www.gnu.org/software/hello/")
    (license license:gpl3+)))

;; td's recipe for GNU hello.
(define td-hello (td-hello/source-sha256 %hello-source-sha256))
