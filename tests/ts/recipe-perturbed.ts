// recipe-perturbed.ts — the differential's discriminator (corpus-independence).
//
// Identical to recipe-hello.ts EXCEPT one wrong byte in the upstream source hash
// (1aqq… -> 1bqq…). A different declared upstream ⇒ a different build derivation,
// so the `corpus` rung's differential must see this DIVERGE from the corpus
// oracle. If it ever converges, the differential has gone vacuous (verified-red).
recipe({
  name: "hello",
  version: "2.12.2",
  source: fetchSource(
    "mirror://gnu/hello/hello-2.12.2.tar.gz",
    "1bqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js"),
  buildSystem: "gnu",
});
