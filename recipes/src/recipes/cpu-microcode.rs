use crate::shared::{BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH};
use crate::types::{Recipe, Step};

// The vendors' CPU microcode (AGENTS.md "CPU microcode"), one file per vendor at
// the names the kernel's early loader looks for in an initramfs. The CPU checks
// each vendor's signature; td only carries the bytes.

/// The early loader's directory, inside the initramfs and this output.
pub const DIR: &str = "kernel/x86/microcode";
/// What the loader asks for on each vendor's CPU.
pub const FILES: &[&str] = &["GenuineIntel.bin", "AuthenticAMD.bin"];
/// The licences, the only part of this output the root carries.
pub const LICENSES: &str = "share/licenses/cpu-microcode";

/// AMD's containers, oldest family first.
const AMD: &[&str] = &[
    "linux-firmware-amd-ucode-source",
    "linux-firmware-amd-ucode-fam15h-source",
    "linux-firmware-amd-ucode-fam16h-source",
    "linux-firmware-amd-ucode-fam17h-source",
    "linux-firmware-amd-ucode-fam19h-source",
    "linux-firmware-amd-ucode-fam1ah-source",
];

pub fn recipe() -> Recipe {
    let amd = AMD
        .iter()
        .map(|pin| format!("'{{in:{pin}}}'"))
        .collect::<Vec<_>>()
        .join(" ");
    // The sort is the C locale's so the bytes do not depend on the build's.
    let script = format!(
        "set -e; export LC_ALL=C; \
         mkdir -p '{{out}}/{DIR}'; \
         cat '{{root}}/intel/intel-ucode'/* > '{{out}}/{DIR}/GenuineIntel.bin'; \
         cat {amd} > '{{out}}/{DIR}/AuthenticAMD.bin'; \
         [ \"$(wc -c < '{{out}}/{DIR}/GenuineIntel.bin')\" -gt 1000000 ]; \
         [ \"$(wc -c < '{{out}}/{DIR}/AuthenticAMD.bin')\" -gt 100000 ]"
    );
    let steps = vec![
        Step::Unpack {
            input: "{in:intel-microcode-source}".into(),
            dest: "{root}/intel".into(),
            keep_top: false,
        },
        Step::MkDir {
            path: format!("{{out}}/{LICENSES}"),
        },
        Step::run("{root}", &[SH, "-c", &script]).env("PATH", PATH),
        Step::CopyFile {
            file: "{root}/intel/license".into(),
            to: format!("{{out}}/{LICENSES}/intel"),
            exec: false,
        },
        Step::CopyFile {
            file: "{in:linux-firmware-amd-ucode-license-source}".into(),
            to: format!("{{out}}/{LICENSES}/LICENSE.amd-ucode"),
            exec: false,
        },
        Step::Require {
            paths: vec![
                format!("{{out}}/{DIR}/GenuineIntel.bin"),
                format!("{{out}}/{DIR}/AuthenticAMD.bin"),
                format!("{{out}}/{LICENSES}/intel"),
                format!("{{out}}/{LICENSES}/LICENSE.amd-ucode"),
            ],
            exec: false,
        },
    ];
    let mut pins = vec!["intel-microcode-source"];
    pins.extend(AMD);
    pins.push("linux-firmware-amd-ucode-license-source");
    Recipe::mesboot("cpu-microcode", "20260925")
        .build_root()
        .inputs(&pins)
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_holds_exactly_the_microcode_roster_unmarked() {
        let recipe = recipe();
        assert!(!recipe.is_foreign());
        let inputs = recipe.inputs.clone().unwrap_or_default();
        for key in crate::source_pins::CPU_MICROCODE {
            assert!(inputs.iter().any(|input| input == key), "{key}");
        }
        assert_eq!(inputs.len(), crate::source_pins::CPU_MICROCODE.len() + 1);
    }

    #[test]
    fn only_this_recipe_names_microcode() {
        for (stem, recipe) in crate::catalog::all() {
            let named = recipe
                .inputs
                .iter()
                .chain(recipe.native_inputs.iter())
                .chain(recipe.payload_inputs.iter())
                .flatten()
                .chain(recipe.source_input.iter());
            for key in named {
                if crate::source_pins::CPU_MICROCODE.contains(&key.as_str()) {
                    assert_eq!(stem, "cpu-microcode", "{stem} names {key}");
                }
            }
        }
    }

    /// The with-caveats tree is a separate opt-in upstream, and an old-format or
    /// mixed tree would be a silent change to what the CPU is offered.
    #[test]
    fn only_intel_ucode_is_concatenated() {
        let steps = recipe().steps.unwrap_or_default();
        let script = steps
            .iter()
            .find_map(|step| match step {
                Step::Run { argv, .. } => argv.get(2),
                _ => None,
            })
            .unwrap_or_else(|| unreachable!("no script"));
        assert!(script.contains("'{root}/intel/intel-ucode'/*"));
        assert!(!script.contains("caveats"));
        assert!(script.contains("LC_ALL=C"));
    }
}
