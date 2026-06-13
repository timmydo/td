// td v0 TypeScript spec dialect (the supported subset) — AMBIENT globals.
//
// Phase 1 of the §5 move-off-Guile goal (DESIGN §7.1 ts-frontend). These
// declarations ARE the spec language a `.ts` system spec is written against.
// They are deliberately AMBIENT (no `import`/`export`): the dialect is a curated
// global, mirroring the boa evaluator's injected globals (sub-task 2+). At
// author/check time `tsc` enforces these signatures so a malformed spec is
// rejected BEFORE it is ever evaluated; at eval time boa provides the same names
// as native functions. Types are load-bearing here, not decoration — the `ts`
// rung proves a spec that violates them FAILS `tsc` (DESIGN §7.1: "a bad spec is
// caught before it ever runs: rootFsType: \"ext3\", a missing field, the wrong
// shape").

/** Root filesystem type — only the types td knows how to lower. A value
 *  outside this union (e.g. "ext3") is a compile-time error. */
declare type RootFsType = "ext4" | "btrfs";

/** The v0 system spec shape. `readonly` so a spec cannot mutate it after
 *  declaration — eval is a pure description, not a program with state. */
interface SystemSpec {
  readonly hostName: string;
  readonly sshPort: number;
  readonly rootFsType: RootFsType;
}

/** Declare the system to build. The evaluator captures the argument and lowers
 *  it to a derivation (sub-task 4+); at type-check time this only constrains the
 *  shape of what may be declared. */
declare function system(spec: SystemSpec): void;
