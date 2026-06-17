// spec-gen1.ts — a WELL-TYPED generation system, authored in TypeScript: spec-v0
// with `generation: 1` (and an extra disposable persistent path). It exercises
// the structured config fields now on the TS surface (generation + persistentPaths)
// and MUST lower to a DIFFERENT system derivation than the frozen oracle — a
// generation system boots through dm-verity onto a tmpfs root and mounts td-state,
// none of which the default (generation #f) oracle emits. This is the LOAD-BEARING
// proof for the new fields: if the bridge ignored `generation`/`persistentPaths`,
// this would converge with the oracle and the `ts-diff` gate would red. Type-checks
// clean — the divergence is at lowering, not type-check.
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
  persistentPaths: [
    { tier: "precious", path: "/var/lib/ssh" },
    { tier: "disposable", path: "/var/log" },
  ],
  generation: 1,
};

system(spec);
