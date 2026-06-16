// recipe-gettext-minimal-perturbed.ts — the corpus-gettext differential's SOURCE
// discriminator. Identical to recipe-gettext-minimal.ts EXCEPT one wrong byte in
// the upstream source hash (0j8f… -> 0k8f…) ⇒ a different build derivation, so the
// gate must see this DIVERGE from the corpus oracle (verified-red — never vacuous).
// The phases' load-bearing-ness is proven separately by stripping `phases` in the
// differential (tests/ts-recipe-gettext-diff.scm).
recipe({
  name: "gettext-minimal",
  version: "0.23.1",
  source: fetchSource(
    "mirror://gnu/gettext/gettext-0.23.1.tar.gz",
    "0k8fijicvg8jkrisgsqbpnbmfb2mz3gx2p6pcwip82731yb7i9aj"),
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
