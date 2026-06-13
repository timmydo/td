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
