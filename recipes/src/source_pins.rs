//! Fixed-output source pins owned by the recipe catalog.
//!
//! This replaces the old external source lock directory. The keys are the
//! staged input names recipes declare through `source_input(...)` or `inputs`.

use crate::types::SourcePin;

struct PinDef {
    key: &'static str,
    aliases: &'static [&'static str],
    url: &'static str,
    sha256: &'static str,
    file: &'static str,
}

const PINS: &[PinDef] = &[
    PinDef {
        key: "ca-certificates-source",
        aliases: &[],
        // Mozilla's trust store as rendered in PEM form by curl. Pin the dated
        // extract rather than curl's moving cacert.pem URL so a reviewed update
        // changes both the source name and its published adjacent SHA-256.
        url: "https://curl.se/ca/cacert-2026-08-13.pem",
        sha256: "f66dff1bdf8f96060b8177976f8b7d9254bc89bc4db933d769f7384d28480bc9",
        file: "cacert-2026-08-13.pem",
    },
    PinDef {
        key: "claude-code-source",
        aliases: &[],
        // The Claude Code 2.1.260 native Linux x86-64 release: one glibc-dynamic
        // ELF, published beside a manifest whose `platforms["linux-x64"].checksum`
        // is this SHA-256. It is a marked sandboxed-application payload
        // (APPLICATIONS.md §B.8): its own recipe consumes it as typed data,
        // and it is never a tool, compilation input, or execution input to a
        // source-built output. The vendor ships no source, so it is a
        // prebuilt payload whose supported run path is td-jail.
        url: "https://downloads.claude.ai/claude-code-releases/2.1.260/linux-x64/claude",
        sha256: "7a2fdc74b6836ea3d183f665b869f0ee3baebc9713cbebffe5838da4ea7bd82e",
        file: "claude-2.1.260-linux-x64",
    },
    PinDef {
        key: "cryptsetup-x86-64-source",
        aliases: &[],
        // LUKS2 userspace for td-install/ENCRYPTION.md, under the owner's
        // principle-2 sign-off; hash matches upstream's sha256sums.asc.
        url: "https://www.kernel.org/pub/linux/utils/cryptsetup/v2.8/cryptsetup-2.8.8.tar.xz",
        sha256: "3acfa685f2dd7fcc832e0b77bc7093aa7da554a51ce8dafbb4138eaa854eee35",
        file: "cryptsetup-2.8.8.tar.xz",
    },
    PinDef {
        key: "curl-x86-64-source",
        aliases: &[],
        // curl 8.21.0 supplies Git's source-built HTTP/HTTPS transport. The
        // recipe keeps only static libcurl and disables every protocol and
        // optional dependency outside the reviewed HTTP(S) closure.
        url: "https://curl.se/download/curl-8.21.0.tar.xz",
        sha256: "aa1b66a70eace83dc624508745646c08ae561de512ab403adffb93ac87fc72e6",
        file: "curl-8.21.0.tar.xz",
    },
    PinDef {
        key: "fd-source",
        aliases: &[],
        url: "https://static.crates.io/crates/fd-find/fd-find-10.2.0.crate",
        sha256: "de08defa195af894cc295a43bfc65ba28903e492fd5f32f7a24bf75eafd9bf34",
        file: "fd-find-10.2.0.crate",
    },
    PinDef {
        key: "git-x86-64-source",
        aliases: &[],
        // Git 2.55.0 is the source-built daily-driver client. Its td configuration
        // keeps local repositories and HTTP(S) remotes and dispatches SSH remotes
        // through the separate OpenSSH package, while omitting language runtimes,
        // WebDAV, and direct OpenSSL use.
        url: "https://www.kernel.org/pub/software/scm/git/git-2.55.0.tar.xz",
        sha256: "457fdb04dc8728e007d4688695e6912e6f680727920f2a40bf11eacc17505357",
        file: "git-2.55.0.tar.xz",
    },
    PinDef {
        key: "gcc-14-source",
        aliases: &[],
        url: "https://ftp.gnu.org/gnu/gcc/gcc-14.3.0/gcc-14.3.0.tar.xz",
        sha256: "e0dc77297625631ac8e50fa92fffefe899a4eb702592da5c32ef04e2293aca3a",
        file: "gcc-14.3.0.tar.xz",
    },
    PinDef {
        key: "gmp63",
        aliases: &[],
        url: "https://ftp.gnu.org/gnu/gmp/gmp-6.3.0.tar.xz",
        sha256: "a3c2b80201b89e68616f4ad30bc66aee4927c3ce50e33929ca819d5c43538898",
        file: "gmp-6.3.0.tar.xz",
    },
    PinDef {
        key: "mpc131",
        aliases: &[],
        url: "https://ftp.gnu.org/gnu/mpc/mpc-1.3.1.tar.gz",
        sha256: "ab642492f5cf882b74aa0cb730cd410a81edcdbec895183ce930e706c1c759b8",
        file: "mpc-1.3.1.tar.gz",
    },
    PinDef {
        key: "mpfr421",
        aliases: &[],
        url: "https://ftp.gnu.org/gnu/mpfr/mpfr-4.2.1.tar.xz",
        sha256: "277807353a6726978996945af13e52829e3abd7a9a5b7fb2793894e18f1fcbb2",
        file: "mpfr-4.2.1.tar.xz",
    },
    PinDef {
        key: "glibc-241-source",
        aliases: &[],
        url: "https://ftp.gnu.org/gnu/glibc/glibc-2.41.tar.xz",
        sha256: "a5a26b22f545d6b7d7b3dd828e11e428f24f4fac43c934fb071b6a7d0828e901",
        file: "glibc-2.41.tar.xz",
    },
    PinDef {
        key: "jetbrains-mono-nerd-font-source",
        aliases: &[],
        // Pinned upstream data (AGENTS.md): the Nerd Fonts release archive of
        // JetBrains Mono, compiled TrueType faces td-ui parses and never runs.
        url: "https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/JetBrainsMono.tar.xz",
        sha256: "04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf",
        file: "jetbrains-mono-nerd-font-3.5.1.tar.xz",
    },
    PinDef {
        key: "nerd-fonts-nerd-fonts-license-source",
        aliases: &[],
        // The notices the Nerd Fonts repository carries at the release's tag
        // for the face's own licence and each merged icon set that has one;
        // jetbrains-mono-nerd-font ships them beside the faces.
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/LICENSE",
        sha256: "1f6ad4edae6479aaace3112ede5279a23284ae54b2a34db66357aef5f64df160",
        file: "nerd-fonts-3.5.1-nerd-fonts-LICENSE",
    },
    PinDef {
        key: "nerd-fonts-codicons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/codicons/LICENSE.txt",
        sha256: "d6239afa918961b465b07bf7411cbe34ff6685854f58553db7966f4881a0211f",
        file: "nerd-fonts-3.5.1-codicons-LICENSE.txt",
    },
    PinDef {
        key: "nerd-fonts-font-awesome-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/font-awesome/LICENSE.txt",
        sha256: "de784a808496d49f5f80dd9d1d6e9a51cff712cfbc339e7c25155631ae3cdb5d",
        file: "nerd-fonts-3.5.1-font-awesome-LICENSE.txt",
    },
    PinDef {
        key: "nerd-fonts-materialdesign-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/materialdesign/LICENSE",
        sha256: "3bae6305bcaa6d10219b999f65c342034c2d40b1c0e8567fa08605e73ccba829",
        file: "nerd-fonts-3.5.1-materialdesign-LICENSE",
    },
    PinDef {
        key: "nerd-fonts-octicons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/octicons/LICENSE",
        sha256: "4f886642f9e8d99eeadd484acdac011c3fde8a338faf52f5ca1408c13b9a9b63",
        file: "nerd-fonts-3.5.1-octicons-LICENSE",
    },
    PinDef {
        key: "nerd-fonts-pomicons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/pomicons/LICENSE",
        sha256: "cb4797a9745683f287c28833db596e9d68ec43082d59359b3a49c504f0f610ce",
        file: "nerd-fonts-3.5.1-pomicons-LICENSE",
    },
    PinDef {
        key: "nerd-fonts-powerline-extra-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/powerline-extra/LICENSE",
        sha256: "544307a58e6826c24145b2af1c3578b53e2e21ae0e6c67ab4c6abd544304d26a",
        file: "nerd-fonts-3.5.1-powerline-extra-LICENSE",
    },
    PinDef {
        key: "nerd-fonts-powerline-symbols-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/powerline-symbols/LICENSE.txt",
        sha256: "46d5355e4734468135d8e4a30d0916292cc4df2b3ec30aa68c866f82497933be",
        file: "nerd-fonts-3.5.1-powerline-symbols-LICENSE.txt",
    },
    PinDef {
        key: "nerd-fonts-weather-icons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/weather-icons/OFL.txt",
        sha256: "69cdedc8a4f57a30d68e4f045af8b9cce0ec3b1a6e7eec1ad41737c894f26790",
        file: "nerd-fonts-3.5.1-weather-icons-OFL.txt",
    },
    PinDef {
        key: "json-c-x86-64-source",
        aliases: &[],
        // cryptsetup's LUKS2 metadata library; only libjson-c.a is built. The
        // project's static release object; its contents are identical to the
        // json-c-0.18-20240915 tag archive whose SHA-512 Fedora records.
        url: "https://s3.amazonaws.com/json-c_releases/releases/json-c-0.18.tar.gz",
        sha256: "876ab046479166b869afc6896d288183bbc0e5843f141200c677b3e8dfb11724",
        file: "json-c-0.18.tar.gz",
    },
    PinDef {
        key: "libdevmapper-x86-64-source",
        aliases: &[],
        // LVM2, of which only libdevmapper.a is built for cryptsetup; the
        // file matches upstream's sha512.sum.
        url: "https://sourceware.org/pub/lvm2/LVM2.2.03.43.tgz",
        sha256: "d87ec0dac9061f1fa58ebced5c6b1360c87d4c5cd7b455dc0ebe1d3f036920d7",
        file: "LVM2.2.03.43.tgz",
    },
    PinDef {
        key: "libressl-x86-64-source",
        aliases: &[],
        // LibreSSL 4.3.2 supplies the OpenSSL-compatible TLS implementation
        // for curl and Git. The recipe builds only static archives and disables
        // upstream assembly so every eventual shipped instruction remains on
        // td's checked frame-pointer path.
        url: "https://ftp.openbsd.org/pub/OpenBSD/LibreSSL/libressl-4.3.2.tar.gz",
        sha256: "edf01aee24c65d69e6a9efcb9d44bcda682ff9d4f3bbbd95e794e1dfa90847b5",
        file: "libressl-4.3.2.tar.gz",
    },
    PinDef {
        key: "linux-source",
        // The 4.14.67 tree stays pinned ONLY as the source of the UAPI kernel
        // headers (`linux-headers-x86-64`) td glibc, gcc-runtime and the
        // build-root C packages build against. The bootable kernel is a
        // SEPARATE, current pin (`linux-kernel-source`): bumping this one
        // rebuilds glibc and everything above it.
        aliases: &["linux-headers-x86-64"],
        url: "https://cdn.kernel.org/pub/linux/kernel/v4.x/linux-4.14.67.tar.xz",
        sha256: "3f4b056dc27233a78f7a4a35ed6fdcfd0a9680ec40b611a898bb6c8b905070ba",
        file: "linux-4.14.67.tar.xz",
    },
    PinDef {
        key: "linux-kernel-source",
        aliases: &[],
        // Linux 7.1.4 — the latest STABLE mainline kernel (not a longterm/LTS
        // line) the `linux-x86-64` recipe builds into a bootable x86_64
        // `vmlinux` in the Debian build root (re #529). DISTINCT from the
        // `linux-source` pin above: this is the full kernel tree to compile, kept
        // current; that one is the frozen 4.14 UAPI-header source for td glibc.
        // Keeping them separate confines a kernel bump to this one recipe
        // instead of glibc and everything above it. A reviewed update bumps
        // version + sha256 + the seed-digests.txt row together.
        url: "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.1.4.tar.xz",
        sha256: "1c63922a119675d38e3ae0f8f6ee07f15c41a786ab9ed66563749bb8c9a08e2e",
        file: "linux-7.1.4.tar.xz",
    },
    PinDef {
        key: "openssh-x86-64-source",
        aliases: &[],
        // OpenSSH Portable 10.5p1 supplies both halves of td's minimal SSH
        // surface. The recipe builds only ssh, ssh-keygen, sshd and the two
        // mandatory privilege-separation helpers, without libcrypto or zlib.
        url: "https://cdn.openbsd.org/pub/OpenBSD/OpenSSH/portable/openssh-10.5p1.tar.gz",
        sha256: "d44d28a839ea9daf969cc69150fde59910b2b39361dad81a3bd6cbd19218db11",
        file: "openssh-10.5p1.tar.gz",
    },
    PinDef {
        key: "popt-x86-64-source",
        aliases: &[],
        // cryptsetup's command-line parser; only libpopt.a is built. The file
        // matches Fedora's recorded SHA-512; the host serves no valid https.
        url: "http://ftp.rpm.org/popt/releases/popt-1.x/popt-1.19.tar.gz",
        sha256: "c25a4838fc8e4c1c8aacb8bd620edb3084a3d63bf8987fdad3ca2758c63240f9",
        file: "popt-1.19.tar.gz",
    },
    PinDef {
        key: "ripgrep-seed-source",
        aliases: &[],
        // The upstream x86-64 musl release is a static PIE and is admitted only
        // as the first sandboxed-application seed. It is never a build tool.
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz",
        sha256: "33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c",
        file: "ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz",
    },
    PinDef {
        key: "ripgrep-source",
        aliases: &[],
        url: "https://static.crates.io/crates/ripgrep/ripgrep-14.1.1.crate",
        sha256: "f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9",
        file: "ripgrep-14.1.1.crate",
    },
    PinDef {
        key: "rust-rustc-dist",
        aliases: &[],
        // The upstream Rust 1.96.0 release (channel-rust-1.96.0.toml, dated
        // 2026-05-28), retargeted onto td's runtime by `rust-toolchain`.
        url: "https://static.rust-lang.org/dist/2026-05-28/rustc-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "7d7fa1d0cfb0fab71a956bb78f41107202c17f30ab56c45288e869a37fd9633d",
        file: "rustc-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
    },
    PinDef {
        key: "rust-std-dist",
        aliases: &[],
        url: "https://static.rust-lang.org/dist/2026-05-28/rust-std-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "c09c7c646248f14f473f5f7a029af15ee57c3a9f9bc93dfa72d9621938586b82",
        file: "rust-std-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
    },
    PinDef {
        key: "rust-cargo-dist",
        aliases: &[],
        url: "https://static.rust-lang.org/dist/2026-05-28/cargo-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "dee75c3c8f9f600ad75bc0c93249e767d3047845a4dd668327ce43ab039ba266",
        file: "cargo-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
    },
    PinDef {
        key: "rust-clippy-dist",
        aliases: &[],
        url: "https://static.rust-lang.org/dist/2026-05-28/clippy-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "9b0b89c67d5ce8195e1c8733587a63e326353fdda65f62455a64fa03e25659f2",
        file: "clippy-1.96.0-x86_64-unknown-linux-gnu.tar.xz",
    },
    PinDef {
        key: "tzdata-source",
        aliases: &[],
        // Approved IANA data-only input; compile with the existing td glibc zic.
        url: "https://data.iana.org/time-zones/releases/tzdata2026d.tar.gz",
        sha256: "0cb2aa8e333c3dc049badc42a0c61f21987b8cd44e107fa900bad764aacc7767",
        file: "tzdata2026d.tar.gz",
    },
    PinDef {
        key: "util-linux-libs-x86-64-source",
        aliases: &[],
        // cryptsetup requires libuuid and libblkid. The recipe builds only
        // those two static libraries; no util-linux programs are shipped.
        url: "https://www.kernel.org/pub/linux/utils/util-linux/v2.42/util-linux-2.42.2.tar.xz",
        sha256: "03a05d3adf9602ef128f2da05b84b3205ce60c351e5737c0370f74000679ce8a",
        file: "util-linux-2.42.2.tar.xz",
    },
    PinDef {
        key: "uutils-source",
        aliases: &[],
        // uutils coreutils 0.9.0 — the read-only-root Rust core userland (#547).
        // The `--auto` source_input pin; the dependency closure is pinned
        // separately by the committed recipes/locks/uutils/Cargo.lock.
        url: "https://static.crates.io/crates/coreutils/coreutils-0.9.0.crate",
        sha256: "b92df9b821533650f3797aadae46e547f72db281c1f8a27f381f36d54284d34b",
        file: "coreutils-0.9.0.crate",
    },
    PinDef {
        key: "zlib-x86-64-source",
        aliases: &[],
        url: "https://github.com/madler/zlib/releases/download/v1.3.1/zlib-1.3.1.tar.gz",
        sha256: "9a93b2b7dfdac77ceba5a558a580e74667dd6fede4585b91eefb60f03b72df23",
        file: "zlib-1.3.1.tar.gz",
    },
];

/// The pins APPLICATIONS.md §B.8 marks FOREIGN: a prebuilt payload that is not a
/// bootstrap seed and never becomes one.
///
/// A roster rather than a field on every `PinDef`, for UNSAFE.md's reason — the
/// COUNT is what a reviewer reads, and `foreign: false` written fifty-odd times
/// buries the one entry that matters. The hole a name-keyed roster would
/// otherwise have is the one this workstream has already refused twice, a
/// declaration that reaches nothing reading as enforcement: `every_foreign_name_
/// is_a_pin_key` closes it, and requires a KEY rather than an alias, since
/// marking an alias would leave the canonical key unmarked.
///
/// Adding an entry is a reviewed pin; adding a CLASS of foreign output is an
/// AGENTS.md amendment.
const FOREIGN: &[&str] = &["claude-code-source", "ripgrep-seed-source"];

pub fn all() -> Vec<SourcePin> {
    all_with(FOREIGN)
}

pub fn by_key(key: &str) -> Option<SourcePin> {
    by_key_with(key, FOREIGN)
}

/// Every NAME a §B.8-marked pin answers to — its key AND its aliases.
///
/// A set built from keys alone is a hole rather than an approximation, and
/// review is what found it: `by_key` RESOLVES an alias, so a recipe naming a
/// marked pin by one gets the payload's bytes with the mark intact, while a
/// key-only set answers "not marked" for that same name. The tool-channel
/// refusal and the closure query both ask that set, so the pin would be neither
/// refused nor reported. Two shipped pin names are aliases today, so this is
/// reachable rather than hypothetical.
pub fn foreign_names() -> Vec<String> {
    foreign_names_with(FOREIGN)
}

fn all_with(foreign: &[&str]) -> Vec<SourcePin> {
    PINS.iter()
        .map(|pin| materialize_with(pin, pin.key, foreign))
        .collect()
}

fn foreign_names_with(foreign: &[&str]) -> Vec<String> {
    PINS.iter()
        .filter(|pin| foreign.contains(&pin.key))
        .flat_map(|pin| std::iter::once(pin.key).chain(pin.aliases.iter().copied()))
        .map(str::to_string)
        .collect()
}

fn by_key_with(key: &str, foreign: &[&str]) -> Option<SourcePin> {
    PINS.iter()
        .find(|pin| pin.key == key || pin.aliases.iter().any(|alias| *alias == key))
        .map(|pin| materialize_with(pin, key, foreign))
}

/// Every function above takes its roster as an ARGUMENT, and the public entry
/// points exist only to supply `FOREIGN`. The behavior remains tested through the
/// `_with` pair over fixtures so misspellings, aliases and an empty roster stay
/// observable independently of the reviewed production entry. The wiring is
/// pinned in source by
/// `the_public_pair_is_the_only_place_the_real_roster_is_read`.
fn materialize_with(pin: &PinDef, key: &str, foreign: &[&str]) -> SourcePin {
    let out = SourcePin::new(key, pin.url, pin.sha256, pin.file);
    // Keyed on the PIN's own key, never on `key` — which may be an ALIAS, and a
    // pin that is foreign under one name and ordinary under another is the same
    // bytes admitted twice under two different trust answers.
    if foreign.contains(&pin.key) {
        out.mark_foreign()
    } else {
        out
    }
}

/// The Debian build root's lock (AGENTS.md, "Debian build root"): an image layer and the
/// .debs unpacked over it, resolved by `td-feed debian-root resolve`. These
/// pins are a class of their own: `by_key` and `all` never return one, so no
/// recipe input or seed can name it, and `source-pins` omits them, so no
/// consumer of that list meets one. `td-feed warm build-root` caches them.
const BUILD_ROOT_LOCK: &str = include_str!("debian-root.lock");

pub fn build_root_pins() -> Result<Vec<SourcePin>, String> {
    Ok(td_engine::debian_root::parse(BUILD_ROOT_LOCK)?
        .pins
        .iter()
        .map(|pin| SourcePin::new(&pin.key, &pin.url, &pin.sha256, &pin.file))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_root_lock_pins_a_layer_and_its_debs() {
        let pins = build_root_pins().unwrap();
        assert!(pins.len() > 1, "a layer and at least one .deb");
        assert!(pins.iter().all(|pin| pin.key.starts_with("debian-root-")));
        assert!(pins.iter().all(|pin| !pin.foreign()));
        let layer = pins
            .iter()
            .find(|pin| pin.key == "debian-root-layer")
            .unwrap();
        assert!(layer.url.starts_with("https://registry-1.docker.io/v2/"));
    }

    /// The build root's pins must be unreachable by every name an ordinary
    /// recipe input resolves through, and share no cache file with a seed pin.
    #[test]
    fn build_root_pins_are_unreachable_by_name_and_disjoint_from_seed_pins() {
        let seeds = all();
        for pin in build_root_pins().unwrap() {
            assert!(by_key(&pin.key).is_none(), "{} resolves by key", pin.key);
            assert!(
                seeds
                    .iter()
                    .all(|seed| seed.key != pin.key && seed.file != pin.file),
                "{} collides with a seed pin",
                pin.key
            );
        }
        assert!(PINS.iter().all(|pin| !pin.key.starts_with("debian-root-")
            && !pin.aliases.iter().any(|a| a.starts_with("debian-root-"))));
    }

    #[test]
    fn catalog_carries_all_legacy_source_locks() {
        // The Linux 4.14 UAPI-header source, GCC 14.3.0 with GMP, MPFR and
        // MPC (gcc-runtime's libgcc) and glibc 2.41 (td glibc) + the
        // linux-x86-64 kernel source (separate from the 4.14 UAPI-header
        // pin, re #529) + the upstream Rust 1.96.0 rustc, rust-std, Cargo and
        // Clippy release components +
        // coreutils-0.9.0, ripgrep 14.1.1, and fd 10.2.0 (the Rust userland
        // `.crate` sources) +
        // util-linux 2.42.2 (cryptsetup's minimal libuuid/libblkid build
        // closure) + the first reviewed
        // foreign application seed, upstream ripgrep 15.2.0, LibreSSL 4.3.2,
        // curl 8.21.0 (the static HTTPS foundation for Git), Git 2.55.0,
        // curl's dated Mozilla CA extract, OpenSSH Portable 10.5p1, and
        // the second reviewed foreign application seed, the Claude Code
        // 2.1.260 native release. The terminal applications are td's own
        // trees (APPLICATIONS.md §W.8) and pin nothing here.
        // IANA timezone data 2026d adds one source-data pin, and the Nerd
        // Fonts v3.5.1 JetBrains Mono release ten pinned upstream data pins:
        // the archive and nine licence notices. cryptsetup 2.8.8 with json-c
        // 0.18, popt 1.19 and LVM2 2.03.43 add the LUKS2 userspace closure.
        // zlib 1.3.x is td zlib. BusyBox's pin left with its last consumer,
        // the kernel's bc, flex and elfutils left when it moved into the
        // Debian build root, and btrfs-progs left when td-btrfs replaced it.
        // CMake and the Rust source and its bootstrap snapshot left with the
        // source-built toolchain, and the stage0 ladder's 36 pins with the
        // ladder.
        assert_eq!(all().len(), 38);
    }

    /// A roster keyed by NAME can name nothing, and this workstream has twice
    /// refused that shape: a declaration reaching no path reads as enforcement
    /// and is none. Here it would be worse than a no-op — a misspelt entry
    /// leaves the payload it meant to mark UNMARKED, which is the answer every
    /// check downstream then gives.
    ///
    /// A KEY and never an alias, because `materialize` marks on `pin.key`: an
    /// alias listed here would resolve to a pin that is foreign under one name
    /// and ordinary under another.
    fn unresolved(foreign: &[&str], pins: &[PinDef]) -> Vec<String> {
        foreign
            .iter()
            .filter(|name| !pins.iter().any(|pin| pin.key == **name))
            .map(|name| (*name).to_string())
            .collect()
    }

    #[test]
    fn every_foreign_name_is_a_pin_key() {
        // This fixture keeps the misspelling and alias cases observable without
        // depending on whichever reviewed pins the real roster currently names.
        let pins = [PinDef {
            key: "app",
            aliases: &["app-alias"],
            url: "u",
            sha256: "s",
            file: "f",
        }];
        assert!(unresolved(&["app"], &pins).is_empty());
        assert_eq!(unresolved(&["typo"], &pins), vec!["typo".to_string()]);
        assert_eq!(
            unresolved(&["app-alias"], &pins),
            vec!["app-alias".to_string()],
            "an alias must not satisfy the check — materialize marks on the key"
        );
        assert!(
            unresolved(FOREIGN, PINS).is_empty(),
            "a FOREIGN name that resolves to no pin marks nothing, and every \
             check downstream then answers `not foreign'"
        );
        // The count is the reviewable artifact, so it is asserted rather than
        // left to be read. Adding an application changes this number.
        assert_eq!(
            FOREIGN.len(),
            2,
            "exactly two application seeds are admitted: ripgrep and Claude Code"
        );
    }

    /// The mark is a property of the PIN, so it must answer the same however the
    /// pin was asked for — and an ALIAS in the roster must mark nothing, which is
    /// what makes the key-only rule above load-bearing rather than a convention.
    #[test]
    fn the_mark_rides_the_pin_not_the_name_it_was_asked_for() {
        let pin = PinDef {
            key: "app",
            aliases: &["app-alias"],
            url: "u",
            sha256: "s",
            file: "f",
        };
        assert!(materialize_with(&pin, "app", &["app"]).foreign());
        assert!(
            materialize_with(&pin, "app-alias", &["app"]).foreign(),
            "asking by alias must not lose the mark"
        );
        assert!(!materialize_with(&pin, "app", &["other"]).foreign());
        assert!(
            !materialize_with(&pin, "app", &["app-alias"]).foreign(),
            "an alias in the roster marks nothing — hence the key-only check"
        );
        let marked: Vec<String> = all()
            .into_iter()
            .filter(|pin| pin.foreign())
            .map(|pin| pin.key)
            .collect();
        assert_eq!(
            marked,
            vec![
                "claude-code-source".to_string(),
                "ripgrep-seed-source".to_string()
            ]
        );
    }

    /// The PRODUCTION lookup, over a REAL pin key, which is the route every
    /// shipped recipe takes: no catalog recipe calls `source_pin` directly, so
    /// the fixture-`PinDef` test above and the shipped code met nowhere until
    /// this one. Driven through `by_key_with` so the fixture can mark an ordinary
    /// production pin without changing the reviewed roster.
    #[test]
    fn the_production_lookup_marks_a_real_pin_from_the_roster() {
        let key = "zlib-x86-64-source";
        assert!(!by_key(key).unwrap().foreign(), "ordinary as shipped");
        assert!(by_key_with(key, &[key]).unwrap().foreign());
        assert_eq!(
            all_with(&[key])
                .iter()
                .filter(|pin| pin.foreign())
                .map(|pin| pin.key.clone())
                .collect::<Vec<_>>(),
            vec![key.to_string()],
            "exactly the named pin, and no neighbour"
        );
    }

    /// Pin the one production read of the reviewed roster. Value tests prove
    /// the current seed is marked; this source assertion also refuses a second
    /// bypassing materializer or an alternate roster read.
    #[test]
    fn the_public_pair_is_the_only_place_the_real_roster_is_read() {
        let src = include_str!("source_pins.rs");
        let shipped = match src.find("\n#[cfg(test)]\nmod tests {") {
            Some(at) => src.get(..at).unwrap_or(src),
            None => src,
        };
        let reads: Vec<&str> = shipped
            .lines()
            .map(str::trim)
            .filter(|l| l.contains("FOREIGN") && !l.starts_with("///") && !l.starts_with("//"))
            .collect();
        assert_eq!(
            reads,
            vec![
                "const FOREIGN: &[&str] = &[\"claude-code-source\", \"ripgrep-seed-source\"];",
                "all_with(FOREIGN)",
                "by_key_with(key, FOREIGN)",
                "foreign_names_with(FOREIGN)",
            ],
            "the roster is declared once and read by exactly the three public \
             entry points — a fourth reader, or any of these passing something \
             else, is how a marked pin comes back unmarked"
        );
    }

    /// A marked pin answers to its ALIASES too, which is what `by_key` does and
    /// what a key-only set silently disagreed with. Driven over the real
    /// `linux-source` pin, whose alias is the reachable case.
    #[test]
    fn a_marked_pins_aliases_are_marked_with_it() {
        const KEY: &str = "linux-source";
        let mut names = foreign_names_with(&[KEY]);
        names.sort();
        assert_eq!(
            names,
            vec!["linux-headers-x86-64".to_string(), KEY.to_string()],
            "every name `by_key' resolves to a marked pin must be marked"
        );
        // A pin with no alias contributes exactly its key, and a neighbour's
        // names are not swept in with it.
        assert_eq!(
            foreign_names_with(&["zlib-x86-64-source"]),
            vec!["zlib-x86-64-source".to_string()]
        );
        assert!(foreign_names_with(&[]).is_empty());
        // A name that resolves to no pin contributes nothing, which is what
        // `every_foreign_name_is_a_pin_key` refuses at the roster instead.
        assert!(foreign_names_with(&["linux-headers"]).is_empty());
    }

    #[test]
    fn linux_header_inputs_resolve_to_the_linux_source_pin() {
        let linux = by_key("linux-headers-x86-64").unwrap();
        assert_eq!(linux.file, "linux-4.14.67.tar.xz");
        assert_eq!(linux.key, "linux-headers-x86-64");
    }
}
