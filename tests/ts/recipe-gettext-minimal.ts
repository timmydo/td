// recipe-gettext-minimal.ts — td's OWN recipe for gettext-minimal, authored in
// TypeScript (input-recipes: reconstruct individual recipes, move-off-Guile §5).
//
// gettext-minimal is one of nano's TWO direct inputs — reconstructing it
// store-path-equal to the corpus oracle starts retiring the resolver for a real
// nano dependency. It is the most elaborate recipe so far: a `doc` output, a
// makeFlag, configure flags, and TWO custom phases. `patch-fixed-paths` is plain
// (literal substitute* over file lists); `patch-tests` exercises the full
// phase-body vocabulary — a match variable, a `let`-`which` binding, a
// `with-fluids` byte-encoding guard, `find-files`, `cons`, and a `format`
// replacement. The bridge lowers all of it to the byte-identical
// `(modify-phases …)` gexp the corpus writes by hand. The `corpus-gettext` gate
// proves it lowers store-path-equal (NAR-hash-equal) to the corpus gettext-minimal
// (Guix is the oracle) and that the artifact functions (msgfmt runs).
recipe({
  name: "gettext-minimal",
  version: "0.23.1",
  source: fetchSource(
    "mirror://gnu/gettext/gettext-0.23.1.tar.gz",
    "0j8fijicvg8jkrisgsqbpnbmfb2mz3gx2p6pcwip82731yb7i9aj"),
  buildSystem: "gnu",
  outputs: ["out", "doc"],
  inputs: ["libunistring", "libxml2", "ncurses"],
  configureFlags: ["--with-included-libunistring=no", "--with-included-libxml=no"],
  makeFlags: ["VERBOSE=yes"],
  phases: [
    {
      position: "before",
      anchor: "patch-source-shebangs",
      name: "patch-fixed-paths",
      body: [
        {
          substitute: {
            list: ["gettext-tools/config.h.in", "gettext-tools/gnulib-tests/init.sh",
                   "gettext-tools/tests/init.sh", "gettext-tools/system-tests/run-test"],
          },
          clauses: [{ from: "/bin/sh", to: "sh" }],
        },
        {
          substitute: {
            list: ["gettext-tools/src/project-id", "gettext-tools/projects/KDE/trigger",
                   "gettext-tools/projects/GNOME/trigger"],
          },
          clauses: [{ from: "/bin/pwd", to: "pwd" }],
        },
      ],
    },
    {
      position: "before",
      anchor: "check",
      name: "patch-tests",
      lambdaArgs: ["inputs"],
      body: [
        {
          substitute: "gettext-tools/gnulib-tests/test-execute.sh",
          clauses: [{ from: "^#!.*", match: ["all"], to: { stringAppend: [{ var: "all" }, "exit 77;\n"] } }],
        },
        {
          letWhich: [{ name: "bash", prog: "sh" }],
          body: [
            {
              withDefaultPortEncodingFalse: true,
              body: [
                {
                  substitute: { findFiles: ["gettext-tools/tests", "^(lang-sh|msg(exec|filter)-[0-9])"] },
                  clauses: [{ from: "#![[:blank:]]/bin/sh", to: { format: ["#!~a", { var: "bash" }] } }],
                },
                {
                  substitute: { cons: ["gettext-tools/src/msginit.c", { findFiles: ["gettext-tools/gnulib-tests", "posix_spawn"] }] },
                  clauses: [{ from: "/bin/sh", to: { var: "bash" } }],
                },
                {
                  substitute: "gettext-tools/src/project-id",
                  clauses: [{ from: "/bin/pwd", to: "pwd" }],
                },
              ],
            },
          ],
        },
      ],
    },
  ],
});
