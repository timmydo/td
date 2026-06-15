// recipe-libatomic-ops-perturbed.ts — the corpus-libatomic differential's SOURCE
// discriminator.
//
// Identical to recipe-libatomic-ops.ts EXCEPT one wrong byte in the upstream
// source hash (0lcv… -> 0mcv…). A different declared upstream ⇒ a different build
// derivation, so the `corpus-libatomic` gate must see this DIVERGE from the corpus
// oracle. If it ever converges, the differential has gone vacuous (verified-red).
// (The NEW `outputs` field's load-bearing-ness is proven separately, by stripping
// it in the differential — see tests/ts-recipe-libatomic-diff.scm.)
recipe({
  name: "libatomic-ops",
  version: "7.8.2",
  source: fetchSource(
    "https://github.com/bdwgc/libatomic_ops/releases/download/v7.8.2/libatomic_ops-7.8.2.tar.gz",
    "0mcv86ib2ryqh18gsgarpkyf6k5l2bd1kh5lbkxv7wh7w9zj01fk"),
  buildSystem: "gnu",
  outputs: ["out", "debug"],
});
