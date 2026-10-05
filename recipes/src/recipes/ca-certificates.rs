use crate::types::{Recipe, Step};

// Mozilla's trust store as rendered by curl's published CA extract service.
// This is data only: the recipe copies the pin-verified PEM bytes to a stable
// consumer path and introduces no TLS implementation or system-image wiring.
// The copy is the engine's own, so the recipe declares no tool at all.
pub fn recipe() -> Recipe {
    let bundle = "{out}/share/ca-certificates/ca-bundle.crt";
    let steps = vec![
        Step::CopyFile {
            file: "{in:ca-certificates-source}".into(),
            to: bundle.into(),
            exec: false,
        },
        Step::Require {
            paths: vec![bundle.into()],
            exec: false,
        },
    ];

    Recipe::mesboot("ca-certificates", "2026-08-13")
        .source_input("ca-certificates-source")
        .steps(steps)
}
