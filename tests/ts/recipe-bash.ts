// recipe-bash.ts — td's OWN recipe for GNU Bash (lever 4: retire the Guix
// toolchain leaf tools, build them with td's own builder; move-off-Guile §5).
// bash is the shell td's builder runs ./configure and make recipes under (no
// /bin/sh in the sandbox); today it comes from guix's `bash` package. Autotools
// with two build inputs — readline (line editing) and ncurses (its termcap) —
// resolved from the pinned lock.
//
// Version 5.2.37 does NOT come from the bash-5.2.tar.gz tarball (that is 5.2.0):
// guix applies the 37 upstream bash patches in its origin, and the lowered source
// is the patch-and-repacked .tar.zst already at 5.2.37. td uses that exact
// guix-prepared source from the pinned lock and unpacks it with the seed's `tar`
// (zstd auto-detected via the pinned `zstd` build input) — so the patches are
// part of the faithful source, not a separate td phase. The compiler seed
// (gcc/glibc/binutils) stays external (§5, retired last).
//
// bash 5.2's mkbuiltins.c uses K&R empty-paren declarations; the seed's gcc-15
// defaults to C23, where `f()` means `f(void)`, so those calls hard-error
// ("too many arguments to function 'xmalloc'"). Building under the older
// standard -std=gnu17 restores the K&R "unspecified args" meaning. Carried as a
// whitespace-bearing CFLAGS (with -O2 -g) through the JSON-encoded configureFlags.
recipe({
  name: "bash",
  version: "5.2.37",
  source: fetchSource(
    "mirror://gnu/bash/bash-5.2.tar.gz",
    "1yrjmf0mqg2q8pqphjlark0mcmgf88b0acq7bqf4gx3zvxkc2fd1"),
  buildSystem: "gnu",
  inputs: ["readline", "ncurses"],
  configureFlags: ["CFLAGS=-O2 -g -std=gnu17"],
});
