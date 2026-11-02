use crate::types::{Recipe, Step};

/// The stock session starts mail and news only on an oracle's boot, which
/// this one is not (it carries no autotest token): the fixture restores the
/// live-only gate on them and on the workspace switches around them, so its
/// boot runs them as its credential clients (td-secret/DESIGN.md).
const BOOT_STARTED: &[&str] = &["applications-workspace", "mail", "news", "shell-workspace"];
const ON_DEMAND_GATE: &str = "unless-exists=/run/td-applications-on-demand\n";
const LIVE_GATE: &str = "unless-exists=/run/td-media\n";

/// `table` with `unit`'s gate replaced, or `None` when the unit or its gate
/// is not where the production table puts it.
fn regate(table: &str, unit: &str, from: &str, to: &str) -> Option<String> {
    let header = format!("[{unit}]\n");
    let (before, after) = table.split_once(&header)?;
    let (body, rest) = after.split_once("\n\n")?;
    if body.matches(from).count() != 1 {
        return None;
    }
    Some(format!(
        "{before}{header}{}\n\n{rest}",
        body.replacen(from, to, 1)
    ))
}

const SERVICE: &str = "\n[secret-fixture]\ntype=daemon\nexec=/bin/secret-vm-init --system\nafter=td-firstboot\nrequires=td-firstboot\nready=/bin/td-util test -f /run/td-secret-system-input-ready\nready-timeout=120\nrestart=never\nlog=/run/td-secret-system.log\nconsole=yes\n";

pub fn recipe() -> Recipe {
    let mut recipe = super::system_x86_64::recipe();
    recipe.name = "system-secret-vm-test".into();
    recipe.checks = None;
    recipe
        .native_inputs
        .get_or_insert_with(Vec::new)
        .push("td-secret-vm-test".into());
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
                        let unit =
                            unit.replace("after=rootcheck\n", "after=rootcheck,secret-fixture\n");
                        let mut patched = Some(format!("{before}{seat}{unit}\n\n{rest}{SERVICE}"));
                        for name in BOOT_STARTED {
                            patched = patched
                                .and_then(|text| regate(&text, name, ON_DEMAND_GATE, LIVE_GATE));
                        }
                        if let Some(patched) = patched {
                            *content = patched;
                            table = true;
                        }
                    }
                }
            }
        }
        if matches!(&step, Step::Run { argv, .. } if argv.iter().any(|arg| arg == "{root}/real-root/etc/td-profiler-objects.tsv"))
        {
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
    fn fixture_keeps_the_stock_service_table_except_for_input_ordering_and_boot_start() {
        fn table(recipe: Recipe) -> String {
            recipe
                .steps
                .unwrap()
                .into_iter()
                .find_map(|step| match step {
                    Step::WriteFile { path, content, .. }
                        if path == "{root}/real-root/etc/td-svc.conf" =>
                    {
                        Some(content)
                    }
                    _ => None,
                })
                .unwrap()
        }
        let variant = recipe();
        let steps = variant.steps.as_ref().unwrap();
        let copy = steps.iter().position(|step| matches!(step, Step::CopyTree { from, .. } if from == "{in:td-secret-vm-test}")).unwrap();
        let index = steps.iter().position(|step| matches!(step, Step::Run { argv, .. } if argv.iter().any(|arg| arg == "{root}/real-root/etc/td-profiler-objects.tsv"))).unwrap();
        assert!(copy < index);
        let mut table = table(variant)
            .strip_suffix(SERVICE)
            .unwrap()
            .replace("after=rootcheck,secret-fixture\n", "after=rootcheck\n");
        for name in BOOT_STARTED {
            let header = format!("[{name}]\n");
            let body = table.split_once(&header).unwrap().1;
            let body = body.split_once("\n\n").unwrap().0;
            assert!(
                body.contains(LIVE_GATE) && !body.contains(ON_DEMAND_GATE),
                "{name}"
            );
            table = regate(&table, name, LIVE_GATE, ON_DEMAND_GATE).unwrap();
        }
        assert_eq!(
            table,
            super::super::system_x86_64::recipe()
                .steps
                .unwrap()
                .into_iter()
                .find_map(|step| match step {
                    Step::WriteFile { path, content, .. }
                        if path == "{root}/real-root/etc/td-svc.conf" =>
                        Some(content),
                    _ => None,
                })
                .unwrap()
        );
    }
}
