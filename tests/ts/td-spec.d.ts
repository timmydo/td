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

/** A part of a `string-append`/`format` replacement: a literal string, a
 *  build-time store path (`{ output: NAME }` → `(assoc-ref outputs NAME)`,
 *  `{ input: NAME }` → `(assoc-ref inputs NAME)`), or `{ var: NAME }` — a value
 *  bound earlier in the phase body (a `let`-`which` binding, or a `substitute*`
 *  match variable). */
type RefPart =
  | string
  | { readonly output: string }
  | { readonly input: string }
  | { readonly var: string };

/** A `substitute*` replacement:
 *  - a literal string;
 *  - `{ var: NAME }` → the bare bound symbol NAME;
 *  - `{ which: PROG }` → `(which PROG)` (resolve a program on PATH at build time);
 *  - `{ stringAppend: PART[] }` → `(string-append PART …)`;
 *  - `{ format: [FMT, PART…] }` → `(format #f FMT PART …)`. */
type Replacement =
  | string
  | { readonly var: string }
  | { readonly which: string }
  | { readonly stringAppend: readonly RefPart[] }
  | { readonly format: readonly [string, ...RefPart[]] };

/** A `substitute*` FILE argument: a literal filename, `{ list: [...] }` → a quoted
 *  file LIST, `{ findFiles: [DIR, REGEX] }` → `(find-files DIR REGEX)`, or
 *  `{ cons: [A, B] }` → `(cons A B)` (prepend a file to a find-files result). */
type FileArg =
  | string
  | { readonly list: readonly string[] }
  | { readonly findFiles: readonly [string, string] }
  | { readonly cons: readonly [FileArg, FileArg] };

/** One `substitute*` clause `((FROM MATCH-VAR…) TO)`. `match` (optional) names the
 *  regexp submatch variables `TO` may reference via `{ var: … }`. */
interface Clause {
  readonly from: string;
  readonly match?: readonly string[];
  readonly to: Replacement;
}

/** A phase-body STATEMENT — the nested forms a real package phase is built from:
 *  - `{ substitute: FILEARG, clauses: [...] }` → `(substitute* FILEARG CLAUSE…)`;
 *  - `{ letWhich: [{name,prog}…], body: [...] }` → `(let* ((name (which prog))…) …)`;
 *  - `{ withDefaultPortEncodingFalse: true, body: [...] }`
 *      → `(with-fluids ((%default-port-encoding #f)) …)` (preserve byte encoding
 *        while patching ISO-8859-1 files). */
type Stmt =
  | { readonly substitute: FileArg; readonly clauses: readonly Clause[] }
  | { readonly letWhich: readonly { readonly name: string; readonly prog: string }[]; readonly body: readonly Stmt[] }
  | { readonly withDefaultPortEncodingFalse: true; readonly body: readonly Stmt[] };

/** One `substitute*` on a source file: replace text matching `from` (a regexp,
 *  exactly as the corpus phase writes it) with `to`. The flat form for a simple
 *  phase; richer phases use `Phase.body` instead. */
interface Substitution {
  readonly file: string;
  readonly from: string;
  readonly to: Replacement;
}

/** A custom build phase, added relative to a `%standard-phases` anchor.
 *  `lambdaArgs` are the keyword parameters the phase procedure takes — omit for a
 *  nullary `(lambda _ …)`, or e.g. `["inputs"]` for a
 *  `(lambda* (#:key inputs #:allow-other-keys) …)`. The phase body is EITHER the
 *  flat `substitutions` (one `substitute*` each, with `returnTrue` for a trailing
 *  `#t`) OR the rich `body` (a nested statement list — file lists, match vars,
 *  find-files, let/with-fluids — for packages like gettext-minimal). The bridge
 *  lowers this DATA to the byte-identical `(modify-phases %standard-phases …)`
 *  gexp the corpus package writes by hand. */
interface Phase {
  readonly position: "before" | "after";
  readonly anchor: string;
  readonly name: string;
  readonly lambdaArgs?: readonly ("inputs" | "outputs")[];
  readonly substitutions?: readonly Substitution[];
  readonly returnTrue?: boolean;
  readonly body?: readonly Stmt[];
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
 *  a package the corpus builds with tests off); `makeFlags` are the build system's
 *  `#:make-flags`. Each enters the build derivation, so declare them exactly as the
 *  corpus package does. Omit them all for a leaf package with default arguments and
 *  a single `"out"` (e.g. hello). */
interface Recipe {
  readonly name: string;
  readonly version: string;
  readonly source: Source;
  readonly buildSystem: BuildSystem;
  readonly inputs?: readonly string[];
  readonly configureFlags?: readonly string[];
  readonly makeFlags?: readonly string[];
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
