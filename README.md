# td

td is an immutable, Rust-first Linux distribution with its own system tools,
Wayland desktop and applications. The system is defined by Rust recipes in
this repository, from pinned build inputs to the bootable image.

## Design philosophy

- **Make the system inspectable.** Recipes declare their inputs and build
  in isolation. The target build uses a pinned Debian build root and upstream
  Rust toolchain. Debian glibc and libgcc are imported; ambient host tools
  and libraries stay outside the target build.
- **Keep the system separate from your data.** The root filesystem is
  read-only. Updates replace complete deployments and retain the previous
  version for rollback, while user files and writable state persist.
- **Build with Rust and keep dependencies small.** td develops its own
  system tools, UI toolkit and desktop applications alongside Rust uutils.
  Shared code lives in local crates; external dependencies are reviewed
  explicitly.
- **Keep local builds possible.** The recipe graph travels in Git, so you
  can build and customize the system yourself. Signed image updates are
  also supported with a configured publisher policy.

The [repository principles](AGENTS.md#principles) and
[build and trust model](AGENTS.md#build-and-trust-model) describe these choices
in detail.

## Get started

### Install td

Download a prebuilt installation ISO from [tdlinux.org](https://www.tdlinux.org/).

The installation ISO boots into a graphical installer and installs the
system offline onto a chosen disk. Installation erases that disk.
The installer is validated on x86-64 UEFI under QEMU; real-hardware
validation is still in progress.

To clone and build the ISO yourself, use an x86-64 Linux host with
unprivileged user namespaces enabled, Rust 1.95 or newer (`cargo` and
`rustc`), a C compiler and network access:

```sh
git clone https://github.com/timmydo/td.git
cd td
./build-iso
```

The image is written to `dist/td-install-x86-64.iso`. A first system build
can take hours. With QEMU installed, `./start` builds and boots a disposable
VM session. See the [installer documentation](td-install/INSTALLER.md)
for installation details.

### Try the applications on another distro

You can run td's editor, terminal, mail client and other desktop programs
on an existing Linux Wayland desktop. You need Rust 1.95 or newer
(`cargo` and `rustc`), a C compiler and network access for the initial
build and font download.

```sh
git clone https://github.com/timmydo/td.git
cd td
./install-fonts
./install-apps
```

Applications are installed into `~/.local/bin`; add it to your `PATH` if
needed, then run `td-editor`, `td-term` or another installed program.
Run `./install-apps` again after pulling updates. These host builds run
as your user; td-agent's tools additionally require unprivileged user
namespaces. See [applications and confinement](APPLICATIONS.md) for
runtime details.

## Explore further

- [Applications and confinement](APPLICATIONS.md)
- [Desktop compositor](td-compositor/README.md) and [UI toolkit](td-ui/DESIGN.md)
- [Development and contribution workflow](DEVELOPMENT.md)

## License

MIT. See [LICENSE](LICENSE). Third-party material keeps its own license;
[THIRD-PARTY.md](THIRD-PARTY.md) lists it.
