// recipe-pcre2.ts — td's OWN recipe for PCRE2 (move-off-Guile §5, lever 4). The
// Perl-compatible regex library; a build input of grep (and ships pcre2test /
// pcre2grep). Today from guix's `pcre2`. Plain autotools; its optional deps
// (bzip2/readline/zlib, for pcre2grep extras) are omitted — the core library +
// pcre2test build without them. The .tar.bz2 source unpacks via the seed's bzip2.
recipe({
  name: "pcre2",
  version: "10.42",
  source: fetchSource(
    "https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.42/pcre2-10.42.tar.bz2",
    "0h78np8h3dxlmvqvpnj558x67267n08n9zsqncmlqapans6csdld"),
  buildSystem: "gnu",
});
