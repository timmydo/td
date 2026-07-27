use crate::ladder::{mesboot0_inputs, mesboot0_path, SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Build a populated Btrfs volume in a regular file, verify it offline, and
// restore its contents without a mount or any host filesystem utility.
pub fn recipe() -> Recipe {
    let mkfs = "{in:btrfs-progs-x86-64}/bin/mkfs.btrfs";
    let btrfs = "{in:btrfs-progs-x86-64}/bin/btrfs";
    // Deployment directory names are the SHA-256 of their manifest bytes.
    let mut steps = vec![
        Step::MkDir {
            path: "{root}/seed/td/deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d".into(),
        },
        Step::MkDir {
            path: "{root}/seed/td/boot".into(),
        },
        Step::MkDir {
            path: "{root}/seed/@var/home".into(),
        },
        Step::MkDir {
            path: "{root}/seed/@var/root".into(),
        },
        Step::WriteFile {
            path: "{root}/seed/td/deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d/manifest".into(),
            content: "td-deployment-v1\ntest-payload\n".into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{root}/seed/@var/home/persisted".into(),
            content: "persistent-state\n".into(),
            exec: false,
        },
        Step::Symlink {
            target: "../deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d".into(),
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
    ];
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
        .env("PATH", &mesboot0_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                btrfs,
                "check",
                "--readonly",
                "--check-data-csum",
                "{root}/volume.btrfs",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                "'{in:btrfs-progs-x86-64}/bin/btrfs' inspect-internal dump-tree -t root '{root}/volume.btrfs' > '{root}/root-tree'; \
                 grep -q 'name @var' '{root}/root-tree' || { echo 'root tree has no @var subvolume reference' >&2; exit 1; }; \
                 awk '\
                   /ROOT_REF [0-9]+\\)/ { candidate=$0; sub(/^.*ROOT_REF /, \"\", candidate); sub(/\\).*$/, \"\", candidate); next } \
                   candidate != \"\" && /name @var$/ { target=candidate; next } \
                   target != \"\" && index($0, \"key (\" target \" ROOT_ITEM 0)\") { in_item=1; next } \
                   in_item && /flags / { found=1; if ($0 ~ /RDONLY/) exit 1; exit 0 } \
                   END { if (!found) exit 1 } \
                 ' '{root}/root-tree' || { echo '@var root item is missing or read-only' >&2; exit 1; }; \
                 '{in:btrfs-progs-x86-64}/bin/btrfs' restore -s -S '{root}/volume.btrfs' '{root}/restored'; \
                 cmp '{root}/seed/td/deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d/manifest' '{root}/restored/td/deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d/manifest' || { echo 'deployment manifest did not round-trip' >&2; exit 1; }; \
                 cmp '{root}/seed/@var/home/persisted' '{root}/restored/@var/home/persisted' || { echo 'persistent-state payload did not round-trip through the @var subvolume' >&2; exit 1; }; \
                 [ \"$(readlink '{root}/restored/td/boot/current')\" = ../deployments/9b749e4f4dd9ef26ce57d6c2e5e7120ea3a5e64de4394c80e56369c35791ea9d ] || { echo 'boot selector symlink did not round-trip' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: target-built btrfs-progs creates, populates, checks, and restores a regular-file Btrfs volume with a writable var subvolume without mounting it\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("btrfs-progs-x86-64-test", "1.0")
        .native_inputs(&["btrfs-progs-x86-64"])
        .inputs_owned(mesboot0_inputs(&[]))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check btrfs-progs-x86-64-test: create and populate a Btrfs image with @var, then check and restore it without mounting"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run btrfs-progs-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
