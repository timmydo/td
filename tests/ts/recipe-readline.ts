// recipe-readline.ts — td's OWN recipe for GNU readline (move-off-Guile §5,
// lever 4: reconstruct the shipped-system closure). Line-editing library; a build
// input of bash and gawk. Today from guix's `readline`. Autotools, with ncurses
// (termcap) as a build input resolved from the pinned lock.
//
// Version 8.2.13 comes from guix's origin patch-and-repack of readline-8.2 (the
// 8.2.tar.gz is 8.2.0); td uses that patched .tar.zst from the lock, unpacked by
// the seed tar via the pinned zstd.
recipe({
  name: "readline",
  version: "8.2.13",
  source: fetchSource(
    "mirror://gnu/readline/readline-8.2.tar.gz",
    "0dbw02ai0z8x6d9s14pl0hnaa2g1kdxnv8qqra1fx13ay5qp3srz"),
  buildSystem: "gnu",
  inputs: ["ncurses"],
});
