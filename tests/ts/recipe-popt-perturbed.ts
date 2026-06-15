// recipe-popt-perturbed.ts — the corpus-popt differential's SOURCE discriminator.
//
// Identical to recipe-popt.ts EXCEPT one wrong byte in the upstream source hash
// (1lf5… -> 1mf5…). A different declared upstream ⇒ a different build derivation,
// so the `corpus-popt` gate must see this DIVERGE from the corpus oracle. If it
// ever converges, the differential has gone vacuous (verified-red). (The NEW
// `phases` field's load-bearing-ness is proven separately, by stripping it in the
// differential — see tests/ts-recipe-popt-diff.scm.)
recipe({
  name: "popt",
  version: "1.18",
  source: fetchSource(
    "http://ftp.rpm.org/popt/releases/popt-1.x/popt-1.18.tar.gz",
    "1mf5zlj5rbg6s4bww7hbhpca97prgprnarx978vcwa0bl81vqnai"),
  buildSystem: "gnu",
  phases: [{
    position: "before",
    anchor: "configure",
    name: "patch-test",
    substitutions: [
      { file: "tests/test-poptrc.in", from: "/bin/echo", to: { which: "echo" } },
      { file: "tests/testit.sh", from: "lt-test1", to: "test1" },
    ],
    returnTrue: true,
  }],
});
