use crate::types::{Recipe, Step};

// Mozilla's trust store as rendered by curl's published CA extract service.
// This is data only: the recipe copies the pin-verified PEM bytes to a stable
// consumer path and introduces no TLS implementation or system-image wiring.
pub fn recipe() -> Recipe {
    let bundle = "{out}/share/ca-certificates/ca-bundle.crt";
    let steps = vec![
        Step::MkDir {
            path: "{out}/share/ca-certificates".into(),
        },
        Step::run(
            "{root}",
            &[
                "{in:busybox-x86-64}/bin/cp",
                "{in:ca-certificates-source}",
                bundle,
            ],
        ),
        Step::Require {
            paths: vec![bundle.into()],
            exec: false,
        },
    ];

    Recipe::mesboot("ca-certificates", "2026-08-13")
        .source_input("ca-certificates-source")
        .native_inputs(&["busybox-x86-64"])
        .steps(steps)
}
