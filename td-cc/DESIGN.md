# td-cc: the native compiler launcher

The standard td image exposes `cc`, `gcc`, `c++`, and `g++` through one
source-built, dependency-free Rust launcher. `td-cc` itself selects C.
Only the invocation basename selects C++; compiler arguments retain their
original bytes and ordering and are never interpreted by a shell.
Callers must preserve the selected invocation name: executing the fully
resolved `td-cc` path selects C even when resolution began at `c++`.

The target recipe compiles the exact declared GCC, binutils, glibc, and
unwinder paths into the launcher. A host Cargo build without those settings
is test tooling and refuses normal execution. Runtime environment variables
cannot change the compiled defaults. The launcher executes the selected GCC
binary directly, preserving the compiler's process identity, signals, and
exit status. A failure to start it reports an error and exits 127.

Defaults select td's assembler/linker, libc headers and startup objects,
static libgcc, the exact glibc interpreter and runtime search path,
frame pointers, line-table debug information, and a deterministic GNU
build ID. C++ also selects static libstdc++. An explicit `-nostdinc`
argument omits the default libc include path for freestanding builds.
The current source directory maps to `/td-build`. User arguments follow
these defaults and can override normal compiler options; this is a
development convenience, not a confinement or recipe-admission boundary.
Recipe builds retain their own declared tools, flags, remaps, and
debug-companion checks from td-profiler/DESIGN.md.

Search-path defaults are additive: the declared libc and unwinder `-L`
paths precede user paths, and interpreter/rpath options remain present
for custom `-nostdlib` links. This launcher does not promise transparent
replacement of arbitrary compiler configurations.

The source-built GCC keeps its unwinder in libgcc.a. The launcher package
contains a deterministic compatibility libgcc_eh.a made from that archive,
so static Rust links can resolve their existing unwinder request. This is
a source-built compilation input, with the same provenance as GCC.

The recipe compiles and runs C using libc and Linux headers, C++ using
containers and exception unwinding, and an offline Cargo build and test.
Compiler probes run without an ambient tool PATH. The launcher is linked
statically and split through the shared target debug policy. The image
stages the launcher, Rust, GCC, binutils, and their declared runtime closure
at their canonical store paths and provides ordinary /bin entry points.

Shipping these tools alone does not make a guest ready for the repository's
whole workflow. The native control-plane helper path uses the same compiler
launcher for static GNU links. Private writable build storage and workspace
provisioning remain separate capabilities; the VM manager must not infer
readiness from compiler presence.
