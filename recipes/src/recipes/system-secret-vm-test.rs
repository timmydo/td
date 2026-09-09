use crate::types::{Recipe, Step};

const SERVICE: &str = "\n[secret-fixture]\ntype=daemon\nexec=/bin/secret-vm-init --system\nafter=td-firstboot\nrequires=td-firstboot\nready=/bin/td-util test -f /run/td-secret-system-input-ready\nready-timeout=30\nrestart=never\nlog=/run/td-secret-system.log\nconsole=yes\n";

pub fn recipe() -> Recipe {
    let mut recipe = super::system_x86_64::recipe();
    recipe.name = "system-secret-vm-test".into();
    recipe.checks = None;
    recipe.native_inputs.get_or_insert_with(Vec::new).push("td-secret-vm-test".into());
    let mut steps = recipe.steps.take().unwrap_or_default();
    let mut table = false;
    let mut indexed = false;
    let mut result = Vec::new();
    for mut step in steps.drain(..) {
        if let Step::WriteFile { path, content, .. } = &mut step {
            if path == "{root}/real-root/etc/td-svc.conf" {
                // Enumerate the fixture keyboard before stock seat assignment.
                // All other service definitions remain the production table.
                let seat = "[seat]\n";
                if let Some((before, after)) = content.split_once(seat) {
                    if let Some((unit, rest)) = after.split_once("\n\n") {
                        let unit = unit.replace("after=rootcheck\n", "after=rootcheck,secret-fixture\n");
                        *content = format!("{before}{seat}{unit}\n\n{rest}{SERVICE}");
                        table = true;
                    }
                }
            }
        }
        if matches!(&step, Step::Run { argv, .. } if argv.iter().any(|arg| arg == "{root}/real-root/etc/td-profiler-objects.tsv")) {
            result.push(Step::CopyTree {
                from: "{in:td-secret-vm-test}".into(),
                dest: "{root}/real-root{in:td-secret-vm-test}".into(),
            });
            for name in ["secret-vm-init", "td-secret-tests"] {
                result.push(Step::Symlink {
                    target: format!("{{in:td-secret-vm-test}}/bin/{name}"),
                    link: format!("{{root}}/real-root/bin/{name}"),
                });
            }
            result.push(Step::WriteFile {
                path: "{root}/real-root/case".into(),
                content: "fido-system".into(),
                exec: false,
            });
            indexed = true;
        }
        result.push(step);
    }
    if !table || !indexed {
        result = vec![Step::Require {
            paths: vec!["{out}/missing-system-secret-fixture-insertion-point".into()],
            exec: false,
        }];
    }
    recipe.steps = Some(result);
    recipe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_keeps_the_stock_service_table_except_for_input_ordering() {
        fn table(recipe: Recipe) -> String {
            recipe.steps.unwrap().into_iter().find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/real-root/etc/td-svc.conf" => Some(content),
                _ => None,
            }).unwrap()
        }
        let variant = recipe();
        let steps = variant.steps.as_ref().unwrap();
        let copy = steps.iter().position(|step| matches!(step, Step::CopyTree { from, .. } if from == "{in:td-secret-vm-test}")).unwrap();
        let index = steps.iter().position(|step| matches!(step, Step::Run { argv, .. } if argv.iter().any(|arg| arg == "{root}/real-root/etc/td-profiler-objects.tsv"))).unwrap();
        assert!(copy < index);
        let table = table(variant).strip_suffix(SERVICE).unwrap().replace("after=rootcheck,secret-fixture\n", "after=rootcheck\n");
        assert_eq!(table, super::super::system_x86_64::recipe().steps.unwrap().into_iter().find_map(|step| match step {
            Step::WriteFile { path, content, .. } if path == "{root}/real-root/etc/td-svc.conf" => Some(content),
            _ => None,
        }).unwrap());
    }
}
