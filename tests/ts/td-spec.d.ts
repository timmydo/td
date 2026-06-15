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

/** A package recipe — the coordinates that determine the build derivation: name,
 *  version, the upstream source, the build system, any configure flags, and the
 *  names of any build inputs (dependencies). An input is named by its corpus
 *  package name; the Guile bridge RESOLVES it from the corpus (input resolution
 *  stays Guix's, retired LAST — DESIGN §5). `configureFlags` are the build
 *  system's `#:configure-flags` (they enter the build derivation, so declare them
 *  exactly as the corpus package does). `outputs` are the package's outputs —
 *  declare extra outputs (`"debug"`, `"static"`, `"doc"`) exactly as the corpus
 *  package splits them, since an extra output enters the derivation. Omit
 *  `inputs`/`configureFlags`/`outputs` for a leaf package with default arguments
 *  and a single `"out"` (e.g. hello). */
interface Recipe {
  readonly name: string;
  readonly version: string;
  readonly source: Source;
  readonly buildSystem: BuildSystem;
  readonly inputs?: readonly string[];
  readonly configureFlags?: readonly string[];
  readonly outputs?: readonly string[];
}

/** Declare an upstream source by URL (or mirror-URL list) + content hash (does
 *  not fetch). */
declare function fetchSource(uri: string | readonly string[], sha256: string): Source;

/** Declare the package to build. The evaluator captures the argument and emits it
 *  as JSON for the Guile recipe bridge, which reconstructs a package and lowers it
 *  to a derivation; at type-check time this constrains the shape of the recipe. */
declare function recipe(r: Recipe): void;
