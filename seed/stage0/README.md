# stage0 seed — the irreducible, auditable bottom of td's source bootstrap

Brick 0 of the source-bootstrap track ([[source-bootstrap]], DESIGN §5, CLAUDE.md north star:
*no guix bytes*). This is the tiny, hand-auditable seed td's `/td/store` toolchain is built up
from — NOT a guix-built binary.

## Provenance

Vendored verbatim from **stage0-posix-x86 commit `3b9c2bb6d4155e4f2e5f642b5e0f59255dfc5934`**
(github.com/oriansj/stage0-posix, the bootstrappable-builds Full-Source Bootstrap that guix
itself uses). Two AMD64 seed binaries + the hex source + the seed kaem script:

| file | bytes | sha256 |
|---|---|---|
| `bootstrap-seeds/POSIX/AMD64/hex0-seed` | 229 | `66c95985e668f20f2465c2b876f83fef066fd7c8c2dd3adb51a969f2d7120c8b` |
| `bootstrap-seeds/POSIX/AMD64/kaem-optional-seed` | 618 | `153b8915b73bd07132b59538d10fe53d26578eb160a67db72af07aaa61c51b3b` |
| `AMD64/hex0_AMD64.hex0` | — | the hex0 assembler's own source (hex digits + `#` comments) |
| `AMD64/kaem-minimal.hex0` | — | the minimal kaem's source |
| `AMD64/mescc-tools-seed-kaem.kaem` | — | the seed build script (2 steps, below) |

## Why a binary seed is still auditable

`hex0-seed` is a 229-byte hand-written ELF — small enough to disassemble by hand — and it is
**self-reproducing**: assembling its OWN source `hex0_AMD64.hex0` with it yields a byte-identical
`hex0-seed`. Likewise `kaem-optional-seed` is reproduced by assembling `kaem-minimal.hex0`. So the
binary seeds are verifiable by reading the hex source and re-assembling — you do not have to trust
the bytes. The `bootstrap-seed` gate (`mk/gates/`) proves exactly this on every run.

## The seed build (`mescc-tools-seed-kaem.kaem`)

```
hex0-seed   hex0_AMD64.hex0   -> artifact/hex0     # the 229B seed assembles a full hex0
hex0        kaem-minimal.hex0 -> artifact/kaem-0   # which assembles kaem-0
```

This is the foundation; later bricks drive `artifact/kaem-0` over the rest of the
stage0-posix → mes → tinycc → gcc → glibc chain, every stage `--prefix=/td/store`, no guix
process and no guix bytes. None of this seed is guix-built.
