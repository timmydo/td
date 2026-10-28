use crate::shared::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// The same static program serves the selector, deployment and installed root.
const SOURCES: &[(&str, &str)] = &[
    (
        "{src}/td-fido/src/crypto.rs",
        include_str!("../../../td-fido/src/crypto.rs"),
    ),
    (
        "{src}/td-fido/src/fido_aes.rs",
        include_str!("../../../td-fido/src/fido_aes.rs"),
    ),
    (
        "{src}/td-fido/src/fido_cbor.rs",
        include_str!("../../../td-fido/src/fido_cbor.rs"),
    ),
    (
        "{src}/td-fido/src/fido_ctap.rs",
        include_str!("../../../td-fido/src/fido_ctap.rs"),
    ),
    (
        "{src}/td-fido/src/fido_device.rs",
        include_str!("../../../td-fido/src/fido_device.rs"),
    ),
    (
        "{src}/td-fido/src/fido_disk.rs",
        include_str!("../../../td-fido/src/fido_disk.rs"),
    ),
    (
        "{src}/td-fido/src/fido_enroll.rs",
        include_str!("../../../td-fido/src/fido_enroll.rs"),
    ),
    (
        "{src}/td-fido/src/fido_fixtures.rs",
        include_str!("../../../td-fido/src/fido_fixtures.rs"),
    ),
    (
        "{src}/td-fido/src/fido_hid.rs",
        include_str!("../../../td-fido/src/fido_hid.rs"),
    ),
    (
        "{src}/td-fido/src/fido_p256.rs",
        include_str!("../../../td-fido/src/fido_p256.rs"),
    ),
    (
        "{src}/td-fido/src/fido_pin.rs",
        include_str!("../../../td-fido/src/fido_pin.rs"),
    ),
    (
        "{src}/td-fido/src/fido_transaction.rs",
        include_str!("../../../td-fido/src/fido_transaction.rs"),
    ),
    (
        "{src}/td-fido/src/fido_virtual.rs",
        include_str!("../../../td-fido/src/fido_virtual.rs"),
    ),
    (
        "{src}/td-fido/src/fido_virtual_tests.rs",
        include_str!("../../../td-fido/src/fido_virtual_tests.rs"),
    ),
    (
        "{src}/td-fido/src/hmac.rs",
        include_str!("../../../td-fido/src/hmac.rs"),
    ),
    (
        "{src}/td-fido/src/lib.rs",
        include_str!("../../../td-fido/src/lib.rs"),
    ),
    (
        "{src}/td-fido/src/root.rs",
        include_str!("../../../td-fido/src/root.rs"),
    ),
    (
        "{src}/td-json/src/arena.rs",
        include_str!("../../../td-json/src/arena.rs"),
    ),
    (
        "{src}/td-json/src/arena_emit.rs",
        include_str!("../../../td-json/src/arena_emit.rs"),
    ),
    (
        "{src}/td-json/src/lib.rs",
        include_str!("../../../td-json/src/lib.rs"),
    ),
    (
        "{src}/td-json/src/retain.rs",
        include_str!("../../../td-json/src/retain.rs"),
    ),
    (
        "{src}/td-json/src/scan.rs",
        include_str!("../../../td-json/src/scan.rs"),
    ),
    (
        "{src}/td-json/src/string.rs",
        include_str!("../../../td-json/src/string.rs"),
    ),
    (
        "{src}/td-json/src/string_array.rs",
        include_str!("../../../td-json/src/string_array.rs"),
    ),
    (
        "{src}/td-luks/bin/dm.rs",
        include_str!("../../../td-luks/bin/dm.rs"),
    ),
    (
        "{src}/td-luks/bin/dm_sys.rs",
        include_str!("../../../td-luks/bin/dm_sys.rs"),
    ),
    (
        "{src}/td-luks/bin/lock.rs",
        include_str!("../../../td-luks/bin/lock.rs"),
    ),
    (
        "{src}/td-luks/bin/main.rs",
        include_str!("../../../td-luks/bin/main.rs"),
    ),
    (
        "{src}/td-luks/bin/reencrypt.rs",
        include_str!("../../../td-luks/bin/reencrypt.rs"),
    ),
    (
        "{src}/td-luks/bin/reencrypt_io.rs",
        include_str!("../../../td-luks/bin/reencrypt_io.rs"),
    ),
    (
        "{src}/td-luks/bin/reencrypt_stack.rs",
        include_str!("../../../td-luks/bin/reencrypt_stack.rs"),
    ),
    (
        "{src}/td-luks/src/header.rs",
        include_str!("../../../td-luks/src/header.rs"),
    ),
    (
        "{src}/td-luks/src/keyslot.rs",
        include_str!("../../../td-luks/src/keyslot.rs"),
    ),
    (
        "{src}/td-luks/src/lib.rs",
        include_str!("../../../td-luks/src/lib.rs"),
    ),
    (
        "{src}/td-luks/src/reencrypt.rs",
        include_str!("../../../td-luks/src/reencrypt.rs"),
    ),
    (
        "{src}/td-luks/src/reencrypt_metadata.rs",
        include_str!("../../../td-luks/src/reencrypt_metadata.rs"),
    ),
    (
        "{src}/td-luks/src/reencrypt_profile.rs",
        include_str!("../../../td-luks/src/reencrypt_profile.rs"),
    ),
    (
        "{src}/td-luks/src/reencrypt_transition.rs",
        include_str!("../../../td-luks/src/reencrypt_transition.rs"),
    ),
    (
        "{src}/td-luks/src/write.rs",
        include_str!("../../../td-luks/src/write.rs"),
    ),
    (
        "{src}/td-encoding/src/lib.rs",
        include_str!("../../../td-encoding/src/lib.rs"),
    ),
    (
        "{src}/engine/src/sha256.rs",
        include_str!("../../../engine/src/sha256.rs"),
    ),
];

pub fn recipe() -> Recipe {
    let mut steps = Vec::new();
    for path in [
        "{out}/bin",
        "{src}/td-luks/src",
        "{src}/td-luks/bin",
        "{src}/td-fido/src",
        "{src}/td-json/src",
        "{src}/td-encoding/src",
        "{src}/engine/src",
    ] {
        steps.push(Step::MkDir { path: path.into() });
    }
    for (path, content) in SOURCES {
        steps.push(Step::WriteFile {
            path: (*path).into(),
            content: (*content).into(),
            exec: false,
        });
    }
    let profile = [
        "--edition",
        "2021",
        "-C",
        "opt-level=3",
        "--target",
        "x86_64-unknown-linux-gnu",
        "-C",
        "target-feature=+crt-static",
        "-C",
        "relocation-model=static",
        "-C",
        "panic=abort",
        "-L",
        "dependency={root}",
    ];
    for (name, source, output, dependencies) in [
        (
            "td_fido",
            "{src}/td-fido/src/lib.rs",
            "{root}/libtd_fido.rlib",
            &[][..],
        ),
        (
            "td_json",
            "{src}/td-json/src/lib.rs",
            "{root}/libtd_json.rlib",
            &[][..],
        ),
        (
            "td_luks",
            "{src}/td-luks/src/lib.rs",
            "{root}/libtd_luks.rlib",
            &[
                "--extern",
                "td_fido={root}/libtd_fido.rlib",
                "--extern",
                "td_json={root}/libtd_json.rlib",
            ][..],
        ),
        (
            "td_luks_bin",
            "{src}/td-luks/bin/main.rs",
            "{out}/bin/td-luks",
            &[
                "--extern",
                "td_luks={root}/libtd_luks.rlib",
                "--extern",
                "td_fido={root}/libtd_fido.rlib",
                "--extern",
                "td_json={root}/libtd_json.rlib",
            ][..],
        ),
    ] {
        let mut args = profile.to_vec();
        if name != "td_luks_bin" {
            args.extend(["--crate-type", "rlib"]);
        }
        args.extend(["--crate-name", name, "-o", output]);
        args.extend(dependencies);
        args.push(source);
        steps.push(
            target_rustc("{src}", "{in:rust-toolchain}/bin/rustc", &args)
                .env("SOURCE_DATE_EPOCH", "1"),
        );
    }
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-luks".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-luks"]));
    Recipe::mesboot("td-luks", "0.1")
        .build_root()
        .native_inputs(&["rust-toolchain", "td-link", "glibc-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_crate_source_is_staged() {
        crate::shared::assert_sibling_sources_staged(
            super::recipe(),
            &["td-luks", "td-fido", "td-json"],
        );
    }
}
