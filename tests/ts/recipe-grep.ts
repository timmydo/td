// recipe-grep.ts — td's OWN recipe for GNU grep (lever 4: retire the Guix
// toolchain leaf tools; move-off-Guile §5). Autotools with one build input:
// pcre2 (Perl-compatible regex), declared by its corpus name and resolved from
// the pinned lock (input resolution stays the seed's, retired last §5). The
// toolchain's `grep` becomes td-built, not guix-resolved.
recipe({
  name: "grep",
  version: "3.11",
  source: fetchSource(
    "mirror://gnu/grep/grep-3.11.tar.xz",
    "1avf4x8skxbqrjp5j2qr9sp5vlf8jkw2i5bdn51fl3cxx3fsxchx"),
  buildSystem: "gnu",
  inputs: ["pcre2"],
});
