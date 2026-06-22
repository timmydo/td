// recipe-bat.ts — `bat` (a `cat` clone with syntax highlighting), built FROM SOURCE by td.
// The LAST of the shipped Rust userland (procs/fd/ripgrep/sd/eza/bat, PR #80) to move from
// guix-packaged to td-built-from-source via build-recipe. The source is the upstream `bat`
// 0.25.0 crate tarball (keyed `bat-source`); its 207-crate closure is vendored as `.crate`
// fixed-output static.crates.io fetches in tests/bat.lock.
//
// bat's default `application` feature pulls BOTH `git` (→ git2 → libgit2/openssl C) AND, via
// `minimal-application`, `regex-onig` (→ oniguruma C). Build noDefaultFeatures with the
// pure-Rust pieces: clap/etcetera/paging/wild (the CLI app, minus git) + `regex-fancy`
// (syntect's pure-Rust regex backend, instead of regex-onig). No C build. (bat highlights
// with its committed prebuilt assets, so `build-assets` is not needed.) The .drv is assembled
// + realized by td (no guix (derivation …) / no guix-daemon); the rustc/cargo/gcc seed is
// external (§5). Auto-classified self-hosted by buildSystem (no census enrollment).
recipe({
  name: "bat",
  version: "0.25.0",
  buildSystem: "rust",
  bins: ["bat"],
  noDefaultFeatures: true,
  features: ["clap", "etcetera", "paging", "wild", "regex-fancy"],
});
