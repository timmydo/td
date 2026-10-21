# td-link: the Rust link driver

`td-link` is the linker every td Rust recipe names: rustc's `-C linker`
for direct recipes, and Cargo's target linker for Cargo recipes, which
Cargo also hands its build scripts and proc macros. It is a
dependency-free, `unsafe`-free Rust program. It reads the gcc-style link
line rustc writes, adds td glibc's and gcc-runtime's start files, search
paths and loader, and runs the upstream toolchain's `rust-lld` in its GNU
flavor. No GCC or binutils is involved in a link it drives.

## Configuration

The target recipe compiles three store paths into the driver: the
toolchain's `rust-lld`, td glibc's `lib`, and gcc-runtime's `lib`. They
are store references of the output, so every recipe that names the
driver has the linker, libc and libgcc it links in its closure. A host
Cargo build without them is test tooling and refuses to link. Nothing in
the environment changes them.

## What it accepts

The driver accepts the arguments rustc 1.96's GNU-cc linker flavor
writes for `x86_64-unknown-linux-gnu` and refuses everything else, so a
compile request, an unexpected option or a host library fails in the
driver rather than inside a link:

- inputs ending `.o`, `.a`, `.rlib`, `.so` or naming `.so.`; any other
  file, a source file included, is refused, as are `-c`, `-S`, `-E`,
  `-x`, the `-M` family and standard input;
- `-o` and `-nodefaultlibs` (or `-nostdlib`), which rustc always passes:
  a line without either is refused, since td-link adds no default
  library and names no default output;
- `-l` and `-L` (joined or separate), `-u`, `-rdynamic`;
- the output kind: `-shared`, `-static`, `-static-pie`, `-pie`,
  `-no-pie`; a conflicting combination is refused;
- `-nostartfiles`;
- `-m64`, `-static-libgcc`, `-fuse-ld=lld`, and `-B` naming the
  `gcc-ld` directory beside the compiled `rust-lld`, which are no-ops:
  the driver is the linker rustc's self-contained lld path would find;
- GCC `@file` response files, nested to depth 16, in libiberty's syntax;
  their contents are checked as if written on the line.

Linker options, whether `-Wl,` pieces (split at commas) or `-Xlinker`
arguments, are an allowlist of what rustc writes: `--as-needed`,
`--no-as-needed`, `-Bstatic`, `-Bdynamic`, `--whole-archive`,
`--no-whole-archive`, `--gc-sections`, `--no-gc-sections`,
`--eh-frame-hdr`, `-O1`, `--strip-debug`, `--strip-all`,
`--no-undefined-version`, `--build-id=sha1`, `--version-script=FILE`,
`-soname=NAME` (one file name), and `-z` followed, in the next piece, by
`noexecstack`, `relro`, `norelro` or `now` (a top-level `-z` takes the
same keywords). Everything else is refused, notably:

- anything naming an interpreter (`--dynamic-linker`, `-dynamic-linker`,
  `--no-dynamic-linker`, `-I`): the interpreter is the driver's;
- an `@file` piece, which LLD would expand unseen, and an empty piece,
  which would make the next argument an option's operand;
- `-rpath`, `-L`, `-l`, `-T`/`--script`, `--sysroot`, `--dynamic-list`
  and positional inputs given through `-Wl,`.

No link input, `-L` directory or version script may resolve, after
`..` is folded and, when it exists, symbolic links are followed, under a
host system prefix: `/usr`, `/lib`, `/lib32`, `/lib64`, `/libx32`, `/bin`,
`/sbin`, `/etc`, `/opt`, or a Guix or Nix store (`/gnu`, `/nix`), and none
may start with `=`, which LLD reads in a search directory as the
sysroot (empty without `--sysroot`, so `/`), or GNU ld's `$SYSROOT`;
the refusal covers inputs and version scripts too, over-broadly. A
`-l` name may not contain `/`, which would walk out of the search paths.
This is a deny list of where a host's or the Debian build root's
libraries live, not an allowlist of the td store: the store and build
directories differ between the image, the builder's sandbox and a check
run.

Before linking, every input the line names must begin as an ELF object
or an ordinary archive (`!<arch>`): LLD reads any other file but LLVM
bitcode as a linker script, which could name host libraries or lay out
sections, and a thin archive names its members by path. Every link
passes `--no-dependent-libraries`, so an object's `.deplibs` section
cannot make LLD open a library or path the line does not name. The
checks cover the paths the line names. A library LLD finds itself for `-l` in an admitted directory is
not opened by td-link: td glibc's `libc.so` is itself a linker script,
and a symbolic link in such a directory is followed by LLD alone. The
recipes' link-map and output checks, not td-link, bound what `-l`
finds.

## What it links

| Output | Mode | Start files | End files |
|---|---|---|---|
| dynamic PIE (default) | `-pie`, td glibc's loader | `Scrt1.o crti.o crtbeginS.o` | `crtendS.o crtn.o` |
| dynamic `-no-pie` | `-no-pie`, td glibc's loader | `crt1.o crti.o crtbegin.o` | `crtend.o crtn.o` |
| `-static` | `-static -no-pie` | `crt1.o crti.o crtbeginT.o` | `crtend.o crtn.o` |
| `-static-pie` | `-static -pie --no-dynamic-linker -z text` | `rcrt1.o crti.o crtbeginS.o` | `crtendS.o crtn.o` |
| `-shared` | `-shared` | `crti.o crtbeginS.o` | `crtendS.o crtn.o` |

`crt1.o`, `Scrt1.o`, `rcrt1.o`, `crti.o` and `crtn.o` are td glibc's;
`crtbegin*`/`crtend*` are gcc-runtime's. Every link also passes
`-m elf_x86_64 --hash-style=gnu --build-id=sha1 --eh-frame-hdr`, then
the caller's arguments in order, then `-L` for td glibc and gcc-runtime
after the caller's. GCC's own spec omits `--eh-frame-hdr` for a static
non-PIE link, but rustc asks for it on every link, so the driver's header
only repeats it. Dynamic executables and shared objects get td glibc's
`lib` as their run path, written as `DT_RPATH` (`--disable-new-dtags`)
as GNU ld wrote it: it also governs the libraries a dependency loads, and
`LD_LIBRARY_PATH` cannot pre-empt it. A dynamic output that links
gcc-runtime's unwinder lookup also needs `ld-linux-x86-64.so.2`, its own
interpreter, for glibc's `_dl_find_object`.

libgcc is static in every output, as it was under td's GCC: rustc's
dynamic `-lgcc_s` is linked as gcc-runtime's `libgcc_eh.a` and
`libgcc.a`, so no output gains a `libgcc_s.so.1` dependency. Every other
way of naming the shared libgcc (`-l:libgcc_s…`, an input file named
`libgcc_s…`, `-Wl,-lgcc_s`) is refused. The driver adds no library of
its own: rustc names them all.

When the caller used a response file, or the argument list does not fit
(`E2BIG`), the driver hands LLD its own response file beside the output
and removes it once LLD returns. LLD's exit status is the driver's; a
signal death exits `128 + signal`.

## Provenance and the recipe

The `td-link` recipe builds in the Debian build root. A throwaway first
stage is linked by the root's GCC with td glibc's and gcc-runtime's start
files; it links the shipped driver, which then links itself again, and
the two must be byte-identical. The linked bytes are rust-lld's, driven
by td-link's own logic, with no object of the root's in them; that the
second and third stages agree shows the root's GCC left no trace in the
driver's behaviour. The root's `objcopy` then splits the static
ET_EXEC's debug companion and strips the shipped file, as it does for
every build-root output, so td-link is a build-root member of every
closure that names it.

`td-link-test` runs with no GCC or binutils in its inputs and no `PATH`.
It links a proc-macro dylib (which rustc then loads), a cdylib, and one
probe program as a dynamic PIE (linking the cdylib), a static ET_EXEC
and a static PIE. Each program checks its own ELF type and interpreter,
that LLD wrote it, that an unwind passes through gcc-runtime's unwinder,
and that a thread runs; each also checks the cdylib and the proc macro,
the static ones first. The dynamic program and both shared objects must
carry td glibc's `lib` as `DT_RPATH`, no `DT_RUNPATH`, and need only
`libc.so.6`, `ld-linux-x86-64.so.2` and, for the program, the cdylib;
the static programs need nothing.

C compiled by Cargo build scripts still uses td's GCC and is outside this
driver (`BUILD-ROOT-PLAN.md`).
