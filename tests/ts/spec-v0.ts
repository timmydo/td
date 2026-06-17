// spec-v0.ts — the v0 td system spec, written in the TypeScript dialect
// (tests/ts/td-spec.d.ts). Its values are exactly the `td-config` defaults, so
// it lowers (tsc -> boa -> JSON -> td-config -> derivation) to a system
// derivation identical to the frozen system/td.scm oracle — the §7.1 acceptance
// the `ts-diff` rung proves (and that `tests/typed-diff.scm` proves for the
// Guile typed front-end). Well-typed: `tsc` accepts it and emits the JS in
// tests/ts/spec-v0.expected.js (the `ts` rung's golden).
const spec: SystemSpec = {
  hostName: "td",
  timezone: "UTC",
  locale: "en_US.utf8",
  bootloaderTarget: "/dev/vda",
  rootFsLabel: "td-root",
  rootMount: "/",
  rootFsType: "ext4",
  sshPort: 22,
  sshPasswordAuth: false,
  sshChallengeResponse: false,
  shipGuix: false,
  persistentPaths: [{ tier: "precious", path: "/var/lib/ssh" }],
  generation: null,
};

system(spec);
