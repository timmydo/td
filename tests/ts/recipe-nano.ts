// recipe-nano.ts — td's OWN recipe for GNU nano, authored in TypeScript
// (corpus-independence, Phase 2 of the §5 move-off-Guile goal). The second
// corpus recipe, and the first WITH build-time inputs.
//
// The CORPUS axis: this reconstructs the package from upstream coordinates — it
// does NOT look the definition up in the Guix corpus. Unlike recipe-hello.ts (a
// leaf package), nano declares two build INPUTS — gettext-minimal and ncurses —
// by their corpus package names. The generic Guile recipe bridge (system
// td-recipe) RESOLVES each from the corpus (input resolution stays Guix's,
// retired LAST — DESIGN §5); the recipe DATA, including which inputs, lives here
// in the TS surface. The `corpus-deps` rung proves this lowers NAR-hash-equal to
// the pinned corpus's build of `nano` (Guix is the oracle, retired last) and that
// the declared inputs are load-bearing.
recipe({
  name: "nano",
  version: "8.7.1",
  source: fetchSource(
    "mirror://gnu/nano/nano-8.7.1.tar.xz",
    "1pyy3hnjr9g0831wcdrs18v0lh7v63yj1kaf3ljz3qpj92rdrw3n"),
  buildSystem: "gnu",
  inputs: ["gettext-minimal", "ncurses"],
});
