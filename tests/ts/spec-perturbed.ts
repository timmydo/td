// spec-perturbed.ts — a WELL-TYPED perturbation of spec-v0.ts: identical except
// sshPort 2222 (the same lever tests/typed-diff.scm uses). It must lower to a
// DIFFERENT system derivation than the oracle — the DISCRIMINATE half of the
// `ts-diff` acceptance (§7.1 #2). If a perturbed spec ever converged with the
// oracle, the differential would be vacuous, so this reds the rung. It type-checks
// clean (2222 is a valid number) — the divergence is at lowering, not type-check.
const spec: SystemSpec = {
  hostName: "td",
  timezone: "UTC",
  locale: "en_US.utf8",
  bootloaderTarget: "/dev/vda",
  rootFsLabel: "td-root",
  rootMount: "/",
  rootFsType: "ext4",
  sshPort: 2222,
  sshPasswordAuth: false,
  sshChallengeResponse: false,
  shipGuix: false,
};

system(spec);
