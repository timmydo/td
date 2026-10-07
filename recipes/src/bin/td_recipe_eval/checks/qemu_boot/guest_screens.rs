//! Display checks a guest asks for by name. The guest writes `PROMPT NAME
//! [ARGUMENT...]` on its console and waits on ttyS0; the host captures the
//! display through QMP until that name's check accepts a capture, within
//! `SCREEN_TIMEOUT`, and only then types `ANSWER NAME`, so the guest acts
//! on nothing the host has not seen. A screen may also hold from its
//! answer until the next screen is accepted: the host keeps capturing,
//! as fast as QMP gives captures, whether or not the guest has asked for
//! the next screen yet, and every capture meanwhile, the accepting one
//! included, must satisfy it, so what the display shows between two steps
//! is checked too, not only where it settles. A plan may also name one
//! screen after whose answer the guest suspends to RAM: the host waits for
//! QEMU to report it suspended, keeps it so for `SUSPEND_HOLD` and wakes
//! it with `system_wakeup`. From the suspension until the next screen is
//! accepted, or the boot ends, every capture must satisfy the wake's own
//! hold, but for the capture taken while it slept, which is admitted only
//! until the first capture after the wake that differs from it; and the
//! wake's inactive output must show at least once after the wake.

use super::serial_shell::SerialPort;
use super::update::{ppm, read_capture};
use super::{qmp_deadline, qmp_json_path, Qmp, QMP_IO_TIMEOUT};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a screen may take to show once the guest asks for it.
pub(super) const SCREEN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a suspended guest stays suspended before the host wakes it,
/// and how long it may take to suspend once its screen is answered.
pub(super) const SUSPEND_HOLD: Duration = Duration::from_secs(10);
const SUSPEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether a 1280x800 capture's RGB pixels show a screen, given the
/// arguments the guest named it with; arguments it does not take are an
/// error, not a screen still to come.
pub(super) type Check<'a> = &'a dyn Fn(&[u8], &[&str]) -> Result<bool, String>;
/// What every capture must satisfy until the next screen is accepted.
pub(super) type Holds<'a> = &'a dyn Fn(&[u8]) -> Result<bool, String>;

pub(super) struct Screen<'a> {
    pub(super) name: &'a str,
    pub(super) check: Check<'a>,
    pub(super) then: Option<Holds<'a>>,
}

/// The screen after whose answer the guest suspends to RAM; what every
/// capture from the suspension until the next screen is accepted must
/// satisfy; and the output that must show at least once after the wake.
pub(super) struct Wake<'a> {
    pub(super) after: &'a str,
    pub(super) holds: Holds<'a>,
    pub(super) inactive: Holds<'a>,
}

/// One boot's screens: the console words, what each name checks, the
/// names the guest must ask for in exactly this order, where the capture
/// a check refused is kept for the operator, and the suspension, if any.
pub(super) struct GuestScreens<'a> {
    pub(super) prompt: &'a str,
    pub(super) answer: &'a str,
    pub(super) screens: &'a [Screen<'a>],
    pub(super) order: &'a [&'a str],
    pub(super) keep: Option<&'a Path>,
    pub(super) wake: Option<Wake<'a>>,
}

struct Pending<'a> {
    screen: &'a Screen<'a>,
    arguments: Vec<String>,
    until: Instant,
}

/// A suspension's display: the capture taken while the guest slept,
/// admitted only until a capture after the wake differs from it; what
/// every other capture must satisfy; and whether the wake has been sent
/// and its inactive output seen since.
struct Suspension<'a> {
    asleep: Option<Vec<u8>>,
    holds: Holds<'a>,
    inactive: Holds<'a>,
    woken: bool,
    inactive_seen: bool,
}

impl<'a> Suspension<'a> {
    fn new(asleep: &[u8], wake: &Wake<'a>) -> Self {
        Self {
            asleep: Some(asleep.to_vec()),
            holds: wake.holds,
            inactive: wake.inactive,
            woken: false,
            inactive_seen: false,
        }
    }

    fn judge(&mut self, pixels: &[u8]) -> Result<(), String> {
        if self.asleep.as_deref() != Some(pixels) {
            if self.woken {
                self.asleep = None;
            }
            match (self.holds)(pixels) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(
                        "the display changed from its suspended capture to what its wake \
                         does not admit"
                            .into(),
                    )
                }
                Err(error) => return Err(format!("after the suspension: {error}")),
            }
        }
        // The asleep capture counts too when it is itself the inactive
        // output.
        if self.woken && (self.inactive)(pixels).map_err(|e| format!("after the wake: {e}"))? {
            self.inactive_seen = true;
        }
        Ok(())
    }

    /// Its end: the inactive output must have shown after the wake.
    fn end(&self) -> Result<(), String> {
        if self.inactive_seen {
            Ok(())
        } else {
            Err("the display never showed its inactive output after the wake".into())
        }
    }
}

/// What the display must show and what it must keep showing: the screen
/// asked for, if any, the hold of the last one accepted, and a
/// suspension's display.
#[derive(Default)]
struct Judge<'a> {
    pending: Option<Pending<'a>>,
    holding: Option<(&'a str, Holds<'a>)>,
    suspension: Option<Suspension<'a>>,
}

impl<'a> Judge<'a> {
    /// One capture's verdict: the screen to answer when it shows. A broken
    /// hold, a check's error and a deadline passed are refusals.
    fn judge(&mut self, pixels: &[u8], now: Instant) -> Result<Option<&'a str>, String> {
        if let Some(suspension) = &mut self.suspension {
            suspension.judge(pixels)?;
        }
        if let Some((after, holds)) = self.holding {
            match holds(pixels) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(format!("the display broke what follows screen {after:?}"))
                }
                Err(error) => return Err(format!("after screen {after:?}: {error}")),
            }
        }
        let Some(pending) = &self.pending else {
            return Ok(None);
        };
        let screen = pending.screen;
        let arguments: Vec<&str> = pending.arguments.iter().map(String::as_str).collect();
        match (screen.check)(pixels, &arguments) {
            Ok(true) => {
                if let Some(suspension) = self.suspension.take() {
                    suspension.end()?;
                }
                self.pending = None;
                self.holding = screen.then.map(|holds| (screen.name, holds));
                Ok(Some(screen.name))
            }
            Ok(false) if now >= pending.until => Err(format!(
                "the display did not show, within {}s, screen {:?}",
                SCREEN_TIMEOUT.as_secs(),
                screen.name
            )),
            Ok(false) => Ok(None),
            Err(error) => Err(format!("screen {:?}: {error}", screen.name)),
        }
    }
}

/// A suspension under way: when the guest must have suspended by, and
/// since when QEMU has reported it suspended.
struct Sleep {
    until: Instant,
    since: Option<Instant>,
}

impl Sleep {
    fn new(now: Instant) -> Result<Self, String> {
        Ok(Self {
            until: now
                .checked_add(SUSPEND_TIMEOUT)
                .ok_or("suspension deadline overflow")?,
            since: None,
        })
    }

    /// One look at QEMU's run state: whether the guest is now due its
    /// wake. A guest not suspended by its deadline, or running again
    /// before it was woken, is a refusal.
    fn observe(&mut self, suspended: bool, now: Instant) -> Result<bool, String> {
        match (self.since, suspended) {
            (None, true) => {
                self.since = Some(now);
                Ok(false)
            }
            (None, false) if now >= self.until => Err(format!(
                "the guest did not suspend within {}s",
                SUSPEND_TIMEOUT.as_secs()
            )),
            (None, false) => Ok(false),
            (Some(_), false) => Err("the guest left its suspension before it was woken".into()),
            (Some(since), true) => Ok(now.saturating_duration_since(since) >= SUSPEND_HOLD),
        }
    }
}

/// Answers the guest's screens in order, over QMP and the serial port.
pub(super) struct ScreenSteps<'a> {
    plan: &'a GuestScreens<'a>,
    path: PathBuf,
    capture: PathBuf,
    qmp: Option<Qmp>,
    /// How many of `order` the guest has asked for, and the console offset
    /// past the last of them.
    next: usize,
    from: usize,
    cap: usize,
    judge: Judge<'a>,
    sleep: Option<Sleep>,
    /// Whether the last poll failed to take its capture at all.
    lost: bool,
}

impl<'a> ScreenSteps<'a> {
    pub(super) fn new(plan: &'a GuestScreens<'a>, path: PathBuf, cap: usize) -> Self {
        let capture = path.with_file_name("guest-screen.ppm");
        Self {
            plan,
            path,
            capture,
            qmp: None,
            next: 0,
            from: 0,
            cap,
            judge: Judge::default(),
            sleep: None,
            lost: false,
        }
    }

    /// Every screen asked for and answered, and no suspension under way.
    pub(super) fn done(&self) -> bool {
        self.next >= self.plan.order.len() && self.judge.pending.is_none() && self.sleep.is_none()
    }

    /// Whether captures are due on every poll: while a screen is asked
    /// for, while one holds, asked for or not, and from a suspension
    /// until the next screen is accepted.
    pub(super) fn watching(&self) -> bool {
        self.judge.pending.is_some()
            || self.judge.holding.is_some()
            || self.judge.suspension.is_some()
            || self.sleep.is_some()
    }

    /// Whether every screen was answered and the last poll then could not
    /// take a capture: the display was lost, as when QEMU exits after the
    /// guest's last request, rather than judged.
    pub(super) fn lost_after_last(&self) -> bool {
        self.lost && self.done()
    }

    /// The screen the guest is due to ask for or the host to see.
    pub(super) fn awaiting(&self) -> Option<&str> {
        match &self.judge.pending {
            Some(pending) => Some(pending.screen.name),
            None => self.plan.order.get(self.next).copied(),
        }
    }

    /// One step: takes the guest's next request if it has made it, then
    /// at most one capture, judged against what holds and what is asked.
    pub(super) fn poll(&mut self, port: &mut SerialPort, console: &[u8]) -> Result<(), String> {
        if self.judge.pending.is_none() {
            self.take(console)?;
        }
        if !self.watching() {
            return Ok(());
        }
        let now = Instant::now();
        let due = if self.sleep.is_some() {
            let suspended = self.suspended()?;
            self.sleep
                .as_mut()
                .ok_or("the suspension disappeared")?
                .observe(suspended, now)?
        } else {
            false
        };
        let captured = self.capture();
        self.lost = captured.is_err();
        let bytes = captured?;
        let pixels = ppm(&bytes)?;
        // The first capture once QEMU reports the guest suspended is the
        // display as it slept.
        if let (Some(Sleep { since: Some(_), .. }), None, Some(wake)) =
            (&self.sleep, &self.judge.suspension, &self.plan.wake)
        {
            self.judge.suspension = Some(Suspension::new(pixels, wake));
        }
        let verdict = self.judge.judge(pixels, now);
        if due && verdict.is_ok() {
            self.wake()?;
            self.sleep = None;
            self.judge
                .suspension
                .as_mut()
                .ok_or("the suspension disappeared before its wake")?
                .woken = true;
        }
        match verdict {
            Ok(Some(name)) => {
                if self
                    .plan
                    .wake
                    .as_ref()
                    .is_some_and(|wake| wake.after == name)
                {
                    self.sleep = Some(Sleep::new(now)?);
                }
                port.send(&format!("{} {name}", self.plan.answer))
            }
            Ok(None) => Ok(()),
            Err(error) => Err(self.refused(&bytes, error)),
        }
    }

    /// After the boot: every screen answered, and no request the host can
    /// no longer judge in the console's last lines.
    pub(super) fn finish(&mut self, console: &[u8]) -> Result<(), String> {
        if console.len() >= self.cap {
            return Err(format!(
                "the console filled its {}-byte buffer, so the guest's last requests cannot be read",
                self.cap
            ));
        }
        if self.judge.pending.is_none() {
            self.take(console)?;
        }
        if let Some(pending) = &self.judge.pending {
            return Err(format!(
                "the boot ended with screen {:?} asked for and not shown",
                pending.screen.name
            ));
        }
        if self.sleep.is_some() {
            return Err("the boot ended with its suspension under way".into());
        }
        if let Some(suspension) = &self.judge.suspension {
            suspension.end()?;
        }
        match self.plan.order.get(self.next) {
            Some(name) => Err(format!("the guest's screens stopped at {name:?}")),
            None => Ok(()),
        }
    }

    /// The guest's next request, from its finished console lines.
    fn take(&mut self, console: &[u8]) -> Result<(), String> {
        let rest = console.get(self.from..).unwrap_or_default();
        let marker = format!("{} ", self.plan.prompt);
        let mut offset = self.from;
        for line in rest.split_inclusive(|byte| *byte == b'\n') {
            offset += line.len();
            if !line.ends_with(b"\n") {
                break;
            }
            let text = String::from_utf8_lossy(line);
            let Some(request) = text.trim_end().strip_prefix(&marker) else {
                continue;
            };
            let mut words = request.split(' ');
            let name = words.next().unwrap_or_default();
            let Some(expected) = self.plan.order.get(self.next) else {
                return Err(format!(
                    "the guest asked for screen {name:?} after its last"
                ));
            };
            if name != *expected {
                return Err(format!(
                    "the guest asked for screen {name:?} where {expected:?} was due"
                ));
            }
            let screen = self
                .plan
                .screens
                .iter()
                .find(|screen| screen.name == name)
                .ok_or_else(|| format!("no check for screen {name:?}"))?;
            let until = Instant::now()
                .checked_add(SCREEN_TIMEOUT)
                .ok_or("screen deadline overflow")?;
            self.judge.pending = Some(Pending {
                screen,
                arguments: words.map(str::to_string).collect(),
                until,
            });
            self.next += 1;
            self.from = offset;
            return Ok(());
        }
        if self.next < self.plan.order.len() && console.len() >= self.cap {
            return Err(format!(
                "the console filled its {}-byte buffer before screen {} of {} was asked for",
                self.cap,
                self.next + 1,
                self.plan.order.len()
            ));
        }
        Ok(())
    }

    /// One QMP command's reply, the connection made on first use and
    /// dropped on a failure, its I/O bounded.
    fn query(&mut self, command: &str) -> Result<Vec<u8>, String> {
        let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
        if self.qmp.is_none() {
            self.qmp = Some(Qmp::connect_until(&self.path, deadline)?);
        }
        let result = self
            .qmp
            .as_mut()
            .ok_or("QMP controller disappeared")?
            .query_until(command, deadline);
        if result.is_err() {
            self.qmp = None;
        }
        result
    }

    /// Whether QEMU reports the guest suspended to RAM.
    fn suspended(&mut self) -> Result<bool, String> {
        let reply = self
            .query(r#"{"execute":"query-status"}"#)
            .map_err(|error| format!("query the guest's run state: {error}"))?;
        Ok(suspended(&reply))
    }

    /// Wakes the suspended guest.
    fn wake(&mut self) -> Result<(), String> {
        self.query(r#"{"execute":"system_wakeup"}"#)
            .map(|_| ())
            .map_err(|error| format!("wake the guest: {error}"))
    }

    /// One capture, its QMP I/O bounded.
    fn capture(&mut self) -> Result<Vec<u8>, String> {
        let deadline = qmp_deadline(QMP_IO_TIMEOUT)?;
        if self.qmp.is_none() {
            self.qmp = Some(Qmp::connect_until(&self.path, deadline)?);
        }
        let filename = qmp_json_path(&self.capture)?;
        let result = self
            .qmp
            .as_mut()
            .ok_or("QMP controller disappeared before the screen capture")?
            .exchange_until(
                &format!(
                    "{{\"execute\":\"screendump\",\"arguments\":{{\"filename\":{filename}}}}}"
                ),
                deadline,
            );
        if let Err(error) = result {
            self.qmp = None;
            return Err(format!("capture the display: {error}"));
        }
        read_capture(&self.capture)
    }

    /// The refusal, after keeping the capture it judged.
    fn refused(&self, bytes: &[u8], error: String) -> String {
        let name = self.awaiting().unwrap_or("after-last");
        let kept = self.plan.keep.map(|dir| {
            let path = dir.join(format!("screen-{}-{name}.ppm", self.next));
            match fs::write(&path, bytes) {
                Ok(()) => format!("; the capture is kept at {}", path.display()),
                Err(error) => format!("; keeping the capture failed: {error}"),
            }
        });
        format!("{error}{}", kept.unwrap_or_default())
    }
}

/// Whether a `query-status` reply names the suspended run state.
fn suspended(reply: &[u8]) -> bool {
    let compact: String = String::from_utf8_lossy(reply)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    compact.contains(r#""status":"suspended""#)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never(_: &[u8], _: &[&str]) -> Result<bool, String> {
        Ok(false)
    }

    /// The guest's requests are taken from finished console lines, one at
    /// a time and only in the plan's order, with their arguments.
    #[test]
    fn requests_follow_the_order_from_finished_lines() {
        let screens = [
            Screen {
                name: "first",
                check: &never,
                then: None,
            },
            Screen {
                name: "second",
                check: &never,
                then: None,
            },
        ];
        let plan = GuestScreens {
            prompt: "ASK",
            answer: "SEEN",
            screens: &screens,
            order: &["first", "second", "first"],
            keep: None,
            wake: None,
        };
        let qmp = || PathBuf::from("/nonexistent/qmp");
        let mut steps = ScreenSteps::new(&plan, qmp(), 4096);
        assert_eq!(steps.awaiting(), Some("first"));
        steps.take(b"boot\nASK first a b").unwrap();
        assert!(steps.judge.pending.is_none() && !steps.watching());
        steps.take(b"boot\nASK first a b\r\n").unwrap();
        let pending = steps.judge.pending.take().unwrap();
        assert_eq!(pending.screen.name, "first");
        assert_eq!(pending.arguments, ["a", "b"]);
        // The same line is not taken twice; an answer's echo is no request.
        let console = b"boot\nASK first a b\r\nSEEN first\r\nASK second\n";
        steps.take(console).unwrap();
        assert_eq!(steps.judge.pending.take().unwrap().screen.name, "second");
        steps.take(console).unwrap();
        assert!(steps.judge.pending.is_none());
        let wrong = [&console[..], b"ASK second\n"].concat();
        assert!(steps.take(&wrong).is_err());
        let mut steps = ScreenSteps::new(&plan, qmp(), 4096);
        assert!(steps.take(b"ASK third\n").is_err());
        let mut done = ScreenSteps::new(&plan, qmp(), 4096);
        done.next = 3;
        assert!(done.done());
        assert!(done.take(b"ASK first\n").is_err());
        // A console full before every request was made refuses.
        let mut full = ScreenSteps::new(&plan, qmp(), 8);
        assert!(full.take(b"12345678").is_err());
    }

    /// After the boot, a request in the console's last lines fails, one
    /// left unshown fails, and so does a console too full to read.
    #[test]
    fn the_last_console_lines_are_judged_after_the_boot() {
        let screens = [Screen {
            name: "only",
            check: &never,
            then: None,
        }];
        let plan = GuestScreens {
            prompt: "ASK",
            answer: "SEEN",
            screens: &screens,
            order: &["only"],
            keep: None,
            wake: None,
        };
        let qmp = || PathBuf::from("/nonexistent/qmp");
        let console = b"ASK only\nSEEN only\n";
        let mut answered = ScreenSteps::new(&plan, qmp(), 4096);
        answered.take(console).unwrap();
        answered.judge.pending = None;
        assert!(answered.finish(console).is_ok());
        let late = [&console[..], b"ASK only\n"].concat();
        assert!(answered.finish(&late).is_err());
        let mut unasked = ScreenSteps::new(&plan, qmp(), 4096);
        assert!(unasked
            .finish(b"boot\n")
            .unwrap_err()
            .contains("stopped at \"only\""));
        let mut unshown = ScreenSteps::new(&plan, qmp(), 4096);
        assert!(unshown
            .finish(b"ASK only\n")
            .unwrap_err()
            .contains("asked for and not shown"));
        let mut full = ScreenSteps::new(&plan, qmp(), 8);
        full.next = 1;
        assert!(full.finish(b"12345678").is_err());
    }

    fn shows(wanted: u8) -> impl Fn(&[u8], &[&str]) -> Result<bool, String> {
        move |pixels, arguments| match arguments {
            [] => Ok(pixels.first() == Some(&wanted)),
            _ => Err("takes no arguments".into()),
        }
    }

    fn ask<'a>(judge: &mut Judge<'a>, screen: &'a Screen<'a>, arguments: &[&str]) {
        judge.pending = Some(Pending {
            screen,
            arguments: arguments.iter().map(|word| word.to_string()).collect(),
            until: Instant::now() + SCREEN_TIMEOUT,
        });
    }

    /// The hold is judged on every capture, the accepting one included,
    /// lasts across the next request until that screen is accepted, and
    /// ends there when that screen holds nothing.
    #[test]
    fn a_hold_lasts_until_the_next_screen_is_accepted() {
        let calm = |pixels: &[u8]| Ok(pixels.get(1) != Some(&b'!'));
        let (a, b, c) = (shows(b'a'), shows(b'b'), shows(b'c'));
        let first = Screen {
            name: "first",
            check: &a,
            then: Some(&calm),
        };
        let second = Screen {
            name: "second",
            check: &b,
            then: None,
        };
        let third = Screen {
            name: "third",
            check: &c,
            then: Some(&calm),
        };
        let now = Instant::now();
        let mut judge = Judge::default();
        assert_eq!(judge.judge(b"x!", now), Ok(None));
        ask(&mut judge, &first, &[]);
        assert_eq!(judge.judge(b"x.", now), Ok(None));
        assert_eq!(judge.judge(b"a.", now), Ok(Some("first")));
        // Held while no screen is asked for: any capture breaking it fails.
        assert_eq!(judge.judge(b"z.", now), Ok(None));
        assert!(judge
            .judge(b"z!", now)
            .unwrap_err()
            .contains("follows screen \"first\""));
        // Held across the next request, and on its accepting capture.
        ask(&mut judge, &second, &[]);
        assert_eq!(judge.judge(b"x.", now), Ok(None));
        assert!(judge.judge(b"b!", now).is_err());
        assert_eq!(judge.judge(b"b.", now), Ok(Some("second")));
        // Accepted without a hold: nothing holds now.
        assert!(judge.holding.is_none());
        assert_eq!(judge.judge(b"z!", now), Ok(None));
        // A hold replaces none, and is cleared only by acceptance.
        ask(&mut judge, &third, &[]);
        assert_eq!(judge.judge(b"c!", now), Ok(Some("third")));
        assert!(judge.judge(b"c!", now).is_err());
    }

    /// A deadline passed, a check's error and a hold's error refuse; an
    /// argument a screen does not take refuses at once.
    #[test]
    fn deadlines_and_check_errors_refuse() {
        let a = shows(b'a');
        let faulty = |_: &[u8]| Err("unreadable".to_string());
        let screen = Screen {
            name: "first",
            check: &a,
            then: Some(&faulty),
        };
        let mut judge = Judge::default();
        ask(&mut judge, &screen, &[]);
        let late = Instant::now() + SCREEN_TIMEOUT + Duration::from_secs(1);
        assert!(judge
            .judge(b"x", late)
            .unwrap_err()
            .contains("did not show"));
        assert_eq!(judge.judge(b"a", late), Ok(Some("first")));
        assert!(judge.judge(b"a", late).unwrap_err().contains("unreadable"));
        let mut judge = Judge::default();
        ask(&mut judge, &screen, &["extra"]);
        assert!(judge
            .judge(b"a", Instant::now())
            .unwrap_err()
            .contains("takes no arguments"));
    }

    /// The guest must suspend by its deadline and stay suspended until it
    /// is woken, which is due once it has been held for `SUSPEND_HOLD`.
    #[test]
    fn a_suspension_is_held_then_due_and_refuses_an_early_or_missing_one() {
        let start = Instant::now();
        let mut sleep = Sleep::new(start).unwrap();
        assert_eq!(sleep.observe(false, start), Ok(false));
        let at = start + Duration::from_secs(1);
        assert_eq!(sleep.observe(true, at), Ok(false));
        assert_eq!(sleep.observe(true, at + SUSPEND_HOLD / 2), Ok(false));
        assert_eq!(sleep.observe(true, at + SUSPEND_HOLD), Ok(true));
        assert!(sleep
            .observe(false, at + SUSPEND_HOLD / 2)
            .unwrap_err()
            .contains("before it was woken"));
        let mut never = Sleep::new(start).unwrap();
        assert!(never
            .observe(false, start + SUSPEND_TIMEOUT)
            .unwrap_err()
            .contains("did not suspend"));
        assert!(suspended(
            br#"{"return": {"status": "suspended", "singlestep": false, "running": false}}"#
        ));
        assert!(!suspended(
            br#"{"return": {"status": "running", "singlestep": false, "running": true}}"#
        ));
    }

    fn locked(pixels: &[u8]) -> Result<bool, String> {
        Ok(matches!(pixels.first(), Some(b'l' | b'i')))
    }

    fn inactive(pixels: &[u8]) -> Result<bool, String> {
        Ok(pixels.first() == Some(&b'i'))
    }

    fn suspended_judge<'a>(woken: bool) -> Judge<'a> {
        let wake = Wake {
            after: "asleep",
            holds: &locked,
            inactive: &inactive,
        };
        let mut suspension = Suspension::new(b"d.", &wake);
        suspension.woken = woken;
        Judge {
            suspension: Some(suspension),
            ..Judge::default()
        }
    }

    /// While suspended, a capture is the one taken asleep or satisfies the
    /// wake's hold. After the wake the asleep capture is admitted only
    /// until a capture differs from it, so the desktop cannot come back.
    #[test]
    fn the_asleep_capture_is_admitted_only_until_the_display_changes_after_the_wake() {
        let now = Instant::now();
        let mut judge = suspended_judge(false);
        assert_eq!(judge.judge(b"d.", now), Ok(None));
        assert_eq!(judge.judge(b"lx", now), Ok(None));
        // Before the wake, a change does not end the exemption.
        assert_eq!(judge.judge(b"d.", now), Ok(None));
        assert!(judge
            .judge(b"d!", now)
            .unwrap_err()
            .contains("suspended capture"));
        let mut judge = suspended_judge(true);
        assert_eq!(judge.judge(b"d.", now), Ok(None));
        assert_eq!(judge.judge(b"i.", now), Ok(None));
        // Desktop, inactive, the identical desktop: refused.
        assert!(judge
            .judge(b"d.", now)
            .unwrap_err()
            .contains("suspended capture"));
        let mut judge = suspended_judge(true);
        assert_eq!(judge.judge(b"l.", now), Ok(None));
        assert!(judge.judge(b"d.", now).is_err());
    }

    /// After the wake the inactive output must show before the next screen
    /// is accepted, or before the boot ends; acceptance ends the
    /// suspension and the screen's own hold applies after it.
    #[test]
    fn the_inactive_output_must_show_after_the_wake() {
        let a = shows(b'l');
        let next = Screen {
            name: "next",
            check: &a,
            then: None,
        };
        let now = Instant::now();
        // Only the lock after the wake: the dead-card premise is unshown.
        let mut judge = suspended_judge(true);
        ask(&mut judge, &next, &[]);
        assert!(judge
            .judge(b"l.", now)
            .unwrap_err()
            .contains("never showed its inactive output"));
        // An asleep capture that is itself the inactive output counts
        // once it shows after the wake.
        let wake = Wake {
            after: "asleep",
            holds: &locked,
            inactive: &inactive,
        };
        let mut asleep_inactive = Suspension::new(b"i.", &wake);
        assert_eq!(asleep_inactive.judge(b"i."), Ok(()));
        assert!(asleep_inactive.end().is_err());
        asleep_inactive.woken = true;
        assert_eq!(asleep_inactive.judge(b"i."), Ok(()));
        assert_eq!(asleep_inactive.end(), Ok(()));
        // Inactive output before the wake does not count.
        let mut judge = suspended_judge(false);
        assert_eq!(judge.judge(b"i.", now), Ok(None));
        judge.suspension.as_mut().unwrap().woken = true;
        ask(&mut judge, &next, &[]);
        assert!(judge.judge(b"l.", now).is_err());
        let mut judge = suspended_judge(true);
        assert_eq!(judge.judge(b"i.", now), Ok(None));
        ask(&mut judge, &next, &[]);
        assert!(judge.judge(b"x.", now).is_err());
        assert_eq!(judge.judge(b"l.", now), Ok(Some("next")));
        assert!(judge.suspension.is_none());
        assert_eq!(judge.judge(b"d!", now), Ok(None));
        // At the boot's end, the same requirement.
        let screens: [Screen; 0] = [];
        let plan = GuestScreens {
            prompt: "ASK",
            answer: "SEEN",
            screens: &screens,
            order: &[],
            keep: None,
            wake: None,
        };
        let mut steps = ScreenSteps::new(&plan, PathBuf::from("/nonexistent/qmp"), 4096);
        steps.judge = suspended_judge(true);
        assert!(steps.finish(b"").unwrap_err().contains("inactive output"));
        assert_eq!(steps.judge.judge(b"i.", now), Ok(None));
        assert!(steps.finish(b"").is_ok());
    }
}
