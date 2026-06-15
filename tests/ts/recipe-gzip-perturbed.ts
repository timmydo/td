// recipe-gzip-perturbed.ts — the corpus-gzip differential's SOURCE discriminator.
//
// Identical to recipe-gzip.ts EXCEPT one wrong byte in the upstream source hash
// (1ihaii… -> 1jhaii…). A different declared upstream ⇒ a different build
// derivation, so the `corpus-gzip` gate must see this DIVERGE from the corpus
// oracle. If it ever converges, the differential has gone vacuous (verified-red).
// (The NEW phase/`tests` capabilities' load-bearing-ness is proven separately, by
// stripping the phase in the differential — see tests/ts-recipe-gzip-diff.scm.)
recipe({
  name: "gzip",
  version: "1.14",
  source: fetchSource(
    "mirror://gnu/gzip/gzip-1.14.tar.xz",
    "1jhaii7d3vznvj9vk1fkmpvd7pqbz0c8fyzr2pvgs2r2pn0vi9q1"),
  buildSystem: "gnu",
  tests: false,
  configureFlags: ["ac_cv_prog_LESS=\"less\""],
  phases: [{
    position: "after",
    anchor: "unpack",
    name: "use-absolute-name-of-gzip",
    lambdaArgs: ["outputs"],
    substitutions: [{
      file: "gunzip.in",
      from: "exec 'gzip'",
      to: { stringAppend: ["exec ", { output: "out" }, "/bin/gzip"] },
    }],
  }],
});
