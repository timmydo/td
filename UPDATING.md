# Keeping td's pinned software current

td builds only from what it pins, so nothing moves until a reviewed commit moves
it. This file is the procedure for finding what upstream has released past
those pins and for carrying each bump to a landed branch. `AGENTS.md` owns the
trust model and `DEVELOPMENT.md` the commit, review and `ready` workflow; this
file restates neither.

Run the check at least weekly, and at once when an advisory names a pinned
project. Security releases go first and land on their own.

# What td pins

| Class | Where it is pinned | How it moves |
| --- | --- | --- |
| Source tarballs, data and the pinned Rust crates | `recipes/src/source_pins.rs`, watched from `recipes/src/source_watches.rs` | [Source pins](#source-pins) |
| The Rust toolchain (rustc, rust-std, Cargo, Clippy) | the four `rust-*-dist` source pins | [Rust toolchain](#rust-toolchain) |
| fd, ripgrep and uutils' crate closures | `recipes/locks/*/Cargo.lock` | with their crate pin ([Crate pins](#crate-pins)) |
| The Debian build root: every C build tool, and the glibc and gcc-runtime td imports from it | `recipes/src/debian-root.lock` | [Debian build root](#debian-build-root) |
| td-net's crates | `net/Cargo.lock` | [Rust dependency closures](#rust-dependency-closures) |
| td-crypto's and td-mta's private closures | their manifests and locks, pinned by `builder/src/crypto_policy.rs` | [Rust dependency closures](#rust-dependency-closures) |

glibc and GCC are not source pins: td imports Debian stable's packages
(`glibc-x86-64`, `gcc-runtime`), so their security fixes arrive with a
re-resolved build root, and their upstream release series is Debian
stable's. Moving to a newer upstream series means moving the root to a newer
Debian release, which is its own decision (below).

Flathub applications and Claude Code float by design (`APPLICATIONS.md`
§W.9): td-app fetches their newest signed release, so there is nothing to
bump. What is reviewed there is the declaration and the signing key.

# Finding what is behind

`td-feed` is td-net's feed applet. Build td-builder and td-net, then ask every
upstream:

```text
cargo build --release --manifest-path builder/Cargo.toml
cargo build --release --locked --manifest-path net/Cargo.toml
net/target/release/td-net feed outdated
```

`td-feed outdated [--transitive] [pins|build-root|cargo]...` changes nothing.
It prints three sections, all of them by default:

- `pins`: each source pin's version against the newest release its watch
  finds. A line reads `-> NEW`, `current`, or `held: REASON`. `(the index no
  longer lists OLD)` means the pinned line is gone: an end-of-life kernel
  series, or a Rust release older than the current stable. `not listed by
  its index` with nothing newer means upstream withdrew the pinned release.
- `build-root`: the build root's image tag against the registry's newest tag
  of the same shape, then every locked `.deb` that the live Debian archives
  have newer, as `PACKAGE LOCKED -> LIVE`. These are the security and stable
  updates a re-resolve would take. Packages the image layer already carries
  are not compared: their updates arrive with a newer image tag, which the
  first line reports.
- `cargo`: every committed Cargo.lock's registry crates against the crates.io
  index. It lists by name each direct dependency with something newer and
  each yanked locked version. It shows both the newest semver-compatible
  release and, marked `(major)`, the newest overall. Transitive crates are
  counted, not listed: with a semver-compatible release, or only past a
  major. `--transitive` lists them. A candidate must be a semver release
  whose `rust_version` the pinned Rust toolchain meets. Semver-compatible is
  not a promise that `cargo update` takes it: a dependent's own requirement
  can hold it back. `recipes/locks/*` are pinned crates' own published
  locks, so they show counts and yanks only: they move with their pin.

It exits 0 when everything is current, 3 when something is behind, and 1
when a watch could not be answered (1 wins over 3). What no edit to that
lock alone can move is reported but does not count. That is a transitive
crate whose only newer release is a new major, and anything in
`recipes/locks/*` except a yank. A yank counts even there, as a reason to
look at the pinned crate's newer releases. A direct dependency behind a
major counts until it is ported.

Read a `-> NEW` as "upstream has released NEW", then decide:

- LibreSSL's directory carries development releases beside stable ones. Take
  the release libressl.org names as the latest stable.
- A Git-tag watch (zlib, Nerd Fonts, the ripgrep
  seed) can see a tag before its release tarball exists. Fetch the tarball
  before trusting the tag.

Each watch is a row in `recipes/src/source_watches.rs`: an index URL and a
pattern in which `{}` stands for the version. A `PointReleases` watch (the
kernel) takes only versions of three or more parts. A fresh line such as
7.3 therefore waits for 7.3.1, as td follows the newest stable point
release. A version whose release part contains `alpha`, `beta`, `rc`,
`pre` or `dev`, or carries a `-TAG` pre-release, is never taken. `+build`
metadata is ignored. `td-recipe-eval source-watches` prints the joined
table.

Before bumping, read the release notes and advisories between the pin and
the target: what changed, what it fixes, and whether a fix reaches td's
build (curl's `--without-nghttp2` build is immune to an HTTP/2 bug, for
example). The commit message records the advisories each bump takes.

# Source pins

One pin, or one tight group (the four Rust components, the Nerd Fonts archive
and its licences), per commit.

1. Edit the `PinDef` in `recipes/src/source_pins.rs`: `url`, `file`,
   `version` and `sha256`. Also change any version the pin's comment or
   `catalog_carries_all_legacy_source_locks` restates. A test refuses a
   `version` that its `url` or `file` does not name whole, or that the
   watch's file pattern does not produce.

2. Take the SHA-256 from upstream's own publication where there is one:

   | Upstream | Where its SHA-256 is |
   | --- | --- |
   | kernel.org (Linux, Git) | `sha256sums.asc` in the release directory |
   | curl's CA extract | `cacert-DATE.pem.sha256` beside the file |
   | LibreSSL | the `SHA256` file in its directory |
   | OpenSSH | the release notes, `txt/release-X.Y`, in base64 |
   | zlib | zlib.net's release line, and the GitHub release asset's digest |
   | crates.io | the version's `cksum` in the sparse index |
   | Rust | `xz_hash` in that date's `channel-rust-stable.toml` |
   | GitHub release assets (Nerd Fonts, the ripgrep seed) | the asset's digest |

   Where upstream publishes only a signature (curl tarballs, tzdata), verify
   it with the maintainer's key, then hash the verified bytes. Where it
   publishes neither (json-c's S3 tarball, LVM2's sha512 list, popt), hash
   the download and cross-check it against a second upstream copy or digest.
   Name the source of each hash in the commit message.

3. Find every other place that restates the old version, and change each
   one that means this pin:

   ```text
   git grep -nF OLD-VERSION
   ```

   Recipes name their version
   (`Recipe::mesboot("curl-x86-64", "NEW-VERSION")`), and their tests,
   design documents and other crates' fixtures often repeat it. Hits that
   only happen to match stay unchanged (td-mta's `421 4.3.2` SMTP replies
   are enhanced status codes, not a LibreSSL version).

4. Fetch and verify the new source and regenerate the seed digest table,
   as `DEVELOPMENT.md` gives it (rebuild the evaluator, warm the cache, then
   write the table aside).

5. Run the checks it reaches:

   ```text
   td-builder affected-checks --run
   ```

   A source pin re-keys every recipe check. A change to the boot path (the
   kernel, `system-x86-64`, the boot crates `DEVELOPMENT.md` names) also
   deserves the integration tier before pushing:
   `td-builder check integration`.

6. Run `td-feed outdated pins` again: the pin now reads `current`.

## Crate pins

fd, ripgrep and uutils pin a crates.io `.crate`. Their recipe builds against
the crate's own published Cargo.lock, committed verbatim at
`recipes/locks/<name>/Cargo.lock`. Bump the pin as above, then replace that
lock with the one inside the new `.crate`. Nothing else edits it. Check the
new release's MSRV against the pinned Rust toolchain.

## Rust toolchain

The four `rust-*-dist` pins move together, to one dated directory of
`static.rust-lang.org/dist`, with hashes from that date's
`channel-rust-stable.toml`. `td-rust` downloads the same pinned components,
so it follows them. The version and the dist date are restated elsewhere:
grep for both. The places include:

- `AGENTS.md`, the normative toolchain paragraph, and `README.md`;
- the engine's and builder's toolchain code (`builder/src/stage0.rs`,
  `builder/src/host_bin.rs`, `engine/src/target_profile.rs`),
  `td-link/src/plan.rs` and the rust-toolchain test;
- `engine/src/rust_dist.rs`, which names the libLLVM files a new LLVM
  renames.

Some restatements carry facts about the release, not just its number;
check each against the new release rather than renaming it. td-link
accepts the linker options rustc's GNU-cc flavor writes plus td's own
(diff `compiler/rustc_codegen_ssa/src/back/link.rs` and `linker.rs`
between the two release tags), and design documents cite which std
APIs are still unstable. Historical records, such as an audit naming
the store path it ran against, stay as written.

New Clippy lints can red the `-D warnings` and `-D unused` legs, so fix
those in the same commit.

td-crypto's portable kit (`builder/src/crypto_portable.rs`,
`td-crypto/PORTABLE.md`) pins its own dist date, its own copies of the
host component hashes and the musl and aarch64-musl rust-std hashes. Move
it in a commit of its own: the kit's reconstructed NAR digests
(`builder/src/crypto_isolated.rs` and PORTABLE.md) are rebuilt with
`td-builder gate-crates crypto-portable-prepare` for each architecture,
and its rustdoc API diagnostic (`builder/src/crypto_api.rs`) pins
rustdoc's JSON `format_version`, which a new release usually advances.
Before moving that number, diff `src/rustdoc-json-types/lib.rs` and
`src/librustdoc/json/conversions.rs` between the release tags: the
diagnostic tolerates added fields, but not a renamed or removed field
it reads, a field whose meaning changes (such as the source of
`target_features[].globally_enabled`), or a new field inside an item
body carrying an `id` key, which its walker follows. Then grep
PORTABLE.md and `crypto_api.rs` for the old number, which they
restate. Prove the kit with `crypto-portable-build` and
`crypto-portable-cross-build` over the same archive directory.

## Holds

A watch that is `Watch::Held(REASON)` is not followed, and `outdated`
prints why. Holds require a concrete reason in the source watch catalog.

## Unreleased fixes

When an advisory affects upstream's newest release and upstream has fixed
it only on its development branch, the recipe may carry the fix as
`Step::substitute_text` edits right after unpacking, each `TextEdit`
expecting exactly one match. Carry upstream's lines verbatim rather than
writing td's own fix, and take every upstream commit to the file after
the release up to and including the fixing commit, so the edited file
matches upstream's at that commit; check that byte for byte. Later
commits that fix no advisory wait for a release. Name the advisories and the upstream commits in
a comment beside the edits and in the commit message. A release that
contains the fix no longer matches, so the bump past it fails until the
edits are deleted: they cannot outlive their need.

## Adding a pin

A new `PinDef` names its `version`. Give it exactly one row in
`recipes/src/source_watches.rs`, or a hold with its reason; the join refuses
a pin without a watch and a watch without a pin. Choose an index that
upstream owns: a release directory, a release feed, the crates.io sparse
index, or the project's Git ref advertisement
(`https://HOST/OWNER/REPO.git/info/refs?service=git-upload-pack`, pattern
`refs/tags/v{}`). Then confirm `td-feed outdated pins` reads it as
`current`. A watch-only edit moves no pinned byte, which is why the table
lives apart from the pins.

# Debian build root

The lock's header comment is the authoritative procedure. In short:

1. Set `image` to the newest tag of the same shape that `outdated` reports
   (for example `library/debian:trixie-20261020-slim`), and `snapshot` to a
   recent `snapshot.debian.org` timestamp, `YYYYMMDDT000000Z`.
2. Resolve, review the diff, and pin the assembled tree:

   ```text
   td-feed debian-root resolve recipes/src/debian-root.lock
   td-feed warm build-root
   cargo build --release --manifest-path builder/Cargo.toml
   td-builder build-root pin ~/.td/sources recipes/src/debian-root.lock WORK
   ```

3. Review the package diff against what `outdated` listed. `libc6` and
   `gcc-14` carry the glibc and gcc-runtime td ships, so their changelogs
   say which security fixes the bump takes.

A new root rebuilds glibc and everything that links it, so this is the most
expensive bump and lands as its own commit. Changing the `archive` lines to a
new Debian release (trixie to forky) also changes the glibc and GCC series td
ships, and is a reviewed decision of its own.

# Rust dependency closures

Every new crate name a lock gains is an external dependency under
`AGENTS.md` principle 2: the commit calls it out and carries the owner's
sign-off.

- td-net: `cargo update --manifest-path net/Cargo.toml` takes the
  semver-compatible releases its dependents allow. Check each new
  release's `rust-version` against the pinned toolchain. A major moves by
  editing `net/Cargo.toml` and adapting the code. The host preparation
  warms the vendored closure from the committed lock, and `net-test` runs
  td-net's suite.
- td-crypto and td-mta pin their direct dependencies exactly (`=1.18.2`)
  in their manifests and in `crypto_policy::DEPENDENCIES` (td-crypto's)
  and `POSTGRES_DEPENDENCIES` (td-mta's), so `cargo update` alone moves
  only what those exact pins leave free. Edit each manifest and its list
  together, then update the lock. After that,
  update what `builder/src/crypto_policy.rs` pins: the manifest and lock
  digests, and the active-graph strings (`ACTIVE`, `POSTGRES_ACTIVE`).
  Also update the inventory tables in `td-crypto/DEPENDENCIES.md`. A
  tokio-postgres bump also amends the normative documents that name its
  version: `AGENTS.md`, `td-crypto/DESIGN.md`, `td-mta/DESIGN.md`,
  `td-mta/POSTGRES.md` and `UNSAFE.md`. Check each crate with
  `td-builder gate-crates crypto-cargo test --manifest-path
  td-crypto/Cargo.toml` (and `td-mta/Cargo.toml`). `td-crypto/DESIGN.md`
  owns the admission rules.

# Landing

Each bump is one green, self-contained commit with its own review record
(`DEVELOPMENT.md`). The message gives the old and new versions, the
advisories taken, the source of each hash, and the checks run. Then
`td-builder ready` and push, as for any change.
