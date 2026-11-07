//! td-ssh-test's real logins (td-ssh/DESIGN.md section 9). A build has no loopback
//! and each `store-ns` has a network namespace of its own, so one driver process
//! in one namespace starts both ends of every connection.

use super::rust_toolchain::path_basename;
use crate::check_runner::{RecipeCheckRunner, TD_STORE_DIR};

const STEM: &str = "td-ssh-test";
const MARKER: &str = "TD-SSH-INTEROP-OK ";
/// Every check the driver has. A leg that stopped running would pass by its exit
/// status alone, so the count is held to the number it should be.
const CHECKS: u32 = 80;

pub(super) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target(STEM)?;
    let build_out = runner.build_plan(STEM)?;
    let in_store = |rung: &str| -> Result<String, String> {
        let output = runner.ladder_out_from(&build_out, rung)?;
        Ok(format!("{TD_STORE_DIR}/{}", path_basename(&output)?))
    };
    let driver = format!("{}/bin/td-ssh-interop", in_store("td-ssh-interop")?);
    let td_ssh = in_store("td-ssh")?;
    let openssh = in_store("openssh-x86-64")?;
    let shell = format!("{}/bin/td-sh", in_store("td-sh")?);
    let report = runner.store_ns_output(
        &[
            &driver,
            "--openssh",
            &openssh,
            "--td-ssh",
            &td_ssh,
            "--shell",
            &shell,
        ],
        None,
    )?;
    let passed = verdict(&report)?;
    println!(
        "PASS: td-ssh-test: {passed} checks between td-ssh and stock OpenSSH, both ways, over the check's loopback"
    );
    Ok(())
}

/// The driver's own exit says most of it. The marker's count refuses a run that
/// left a leg out, which a success alone could not.
fn verdict(report: &str) -> Result<u32, String> {
    let passed = report
        .lines()
        .find_map(|line| line.strip_prefix(MARKER))
        .and_then(|count| count.trim().parse::<u32>().ok())
        .ok_or_else(|| format!("td-ssh-test: no result from the driver:\n{report}"))?;
    if passed != CHECKS {
        return Err(format!(
            "td-ssh-test: the driver passed {passed} checks, not {CHECKS}:\n{report}"
        ));
    }
    Ok(passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_counted_marker_is_a_pass() {
        let counted = format!("ok   x\nTD-SSH-INTEROP-OK {CHECKS}\n");
        assert_eq!(verdict(&counted), Ok(CHECKS));
        assert!(verdict("").is_err());
        assert!(verdict("TD-SSH-INTEROP-OK 0\n").is_err());
        // A leg that stopped running, and one added without the count that says so.
        assert!(verdict(&format!("TD-SSH-INTEROP-OK {}\n", CHECKS - 1)).is_err());
        assert!(verdict(&format!("TD-SSH-INTEROP-OK {}\n", CHECKS + 1)).is_err());
        assert!(verdict("TD-SSH-INTEROP-OK many\n").is_err());
    }
}
