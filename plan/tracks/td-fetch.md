section: side
status: claimed
title: td-fetch
handle: claude-fable-5caf33
date: 2026-06-20
notes: plan/td-fetch.md
summary: move-off-Guile §5 — td's OWN seed fetcher. A small vendored-Rust HTTP(S)+sha256 client (fetch/, ureq + rustls/ring + sha2, 74 crates) that GETs a pinned blob and verifies its sha256 — replacing guix url-fetch as the FETCHER of the pinned fixed-output seeds (proven on the real tsgo tarball). Built from source via build-recipe + stage0 (guix/Guile off PATH, TD_VENDOR_CRATES). The rust-fetch gate proves it with a self-contained LOOPBACK round-trip (serve the tsgo bytes on 127.0.0.1, fetch+verify them back — offline, like the russh gate's loopback SSH), a load-bearing sha256 self-discrimination control, and a migration oracle: td-fetch's verified sha256 equals guix's `td-tsgo-tarball` origin pin. Reproducible by td-builder check.
