// td v0 TypeScript spec dialect (the supported subset) — AMBIENT globals.
//
// Phase 1 of the §5 move-off-Guile goal (DESIGN §7.1 ts-frontend). These
// declarations ARE the spec language a `.ts` system spec is written against, and
// they mirror the scalar fields of the Guile `td-config` record (system
// td-typed) one-to-one — camelCase here, kebab there — because a spec lowers by
// being handed to `td-config` (the migration lowering target; the Guile/gexp
// layer stays underneath, DESIGN §5). They are deliberately AMBIENT (no
// `import`/`export`): the dialect is a curated global, mirroring the boa
// evaluator's injected globals. At author/check time `tsc` enforces these
// signatures so a malformed spec — a wrong fs type, a missing field, the wrong
// shape — is rejected BEFORE it is ever evaluated; at eval time boa provides the
// same names. Types are load-bearing here, not decoration (the `ts` rung proves
// a spec that violates them FAILS `tsc`).

/** Root filesystem type — only the types td knows how to lower (mirrors
 *  `%known-fs-types` in system td-typed). A value outside this union (e.g.
 *  "ext3") is a compile-time error. */
declare type RootFsType = "ext4" | "btrfs" | "xfs";

/** The v0 system spec shape — the scalar `td-config` fields. `readonly` so a
 *  spec cannot mutate it after declaration: eval is a pure description, not a
 *  program with state. (Manifest / generation / persistent-paths are NOT in v0:
 *  they default in `td-config`, so the default spec lowers byte-identically to
 *  the frozen oracle. Driving them from TS is later work — see plan/ts-frontend.md.) */
interface SystemSpec {
  readonly hostName: string;
  readonly timezone: string;
  readonly locale: string;
  readonly bootloaderTarget: string;
  readonly rootFsLabel: string;
  readonly rootMount: string;
  readonly rootFsType: RootFsType;
  readonly sshPort: number;
  readonly sshPasswordAuth: boolean;
  readonly sshChallengeResponse: boolean;
  readonly shipGuix: boolean;
}

/** Declare the system to build. The evaluator captures the argument and emits it
 *  as JSON for the Guile lowering bridge, which builds a `td-config` and lowers
 *  it to a derivation; at type-check time this only constrains the shape of what
 *  may be declared. */
declare function system(spec: SystemSpec): void;

// ---------------------------------------------------------------------------
// corpus-independence (Phase 2 of the §5 move-off-Guile goal, DESIGN §7.1).
// The CORPUS axis: a package RECIPE authored here in TypeScript — reconstructed
// from upstream coordinates, not looked up in the Guix corpus — lowered by the
// generic Guile recipe bridge (system td-recipe) and proven NAR-hash-equal to the
// pinned corpus's build of the same package (Guix is the oracle). What stays
// Guile is the bridge (the retire-last lowering target); the recipe DATA lives
// here, in the TS surface.

/** An upstream source: a URL (or a LIST of mirror URLs, like pkg-config's) and
 *  its content hash (nix-base32 sha256). The evaluator records this as data; the
 *  fetch itself is the Guile lowering's declared fixed-output `url-fetch`
 *  (offline contract unchanged). A list and a single URL lower to DIFFERENT
 *  source derivations, so the shape is load-bearing — declare it exactly as
 *  upstream/corpus does. */
interface Source {
  readonly uri: string | readonly string[];
  readonly sha256: string;
}

/** Build systems td knows how to lower (mirrors the bridge's dispatch). A value
 *  outside this union is a compile-time error — like `RootFsType`. v0 is `"gnu"`. */
declare type BuildSystem = "gnu";

/** A part of a `string-append` replacement: a literal string, or a build-time
 *  store path — `{ output: NAME }` → `(assoc-ref outputs NAME)` (an output dir),
 *  `{ input: NAME }` → `(assoc-ref inputs NAME)` (a build input's path). */
type RefPart = string | { readonly output: string } | { readonly input: string };

/** A `substitute*` replacement:
 *  - a literal string;
 *  - `{ which: PROG }` → `(which PROG)` (resolve a program on PATH at build time);
 *  - `{ stringAppend: PART[] }` → `(string-append PART …)`, the idiom that bakes a
 *    build-time store path (an output dir, an input's path) into a patched file. */
type Replacement =
  | string
  | { readonly which: string }
  | { readonly stringAppend: readonly RefPart[] };

/** One `substitute*` on a source file: replace text matching `from` (a regexp,
 *  exactly as the corpus phase writes it) with `to`. */
interface Substitution {
  readonly file: string;
  readonly from: string;
  readonly to: Replacement;
}

/** A custom build phase, added relative to a `%standard-phases` anchor. The body
 *  is a list of `substitute*` source patches (the dominant patch idiom in corpus
 *  recipes). `lambdaArgs` are the keyword parameters the phase procedure takes —
 *  omit for a nullary `(lambda _ …)`, or e.g. `["outputs"]` for a
 *  `(lambda* (#:key outputs #:allow-other-keys) …)` (needed when a substitution
 *  references the build's `outputs`/`inputs`). `returnTrue` appends a trailing
 *  `#t` to the phase body, matching packages whose phase ends in `#t`. The bridge
 *  lowers this DATA to the same `(modify-phases %standard-phases (add-{before,
 *  after} 'anchor 'name …))` gexp the corpus package writes by hand — so a recipe
 *  that declares a package's real phase converges on it. */
interface Phase {
  readonly position: "before" | "after";
  readonly anchor: string;
  readonly name: string;
  readonly substitutions: readonly Substitution[];
  readonly lambdaArgs?: readonly ("inputs" | "outputs")[];
  readonly returnTrue?: boolean;
}

/** A package recipe — the coordinates that determine the build derivation: name,
 *  version, the upstream source, the build system, any configure flags, any extra
 *  outputs, any custom build phases, and the names of any build inputs
 *  (dependencies). An input is named by its corpus package name; the Guile bridge
 *  RESOLVES it from the corpus (input resolution stays Guix's, retired LAST —
 *  DESIGN §5). `configureFlags` are the build system's `#:configure-flags`;
 *  `outputs` are the package's outputs (declare extra `"debug"`/`"static"`/`"doc"`
 *  exactly as the corpus splits them); `phases` are custom build phases; `tests`
 *  is whether to run the test suite (`#:tests?`, default `true` — set `false` for
 *  a package the corpus builds with tests off). Each enters the build derivation,
 *  so declare them exactly as the corpus package does. Omit them all for a leaf
 *  package with default arguments and a single `"out"` (e.g. hello). */
interface Recipe {
  readonly name: string;
  readonly version: string;
  readonly source: Source;
  readonly buildSystem: BuildSystem;
  readonly inputs?: readonly string[];
  readonly configureFlags?: readonly string[];
  readonly outputs?: readonly string[];
  readonly phases?: readonly Phase[];
  readonly tests?: boolean;
}

/** Declare an upstream source by URL (or mirror-URL list) + content hash (does
 *  not fetch). */
declare function fetchSource(uri: string | readonly string[], sha256: string): Source;

/** Declare the package to build. The evaluator captures the argument and emits it
 *  as JSON for the Guile recipe bridge, which reconstructs a package and lowers it
 *  to a derivation; at type-check time this constrains the shape of the recipe. */
declare function recipe(r: Recipe): void;
