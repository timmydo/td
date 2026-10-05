//! The qemu accelerator the evaluator's boots choose, shared by the
//! interactive runner (`run`), the headless system oracles (`qemu_boot`) and
//! the ISO launcher test (`qemu_boot::test_iso`), so they cannot drift apart
//! in what they probe or how they explain it. An operator's launch (`run`,
//! `test-iso`) takes KVM when this host can hand it to qemu with TCG behind
//! it; a check's boot takes KVM alone and is a host gap without it
//! (`headless_from_env`). `TD_QEMU_ACCEL` pins either. `qemu-update` is told
//! its accelerator by `--accel` instead.
use std::ffi::OsStr;
use std::fs::OpenOptions;

use crate::check_runner::HOST_GAP;

/// Operator override for the accelerator choice: `kvm` or `tcg`. Anything else is an
/// error rather than a silent fall-through, so a typo cannot quietly re-slow a boot.
pub(crate) const ACCEL_ENV: &str = "TD_QEMU_ACCEL";

/// What this run tells qemu to accelerate with, and how the banner describes it.
#[derive(Debug)]
pub(crate) struct AccelPlan {
    /// `-accel` names in qemu's preference order.
    pub(crate) names: &'static [&'static str],
    /// What the boot banner calls the choice.
    pub(crate) label: &'static str,
    /// Why this boot is software-emulated, when it is — the operator's cue that the
    /// slowness is fixable, and what would fix it. `None` once KVM is in play.
    pub(crate) hint: Option<&'static str>,
    /// Set when `ACCEL_ENV` chose this rather than the probe, so a qemu that then fails
    /// to start can say the override is why nothing fell back to TCG.
    pub(crate) forced: bool,
}

/// Wrapped to the banner's continuation indent: these print among hand-wrapped lines,
/// and a single 300-column paragraph in the middle of them wraps raggedly.
const TCG_NO_NODE_HINT: &str = "This host has no usable /dev/kvm, so the guest is emulated\n         \
     instruction-by-instruction and boots several times slower. Where the host does have\n         \
     KVM, giving this user read/write access to /dev/kvm (usually membership in the `kvm`\n         \
     group) is what makes the line above say KVM.";

/// The other reason KVM is unavailable needs its OWN advice: telling an operator whose
/// host is not x86_64 to get at /dev/kvm sends them after access that changes nothing,
/// and they may well have it already.
const TCG_WRONG_ARCH_HINT: &str =
    "KVM accelerates only a guest of the host's OWN architecture,\n         \
     so an x86_64 guest is emulated instruction-by-instruction here no matter what\n         \
     /dev/kvm permits — TCG is what makes it bootable on this host at all.";

/// Whether the host can hand qemu KVM, and when it cannot, which of the two unrelated
/// reasons applies — they take different advice, so a bare `false` cannot be explained.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum KvmStatus {
    Usable,
    /// KVM virtualizes the HOST architecture; no device permission makes it accelerate
    /// a foreign-arch guest.
    WrongArch,
    /// The node did not open O_RDWR: absent, or present but owned by a `kvm` group this
    /// user is not in.
    NodeUnavailable,
}

impl KvmStatus {
    fn hint(self) -> Option<&'static str> {
        match self {
            Self::Usable => None,
            Self::WrongArch => Some(TCG_WRONG_ARCH_HINT),
            Self::NodeUnavailable => Some(TCG_NO_NODE_HINT),
        }
    }

    /// The same two reasons, worded for a check's boot, which emulates nothing
    /// and so refuses. The process's groups are fixed at login, which is how a
    /// user in `kvm` still has a run that cannot open the node.
    fn refusal(self) -> Option<&'static str> {
        match self {
            Self::Usable => None,
            Self::WrongArch => Some(
                "KVM accelerates only a guest of the host's own architecture, and this \
                 guest is x86_64",
            ),
            Self::NodeUnavailable => Some(
                "/dev/kvm does not open read/write for this process; it is absent, or its \
                 `kvm` group is not among this process's groups (a login started before \
                 joining the group keeps the old ones)",
            ),
        }
    }
}

/// The pure half of the probe: what an architecture and an open attempt mean together.
/// A foreign arch wins over an openable node — an aarch64 host has its own working
/// /dev/kvm, and it still cannot run an x86_64 guest natively.
fn kvm_status_from(arch: &str, node_opens: bool) -> KvmStatus {
    if arch != "x86_64" {
        KvmStatus::WrongArch
    } else if node_opens {
        KvmStatus::Usable
    } else {
        KvmStatus::NodeUnavailable
    }
}

/// Read the override out of the environment. A non-UTF-8 value is an error for the same
/// reason a misspelled one is: it is an operator asking for something, and the answer
/// must not be a silent slow boot. Blank reads as unset, matching how the runner's
/// `host_display_available` treats an empty variable. Pure in its argument so the
/// parsing is testable without mutating process env.
fn forced_accel(raw: Option<&OsStr>) -> Result<Option<&str>, String> {
    let Some(raw) = raw else { return Ok(None) };
    let Some(text) = raw.to_str() else {
        return Err(format!(
            "{ACCEL_ENV} is not valid UTF-8; use `kvm`, `tcg`, or unset it to let the \
             runner probe /dev/kvm"
        ));
    };
    Ok(Some(text).filter(|t| !t.trim().is_empty()))
}

/// Choose the accelerator. A `-accel` may be repeated: qemu tries them in order and
/// moves to the next when one fails to initialize, so listing tcg behind kvm keeps a
/// host whose `/dev/kvm` opened but whose kernel then refuses (wedged module, nested
/// virt off) booting rather than erroring out. An explicitly forced `kvm` gets NO
/// fallback — an operator who asked for it wants the failure, not a silent hour of TCG.
/// `probe` runs only when nothing is forced, so an override never touches /dev/kvm.
fn accel_plan(
    probe: impl FnOnce() -> KvmStatus,
    forced: Option<&str>,
) -> Result<AccelPlan, String> {
    match forced.map(str::trim) {
        Some("tcg") => Ok(AccelPlan {
            names: &["tcg"],
            label: "TCG",
            // Forced: the operator already knows why, so no hint.
            hint: None,
            forced: true,
        }),
        Some("kvm") => Ok(AccelPlan {
            names: &["kvm"],
            label: "KVM",
            hint: None,
            forced: true,
        }),
        // Report the value as SET, not as trimmed: echoing back `"kvm"` for a
        // `TD_QEMU_ACCEL=$'kvm\t'` that was rejected shows the operator a value that
        // looks exactly right.
        Some(_) => Err(format!(
            "{ACCEL_ENV}={:?} is not a known accelerator; use `kvm`, `tcg`, or unset it \
             to let the runner probe /dev/kvm",
            forced.unwrap_or_default()
        )),
        None => Ok(match probe() {
            // The label names the whole list, not its head: the probe proves only that
            // /dev/kvm opened, and a kernel that then refuses sends qemu to tcg. A bare
            // "KVM" would print the fast answer over the slow boot it fell back to.
            KvmStatus::Usable => AccelPlan {
                names: &["kvm", "tcg"],
                label: "KVM, TCG fallback",
                hint: None,
                forced: false,
            },
            unusable => AccelPlan {
                names: &["tcg"],
                label: "TCG",
                hint: unusable.hint(),
                forced: false,
            },
        }),
    }
}

/// Ask the host. The arch is the one this binary was built for, which IS the host's —
/// the runner is compiled by the host cargo. Existence of the node is not enough: qemu
/// opens `/dev/kvm` O_RDWR, and the common unusable case is a present node whose `kvm`
/// group the operator is not in — only an open attempt distinguishes the two. Opening
/// the control node creates no VM; the fd closes here. The device is touched only when
/// the arch could use it.
fn kvm_status() -> KvmStatus {
    let arch = std::env::consts::ARCH;
    let node_opens = arch == "x86_64"
        && OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok();
    kvm_status_from(arch, node_opens)
}

/// The plan for this process: `TD_QEMU_ACCEL` when set, else the probe.
pub(crate) fn from_env() -> Result<AccelPlan, String> {
    let forced = std::env::var_os(ACCEL_ENV);
    accel_plan(kvm_status, forced_accel(forced.as_deref())?)
}

/// `from_env` for an operator's launch nobody else watches (`test-iso`): a
/// forced `kvm` this host cannot hand over is refused before anything is
/// created, with the probe's reason, since nothing falls back and qemu's own
/// error does not name the override.
pub(crate) fn launch_from_env() -> Result<AccelPlan, String> {
    launch_plan(from_env()?, kvm_status)
}

fn launch_plan(plan: AccelPlan, probe: impl FnOnce() -> KvmStatus) -> Result<AccelPlan, String> {
    if plan.forced && plan.names == ["kvm"] {
        if let Some(hint) = probe().hint() {
            return Err(format!(
                "{ACCEL_ENV}=kvm, but this host cannot hand qemu KVM. {hint}"
            ));
        }
    }
    Ok(plan)
}

/// The plan for a check's boot: KVM with nothing behind it. TCG runs these
/// boots several times slower, hours for the system oracles, and a pass says
/// nothing of it, so a host that cannot hand qemu KVM is a host gap, refused
/// before anything is created. `TD_QEMU_ACCEL=tcg` is the explicit way to
/// emulate one anyway.
pub(crate) fn headless_from_env() -> Result<AccelPlan, String> {
    let forced = std::env::var_os(ACCEL_ENV);
    headless_plan(kvm_status, forced_accel(forced.as_deref())?)
}

/// What a check's boot asks qemu for under this environment, without asking
/// the host: the accelerator a pass is keyed on. A boot gets exactly these or
/// does not boot, so the probe would add nothing to the key.
pub(crate) fn headless_names_from_env() -> Result<&'static [&'static str], String> {
    let forced = std::env::var_os(ACCEL_ENV);
    Ok(headless_plan(|| KvmStatus::Usable, forced_accel(forced.as_deref())?)?.names)
}

fn headless_plan(
    probe: impl FnOnce() -> KvmStatus,
    forced: Option<&str>,
) -> Result<AccelPlan, String> {
    match forced.map(str::trim) {
        // A pinned tcg, or the error an unknown value earns; neither probes.
        Some(pin) if pin != "kvm" => accel_plan(probe, forced),
        _ => match probe().refusal() {
            None => Ok(AccelPlan {
                names: &["kvm"],
                label: "KVM",
                hint: None,
                forced: forced.is_some(),
            }),
            Some(why) => Err(format!(
                "{HOST_GAP}a check's boot runs on KVM alone{pinned}, and this host cannot \
                 hand qemu KVM: {why}. {ACCEL_ENV}=tcg emulates it instead, several times \
                 slower",
                pinned = if forced.is_some() {
                    format!(" ({ACCEL_ENV}=kvm pins it)")
                } else {
                    String::new()
                }
            )),
        },
    }
}

/// The accelerator line a boot log carries, with the reason a boot is
/// software-emulated when it is: every timing in the log reads differently
/// under TCG.
pub(crate) fn describe(plan: &AccelPlan) -> String {
    match plan.hint {
        Some(hint) => format!("accelerator: {}\n         {hint}", plan.label),
        None if plan.forced => format!("accelerator: {} ({ACCEL_ENV})", plan.label),
        None => format!("accelerator: {}", plan.label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn probed_kvm_keeps_tcg_behind_it() {
        // The probe only proves /dev/kvm opened; the kernel can still refuse at
        // KVM_CREATE_VM, and qemu then walks to the next -accel instead of exiting.
        let plan = accel_plan(|| KvmStatus::Usable, None).unwrap();
        assert_eq!(plan.names, ["kvm", "tcg"]);
        // ...which is why the label names the fallback too: an operator reading a bare
        // "KVM" over a boot qemu had quietly demoted to TCG is told the wrong thing.
        assert_eq!(plan.label, "KVM, TCG fallback");
        assert!(plan.hint.is_none());
        assert!(!plan.forced);
    }

    #[test]
    fn no_kvm_falls_back_to_tcg_and_says_why() {
        let plan = accel_plan(|| KvmStatus::NodeUnavailable, None).unwrap();
        assert_eq!(plan.names, ["tcg"]);
        assert_eq!(plan.label, "TCG");
        // The whole point of the hint: an operator who does not know the boot COULD be
        // fast has no reason to go looking.
        assert_eq!(plan.hint, Some(TCG_NO_NODE_HINT));
    }

    #[test]
    fn a_foreign_arch_gets_advice_that_can_actually_work() {
        // Both unusable cases boot TCG, but only one is fixable by getting at the
        // device. Handing the /dev/kvm advice to an aarch64 host sends its operator
        // after access that changes nothing — and that they may already hold.
        let plan = accel_plan(|| KvmStatus::WrongArch, None).unwrap();
        assert_eq!(plan.names, ["tcg"]);
        assert_eq!(plan.hint, Some(TCG_WRONG_ARCH_HINT));
        assert_ne!(plan.hint, Some(TCG_NO_NODE_HINT));
    }

    #[test]
    fn a_foreign_arch_outranks_an_openable_node() {
        // An aarch64 host has a perfectly good /dev/kvm of its own; it still cannot run
        // an x86_64 guest natively, so the arch has to win.
        assert_eq!(kvm_status_from("aarch64", true), KvmStatus::WrongArch);
        assert_eq!(kvm_status_from("aarch64", false), KvmStatus::WrongArch);
        assert_eq!(kvm_status_from("x86_64", true), KvmStatus::Usable);
        // The group case: node present, this user cannot open it.
        assert_eq!(kvm_status_from("x86_64", false), KvmStatus::NodeUnavailable);
    }

    #[test]
    fn every_unusable_status_explains_itself() {
        // A status that boots TCG with no hint is a slow boot with nothing said.
        for status in [KvmStatus::WrongArch, KvmStatus::NodeUnavailable] {
            assert!(status.hint().is_some(), "{status:?}");
        }
        assert!(KvmStatus::Usable.hint().is_none());
    }

    #[test]
    fn forced_kvm_does_not_silently_fall_back() {
        // Forcing it is how an operator checks that KVM works; a TCG fallback would
        // answer "yes, slowly" to a question about hardware acceleration.
        let plan = accel_plan(|| KvmStatus::NodeUnavailable, Some("kvm")).unwrap();
        assert_eq!(plan.names, ["kvm"]);
        assert_eq!(plan.label, "KVM");
        // Recorded so a qemu that then refuses to start can say the override is why
        // nothing caught it.
        assert!(plan.forced);
    }

    #[test]
    fn forced_tcg_overrides_a_usable_kvm() {
        let plan = accel_plan(|| KvmStatus::Usable, Some("tcg")).unwrap();
        assert_eq!(plan.names, ["tcg"]);
        assert_eq!(plan.label, "TCG");
        // Asked for, so not a surprise worth explaining.
        assert!(plan.hint.is_none());
        assert!(plan.forced);
    }

    #[test]
    fn an_override_never_probes_dev_kvm() {
        // The operator settled the question; opening the device anyway would make the
        // answer depend on something the override exists to take out of the picture.
        for forced in ["kvm", "tcg", "bogus"] {
            let probed = Cell::new(false);
            let _ = accel_plan(
                || {
                    probed.set(true);
                    KvmStatus::Usable
                },
                Some(forced),
            );
            assert!(!probed.get(), "{forced} probed /dev/kvm");
        }
    }

    #[test]
    fn surrounding_whitespace_still_selects() {
        assert_eq!(
            accel_plan(|| KvmStatus::NodeUnavailable, Some("  kvm "))
                .unwrap()
                .names,
            ["kvm"]
        );
    }

    #[test]
    fn an_unknown_accelerator_is_an_error_not_a_shrug() {
        // Silently ignoring `TD_QEMU_ACCEL=KVM` would hand back the slow boot the
        // operator was trying to escape, with nothing said.
        for bad in ["KVM", "kvm:tcg", "hvf", "1", "none"] {
            let err = accel_plan(|| KvmStatus::Usable, Some(bad)).unwrap_err();
            assert!(err.contains(ACCEL_ENV), "{bad}: {err}");
        }
    }

    #[test]
    fn a_rejected_value_is_echoed_as_it_was_set() {
        // Trimming before reporting shows the operator `"kvm"` as the thing that was
        // rejected — a value that looks exactly right, hiding the trailing tab that is
        // the actual complaint.
        let err = accel_plan(|| KvmStatus::Usable, Some("kvm\tx")).unwrap_err();
        assert!(err.contains("kvm\\tx"), "{err}");
    }

    #[test]
    fn an_unset_or_blank_override_leaves_it_to_the_probe() {
        assert_eq!(forced_accel(None).unwrap(), None);
        for blank in ["", "   ", "\t"] {
            assert_eq!(forced_accel(Some(OsStr::new(blank))).unwrap(), None);
        }
        assert_eq!(forced_accel(Some(OsStr::new("kvm"))).unwrap(), Some("kvm"));
    }

    #[test]
    fn a_non_utf8_override_errors_rather_than_reading_as_unset() {
        // `env::var(..).ok()` would fold this into `None` and probe on, so an operator
        // who set the variable to something unrepresentable gets the behaviour they
        // were overriding — the one outcome this whole knob exists to prevent.
        let bad = OsStr::from_bytes(b"kv\xffm");
        let err = forced_accel(Some(bad)).unwrap_err();
        assert!(err.contains(ACCEL_ENV), "{err}");
    }

    #[test]
    fn every_plan_names_at_least_one_known_accelerator() {
        // An empty list would drop `-accel` from the argv entirely and leave the guest
        // on whatever qemu defaults to — the silent revert this whole change is about.
        for status in [
            KvmStatus::Usable,
            KvmStatus::NodeUnavailable,
            KvmStatus::WrongArch,
        ] {
            for forced in [None, Some("kvm"), Some("tcg")] {
                let plan = accel_plan(|| status, forced).unwrap();
                assert!(!plan.names.is_empty());
                for name in plan.names {
                    assert!(matches!(*name, "kvm" | "tcg"), "unknown accelerator {name}");
                }
            }
        }
    }

    #[test]
    fn a_check_boot_runs_on_kvm_alone() {
        // Probed or pinned, a usable KVM is the whole list: with tcg behind it
        // a kernel that refused KVM would send the boot to hours of emulation
        // that its pass never mentions.
        for forced in [None, Some("kvm"), Some(" kvm ")] {
            let plan = headless_plan(|| KvmStatus::Usable, forced).unwrap();
            assert_eq!(plan.names, ["kvm"], "{forced:?}");
            assert_eq!(plan.label, "KVM");
            assert_eq!(plan.forced, forced.is_some());
        }
        let probed = headless_plan(|| KvmStatus::Usable, None).unwrap();
        assert_eq!(describe(&probed), "accelerator: KVM");
    }

    #[test]
    fn a_check_boot_without_kvm_is_a_host_gap_not_an_emulated_run() {
        // Probed or pinned: a pinned kvm the host lacks is the same gap.
        for forced in [None, Some("kvm")] {
            for (status, cue) in [
                (KvmStatus::NodeUnavailable, "/dev/kvm"),
                (KvmStatus::WrongArch, "architecture"),
            ] {
                let err = headless_plan(|| status, forced).unwrap_err();
                assert!(err.starts_with(HOST_GAP), "{forced:?} {status:?}: {err}");
                assert!(err.contains(cue), "{status:?} names its reason: {err}");
                assert!(err.contains("TD_QEMU_ACCEL=tcg"), "names the opt-in: {err}");
                assert_eq!(err.contains("TD_QEMU_ACCEL=kvm"), forced.is_some(), "{err}");
            }
        }
    }

    #[test]
    fn a_pinned_tcg_still_emulates_a_check_boot_and_never_probes() {
        let tcg = headless_plan(|| panic!("tcg is not probed"), Some("tcg")).unwrap();
        assert_eq!(tcg.names, ["tcg"]);
        assert_eq!(describe(&tcg), "accelerator: TCG (TD_QEMU_ACCEL)");
        // An unknown value is the operator's error, not a host gap.
        let err = headless_plan(|| panic!("not probed"), Some("kvm:tcg")).unwrap_err();
        assert!(
            !err.starts_with(HOST_GAP) && err.contains(ACCEL_ENV),
            "{err}"
        );
    }

    #[test]
    fn an_operator_launch_keeps_its_fallback_and_refuses_a_forced_kvm_it_cannot_have() {
        // `run` and `test-iso` have an operator watching, and a slow boot
        // beats none there; they are no check.
        let forced = |name| accel_plan(|| KvmStatus::Usable, Some(name)).unwrap();
        let refused = launch_plan(forced("kvm"), || KvmStatus::NodeUnavailable);
        assert!(refused.is_err_and(|e| e.contains("TD_QEMU_ACCEL=kvm") && e.contains("kvm")));
        assert!(launch_plan(forced("kvm"), || KvmStatus::WrongArch).is_err());
        let granted = launch_plan(forced("kvm"), || KvmStatus::Usable).unwrap();
        assert_eq!(granted.names, ["kvm"]);
        // Only a forced kvm is probed: the probe already chose the others.
        let tcg = launch_plan(forced("tcg"), || panic!("tcg is not probed")).unwrap();
        assert_eq!(tcg.names, ["tcg"]);
        let probed = accel_plan(|| KvmStatus::Usable, None).unwrap();
        let probed = launch_plan(probed, || panic!("a probed plan is not re-probed")).unwrap();
        assert_eq!(probed.names, ["kvm", "tcg"]);
        assert_eq!(describe(&probed), "accelerator: KVM, TCG fallback");
        let slow = accel_plan(|| KvmStatus::NodeUnavailable, None).unwrap();
        assert!(describe(&slow).starts_with("accelerator: TCG\n"));
    }
}
