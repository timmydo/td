use crate::types::{Recipe, Step};

// The device firmware drivers ask the kernel's loader for, unmodified, with the
// notices and licences its licences ask a distribution to carry (AGENTS.md
// "Device firmware"). A desktop image holds this in its store, links it at
// /lib/firmware and copies the files into its deployment initramfs.

/// Where the files sit in the output, the loader's own search path.
pub const DIR: &str = "lib/firmware";

/// Where the notices sit in the output.
pub const NOTICES_DIR: &str = "share/licenses/linux-firmware";

/// The Phoenix and Cezanne GPU and WCN6855 files requested at probe, each by
/// its pin and the name the driver asks for.
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
    steps.push(Step::MkDir {
        path: notice("ath11k/WCN6855/hw2.0"),
    });
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
