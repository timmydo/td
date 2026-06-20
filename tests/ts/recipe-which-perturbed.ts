// recipe-which-perturbed.ts — the corpus-which self-discrimination discriminator.
//
// Identical to recipe-which.ts EXCEPT it adds a load-bearing `configureFlags` entry.
// In the corpus build path the SOURCE is resolved from the pinned lock (not the
// recipe hash), so the recipe's CONTENT proves itself load-bearing through a field
// that flows into the assembled .drv: configureFlags become a drv env var, so this
// recipe MUST assemble a DISTINCT .drv from recipe-which.ts. The corpus-no-guix gate
// emits this recipe, assembles its .drv (no build — assemble-only), and asserts the
// path differs from the real which .drv. If they ever match, the recipe's fields are
// not reaching the .drv and the build is not recipe-driven (verified-red). Not in
// corpus_SPECS — never built; only its assembled .drv path is compared.
recipe({
  name: "which",
  version: "2.21",
  source: fetchSource(
    "mirror://gnu/which/which-2.21.tar.gz",
    "1bgafvy3ypbhhfznwjv1lxmd6mci3x1byilnnkc7gcr486wlb8pl"),
  buildSystem: "gnu",
  configureFlags: ["--disable-iberty"],
});
