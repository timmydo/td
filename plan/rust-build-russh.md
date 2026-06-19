# rust-build-russh — td builds a Rust SSH from source (Increment 4)

Handle: claude-fable-c018e3 · claimed 2026-06-18 · section: side

## Why (human-chosen, 2026-06-18)

After self-host (#84), vendored deps (#87), and a real coreutils tool (#89), the human
picked **russh — a Rust SSH from source** as the next target: a NEW domain (crypto +
networking) beyond userland text tools. russh is a library, so the artifact is a
self-contained **client<->server loopback round-trip**: start an SSH server on
127.0.0.1, connect a client, authenticate by public key, exec a command, read the
reply — a real SSH handshake (curve25519 kex + the aws-lc crypto backend), no external
sshd.

## What it took (de-risked on the host first)

- russh 0.61 + tokio + anyhow → **188 vendored deps**. Builds offline + REPRODUCIBLE
  (built twice at different paths → byte-identical, incl. the aws-lc C crypto build).
- **The C build env was the crux**: russh's crypto backend `aws-lc-sys` compiles C that
  needs (a) a tool literally named for `CC` — gcc-toolchain ships `gcc`, so the `cc`
  crate needs `CC=gcc`; (b) **kernel headers** (`linux/random.h`, `linux/limits.h`) on
  `C_INCLUDE_PATH` (gcc-toolchain has glibc headers, not linux-libre-headers); (c)
  `cmake`. So: `run_rust` now sets `CC`/`CXX` + `C_INCLUDE_PATH`/`CPLUS_INCLUDE_PATH`
  from the inputs' `include/` dirs (mirroring the autotools path), and the seed adds
  `cmake-minimal` + `linux-libre-headers`.
- **Fixed test key** (embedded ed25519 OpenSSH key, loaded via `from_openssh`) instead
  of `PrivateKey::random` — sidesteps a rand_core version skew AND makes the build/run
  deterministic. The round-trip prints `td-russh-ok: ping`.

## Pieces

- `builder/src/build.rs` `run_rust`: C set-paths (CC/CXX + C_INCLUDE_PATH). Safe for the
  pure-Rust builds — the self-host (td-builder, zero deps) compiles no C, so its output
  and the rust-build gate's hash comparison are unchanged.
- `tests/russh-demo/` — the authored crate (Cargo.toml/Cargo.lock/src/main.rs).
- `tests/td-russh-demo.lock` — seed (+ cmake + kernel headers) + 188 vendored deps.
- `tests/td-russh-demo-source.scm`, `tests/ts/recipe-td-russh-demo.ts`,
  `mk/gates/345-rust-russh.mk`, guix-dependence self-host-specs += "td-russh-demo".

## Sub-task ladder
1. [x] de-risk: russh builds offline (C env: CC/CXX + kernel headers + cmake), reproducible
2. [x] de-risk: author the client<->server round-trip; it runs (`td-russh-ok: ping`)
3. [x] run_rust C set-paths; demo crate + 188-dep lock + recipe + source helper + gate
4. [x] claim + plan-index
5. [ ] ./check.sh rust-russh green + verified-red
6. [ ] full ./check.sh green; review; ready + auto-merge

## Verified-red evidence
(to fill)
