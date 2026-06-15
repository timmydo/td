// recipe-gzip.ts — td's OWN recipe for gzip, authored in TypeScript
// (input-recipes: reconstruct individual recipes, move-off-Guile §5).
//
// The PHASE-REFERENCES-PATHS rung. Where recipe-popt.ts proves a phase with
// literal / `(which …)` substitutions, this proves a phase that bakes a build
// store PATH into a patched file: gzip's `use-absolute-name-of-gzip` phase
// rewrites `exec 'gzip'` to `exec <out>/bin/gzip` via
// `(string-append "exec " (assoc-ref outputs "out") "/bin/gzip")`. So this rung
// adds two recipe-DSL capabilities: `tests` (gzip builds with `#:tests? #f`) and a
// `stringAppend` substitution replacement with an `{output}` part (lowered through
// a `(lambda* (#:key outputs …) …)`). These are the idioms nano's DIRECT inputs
// (ncurses, gettext-minimal) use to inject store paths in their phases. The
// `corpus-gzip` gate proves this lowers store-path-equal (NAR-hash-equal) to the
// pinned corpus's gzip (Guix is the oracle) and that the phase is load-bearing.
recipe({
  name: "gzip",
  version: "1.14",
  source: fetchSource(
    "mirror://gnu/gzip/gzip-1.14.tar.xz",
    "1ihaii7d3vznvj9vk1fkmpvd7pqbz0c8fyzr2pvgs2r2pn0vi9q1"),
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
