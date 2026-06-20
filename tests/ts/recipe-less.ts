// recipe-less.ts — td's OWN recipe for GNU less, authored in TypeScript. A NEW
// owned recipe that CHAINS onto td's ncurses: less's only build input is ncurses
// (the terminal library), itself an owned recipe. `td-builder build-plan --auto`
// derives the chain from this `inputs` list (no manifest), marks the ncurses edge
// `td-recipe-output`, and the build-plan gate (mk/gates/365) PROVES less's .drv
// references td's OWN ncurses output, not guix's — so less is edge-owned the moment
// it lands (the self-maintaining infra from #113). Input resolution otherwise stays
// the guix-built seed (retired LAST — DESIGN §5).
//
// guix's less applies a Hurd-only patch (less-hurd-path-max); td builds the RAW
// upstream tarball, which is irrelevant on Linux. The build-plan gate's Guix leg
// only asserts a DISTINCT store path (own, then diverge), not byte-identity.
recipe({
  name: "less",
  version: "608",
  source: fetchSource(
    "mirror://gnu/less/less-608.tar.gz",
    "02f2d9d6hyf03va28ip620gjc6rf4aikmdyk47h7frqj18pbx6m6"),
  buildSystem: "gnu",
  inputs: ["ncurses"],
  // less 608 is legacy C: screen.c declares the termcap functions K&R-style
  // (`char *tgetstr()`). td's toolchain seed is gcc 15, which defaults to C23
  // where empty `()` means `(void)` — so those decls CONFLICT with td's ncurses
  // termcap.h prototypes ("too many arguments to 'tgetstr'; expected 0"). Build
  // with the pre-C23 std so `()` is "unspecified args" again — the standard way
  // to build old C with a modern compiler. (Bounded to less; no other recipe.)
  configureFlags: ["CFLAGS=-O2 -std=gnu17"],
});
