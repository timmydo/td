use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

const GOOD_ID: &str = "ba7dfe039aae6703b7c58003bf32909c9b9df7801d4d18bd72bf3fa8425ecd0b";
const BAD_ID: &str = "0000000000000000000000000000000000000000000000000000000000000000";

// Exercise the target-linked binary against a corrupt current deployment, a
// verified previous deployment, and a subsequently tampered previous payload.
pub fn recipe() -> Recipe {
    let bin = "{in:td-boot}/bin/td-boot";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let good = format!("{{root}}/volume/td/deployments/{GOOD_ID}");
    let bad = format!("{{root}}/volume/td/deployments/{BAD_ID}");
    let mut steps = vec![
        Step::MkDir { path: good.clone() },
        Step::MkDir { path: bad.clone() },
        Step::MkDir {
            path: "{root}/volume/td/boot".into(),
        },
        Step::WriteFile {
            path: format!("{good}/bzImage"),
            content: "kernel-payload\n".into(),
            exec: false,
        },
        Step::WriteFile {
            path: format!("{good}/initramfs.cpio"),
            content: "initramfs-payload\n".into(),
            exec: false,
        },
        Step::WriteFile {
            path: format!("{good}/root.erofs"),
            content: "root-payload\n".into(),
            exec: false,
        },
        Step::WriteFile {
            path: format!("{bad}/manifest"),
            content: "invalid-current\n".into(),
            exec: false,
        },
        Step::Symlink {
            target: format!("../deployments/{BAD_ID}"),
            link: "{root}/volume/td/boot/current".into(),
        },
        Step::Symlink {
            target: format!("../deployments/{GOOD_ID}"),
            link: "{root}/volume/td/boot/previous".into(),
        },
    ];
    // Use the same typed producer as system-x86-64, binding the fixture to the
    // real td-deployment-v1 byte format. GOOD_ID is this manifest's SHA-256.
    steps.push(Step::Sha256Manifest {
        output: format!("{good}/manifest"),
        entries: vec![
            ("bzImage".into(), format!("{good}/bzImage")),
            ("initramfs.cpio".into(), format!("{good}/initramfs.cpio")),
            ("root.erofs".into(), format!("{good}/root.erofs")),
        ],
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "'{bin}' verify '{{root}}/volume' > '{{root}}/selected' 2> '{{root}}/warning' || exit 1; \
                     grep -qx 'previous {GOOD_ID} successful' '{{root}}/selected' || {{ echo 'td-boot did not report the verified previous deployment successful' >&2; exit 1; }}; \
                     grep -q 'current rejected' '{{root}}/warning' || {{ echo 'td-boot did not report current fallback' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(Step::WriteFile {
        path: format!("{good}/root.erofs"),
        content: "tampered-payload\n".into(),
        exec: false,
    });
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "if '{bin}' verify '{{root}}/volume' > '{{root}}/tampered-out' 2> '{{root}}/tampered-error'; then echo 'td-boot accepted a tampered payload' >&2; exit 1; fi; \
                     grep -q 'root.erofs hash mismatch' '{{root}}/tampered-error' || {{ echo 'td-boot did not diagnose the tampered payload' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-boot' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi ELF64 || {{ echo 'td-boot is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi x86-64 || {{ echo 'td-boot is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-boot is not static ET_EXEC' >&2; exit 1; }}; \
                     l=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-boot' >&2; exit 1; }}; \
                     ! printf '%s\\n' \"$l\" | grep -qi INTERP || {{ echo 'td-boot carries PT_INTERP' >&2; exit 1; }}; \
                     d=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-boot' >&2; exit 1; }}; \
                     ! printf '%s\\n' \"$d\" | grep -qi NEEDED || {{ echo 'td-boot has dynamic NEEDED entries' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.extend([
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: target-built static td-boot rejects corrupt current, reports the successful previous deployment, then rejects payload tampering\n".into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ]);

    Recipe::mesboot("td-boot-test", "1.0")
        .native_inputs(&["td-boot", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check td-boot-test: target static boot shim verifies current/previous deployments and rejects tampering"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-boot-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
