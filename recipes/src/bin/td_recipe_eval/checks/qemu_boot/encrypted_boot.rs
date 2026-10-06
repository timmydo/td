//! `td-recipe-eval qemu-boot-encrypted --tpm /absolute/path/to/swtpm`:
//! td-install/ENCRYPTION.md increment 6's oracle, outside the integration
//! tier. It installs a device-bound system as `qemu-install-encrypted`
//! does, onto a disposable disk under firmware measuring into the pinned
//! swtpm, then boots the selector td ships from that disk through the same
//! firmware and the same TPM state, and drives every leg of "Acceptance
//! evidence" but the no-TPM disclosure. Without `--tpm` it is an
//! unprovisioned host gap.
//!
//! The recovery key leaves the installation guest over a second serial
//! port only the host reads, never the console; the host carries it into
//! the two guest legs that need it, in their initramfs, and removes each
//! medium once its boot ends. Every console is checked for the key before
//! it is kept.
use super::encrypted::{self, Bench, HeaderState, HeaderToken};
use super::install::{self, protocol, Installed, TargetDisk};
use super::live;
use super::recovery_screen::KeyGlyphs;
use super::serial_shell::{AnswerLine, ConsoleAnswer, ConsoleAnswers};
use super::setup_input::KeyCell;
use super::*;

/// What this oracle builds: the system and the guest fixture.
pub(crate) const TARGETS: &[&str] = &["system-x86-64", "td-install-qemu-test"];

const LABEL: &str = "qemu-boot-encrypted";
const RECOVERY_DIGITS: usize = 48;
/// The installed selector's halt, which ends every refused boot.
const HALTED: &str = "td-boot: halted; only a platform reset leaves this";
/// The installed selector's console lines all begin so.
const TD_BOOT: &str = "td-boot: ";
/// The first-boot transition's commits, in td-protector's plan order
/// (td-protector/DESIGN.md "Transitions"): the first-boot token 0 on
/// keyslot 1 is replaced by a device-bound token 1 on keyslot 2.
const FIRST_BOOT_STEPS: &[&str] = &[
    "td-boot: transition step 1/6: keyslot 1 tested",
    "td-boot: transition step 2/6: keyslot 2 added, keyslot 1 authorizing",
    "td-boot: transition step 3/6: token for keyslot 2 imported",
    "td-boot: transition step 4/6: keyslot 2 tested",
    "td-boot: transition step 5/6: keyslot 1 destroyed, keyslot 2 authorizing",
    "td-boot: transition step 6/6: token 0 removed",
];
const FIRST_BOOT_RELEASED: &str = "td-boot: td token 0 (first-boot) released";
const DEVICE_BOUND_RELEASED: &str = "td-boot: td token 1 (device-bound) released";
const CAP_CLOSED: &str = "td-boot: PCR 12 release cap closed";
/// The installed selector's wait, printed only when the TPM's node is not
/// there yet when it looks.
const TPM_WAIT_PREFIX: &str = "td-boot: waiting up to ";
/// The deployment initramfs's post-cap check (ENCRYPTION.md "Boot and
/// authority boundaries").
const POST_CAP_PREFIX: &str = "td-boot: post-cap unseal of td token ";
/// The release's reason for recovery when no td token released.
const NOTHING_RELEASED: &str = "td-boot: no td protector released";
/// The recovery flow's console contract (td-boot `selector_release`):
/// secret-line prints each prompt, with no newline until the entry ends.
const KEY_PROMPT: &str = "td recovery key: ";
const RESEAL_PROMPT: &str =
    "Type reseal to seal a protector to this boot chain, anything else to boot once: ";
const RECOVERY_ASKED: &str = "td-boot: recovery: enter the recovery key's 48 digits on the \
     keyboard's top row, unshifted, spaces or hyphens between groups optional, keypad digits \
     only with Num Lock on; it is not shown as it is typed";
const ENTER_AGAIN: &str = ": enter it again";
const WRONG_KEY_PREFIX: &str = "td-boot: the recovery key does not open keyslot 0 (";
const KEY_OPENS: &str = "td-boot: the recovery key opens keyslot 0";
const RESEAL_WARNING: &str = "td-boot: a reseal binds release to this boot chain and retires \
     every other td protector; keyslot 0's recovery key stays";
/// Printed after the warning, before the reseal question.
const RESEAL_KEYS: &str =
    "td-boot: type the answer at its US key positions: the keyboard is read as a US one";
const RESEAL_DECLINED: &str = "td-boot: reseal declined: this boot opens with the recovery key \
     once and the header is unchanged";
const RESEAL_COMPLETE_PREFIX: &str = "td-boot: reseal complete: keyslot ";
/// An entry the recovery-key codec refuses before cryptsetup sees it.
const REFUSED_ENTRY: &[u8] = b"12345";
/// The live selector's cap and its handoff (MEDIA.md "Live boot").
const LIVE_CAPPING: &str = "td-boot: capping PCR 12";
const LIVE_DEPLOYMENT_PREFIX: &str = "td-boot: live deployment ";
/// A load option the changed-load-option leg adds through a firmware boot
/// entry; the EFI stub measures it into PCR 9.
const LOAD_OPTION: &str = "td.oracle-load-option=1";

pub(crate) fn options(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--tpm" && Path::new(path).is_absolute() => {
            Ok(Some(PathBuf::from(path)))
        }
        _ => {
            Err("usage: td-recipe-eval qemu-boot-encrypted [--tpm /absolute/path/to/swtpm]".into())
        }
    }
}

/// A first-boot transition commit the host cuts power on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Commit {
    Keyslot,
    Token,
    Test,
    Destroy,
}

impl Commit {
    /// The plan step whose console line the host cuts on, counted from 1.
    fn step(self) -> usize {
        match self {
            Self::Keyslot => 2,
            Self::Token => 3,
            Self::Test => 4,
            Self::Destroy => 5,
        }
    }

    fn line(self) -> &'static str {
        FIRST_BOOT_STEPS
            .get(self.step() - 1)
            .copied()
            .unwrap_or_default()
    }

    fn name(self) -> &'static str {
        match self {
            Self::Keyslot => "keyslot",
            Self::Token => "token",
            Self::Test => "test",
            Self::Destroy => "destroy",
        }
    }
}

/// A boot chain or TPM the release was not sealed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// `\EFI\BOOT\BOOTX64.EFI` with its PE time stamp moved: PCR 4.
    SelectorImage,
    /// `\EFI\BOOT\INITRD` with one cpio header's mtime moved: PCR 9.
    Initramfs,
    /// A firmware boot entry with a load option: PCR 9.
    LoadOption,
    /// A fresh swtpm state: a new storage primary seed, as a cleared or
    /// different TPM has.
    FreshTpm,
}

impl Change {
    fn name(self) -> &'static str {
        match self {
            Self::SelectorImage => "changed-selector-image",
            Self::Initramfs => "changed-initramfs",
            Self::LoadOption => "changed-load-option",
            Self::FreshTpm => "fresh-tpm",
        }
    }

    /// Whether the leg confirms the reseal at recovery. The selector-image
    /// leg declines it, so that its boot opens once with the recovery key
    /// and the header is unchanged.
    fn reseals(self) -> bool {
        !matches!(self, Self::SelectorImage)
    }

    /// Where the leg answers recovery: the changed initramfs on the VT,
    /// through the PS/2 keyboard with the serial line idle, the others on
    /// the serial line with the VT idle (ENCRYPTION.md "Acceptance
    /// evidence", increment 7).
    fn line(self) -> AnswerLine {
        match self {
            Self::Initramfs => AnswerLine::Vt,
            _ => AnswerLine::Serial,
        }
    }

    /// How td-protector types the device-bound token's refusal: a changed
    /// chain passes PolicyPCR before the cap and is refused at Unseal; a
    /// new storage primary cannot load the sealed object at all.
    fn refusal(self) -> &'static str {
        match self {
            Self::FreshTpm => "load refused: ",
            _ => "policy refused: ",
        }
    }
}

/// One leg of the oracle, in the order `LEGS` runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leg {
    /// The default wizard on the production live medium, the TPM, display
    /// and keyboard attached: device-bound by the probes, its key read from
    /// the completion page's pixels and typed back, then the installed disk
    /// booted twice with nothing typed. A machine of its own.
    Wizard,
    /// The device-bound installation, the key typed back and sent to the
    /// host over the private serial port.
    Install,
    /// The installed first boot: the first-boot protector releases, the
    /// selector seals the device-bound one, caps PCR 12 and transitions,
    /// and the system comes up and writes a file into the account's home.
    FirstBoot,
    /// The second boot releases the device-bound protector with no
    /// interaction and reads the file back.
    SecondBoot,
    /// A copy of the installed disk, power cut when the selector reports
    /// the commit, then booted to completion.
    Interrupted(Commit),
    /// The live medium on the installed machine: its selector caps, and
    /// nothing releases.
    Live,
    /// Header states a guest builds with cryptsetup, which the selector's
    /// next plan retires.
    Headers,
    /// A changed boot chain or TPM refuses release and reaches recovery.
    Changed(Change),
    /// The volume key, taken in a guest with the recovery key, and the
    /// recovery key are nowhere on the disk, in the plaintext or in what
    /// the host kept.
    Inspect,
}

const LEGS: &[Leg] = &[
    Leg::Wizard,
    Leg::Install,
    Leg::FirstBoot,
    Leg::SecondBoot,
    Leg::Interrupted(Commit::Keyslot),
    Leg::Interrupted(Commit::Token),
    Leg::Interrupted(Commit::Test),
    Leg::Interrupted(Commit::Destroy),
    Leg::Live,
    Leg::Headers,
    Leg::Changed(Change::SelectorImage),
    Leg::Changed(Change::Initramfs),
    Leg::Changed(Change::LoadOption),
    Leg::Changed(Change::FreshTpm),
    Leg::Inspect,
];

/// The recovery key, as its 48 digits, held in one heap buffer zeroed on
/// drop.
struct RecoveryKey(Box<[u8; RECOVERY_DIGITS]>);

impl Drop for RecoveryKey {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

impl RecoveryKey {
    /// The private serial port's whole content: exactly the digits and a
    /// newline.
    fn from_channel(mut bytes: Vec<u8>) -> Result<Self, String> {
        let parsed = match bytes.strip_suffix(b"\n") {
            Some(digits)
                if digits.len() == RECOVERY_DIGITS && digits.iter().all(u8::is_ascii_digit) =>
            {
                let mut key = Box::new([0; RECOVERY_DIGITS]);
                key.copy_from_slice(digits);
                Ok(Self(key))
            }
            _ => Err(format!(
                "the private serial port carried {} bytes, not the 48 digits and a newline",
                bytes.len()
            )),
        };
        bytes.fill(0);
        std::hint::black_box(&bytes);
        parsed
    }

    fn digits(&self) -> &[u8] {
        &self.0[..]
    }

    /// The display form: eight groups of six digits joined by hyphens.
    fn display(&self) -> Vec<u8> {
        let mut text = Vec::with_capacity(RECOVERY_DIGITS + 7);
        for (index, digit) in self.0.iter().enumerate() {
            if index > 0 && index % 6 == 0 {
                text.push(b'-');
            }
            text.push(*digit);
        }
        text
    }

    /// Whether `bytes` holds the key: its digits alone, or its groups
    /// joined by hyphens or by spaces.
    fn found_in(&self, bytes: &[u8]) -> bool {
        let mut display = self.display();
        let mut spaced: Vec<u8> = display
            .iter()
            .map(|byte| if *byte == b'-' { b' ' } else { *byte })
            .collect();
        let found = bytes.windows(RECOVERY_DIGITS).any(|w| w == self.digits())
            || bytes
                .windows(display.len())
                .any(|w| w == display.as_slice() || w == spaced.as_slice());
        display.fill(0);
        spaced.fill(0);
        std::hint::black_box((&display, &spaced));
        found
    }

    /// A key the codec admits that is not this one, in display form: the
    /// first group's five-digit value moved by one (within 16 bits) and its
    /// Damm check digit made again, so cryptsetup, not the codec, refuses
    /// it.
    fn wrong(&self) -> Result<Vec<u8>, String> {
        let mut text = self.display();
        let group = text.get_mut(..GROUP_DIGITS).ok_or("short recovery key")?;
        let value = group
            .iter()
            .take(GROUP_DIGITS - 1)
            .try_fold(0u32, |value, digit| {
                digit
                    .checked_sub(b'0')
                    .filter(|digit| *digit < 10)
                    .map(|digit| value * 10 + u32::from(digit))
            })
            .ok_or("a recovery key group that is not digits")?;
        let moved = format!("{:05}", (value + 1) % 65_536);
        let check = damm(moved.as_bytes()).ok_or("a moved group that is not digits")?;
        for (slot, digit) in group.iter_mut().zip(moved.bytes().chain([b'0' + check])) {
            *slot = digit;
        }
        Ok(text)
    }
}

/// Digits in a recovery-key group: five for a 16-bit value, one check.
const GROUP_DIGITS: usize = 6;

/// The Damm check digit over ASCII `digits`, as td-protector's recovery
/// codec computes it; zero over a whole group whose check digit agrees.
fn damm(digits: &[u8]) -> Option<u8> {
    const TABLE: [[u8; 10]; 10] = [
        [0, 3, 1, 7, 5, 9, 8, 6, 4, 2],
        [7, 0, 9, 2, 1, 5, 4, 8, 6, 3],
        [4, 2, 0, 6, 8, 7, 1, 3, 5, 9],
        [1, 7, 5, 0, 9, 8, 3, 4, 2, 6],
        [6, 1, 2, 3, 0, 4, 5, 9, 7, 8],
        [3, 6, 7, 4, 2, 0, 9, 5, 8, 1],
        [5, 8, 6, 9, 7, 2, 0, 1, 3, 4],
        [8, 9, 4, 5, 3, 6, 2, 0, 1, 7],
        [9, 4, 3, 8, 6, 1, 7, 2, 0, 5],
        [2, 5, 8, 1, 4, 3, 6, 7, 9, 0],
    ];
    digits.iter().try_fold(0u8, |interim, digit| {
        let column = usize::from(digit.checked_sub(b'0')?);
        TABLE
            .get(usize::from(interim))
            .and_then(|row| row.get(column))
            .copied()
    })
}

/// What a changed leg types at recovery: an entry the codec refuses, a
/// well-formed wrong key, the key, then the reseal answer.
fn recovery_answers(key: &RecoveryKey, reseal: bool) -> Result<Vec<ConsoleAnswer>, String> {
    let answer = |prompt, reply| ConsoleAnswer { prompt, reply };
    Ok(vec![
        answer(KEY_PROMPT, REFUSED_ENTRY.to_vec()),
        answer(KEY_PROMPT, key.wrong()?),
        answer(KEY_PROMPT, key.display()),
        answer(
            RESEAL_PROMPT,
            if reseal {
                b"reseal".to_vec()
            } else {
                b"boot once".to_vec()
            },
        ),
    ])
}

/// The installed selector's and deployment initramfs's console lines, as
/// they reached the console, each trimmed: every line beginning `td-boot: `.
fn td_boot_lines(console: &str) -> Vec<&str> {
    console
        .lines()
        .map(str::trim_end)
        .filter(|line| line.starts_with(TD_BOOT))
        .collect()
}

/// The selector's release lines: every `td-boot:` line up to and with the
/// first line `end` accepts, its optional TPM wait left out. `None` when
/// no line ends it.
fn release_lines<'a>(console: &'a str, end: &dyn Fn(&str) -> bool) -> Option<Vec<&'a str>> {
    let mut lines = Vec::new();
    for line in td_boot_lines(console) {
        if line.starts_with(TPM_WAIT_PREFIX) {
            continue;
        }
        lines.push(line);
        if end(line) {
            return Some(lines);
        }
    }
    None
}

/// The selector's line opening the volume, as td-boot reports it: the
/// volume, the keyslot, then the mapping's `dm-N` node and its name.
fn opened_with(line: &str, uuid: &str) -> Option<u8> {
    let rest = line.strip_prefix(&format!("td-boot: volume {uuid} opened with keyslot "))?;
    let (slot, mapping) = rest.split_once(" as ")?;
    let node = mapping.strip_suffix(" (td-selector)")?;
    let minor = node.strip_prefix("dm-")?;
    (!minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| slot.parse::<u8>().ok())
        .flatten()
}

/// The released boot's release order: `expected` in order, then the line
/// opening the volume with `keyslot`. Then the deployment initramfs's
/// post-cap check refused every td token the selector would try, each by
/// policy, after the selection: `post_cap` names them.
fn require_release(
    result: &BootResult,
    uuid: &str,
    expected: &[&str],
    keyslot: u8,
    post_cap: &[(u8, &str)],
    leg: &str,
) -> Result<(), String> {
    let console = &result.console;
    let show = || encrypted::redacted(&tail(console, 160));
    let lines = release_lines(console, &|line| opened_with(line, uuid).is_some())
        .ok_or_else(|| format!("{leg}: the selector never opened the volume\n{}", show()))?;
    let (opened, before) = lines
        .split_last()
        .ok_or_else(|| format!("{leg}: no release lines"))?;
    if before != expected || opened_with(opened, uuid) != Some(keyslot) {
        return Err(format!(
            "{leg}: the selector's release lines were {lines:#?}, not {expected:#?} then the \
             volume opened with keyslot {keyslot}\n{}",
            show()
        ));
    }
    let post: Vec<&str> = td_boot_lines(console)
        .into_iter()
        .filter(|line| line.starts_with(POST_CAP_PREFIX))
        .collect();
    let refused = post.len() == post_cap.len()
        && post.iter().zip(post_cap).all(|(line, (number, role))| {
            line.starts_with(&format!(
                "{POST_CAP_PREFIX}{number} ({role}) refused: policy refused: "
            ))
        });
    if !refused {
        return Err(format!(
            "{leg}: the deployment initramfs's post-cap check reported {post:#?}, not a policy \
             refusal of each of {post_cap:?}\n{}",
            show()
        ));
    }
    // The check runs after kexec, so after the selection.
    let selected = console
        .find(td_boot_protocol::SELECTED_CURRENT_MARKER)
        .ok_or_else(|| format!("{leg}: no deployment was selected\n{}", show()))?;
    let checked = console.find(POST_CAP_PREFIX).unwrap_or(0);
    if checked < selected {
        return Err(format!(
            "{leg}: the post-cap check ran before the selection"
        ));
    }
    for refused in [
        HALTED,
        "failed (",
        "does not open",
        "no transition this boot",
    ] {
        if td_boot_lines(console)
            .iter()
            .any(|line| line.contains(refused))
        {
            return Err(format!(
                "{leg}: the selector reported {refused:?}\n{}",
                show()
            ));
        }
    }
    Ok(())
}

/// A recovered boot: the device-bound token refused as `change` makes
/// the TPM refuse it, the cap closed, nothing released, then the recovery
/// flow over `recovery_answers` (the codec's refusal, cryptsetup's, the
/// key opening keyslot 0, the reseal warning) and the declined or
/// completed reseal, the volume opened on keyslot 0 and, after the
/// selection, the post-cap check's policy refusal of `post_cap`, the td
/// token the header then holds. Returns the reseal's plan steps and the
/// keyslot it reported.
fn require_recovered<'a>(
    result: &'a BootResult,
    uuid: &str,
    change: Change,
    post_cap: u8,
) -> Result<(Vec<&'a str>, Option<u8>), String> {
    let leg = change.name();
    let show = || encrypted::redacted(&tail(&result.console, 160));
    let lines = release_lines(&result.console, &|line| opened_with(line, uuid).is_some())
        .ok_or_else(|| format!("{leg}: the selector never opened the volume\n{}", show()))?;
    let refusal = format!("td-boot: td token 1 (device-bound): {}", change.refusal());
    let (head, rest) = lines.split_at(lines.len().min(9));
    let head_ok = match head {
        [token, cap, nothing, asked, codec, wrong, opens, warning, keys] => {
            token.starts_with(&refusal)
                && *cap == CAP_CLOSED
                && *nothing == NOTHING_RELEASED
                && *asked == RECOVERY_ASKED
                && codec.ends_with(ENTER_AGAIN)
                && !codec.starts_with(WRONG_KEY_PREFIX)
                && wrong.starts_with(WRONG_KEY_PREFIX)
                && wrong.ends_with(&format!("){ENTER_AGAIN}"))
                && *opens == KEY_OPENS
                && *warning == RESEAL_WARNING
                && *keys == RESEAL_KEYS
        }
        _ => false,
    };
    let (outcome, opened) = match rest {
        [between @ .., outcome, opened] => (between, (outcome, opened)),
        _ => (&[][..], (&"", &"")),
    };
    let (reported, opened) = opened;
    let steps = plan_steps(outcome)?;
    let keyslot = reported
        .strip_prefix(RESEAL_COMPLETE_PREFIX)
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(slot, _)| slot.parse::<u8>().ok());
    let tail_ok = if change.reseals() {
        steps.len() == outcome.len()
            && keyslot.is_some()
            && steps.iter().any(|step| {
                Some(*step) == keyslot.map(|k| format!("keyslot {k} tested")).as_deref()
            })
    } else {
        outcome.is_empty() && *reported == RESEAL_DECLINED
    };
    let prompts = |prompt: &str| result.console.matches(prompt).count();
    if !head_ok
        || !tail_ok
        || opened_with(opened, uuid) != Some(0)
        || prompts(KEY_PROMPT) != 3
        || prompts(RESEAL_PROMPT) != 1
    {
        return Err(format!(
            "{leg}: the recovered boot's lines were {lines:#?}: not {refusal}..., the cap, \
             nothing released, the codec's and cryptsetup's refusals at three key prompts, \
             keyslot 0 opened, the reseal {} and the volume opened on keyslot 0\n{}",
            if change.reseals() {
                "completed"
            } else {
                "declined"
            },
            show()
        ));
    }
    let post: Vec<&str> = td_boot_lines(&result.console)
        .into_iter()
        .filter(|line| line.starts_with(POST_CAP_PREFIX))
        .collect();
    let expected = format!("{POST_CAP_PREFIX}{post_cap} (device-bound) refused: policy refused: ");
    if !matches!(post.as_slice(), [line] if line.starts_with(&expected)) {
        return Err(format!(
            "{leg}: the post-cap check reported {post:#?}, not {expected}...\n{}",
            show()
        ));
    }
    for refused in [
        HALTED,
        "failed (",
        "no transition this boot",
        "reseal stopped",
    ] {
        // The wrong key's refusal quotes cryptsetup's failed open.
        if td_boot_lines(&result.console)
            .iter()
            .any(|line| line.contains(refused) && !line.starts_with(WRONG_KEY_PREFIX))
        {
            return Err(format!(
                "{leg}: the selector reported {refused:?}\n{}",
                show()
            ));
        }
    }
    Ok((steps, keyslot))
}

/// The header the first-boot transition leaves: keyslots 0 and 2, one
/// device-bound token, numbered 1, on keyslot 2.
fn converged() -> HeaderState {
    HeaderState {
        keyslots: vec![0, 2],
        tokens: vec![td_token(1, "device-bound", &[2])],
    }
}

fn td_token(number: u8, role: &str, keyslots: &[u8]) -> HeaderToken {
    HeaderToken {
        number,
        kind: "td-protector".into(),
        role: Some(role.into()),
        keyslots: keyslots.to_vec(),
    }
}

/// The header after each step of the first-boot transition, from the
/// installed one (after none).
fn transition_states() -> Vec<HeaderState> {
    let first_boot = td_token(0, "first-boot", &[1]);
    let bound = td_token(1, "device-bound", &[2]);
    let state = |keyslots: &[u8], tokens: &[HeaderToken]| HeaderState {
        keyslots: keyslots.to_vec(),
        tokens: tokens.to_vec(),
    };
    vec![
        state(&[0, 1], std::slice::from_ref(&first_boot)),
        state(&[0, 1], std::slice::from_ref(&first_boot)),
        state(&[0, 1, 2], std::slice::from_ref(&first_boot)),
        state(&[0, 1, 2], &[first_boot.clone(), bound.clone()]),
        state(&[0, 1, 2], &[first_boot, bound.clone()]),
        state(&[0, 2], &[td_token(0, "first-boot", &[]), bound.clone()]),
        state(&[0, 2], &[bound]),
    ]
}

/// Where a cut on a step's console line left the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    /// On that step's own state; steps 3 and 4 leave the same one.
    Landed,
    /// On a later step's state: the guest committed on before the kill.
    Late(usize),
    /// Before the step, or in no state of the plan.
    Unknown,
}

/// Where a cut on `step`'s console line left `state`.
fn cut_lands(state: &HeaderState, step: usize) -> Cut {
    let states = transition_states();
    if states.get(step) == Some(state) {
        return Cut::Landed;
    }
    states
        .iter()
        .enumerate()
        .skip(step + 1)
        .find(|(_, each)| *each == state)
        .map_or(Cut::Unknown, |(index, _)| Cut::Late(index))
}

/// How many times a cut that landed past its own commit is tried again.
const CUT_ATTEMPTS: usize = 3;

/// The selector kernel's command line as it printed it: the first
/// `Kernel command line:` notice, after any printk time stamp. The kernel
/// prints it once its built-in line and the firmware's load options,
/// which the EFI stub passes, are joined; the deployment kernel's comes
/// later.
fn selector_command_line(console: &str) -> Option<&str> {
    const LEAD: &str = "Kernel command line:";
    console.lines().find_map(|line| {
        line.find(LEAD)
            .and_then(|at| line.get(at + LEAD.len()..))
            .map(str::trim)
    })
}

/// The private file a boot's whole raw console is written to.
fn console_path(bench: &Bench, leg: &str) -> PathBuf {
    bench.scratch.dir.join(format!("{leg}.console"))
}

/// The whole raw console QEMU wrote to `path`, which is then removed: it
/// may hold what the scans look for.
fn take_console(path: &Path) -> Result<Vec<u8>, String> {
    let read = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()));
    match fs::remove_file(path) {
        Ok(()) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => read,
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

/// The raw capture must hold the end of the boot loop's bounded tail, so
/// the scans see every byte that tail came from: never a truncated
/// console. The tail's first line may be cut mid-character, so it is left
/// out.
fn require_whole(leg: &str, result: &BootResult, raw: &[u8]) -> Result<(), String> {
    let tail = result.console.trim_end();
    let mut start = tail.len().saturating_sub(4096);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    let end = tail.get(start..).unwrap_or_default();
    let end = end.split_once('\n').map_or(end, |(_, rest)| rest);
    if raw.is_empty() || !String::from_utf8_lossy(raw).contains(end) {
        return Err(format!(
            "{leg}: the raw console ({} bytes) does not hold the boot's console tail",
            raw.len()
        ));
    }
    Ok(())
}

/// The header the header-state leg builds on the converged one: an orphan
/// keyslot 1, the superseded device-bound token 2 on keyslot 3 and the
/// first-boot token 0 its killed keyslot left naming none.
fn constructed() -> HeaderState {
    HeaderState {
        keyslots: vec![0, 1, 2, 3],
        tokens: vec![
            td_token(0, "first-boot", &[]),
            td_token(1, "device-bound", &[2]),
            td_token(2, "device-bound", &[3]),
        ],
    }
}

/// The plan steps that retire the constructed states, in any order the
/// planner chose: the released keyslot tested, the orphan keyslot and the
/// superseded token's keyslot destroyed, and both tokens removed.
const RETIRING_STEPS: &[&str] = &[
    "keyslot 2 tested",
    "keyslot 1 destroyed, keyslot 2 authorizing",
    "keyslot 3 destroyed, keyslot 2 authorizing",
    "token 0 removed",
    "token 2 removed",
];

/// `transition step N/M: WHAT` lines, numbered in order from 1 to M, as
/// their `WHAT`s.
fn plan_steps<'a>(lines: &[&'a str]) -> Result<Vec<&'a str>, String> {
    let steps: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.strip_prefix("td-boot: transition step "))
        .collect();
    let count = steps.len();
    steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            step.strip_prefix(&format!("{}/{count}: ", index + 1))
                .ok_or_else(|| format!("transition steps out of order: {steps:?}"))
        })
        .collect()
}

/// A position-independent change to `\EFI\BOOT\BOOTX64.EFI`: its COFF
/// header's TimeDateStamp, which the firmware's image digest covers and
/// nothing reads.
fn change_pe_timestamp(image: &mut [u8]) -> Result<(), String> {
    let pe = image
        .get(0x3c..0x40)
        .and_then(|field| field.try_into().ok())
        .map(u32::from_le_bytes)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or("the selector image has no PE header offset")?;
    if image.get(..2) != Some(&b"MZ"[..]) || image.get(pe..pe + 4) != Some(&b"PE\0\0"[..]) {
        return Err("the selector image is not a PE image".into());
    }
    let stamp = image
        .get_mut(pe + 8..pe + 12)
        .ok_or("the selector image's COFF header is cut short")?;
    let current: [u8; 4] = (&*stamp)
        .try_into()
        .map_err(|_| "the selector image's COFF header is cut short")?;
    stamp.copy_from_slice(&u32::from_le_bytes(current).wrapping_add(1).to_le_bytes());
    Ok(())
}

/// A change to `\EFI\BOOT\INITRD` the unpacker ignores: the first newc
/// header's mtime, its last hexadecimal digit moved.
fn change_cpio_mtime(archive: &mut [u8]) -> Result<(), String> {
    if archive.get(..6) != Some(&b"070701"[..]) {
        return Err("the selector initramfs does not begin with a newc header".into());
    }
    // magic, ino, mode, uid, gid, nlink, then mtime: its last digit.
    let digit = archive
        .get_mut(6 + 8 * 5 + 7)
        .ok_or("the newc header is cut short")?;
    *digit = match *digit {
        b'0' => b'1',
        b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => b'0',
        _ => return Err("the newc mtime is not hexadecimal".into()),
    };
    Ok(())
}

fn le16(bytes: &[u8], at: usize) -> Result<u64, String> {
    bytes
        .get(at..at + 2)
        .and_then(|field| field.try_into().ok())
        .map(|field| u64::from(u16::from_le_bytes(field)))
        .ok_or_else(|| format!("short FAT field at {at}"))
}

fn le32(bytes: &[u8], at: usize) -> Result<u64, String> {
    bytes
        .get(at..at + 4)
        .and_then(|field| field.try_into().ok())
        .map(|field| u64::from(u32::from_le_bytes(field)))
        .ok_or_else(|| format!("short FAT field at {at}"))
}

/// An 8.3 name as a FAT directory entry spells it.
fn short_name(name: &str) -> Result<[u8; 11], String> {
    let (base, extension) = name.split_once('.').unwrap_or((name, ""));
    if base.is_empty() || base.len() > 8 || extension.len() > 3 {
        return Err(format!("{name} is not an 8.3 name"));
    }
    let mut entry = [b' '; 11];
    for (slot, byte) in entry.iter_mut().zip(base.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    for (slot, byte) in entry.iter_mut().skip(8).zip(extension.bytes()) {
        *slot = byte.to_ascii_uppercase();
    }
    Ok(entry)
}

/// The byte range of the file at `path` inside the FAT32 volume that
/// begins at `base` in `image`: through its BPB, the root directory, each
/// directory's entries along `path` and the FAT chain, which must be one
/// run, as td's writer allocates every file (td-engine `fat`).
fn fat_file(image: &File, base: u64, path: &[&str]) -> Result<(u64, u64), String> {
    let boot = encrypted::read_at(image, base, 512)?;
    let sector = le16(&boot, 11)?;
    let per_cluster = u64::from(*boot.get(13).ok_or("short boot sector")?);
    let reserved = le16(&boot, 14)?;
    let fats = u64::from(*boot.get(16).ok_or("short boot sector")?);
    let fat_sectors = le32(&boot, 36)?;
    let root = le32(&boot, 44)?;
    if boot.get(510..512) != Some(&[0x55, 0xaa][..]) || sector == 0 || per_cluster == 0 {
        return Err("the ESP holds no FAT32 boot sector".into());
    }
    let cluster_bytes = sector * per_cluster;
    let fat_at = base + reserved * sector;
    let data = fat_at + fats * fat_sectors * sector;
    let cluster_at = |cluster: u64| -> Result<u64, String> {
        cluster
            .checked_sub(2)
            .map(|index| data + index * cluster_bytes)
            .ok_or_else(|| format!("cluster {cluster} is not a data cluster"))
    };
    // A chain of at most `limit` clusters from `first`.
    let chain = |first: u64, limit: u64| -> Result<Vec<u64>, String> {
        let mut clusters = vec![first];
        let mut cluster = first;
        loop {
            let next = le32(&encrypted::read_at(image, fat_at + cluster * 4, 4)?, 0)? & 0x0fff_ffff;
            if next >= 0x0fff_fff8 {
                return Ok(clusters);
            }
            if next < 2 || clusters.len() as u64 >= limit {
                return Err(format!("a broken FAT chain from cluster {first}"));
            }
            clusters.push(next);
            cluster = next;
        }
    };
    let mut directory = root;
    let mut found = None;
    for (depth, component) in path.iter().enumerate() {
        let name = short_name(component)?;
        let mut entries = Vec::new();
        for cluster in chain(directory, 64)? {
            entries.extend(encrypted::read_at(
                image,
                cluster_at(cluster)?,
                cluster_bytes,
            )?);
        }
        let entry = entries
            .as_chunks::<32>()
            .0
            .iter()
            .take_while(|entry| entry.first() != Some(&0))
            .find(|entry| {
                entry.first() != Some(&0xe5)
                    && entry.get(11) != Some(&0x0f)
                    && entry.get(..11) == Some(&name[..])
            })
            .ok_or_else(|| format!("the ESP has no {}", path.join("\\")))?;
        let first = (le16(entry, 20)? << 16) | le16(entry, 26)?;
        let directory_entry = entry.get(11).is_some_and(|attr| attr & 0x10 != 0);
        if depth + 1 < path.len() {
            if !directory_entry {
                return Err(format!("{component} is not a directory on the ESP"));
            }
            directory = first;
        } else {
            found = Some((first, le32(entry, 28)?, directory_entry));
        }
    }
    let (first, size, directory_entry) = found.ok_or("an empty ESP path")?;
    if directory_entry || size == 0 {
        return Err(format!("{} is not a file on the ESP", path.join("\\")));
    }
    let clusters = chain(first, size.div_ceil(cluster_bytes))?;
    if clusters
        .iter()
        .zip(clusters.iter().skip(1))
        .any(|(a, b)| a + 1 != *b)
        || (clusters.len() as u64) * cluster_bytes < size
    {
        return Err(format!("{} is not one run of clusters", path.join("\\")));
    }
    Ok((cluster_at(first)?, size))
}

/// Rewrites one ESP file of the installed image at `path` with `change`
/// applied, in place: the same length, so its clusters do not move.
fn change_esp_file(
    path: &Path,
    file: &[&str],
    change: fn(&mut [u8]) -> Result<(), String>,
) -> Result<(), String> {
    use std::os::unix::fs::FileExt;
    let image = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let (offset, size) = fat_file(&image, td_boot_protocol::PARTITION_ALIGN_BYTES, file)?;
    let mut bytes = encrypted::read_at(&image, offset, size)?;
    let before = bytes.clone();
    change(&mut bytes)?;
    if bytes == before {
        return Err(format!("the change left {} as it was", file.join("\\")));
    }
    image
        .write_all_at(&bytes, offset)
        .and_then(|()| image.sync_all())
        .map_err(|error| format!("rewrite {}: {error}", file.join("\\")))
}

/// Both LUKS2 header copies' bytes, to show a leg left them unchanged.
fn header_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    encrypted::read_at(&file, encrypted::volume_offset()?, 2 * 16 * 1024)
}

/// What a boot adds beside its disk, TPM and marker: answers typed at the
/// console's prompts, and a firmware boot entry carrying `LOAD_OPTION`,
/// written into the boot's own fresh copy of the variables.
#[derive(Default)]
struct Extra<'b> {
    answers: Option<&'b ConsoleAnswers<'b>>,
    load_option: bool,
    /// The marker is a power cut (`BootPlan::cut`).
    cut: bool,
}

/// Everything the legs share once the installation is done.
struct Machine<'a> {
    bench: &'a Bench,
    tpm: &'a Path,
    /// The installed machine's TPM state, kept across its boots; a short
    /// private path, for the emulator's socket.
    state: Scratch,
    uuid: String,
    key: RecoveryKey,
    /// The whole raw consoles of every boot, the installation's first,
    /// for the inspection leg's search.
    consoles: Vec<u8>,
}

impl Machine<'_> {
    fn emulator(&self, state: &Path, case: &str) -> Result<secret::Emulator, String> {
        secret::Emulator::start(self.tpm, state, case)
    }

    /// Checks a boot's whole raw console, every byte as captured, for the
    /// recovery key in each form and for key-shaped text, keeps it for the
    /// inspection's volume-key search, and saves the tail redacted.
    fn keep(
        &mut self,
        runner: &RecipeCheckRunner,
        leg: &str,
        result: &BootResult,
        raw: &[u8],
    ) -> Result<(), String> {
        require_whole(leg, result, raw)?;
        if self.key.found_in(raw) {
            return Err(format!("{leg}: the console carries the recovery key"));
        }
        encrypted::require_no_key_text(&String::from_utf8_lossy(raw))?;
        self.consoles.extend_from_slice(raw);
        fs::write(
            runner
                .scratch_dir()
                .join(format!("boot-encrypted-{leg}.log")),
            encrypted::redacted(&result.console),
        )
        .map_err(|error| format!("save the {leg} console: {error}"))
    }

    /// One firmware boot of the installed `disk` under the TPM state in
    /// `state`, stopped at `marker`: a full system boot when `system`
    /// (its memory and sound device, and the account's serial shell running
    /// `steps`), the selector alone otherwise.
    #[allow(clippy::too_many_arguments)]
    fn boot_installed(
        &self,
        disk: &TargetDisk,
        state: &Path,
        leg: &str,
        marker: &str,
        system: bool,
        steps: Option<&[serial_shell::ShellStep]>,
        extra: Extra<'_>,
    ) -> Result<(BootResult, Vec<u8>), String> {
        let vars = self.bench.vars(leg)?;
        let console = console_path(self.bench, leg);
        if extra.load_option {
            efi::boot_entry(&vars, "td-oracle", LOAD_OPTION)?;
        }
        let emulator = self.emulator(state, leg)?;
        let mut plan = install::target_plan(disk, marker);
        plan.tpm_socket = Some(emulator.socket.as_path());
        if system {
            plan.mem = install::INSTALLED_SYSTEM_MEMORY_MIB;
            plan.audio = true;
        }
        plan.shell = steps;
        // A boot that types nothing ends at once on a recovery prompt
        // rather than wait out its deadline there.
        let unattended = ConsoleAnswers {
            answers: &[],
            refusal: KEY_PROMPT,
            line: AnswerLine::Serial,
        };
        plan.answers = Some(extra.answers.unwrap_or(&unattended));
        plan.cut = extra.cut;
        plan.keep_console = Some(console.as_path());
        println!("   [{LABEL}] {leg}");
        let booted = boot_source(
            &self.bench.qemu,
            BootSource::Firmware {
                code: &self.bench.code,
                vars: &vars,
                attachment: FirmwareAttachment::InstalledFixture,
                installation_target: None,
            },
            plan,
            &self.bench.scratch.dir,
            self.bench.timeout,
        )
        .map_err(|error| {
            encrypted::redacted(&emulator.diagnostic(&format!("{error}{}", firmware_tpm(&error))))
        });
        // The console goes whatever the boot did.
        let raw = take_console(&console);
        let result = booted?;
        let raw = raw?;
        emulator.finish()?;
        encrypted::remove(&vars)?;
        println!(
            "   [{LABEL}] {leg} elapsed: {:.2}s",
            result.elapsed.as_secs_f64()
        );
        Ok((result, raw))
    }

    fn installed(&self) -> Installed<'_> {
        Installed {
            uuid: &self.uuid,
            id: &self.bench.id,
            username: protocol::USERNAME,
            hostname: protocol::HOSTNAME,
            zone: Some(protocol::TIMEZONE_ID),
        }
    }
}

/// The legs in `LEGS`' order. Every disk is a fresh image in the bench's
/// scratch; the interrupted legs boot copies of the disk as installed, and
/// the header-state and changed legs copies of it after its second boot.
pub(crate) fn run(runner: &RecipeCheckRunner, tpm: &Path) -> Result<(), String> {
    secret::verify_swtpm(tpm)?;
    let bench = Bench::new(runner)?;
    let started = Instant::now();
    let mut installed: Option<(TargetDisk, TargetDisk)> = None;
    let mut machine: Option<Machine<'_>> = None;
    let mut summary = Vec::new();
    for leg in LEGS {
        let leg_started = Instant::now();
        let outcome = match (*leg, machine.as_mut(), installed.as_ref()) {
            (Leg::Install, _, _) => {
                let (disk, made) = install_leg(runner, &bench, tpm)?;
                let pristine = disk.copy_to(&bench.scratch.dir, "pristine.img")?;
                installed = Some((disk, pristine));
                machine = Some(made);
                "installed device-bound, the key typed back".to_string()
            }
            (Leg::Wizard, _, _) => wizard(runner, &bench, tpm)?,
            (Leg::FirstBoot, Some(machine), Some((disk, _))) => {
                first_boot(runner, machine, disk, "first-boot")?
            }
            (Leg::SecondBoot, Some(machine), Some((disk, _))) => {
                second_boot(runner, machine, disk, "second-boot")?
            }
            (Leg::Interrupted(commit), Some(machine), Some((_, pristine))) => {
                interrupted(runner, machine, pristine, commit)?
            }
            (Leg::Live, Some(machine), Some((disk, _))) => live(runner, machine, disk)?,
            (Leg::Headers, Some(machine), Some((disk, _))) => headers(runner, machine, disk)?,
            (Leg::Changed(change), Some(machine), Some((disk, _))) => {
                changed(runner, machine, disk, change)?
            }
            (Leg::Inspect, Some(machine), Some((disk, _))) => inspect(runner, machine, disk)?,
            (leg, _, _) => return Err(format!("{leg:?} ran before the installation")),
        };
        println!(
            "   [{LABEL}] {leg:?} passed in {:.2}s: {outcome}",
            leg_started.elapsed().as_secs_f64()
        );
        summary.push(format!("{leg:?}: {outcome}"));
    }
    println!(
        "PASS: {} legs in {:.0}s under the pinned swtpm; {}",
        LEGS.len(),
        started.elapsed().as_secs_f64(),
        summary.join("; ")
    );
    Ok(())
}

/// The face td-setup draws the recovery key in: the image's outline face.
const FACE_RECIPE: &str = "jetbrains-mono-nerd-font";
/// Typed into the wizard's time zone row, which seeks the first zone it
/// begins: `protocol::TIMEZONE_ID`.
const ZONE_SEEK: &str = "europe/lon";

/// The default wizard's leg (ENCRYPTION.md "Acceptance evidence",
/// increment 7): the production live medium over USB on a machine with the
/// swtpm, a display device and the PS/2 keyboard, its wizard driven with
/// physical keys to a review naming device-bound storage and consent to
/// the same; the recovery key read only from the completion page's pixels
/// and typed back; then the installed disk's first and second boots with
/// nothing typed, under the same TPM state.
fn wizard(runner: &RecipeCheckRunner, bench: &Bench, tpm: &Path) -> Result<String, String> {
    runner.prepare_recipe_target(FACE_RECIPE)?;
    let face_out = runner.build_plan(FACE_RECIPE)?;
    let face_dir = runner
        .ladder_out_from(&face_out, FACE_RECIPE)?
        .join(td_recipe::catalog::outline_face::DIR);
    let glyphs = std::rc::Rc::new(KeyGlyphs::render(&crate::face_file::read(
        &face_dir,
        crate::face_file::REGULAR,
    )?)?);
    let key: KeyCell = std::rc::Rc::default();
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let state = Scratch {
        dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
    };
    let dir = bench.scratch.dir.join("wizard");
    fs::create_dir(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let medium =
        build_iso::live_medium(&bench.selector, &bench.store_deployment, &dir, &bench.trust)?;
    if medium.id != bench.id {
        return Err(format!(
            "the live medium's deployment {} is not the bench's {}",
            medium.id, bench.id
        ));
    }
    let iso = dir.join("live.iso");
    media::write_image_with_payloads(&iso, &bench.kernel, &medium.selector, &medium.payloads)?;
    let disk = TargetDisk::with_capacity(&bench.scratch.dir, "wizard.img", bench.capacity)?;
    let vars = bench.vars("wizard")?;
    let console = console_path(bench, "wizard");
    let emulator = secret::Emulator::start(tpm, &state.dir, "wizard")?;
    let wizard = live::Wizard {
        username: protocol::USERNAME,
        hostname: protocol::HOSTNAME,
        zone_seek: ZONE_SEEK,
        zone: protocol::TIMEZONE_ID,
        storage: live::WizardStorage::DeviceBound {
            glyphs,
            key: std::rc::Rc::clone(&key),
        },
    };
    println!("   [{LABEL}] the default wizard under swtpm, with a display and keyboard");
    let driven = live::drive_wizard(&live::LiveRun {
        qemu: &bench.qemu,
        code: &bench.code,
        vars: &vars,
        iso: &iso,
        target: &disk,
        capacity: bench.capacity,
        id: &medium.id,
        wizard: &wizard,
        tpm_socket: Some(emulator.socket.as_path()),
        keep_console: Some(&console),
        scratch: &bench.scratch.dir,
        label: LABEL,
    })
    .map_err(|error| encrypted::redacted(&emulator.diagnostic(&error)));
    drop(wizard);
    let raw = take_console(&console);
    let result = driven?;
    let raw = raw?;
    emulator.finish()?;
    encrypted::remove(&iso)?;
    encrypted::remove(&vars)?;
    let digits = key
        .borrow_mut()
        .take()
        .ok_or("the wizard completed with no key read")?;
    let key = RecoveryKey(digits);
    require_whole("wizard", &result, &raw)?;
    if key.found_in(&raw) {
        return Err("the live session's console carries the recovery key".into());
    }
    encrypted::require_no_key_text(&String::from_utf8_lossy(&raw))?;
    fs::write(
        runner.scratch_dir().join("boot-encrypted-wizard.log"),
        encrypted::redacted(&result.console),
    )
    .map_err(|error| format!("save the wizard's console: {error}"))?;
    let uuid = encrypted::luks_uuid(&disk.path)?;
    let image = encrypted::verify_image(&disk.path, 512, &uuid, true)?;
    let mut machine = Machine {
        bench,
        tpm,
        state,
        uuid,
        key,
        consoles: raw,
    };
    let first = first_boot(runner, &mut machine, &disk, "wizard-first-boot")?;
    let second = second_boot(runner, &mut machine, &disk, "wizard-second-boot")?;
    encrypted::remove(&disk.path)?;
    Ok(format!(
        "the wizard reviewed device-bound storage and consented to it, the key read from the \
         page's pixels and typed back, LUKS2 volume {} with {} data bytes; {first}; {second}",
        machine.uuid, image.data_bytes
    ))
}

/// The installation, as `qemu-install-encrypted`'s installed leg checks it,
/// under the machine's TPM state, the key typed back then sent to the host
/// over the private serial port. Returns the disk and the machine.
fn install_leg<'a>(
    runner: &RecipeCheckRunner,
    bench: &'a Bench,
    tpm: &'a Path,
) -> Result<(TargetDisk, Machine<'a>), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let state = Scratch {
        dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
    };
    let iso = bench.medium("install-encrypted-key", Vec::new())?;
    let disk = TargetDisk::with_capacity(&bench.scratch.dir, "installed.img", bench.capacity)?;
    encrypted::seed_table(&disk.path, bench.capacity)?;
    let vars = bench.vars("install")?;
    let channel = state.dir.join("key-channel");
    let console = console_path(bench, "install");
    let emulator = secret::Emulator::start(tpm, &state.dir, "install")?;
    println!(
        "   [{LABEL}] installing {} bytes device-bound under swtpm",
        bench.payload_bytes
    );
    let booted = bench
        .host()
        .boot_with(
            &iso,
            &vars,
            &disk,
            protocol::ENCRYPTED_END_MARKER,
            Some(emulator.socket.as_path()),
            Some(&channel),
            Some(&console),
            Devices::ALL,
        )
        .map_err(|error| encrypted::redacted(&emulator.diagnostic(&error)));
    // The channel and the console are read and removed whatever the boot
    // did.
    let sent = install::read(&channel);
    encrypted::remove(&channel)?;
    let raw = take_console(&console);
    let result = booted?;
    let raw = raw?;
    let key = RecoveryKey::from_channel(sent?)?;
    require_whole("install", &result, &raw)?;
    if key.found_in(&raw) {
        return Err("the installation console carries the recovery key".into());
    }
    encrypted::require_no_key_text(&String::from_utf8_lossy(&raw))?;
    let (uuid, mapped) = encrypted::validate_installation(&result, bench.capacity, &bench.id)
        .map_err(|error| encrypted::redacted(&emulator.diagnostic(&error)))?;
    let sent_lines = result
        .console
        .lines()
        .filter(|line| line.trim_end() == protocol::ENCRYPTED_KEY_SENT_MARKER)
        .count();
    if sent_lines != 1 {
        return Err("the installation did not report the key sent once".into());
    }
    emulator.finish()?;
    encrypted::remove(&iso)?;
    encrypted::remove(&vars)?;
    fs::write(
        runner.scratch_dir().join("boot-encrypted-install.log"),
        encrypted::redacted(&result.console),
    )
    .map_err(|error| format!("save the installation console: {error}"))?;
    let image = encrypted::verify_image(&disk.path, 512, &uuid, true)?;
    if mapped != image.data_bytes {
        return Err(format!(
            "the guest scanned {mapped} bytes of the opened volume; its data segment is {}",
            image.data_bytes
        ));
    }
    println!(
        "   [{LABEL}] installation elapsed: {:.2}s; volume {uuid}",
        result.elapsed.as_secs_f64()
    );
    Ok((
        disk,
        Machine {
            bench,
            tpm,
            state,
            uuid,
            key,
            consoles: raw,
        },
    ))
}

/// What a boot's console says of the firmware's TPM when it failed: OVMF
/// that has disabled its TPM exports no event log, and the EFI stub's
/// measurement fails with EFI_DEVICE_ERROR. Nothing then measured the
/// selector, so release refuses for a reason outside td.
fn firmware_tpm(console: &str) -> &'static str {
    if console.contains("EFI stub: WARNING: Failed to measure data for event") {
        "\n(the firmware's TPM failed before the selector ran: the EFI stub could not \
         measure, so PCRs 4 and 9 hold no selector measurement; this is the firmware's \
         TPM, not td's release)"
    } else {
        ""
    }
}

/// The installed volume's partition as the guest names it.
fn volume_device(disk: &TargetDisk) -> String {
    format!("/dev/{}", partition_name(disk.bus.name(false), 2))
}

/// The nonce the first boot's serial shell writes into the account's home,
/// as `cold_boots` names it.
fn nonce(uuid: &str) -> String {
    uuid.replace('-', "")
}

fn first_boot(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    disk: &TargetDisk,
    leg: &str,
) -> Result<String, String> {
    let nonce = nonce(&machine.uuid);
    let installed = machine.installed();
    let steps = install::session_steps(&installed, &nonce, None)?;
    let (result, result_raw) = machine.boot_installed(
        disk,
        &machine.state.dir,
        leg,
        SYSTEM_BOOT_SUCCESS_MARKER,
        true,
        Some(&steps),
        Extra::default(),
    )?;
    machine.keep(runner, leg, &result, &result_raw)?;
    let mut expected = vec![FIRST_BOOT_RELEASED, CAP_CLOSED];
    expected.extend_from_slice(FIRST_BOOT_STEPS);
    require_release(
        &result,
        &machine.uuid,
        &expected,
        2,
        &[(1, "device-bound")],
        "first boot",
    )?;
    install::validate_installed_as(&result, &volume_device(disk), &machine.installed(), true)?;
    install::reported_home_inode(&result.console, &nonce)?;
    let state = encrypted::image_header(&disk.path, &machine.uuid)?;
    if state != converged() {
        return Err(format!(
            "the first boot left {}, not the converged header",
            state.describe()
        ));
    }
    Ok(format!(
        "first-boot protector released, device-bound sealed and transitioned in 6 commits, \
         post-cap unseal refused by policy, system acknowledged, {}",
        state.describe()
    ))
}

fn second_boot(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    disk: &TargetDisk,
    leg: &str,
) -> Result<String, String> {
    let nonce = nonce(&machine.uuid);
    let kept = install::reported_home_inode(&String::from_utf8_lossy(&machine.consoles), &nonce)?;
    let installed = machine.installed();
    let steps = install::session_steps(&installed, &nonce, Some(&kept))?;
    let (result, result_raw) = machine.boot_installed(
        disk,
        &machine.state.dir,
        leg,
        SYSTEM_BOOT_SUCCESS_MARKER,
        true,
        Some(&steps),
        Extra::default(),
    )?;
    machine.keep(runner, leg, &result, &result_raw)?;
    require_release(
        &result,
        &machine.uuid,
        &[
            DEVICE_BOUND_RELEASED,
            CAP_CLOSED,
            "td-boot: transition step 1/1: keyslot 2 tested",
        ],
        2,
        &[(1, "device-bound")],
        "second boot",
    )?;
    install::validate_installed_as(&result, &volume_device(disk), &machine.installed(), false)?;
    let state = encrypted::image_header(&disk.path, &machine.uuid)?;
    if state != converged() {
        return Err(format!(
            "the second boot changed the header to {}",
            state.describe()
        ));
    }
    let data = encrypted::installed_ciphertext(&disk.path, &[nonce.as_bytes()])?;
    Ok(format!(
        "device-bound protector released with no interaction, the home's file read back; \
         {data} ciphertext bytes hold no plaintext marker or the file's nonce"
    ))
}

fn interrupted(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    pristine: &TargetDisk,
    commit: Commit,
) -> Result<String, String> {
    let name = format!("cut-{}", commit.name());
    let mut misses = Vec::new();
    let mut landed = None;
    for attempt in 1..=CUT_ATTEMPTS {
        let leg = format!("{name}-{attempt}");
        let disk = pristine.copy_to(&machine.bench.scratch.dir, &format!("{leg}.img"))?;
        let (result, result_raw) = machine.boot_installed(
            &disk,
            &machine.state.dir,
            &leg,
            commit.line(),
            false,
            None,
            Extra {
                cut: true,
                ..Extra::default()
            },
        )?;
        machine.keep(runner, &leg, &result, &result_raw)?;
        // The commit was reported, so the cut landed after it; the lines
        // that reached the console before the cut are the plan's, in order.
        let lines = release_lines(&result.console, &|line| line == commit.line())
            .ok_or_else(|| format!("{leg}: the commit line never arrived"))?;
        let mut expected = vec![FIRST_BOOT_RELEASED, CAP_CLOSED];
        expected.extend_from_slice(FIRST_BOOT_STEPS.get(..commit.step()).unwrap_or_default());
        if lines != expected || !result.marker_killed {
            return Err(format!(
                "{leg}: the cut boot reported {lines:#?}, not {expected:#?}, or was not cut"
            ));
        }
        let (left, torn) = encrypted::image_header_after_cut(&disk.path, &machine.uuid)?;
        match cut_lands(&left, commit.step()) {
            Cut::Landed => {
                landed = Some((leg, disk, left, torn));
                break;
            }
            // Nothing was interrupted at this commit: a miss, tried again.
            Cut::Late(step) => {
                misses.push(format!("attempt {attempt} landed after step {step}"));
                retire_copy(runner, machine, &disk, &leg)?;
            }
            Cut::Unknown => {
                return Err(format!(
                    "{leg}: the cut left {}, which is no state at or after step {}",
                    left.describe(),
                    commit.step()
                ))
            }
        }
    }
    let (leg, disk, left, torn) = landed.ok_or_else(|| {
        format!(
            "{name}: no cut in {CUT_ATTEMPTS} attempts left step {}'s own state: {}",
            commit.step(),
            misses.join("; ")
        )
    })?;
    // The next boot completes what the cut left, and boots the system.
    let resumed = format!("{leg}-resumed");
    let (completed, completed_raw) = machine.boot_installed(
        &disk,
        &machine.state.dir,
        &resumed,
        SYSTEM_BOOT_SUCCESS_MARKER,
        true,
        None,
        Extra::default(),
    )?;
    machine.keep(runner, &resumed, &completed, &completed_raw)?;
    let lines = release_lines(&completed.console, &|line| {
        opened_with(line, &machine.uuid).is_some()
    })
    .ok_or_else(|| format!("{name}: the resumed boot never opened the volume"))?;
    let steps = plan_steps(&lines)?;
    let opened = lines
        .last()
        .and_then(|line| opened_with(line, &machine.uuid));
    let header = encrypted::image_header(&disk.path, &machine.uuid)?;
    let finished = header.keyslots.len() == 2
        && header.keyslots.first() == Some(&0)
        && matches!(header.tokens.as_slice(), [token]
            if token.kind == "td-protector"
                && token.role.as_deref() == Some("device-bound")
                && token.keyslots.len() == 1
                && token.keyslots.first() == header.keyslots.get(1)
                && opened == header.keyslots.get(1).copied());
    let failed = td_boot_lines(&completed.console)
        .iter()
        .any(|line| line.contains("failed (") || line.contains(HALTED));
    if !finished || failed || !completed.evidence.boot_success {
        return Err(format!(
            "{name}: the resumed boot left {} (opened with {opened:?}) or reported a failure\n{}",
            header.describe(),
            encrypted::redacted(&tail(&completed.console, 160))
        ));
    }
    let searched = retire_copy(runner, machine, &disk, &leg)?;
    Ok(format!(
        "cut on step {}'s own state ({}{}{}); the next boot ran {} steps [{}] and booted the \
         system on {}; {searched}",
        commit.step(),
        left.describe(),
        torn.map(|note| format!("; {note}")).unwrap_or_default(),
        if misses.is_empty() {
            String::new()
        } else {
            format!("; after {} late: {}", misses.len(), misses.join(", "))
        },
        steps.len(),
        steps.join("; "),
        header.describe()
    ))
}

fn live(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    disk: &TargetDisk,
) -> Result<String, String> {
    let before = header_bytes(&disk.path)?;
    let dir = machine.bench.scratch.dir.join("live");
    fs::create_dir(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let medium = build_iso::live_medium(
        &machine.bench.selector,
        &machine.bench.store_deployment,
        &dir,
        &machine.bench.trust,
    )?;
    let iso = dir.join("live.iso");
    media::write_image_with_payloads(
        &iso,
        &machine.bench.kernel,
        &medium.selector,
        &medium.payloads,
    )?;
    let vars = machine.bench.vars("live")?;
    let emulator = machine.emulator(&machine.state.dir, "live")?;
    let console = console_path(machine.bench, "live");
    let mut plan = install::plan(&iso, true, LIVE_DEPLOYMENT_PREFIX);
    plan.tpm_socket = Some(emulator.socket.as_path());
    plan.keep_console = Some(console.as_path());
    println!("   [{LABEL}] the live medium on the installed machine");
    let booted = boot_source(
        &machine.bench.qemu,
        BootSource::Firmware {
            code: &machine.bench.code,
            vars: &vars,
            attachment: FirmwareAttachment::Usb,
            installation_target: Some(disk),
        },
        plan,
        &machine.bench.scratch.dir,
        machine.bench.timeout,
    )
    .map_err(|error| encrypted::redacted(&emulator.diagnostic(&error)));
    let raw = take_console(&console);
    let result = booted?;
    let raw = raw?;
    emulator.finish()?;
    machine.keep(runner, "live", &result, &raw)?;
    let lines = td_boot_lines(&result.console);
    let capped = matches!(lines.as_slice(), [capping, closed, ..]
        if *capping == LIVE_CAPPING && *closed == CAP_CLOSED);
    let handed = lines
        .iter()
        .any(|line| line.starts_with(&format!("{LIVE_DEPLOYMENT_PREFIX}{} ", medium.id)));
    let released = lines.iter().any(|line| {
        line.contains(" released")
            || line.contains("opened with keyslot")
            || line.contains("post-cap")
    });
    if !capped || !handed || released || !result.marker_killed {
        return Err(format!(
            "the live boot's lines were {lines:#?}: not the cap closed before its deployment \
             {} with nothing released\n{}",
            medium.id,
            encrypted::redacted(&tail(&result.console, 160))
        ));
    }
    if header_bytes(&disk.path)? != before {
        return Err("the live boot changed the installed volume's header".into());
    }
    encrypted::remove(&vars)?;
    fs::remove_dir_all(&dir).map_err(|error| format!("remove {}: {error}", dir.display()))?;
    Ok(format!(
        "live selector capped PCR 12 before booting deployment {}; no td token released and the \
         installed header unchanged",
        medium.id
    ))
}

fn headers(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    installed: &TargetDisk,
) -> Result<String, String> {
    let disk = installed.copy_to(&machine.bench.scratch.dir, "headers.img")?;
    let iso = machine.bench.medium(
        "encrypted-headers",
        vec![(
            protocol::ORACLE_RECOVERY_KEY.into(),
            0o400,
            machine.key.digits().to_vec(),
        )],
    )?;
    let vars = machine.bench.vars("headers-guest")?;
    let console = console_path(machine.bench, "headers-guest");
    let emulator = machine.emulator(&machine.state.dir, "headers-guest")?;
    println!("   [{LABEL}] a guest builds orphan and superseded header states");
    let booted = machine.bench.host().boot_with(
        &iso,
        &vars,
        &disk,
        protocol::ENCRYPTED_END_MARKER,
        Some(emulator.socket.as_path()),
        None,
        Some(&console),
        Devices::ALL,
    );
    // The medium carried the key: it goes whatever the boot did.
    encrypted::remove(&iso)?;
    let raw = take_console(&console);
    let result = booted.map_err(|error| encrypted::redacted(&emulator.diagnostic(&error)))?;
    let raw = raw?;
    emulator.finish()?;
    encrypted::remove(&vars)?;
    machine.keep(runner, "headers-guest", &result, &raw)?;
    let built = result
        .console
        .lines()
        .filter(|line| line.starts_with(protocol::ENCRYPTED_HEADERS_MARKER))
        .count();
    if built != 1 || !result.evidence.target {
        return Err(format!(
            "the header guest did not build its states\n{}",
            encrypted::redacted(&tail(&result.console, 160))
        ));
    }
    let state = encrypted::image_header(&disk.path, &machine.uuid)?;
    if state != constructed() {
        return Err(format!("the guest built {}", state.describe()));
    }
    let (booted, booted_raw) = machine.boot_installed(
        &disk,
        &machine.state.dir,
        "headers-boot",
        SYSTEM_BOOT_SUCCESS_MARKER,
        true,
        None,
        Extra::default(),
    )?;
    machine.keep(runner, "headers-boot", &booted, &booted_raw)?;
    let lines = release_lines(&booted.console, &|line| {
        opened_with(line, &machine.uuid).is_some()
    })
    .ok_or("the header-state boot never opened the volume")?;
    let steps = plan_steps(&lines)?;
    let mut sorted = steps.clone();
    sorted.sort_unstable();
    let mut retiring = RETIRING_STEPS.to_vec();
    retiring.sort_unstable();
    let released = lines.first() == Some(&DEVICE_BOUND_RELEASED)
        && lines.contains(&"td-boot: td token 2 (device-bound) released")
        && steps.first() == Some(&"keyslot 2 tested");
    let after = encrypted::image_header(&disk.path, &machine.uuid)?;
    if sorted != retiring || !released || after != converged() || !booted.evidence.boot_success {
        return Err(format!(
            "the header-state boot ran {steps:?} from {lines:#?} and left {}\n{}",
            after.describe(),
            encrypted::redacted(&tail(&booted.console, 160))
        ));
    }
    let searched = retire_copy(runner, machine, &disk, "headers")?;
    Ok(format!(
        "built {}; the next boot released tokens 1 and 2, ran [{}] and converged on {}; \
         {searched}",
        state.describe(),
        steps.join("; "),
        after.describe()
    ))
}

fn changed(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    installed: &TargetDisk,
    change: Change,
) -> Result<String, String> {
    let name = change.name();
    let disk = installed.copy_to(&machine.bench.scratch.dir, &format!("{name}.img"))?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut fresh = None;
    match change {
        Change::SelectorImage => change_esp_file(
            &disk.path,
            &["EFI", "BOOT", "BOOTX64.EFI"],
            change_pe_timestamp,
        )?,
        Change::Initramfs => {
            change_esp_file(&disk.path, &["EFI", "BOOT", "INITRD"], change_cpio_mtime)?
        }
        Change::LoadOption => {}
        Change::FreshTpm => {
            fresh = Some(Scratch {
                dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
            });
        }
    }
    let load_option = change == Change::LoadOption;
    let before = header_bytes(&disk.path)?;
    let state = fresh
        .as_ref()
        .map_or(machine.state.dir.as_path(), |fresh| fresh.dir.as_path())
        .to_path_buf();
    let answers = recovery_answers(&machine.key, change.reseals())?;
    let typed = ConsoleAnswers {
        answers: &answers,
        refusal: HALTED,
        line: change.line(),
    };
    let (recovered, recovered_raw) = machine.boot_installed(
        &disk,
        &state,
        name,
        SYSTEM_BOOT_SUCCESS_MARKER,
        true,
        None,
        Extra {
            answers: Some(&typed),
            load_option,
            cut: false,
        },
    )?;
    drop(answers);
    machine.keep(runner, name, &recovered, &recovered_raw)?;
    // The refusal is tied to the change: the selector kernel's printed
    // command line carries the load option exactly when this leg wrote one.
    let console = String::from_utf8_lossy(&recovered_raw);
    let command_line = selector_command_line(&console)
        .ok_or_else(|| format!("{name}: the selector kernel printed no command line"))?;
    if command_line
        .split_ascii_whitespace()
        .any(|token| token == LOAD_OPTION)
        != load_option
    {
        return Err(format!(
            "{name}: the selector kernel's command line {command_line:?} does not carry \
             {LOAD_OPTION} exactly when the leg wrote it"
        ));
    }
    install::validate_installed_as(
        &recovered,
        &volume_device(&disk),
        &machine.installed(),
        false,
    )?;
    let after = encrypted::image_header(&disk.path, &machine.uuid)?;
    if !change.reseals() {
        require_recovered(&recovered, &machine.uuid, change, 1)?;
        if header_bytes(&disk.path)? != before {
            return Err(format!("{name}: the declined reseal changed the header"));
        }
        let searched = retire_copy(runner, machine, &disk, name)?;
        return Ok(format!(
            "device-bound token {}, cap closed, nothing released; at recovery a malformed \
             entry and a wrong key re-prompted, the key opened keyslot 0, the reseal was \
             declined and the system booted with the header unchanged; {searched}",
            change.refusal().trim_end_matches(": ")
        ));
    }
    // The reseal leaves keyslot 0 and one device-bound token on a new
    // keyslot, which the next boot of the same chain and TPM releases.
    let (token, keyslot) = match (after.keyslots.as_slice(), after.tokens.as_slice()) {
        ([0, keyslot], [token])
            if token.kind == "td-protector"
                && token.role.as_deref() == Some("device-bound")
                && token.keyslots == [*keyslot]
                && *keyslot != 2 =>
        {
            (token.number, *keyslot)
        }
        _ => {
            return Err(format!(
                "{name}: the reseal left {}, not keyslot 0 and one new device-bound protector",
                after.describe()
            ))
        }
    };
    let (steps, reported) = require_recovered(&recovered, &machine.uuid, change, token)?;
    if reported != Some(keyslot) {
        return Err(format!(
            "{name}: the reseal reported keyslot {reported:?}; the header holds {keyslot}"
        ));
    }
    let next = format!("{name}-resealed");
    let (unattended, unattended_raw) = machine.boot_installed(
        &disk,
        &state,
        &next,
        SYSTEM_BOOT_SUCCESS_MARKER,
        true,
        None,
        Extra {
            answers: None,
            cut: false,
            load_option,
        },
    )?;
    machine.keep(runner, &next, &unattended, &unattended_raw)?;
    let released = format!("td-boot: td token {token} (device-bound) released");
    let tested = format!("td-boot: transition step 1/1: keyslot {keyslot} tested");
    require_release(
        &unattended,
        &machine.uuid,
        &[released.as_str(), CAP_CLOSED, tested.as_str()],
        keyslot,
        &[(token, "device-bound")],
        &next,
    )?;
    install::validate_installed_as(
        &unattended,
        &volume_device(&disk),
        &machine.installed(),
        false,
    )?;
    if encrypted::image_header(&disk.path, &machine.uuid)? != after {
        return Err(format!(
            "{next}: the released boot changed the resealed header"
        ));
    }
    let searched = retire_copy(runner, machine, &disk, name)?;
    Ok(format!(
        "device-bound token {}, cap closed, nothing released; at recovery a malformed entry \
         and a wrong key re-prompted, the key opened keyslot 0 and the confirmed reseal ran \
         [{}] onto {}; the next boot released token {token} with no interaction; {searched}",
        change.refusal().trim_end_matches(": "),
        steps.join("; "),
        after.describe()
    ))
}

/// A leg's disk copy searched as the inspection searches the disk, before
/// it is removed: a key a regression left on a copy fails here.
fn retire_copy(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    disk: &TargetDisk,
    leg: &str,
) -> Result<String, String> {
    let (whole, mapped, _) = inspect_disk(runner, machine, disk, &format!("{leg}-inspect"), false)?;
    encrypted::remove(&disk.path)?;
    Ok(format!(
        "neither key on its whole copy ({whole} bytes) or in its plaintext ({mapped})"
    ))
}

fn inspect(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    disk: &TargetDisk,
) -> Result<String, String> {
    let (whole, mapped, searched) = inspect_disk(runner, machine, disk, "inspect", true)?;
    Ok(format!(
        "the volume key (raw and hexadecimal) and the recovery key are absent from the whole \
         disk ({whole} bytes), its plaintext ({mapped}) and the {searched} raw bytes of every \
         console kept, the installation's included"
    ))
}

/// The `encrypted-inspect` guest over `disk`: the volume key, taken with
/// the recovery key into the guest's RAM, and the recovery key must be
/// nowhere on the whole disk or in the opened volume's plaintext and, with
/// `artifacts`, in every raw console kept so far. Returns the byte counts
/// the guest searched: the disk, the plaintext, the consoles.
fn inspect_disk(
    runner: &RecipeCheckRunner,
    machine: &mut Machine<'_>,
    disk: &TargetDisk,
    leg: &str,
    artifacts: bool,
) -> Result<(u64, u64, u64), String> {
    let mut files = vec![(
        protocol::ORACLE_RECOVERY_KEY.into(),
        0o400,
        machine.key.digits().to_vec(),
    )];
    let mut length = 0;
    if artifacts {
        let consoles = std::mem::take(&mut machine.consoles);
        length = consoles.len();
        files.push((protocol::ORACLE_ARTIFACTS.into(), 0o400, consoles));
    }
    let iso = machine.bench.medium("encrypted-inspect", files)?;
    let vars = machine.bench.vars(leg)?;
    let console = console_path(machine.bench, leg);
    println!(
        "   [{LABEL}] {leg}: searching the disk, the plaintext and {length} console bytes for \
         the keys"
    );
    let booted = machine.bench.host().boot_with(
        &iso,
        &vars,
        disk,
        protocol::ENCRYPTED_END_MARKER,
        None,
        None,
        Some(&console),
        Devices::ALL,
    );
    encrypted::remove(&iso)?;
    let raw = take_console(&console);
    let result = booted?;
    let raw = raw?;
    encrypted::remove(&vars)?;
    machine.keep(runner, leg, &result, &raw)?;
    let lead = format!("{} ", protocol::ENCRYPTED_SECRETS_ABSENT_MARKER);
    let records: Vec<&str> = result
        .console
        .lines()
        .filter_map(|line| line.trim_end().strip_prefix(&lead))
        .collect();
    let counts: Vec<u64> = match records.as_slice() {
        [record] => record
            .split(' ')
            .map(|count| {
                count
                    .parse::<u64>()
                    .map_err(|_| format!("malformed {record:?}"))
            })
            .collect::<Result<_, _>>()?,
        _ => {
            return Err(format!(
                "{leg}: the inspection did not report once\n{}",
                encrypted::redacted(&tail(&result.console, 160))
            ))
        }
    };
    let data = encrypted::installed_ciphertext(&disk.path, &[])?;
    match counts.as_slice() {
        [whole, mapped, searched]
            if *whole == machine.bench.capacity
                && *mapped == data
                && *searched == length as u64 =>
        {
            Ok((*whole, *mapped, *searched))
        }
        _ => Err(format!(
            "{leg}: the inspection read {counts:?}, not the disk's {}, the data segment's \
             {data} and the {length} console bytes",
            machine.bench.capacity
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "5a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a";

    #[test]
    fn options_need_an_absolute_emulator_or_nothing() {
        assert_eq!(options(&[]).unwrap(), None);
        assert_eq!(
            options(&["--tpm".into(), "/tmp/swtpm".into()]).unwrap(),
            Some(PathBuf::from("/tmp/swtpm"))
        );
        for args in [
            vec!["--tpm"],
            vec!["--tpm", "swtpm"],
            vec!["x", "/tmp/swtpm"],
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_string).collect();
            assert!(options(&args).is_err());
        }
    }

    /// The installation comes first and the inspection last; the second
    /// boot follows the first; every commit and change runs once.
    #[test]
    fn the_leg_plan_runs_each_leg_once_in_order() {
        assert_eq!(LEGS.first(), Some(&Leg::Wizard));
        assert_eq!(LEGS.get(1), Some(&Leg::Install));
        assert_eq!(LEGS.last(), Some(&Leg::Inspect));
        let at = |leg: Leg| LEGS.iter().position(|each| *each == leg).unwrap();
        assert!(at(Leg::FirstBoot) < at(Leg::SecondBoot));
        for later in [Leg::Live, Leg::Headers, Leg::Changed(Change::FreshTpm)] {
            assert!(at(Leg::SecondBoot) < at(later));
        }
        for commit in [
            Commit::Keyslot,
            Commit::Token,
            Commit::Test,
            Commit::Destroy,
        ] {
            assert_eq!(
                LEGS.iter()
                    .filter(|leg| **leg == Leg::Interrupted(commit))
                    .count(),
                1
            );
        }
        for change in [
            Change::SelectorImage,
            Change::Initramfs,
            Change::LoadOption,
            Change::FreshTpm,
        ] {
            assert_eq!(
                LEGS.iter()
                    .filter(|leg| **leg == Leg::Changed(change))
                    .count(),
                1
            );
        }
        assert_eq!(LEGS.len(), 15);
        // The changed initramfs answers recovery on the VT, every other
        // changed chain on the serial line.
        for change in [Change::SelectorImage, Change::LoadOption, Change::FreshTpm] {
            assert_eq!(change.line(), AnswerLine::Serial);
        }
        assert_eq!(Change::Initramfs.line(), AnswerLine::Vt);
    }

    #[test]
    fn each_commit_cuts_on_its_own_step_line() {
        assert_eq!(
            Commit::Keyslot.line(),
            "td-boot: transition step 2/6: keyslot 2 added, keyslot 1 authorizing"
        );
        assert_eq!(
            Commit::Token.line(),
            "td-boot: transition step 3/6: token for keyslot 2 imported"
        );
        assert_eq!(
            Commit::Test.line(),
            "td-boot: transition step 4/6: keyslot 2 tested"
        );
        assert_eq!(
            Commit::Destroy.line(),
            "td-boot: transition step 5/6: keyslot 1 destroyed, keyslot 2 authorizing"
        );
    }

    fn console(lines: &[&str]) -> String {
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push_str("\r\n");
        }
        text
    }

    #[test]
    fn release_lines_run_to_their_end_without_the_wait() {
        let text = console(&[
            "[    1.0] kernel noise",
            "TD-BOOT-VOLUME x /dev/vda2",
            "td-boot: waiting up to 10 s for /dev/tpmrm0",
            FIRST_BOOT_RELEASED,
            CAP_CLOSED,
            &format!("td-boot: volume {UUID} opened with keyslot 2 as dm-0 (td-selector)"),
            "td-boot: post-cap unseal of td token 1 (device-bound) refused: policy refused: x",
        ]);
        let lines = release_lines(&text, &|line| opened_with(line, UUID).is_some()).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], FIRST_BOOT_RELEASED);
        assert!(release_lines(&text, &|line| line == HALTED).is_none());
    }

    #[test]
    fn the_opened_line_names_the_keyslot_and_the_selector_mapping() {
        let line = |rest: &str| format!("td-boot: volume {UUID} opened with keyslot {rest}");
        assert_eq!(opened_with(&line("2 as dm-0 (td-selector)"), UUID), Some(2));
        assert_eq!(
            opened_with(&line("12 as dm-3 (td-selector)"), UUID),
            Some(12)
        );
        for bad in [
            "2 as dm-0 (td-system)",
            "2 as dm- (td-selector)",
            "x as dm-0 (td-selector)",
            "2 as /dev/dm-0 (td-selector)",
            "2 as dm-x (td-selector)",
            "2",
        ] {
            assert_eq!(opened_with(&line(bad), UUID), None, "{bad}");
        }
        assert_eq!(
            opened_with(
                &format!("td-boot: volume 6a5a5a5a-5a5a-405a-805a-5a5a5a5a5a5a opened with keyslot 2 as dm-0 (td-selector)"),
                UUID
            ),
            None
        );
    }

    fn result(text: &str, killed: bool) -> BootResult {
        BootResult {
            evidence: ConsoleEvidence {
                target: true,
                ..ConsoleEvidence::default()
            },
            exited_clean: false,
            marker_killed: killed,
            reason: "fixture".into(),
            console: text.into(),
            elapsed: Duration::ZERO,
            firefox_audio: FirefoxAudioCapture::NotRequested,
        }
    }

    #[test]
    fn a_release_needs_its_lines_in_order_and_each_post_cap_refusal() {
        let opened = format!("td-boot: volume {UUID} opened with keyslot 2 as dm-0 (td-selector)");
        let selected = format!(
            "{} {}",
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            "a".repeat(64)
        );
        let good = [
            DEVICE_BOUND_RELEASED,
            CAP_CLOSED,
            "td-boot: transition step 1/1: keyslot 2 tested",
            &opened,
            &selected,
            "td-boot: post-cap unseal of td token 1 (device-bound) refused: policy refused: PolicyPCR",
        ];
        let expected = [
            DEVICE_BOUND_RELEASED,
            CAP_CLOSED,
            "td-boot: transition step 1/1: keyslot 2 tested",
        ];
        let check = |lines: &[&str], slot| {
            require_release(
                &result(&console(lines), true),
                UUID,
                &expected,
                slot,
                &[(1, "device-bound")],
                "leg",
            )
        };
        check(&good, 2).unwrap();
        assert!(check(&good, 3).is_err());
        let mut swapped = good;
        swapped.swap(0, 1);
        assert!(check(&swapped, 2).is_err());
        let mut loaded = good;
        loaded[5] =
            "td-boot: post-cap unseal of td token 1 (device-bound) refused: load refused: x";
        assert!(check(&loaded, 2).is_err());
        let mut early = good;
        early.swap(4, 5);
        assert!(check(&early, 2).is_err());
        let mut halted = good.to_vec();
        halted.push(HALTED);
        assert!(check(&halted, 2).is_err());
        assert!(check(&good[..4], 2).is_err());
    }

    /// A recovered console as the selector prints it: secret-line's
    /// prompts each end their own line once the entry is read.
    fn recovered(token: &str, outcome: &[&str], post_cap: &str) -> String {
        let opened = format!("td-boot: volume {UUID} opened with keyslot 0 as dm-0 (td-selector)");
        let selected = format!(
            "{} {}",
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            "a".repeat(64)
        );
        let mut lines = vec![
            token,
            CAP_CLOSED,
            NOTHING_RELEASED,
            RECOVERY_ASKED,
            KEY_PROMPT,
            "td-boot: the entry holds 5 digits, not 48: enter it again",
            KEY_PROMPT,
            "td-boot: the recovery key does not open keyslot 0 (/bin/cryptsetup open failed (exit status: 2)): enter it again",
            KEY_PROMPT,
            KEY_OPENS,
            RESEAL_WARNING,
            RESEAL_KEYS,
            RESEAL_PROMPT,
        ];
        lines.extend_from_slice(outcome);
        lines.push(&opened);
        lines.push(&selected);
        lines.push(post_cap);
        console(&lines)
    }

    const POLICY: &str = "td-boot: td token 1 (device-bound): policy refused: Unseal 0x99d";
    const LOAD: &str = "td-boot: td token 1 (device-bound): load refused: Load 0x1df";
    const COMPLETE: &str = "td-boot: reseal complete: keyslot 1 holds a protector sealed to \
         this boot chain, first released on the next boot; keyslot 0 remains, so a failure \
         there returns to recovery";

    fn resealed() -> Vec<&'static str> {
        vec![
            "td-boot: transition step 1/5: keyslot 1 added, keyslot 0 authorizing",
            "td-boot: transition step 2/5: token for keyslot 1 imported",
            "td-boot: transition step 3/5: keyslot 1 tested",
            "td-boot: transition step 4/5: keyslot 2 destroyed, keyslot 1 authorizing",
            "td-boot: transition step 5/5: token 1 removed",
            COMPLETE,
        ]
    }

    #[test]
    fn a_recovered_boot_reads_its_refusals_the_key_and_the_reseal() {
        let post = |n: u8| {
            format!("td-boot: post-cap unseal of td token {n} (device-bound) refused: policy refused: x")
        };
        let declined = recovered(POLICY, &[RESEAL_DECLINED], &post(1));
        let boot = result(&declined, true);
        let (steps, keyslot) = require_recovered(&boot, UUID, Change::SelectorImage, 1).unwrap();
        assert!(steps.is_empty());
        assert_eq!(keyslot, None);
        let text = recovered(LOAD, &resealed(), &post(0));
        let boot = result(&text, true);
        let (steps, keyslot) = require_recovered(&boot, UUID, Change::FreshTpm, 0).unwrap();
        assert_eq!(steps.len(), 5);
        assert_eq!(keyslot, Some(1));
        // The wrong refusal, a declined reseal where one was confirmed, a
        // completed one where it was declined, the wrong post-cap token.
        let fails = |text: &str, change, token| {
            require_recovered(&result(text, true), UUID, change, token).is_err()
        };
        assert!(fails(
            &recovered(POLICY, &resealed(), &post(0)),
            Change::FreshTpm,
            0
        ));
        assert!(fails(
            &recovered(POLICY, &[RESEAL_DECLINED], &post(1)),
            Change::Initramfs,
            1
        ));
        assert!(fails(
            &recovered(POLICY, &resealed(), &post(0)),
            Change::SelectorImage,
            1
        ));
        assert!(fails(
            &recovered(POLICY, &resealed(), &post(1)),
            Change::Initramfs,
            0
        ));
        // A reseal with no tested keyslot, or out of order.
        let mut untested = resealed();
        untested.remove(2);
        assert!(fails(
            &recovered(POLICY, &untested, &post(0)),
            Change::Initramfs,
            0
        ));
        // Only two key prompts: the wrong key was never tried.
        let short = declined.replacen(KEY_PROMPT, "", 1);
        assert!(fails(&short, Change::SelectorImage, 1));
        let stopped = recovered(POLICY, &["td-boot: reseal stopped (x)", COMPLETE], &post(0));
        assert!(fails(&stopped, Change::Initramfs, 0));
    }

    #[test]
    fn the_wrong_key_passes_the_codec_and_differs_from_the_key() {
        // td-boot's fixture group: 12345 with check digit 9.
        assert_eq!(damm(b"12345"), Some(9));
        assert_eq!(damm(b"123459"), Some(0));
        assert_eq!(damm(b"12a45"), None);
        let key =
            RecoveryKey::from_channel(format!("{}\n", "123459".repeat(8)).into_bytes()).unwrap();
        let wrong = key.wrong().unwrap();
        assert_ne!(wrong, key.display());
        assert!(wrong.starts_with(b"12346"));
        for group in wrong.split(|byte| *byte == b'-') {
            assert_eq!(group.len(), GROUP_DIGITS);
            assert_eq!(damm(group), Some(0), "{group:?}");
        }
        assert_eq!(&wrong[GROUP_DIGITS..], &key.display()[GROUP_DIGITS..]);
        // The largest value wraps within 16 bits.
        let high = RecoveryKey::from_channel(
            format!("65535{}{}\n", damm(b"65535").unwrap(), "123459".repeat(7)).into_bytes(),
        )
        .unwrap();
        assert!(high.wrong().unwrap().starts_with(b"00000"));
        let answers = recovery_answers(&key, true).unwrap();
        let prompts: Vec<&str> = answers.iter().map(|answer| answer.prompt).collect();
        assert_eq!(prompts, [KEY_PROMPT, KEY_PROMPT, KEY_PROMPT, RESEAL_PROMPT]);
        assert_eq!(answers[2].reply, key.display());
        assert_eq!(answers[3].reply, b"reseal");
        assert_ne!(recovery_answers(&key, false).unwrap()[3].reply, b"reseal");
    }

    #[test]
    fn a_cut_lands_only_on_its_own_commit() {
        let states = transition_states();
        assert_eq!(states.len(), 7);
        assert_eq!(states[6], converged());
        assert_eq!(cut_lands(&states[2], 2), Cut::Landed);
        // A kill after the whole transition interrupted nothing.
        assert_eq!(cut_lands(&states[6], 2), Cut::Late(6));
        // Steps 3 and 4 leave the same header.
        assert_eq!(states[3], states[4]);
        assert_eq!(cut_lands(&states[3], 3), Cut::Landed);
        assert_eq!(cut_lands(&states[4], 4), Cut::Landed);
        assert_eq!(cut_lands(&states[5], 4), Cut::Late(5));
        assert_eq!(cut_lands(&states[5], 5), Cut::Landed);
        assert_eq!(cut_lands(&states[6], 5), Cut::Late(6));
        // Before its commit, or in no state of the plan.
        assert_eq!(cut_lands(&states[2], 3), Cut::Unknown);
        assert_eq!(cut_lands(&constructed(), 2), Cut::Unknown);
    }

    #[test]
    fn the_selector_command_line_is_the_first_printed() {
        let console = "BdsDxe: starting\r\n[    0.000000] Command line: initrd=x\r\n\
                       [    0.012345] Kernel command line: initrd=x td.oracle-load-option=1\r\n\
                       [    0.000000] Kernel command line: quiet\r\n";
        let line = selector_command_line(console).unwrap();
        assert_eq!(line, "initrd=x td.oracle-load-option=1");
        assert_eq!(selector_command_line("Kernel command line: \r\n"), Some(""));
        assert_eq!(selector_command_line("Command line: x\n"), None);
    }

    #[test]
    fn a_raw_console_must_hold_the_tail() {
        let raw = b"early\nTD-BOOT-VOLUME x\nlater line\nlast\r\n";
        let whole = result("TD-BOOT-VOLUME x\nlater line\nlast\r\n", true);
        require_whole("leg", &whole, raw).unwrap();
        // A tail whose first line is cut mid-way still matches.
        require_whole("leg", &result("OOT-VOLUME x\nlater line\nlast", true), raw).unwrap();
        assert!(require_whole("leg", &whole, b"early\nTD-BOOT-VOLUME x\n").is_err());
        assert!(require_whole("leg", &whole, b"").is_err());
    }

    #[test]
    fn plan_steps_are_numbered_in_order() {
        let lines = [
            DEVICE_BOUND_RELEASED,
            "td-boot: transition step 1/2: keyslot 2 tested",
            "td-boot: transition step 2/2: token 0 removed",
        ];
        assert_eq!(
            plan_steps(&lines).unwrap(),
            ["keyslot 2 tested", "token 0 removed"]
        );
        let skipped = [lines[0], lines[2]];
        assert!(plan_steps(&skipped).is_err());
        assert_eq!(plan_steps(&[CAP_CLOSED]).unwrap(), Vec::<&str>::new());
    }

    #[test]
    fn the_recovery_key_channel_holds_exactly_the_digits() {
        let key =
            RecoveryKey::from_channel(format!("{}\n", "123456".repeat(8)).into_bytes()).unwrap();
        assert!(key.found_in(format!("x{}y", "123456".repeat(8)).as_bytes()));
        assert!(key.found_in(["123456"; 8].join("-").as_bytes()));
        assert!(key.found_in(["123456"; 8].join(" ").as_bytes()));
        assert!(!key.found_in("123456".repeat(7).as_bytes()));
        for bad in [
            "123456".repeat(8),
            format!("{}\n", "123456".repeat(7)),
            format!("{}0\n", "123456".repeat(8)),
            format!("{}a\n", "12345".repeat(9)),
            format!("{}\n\n", "123456".repeat(8)),
        ] {
            assert!(RecoveryKey::from_channel(bad.into_bytes()).is_err());
        }
    }

    #[test]
    fn the_esp_changes_move_only_their_own_fields() {
        let mut pe = vec![0u8; 0x200];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        let original = pe.clone();
        change_pe_timestamp(&mut pe).unwrap();
        let moved: Vec<usize> = (0..pe.len()).filter(|i| pe[*i] != original[*i]).collect();
        assert_eq!(moved, [0x88]);
        assert!(change_pe_timestamp(&mut vec![0u8; 0x200]).is_err());
        let mut cpio = b"07070100000001000041ed00000000000000000000000200000000".to_vec();
        cpio.extend_from_slice(&[0; 64]);
        let original = cpio.clone();
        change_cpio_mtime(&mut cpio).unwrap();
        let moved: Vec<usize> = (0..cpio.len())
            .filter(|i| cpio[*i] != original[*i])
            .collect();
        assert_eq!(moved, [53]);
        assert!(change_cpio_mtime(&mut b"\x1f\x8b gzip".to_vec()).is_err());
    }

    #[test]
    fn a_file_is_found_through_the_fat_td_writes() {
        use td_engine::fat::{Node, Volume};
        let boot = vec![0x4d; 70_000];
        let initrd = vec![0x30; 5_000];
        let volume = Volume {
            bytes_per_sector: 512,
            total_sectors: 80_000,
            hidden_sectors: 0,
            volume_id: 1,
            label: "TD-ESP".into(),
            sectors_per_cluster: Some(1),
            root: vec![(
                "EFI".into(),
                Node::Dir(vec![(
                    "BOOT".into(),
                    Node::Dir(vec![
                        ("BOOTX64.EFI".into(), Node::File(&boot)),
                        ("INITRD".into(), Node::File(&initrd)),
                    ]),
                )]),
            )],
        };
        let image = td_engine::fat::build(&volume).unwrap().to_vec().unwrap();
        let dir = env::temp_dir().join(format!("td-boot-encrypted-fat-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("esp");
        let mut bytes = vec![0u8; 4096];
        bytes.extend_from_slice(&image);
        fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        for (name, contents) in [("BOOTX64.EFI", &boot), ("INITRD", &initrd)] {
            let (offset, size) = fat_file(&file, 4096, &["EFI", "BOOT", name]).unwrap();
            assert_eq!(size, contents.len() as u64);
            assert_eq!(
                &bytes[offset as usize..(offset + size) as usize],
                contents.as_slice()
            );
        }
        assert!(fat_file(&file, 4096, &["EFI", "BOOT", "OTHER"]).is_err());
        assert!(fat_file(&file, 4096, &["EFI", "BOOT"]).is_err());
        assert!(fat_file(&file, 0, &["EFI", "BOOT", "INITRD"]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
