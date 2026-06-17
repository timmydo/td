// recipe-file.ts — td's OWN recipe for `file` (libmagic) (lever 4: retire the
// Guix toolchain leaf tools, build them with td's own builder; move-off-Guile
// §5). `file` identifies file types during builds; today it comes from guix's
// `file` package. Plain autotools, no build inputs (the pinned seed's gcc builds
// it). The compiler seed (gcc/glibc/binutils) stays external (§5, retired last).
recipe({
  name: "file",
  version: "5.46",
  source: fetchSource(
    "http://ftp.astron.com/pub/file/file-5.46.tar.gz",
    "1230v1sks2p4ijc7x68iy2z9sqfm17v5lmfwbq9l7ib0qp3pgk69"),
  buildSystem: "gnu",
});
