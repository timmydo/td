//! Display checks a guest asks for by name. The guest writes `PROMPT NAME
//! [ARGUMENT...]` on its console and waits on ttyS0; the host captures the
//! display through QMP until that name's check accepts a capture, within
//! `SCREEN_TIMEOUT`, and only then types `ANSWER NAME`, so the guest acts
//! on nothing the host has not seen. A screen may also hold from its
//! answer until the next screen is accepted: the host keeps capturing,
//! as fast as QMP gives captures, whether or not the guest has asked for
//! the next screen yet, and every capture meanwhile, the accepting one
//! included, must satisfy it, so what the display shows between two steps
//! is checked too, not only where it settles.

use super::serial_shell::SerialPort;
use super::update::{ppm, read_capture};
use super::{qmp_deadline, qmp_json_path, Qmp, QMP_IO_TIMEOUT};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a screen may take to show once the guest asks for it.
pub(super) const SCREEN_TIMEOUT: Duration = Duration::from_secs(60);

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

/// One boot's screens: the console words, what each name checks, the
/// names the guest must ask for in exactly this order, and where the
/// capture a check refused is kept for the operator.
pub(super) struct GuestScreens<'a> {
    pub(super) prompt: &'a str,
    pub(super) answer: &'a str,
    pub(super) screens: &'a [Screen<'a>],
    pub(super) order: &'a [&'a str],
    pub(super) keep: Option<&'a Path>,
}

struct Pending<'a> {
    screen: &'a Screen<'a>,
    arguments: Vec<String>,
    until: Instant,
}

/// What the display must show and what it must keep showing: the screen
/// asked for, if any, and the hold of the last one accepted.
#[derive(Default)]
struct Judge<'a> {
    pending: Option<Pending<'a>>,
    holding: Option<(&'a str, Holds<'a>)>,
}

impl<'a> Judge<'a> {
    /// One capture's verdict: the screen to answer when it shows. A broken
    /// hold, a check's error and a deadline passed are refusals.
    fn judge(&mut self, pixels: &[u8], now: Instant) -> Result<Option<&'a str>, String> {
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
        }
    }

    /// Every screen asked for and answered.
    pub(super) fn done(&self) -> bool {
        self.next >= self.plan.order.len() && self.judge.pending.is_none()
    }

    /// Whether captures are due on every poll: while a screen is asked
    /// for, and while one holds, asked for or not.
    pub(super) fn watching(&self) -> bool {
        self.judge.pending.is_some() || self.judge.holding.is_some()
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
        let bytes = self.capture()?;
        let pixels = ppm(&bytes)?;
        match self.judge.judge(pixels, Instant::now()) {
            Ok(Some(name)) => port.send(&format!("{} {name}", self.plan.answer)),
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
}
