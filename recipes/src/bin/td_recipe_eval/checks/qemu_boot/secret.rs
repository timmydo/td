use super::*;
use td_engine::cpio::{self, Entry, Kind};

#[path = "../../../../fixtures/secret_vm.rs"]
#[allow(dead_code)]
mod fixture;

pub(crate) const TARGETS: &[&str] = &["linux-x86-64", "td-secret-vm-test", "td-secret", "td-init"];

fn output(runner: &RecipeCheckRunner, name: &str) -> Result<PathBuf, String> {
    runner.prepare_recipe_target(name)?;
    let log = runner.build_plan(name)?;
    runner.ladder_out_from(&log, name)
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (kernel, base) = build_kernel(runner)?;
    let tests = output(runner, "td-secret-vm-test")?;
    let secret = output(runner, "td-secret")?;
    let init = output(runner, "td-init")?;
    let files = [
        ("init", tests.join("bin/secret-vm-init")),
        ("bin/td-authd-tests", tests.join("bin/td-authd-tests")),
        ("bin/td-secret", secret.join("bin/td-secret")),
        ("bin/td-init", init.join("bin/td-init")),
    ];
    let mut contents = Vec::new();
    for (name, path) in &files {
        contents.push((
            *name,
            fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?,
        ));
    }
    let base = fs::read(&base).map_err(|e| format!("read base initramfs: {e}"))?;
    if base.len() % 4 != 0 {
        return Err("base initramfs is not four-byte aligned".into());
    }
    for (name, test) in fixture::CASES {
        let mut entries: Vec<Entry<'_>> = contents
            .iter()
            .map(|(name, bytes)| Entry {
                name,
                mode: 0o755,
                kind: Kind::File(bytes),
            })
            .collect();
        entries.push(Entry {
            name: "case",
            mode: 0o444,
            kind: Kind::File(name.as_bytes()),
        });
        let archive = runner.scratch_dir().join(format!("secret-{name}.cpio"));
        let mut file =
            File::create(&archive).map_err(|e| format!("create fixture archive: {e}"))?;
        file.write_all(&base)
            .and_then(|_| file.write_all(&cpio::build(&entries).map_err(std::io::Error::other)?))
            .map_err(|e| format!("write fixture archive: {e}"))?;
        drop(file);
        println!("[qemu-secret] {name}: {test}");
        let result = boot_with_timeout(
            &qemu,
            &kernel,
            &archive,
            BootPlan {
                disk: None,
                mem: "512",
                target_marker: fixture::PASS,
                kill_on_marker: false,
                extra_append: "td.operation-fixture=1 td.write-intake-fixture=1",
                user_net: false,
                audio: false,
                physical_input: false,
                capture_firefox_audio: false,
            },
            runner.scratch_dir(),
            Duration::from_secs(180),
        )?;
        if !result.evidence.target
            || !result.exited_clean
            || result.evidence.kernel_panic
            || result
                .console
                .lines()
                .any(|line| line.starts_with(fixture::FAIL))
        {
            return Err(format!(
                "secret VM {name} failed: {}\n{}",
                result.reason,
                tail(&result.console, 80)
            ));
        }
        println!(
            "[qemu-secret] {name} passed in {:.2}s",
            result.elapsed.as_secs_f64()
        );
    }
    println!("PASS: secret authority VM cases ({} fresh guests); credential intake, inspection and relocking; no token or TPM release claim", fixture::CASES.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_vm_requires_a_real_single_test_pass() {
        let pass =
            "test result: ok. 1 passed; 0 failed; 0 ignored; 42 filtered out; finished in 0.01s";
        assert!(fixture::test_passed(true, pass));
        assert!(!fixture::test_passed(false, pass));
        for bad in [
            "",
            "test result: ok. 0 passed; 0 failed; 0 ignored;",
            "test result: ok. 0 passed; 0 failed; 1 ignored;",
            "test result: FAILED. 0 passed; 1 failed; 0 ignored;",
        ] {
            assert!(!fixture::test_passed(true, bad));
        }
        assert!(!fixture::test_passed(
            true,
            &format!("{pass}\ntest result: ok. 0 passed; 0 failed; 0 ignored;")
        ));
    }

    #[test]
    fn secret_vm_roster_names_only_explicit_root_oracles() {
        let source = [
            include_str!("../../../../../../td-authd/tests/secret_intake.rs"),
            include_str!("../../../../../../td-authd/tests/session.rs"),
            include_str!("../../../../../../td-authd/tests/unlock.rs"),
        ]
        .join("\n");
        let names: std::collections::BTreeSet<_> =
            fixture::CASES.iter().map(|(name, _)| name).collect();
        assert_eq!(names.len(), 4);
        for (_, test) in fixture::CASES {
            let name = test.rsplit("::").next().unwrap();
            assert!(name.starts_with("root_"));
            assert!(source.contains(&format!("fn {name}()")));
        }
        let recipe = td_recipe::catalog::lookup("td-secret-vm-test").unwrap();
        assert!(recipe.checks.is_none());
        let readers = td_recipe::catalog::named_dirs("td-secret-vm-test");
        for source in ["td-authd", "td-firstboot"] {
            assert!(readers.contains(&source));
        }
        let system = crate::check_runner::recipe_closure(&["system-x86-64"]).unwrap();
        // Test executables are deliberately absent from the system closure.
        assert!(!system.iter().any(|node| node.stem == "td-secret-vm-test"));
    }
}
