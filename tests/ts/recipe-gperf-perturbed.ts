// recipe-gperf-perturbed.ts — the corpus-gperf self-discrimination discriminator.
//
// Identical to recipe-gperf.ts EXCEPT it adds a load-bearing `configureFlags` entry,
// which flows into the assembled .drv. In the corpus build path the SOURCE is resolved
// from the pinned lock (not the recipe hash), so a recipe FIELD is the load-bearing
// perturbation: this recipe MUST assemble a DISTINCT .drv from recipe-gperf.ts. The
// corpus-no-guix gate assembles its .drv and asserts the path differs from the real
// gperf .drv; if they ever match, the recipe's fields are not reaching the .drv
// (verified-red). Not in corpus_SPECS — never built; only its assembled .drv path is
// compared.
recipe({
  name: "gperf",
  version: "3.3",
  source: fetchSource(
    "mirror://gnu/gperf/gperf-3.3.tar.gz",
    "1n2ac3cxinbfbq41jdpb7mlz58q3vga6rzbshdaf0fp4lymy11zx"),
  buildSystem: "gnu",
  configureFlags: ["--disable-dependency-tracking"],
});
