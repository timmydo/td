use crate::ladder::split_target_debug;
use crate::types::{Recipe, Step};

// Reuses td-authd/ and td-secret/ embeds, including td-firstboot/ and td-busd/.
use super::td_authd as producer;

pub fn recipe() -> Recipe {
    let mut recipe = producer::recipe();
    recipe.name = "td-secret-vm-test".into();
    recipe.checks = None;
    let mut steps = recipe.steps.take().unwrap_or_default();
    steps.retain(|step| !matches!(step, Step::SplitDebugTree { .. }));
    let compiler = steps.iter().find_map(|step| match step {
        Step::Run { argv, env, dir } if argv.iter().any(|arg| arg == "{out}/bin/td-authd") => {
            let argv = argv
                .iter()
                .map(|arg| match arg.as_str() {
                    "{out}/bin/td-authd" => "{out}/bin/secret-vm-init".into(),
                    "{src}/td-authd/src/main.rs" => "{src}/secret-vm.rs".into(),
                    _ => arg.clone(),
                })
                .collect();
            Some(Step::Run {
                argv,
                env: env.clone(),
                dir: dir.clone(),
            })
        }
        _ => None,
    });
    steps.push(Step::WriteFile {
        path: "{src}/secret-vm.rs".into(),
        content: include_str!("../fixtures/secret_vm.rs").into(),
        exec: false,
    });
    if let Some(compiler) = compiler {
        steps.push(compiler);
    }
    steps.push(Step::CopyFile {
        file: "{root}/channel-tests".into(),
        to: "{out}/bin/td-authd-tests".into(),
        exec: true,
    });
    let mut secret_steps = super::td_secret::recipe().steps.unwrap_or_default();
    secret_steps.retain(|step| !matches!(step, Step::SplitDebugTree { .. }));
    steps.extend(secret_steps);
    steps.push(Step::CopyFile {
        file: "{root}/secret-tests".into(),
        to: "{out}/bin/td-secret-tests".into(),
        exec: true,
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/secret-vm-init".into(),
            "{out}/bin/td-authd-tests".into(),
            "{out}/bin/td-secret-tests".into(),
        ],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&[
        "{out}/bin/secret-vm-init",
        "{out}/bin/td-authd-tests",
        "{out}/bin/td-secret-tests",
    ]));
    recipe.steps = Some(steps);
    recipe
}
