// recipe-libsigsegv.ts — td's OWN recipe for GNU libsigsegv (move-off-Guile §5,
// lever 4: reconstruct the shipped-system closure package-by-package so td's
// build-time independence from guix climbs). libsigsegv (page-fault / stack-
// overflow handling) is a build input of gawk; today it comes from guix's
// `libsigsegv` package. Plain autotools, no build inputs beyond the seed.
recipe({
  name: "libsigsegv",
  version: "2.14",
  source: fetchSource(
    "mirror://gnu/libsigsegv/libsigsegv-2.14.tar.gz",
    "15d2r831xz94s7540nvb1gbfl062g7mrnj88m60wyr1kh10kkb6d"),
  buildSystem: "gnu",
});
