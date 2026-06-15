// recipe-popt.ts — td's OWN recipe for popt, authored in TypeScript
// (input-recipes: reconstruct individual recipes, move-off-Guile §5).
//
// The PHASES rung. Where the earlier rungs add configure-flags, a multi-URI
// source, and extra outputs, this adds the recipe-DSL `phases` field: popt's
// corpus recipe adds a custom `patch-test` phase before `configure` that patches
// two test files with `substitute*`. popt is the cleanest phase demonstrator —
// its ONLY non-default argument is this one phase (no configure-flags, no extra
// outputs, no inputs), so the phase capability is isolated. The bridge lowers the
// phase DATA to the byte-identical `(modify-phases …)` gexp the corpus writes by
// hand. This is the prerequisite capability for nano's DIRECT inputs ncurses +
// gettext-minimal, whose recipes patch source files in custom phases. The
// `corpus-popt` gate proves this lowers store-path-equal (NAR-hash-equal) to the
// pinned corpus's popt (Guix is the oracle) and that the phase is load-bearing.
recipe({
  name: "popt",
  version: "1.18",
  source: fetchSource(
    "http://ftp.rpm.org/popt/releases/popt-1.x/popt-1.18.tar.gz",
    "1lf5zlj5rbg6s4bww7hbhpca97prgprnarx978vcwa0bl81vqnai"),
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
