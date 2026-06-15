// recipe-libatomic-ops.ts — td's OWN recipe for libatomic-ops, authored in
// TypeScript (input-recipes: reconstruct individual recipes, move-off-Guile §5).
//
// The MULTI-OUTPUT rung. Where recipe-pkg-config.ts adds configure-flags + a
// multi-URI source, this adds the recipe-DSL `outputs` field: libatomic-ops splits
// a `debug` output off `out`, and that extra output enters the build derivation.
// libatomic-ops is the cleanest demonstrator — it sets NO configure-flags and NO
// custom phases, so the multi-output capability is isolated (the only thing beyond
// a leaf recipe is the second output). It is the prerequisite for reconstructing
// nano's DIRECT inputs ncurses + gettext-minimal, which both carry a `doc` output
// (plus phases, a later rung). The `corpus-libatomic` gate proves this lowers
// store-path-equal (NAR-hash-equal, per output) to the pinned corpus's
// libatomic-ops (Guix is the oracle) and that the extra output is load-bearing.
recipe({
  name: "libatomic-ops",
  version: "7.8.2",
  source: fetchSource(
    "https://github.com/bdwgc/libatomic_ops/releases/download/v7.8.2/libatomic_ops-7.8.2.tar.gz",
    "0lcv86ib2ryqh18gsgarpkyf6k5l2bd1kh5lbkxv7wh7w9zj01fk"),
  buildSystem: "gnu",
  outputs: ["out", "debug"],
});
