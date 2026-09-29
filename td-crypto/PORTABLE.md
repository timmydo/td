# Portable build inputs

## Status

M03b2a implements header preparation only. The isolated portable build and
artifact qualification remain M03b2b. This is a host build path for the
standalone td-mta executable; it does not grant the source-bootstrap provenance
of td's target image graph. Nothing prepared here is automatically admitted
as a target recipe input.

## x86-64 musl headers

The header source is musl 1.2.5, matching the version named by Rust 1.96.0's
`src/ci/docker/scripts/musl-toolchain.sh`. Rust's target standard library owns
the linked libc and its upstream patches. This command generates headers;
it does not build or replace libc, and does not claim that an unpatched musl
1.2.5 runtime is suitable for deployment. The complete Rust target archive
pin and runtime qualification belong to M03b2b.

| Input | Pin |
| --- | --- |
| Upstream archive | `https://musl.libc.org/releases/musl-1.2.5.tar.gz` |
| Archive bytes | `1080786` |
| SHA-256 | `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4` |
| Architecture | `x86_64` |
| Installed header count | `218` |
| Header tree SHA-256 | `a673b15579a83881c2d9aa61a65ce26dd62557d16c497e5a04e4e6a8bb6d601b` |

Prepare a previously fetched archive offline:

```text
td-builder gate-crates crypto-musl-headers --archive /path/to/musl-1.2.5.tar.gz
```

The command prints the prepared directory under the worktree's
`.td-build-cache/crypto-musl-x86_64-1.2.5-<output-digest>`. The archive argument
must name a regular file, not a symlink. It verifies a bounded in-memory copy
of the archive and passes those same bytes to td's existing gzip/tar reader. An
oversized archive fails the bounded read before hashing. It
does not fetch, run `configure`, execute `make`/`sed`, or search host headers.
Its std-only generator follows musl's installed-header rules: public include
files, architecture bits overriding generic bits, guarded alltypes
declarations, and syscall aliases. This is a version-specific generator;
changing the archive or architecture requires reviewing the rules and output.

The tree digest covers each relative header path in ascending byte order,
then NUL, the decimal byte length, NUL, and the file bytes, concatenated into
SHA-256. It was derived from the complete upstream `make install-headers`
output, independently of td's generator. Every invocation checks the generated
tree against it, including the header count. This catches wrong overrides,
declaration transformations, omitted files and extra files.

Output contains `include/`, the upstream `COPYRIGHT`, and a `SOURCE` receipt
naming the URL, archive hash and header-only scope. Musl is predominantly
MIT-licensed; retain the complete upstream notice for its additional notices.
A fresh output is staged privately and published as one directory. The suffix
hashes all output file paths and bytes with the same encoding as the header
digest, so a changed receipt or license selects a new cache entry. Existing
output is re-read and compared with freshly verified source bytes, including
the license and receipt. Modified, missing, added or symlink files fail;
the command does not silently repair them. A concurrent identical publication
may be reused. Empty extra directories carry no build input and are ignored.
The host cache is caller-owned, not a security boundary against another process
with the same uid. A later isolated compilation must bind its verified input
read-only and exclude undeclared paths and ambient flags.
On a cache mismatch, remove only the named cache entry and retry. This cache
is reconstructible and publication is not fsynced. A hard-killed preparer may
leave a private `crypto-headers-<pid>-<attempt>` scratch directory; remove it
only after confirming that preparer is no longer running.

Other architecture header rules are not yet implemented. This x86-64 command
does not prevent a separately pinned and tested ARM path later.
