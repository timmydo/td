// recipe-nano-perturbed.ts — the corpus-deps differential's SOURCE discriminator.
//
// Identical to recipe-nano.ts EXCEPT one wrong byte in the upstream source hash
// (1pyy… -> 1qyy…). A different declared upstream ⇒ a different build derivation,
// so the `corpus-deps` rung's differential must see this DIVERGE from the corpus
// oracle. If it ever converges, the differential has gone vacuous (verified-red).
recipe({
  name: "nano",
  version: "8.7.1",
  source: fetchSource(
    "mirror://gnu/nano/nano-8.7.1.tar.xz",
    "1qyy3hnjr9g0831wcdrs18v0lh7v63yj1kaf3ljz3qpj92rdrw3n"),
  buildSystem: "gnu",
  inputs: ["gettext-minimal", "ncurses"],
});
