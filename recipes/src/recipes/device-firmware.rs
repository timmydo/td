use crate::types::{Recipe, Step};

// The device firmware drivers ask the kernel's loader for, unmodified, with the
// notices and licences its licences ask a distribution to carry (AGENTS.md
// "Device firmware"). A desktop image holds this in its store, links it at
// /lib/firmware and copies the files into its deployment initramfs.

/// Where the files sit in the output, the loader's own search path.
pub const DIR: &str = "lib/firmware";

/// Where the notices sit in the output.
pub const NOTICES_DIR: &str = "share/licenses/linux-firmware";

/// What the drivers built into the kernel request at probe, each by its pin and
/// the name the driver asks for: the Phoenix and Cezanne GPUs, then Wi-Fi. For
/// iwlwifi, the highest API the kernel accepts for each family; the files are
/// the driver's names, linked in linux-firmware from `intel/iwlwifi/`.
pub const FIRMWARE: &[(&str, &str)] = &[
    (
        "linux-firmware-amdgpu-gc-11-0-1-imu-source",
        "amdgpu/gc_11_0_1_imu.bin",
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-mes1-source",
        "amdgpu/gc_11_0_1_mes1.bin",
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-mes-2-source",
        "amdgpu/gc_11_0_1_mes_2.bin",
    ),
    (
        "linux-firmware-amdgpu-dcn-3-1-4-dmcub-source",
        "amdgpu/dcn_3_1_4_dmcub.bin",
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-me-source",
        "amdgpu/gc_11_0_1_me.bin",
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-mec-source",
        "amdgpu/gc_11_0_1_mec.bin",
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-pfp-source",
        "amdgpu/gc_11_0_1_pfp.bin",
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-rlc-source",
        "amdgpu/gc_11_0_1_rlc.bin",
    ),
    (
        "linux-firmware-amdgpu-psp-13-0-4-ta-source",
        "amdgpu/psp_13_0_4_ta.bin",
    ),
    (
        "linux-firmware-amdgpu-psp-13-0-4-toc-source",
        "amdgpu/psp_13_0_4_toc.bin",
    ),
    (
        "linux-firmware-amdgpu-sdma-6-0-1-source",
        "amdgpu/sdma_6_0_1.bin",
    ),
    (
        "linux-firmware-amdgpu-vcn-4-0-2-source",
        "amdgpu/vcn_4_0_2.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-asd-source",
        "amdgpu/green_sardine_asd.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-ce-source",
        "amdgpu/green_sardine_ce.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-dmcub-source",
        "amdgpu/green_sardine_dmcub.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-me-source",
        "amdgpu/green_sardine_me.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-mec-source",
        "amdgpu/green_sardine_mec.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-pfp-source",
        "amdgpu/green_sardine_pfp.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-rlc-source",
        "amdgpu/green_sardine_rlc.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-sdma-source",
        "amdgpu/green_sardine_sdma.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-ta-source",
        "amdgpu/green_sardine_ta.bin",
    ),
    (
        "linux-firmware-amdgpu-green-sardine-vcn-source",
        "amdgpu/green_sardine_vcn.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-amss-source",
        "ath11k/WCN6855/hw2.0/amss.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-m3-source",
        "ath11k/WCN6855/hw2.0/m3.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-board-2-source",
        "ath11k/WCN6855/hw2.0/board-2.bin",
    ),
    (
        "linux-firmware-ath11k-wcn6855-regdb-source",
        "ath11k/WCN6855/hw2.0/regdb.bin",
    ),
    (
        "linux-firmware-iwlwifi-6000g2a-source",
        "iwlwifi-6000g2a-6.ucode",
    ),
    (
        "linux-firmware-iwlwifi-7260-source",
        "iwlwifi-7260-17.ucode",
    ),
    (
        "linux-firmware-iwlwifi-7265-source",
        "iwlwifi-7265-17.ucode",
    ),
    (
        "linux-firmware-iwlwifi-7265d-source",
        "iwlwifi-7265D-29.ucode",
    ),
    (
        "linux-firmware-iwlwifi-3160-source",
        "iwlwifi-3160-17.ucode",
    ),
    (
        "linux-firmware-iwlwifi-3168-source",
        "iwlwifi-3168-29.ucode",
    ),
    (
        "linux-firmware-iwlwifi-8000c-source",
        "iwlwifi-8000C-36.ucode",
    ),
    (
        "linux-firmware-iwlwifi-8265-source",
        "iwlwifi-8265-36.ucode",
    ),
    (
        "linux-firmware-iwlwifi-9000-pu-b0-jf-b0-source",
        "iwlwifi-9000-pu-b0-jf-b0-46.ucode",
    ),
    (
        "linux-firmware-iwlwifi-9260-th-b0-jf-b0-source",
        "iwlwifi-9260-th-b0-jf-b0-46.ucode",
    ),
    (
        "linux-firmware-iwlwifi-cc-a0-source",
        "iwlwifi-cc-a0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-qu-b0-hr-b0-source",
        "iwlwifi-Qu-b0-hr-b0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-qu-c0-hr-b0-source",
        "iwlwifi-Qu-c0-hr-b0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-quz-a0-hr-b0-source",
        "iwlwifi-QuZ-a0-hr-b0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-qu-b0-jf-b0-source",
        "iwlwifi-Qu-b0-jf-b0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-qu-c0-jf-b0-source",
        "iwlwifi-Qu-c0-jf-b0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-quz-a0-jf-b0-source",
        "iwlwifi-QuZ-a0-jf-b0-77.ucode",
    ),
    (
        "linux-firmware-iwlwifi-ty-a0-gf-a0-source",
        "iwlwifi-ty-a0-gf-a0-89.ucode",
    ),
    (
        "linux-firmware-iwlwifi-so-a0-gf-a0-source",
        "iwlwifi-so-a0-gf-a0-89.ucode",
    ),
    (
        "linux-firmware-iwlwifi-so-a0-hr-b0-source",
        "iwlwifi-so-a0-hr-b0-89.ucode",
    ),
    (
        "linux-firmware-iwlwifi-ma-b0-gf-a0-source",
        "iwlwifi-ma-b0-gf-a0-89.ucode",
    ),
    (
        "linux-firmware-iwlwifi-ma-b0-gf4-a0-source",
        "iwlwifi-ma-b0-gf4-a0-89.ucode",
    ),
    (
        "linux-firmware-iwlwifi-ma-b0-hr-b0-source",
        "iwlwifi-ma-b0-hr-b0-89.ucode",
    ),
    (
        "linux-firmware-iwlwifi-gl-c0-fm-c0-source",
        "iwlwifi-gl-c0-fm-c0-c107.ucode",
    ),
    (
        "linux-firmware-iwlwifi-bz-b0-fm-c0-source",
        "iwlwifi-bz-b0-fm-c0-c107.ucode",
    ),
    (
        "linux-firmware-iwlwifi-sc-a0-fm-c0-source",
        "iwlwifi-sc-a0-fm-c0-c107.ucode",
    ),
    (
        "linux-firmware-iwlwifi-ty-a0-gf-a0-pnvm-source",
        "iwlwifi-ty-a0-gf-a0.pnvm",
    ),
    (
        "linux-firmware-iwlwifi-so-a0-gf-a0-pnvm-source",
        "iwlwifi-so-a0-gf-a0.pnvm",
    ),
    (
        "linux-firmware-iwlwifi-ma-b0-gf-a0-pnvm-source",
        "iwlwifi-ma-b0-gf-a0.pnvm",
    ),
    (
        "linux-firmware-iwlwifi-ma-b0-gf4-a0-pnvm-source",
        "iwlwifi-ma-b0-gf4-a0.pnvm",
    ),
    (
        "linux-firmware-mt76-mt7921-wm-source",
        "mediatek/WIFI_RAM_CODE_MT7961_1.bin",
    ),
    (
        "linux-firmware-mt76-mt7921-rom-patch-source",
        "mediatek/WIFI_MT7961_patch_mcu_1_2_hdr.bin",
    ),
    (
        "linux-firmware-mt76-mt7922-wm-source",
        "mediatek/WIFI_RAM_CODE_MT7922_1.bin",
    ),
    (
        "linux-firmware-mt76-mt7922-rom-patch-source",
        "mediatek/WIFI_MT7922_patch_mcu_1_1_hdr.bin",
    ),
    (
        "linux-firmware-mt76-mt7925-wm-source",
        "mediatek/mt7925/WIFI_RAM_CODE_MT7925_1_1.bin",
    ),
    (
        "linux-firmware-mt76-mt7925-rom-patch-source",
        "mediatek/mt7925/WIFI_MT7925_PATCH_MCU_1_1_hdr.bin",
    ),
    (
        "linux-firmware-mt76-mt7920-wm-source",
        "mediatek/WIFI_RAM_CODE_MT7961_1a.bin",
    ),
    (
        "linux-firmware-mt76-mt7920-rom-patch-source",
        "mediatek/WIFI_MT7961_patch_mcu_1a_2_hdr.bin",
    ),
    (
        "linux-firmware-mt76-mt7902-wm-source",
        "mediatek/WIFI_RAM_CODE_MT7902_1.bin",
    ),
    (
        "linux-firmware-mt76-mt7902-rom-patch-source",
        "mediatek/WIFI_MT7902_patch_mcu_1_1_hdr.bin",
    ),
    (
        "linux-firmware-rtw88-rtw8822c-source",
        "rtw88/rtw8822c_fw.bin",
    ),
    (
        "linux-firmware-rtw88-rtw8822c-wow-source",
        "rtw88/rtw8822c_wow_fw.bin",
    ),
    (
        "linux-firmware-rtw88-rtw8821c-source",
        "rtw88/rtw8821c_fw.bin",
    ),
    (
        "linux-firmware-rtw88-rtw8822b-source",
        "rtw88/rtw8822b_fw.bin",
    ),
    (
        "linux-firmware-rtw89-rtw8852a-source",
        "rtw89/rtw8852a_fw-1.bin",
    ),
    (
        "linux-firmware-rtw89-rtw8852b-source",
        "rtw89/rtw8852b_fw-2.bin",
    ),
    (
        "linux-firmware-rtw89-rtw8852c-source",
        "rtw89/rtw8852c_fw-2.bin",
    ),
    (
        "linux-firmware-ath10k-qca6174-firmware-6-source",
        "ath10k/QCA6174/hw3.0/firmware-6.bin",
    ),
    (
        "linux-firmware-ath10k-qca6174-board-2-source",
        "ath10k/QCA6174/hw3.0/board-2.bin",
    ),
    (
        "linux-firmware-ath10k-qca6174-board-source",
        "ath10k/QCA6174/hw3.0/board.bin",
    ),
    (
        "linux-firmware-ath12k-wcn7850-amss-source",
        "ath12k/WCN7850/hw2.0/amss.bin",
    ),
    (
        "linux-firmware-ath12k-wcn7850-m3-source",
        "ath12k/WCN7850/hw2.0/m3.bin",
    ),
    (
        "linux-firmware-ath12k-wcn7850-board-2-source",
        "ath12k/WCN7850/hw2.0/board-2.bin",
    ),
];

/// Each notice's pin and its path under `NOTICES_DIR`, as linux-firmware lays
/// it out.
pub const NOTICES: &[(&str, &str)] = &[
    ("linux-firmware-amdgpu-license-source", "LICENSE.amdgpu"),
    (
        "linux-firmware-ath11k-wcn6855-notice-source",
        "ath11k/WCN6855/hw2.0/Notice.txt",
    ),
    (
        "linux-firmware-qualcomm-atheros-license-source",
        "LICENSE.QualcommAtheros_ath10k",
    ),
    (
        "linux-firmware-iwlwifi-license-source",
        "LICENCE.iwlwifi_firmware",
    ),
    ("linux-firmware-mt76-license-source", "LICENCE.mediatek"),
    (
        "linux-firmware-rtw-license-source",
        "LICENCE.rtlwifi_firmware.txt",
    ),
    (
        "linux-firmware-ath10k-qca6174-notice-source",
        "ath10k/QCA6174/hw3.0/notice_ath10k_firmware-6.txt",
    ),
    (
        "linux-firmware-ath12k-wcn7850-notice-source",
        "ath12k/WCN7850/hw2.0/Notice.txt",
    ),
];

/// Each directory under `DIR` that holds a file, parents first.
pub fn directories() -> Vec<String> {
    let mut dirs = std::collections::BTreeSet::new();
    for (_, name) in FIRMWARE {
        let mut at = 0;
        while let Some(slash) = name.get(at..).and_then(|rest| rest.find('/')) {
            at += slash + 1;
            dirs.insert(name.get(..at - 1).unwrap_or_default().to_string());
        }
    }
    dirs.into_iter().collect()
}

pub fn recipe() -> Recipe {
    let file = |name: &str| format!("{{out}}/{DIR}/{name}");
    let notice = |path: &str| format!("{{out}}/{NOTICES_DIR}/{path}");
    let mut steps: Vec<Step> = directories()
        .iter()
        .map(|dir| Step::MkDir { path: file(dir) })
        .collect();
    let mut notice_dirs: Vec<&str> = NOTICES
        .iter()
        .filter_map(|(_, path)| path.rsplit_once('/').map(|(dir, _)| dir))
        .collect();
    notice_dirs.sort_unstable();
    notice_dirs.dedup();
    steps.extend(
        notice_dirs
            .iter()
            .map(|dir| Step::MkDir { path: notice(dir) }),
    );
    let copy = |pin: &str, to: String| Step::CopyFile {
        file: format!("{{in:{pin}}}"),
        to,
        exec: false,
    };
    steps.extend(FIRMWARE.iter().map(|(pin, name)| copy(pin, file(name))));
    steps.extend(NOTICES.iter().map(|(pin, path)| copy(pin, notice(path))));
    steps.push(Step::Require {
        paths: FIRMWARE
            .iter()
            .map(|(_, name)| file(name))
            .chain(NOTICES.iter().map(|(_, path)| notice(path)))
            .collect(),
        exec: false,
    });
    let pins: Vec<&str> = FIRMWARE
        .iter()
        .chain(NOTICES.iter())
        .map(|(pin, _)| *pin)
        .collect();
    Recipe::mesboot("device-firmware", "20261019")
        .inputs(&pins)
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_holds_exactly_the_firmware_roster_as_unmarked_data() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        assert_eq!(recipe.native_inputs, None);
        assert!(!recipe.build_root);
        let mut staged: Vec<&str> = FIRMWARE.iter().map(|(pin, _)| *pin).collect();
        let mut roster = crate::source_pins::DEVICE_FIRMWARE.to_vec();
        staged.sort_unstable();
        roster.sort_unstable();
        assert_eq!(staged, roster, "the recipe stages exactly the roster");
        let steps = recipe.steps.unwrap_or_default();
        assert!(
            steps.iter().all(|step| !matches!(step, Step::Run { .. })),
            "copied unmodified, by the engine"
        );
        for (pin, name) in FIRMWARE {
            assert!(steps.iter().any(|step| matches!(
                step,
                Step::CopyFile { file, to, exec: false }
                    if *file == format!("{{in:{pin}}}")
                        && *to == format!("{{out}}/lib/firmware/{name}")
            )));
        }
    }

    #[test]
    fn the_notices_are_data_from_the_firmwares_tag() {
        for (key, _) in NOTICES {
            let pin = crate::source_pins::all()
                .into_iter()
                .find(|def| def.key == *key)
                .unwrap_or_else(|| unreachable!("{key} is not pinned"));
            assert!(!pin.foreign(), "{key}");
            assert!(
                pin.url.starts_with(
                    "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/20261019/"
                ),
                "{key}"
            );
            assert!(!crate::source_pins::DEVICE_FIRMWARE.contains(key));
        }
    }

    /// The roster reaches the graph only through this recipe, on any input
    /// channel, so the closure query's `device-firmware` lines cannot miss a
    /// copy; and every linux-firmware pin is the roster or a notice.
    #[test]
    fn only_this_recipe_names_device_firmware() {
        for (stem, recipe) in crate::catalog::all() {
            let named = recipe
                .inputs
                .iter()
                .chain(recipe.native_inputs.iter())
                .chain(recipe.payload_inputs.iter())
                .flatten()
                .chain(recipe.source_input.iter());
            for key in named {
                if crate::source_pins::DEVICE_FIRMWARE.contains(&key.as_str()) {
                    assert_eq!(stem, "device-firmware", "{stem} names {key}");
                }
            }
        }
        for pin in crate::source_pins::all() {
            if pin.url.contains("/kernel-firmware/linux-firmware/") {
                assert!(
                    crate::source_pins::DEVICE_FIRMWARE.contains(&pin.key.as_str())
                        || crate::source_pins::CPU_MICROCODE.contains(&pin.key.as_str())
                        || pin.key == "linux-firmware-amd-ucode-license-source"
                        || NOTICES.iter().any(|(key, _)| *key == pin.key),
                    "{} is neither firmware, microcode nor a shipped notice",
                    pin.key
                );
            }
        }
    }

    #[test]
    fn directories_come_parents_first() {
        let dirs = directories();
        assert!(dirs.contains(&"amdgpu".to_string()));
        for path in ["ath11k", "ath11k/WCN6855", "ath11k/WCN6855/hw2.0"] {
            assert!(dirs.contains(&path.to_string()), "{path}");
        }
        let mut sorted = dirs.clone();
        sorted.sort();
        assert_eq!(dirs, sorted);
    }
}
