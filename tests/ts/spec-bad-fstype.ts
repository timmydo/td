// spec-bad-fstype.ts — the ALWAYS-ON negative control for the `ts` rung.
//
// Identical to spec-v0.ts except `rootFsType: "ext3"`, which is NOT in the
// `RootFsType` union (tests/ts/td-spec.d.ts). `tsc` MUST reject it (TS2322). If
// this spec ever type-checks clean, the types have stopped being load-bearing —
// the rung goes red. This is the verified-red baked into the suite (DESIGN §7.1
// sub-task 1: "an ill-typed one (e.g. rootFsType: \"ext3\") FAILS tsc").
const spec: SystemSpec = {
  hostName: "td",
  sshPort: 22,
  rootFsType: "ext3",
};

system(spec);
