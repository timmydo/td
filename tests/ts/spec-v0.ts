// spec-v0.ts — the v0 td system spec, written in the TypeScript dialect
// (tests/ts/td-spec.d.ts). Well-typed: `tsc` accepts it and emits the JS in
// tests/ts/spec-v0.expected.js (the `ts` rung's golden). This is the spec the
// later sub-tasks evaluate (boa) and lower to a derivation NAR-hash-equal to the
// frozen system/td.scm oracle.
const spec: SystemSpec = {
  hostName: "td",
  sshPort: 22,
  rootFsType: "ext4",
};

system(spec);
