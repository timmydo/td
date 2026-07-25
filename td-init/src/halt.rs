//! `reboot`, `poweroff`, `halt` — the three `reboot(2)` commands.
//!
//! busybox's versions default to SIGNALLING init and need `-f`. td's init has
//! no signal handlers, so the direct syscall IS the behaviour here and `-f` is
//! accepted as the no-op it has become.
//!
//! `sync(2)` runs first unless `-n`: `/var` is a writable Btrfs volume and the
//! kernel's shutdown path is not obliged to flush it. `halt -p` powers off
//! rather than halting, so it is parsed into the command, not a boolean.

use crate::sys;

pub fn reboot(args: &[String]) -> Result<u8, String> {
    run("reboot", sys::REBOOT_RESTART, args)
}

pub fn poweroff(args: &[String]) -> Result<u8, String> {
    run("poweroff", sys::REBOOT_POWER_OFF, args)
}

pub fn halt(args: &[String]) -> Result<u8, String> {
    run("halt", sys::REBOOT_HALT, args)
}

fn usage(name: &str, allow_poweroff: bool) -> String {
    let mut text = format!(
        "usage: {name} [-f] [-n]{}\n  -f  force (accepted; td always issues reboot(2) directly)\n  -n  do not sync(2) first",
        if allow_poweroff { " [-p]" } else { "" }
    );
    if allow_poweroff {
        text.push_str("\n  -p  power off rather than halt");
    }
    text
}

/// What the parsed options say to do: sync first, and which `reboot(2)` command
/// to issue (only `-p` changes the latter).
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    do_sync: bool,
    cmd: usize,
}

/// Options are parsed BEFORE anything irreversible happens, so a typo exits 1
/// instead of powering the machine off.
fn parse(name: &str, cmd: usize, args: &[String]) -> Result<Plan, String> {
    let mut plan = Plan {
        do_sync: true,
        cmd,
    };
    // `-p` means "power off INSTEAD of halting", so it is meaningful only where
    // halting is the default. `reboot -p` on a headless machine would mean
    // "never comes back" rather than "reboots".
    let allow_poweroff = cmd == sys::REBOOT_HALT;
    for a in args {
        match a.as_str() {
            "--force" => {}
            "--no-sync" => plan.do_sync = false,
            "--poweroff" if allow_poweroff => plan.cmd = sys::REBOOT_POWER_OFF,
            // Short flags may be clustered: `halt -np` is the busybox and
            // sysvinit spelling of "power off without syncing". An unknown
            // letter rejects the WHOLE argument, so a typo still never reaches
            // reboot(2).
            other if other.starts_with('-') && !other.starts_with("--") && other.len() > 1 => {
                for c in other.chars().skip(1) {
                    match c {
                        'f' => {}
                        'n' => plan.do_sync = false,
                        'p' if allow_poweroff => plan.cmd = sys::REBOOT_POWER_OFF,
                        _ => {
                            return Err(format!(
                                "unrecognised option '-{c}' in '{other}'\n{}",
                                usage(name, allow_poweroff)
                            ))
                        }
                    }
                }
            }
            other => {
                return Err(format!(
                    "unrecognised argument '{other}'\n{}",
                    usage(name, allow_poweroff)
                ))
            }
        }
    }
    Ok(plan)
}

fn run(name: &str, cmd: usize, args: &[String]) -> Result<u8, String> {
    let Plan { do_sync, cmd } = parse(name, cmd, args)?;
    if do_sync {
        sys::sync();
    }
    match sys::reboot(cmd) {
        // reboot(2) does not return on success; reaching here at all is a fault.
        Ok(()) => Err("the kernel returned from reboot(2)".to_string()),
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    fn plan(name: &str, cmd: usize, xs: &[&str]) -> Result<Plan, String> {
        parse(name, cmd, &argv(xs))
    }

    #[test]
    fn no_arguments_means_sync_then_the_applet_s_own_command() {
        assert_eq!(
            plan("reboot", sys::REBOOT_RESTART, &[]),
            Ok(Plan {
                do_sync: true,
                cmd: sys::REBOOT_RESTART
            })
        );
    }

    #[test]
    fn dash_n_suppresses_the_sync_and_dash_f_is_a_no_op() {
        let halt = |xs: &[&str]| plan("halt", sys::REBOOT_HALT, xs).map(|p| p.do_sync);
        assert_eq!(halt(&["-n"]), Ok(false));
        assert_eq!(halt(&["--no-sync"]), Ok(false));
        assert_eq!(halt(&["-f"]), Ok(true));
        assert_eq!(halt(&["-f", "-n"]), Ok(false));
    }

    /// `halt -p` is a power-off, not a halt — the one option that changes which
    /// command reaches the kernel. It must not change the OTHER applets' default.
    #[test]
    fn dash_p_turns_halt_into_a_power_off() {
        assert_eq!(
            plan("halt", sys::REBOOT_HALT, &["-p"]).map(|p| p.cmd),
            Ok(sys::REBOOT_POWER_OFF)
        );
        assert_eq!(
            plan("halt", sys::REBOOT_HALT, &[]).map(|p| p.cmd),
            Ok(sys::REBOOT_HALT)
        );
        assert_eq!(
            plan("reboot", sys::REBOOT_RESTART, &[]).map(|p| p.cmd),
            Ok(sys::REBOOT_RESTART)
        );
    }

    /// `halt -np` is how busybox and sysvinit spell "power off without a sync",
    /// so the clustered form has to mean the same as the separated one.
    #[test]
    fn clustered_short_flags_mean_what_the_separated_ones_do() {
        assert_eq!(
            plan("halt", sys::REBOOT_HALT, &["-np"]),
            Ok(Plan {
                do_sync: false,
                cmd: sys::REBOOT_POWER_OFF
            })
        );
        assert_eq!(
            plan("halt", sys::REBOOT_HALT, &["-np"]),
            plan("halt", sys::REBOOT_HALT, &["-n", "-p"])
        );
        assert_eq!(
            plan("reboot", sys::REBOOT_RESTART, &["-fn"]),
            Ok(Plan {
                do_sync: false,
                cmd: sys::REBOOT_RESTART
            })
        );
        // One bad letter rejects the whole cluster rather than applying the
        // good ones and powering the machine off anyway.
        assert!(plan("halt", sys::REBOOT_HALT, &["-nx"]).is_err());
        assert!(plan("halt", sys::REBOOT_HALT, &["-xp"]).is_err());
    }

    /// `-p` belongs to `halt` alone. On a headless machine `reboot -p` accepted
    /// as a power-off means "never comes back" rather than "reboots", so the
    /// other two applets must reject it — clustered as well as separate.
    #[test]
    fn only_halt_understands_dash_p() {
        for (name, cmd) in [
            ("reboot", sys::REBOOT_RESTART),
            ("poweroff", sys::REBOOT_POWER_OFF),
        ] {
            assert!(plan(name, cmd, &["-p"]).is_err(), "{name} -p");
            assert!(plan(name, cmd, &["--poweroff"]).is_err(), "{name} --poweroff");
            assert!(plan(name, cmd, &["-fp"]).is_err(), "{name} -fp");
        }
        // The usage text only advertises it where it works.
        assert!(usage("halt", true).contains("-p"));
        assert!(!usage("reboot", false).contains("-p"));
    }

    /// The discriminating case: an unknown option must NOT fall through to the
    /// syscall. A `poweroff -x` that powered the machine off anyway would be a
    /// data-loss bug, so parsing is a separate, testable step.
    #[test]
    fn an_unknown_argument_is_rejected_before_any_syscall() {
        let off = |xs: &[&str]| plan("poweroff", sys::REBOOT_POWER_OFF, xs);
        assert!(off(&["-x"]).is_err());
        assert!(off(&["--now"]).is_err());
        assert!(off(&["5"]).is_err());
        assert!(off(&["-n", "-x"]).is_err());
    }
}
