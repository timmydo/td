// recipe-libunistring.ts — td's OWN recipe for GNU libunistring (move-off-Guile
// §5, lever 4). Unicode string library; a build input of gettext-minimal. Today
// from guix's `libunistring`. Plain autotools, no build inputs beyond the seed.
recipe({
  name: "libunistring",
  version: "1.3",
  source: fetchSource(
    "mirror://gnu/libunistring/libunistring-1.3.tar.xz",
    "09wmas38i9fw7l3sv92xkbvy7idcl76ifhzv7l7ia98xhdn7higj"),
  buildSystem: "gnu",
});
