# td build and trust model

This document is normative for td's two build graphs, its trust zones and
every class of code or data td takes without building it from source.
`AGENTS.md` keeps the invariants every task needs and routes here; read
this document before changing recipe inputs, source pins, the Debian build
root, the Rust toolchain, target compiler flags, what an image ships, or
how a foreign payload enters or runs. Adding a class of foreign output, of
upstream data or of code td does not build is an amendment to this file.
Each section keeps the name it had when it lived in `AGENTS.md`, so a
citation of `AGENTS.md` by one of these names means the section here.

td has two build graphs. Do not confuse the host control plane with the
target distribution artifact graph.

## Trust zones

td has three trust zones. Zone one is the target artifact graph, built from
pinned sources by the Debian build root's and the upstream Rust toolchain's
compilers, with imported Debian glibc and libgcc. Zone two is the host control
plane that evaluates and builds that graph but never enters it. Zone three is
each marked foreign prebuilt
application payload. A zone-three payload may live in `/td/store` and ship in
the image, but it is never a tool, compilation input, or execution input to a
source-built output. Only the explicit read-only `payload_inputs` channel may
carry it into image composition or runtime association; td runs it only
through the application confinement path described below and in
`APPLICATIONS.md`. Flathub payloads, the second class of foreign output
below, run through the same path, except that a Flathub runtime a td
workspace names also runs in that workspace's jail, which is not that
path. No Flathub byte is a zone-two tool either, so such a workspace is
no host for the control plane: its `PATH` falls through td's `/bin` to
the runtime's `/usr/bin` and td's glibc loads from the runtime there.
Do not build, run or check td's control plane in a workspace that names
a Flathub runtime (td-agent/DESIGN.md §8, Runtime); `native-glibc`, td's
own trees and glibc, is not one. td has no `/usr`, which
the system recipe pins, so td-builder refuses to run on a system whose
os-release says td and that has one (but for a few verbs that build,
compile, link or check nothing, `affected-checks` among them, which
reads git; its provisioning refuses again). Cargo run outside
td-builder is not refused: by hand, or by td-recipe-eval building a
missing td-builder; `cargo test` is refused only at its runner,
td-builder, after it has compiled.

## Control plane

The host supplies a pinned Rust toolchain to compile host-seeded instances of
td's control-plane programs (`td-builder`, `td-recipe-eval`, and the `td-net`
multicall). These may evaluate recipes, fetch declared fixed-output sources,
construct sandboxes, and place outputs. The staged host-built `td-builder` has
explicit `ControlPlaneBuilder` provenance and may run only as the derivation
implementation, never through a recipe's `PATH` or argv. It is not a recipe
tool, target artifact, or runtime input, and no other host-built program may be
exposed to recipe steps.

Program role and build provenance are separate. With its target Rust
toolchain, td rebuilds those same control-plane programs as target recipes.
Those target-built copies are distribution artifacts and ship so a td system
can build itself. Only instances built by the host Rust seed are excluded from
the distribution closure.

## Target artifact graph

The target graph builds directly into `/td/store` from pinned upstream
sources and imported runtimes. Its trust roots are the pinned Debian build
root (including its reviewed runtime exports) and the upstream Rust
toolchain (both below):

```text
pinned sources, in the Debian build root
  -> imported Debian glibc and libgcc, the kernel, C libraries and td-link
  -> upstream Rust toolchain, retargeted onto td glibc
  -> target-built td control plane, tools, and Rust userland -> image
```

No compiler in this graph is source-bootstrapped: the build root's GCC and
binutils and the toolchain's rustc and LLVM compile the source-built outputs;
glibc and libgcc are imported prebuilt. The closure query's provenance claim (`APPLICATIONS.md` §B.8) names them as
trust roots wherever they reach.
What td establishes is that nothing undeclared enters a build. Recipe steps
may execute only unmarked outputs of earlier recipes, executables created by
the current build, and, in a recipe that declares it, the Debian build root's
tools. Host `/bin`, `/usr`, ambient `PATH`, and arbitrary host store paths are
never target inputs.

`td-builder build` stages declared inputs and sets compatibility `NIX_STORE`
to the active td store. A `TD_STORE_DIR=/td/store` build is hashed for and
built at `/td/store`; there is no post-hoc prefix rewrite and no Nix runtime
dependency.

## Foreign application payloads

td's target graph contains no foreign binary other than the Debian build
root's tools and reviewed runtime exports, the upstream Rust toolchain,
the non-executable pinned upstream data below and the device firmware
and CPU microcode after it. A third-party application is a marked foreign prebuilt
payload in `/td/store`, not a build-tool trust root. The mark is load-bearing:

- ordinary input channels refuse it; only `payload_inputs` may carry it as
  read-only data for image composition or runtime association;
- no recipe may execute it, link against it, or name it as a tool;
- closure queries exclude and report it in provenance claims;
- its source pin carries a reviewed URL, compiled expected SHA-256, and the
  mark that propagates to derived outputs.

Adding an application is a reviewed pin. Adding a class of foreign output is
an amendment to this file.

Flathub applications and runtimes are the second class (`APPLICATIONS.md`
§W.9). Their bytes are neither pinned nor in
`/td/store` or the image: what is reviewed is each declaration
(repository, exact app and runtime refs and td's own policy) and Flathub's
signing key compiled into td-net. The payload is fetched, only when the
session owner installs or updates it in td-software (or an autotest
oracle boot installs it through the same root operation), the
application into its account's private cache and its runtime into the
runtime store a dedicated runtime account owns, and admitted only under
that signature, exact ref bindings, a monotonic commit time and
§B.3.1's bounds and validators. It is never an input to any recipe, is
outside every closure and source-bootstrap claim, and runs only through
the same confinement or, for a runtime a td workspace names, inside that
workspace's jail (`APPLICATIONS.md` §C). That jail confines less than
an application's: the runtime's code runs under the workspace kind's
seccomp filter (the development profile where that applies), with the
workspace's grants (its writable worktrees and shares, its network and
the host-local sockets that reaches), as the sandbox's own account for a
td-sandbox sandbox and as the conversation's ledger account for a
confined td-agent workspace, and not behind the application kind's D-Bus,
portal or Wayland boundaries. td's own
glibc programs in such a workspace can load the runtime's code too: td's
imported glibc keeps its compiled-in `/usr` paths, so its gconv modules,
its locale data and whatever a program loads or `dlopen`s by name after
its `RUNPATH` resolve into the runtime there.

The image asserts that a dynamic payload's `PT_INTERP` is absent from the
image root, but this does not prove the bytes can execute only in a jail: an
explicit store `ld.so` can load them, a `native-glibc` workspace names td's
loader at that interpreter path inside its jail (`APPLICATIONS.md` §C), and
static payloads have no interpreter.
The claim is narrower and exact: no foreign application binary is a tool,
compilation input, or execution input to a source-built output. td's supported
run path puts applications behind td-owned namespace, seccomp, D-Bus, portal,
and Wayland boundaries; a Flathub runtime a workspace names has the
workspace's boundaries alone (above). See `APPLICATIONS.md` §B.8.

Image-shipped applications ride the system deployment, so independent
side-by-side retention is deferred for them (`APPLICATIONS.md` §W.2); a
Flathub application keeps its current and previous generations in its
account's cache, and its runtime in the runtime store (§W.9).

## Pinned upstream data

Some data td takes as upstream built rather than rebuilding it from
source: curl's rendering of Mozilla's trust store, the Nerd Fonts
JetBrains Mono release, and Noto Sans and Noto Serif from Noto's
release,
whose compiled TrueType faces td-ui parses. Such
a pin carries a reviewed URL and SHA-256 like any source, and stays
unmarked because nothing executes, links, or loads it as code: td's
builder copies it into zone-one outputs, and td-built programs parse it
under their own bounds. td-ui's font reader runs no hinting bytecode. It
is data inside the target graph, not source-built; the closure query
counts it among the audited seeds rather than naming it. Adding a pin of
this class is a reviewed pin; data a td program would execute or load as
code belongs to no class here and needs an amendment to this file.

## Device firmware

Some hardware runs code td cannot build: the ThinkPad T14s Gen 4 AMD's
Qualcomm WCN6855 Wi-Fi and Phoenix Radeon GPU, the Ryzen 5700G
desktop's Cezanne Radeon GPU, the Framework Desktop's Strix Halo Radeon GPU,
and the Intel, MediaTek, Realtek and Qualcomm
Wi-Fi chips the roster lists, load their firmware from the host when
their drivers probe. td takes such firmware as upstream bytes from linux-firmware's
tagged tree, each file a pin with a reviewed URL and SHA-256, listed in
`DEVICE_FIRMWARE` (`recipes/src/source_pins.rs`). Only the peripheral
it is written for runs it. The `device-firmware` recipe copies the
pinned files unmodified into `lib/firmware` in its output, with their
notices as data beside them. A desktop image holds that output in its
store, links it at `/lib/firmware`, and copies the files into its
deployment initramfs, so a driver built into the kernel finds them when
it probes and again after `switch_root`, which empties the initramfs only
once the root has replaced it. The selector's initramfs holds none, so the
same drivers get none there: no such device runs before the handover,
and the display stays on the firmware framebuffer. The kernel's firmware
loader hands them to the device's driver; no recipe
step executes, links or loads them as host code. Some are ELF images
for the device's own processor, which is why the profiler's object
index leaves that store item out. Only that recipe names a roster pin,
which a catalog test holds. They stay
unmarked, like the data above, because nothing in zone one runs them,
and the closure query counts them among the audited seeds. Being code,
they are also named: one `device-firmware` line per pin, beside the
trust roots, for every closure holding that recipe.
What td does not establish is anything about that code: a device with
firmware can reach host memory by DMA, which an IOMMU, where the kernel
drives one, bounds (with lazy unmapping), and the WCN6855 and WCN7850
answer QMI over the QRTR socket family, which td-jail's filter keeps from
applications (not from unjailed session processes). Adding firmware
for a device is a reviewed pin in that roster; code the host CPU
executes belongs to no class here but the next.

## CPU microcode

A CPU loads its vendor's microcode, which td cannot build and which
vulnerability mitigations depend on. td takes it as upstream bytes:
Intel's release tree at its tag and AMD's containers at linux-firmware's,
each a pin with a reviewed URL and SHA-256, listed in `CPU_MICROCODE`
(`recipes/src/source_pins.rs`). Only the `cpu-microcode` recipe names
them, which a catalog test holds; it concatenates each vendor's set
unmodified into the one file the kernel's early loader asks for, and
every profile's deployment initramfs carries the two files at its head,
with their licences as data in the root. The kernel hands the patch
matching this CPU to the CPU, which checks the vendor's signature; on
AMD's Zen CPUs the kernel first refuses a patch missing from its own
digest table, so a firmware tag can outrun a kernel (`UPDATING.md`). No
recipe step executes them, and nothing in zone one links or runs them.
They stay unmarked, and the closure query names each: one `cpu-microcode`
line per pin. td establishes no property of that code, and offering it is
not proof a CPU took it, which only that machine's kernel log says. A
hypervisor's guest does not load any. The selector's initramfs carries
none: the selector takes the PIN, the TPM release and the volume key on
whatever microcode platform firmware supplied, running no untrusted code,
and the deployment's kernel applies the update after it hands over. Adding
a pin to the roster is a review; any other code the host CPU executes
belongs to no class here.

## Debian build root

td's C toolchain is the Debian build root. It is a build environment, not
an input: a pinned Debian image layer plus pinned .debs from
snapshot.debian.org, resolved into
`recipes/src/debian-root.lock` by `td-feed debian-root resolve`, and
assembled by td-builder into one input-addressed store path whose NAR hash
the lock pins. A recipe that declares `build_root` runs with that tree as
its read-only sandbox `/`, so its steps may run Debian's gcc, binutils, make
and the rest, with writable scratch where the minimal root had it
(`UNSAFE.md` lists the mounts). What td enforces:

- its pins are a class of their own: no input channel, seed or source-pin
  lookup resolves one, and the sandbox binds as `/` only the path the
  compiled lock names, at its pinned hash, vouched by a db typed
  `BuildRoot`, never a builder or tool input;
- an output built in it may not reference its store path, and no file in
  that output may be an ELF naming an interpreter or RUNPATH outside the
  store (a non-executable debug-only companion, whose loader ranges hold
  no data, is judged by the runtime it pairs with), or a symlink to an
  absolute path outside the store.

The `glibc-x86-64` and `gcc-runtime` recipes import a reviewed subset of
that root into normal store outputs: Debian's glibc libraries, headers,
startup objects and timezone tools, and its libgcc libraries and GCC startup
objects. They compile nothing. `debianRuntime` names a versioned transform;
its compiled file roster selects the bytes, normalizes the multiarch layout,
rewrites libc/libm linker scripts and retargets ELF interpreters to the store.
Changing the transform or roster requires advancing its version token.
No package maintainer script runs. These are unmarked upstream runtime trust
roots, not application payloads. The closure query names each with
`upstream-runtime\tdebian\t<recipe>` alongside its build-root provenance.

Other recipes link only against these declared store libraries and their
own declared inputs, never ambient build-root libraries. Static objects
cannot be distinguished after linking, so their link-map checks establish
that boundary. The imported libraries and their code inside static outputs
are profiler coverage boundaries, not source-built or frame-pointer claims.
The source GCC/glibc bootstrap and its source pins are absent.

A C package td builds from an upstream release is an ordinary source pin,
compiled in the root against td glibc; one that links gcc-runtime's start
files and libgcc in place of GCC's is listed in `BUILD_ROOT_LINKED`
(`engine/src/target_profile.rs`), and its link-map checks show where its
links' inputs came from. PostgreSQL 17 (`postgresql-x86-64`), the server
td-mta stores mail in, is one: every link of its build writes a checked
map, and every ELF it ships must be byte-identical to one of those
links' outputs. Its release tarball carries no generated sources, so the
root's bison, flex and perl write its parser, scanner and catalog files
during the build, as they do the kernel's; what they write is source the
root's GCC compiles, never a runtime input. It links td zlib and no TLS
library, since td builds none in C: a td-built server offers td-mta its
Unix socket alone, as td-mta refuses a network connection without
verified TLS (`td-mta/POSTGRES.md` §1).

Changing the root's packages is a reviewed lock change.

## Rust toolchain and dependencies

td uses upstream's prebuilt Rust 1.99.1 for `x86_64-unknown-linux-gnu`: the
rustc, rust-std, Cargo and Clippy release components, each a source pin with
an upstream URL and its channel-manifest SHA-256. The `rust-toolchain`
recipe's `rust-dist` transform assembles them, links td glibc,
gcc-runtime's libgcc_s and td zlib into the tree, and relinks every shipped
executable onto td glibc's loader; it compiles nothing. The toolchain,
rust-lld and its LLVM included, is a build-tool trust root like the Debian
build root: unmarked, executed by recipes, and named
by a provenance claim (`upstream-rust`) for every closure that contains
it. The image ships launchers, not the toolchain: `td-rust` downloads the
same pinned components on first use into the invoking account's private
cache. Recipe builds retain their declared `rust-toolchain` input; the
account cache is never a target recipe input. `td-link/DESIGN.md` specifies
the runtime linker and `APPLICATIONS.md` §G specifies the downloader.
Every Rust output links its prebuilt standard library. A
stronger claim needs a separately specified diverse bootstrap or
diverse-double-compilation proof. Every Rust output a td recipe builds,
Cargo's build scripts and proc macros included, links through `td-link`
(`td-link/DESIGN.md`): a dependency-free driver that takes rustc's
gcc-style link line, refuses compile requests, unknown linker options,
host library paths on its line and inputs that are not ELF objects or
ordinary archives, and runs the toolchain's rust-lld with td glibc's and
gcc-runtime's start files and libraries, so no GCC or binutils takes part
in the link. td-link is itself a Debian build-root member: the root's GCC
links its throwaway first stage, and its claim to link without GCC rests
on its second and third stages linking byte-identically. The image ships
td-link, not GCC or binutils: its `/bin/cc` is td-link, so rustc's and
Cargo's default linker, and the native control-plane build on a td host,
link through it, and a td host compiles no C outside the build root.
Every Cargo recipe builds in the Debian build root: the C its build
scripts compile is the root's GCC, which the runner's CFLAGS forbid to
link, against td glibc's headers and GCC's own freestanding headers,
archived by the root's `ar`, and the root's `objcopy` splits its
companions. Each object the build leaves must show it: its dependency
list resolves into those headers or the build's trees, and the recipe
declares how many objects there are. No Cargo recipe declares any today,
so an object from one fails its build. Every other recipe that runs
rustc directly builds there too, but td-link-test, which builds outside
it to prove td-link needs no GCC or binutils, and the root's `objcopy`
splits its companions.

Cargo dependency sources are fixed-output inputs selected by committed
`Cargo.lock` checksums. Builds run offline. Git dependencies are unsupported
without explicit sign-off and a fixed-output representation pinned by commit
and archive hash. Build scripts, proc macros, C, and assembly may use only
td-built tools, the pinned Rust toolchain and, for C and assembly, the
Debian build root's compiler and archiver. New td-owned shipped userland
should be Rust built with that toolchain.

The final image uses uutils for its core userland and carries no GNU
userland. glibc and libgcc, the kernel, boot/firmware components, and
explicitly reviewed non-Rust packages are the exceptions; it ships no C
compiler.

## Target user-mode profiling contract

Every source-built user-mode artifact that ships and can appear in a runtime
stack keeps frame pointers. This includes the source-built
libraries and userland closure; td-owned programs, dependency crates, and
native objects compiled by Cargo build scripts. The upstream Rust toolchain is
not source-built: its own objects carry no debug companions, and the prebuilt
standard library every Rust output links keeps frame pointers in only part of
its code; `td-profiler/DESIGN.md` names both as coverage boundaries. Debian
glibc and libgcc likewise retain upstream coverage: their shared objects
have no td companions and their static code is named in every affected
output's coverage marker. It is one target-wide policy, not a per-recipe
release-profile choice. A recipe may not weaken it without amending this contract and `td-profiler/DESIGN.md`.

Every such ELF also has a deterministic debug companion in the same recipe
output and system image. A companion carries the ordinary symbol table and no
DWARF: profiler output attributes functions, and source lines are
deliberately unavailable. The Debian build root, build-only
intermediates, the kernel, firmware, marked foreign payloads, the upstream
Rust toolchain, hand-written assembly, and the exact coverage and
reproducibility rules are specified in `td-profiler/DESIGN.md`; profiler
output reports every boundary explicitly. The image's object index leaves the
toolchain out, so its frames report as unresolved symbol coverage.

The first `td-profiler/DESIGN.md` implementation increment enforces the
producer side in compiler flags, per-output runtime/debug pair checks, size
ceilings, and reproducibility oracles. The image-integration increment adds
the whole-deployment coverage walk: the image build indexes each runtime ELF
in the deployment root, apart from the glibc, gcc-runtime and device-firmware
outputs and the marked foreign application roots it excludes, and fails on one without a
build-ID-matched companion (`td-profiler/DESIGN.md` §4).
