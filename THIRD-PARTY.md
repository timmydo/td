# Third-party material

td's own code is MIT (see [LICENSE](LICENSE)). The material below is not
td's own work, or derives from someone else's, and keeps its own terms.
[LICENSES/](LICENSES) holds the texts those terms ask to travel with the
material: upstream's own copies, except where a row says otherwise. CC0
asks for none, and CC-BY-SA is named by its URI, as its terms allow.
Upstream sources td fetches at build time, and the crates in td-net's and
td-crypto's fetched closures, carry their own licenses and are not in this
tree.

## Code in td's programs

Permissive terms, which ask that the notice travel with the code.

| Where | What | From | Terms |
|---|---|---|---|
| `td-photo/src/transform.rs`, `td-photo/src/av1.rs` (the entropy coder; the encoder's cost table, RD scale and multipliers, fit constants, trellis and end-of-block token shapes; the `libaom_directional` test oracle), `td-photo/src/cdf.rs`, `td-photo/src/deblock.rs` | AV1 transforms, entropy coder, encoder decisions, default CDFs in libaom's form, loop filter (whose kernels dav1d shares), and a directional-prediction oracle in libaom's shape; where the AV1 specification defines the arithmetic, libaom is the form it was taken in | libaom 3.9.1, Copyright (c) 2001-2016, Alliance for Open Media (its LICENSE file says 2016) | BSD-2-Clause ([LICENSES/BSD-2-Clause-libaom.txt](LICENSES/BSD-2-Clause-libaom.txt)), with the Alliance for Open Media Patent License 1.0, which asks to sit at the root of the implementation's source: [td-photo/PATENTS](td-photo/PATENTS) |
| `td-photo/src/cdef.rs` | CDEF filter kernels | dav1d 1.5.1, Copyright © 2018-2019, VideoLAN and dav1d authors; Copyright © 2018, Two Orioles, LLC | BSD-2-Clause ([LICENSES/BSD-2-Clause-dav1d.txt](LICENSES/BSD-2-Clause-dav1d.txt)) |
| `td-photo/src/nef.rs` (the Nikon Huffman trees and the `nikon_load_raw` decode), `td-photo/src/color.rs` (`cam_xyz` construction) | Nikon NEF decoding, camera colour construction | dcraw 9.28, Copyright 1997-2018 by Dave Coffin | dcraw's terms ([LICENSES/dcraw-notice.txt](LICENSES/dcraw-notice.txt), its header): code other than its RESTRICTED (Foveon) functions is "free for all uses"; none of those is used |
| `td-json/src/lib.rs` (the `json!` macro family) | token-muncher macro | serde_json, by Erick Tryzelaar and David Tolnay | MIT, the arm td takes of serde_json's MIT OR Apache-2.0 ([LICENSES/MIT-serde_json.txt](LICENSES/MIT-serde_json.txt)) |
| `td-ui/src/vt_render.rs` (`BASE`, the default ink pair, `WORD_DELIMITERS`) | default palette, colours and word delimiters | foot, Copyright (c) 2019 Daniel Eklöf | MIT ([LICENSES/MIT-foot.txt](LICENSES/MIT-foot.txt)) |

## Shipped data

| Where | What | From | Terms |
|---|---|---|---|
| `td-compositor/src/font_data.rs` | font data, converted by td's importer | GNU Unifont 16.0.04 | OFL-1.1 (`td-compositor/assets/unifont-OFL-1.1.txt`, `unifont-COPYING`) |
| `td-mime/unicode/17.0.0/`, `td-mime/src/unicode_tables.rs` | Unicode Character Database, and tables generated from it | Unicode, Inc. | Unicode-3.0 (`td-mime/unicode/17.0.0/license.txt`) |
| `td-mime/leap-seconds/leap-seconds.list` | leap-second table | IERS, as distributed with tzdata | public domain |
| `td-civil/src/tzif.rs` (`days_from_civil`, `civil_from_days`) | the day-count conversions | Howard Hinnant's published algorithms | public domain |
| `td-photo/src/camera.rs` (the Nikon Z 8 entry) | black and white levels and the XYZ-to-camera matrix, mode `14bit-compressed` | RawSpeed, `data/cameras.xml` | CC-BY-SA-3.0 (<https://creativecommons.org/licenses/by-sa/3.0/>) |

The programs that embed the Unifont data print its notices with
`--font-license` where they offer that flag (td-dua, td-editor, td-setup,
td-taskmgr); td-compositor embeds it and does not yet.

## Test corpora and fixtures

| Where | What | From | Terms |
|---|---|---|---|
| `td-sh/spec/*.test.sh` except `smoke.test.sh` and `expansion.test.sh` | shell spec tests, unmodified; see `td-sh/spec/README` | Oils (oils-for-unix/oils) | Apache-2.0 ([LICENSES/Apache-2.0.txt](LICENSES/Apache-2.0.txt)) |
| `td-txt/spec/gnu-grep/` | GNU grep 3.11 `tests/` (`spencer1.tests` from Henry Spencer's regex suite, as GNU grep carries it) | GNU grep | GPL-3.0-or-later |
| `td-txt/spec/gnu-sed/` | GNU sed 4.2.2 `testsuite/` (`uniq.inp` and `uniq.good` hold PCRE source, © 1997-2000 University of Cambridge, under the licence it carries in that text; `xemacs.inp` an Automake notice) | GNU sed | GPL-3.0-or-later |
| `td-busd/spec/auth/` | dbus 1.16.2 `test/data/auth` | dbus | AFL-2.1 OR GPL-2.0-or-later ([LICENSES/AFL-2.1.txt](LICENSES/AFL-2.1.txt)) |
| `td-busd/spec/*.conversation` | traffic recorded from dbus-daemon, dbus-send, dbus-monitor and busctl (elogind 255), including generated introspection XML; program output, none of it taken from their source | dbus; elogind | listed for their output, under AFL-2.1 OR GPL-2.0-or-later and LGPL-2.1-or-later |
| `td-ui/spec/vt/libvterm-0.3.3.*` | generated from the libvterm 0.3.3 test suite | libvterm, Paul Evans | MIT (`td-ui/spec/vt/LICENSE.libvterm`) |
| `td-ui/tests/fixtures/us.xkb`, `us-keys.tsv`, `us-types.tsv` | compiled from xkeyboard-config 2.44, and xkbcommon's reading of it | xkeyboard-config | MIT/X11 and HPND-style notices (`td-ui/tests/fixtures/XKB-COPYING`) |
| `td-secret/tests/p256_vectors.txt` (its NIST rows; the rest are OpenSSL output), and the SP 800-38A vectors in `td-secret/src/fido_aes.rs`'s tests | test vectors from NIST's CAVP corpora and SP 800-38A | NIST | public domain (a US government work) |
| `engine/tests/fixtures/flathub-*.hex` | OSTree objects captured from Flathub, the Firefox Flatpak's metadata among them | Flathub | each publisher's terms; small, mostly factual metadata |
| `td-photo/tests/fixtures/nikon_ref.py` | test oracle, transcribed from dcraw 9.28's `nikon_load_raw` | dcraw, Copyright 1997-2018 by Dave Coffin | dcraw's terms (above) |

## Bootstrap build inputs

Patches to, and adaptations of, the build files of the projects td
bootstraps. They keep their own terms; the GPL and LGPL texts are in
[LICENSES/](LICENSES).

| Where | What | From | Terms |
|---|---|---|---|
| `seed/patches/*.patch` | patches to binutils 2.20.1a, GCC 2.95.3 and 4.6.4, and glibc 2.2.5 and 2.16.0, unmodified | GNU Guix's commencement patches, by Jan Nieuwenhuizen and the Guix authors | GPL-3.0-or-later (Guix), against GPL-3.0-or-later binutils and GCC 4.6.4, GPL-2.0-or-later GCC 2.95.3, LGPL-2.1-or-later glibc |
| `recipes/src/recipes/coreutils-mesboot0-*.patch` except the two below | patches to coreutils 5.0 (their SPDX header lines are not carried; the contributors they name are) | live-bootstrap; by Andrius Štikonas (2021-2022), Samuel Tyler (2021) and Emily Trau (2023); `mbstate` takes code from glibc 2.32 (FSF, LGPL-2.1-or-later) | GPL-2.0-or-later |
| `recipes/src/recipes/coreutils-mesboot0-touch-dereference.patch` | `touch -h`, backported to coreutils 5.0 from later coreutils (commit 9e13b6a, 2009), its help text and code | coreutils, © Free Software Foundation; by Eric Blake (2009), adapted by Andrius Štikonas (2022) for live-bootstrap | GPL-3.0-or-later, as that coreutils source is |
| `recipes/src/recipes/coreutils-mesboot0-uniq-fopen.patch` | a backport of coreutils commit 786ebb2 (2005) | coreutils, © Free Software Foundation; by Paul Eggert (2005), adapted by Emily Trau (2023) for live-bootstrap | GPL-2.0-or-later, as that coreutils source is |
| `recipes/src/recipes/bash-mesboot.rs` (five substitutions) | hunks of live-bootstrap's bash 2.05b patches (mes-libc, tinycc, missing-defines, locale, dev-tty) | live-bootstrap; © 2021 Samuel Tyler | GPL-2.0-or-later |
| `recipes/src/recipes/oyacc.rs` (two edits) | hunks of live-bootstrap's oyacc 6.6 patches (tcc, meslibc) | live-bootstrap; © 2025 Samuel Tyler | BSD-3-Clause ([LICENSES/BSD-3-Clause-live-bootstrap.txt](LICENSES/BSD-3-Clause-live-bootstrap.txt): live-bootstrap's template, its holder line filled in from the patches' SPDX lines) |
| `recipes/src/recipes/{bash-mesboot,bash-mesboot-common,bash-mesboot-builtins,coreutils-mesboot0,grep-mesboot0,diffutils-mesboot0,gawk-mesboot0,sed-mesboot0}.mk`, and the `-D` lists in `recipes/src/recipes/{bash-mesboot,coreutils-mesboot0,sed-mesboot0,grep-mesboot0,gawk-mesboot0,diffutils-mesboot0}-config.h` | adapted from live-bootstrap's `mk/` makefiles | live-bootstrap; © 2021 Andrius Štikonas, 2021-2022 Samuel Tyler, 2021 Paul Dersey, 2023 Emily Trau | GPL-3.0-or-later |
| `recipes/src/recipes/oyacc.mk` | adapted from live-bootstrap's oyacc makefile | live-bootstrap; © 2019 Brian Callahan, 2025 Samuel Tyler | CC0-1.0 |
| `recipes/src/recipes/patch-mesboot.mk` | from the Makefile GNU patch 2.5.9's configure generates | GNU patch | GPL-2.0-or-later |
| `recipes/src/recipes/make-mesboot0.kaem` | transcribes the `build.sh` GNU Make 3.80's configure emits | GNU Make | GPL-2.0-or-later |
| `recipes/src/recipes/make-mesboot0-config.h`, `recipes/src/recipes/patch-mesboot-config.h` | configure output of GNU Make 3.80 and GNU patch 2.5.9 | GNU Make; GNU patch | GPL-2.0-or-later |
| `recipes/src/recipes/patch-mesboot-stdbool.h` | gnulib `stdbool.h.in` | gnulib, FSF | GPL-2.0-or-later |

`recipes/src/recipes/tcc.kaem` and `tcc-config.h`, which replace tcc's
`bootstrap.sh`/`boot.sh` with a command sequence and its configure
defines, are td's own, as are the Rust ports of Mes's boot scripts; only
what transcribes upstream text is listed above.

`builder/src/mes_boot.rs` lists Mes's library sources in the order GNU
Mes's `build-aux/configure-lib.sh` (GPL-3.0-or-later, Jan Nieuwenhuizen)
gives them; the lists are build facts, credited here.
