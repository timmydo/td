use crate::ladder::unpack_keep_top;
use crate::types::{Recipe, Step};

// mesboot-headers — bootstrap rung 8 prerequisite (#378, guix's mesboot-headers):
// the host-produced pure Linux UAPI headers (td-feed warm sources, interned as
// this rung's source) merged with the mes includes. Pure placement, no build.
//
// Host-tool-free (re #469): every step is engine-native (Unpack, CopyTree,
// Require) — the rung shells out to nothing — so it declares NO BASE_TOOLS. The
// prior `base_inputs(&[])` staged bash/coreutils/sed/grep/gawk/findutils/diffutils
// that no step referenced: dead host-executable ingress, now dropped. Inputs are
// exactly the pinned header source plus the td-built `mes` include tree.
pub fn recipe() -> Recipe {
    let mut steps = unpack_keep_top("mesboot-headers-source", "{out}/include");
    steps.push(Step::CopyTree {
        from: "{in:mes}/include".into(),
        dest: "{out}/include".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/include/linux/version.h".into(),
            "{out}/include/asm/unistd.h".into(),
        ],
        exec: false,
    });
    Recipe::mesboot("mesboot-headers", "4.14.67")
        .source_input("linux-headers")
        .native_inputs(&["mes"])
        .steps(steps)
}
