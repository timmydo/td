use crate::ladder::{post_rust_inputs, post_rust_tool_farm, POST_RUST_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Build a populated Btrfs volume in a regular file with the target-built
// mkfs.btrfs, check it offline, list it and restore its contents, without a
// mount or any host filesystem utility.
pub fn recipe() -> Recipe {
    let mkfs = "{in:td-btrfs}/bin/mkfs.btrfs";
    let td_btrfs = "{in:td-btrfs}/bin/td-btrfs";
    let path = "{tools}";
    // Deployment directory names are the SHA-256 of their manifest bytes.
    let deployment =
        "td/deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d";
    // Larger than an inline extent: regular data with checksums.
    let payload: String = (0..300_000u32)
        .map(|n| char::from(b'a' + (n * 7 % 26) as u8))
        .collect();
    let mut steps = vec![post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk")];
    for dir in [
        deployment,
        "td/boot",
        "td/incoming",
        "@var/home",
        "@var/root",
    ] {
        steps.push(Step::MkDir {
            path: format!("{{root}}/seed/{dir}"),
        });
    }
    for (file, content) in [
        (
            format!("{deployment}/manifest"),
            "td-deployment-v1\ntest-payload\n".to_string(),
        ),
        (format!("{deployment}/root.erofs"), payload),
        (
            "@var/home/persisted".to_string(),
            "persistent-state\n".to_string(),
        ),
    ] {
        steps.push(Step::WriteFile {
            path: format!("{{root}}/seed/{file}"),
            content,
            exec: false,
        });
    }
    steps.extend([
        Step::Symlink {
            target:
                "../deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d"
                    .into(),
            link: "{root}/seed/td/boot/current".into(),
        },
        Step::WriteFile {
            path: "{root}/volume.btrfs".into(),
            content: String::new(),
            exec: false,
        },
        Step::MkDir {
            path: "{root}/restored".into(),
        },
    ]);
    steps.push(
        Step::run(
            "{root}",
            &[
                mkfs,
                "--rootdir",
                "{root}/seed",
                "--subvol",
                "rw:@var",
                "--byte-count",
                "256M",
                "--uuid",
                "12345678-1234-4234-8234-123456789abc",
                "--label",
                "td-system",
                "{root}/volume.btrfs",
            ],
        )
        .env("PATH", path),
    );
    steps.push(Step::run("{root}", &[td_btrfs, "check", "{root}/volume.btrfs"]).env("PATH", path));
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                &format!(
                    "'{td_btrfs}' list '{{root}}/volume.btrfs' > '{{root}}/listing'; \
                     awk '$1 == \"d\" && $6 == \"/@var\" && $7 == \"[subvolume\" && $8 == \"256\" && $9 == \"rw]\" {{ found = 1 }} END {{ exit !found }}' '{{root}}/listing' || {{ echo '@var is not a writable subvolume' >&2; cat '{{root}}/listing' >&2; exit 1; }}; \
                     awk '$1 == \"d\" && $5 == \"0\" && $6 == \"/td/incoming\" {{ found = 1 }} END {{ exit !found }}' '{{root}}/listing' || {{ echo 'the empty incoming directory did not survive' >&2; exit 1; }}; \
                     [ \"$(head -c 65536 '{{root}}/volume.btrfs' | tr -d '\\000' | wc -c)\" = 0 ] || {{ echo 'the boot area is not clear' >&2; exit 1; }}; \
                     '{td_btrfs}' restore '{{root}}/volume.btrfs' '{{root}}/restored'; \
                     for f in {deployment}/manifest {deployment}/root.erofs @var/home/persisted; do cmp \"{{root}}/seed/$f\" \"{{root}}/restored/$f\" || {{ echo \"$f did not round-trip\" >&2; exit 1; }}; done; \
                     [ -d '{{root}}/restored/@var/root' ] || {{ echo 'the empty @var/root directory did not survive' >&2; exit 1; }}; \
                     [ \"$(readlink '{{root}}/restored/td/boot/current')\" = ../deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d ] || {{ echo 'boot selector symlink did not round-trip' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", path),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: target-built td-btrfs creates, checks, lists and restores a regular-file Btrfs volume with a writable var subvolume without mounting it\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-btrfs-test", "1.0")
        .native_inputs(&post_rust_inputs("gawk-x86-64-self", &["td-btrfs"]))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-btrfs-test: create a Btrfs image with @var, then check, list and restore it without mounting"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-btrfs-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
