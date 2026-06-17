// spec-bad-fstype.ts — the ALWAYS-ON negative control for the `ts` rung.
//
// Identical to spec-v0.ts except `rootFsType: "ext3"`, which is NOT in the
// `RootFsType` union (tests/ts/td-spec.d.ts). Every other field is valid, so the
// ONLY error is the fs type — `tsc` MUST reject it (TS2322). If this spec ever
// type-checks clean, the types have stopped being load-bearing — the rung goes
// red. This is the verified-red baked into the suite (DESIGN §7.1 sub-task 1:
// "an ill-typed one (e.g. rootFsType: \"ext3\") FAILS tsc").
const spec: SystemSpec = {
  hostName: "td",
  timezone: "UTC",
  locale: "en_US.utf8",
  bootloaderTarget: "/dev/vda",
  rootFsLabel: "td-root",
  rootMount: "/",
  rootFsType: "ext3",
  sshPort: 22,
  sshPasswordAuth: false,
  sshChallengeResponse: false,
  shipGuix: false,
  persistentPaths: [{ tier: "precious", path: "/var/lib/ssh" }],
  generation: null,
};

system(spec);
