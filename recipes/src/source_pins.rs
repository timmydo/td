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
        // helpers, scp, sftp), without libcrypto or zlib.
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
        assert_eq!(all().len(), 61);
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
