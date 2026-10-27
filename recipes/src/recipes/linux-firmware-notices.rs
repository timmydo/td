use crate::types::{Recipe, Step};

// The notice and licence of the device firmware linux-x86-64 embeds
// (AGENTS.md "Device firmware"). The image takes only the kernel's bzImage,
// so the notices are their own data recipe, copied into the image's real
// root as the outline face's licences are.

/// Where the notices sit in the output.
pub const DIR: &str = "share/licenses/linux-firmware";

/// Each notice's pin and its path under `DIR`, as linux-firmware lays it out.
pub const NOTICES: &[(&str, &str)] = &[
    (
        "linux-firmware-ath11k-wcn6855-notice-source",
        "ath11k/WCN6855/hw2.0/Notice.txt",
    ),
    (
        "linux-firmware-qualcomm-atheros-license-source",
        "LICENSE.QualcommAtheros_ath10k",
    ),
];

pub fn recipe() -> Recipe {
    let shipped = |path: &str| format!("{{out}}/{DIR}/{path}");
    let mut steps = vec![Step::MkDir {
        path: shipped("ath11k/WCN6855/hw2.0"),
    }];
    steps.extend(NOTICES.iter().map(|(pin, path)| Step::CopyFile {
        file: format!("{{in:{pin}}}"),
        to: shipped(path),
        exec: false,
    }));
    steps.push(Step::Require {
        paths: NOTICES.iter().map(|(_, path)| shipped(path)).collect(),
        exec: false,
    });
    let pins: Vec<&str> = NOTICES.iter().map(|(pin, _)| *pin).collect();
    Recipe::mesboot("linux-firmware-notices", "20261019")
        .inputs(&pins)
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notices_are_unmarked_data_from_the_firmwares_tag() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        assert_eq!(recipe.native_inputs, None);
        let pins: Vec<String> = NOTICES.iter().map(|(pin, _)| (*pin).into()).collect();
        assert_eq!(recipe.inputs, Some(pins));
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

    #[test]
    fn the_image_carries_the_notices_in_its_store() {
        let steps = crate::catalog::all()
            .into_iter()
            .find(|(stem, _)| *stem == "system-x86-64")
            .and_then(|(_, recipe)| recipe.steps)
            .unwrap_or_default();
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::CopyTree { from, dest }
                if from == "{in:linux-firmware-notices}"
                    && dest == "{root}/real-root{in:linux-firmware-notices}"
        )));
    }
}
