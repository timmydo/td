// recipe-hello.ts — td's OWN recipe for GNU hello, authored in TypeScript
// (corpus-independence, Phase 2 of the §5 move-off-Guile goal).
//
// The CORPUS axis: this reconstructs the package from upstream coordinates —
// it does NOT look the definition up in the Guix corpus. The differential
// (the `corpus` rung) lowers this through the generic Guile recipe bridge
// (system td-recipe) and proves it NAR-hash-equal to the pinned corpus's build
// of `hello` (Guix is the oracle, retired last).
recipe({
  name: "hello",
  version: "2.12.2",
  source: fetchSource(
    "mirror://gnu/hello/hello-2.12.2.tar.gz",
    "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js"),
  buildSystem: "gnu",
});
