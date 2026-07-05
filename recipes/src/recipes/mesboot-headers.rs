use crate::ladder::unpack_keep_top;
use crate::types::{Recipe, Step};

// mesboot-headers — bootstrap rung 8 prerequisite (#378, guix's mesboot-headers):
// the host-produced pure Linux UAPI headers (td-feed warm sources, interned as
// this rung's source) merged with the mes includes. Pure placement, no build.
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
        .native_inputs(&["mes"])
        .inputs(&[
            "bash",
            "coreutils",
            "sed",
            "grep",
            "gawk",
            "tar",
            "gzip",
            "bzip2",
            "xz",
            "findutils",
            "diffutils",
        ])
        .steps(steps)
}
