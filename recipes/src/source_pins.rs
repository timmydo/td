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
    /// The upstream release this pin is, as its watch spells versions
    /// (`source_watches.rs`).
    version: &'static str,
}

/// Each pin's key and upstream release, in table order.
pub fn versions() -> Vec<(&'static str, &'static str)> {
    PINS.iter().map(|pin| (pin.key, pin.version)).collect()
}

const PINS: &[PinDef] = &[
    PinDef {
        key: "ca-certificates-source",
        aliases: &[],
        // Mozilla's trust store as rendered in PEM form by curl. Pin the dated
        // extract rather than curl's moving cacert.pem URL so a reviewed update
        // changes both the source name and its published adjacent SHA-256.
        url: "https://curl.se/ca/cacert-2026-11-05.pem",
        sha256: "427c064216761a1e72a8b30f0d954d08cb6e4435a82091fb865afd20c8497285",
        file: "cacert-2026-11-05.pem",
        version: "2026-11-05",
    },
    PinDef {
        key: "fd-source",
        aliases: &[],
        url: "https://static.crates.io/crates/fd-find/fd-find-10.5.1.crate",
        sha256: "d3d01f6d39c2cc05c28551a22ae66f82ef1526176ca9ad52073323528a7609f7",
        file: "fd-find-10.5.1.crate",
        version: "10.5.1",
    },
    PinDef {
        key: "git-x86-64-source",
        aliases: &[],
        // Git 2.56.0 is the source-built daily-driver client. Its td configuration
        // keeps local repositories and dispatches SSH remotes through the separate
        // td-ssh package and HTTP(S) remotes through td-net's helper, while
        // omitting language runtimes, libcurl, WebDAV, and direct OpenSSL use.
        url: "https://www.kernel.org/pub/software/scm/git/git-2.56.0.tar.xz",
        sha256: "26c56c296b38c0695b26fa95f475f1d01704d2d38e73465ca30b0b2f5dc789d3",
        file: "git-2.56.0.tar.xz",
        version: "2.56.0",
    },
    PinDef {
        key: "jetbrains-mono-nerd-font-source",
        aliases: &[],
        // Pinned upstream data (AGENTS.md): the Nerd Fonts release archive of
        // JetBrains Mono, compiled TrueType faces td-ui parses and never runs.
        url: "https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/JetBrainsMono.tar.xz",
        sha256: "04d5e8f903693f9dd13e16f867e994834e681eb3c72c0d337a770dcda09010cf",
        file: "jetbrains-mono-nerd-font-3.5.1.tar.xz",
        version: "3.5.1",
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
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-codicons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/codicons/LICENSE.txt",
        sha256: "d6239afa918961b465b07bf7411cbe34ff6685854f58553db7966f4881a0211f",
        file: "nerd-fonts-3.5.1-codicons-LICENSE.txt",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-font-awesome-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/font-awesome/LICENSE.txt",
        sha256: "de784a808496d49f5f80dd9d1d6e9a51cff712cfbc339e7c25155631ae3cdb5d",
        file: "nerd-fonts-3.5.1-font-awesome-LICENSE.txt",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-materialdesign-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/materialdesign/LICENSE",
        sha256: "3bae6305bcaa6d10219b999f65c342034c2d40b1c0e8567fa08605e73ccba829",
        file: "nerd-fonts-3.5.1-materialdesign-LICENSE",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-octicons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/octicons/LICENSE",
        sha256: "4f886642f9e8d99eeadd484acdac011c3fde8a338faf52f5ca1408c13b9a9b63",
        file: "nerd-fonts-3.5.1-octicons-LICENSE",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-pomicons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/pomicons/LICENSE",
        sha256: "cb4797a9745683f287c28833db596e9d68ec43082d59359b3a49c504f0f610ce",
        file: "nerd-fonts-3.5.1-pomicons-LICENSE",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-powerline-extra-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/powerline-extra/LICENSE",
        sha256: "544307a58e6826c24145b2af1c3578b53e2e21ae0e6c67ab4c6abd544304d26a",
        file: "nerd-fonts-3.5.1-powerline-extra-LICENSE",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-powerline-symbols-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/powerline-symbols/LICENSE.txt",
        sha256: "46d5355e4734468135d8e4a30d0916292cc4df2b3ec30aa68c866f82497933be",
        file: "nerd-fonts-3.5.1-powerline-symbols-LICENSE.txt",
        version: "3.5.1",
    },
    PinDef {
        key: "nerd-fonts-weather-icons-license-source",
        aliases: &[],
        url: "https://raw.githubusercontent.com/ryanoasis/nerd-fonts/v3.5.1/src/glyphs/weather-icons/OFL.txt",
        sha256: "69cdedc8a4f57a30d68e4f045af8b9cce0ec3b1a6e7eec1ad41737c894f26790",
        file: "nerd-fonts-3.5.1-weather-icons-OFL.txt",
        version: "3.5.1",
    },
    PinDef {
        key: "linux-firmware-amdgpu-dcn-3-1-4-dmcub-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/dcn_3_1_4_dmcub.bin",
        sha256: "454d3eb73045d0a46f9014c42c76df9e65ae9e5da45b8c331584443fe49a8516",
        file: "linux-firmware-20261019-amdgpu-dcn_3_1_4_dmcub.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-imu-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_imu.bin",
        sha256: "ac71f4eec713fc35b4a1fe27531e3eb04edd81eeac2cef64df01ac50d8510805",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_imu.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-mes1-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_mes1.bin",
        sha256: "3a8b60ea88b7adbbc71da0ddee0370d5d65c1d66186fd4f4ba9d16a854b24db8",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_mes1.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-mes-2-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_mes_2.bin",
        sha256: "962f56c7e661479e3cabf89c1fcd6344f10cfa8b8d1f672fc445bb21414c1392",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_mes_2.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-me-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_me.bin",
        sha256: "62c5e749ad848941e5d8b85f66eeb2b969ee82919724dfd94dbf6e354be8befc",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_me.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-mec-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_mec.bin",
        sha256: "b0a4006b3934adc1d7996e6d901206dea37203b4839debe4a5ed4e393a615cbb",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_mec.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-pfp-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_pfp.bin",
        sha256: "449c72cae1c57838185197ad4003aeee7ccbdba892a28bb2624d9e85f57a2df1",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_pfp.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-gc-11-0-1-rlc-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/gc_11_0_1_rlc.bin",
        sha256: "5f07dc1f0a75ecd9cb56d805ea869184a50ed9e43d811ebf833b8906534650ef",
        file: "linux-firmware-20261019-amdgpu-gc_11_0_1_rlc.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-asd-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_asd.bin",
        sha256: "ebe32baa51bc1fa04dc4df6d7e1f90a3ebb6b0e8931aeb40fc838e0ba2e2c192",
        file: "linux-firmware-20261019-amdgpu-green_sardine_asd.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-ce-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_ce.bin",
        sha256: "3bba10cff48ec51eb63e78078456596258c9fea2e8b38d91d02be66c0aca70fd",
        file: "linux-firmware-20261019-amdgpu-green_sardine_ce.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-dmcub-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_dmcub.bin",
        sha256: "1c9d74c58dd14dacd6d4a330dcf9820b2263d6a7d39fa54c826636a797b4b6a8",
        file: "linux-firmware-20261019-amdgpu-green_sardine_dmcub.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-me-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_me.bin",
        sha256: "671af908c8d631e659895d62db2e6bffc1d2c87d6db4c08246be0216f468fb9c",
        file: "linux-firmware-20261019-amdgpu-green_sardine_me.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-mec-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_mec.bin",
        sha256: "0e4c6712f75af494c3fd70386bc07739cab3aafd77c4fe8efb46c0bc1ae4eba6",
        file: "linux-firmware-20261019-amdgpu-green_sardine_mec.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-pfp-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_pfp.bin",
        sha256: "94e1474b3c8d422e35661d2d584297d891464c173ebf4ad45536277bdf7bdcdc",
        file: "linux-firmware-20261019-amdgpu-green_sardine_pfp.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-rlc-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_rlc.bin",
        sha256: "66f4397c2e9d19af245f9da290c8678eb1a51cc815fa7864a9383c2cd0db319d",
        file: "linux-firmware-20261019-amdgpu-green_sardine_rlc.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-sdma-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_sdma.bin",
        sha256: "cba8658ea950a99115ca46ee88c9622240632a0ca1731abbcd18b6d9be9e09de",
        file: "linux-firmware-20261019-amdgpu-green_sardine_sdma.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-ta-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_ta.bin",
        sha256: "53ad33b376b85b90f858c8cc3b4bc6c709135b4bd37340ed69b1f00fecd6626b",
        file: "linux-firmware-20261019-amdgpu-green_sardine_ta.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-green-sardine-vcn-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/green_sardine_vcn.bin",
        sha256: "5b32a9f1ee1176babe56910fa1fa52df938b60665febbeab1ce0ee38cb4d5c98",
        file: "linux-firmware-20261019-amdgpu-green_sardine_vcn.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-psp-13-0-4-ta-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/psp_13_0_4_ta.bin",
        sha256: "f4a2687e1cd940b016e308bc87b9b840f845526d9ef21147416d89c1c72a7944",
        file: "linux-firmware-20261019-amdgpu-psp_13_0_4_ta.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-psp-13-0-4-toc-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/psp_13_0_4_toc.bin",
        sha256: "11e3529d748e2b6b204681183360a6ead80665b8deda996dfcf35973f789fc3d",
        file: "linux-firmware-20261019-amdgpu-psp_13_0_4_toc.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-sdma-6-0-1-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/sdma_6_0_1.bin",
        sha256: "708c2c2c45262c98ebe8e34e32c3f1ce8eb5b007bab560c9ea9b576a8e4d6768",
        file: "linux-firmware-20261019-amdgpu-sdma_6_0_1.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-vcn-4-0-2-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amdgpu/vcn_4_0_2.bin",
        sha256: "5633a5212d41e1f3932c1e7e630b1f2c8c4d27bc53ce8149c3be75d1d832053c",
        file: "linux-firmware-20261019-amdgpu-vcn_4_0_2.bin",
    },
    PinDef {
        key: "linux-firmware-amdgpu-license-source",
        aliases: &[],
        version: "20261019",
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/LICENSES/LICENSE.amdgpu",
        sha256: "572872598565dc3513470de971a32bf9db301f47afeef3636345eadae33b2eee",
        file: "linux-firmware-20261019-amdgpu-LICENSE.amdgpu",
    },
    PinDef {
        key: "linux-firmware-ath11k-wcn6855-amss-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"): linux-firmware's
        // files for the Qualcomm WCN6855 Wi-Fi at its 20261019 tag, which the
        // device-firmware recipe carries unmodified for ath11k to hand to the
        // device.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath11k/WCN6855/hw2.0/amss.bin",
        sha256: "e12b23ddc4b8d2d2a10a651a5d6fdcd00f60fcae884d2cf5dad17627211fcdfd",
        file: "linux-firmware-20261019-ath11k-WCN6855-hw2.0-amss.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath11k-wcn6855-board-2-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath11k/WCN6855/hw2.0/board-2.bin",
        sha256: "3a92de58509ee13d417241a6b17be446fa3b0fa938c4cbd8492b1be25ae0f4a7",
        file: "linux-firmware-20261019-ath11k-WCN6855-hw2.0-board-2.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath11k-wcn6855-m3-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath11k/WCN6855/hw2.0/m3.bin",
        sha256: "0c590881870d0e6e98fc7d393ce05690e09287933b1b535e935bf5d98b77713f",
        file: "linux-firmware-20261019-ath11k-WCN6855-hw2.0-m3.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath11k-wcn6855-notice-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath11k/WCN6855/hw2.0/Notice.txt",
        sha256: "770720d03de70ff503c6d9bb6f6932fc7833d078647fe8352c6d5d49d44bbc5f",
        file: "linux-firmware-20261019-ath11k-WCN6855-hw2.0-Notice.txt",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath11k-wcn6855-regdb-source",
        aliases: &[],
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath11k/WCN6855/hw2.0/regdb.bin",
        sha256: "e1b774b1feda4cab01f5a26089124059539fc31544ac34129dce45c8ff26d645",
        file: "linux-firmware-20261019-ath11k-WCN6855-hw2.0-regdb.bin",
        version: "20261019",
    },
    PinDef {
        key: "intel-microcode-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"): Intel's release tree at its
        // tag. Only the licence and intel-ucode/ are read, not with-caveats/.
        url: "https://github.com/intel/Intel-Linux-Processor-Microcode-Data-Files/archive/refs/tags/microcode-20260925.tar.gz",
        sha256: "0ec19023dad1e6dd0c5909174b8dff2555b91cf01671e36271a1ef36655c0cfc",
        file: "intel-microcode-20260925.tar.gz",
        version: "20260925",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amd-ucode/microcode_amd.bin",
        sha256: "8a9d9e8b788e31e61cddc03cb1eeab5db99e0f667128943ff0780e6437d2e43e",
        file: "linux-firmware-20261019-amd-ucode-microcode_amd.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-fam15h-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amd-ucode/microcode_amd_fam15h.bin",
        sha256: "9d4a668410e72a4bdb86dc23e4261eca04daa83456ada02504115223f356981a",
        file: "linux-firmware-20261019-amd-ucode-microcode_amd_fam15h.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-fam16h-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amd-ucode/microcode_amd_fam16h.bin",
        sha256: "e02ad653b39c975d6c52674b50f23727bb6706bab7b4e5b391a4ce229e7ff121",
        file: "linux-firmware-20261019-amd-ucode-microcode_amd_fam16h.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-fam17h-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amd-ucode/microcode_amd_fam17h.bin",
        sha256: "966e4b796ec689c618868d08f8a37f347b0e7bfce4ae9df793e08471d363b7d0",
        file: "linux-firmware-20261019-amd-ucode-microcode_amd_fam17h.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-fam19h-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amd-ucode/microcode_amd_fam19h.bin",
        sha256: "c614d6db8056c5c67a9189b225124127d56990a190305bcb3927d50e132de7dd",
        file: "linux-firmware-20261019-amd-ucode-microcode_amd_fam19h.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-fam1ah-source",
        aliases: &[],
        // CPU microcode (AGENTS.md "CPU microcode"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/amd-ucode/microcode_amd_fam1ah.bin",
        sha256: "a6b22566f5aac770bdeae5610d417ee35ffd4af13eeaa624a2995f57e2d5b274",
        file: "linux-firmware-20261019-amd-ucode-microcode_amd_fam1ah.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-amd-ucode-license-source",
        aliases: &[],
        // The licence WHENCE names for AMD's microcode.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/LICENSES/LICENSE.amd-ucode",
        sha256: "2103bd999f77522c5ab5fd35df0579ed00e2a6b885f6f8ebd444e97ffa482991",
        file: "linux-firmware-20261019-LICENSE.amd-ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-6000g2a-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-6000g2a-6.ucode",
        sha256: "9c435c7f413ff2ff1320604c68beb6e8239bb93522dc4b7c02c8cc94a393d88a",
        file: "linux-firmware-20261019-iwlwifi-6000g2a-6.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-7260-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-7260-17.ucode",
        sha256: "5d81a6003df0228a497ad27f916ba2c979614b4c439b0f45a5f2873dc0607fe8",
        file: "linux-firmware-20261019-iwlwifi-7260-17.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-7265-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-7265-17.ucode",
        sha256: "91f699308f102a9ed90217d813bfdb355464ece958a4f8ac96bba5e9d77fe870",
        file: "linux-firmware-20261019-iwlwifi-7265-17.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-7265d-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-7265D-29.ucode",
        sha256: "c52bcaa0595f64d5fb32b7db3dd4b832ea7bf78f1037bcafc19d9c0f5c5c617e",
        file: "linux-firmware-20261019-iwlwifi-7265D-29.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-3160-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-3160-17.ucode",
        sha256: "0e893c1ed74c3186047508e93442b2d71a07dd5fac00d9dbdb68f34407d7be14",
        file: "linux-firmware-20261019-iwlwifi-3160-17.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-3168-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-3168-29.ucode",
        sha256: "3afae7bf4bb5fbb83a895ec7ad097b9fd20b7d8794f405c2823cbea5284e65c3",
        file: "linux-firmware-20261019-iwlwifi-3168-29.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-8000c-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-8000C-36.ucode",
        sha256: "479931721f5e168d69d67c297c11738acd75da390adb94f96030a1055f4cf57a",
        file: "linux-firmware-20261019-iwlwifi-8000C-36.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-8265-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-8265-36.ucode",
        sha256: "1336afcd028ed094d1fe33893c84c273bb5711be52970040344a75a12f276d56",
        file: "linux-firmware-20261019-iwlwifi-8265-36.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-9000-pu-b0-jf-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-9000-pu-b0-jf-b0-46.ucode",
        sha256: "9a0c5d938646942f9798e6212579c25771e4bb44301cd409b82abe361ca2c3e0",
        file: "linux-firmware-20261019-iwlwifi-9000-pu-b0-jf-b0-46.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-9260-th-b0-jf-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-9260-th-b0-jf-b0-46.ucode",
        sha256: "05f4c12514fddfd56fea660b3d2eb6b6e6f4a87a324d5d40464050312ddca911",
        file: "linux-firmware-20261019-iwlwifi-9260-th-b0-jf-b0-46.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-cc-a0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-cc-a0-77.ucode",
        sha256: "1866a58a1daee56b120362122d5f7548b443b53d33f2558a7e4407d58fd27dab",
        file: "linux-firmware-20261019-iwlwifi-cc-a0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-qu-b0-hr-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-Qu-b0-hr-b0-77.ucode",
        sha256: "df005896aafdf5eb9c427033781890a58c7ab97a8dc1e1ea6b8df4eefc033a9d",
        file: "linux-firmware-20261019-iwlwifi-Qu-b0-hr-b0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-qu-c0-hr-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-Qu-c0-hr-b0-77.ucode",
        sha256: "a250e179666288396e19ff15c307edfe8f089341c8713a22b457b7304fc1a6fe",
        file: "linux-firmware-20261019-iwlwifi-Qu-c0-hr-b0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-quz-a0-hr-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-QuZ-a0-hr-b0-77.ucode",
        sha256: "39d79587dd035bc84172b6bcf6de81217560088b0ceb06304d5121ba56db8d83",
        file: "linux-firmware-20261019-iwlwifi-QuZ-a0-hr-b0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-qu-b0-jf-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-Qu-b0-jf-b0-77.ucode",
        sha256: "b66c0551a5c6c3db23d32d05914ee5450ed1af98e4c4723d04906db443cc9c5e",
        file: "linux-firmware-20261019-iwlwifi-Qu-b0-jf-b0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-qu-c0-jf-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-Qu-c0-jf-b0-77.ucode",
        sha256: "d54ea577cc799b2e53b55fc320f6d9b6742d6582af7359fed21c89495d315077",
        file: "linux-firmware-20261019-iwlwifi-Qu-c0-jf-b0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-quz-a0-jf-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-QuZ-a0-jf-b0-77.ucode",
        sha256: "5a9aef304c60d92b26878bf0c7e0c138776b71e5ce772de2f0fb7455d5340478",
        file: "linux-firmware-20261019-iwlwifi-QuZ-a0-jf-b0-77.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ty-a0-gf-a0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ty-a0-gf-a0-89.ucode",
        sha256: "792478ef7752595525379a5068e2d47e310a2c344636b7df866c54746ced1dc0",
        file: "linux-firmware-20261019-iwlwifi-ty-a0-gf-a0-89.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-so-a0-gf-a0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-so-a0-gf-a0-89.ucode",
        sha256: "fae8230ebd1426dfb7d5b5594fdd89e346af5144080659d010b8d2a2fd2f7d4a",
        file: "linux-firmware-20261019-iwlwifi-so-a0-gf-a0-89.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-so-a0-hr-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-so-a0-hr-b0-89.ucode",
        sha256: "6c4a1b2b4720865ee954a055b5aac19377187b142e1b7c5567524c61445bde20",
        file: "linux-firmware-20261019-iwlwifi-so-a0-hr-b0-89.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ma-b0-gf-a0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ma-b0-gf-a0-89.ucode",
        sha256: "83c358a33c66bd41f753ee8e0e4fff5c9741694a397ac13a61e851e6bcf5dd6b",
        file: "linux-firmware-20261019-iwlwifi-ma-b0-gf-a0-89.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ma-b0-gf4-a0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ma-b0-gf4-a0-89.ucode",
        sha256: "2469b6bdd91a42fe9a57f5004a3e63838718147b5f604652a505d2db0cb28d48",
        file: "linux-firmware-20261019-iwlwifi-ma-b0-gf4-a0-89.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ma-b0-hr-b0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ma-b0-hr-b0-89.ucode",
        sha256: "3cdc5ae5fcbf3c4638758bef28b7695c010bd14d57d46357235923f2569b0ad7",
        file: "linux-firmware-20261019-iwlwifi-ma-b0-hr-b0-89.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-gl-c0-fm-c0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-gl-c0-fm-c0-c107.ucode",
        sha256: "cb33a383c65f3bc3bb19db319f50eedcae07c2ea2e53bfb4c88173d0b51144ed",
        file: "linux-firmware-20261019-iwlwifi-gl-c0-fm-c0-c107.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-bz-b0-fm-c0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-bz-b0-fm-c0-c107.ucode",
        sha256: "b271756a357b6f9a901d2f0c8a56a5c1046dae12905728dd9163729e3b5ceabc",
        file: "linux-firmware-20261019-iwlwifi-bz-b0-fm-c0-c107.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-sc-a0-fm-c0-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-sc-a0-fm-c0-c107.ucode",
        sha256: "57ba546820b2ebb7f9ab2e6949855b42fddad9ae002bed38192c6651de1013a6",
        file: "linux-firmware-20261019-iwlwifi-sc-a0-fm-c0-c107.ucode",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ty-a0-gf-a0-pnvm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ty-a0-gf-a0.pnvm",
        sha256: "9697e327f8171454b3c19b1ec9449753d5b934d4e91722f469616818a6d477b0",
        file: "linux-firmware-20261019-iwlwifi-ty-a0-gf-a0.pnvm",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-so-a0-gf-a0-pnvm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-so-a0-gf-a0.pnvm",
        sha256: "78abe08cf33bc209b96a887c7e3db8855a1d9650edacf849c4fa0d4596bcfac5",
        file: "linux-firmware-20261019-iwlwifi-so-a0-gf-a0.pnvm",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ma-b0-gf-a0-pnvm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ma-b0-gf-a0.pnvm",
        sha256: "65ccc7ed05c0ecb3ea0374b607ecacfdc693df4e5cbf825a4c91f71ce8fa9b7b",
        file: "linux-firmware-20261019-iwlwifi-ma-b0-gf-a0.pnvm",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-ma-b0-gf4-a0-pnvm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/intel/iwlwifi/iwlwifi-ma-b0-gf4-a0.pnvm",
        sha256: "ed9f78deb780a6c50aa0ac594558f3d3324cc57f38a31302c9d71ea853719fda",
        file: "linux-firmware-20261019-iwlwifi-ma-b0-gf4-a0.pnvm",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7921-wm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_RAM_CODE_MT7961_1.bin",
        sha256: "b94217a951518a9c14095765f367bc5dd7698f2dc033941d6f18fc2ebd6a2ab9",
        file: "linux-firmware-20261019-mediatek-WIFI_RAM_CODE_MT7961_1.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7921-rom-patch-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_MT7961_patch_mcu_1_2_hdr.bin",
        sha256: "a276c06c2b772adb50b86639d33c82824ff4c21d617feb78caea74c040b873f6",
        file: "linux-firmware-20261019-mediatek-WIFI_MT7961_patch_mcu_1_2_hdr.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7922-wm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_RAM_CODE_MT7922_1.bin",
        sha256: "d100e5e04c324fc8da6302112d1e0b4fac952286447d2419eabcd8fb0a1f5b93",
        file: "linux-firmware-20261019-mediatek-WIFI_RAM_CODE_MT7922_1.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7922-rom-patch-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_MT7922_patch_mcu_1_1_hdr.bin",
        sha256: "7fc9075d6d54b31e0539734614ca971fa52caf41b642a3000c8a4501591a4d98",
        file: "linux-firmware-20261019-mediatek-WIFI_MT7922_patch_mcu_1_1_hdr.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7925-wm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/mt7925/WIFI_RAM_CODE_MT7925_1_1.bin",
        sha256: "23ff53b4bb639b30481e2e06bb1688569ad1ba971b897936db539882abfbd120",
        file: "linux-firmware-20261019-mediatek-mt7925-WIFI_RAM_CODE_MT7925_1_1.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7925-rom-patch-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/mt7925/WIFI_MT7925_PATCH_MCU_1_1_hdr.bin",
        sha256: "8eb46014d2a6b4124472eee7476d995008a6f40b1daffef87eb42f30d98699e1",
        file: "linux-firmware-20261019-mediatek-mt7925-WIFI_MT7925_PATCH_MCU_1_1_hdr.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7920-wm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_RAM_CODE_MT7961_1a.bin",
        sha256: "3edec74ffe341b917e7fa4ddb2b68851411f2416555f10619a5900fa4b141e6c",
        file: "linux-firmware-20261019-mediatek-WIFI_RAM_CODE_MT7961_1a.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7920-rom-patch-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_MT7961_patch_mcu_1a_2_hdr.bin",
        sha256: "a766c4e39815e554c80ce5b1596095ff283a60db93356679fecd6494f71cee23",
        file: "linux-firmware-20261019-mediatek-WIFI_MT7961_patch_mcu_1a_2_hdr.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7902-wm-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_RAM_CODE_MT7902_1.bin",
        sha256: "b5958ac72c71fb8405e080f52d378d02b484812b9f1010c8843727056bbfc998",
        file: "linux-firmware-20261019-mediatek-WIFI_RAM_CODE_MT7902_1.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-mt7902-rom-patch-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/mediatek/WIFI_MT7902_patch_mcu_1_1_hdr.bin",
        sha256: "d73ba9e982f781221a2b9f10c42031f20a9dce046929fc0d55c791a417efe30a",
        file: "linux-firmware-20261019-mediatek-WIFI_MT7902_patch_mcu_1_1_hdr.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw88-rtw8822c-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw88/rtw8822c_fw.bin",
        sha256: "3deecb31210986d98cdbfb000391e08d602a6eee4ffc883969faa2b907ab03ba",
        file: "linux-firmware-20261019-rtw88-rtw8822c_fw.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw88-rtw8822c-wow-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw88/rtw8822c_wow_fw.bin",
        sha256: "6717fc6ff426a74571ac4f04cd9698fb2f356012c05c58aa64cf21ceb6b958a2",
        file: "linux-firmware-20261019-rtw88-rtw8822c_wow_fw.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw88-rtw8821c-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw88/rtw8821c_fw.bin",
        sha256: "2ef409bc418549fcf294061dd0cae1fc22fd9da79b60524950b25de18732f3f0",
        file: "linux-firmware-20261019-rtw88-rtw8821c_fw.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw88-rtw8822b-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw88/rtw8822b_fw.bin",
        sha256: "a72da690597bfa99d8eb6fc2ab090d18d8ad92ac2befd35db1c9e3662d8d8418",
        file: "linux-firmware-20261019-rtw88-rtw8822b_fw.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw89-rtw8852a-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw89/rtw8852a_fw-1.bin",
        sha256: "fdb5513cf7372440d7980a61aa4b753b192b056734a978458ae4a4ef7f4b2c1e",
        file: "linux-firmware-20261019-rtw89-rtw8852a_fw-1.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw89-rtw8852b-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw89/rtw8852b_fw-2.bin",
        sha256: "6ff6aeb56bb5b85cba333b755f8cc1dbfe9ff6e5b16e39f6ba01ce7f1eaa7714",
        file: "linux-firmware-20261019-rtw89-rtw8852b_fw-2.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw89-rtw8852c-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/rtw89/rtw8852c_fw-2.bin",
        sha256: "916e87d1470bce5400712f53d419ae68a0f44d858670c57e118d4f0b1d89c5b4",
        file: "linux-firmware-20261019-rtw89-rtw8852c_fw-2.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath10k-qca6174-firmware-6-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath10k/QCA6174/hw3.0/firmware-6.bin",
        sha256: "04d3bad5efa3f9fbe3ba53fd3e25fa9b0585ed227eea8111303b4e08861f979d",
        file: "linux-firmware-20261019-ath10k-QCA6174-hw3.0-firmware-6.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath10k-qca6174-board-2-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath10k/QCA6174/hw3.0/board-2.bin",
        sha256: "66e83dde1c9af535df1fcd17c72971a96a263357300e921b358d35a353227d60",
        file: "linux-firmware-20261019-ath10k-QCA6174-hw3.0-board-2.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath10k-qca6174-board-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath10k/QCA6174/hw3.0/board.bin",
        sha256: "1a8d225818b46986fc4f615594fbe448fa820618590d6902c8f844bb37cda667",
        file: "linux-firmware-20261019-ath10k-QCA6174-hw3.0-board.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath12k-wcn7850-amss-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath12k/WCN7850/hw2.0/amss.bin",
        sha256: "43aadfd3df887f27de74020273aee484bac6a31dd53068f91baf2a9b094d6a68",
        file: "linux-firmware-20261019-ath12k-WCN7850-hw2.0-amss.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath12k-wcn7850-m3-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath12k/WCN7850/hw2.0/m3.bin",
        sha256: "0e72f44df7defc269fe92dcea25d4d409046c04b77d41c510c52879b3dfc1055",
        file: "linux-firmware-20261019-ath12k-WCN7850-hw2.0-m3.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath12k-wcn7850-board-2-source",
        aliases: &[],
        // Device firmware (AGENTS.md "Device firmware"), at linux-firmware's tag.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath12k/WCN7850/hw2.0/board-2.bin",
        sha256: "6a98237c743d864773a547e81e23f0c6dc721dcd7331926bee7381e2152e00fd",
        file: "linux-firmware-20261019-ath12k-WCN7850-hw2.0-board-2.bin",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-iwlwifi-license-source",
        aliases: &[],
        // A notice linux-firmware's WHENCE names for Wi-Fi firmware.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/LICENSES/LICENCE.iwlwifi_firmware",
        sha256: "16d5040c7cf851fc693b7542e20870935b99802533ea1bfd231c377a2305e5c5",
        file: "linux-firmware-20261019-LICENCE.iwlwifi_firmware",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-mt76-license-source",
        aliases: &[],
        // A notice linux-firmware's WHENCE names for Wi-Fi firmware.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/LICENSES/LICENCE.mediatek",
        sha256: "a90d3f66704d85889945fec5525ea77622549da83aced1aac99828383f8f1805",
        file: "linux-firmware-20261019-LICENCE.mediatek",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-rtw-license-source",
        aliases: &[],
        // A notice linux-firmware's WHENCE names for Wi-Fi firmware.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/LICENSES/LICENCE.rtlwifi_firmware.txt",
        sha256: "a61351665b4f264f6c631364f85b907d8f8f41f8b369533ef4021765f9f3b62e",
        file: "linux-firmware-20261019-LICENCE.rtlwifi_firmware.txt",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath10k-qca6174-notice-source",
        aliases: &[],
        // A notice linux-firmware's WHENCE names for Wi-Fi firmware.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath10k/QCA6174/hw3.0/notice_ath10k_firmware-6.txt",
        sha256: "8ce5c6ea0542bf4aac31fc3ae16a39792ad22d0eae4543063fac56fb3380f021",
        file: "linux-firmware-20261019-ath10k-QCA6174-hw3.0-notice_ath10k_firmware-6.txt",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-ath12k-wcn7850-notice-source",
        aliases: &[],
        // A notice linux-firmware's WHENCE names for Wi-Fi firmware.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/ath12k/WCN7850/hw2.0/Notice.txt",
        sha256: "515bf4c9d620a87458e4447fe01a0e9bc384d1c3e0037cc4c3d2037b1ff25525",
        file: "linux-firmware-20261019-ath12k-WCN7850-hw2.0-Notice.txt",
        version: "20261019",
    },
    PinDef {
        key: "linux-firmware-qualcomm-atheros-license-source",
        aliases: &[],
        // The licence WHENCE names for those files, which the image ships
        // through `device-firmware`.
        url: "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/LICENSES/LICENSE.QualcommAtheros_ath10k",
        sha256: "337a55102138d7baa143ee4a4c6c91693e0113fece35d380b2a12109e8c23b3f",
        file: "linux-firmware-20261019-LICENSE.QualcommAtheros_ath10k",
        version: "20261019",
    },
    PinDef {
        key: "linux-kernel-source",
        aliases: &[],
        // The bootable kernel; libc's UAPI headers come from its own recipe.
        url: "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.1.tar.xz",
        sha256: "fa595aad93734f829c963c2a90ef7c3673224fd7fdae7b49406e397cde2de231",
        file: "linux-7.3.1.tar.xz",
        version: "7.3.1",
    },
    PinDef {
        key: "openssh-x86-64-source",
        aliases: &[],
        // OpenSSH Portable 10.6p2, td-ssh-test's peer and in no image. The recipe
        // builds only what that check drives (ssh, ssh-keygen, sshd and its two
        // helpers), without libcrypto or zlib.
        url: "https://cdn.openbsd.org/pub/OpenBSD/OpenSSH/portable/openssh-10.6p2.tar.gz",
        sha256: "9ae44872090cf2f06b3cf9ff147e7f16f309671340763ed2f99248398ae29d5b",
        file: "openssh-10.6p2.tar.gz",
        version: "10.6p2",
    },
    PinDef {
        key: "ripgrep-seed-source",
        aliases: &[],
        // The upstream x86-64 musl release is a static PIE and is admitted only
        // as the first sandboxed-application seed. It is never a build tool.
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz",
        sha256: "33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c",
        file: "ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz",
        version: "15.2.0",
    },
    PinDef {
        key: "ripgrep-source",
        aliases: &[],
        url: "https://static.crates.io/crates/ripgrep/ripgrep-15.2.0.crate",
        sha256: "a30750b6d0743bfdd2656ebbaf4555aa278c43144b84bc389bcbfa399485ec71",
        file: "ripgrep-15.2.0.crate",
        version: "15.2.0",
    },
    PinDef {
        key: "rust-rustc-dist",
        aliases: &[],
        // The upstream Rust 1.99.1 release (channel-rust-1.99.1.toml, dated
        // 2026-10-15), retargeted onto td's runtime by `rust-toolchain`.
        url: "https://static.rust-lang.org/dist/2026-10-15/rustc-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "2ff05ff9c08a90861e7f6e4065b40a8b87463b4576186fd613871c3e37f2091f",
        file: "rustc-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        version: "1.99.1",
    },
    PinDef {
        key: "rust-std-dist",
        aliases: &[],
        url: "https://static.rust-lang.org/dist/2026-10-15/rust-std-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "109b074225bf5b9ea3820890876bfdf4ed161adbfaf94b2b4b5984c85ee5d958",
        file: "rust-std-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        version: "1.99.1",
    },
    PinDef {
        key: "rust-cargo-dist",
        aliases: &[],
        url: "https://static.rust-lang.org/dist/2026-10-15/cargo-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "53f3521949404d595efb06a55c12c5658d4ed1c75566f33d3d514eaebe70bd8a",
        file: "cargo-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        version: "1.99.1",
    },
    PinDef {
        key: "rust-clippy-dist",
        aliases: &[],
        url: "https://static.rust-lang.org/dist/2026-10-15/clippy-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        sha256: "458339712a058e52cc4522ff9ef8b59cd1ea1340312216cd94194c464a3f6dc5",
        file: "clippy-1.99.1-x86_64-unknown-linux-gnu.tar.xz",
        version: "1.99.1",
    },
    PinDef {
        key: "tzdata-source",
        aliases: &[],
        // Approved IANA data-only input; compile with the existing td glibc zic.
        url: "https://data.iana.org/time-zones/releases/tzdata2026f.tar.gz",
        sha256: "01b4a91cec5167ed48c11e8ab78f2048cead78ce94b0cafcad0ae8c5a3044d3d",
        file: "tzdata2026f.tar.gz",
        version: "2026f",
    },
    PinDef {
        key: "uutils-source",
        aliases: &[],
        // uutils coreutils 0.12.0 — the read-only-root Rust core userland (#547).
        // The `--auto` source_input pin; the dependency closure is pinned
        // separately by the committed recipes/locks/uutils/Cargo.lock.
        url: "https://static.crates.io/crates/coreutils/coreutils-0.12.0.crate",
        sha256: "a179dc3f3af0389cbd0a8d238ea047abbd72b47225bb49cbadb9a3824a44938e",
        file: "coreutils-0.12.0.crate",
        version: "0.12.0",
    },
    PinDef {
        key: "zlib-x86-64-source",
        aliases: &[],
        url: "https://github.com/madler/zlib/releases/download/v1.4/zlib-1.4.tar.gz",
        sha256: "02dd159783677a749c04adbdd02b55b8a2bcc7e054f51b76b549199a916ad584",
        file: "zlib-1.4.tar.gz",
        version: "1.4",
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
const FOREIGN: &[&str] = &["ripgrep-seed-source"];

/// The pins AGENTS.md "CPU microcode" admits: the code a host CPU itself loads,
/// signed by its vendor. Unmarked, as device firmware is; the closure query names
/// each. Adding an entry is a reviewed pin.
pub const CPU_MICROCODE: &[&str] = &[
    "intel-microcode-source",
    "linux-firmware-amd-ucode-source",
    "linux-firmware-amd-ucode-fam15h-source",
    "linux-firmware-amd-ucode-fam16h-source",
    "linux-firmware-amd-ucode-fam17h-source",
    "linux-firmware-amd-ucode-fam19h-source",
    "linux-firmware-amd-ucode-fam1ah-source",
];

/// The pins AGENTS.md "Device firmware" admits: linux-firmware files a
/// peripheral runs or reads, which `device-firmware` carries unmodified and
/// the host never executes. Unmarked, so no channel refuses them; the closure query
/// names each. Adding an entry is a reviewed pin; firmware the host CPU runs
/// is `CPU_MICROCODE`, and any other belongs to no class.
pub const DEVICE_FIRMWARE: &[&str] = &[
    "linux-firmware-iwlwifi-6000g2a-source",
    "linux-firmware-iwlwifi-7260-source",
    "linux-firmware-iwlwifi-7265-source",
    "linux-firmware-iwlwifi-7265d-source",
    "linux-firmware-iwlwifi-3160-source",
    "linux-firmware-iwlwifi-3168-source",
    "linux-firmware-iwlwifi-8000c-source",
    "linux-firmware-iwlwifi-8265-source",
    "linux-firmware-iwlwifi-9000-pu-b0-jf-b0-source",
    "linux-firmware-iwlwifi-9260-th-b0-jf-b0-source",
    "linux-firmware-iwlwifi-cc-a0-source",
    "linux-firmware-iwlwifi-qu-b0-hr-b0-source",
    "linux-firmware-iwlwifi-qu-c0-hr-b0-source",
    "linux-firmware-iwlwifi-quz-a0-hr-b0-source",
    "linux-firmware-iwlwifi-qu-b0-jf-b0-source",
    "linux-firmware-iwlwifi-qu-c0-jf-b0-source",
    "linux-firmware-iwlwifi-quz-a0-jf-b0-source",
    "linux-firmware-iwlwifi-ty-a0-gf-a0-source",
    "linux-firmware-iwlwifi-so-a0-gf-a0-source",
    "linux-firmware-iwlwifi-so-a0-hr-b0-source",
    "linux-firmware-iwlwifi-ma-b0-gf-a0-source",
    "linux-firmware-iwlwifi-ma-b0-gf4-a0-source",
    "linux-firmware-iwlwifi-ma-b0-hr-b0-source",
    "linux-firmware-iwlwifi-gl-c0-fm-c0-source",
    "linux-firmware-iwlwifi-bz-b0-fm-c0-source",
    "linux-firmware-iwlwifi-sc-a0-fm-c0-source",
    "linux-firmware-iwlwifi-ty-a0-gf-a0-pnvm-source",
    "linux-firmware-iwlwifi-so-a0-gf-a0-pnvm-source",
    "linux-firmware-iwlwifi-ma-b0-gf-a0-pnvm-source",
    "linux-firmware-iwlwifi-ma-b0-gf4-a0-pnvm-source",
    "linux-firmware-mt76-mt7921-wm-source",
    "linux-firmware-mt76-mt7921-rom-patch-source",
    "linux-firmware-mt76-mt7922-wm-source",
    "linux-firmware-mt76-mt7922-rom-patch-source",
    "linux-firmware-mt76-mt7925-wm-source",
    "linux-firmware-mt76-mt7925-rom-patch-source",
    "linux-firmware-mt76-mt7920-wm-source",
    "linux-firmware-mt76-mt7920-rom-patch-source",
    "linux-firmware-mt76-mt7902-wm-source",
    "linux-firmware-mt76-mt7902-rom-patch-source",
    "linux-firmware-rtw88-rtw8822c-source",
    "linux-firmware-rtw88-rtw8822c-wow-source",
    "linux-firmware-rtw88-rtw8821c-source",
    "linux-firmware-rtw88-rtw8822b-source",
    "linux-firmware-rtw89-rtw8852a-source",
    "linux-firmware-rtw89-rtw8852b-source",
    "linux-firmware-rtw89-rtw8852c-source",
    "linux-firmware-ath10k-qca6174-firmware-6-source",
    "linux-firmware-ath10k-qca6174-board-2-source",
    "linux-firmware-ath10k-qca6174-board-source",
    "linux-firmware-ath12k-wcn7850-amss-source",
    "linux-firmware-ath12k-wcn7850-m3-source",
    "linux-firmware-ath12k-wcn7850-board-2-source",
    "linux-firmware-amdgpu-gc-11-0-1-mes-2-source",
    "linux-firmware-amdgpu-gc-11-0-1-mes1-source",
    "linux-firmware-amdgpu-gc-11-0-1-imu-source",
    "linux-firmware-amdgpu-dcn-3-1-4-dmcub-source",
    "linux-firmware-amdgpu-gc-11-0-1-me-source",
    "linux-firmware-amdgpu-gc-11-0-1-mec-source",
    "linux-firmware-amdgpu-gc-11-0-1-pfp-source",
    "linux-firmware-amdgpu-gc-11-0-1-rlc-source",
    "linux-firmware-amdgpu-green-sardine-asd-source",
    "linux-firmware-amdgpu-green-sardine-ce-source",
    "linux-firmware-amdgpu-green-sardine-dmcub-source",
    "linux-firmware-amdgpu-green-sardine-me-source",
    "linux-firmware-amdgpu-green-sardine-mec-source",
    "linux-firmware-amdgpu-green-sardine-pfp-source",
    "linux-firmware-amdgpu-green-sardine-rlc-source",
    "linux-firmware-amdgpu-green-sardine-sdma-source",
    "linux-firmware-amdgpu-green-sardine-ta-source",
    "linux-firmware-amdgpu-green-sardine-vcn-source",
    "linux-firmware-amdgpu-psp-13-0-4-ta-source",
    "linux-firmware-amdgpu-psp-13-0-4-toc-source",
    "linux-firmware-amdgpu-sdma-6-0-1-source",
    "linux-firmware-amdgpu-vcn-4-0-2-source",
    "linux-firmware-ath11k-wcn6855-amss-source",
    "linux-firmware-ath11k-wcn6855-board-2-source",
    "linux-firmware-ath11k-wcn6855-m3-source",
    "linux-firmware-ath11k-wcn6855-regdb-source",
];

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
        assert_eq!(all().len(), 119);
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

    /// Device firmware is a pin key from linux-firmware's reviewed tag,
    /// never an alias and never marked: no recipe executes it, and the mark
    /// would refuse the kernel's ordinary input channel.
    #[test]
    fn device_firmware_names_unmarked_pins_from_the_reviewed_tag() {
        assert!(unresolved(DEVICE_FIRMWARE, PINS).is_empty());
        for key in DEVICE_FIRMWARE {
            let pin = by_key(key).unwrap_or_else(|| unreachable!("{key}"));
            assert!(!pin.foreign(), "{key} is marked");
            assert!(
                pin.url.starts_with(
                    "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/"
                ),
                "{key}"
            );
            assert!(PINS.iter().all(|def| !def.aliases.contains(key)));
        }
    }

    #[test]
    fn cpu_microcode_is_unmarked_and_not_device_firmware() {
        assert!(unresolved(CPU_MICROCODE, PINS).is_empty());
        for key in CPU_MICROCODE {
            let pin = by_key(key).unwrap_or_else(|| unreachable!("{key}"));
            assert!(!pin.foreign(), "{key} is marked");
            assert!(!DEVICE_FIRMWARE.contains(key), "{key}");
        }
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
            version: "1",
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
            1,
            "only ripgrep remains a pinned foreign payload"
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
            version: "1",
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
        assert_eq!(marked, vec!["ripgrep-seed-source".to_string()]);
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
                "const FOREIGN: &[&str] = &[\"ripgrep-seed-source\"];",
                "all_with(FOREIGN)",
                "by_key_with(key, FOREIGN)",
                "foreign_names_with(FOREIGN)",
            ],
            "the roster is declared once and read by exactly the three public \
             entry points — a fourth reader, or any of these passing something \
             else, is how a marked pin comes back unmarked"
        );
    }

    #[test]
    fn foreign_names_include_only_declared_pins() {
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
}
