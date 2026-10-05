//! The installer window: a `td_ui` client that presents the wizard's pages
//! over a Wayland toplevel. From welcome it asks the installation service
//! for eligible disks and lists them, or says why it cannot; a chosen disk
//! leads to the settings form, whose time zones the service supplies, and
//! the completed form to the service's review of it; from the review it asks
//! the service to seek trusted consent and follows the installation's
//! progress to its outcome. It owns no Wayland objects of its own, so its
//! `Tag` is the empty `Object`; the seat and its devices are the client's.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use td_install::installation_plan::{Destination, Plan, Settings};

use td_ui::client::{run, App, Client, Handled, KeyboardEvent, Tag};
use td_ui::font::Font;
use td_ui::keys::{self, Overlay, Section, Step};
use td_ui::pointer;
use td_ui::raster::{Composition, Draw, Primitive, Raster, Scale, Surface, CHROME};
use td_ui::theme_file::Kept;
use td_ui::wayland::{connect, endpoint};
use td_ui::wire::Message;

use crate::destination::DestinationPage;
use crate::evidence::{self, field, Proof};
use crate::outcome::{CompletionPage, Progress, ProgressPage};
use crate::review::ReviewPage;
use crate::service::{Answer, Ending, Service, Stage, Standing, SOCKET};
use crate::settings::{Draft, TIME_ZONE};
use crate::welcome::Welcome;
use std::io::Write;

type Result<T> = std::result::Result<T, String>;

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}

/// The default toplevel extent, used until the compositor configures one.
const DEFAULT_SIZE: (usize, usize) = (800, 600);

/// The installer owns no Wayland objects of its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Object {}

impl Tag for Object {
    fn retired(self) -> bool {
        match self {}
    }
}

struct Window {
    client: Client<Object>,
    font: Font,
    /// The outline face the live window draws its text in.
    typeface: Option<td_ui::typeface::Typeface>,
    /// The theme the live window paints in and the file it is kept in.
    theme: Kept,
    /// The key list `keys::CHORD` shows over the page.
    key_list: Overlay,
    size: (usize, usize),
    dirty: bool,
    front: Front,
    /// The boot evidence, when the command line asks for it.
    proof: Option<Proof>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Page {
    Welcome,
    /// Asked for disks; no answer yet.
    Waiting,
    /// No service, or the connection to it ended.
    Unavailable,
    Refused(&'static str),
    Destinations,
    /// Account and regional settings for the selected disk.
    Settings,
    /// The service's review of the proposal, which holds the disk's claim.
    Review,
    /// Consent, the installation's progress, or an outcome not known.
    Progress(Progress),
    /// The service reported the installation complete.
    Complete,
}

/// A request sent to the service whose answer is awaited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Asked {
    Disks,
    Zones,
    Review,
    Withdraw,
    Execute,
    Status,
    End,
}

/// Where the completion page's restart or power-off stands; each asked
/// variant names the ending asked for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Power {
    Offered,
    /// Return was pressed; the request is not yet sent.
    Wanted(Ending),
    Sent(Ending),
    /// The supervisor accepted it; the session is ending.
    Accepted(Ending),
    Refused(&'static str),
    /// The connection is gone, and only welcome connects: nothing here can
    /// ask any more.
    Unavailable,
    /// The connection ended with the ending asked for and unanswered: the
    /// supervisor may have accepted it, its teardown ending the connection.
    Unknown(Ending),
}

/// What the wizard knows of the service's time zone catalog.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Catalog {
    Unasked,
    Listed,
    Refused(&'static str),
}

/// The evdev code of the left pointer button.
const BUTTON_LEFT: u32 = 272;

/// What a key asks of the window beyond the wizard's own state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    None,
    /// Stop waiting: the outstanding request, and its connection, go.
    Abandon,
}

/// The keys `Wizard::key` takes on each page, for the window's key list;
/// the settings form's own are `settings`'.
const WELCOME_KEYS: &[(&str, &str)] =
    &[("Return", "ask the installer service for the eligible disks")];
const DISK_KEYS: &[(&str, &str)] = &[
    ("Up/Down", "select the disk above or below"),
    (
        "PageUp/PageDown",
        "page through the selected disk's identity",
    ),
    ("Return", "continue with the selected disk to the settings"),
    (
        "Escape",
        "return to welcome; while the disks are awaited, stop asking",
    ),
];
const SETTINGS_KEYS: &[(&str, &str)] = &[("Escape", "back to the disks, keeping the drafts")];
const TIME_ZONE_KEYS: &[(&str, &str)] = &[
    ("Return", "ask the installer service to review the form"),
    ("Escape", "back to the disks, keeping the drafts"),
];
const REVIEW_KEYS: &[(&str, &str)] = &[
    ("PageUp/PageDown", "page through the review's details"),
    (
        "Return",
        "ask the installer service to seek consent at the secure prompt",
    ),
    ("Escape", "withdraw the review and go back to the settings"),
];
const CONSENT_KEYS: &[(&str, &str)] =
    &[("Escape", "withdraw the review and go back to the settings")];
const COMPLETE_KEYS: &[(&str, &str)] = &[
    ("Left/Up", "choose restart"),
    ("Right/Down", "choose power off"),
    ("Tab/S-Tab", "move to the other button"),
    (
        "Return",
        "do it; remove the installation media once it has restarted or powered off",
    ),
];

/// Shown on completion while an ending is asked for.
fn asking(ending: Ending) -> &'static str {
    match ending {
        Ending::Restart => "Asking the installer service to restart the computer\u{2026}",
        Ending::PowerOff => "Asking the installer service to power off the computer\u{2026}",
    }
}
/// Shown on completion once the supervisor accepted an ending.
fn ending_notice(ending: Ending) -> &'static str {
    match ending {
        Ending::Restart => "Restarting\u{2026}",
        Ending::PowerOff => "Powering off\u{2026}",
    }
}
/// Shown on completion once an asked ending's answer cannot come.
fn unknown_notice(ending: Ending) -> &'static str {
    match ending {
        Ending::Restart => "The computer may be restarting; if it does not, restart it yourself.",
        Ending::PowerOff => {
            "The computer may be powering off; if it does not, power it off yourself."
        }
    }
}
/// An ending's name in the page state.
fn ending_name(ending: Ending) -> &'static str {
    match ending {
        Ending::Restart => "restart",
        Ending::PowerOff => "poweroff",
    }
}
/// Shown on completion once nothing can ask for an ending.
const POWER_UNAVAILABLE: &str =
    "The installer service has ended; restart or power off the computer yourself.";

/// Shown on settings while a proposal is with the service.
const REVIEWING: &str = "Asking the installer service for a review\u{2026}";
/// Shown on review while execute is with the service.
const SEEKING: &str = "Asking the installer service to seek consent\u{2026}";
/// How often the service's state is asked for while consent is sought or
/// the installation runs, in milliseconds.
const POLL: u64 = 500;

/// The wizard's navigation over the service's answers. A selected disk is
/// a list index and a draft is typed text, not claims: the service
/// rechecks everything it is sent, and only its review is shown for
/// review.
#[derive(Debug)]
struct Wizard {
    page: Page,
    disks: Vec<Destination>,
    selected: Option<usize>,
    first: usize,
    detail: usize,
    asked: Option<Asked>,
    catalog: Catalog,
    draft: Draft,
    /// A proposal not yet sent.
    proposal: Option<(Destination, Settings)>,
    /// Whether the review of the proposal last sent is still wanted. Only
    /// a send sets it, so a review answers its own proposal, never one
    /// made after it was given up.
    awaited: bool,
    /// What became of the last proposal, shown on settings.
    notice: Option<&'static str>,
    /// The review shown, and its detail page.
    plan: Option<Box<Plan>>,
    review_page: usize,
    /// A review to release, by its nonce, once nothing is outstanding.
    withdraw: Option<[u8; 32]>,
    /// The review to execute, once nothing is outstanding.
    execute: Option<Box<Plan>>,
    /// The review execute was sent for, until its outcome is known or it
    /// is withdrawn; while it is set a lost connection is an unknown
    /// outcome, never merely an unavailable service.
    executing: Option<[u8; 32]>,
    /// What became of the last execute, shown on review.
    review_notice: Option<&'static str>,
    /// Whether the service's state is due to be asked for.
    poll_due: bool,
    /// The review the withdraw last sent names.
    withdrawing: Option<[u8; 32]>,
    /// The complete installation's review, which an ending names.
    completed: Option<[u8; 32]>,
    /// The completion page's selected button.
    choice: Ending,
    power: Power,
}

impl Wizard {
    fn new() -> Self {
        Self {
            page: Page::Welcome,
            disks: Vec::new(),
            selected: None,
            first: 0,
            detail: 0,
            asked: None,
            catalog: Catalog::Unasked,
            draft: Draft::default(),
            proposal: None,
            awaited: false,
            notice: None,
            plan: None,
            review_page: 0,
            withdraw: None,
            execute: None,
            executing: None,
            review_notice: None,
            poll_due: false,
            withdrawing: None,
            completed: None,
            choice: Ending::Restart,
            power: Power::Offered,
        }
    }

    fn key(&mut self, chord: &str) -> Action {
        match (&self.page, chord) {
            (Page::Welcome, "Return") => self.page = Page::Waiting,
            (Page::Welcome, _) => {}
            (Page::Waiting, "Escape") => {
                // The connection goes, and with it any review it held. If an
                // execute's outcome was not yet settled, it may be installing.
                self.page = if self.executing.take().is_some() {
                    Page::Progress(Progress::Unknown)
                } else {
                    Page::Welcome
                };
                self.asked = None;
                self.withdraw = None;
                self.withdrawing = None;
                return Action::Abandon;
            }
            // Back keeps the drafts and the disk they were for; a review
            // still coming is released when it comes.
            (Page::Settings, "Escape") => {
                self.page = Page::Destinations;
                self.proposal = None;
                self.awaited = false;
                self.notice = None;
            }
            (Page::Settings, "Return") if self.draft.focused() == TIME_ZONE => self.propose(),
            (Page::Settings, _) => {
                if self.draft.key(chord) && !self.reviewing() {
                    self.notice = None;
                }
            }
            // Back from review releases it, and the disk's claim; an execute
            // not yet sent goes, and one sent is answered before the release.
            (Page::Review, "Escape") => {
                self.withdraw = self.plan.take().map(|plan| *plan.nonce());
                self.execute = None;
                self.review_notice = None;
                self.review_page = 0;
                self.page = Page::Settings;
            }
            (Page::Review, "Return") => {
                if self.execute.is_none() && self.executing.is_none() {
                    self.execute = self.plan.clone();
                    self.review_notice = Some(SEEKING);
                }
            }
            // Leaving the secure prompt withdraws the review; until that is
            // confirmed the outcome stays the execute's.
            (Page::Progress(Progress::Consent), "Escape") => {
                self.withdraw = self.executing;
                self.plan = None;
                self.review_page = 0;
                self.page = Page::Settings;
            }
            // Only a complete installation's own review ends the session,
            // once asked, or again after a refusal.
            // The buttons sit Restart then Power off: arrows choose one,
            // Tab and S-Tab move to the other.
            (Page::Complete, "Up" | "Left") if self.endable() => self.choice = Ending::Restart,
            (Page::Complete, "Down" | "Right") if self.endable() => self.choice = Ending::PowerOff,
            (Page::Complete, "Tab" | "S-Tab") if self.endable() => {
                self.choice = match self.choice {
                    Ending::Restart => Ending::PowerOff,
                    Ending::PowerOff => Ending::Restart,
                };
            }
            (Page::Complete, "Return") if self.endable() => {
                self.power = Power::Wanted(self.choice);
            }
            // Nothing here undoes an installation or hides its outcome.
            (Page::Progress(_) | Page::Complete, _) => {}
            (Page::Review, "PageDown") => self.review_page = self.review_page.saturating_add(1),
            (Page::Review, "PageUp") => self.review_page = self.review_page.saturating_sub(1),
            (Page::Review, _) => {}
            (_, "Escape") => self.page = Page::Welcome,
            (Page::Destinations, "Return") if self.selected.is_some() => {
                self.page = Page::Settings;
                if matches!(self.catalog, Catalog::Refused(_)) {
                    self.catalog = Catalog::Unasked;
                }
            }
            (Page::Destinations, "Down") => {
                self.select(self.selected.map_or(0, |index| index.saturating_add(1)));
            }
            (Page::Destinations, "Up") => {
                self.select(self.selected.map_or(0, |index| index.saturating_sub(1)));
            }
            (Page::Destinations, "PageDown") => {
                self.detail = self.detail.saturating_add(1);
            }
            (Page::Destinations, "PageUp") => {
                self.detail = self.detail.saturating_sub(1);
            }
            _ => {}
        }
        Action::None
    }

    /// The pages' keys for the window's list, as `key` and the drafts take
    /// them, the shown page's first: the time zone's before the rest of
    /// the form's while its row has the focus.
    fn key_sections(&self) -> Vec<Section> {
        let mut settings = Section::new("Settings", SETTINGS_KEYS);
        let fields = crate::settings::FIELD_KEYS.iter().copied().map(keys::row);
        settings.rows.extend(fields);
        let mut zone = Section::new("Time zone", TIME_ZONE_KEYS);
        let zones = crate::settings::ZONE_KEYS.iter().copied().map(keys::row);
        zone.rows.extend(zones);
        let mut sections = vec![
            Some(Section::new("Welcome", WELCOME_KEYS)),
            Some(Section::new("Disks", DISK_KEYS)),
            Some(settings),
            Some(zone),
            Some(Section::new("Review", REVIEW_KEYS)),
            Some(Section::new("Consent", CONSENT_KEYS)),
            Some(Section::new("Complete", COMPLETE_KEYS)),
        ];
        let leads: &[usize] = match &self.page {
            Page::Welcome => &[0],
            Page::Waiting | Page::Unavailable | Page::Refused(_) | Page::Destinations => &[1],
            Page::Settings if self.draft.focused() == TIME_ZONE => &[3, 2],
            Page::Settings => &[2],
            Page::Review => &[4],
            Page::Progress(Progress::Consent) => &[5],
            Page::Complete if self.endable() => &[6],
            Page::Complete => &[],
            // These take no key.
            Page::Progress(_) => &[],
        };
        let mut ordered = Vec::with_capacity(sections.len());
        ordered.extend(
            leads
                .iter()
                .filter_map(|&lead| sections.get_mut(lead).and_then(Option::take)),
        );
        ordered.extend(sections.into_iter().flatten());
        ordered
    }

    fn select(&mut self, index: usize) {
        if index < self.disks.len() && self.selected != Some(index) {
            self.selected = Some(index);
            self.detail = 0;
        }
    }

    /// Whether a proposal is queued or its review still wanted.
    fn reviewing(&self) -> bool {
        self.proposal.is_some() || self.awaited
    }

    /// Proposes the selected disk with the drafts, once they are complete
    /// and no proposal is pending. One given up but still with the service
    /// is answered, and released, before this one is sent.
    fn propose(&mut self) {
        if self.reviewing() {
            return;
        }
        if let Some(missing) = self.draft.missing() {
            self.notice = Some(missing);
            return;
        }
        let Some(disk) = self.selected.and_then(|index| self.disks.get(index)) else {
            return;
        };
        let [username, hostname, keyboard, zone] = self.draft.values();
        match Settings::new(username, hostname, keyboard, zone) {
            Ok(settings) => {
                self.proposal = Some((disk.clone(), settings));
                self.notice = Some(REVIEWING);
            }
            Err(_) => self.notice = Some("These settings cannot be proposed."),
        }
    }

    /// The request the wizard needs next, once nothing is outstanding: a
    /// review to release first, then what the shown page waits on.
    fn wanted(&self) -> Option<Asked> {
        if self.asked.is_some() {
            return None;
        }
        if self.withdraw.is_some() {
            return Some(Asked::Withdraw);
        }
        match (&self.page, self.catalog) {
            (Page::Waiting, _) => Some(Asked::Disks),
            (Page::Settings, Catalog::Unasked) => Some(Asked::Zones),
            (Page::Settings, _) if self.proposal.is_some() => Some(Asked::Review),
            (Page::Review, _) if self.execute.is_some() => Some(Asked::Execute),
            (Page::Complete, _) if matches!(self.power, Power::Wanted(_)) => Some(Asked::End),
            _ if self.poll_due && self.executing.is_some() && self.following() => {
                Some(Asked::Status)
            }
            _ => None,
        }
    }

    /// The service's answer to what was asked; disks the wizard no longer
    /// waits for are dropped, a catalog is kept for when settings are
    /// shown again, and a review no longer wanted is released.
    fn answered(&mut self, answer: std::result::Result<Answer, String>) {
        match (self.asked.take(), answer) {
            (_, Err(_)) => self.lost(),
            (Some(Asked::Disks), Ok(Answer::Destinations(disks))) if self.page == Page::Waiting => {
                self.disks = disks;
                self.selected = None;
                self.first = 0;
                self.detail = 0;
                self.page = Page::Destinations;
            }
            (Some(Asked::Disks), Ok(Answer::Refused(reason))) if self.page == Page::Waiting => {
                self.page = Page::Refused(reason);
            }
            (Some(Asked::Zones), Ok(Answer::Timezones(zones))) => {
                self.draft.offer(zones);
                self.catalog = Catalog::Listed;
            }
            (Some(Asked::Zones), Ok(Answer::Refused(reason))) => {
                self.catalog = Catalog::Refused(reason);
            }
            (Some(Asked::Review), Ok(Answer::Reviewed(plan))) => {
                if std::mem::take(&mut self.awaited) && self.page == Page::Settings {
                    self.notice = None;
                    self.plan = Some(plan);
                    self.review_page = 0;
                    self.page = Page::Review;
                } else {
                    self.withdraw = Some(*plan.nonce());
                }
            }
            (Some(Asked::Review), Ok(Answer::Refused(reason))) if self.awaited => {
                self.awaited = false;
                self.notice = Some(reason);
            }
            // An execute answered after the review was left is the
            // release's to settle.
            (Some(Asked::Execute), Ok(Answer::Standing(standing))) if self.page == Page::Review => {
                self.review_notice = None;
                self.follow(standing);
            }
            // Abandoned before the release was sent: the claim is already
            // released, and the release would find no review.
            (
                Some(Asked::Execute),
                Ok(Answer::Standing(Standing::Review {
                    nonce,
                    stage: Stage::Abandoned(_),
                })),
            ) if self.executing == Some(nonce) => {
                self.executing = None;
                if self.withdraw == Some(nonce) {
                    self.withdraw = None;
                }
            }
            // Refused with the review still held: it may be executed again.
            (Some(Asked::Execute), Ok(Answer::Refused(reason))) => {
                self.executing = None;
                if self.page == Page::Review {
                    self.review_notice = Some(reason);
                }
            }
            // The service holds no review: nothing to execute or release.
            (Some(Asked::Execute), Ok(Answer::Unheld(reason))) => {
                self.withdraw = None;
                self.unheld(reason);
            }
            // It holds another review, with its claim: the review shown is
            // released, and the release of a nonce it does not hold ends
            // the connection, which ends that review.
            (Some(Asked::Execute), Ok(Answer::Stale(reason))) => {
                self.withdraw = self.withdraw.or(self.executing);
                self.unheld(reason);
            }
            (Some(Asked::Status), Ok(Answer::Standing(standing))) if self.following() => {
                self.follow(standing)
            }
            // A state asked for before the prompt was left does not bring it
            // back; an end it reports for the executed review settles that.
            (Some(Asked::Status), Ok(Answer::Standing(Standing::Review { nonce, stage })))
                if self.executing == Some(nonce) =>
            {
                match stage {
                    Stage::Abandoned(_) => {
                        self.executing = None;
                        if self.withdraw == Some(nonce) {
                            self.withdraw = None;
                        }
                    }
                    Stage::Complete | Stage::Failed(_) => {
                        if self.withdraw == Some(nonce) {
                            self.withdraw = None;
                        }
                        self.follow(Standing::Review { nonce, stage });
                    }
                    Stage::Reviewed | Stage::AwaitingConsent | Stage::Running(_) => {}
                }
            }
            (Some(Asked::End), Ok(Answer::Ending(ending))) => self.power = Power::Accepted(ending),
            (Some(Asked::End), Ok(Answer::Refused(reason))) => {
                self.power = Power::Refused(reason);
            }
            (Some(Asked::Withdraw), Ok(Answer::Withdrawn)) => {
                let released = self.withdrawing.take();
                if released.is_some() && self.executing == released {
                    self.executing = None;
                }
            }
            _ => {}
        }
    }

    /// Execute found the review shown not held: back to settings, saying
    /// why.
    fn unheld(&mut self, reason: &'static str) {
        self.executing = None;
        self.plan = None;
        self.review_notice = None;
        if matches!(self.page, Page::Review | Page::Settings) {
            self.notice = Some(reason);
            self.page = Page::Settings;
        }
    }

    /// Whether completion may ask for an ending: once, or again after a
    /// refusal, while the connection lasts.
    fn endable(&self) -> bool {
        self.completed.is_some() && matches!(self.power, Power::Offered | Power::Refused(_))
    }

    /// Whether the shown page follows an installation's state.
    fn following(&self) -> bool {
        matches!(
            self.page,
            Page::Progress(Progress::Consent | Progress::Running(_))
        )
    }

    /// Follows the state of the review execute was sent for. Only that
    /// review's own report moves on; any other state, idle included, is
    /// an outcome the installer cannot know.
    fn follow(&mut self, standing: Standing) {
        let (review, stage) = match standing {
            Standing::Review { nonce, stage } if self.executing == Some(nonce) => {
                (Some(nonce), stage)
            }
            _ => (None, Stage::Reviewed),
        };
        if stage == Stage::Complete {
            self.completed = review;
            self.choice = Ending::Restart;
            self.power = Power::Offered;
        }
        let page = match stage {
            Stage::AwaitingConsent => Page::Progress(Progress::Consent),
            Stage::Running(phase) => Page::Progress(Progress::Running(phase)),
            Stage::Complete => Page::Complete,
            Stage::Failed(failure) => Page::Progress(Progress::Failed(failure)),
            // Ended before any write: the claim is released, and a new
            // review may be proposed.
            Stage::Abandoned(reason) => {
                self.notice = Some(reason);
                Page::Settings
            }
            Stage::Reviewed => Page::Progress(Progress::Unknown),
        };
        if !matches!(stage, Stage::AwaitingConsent | Stage::Running(_)) {
            self.executing = None;
            self.plan = None;
            self.review_page = 0;
        }
        self.page = page;
    }

    /// The service is gone; a listed disk was its observation, and a
    /// review its claim, so they go too. The drafts stay, and so does a
    /// catalog already listed.
    fn lost(&mut self) {
        self.disks.clear();
        self.selected = None;
        self.first = 0;
        self.detail = 0;
        self.asked = None;
        self.proposal = None;
        self.awaited = false;
        self.notice = None;
        self.plan = None;
        self.review_page = 0;
        self.withdraw = None;
        self.execute = None;
        self.review_notice = None;
        self.poll_due = false;
        if self.catalog != Catalog::Listed {
            self.catalog = Catalog::Unasked;
        }
        self.withdrawing = None;
        // After execute, or once progress is shown, the service may still
        // be installing: the outcome is unknown, never merely unavailable.
        // A reported completion or failure stands.
        let reported = matches!(
            self.page,
            Page::Complete | Page::Progress(Progress::Failed(_))
        );
        // Completion can no longer ask for an ending; one asked for may
        // still be under way.
        if self.page == Page::Complete {
            self.power = match self.power {
                Power::Accepted(ending) => Power::Accepted(ending),
                Power::Sent(ending) | Power::Unknown(ending) => Power::Unknown(ending),
                Power::Offered | Power::Wanted(_) | Power::Refused(_) | Power::Unavailable => {
                    Power::Unavailable
                }
            };
        }
        if reported {
            self.executing = None;
        } else if self.executing.take().is_some() || matches!(self.page, Page::Progress(_)) {
            self.page = Page::Progress(Progress::Unknown);
        } else if self.page != Page::Welcome {
            self.page = Page::Unavailable;
        }
    }

    /// Records a request as sent.
    fn sent(&mut self, asked: Asked) {
        self.asked = Some(asked);
        match asked {
            Asked::Review => self.awaited = true,
            Asked::Status => self.poll_due = false,
            _ => {}
        }
    }

    /// The shown page's state for the boot evidence: which page, and on
    /// the destination, settings and review pages what the person chose.
    fn state(&self) -> String {
        let mut state = String::from("page=");
        match &self.page {
            Page::Welcome => state.push_str("welcome"),
            Page::Waiting => state.push_str("waiting"),
            Page::Unavailable => state.push_str("unavailable"),
            Page::Refused(_) => state.push_str("refused"),
            Page::Destinations => {
                state.push_str("destinations");
                field(&mut state, "disks", &self.disks.len().to_string());
                let selected = self
                    .selected
                    .and_then(|index| self.disks.get(index))
                    .map_or("-", Destination::name);
                field(&mut state, "selected", selected);
            }
            Page::Settings => {
                let [username, hostname, _, zone] = self.draft.values();
                state.push_str("settings");
                field(&mut state, "field", &self.draft.focused().to_string());
                field(&mut state, "username", username);
                field(&mut state, "hostname", hostname);
                field(&mut state, "seek", self.draft.seek());
                field(&mut state, "zone", zone);
                // A review left is released only once the service says so.
                let releasing = self.withdraw.is_some() || self.withdrawing.is_some();
                field(
                    &mut state,
                    "withdrawal",
                    if releasing { "pending" } else { "none" },
                );
            }
            Page::Review => {
                state.push_str("review");
                if let Some(plan) = &self.plan {
                    let settings = plan.settings();
                    field(&mut state, "disk", plan.destination().name());
                    field(&mut state, "username", settings.username());
                    field(&mut state, "hostname", settings.hostname());
                    field(&mut state, "zone", settings.timezone());
                }
            }
            Page::Progress(Progress::Consent) => state.push_str("consent"),
            Page::Progress(Progress::Running(_)) => state.push_str("running"),
            Page::Progress(Progress::Failed(_)) => state.push_str("failed"),
            Page::Progress(Progress::Unknown) => state.push_str("unknown"),
            Page::Complete => {
                state.push_str("complete");
                field(&mut state, "choice", ending_name(self.choice));
                let end = match self.power {
                    Power::Offered => "offered".to_string(),
                    Power::Wanted(ending) | Power::Sent(ending) => {
                        format!("asked-{}", ending_name(ending))
                    }
                    Power::Accepted(ending) => format!("accepted-{}", ending_name(ending)),
                    Power::Refused(_) => "refused".to_string(),
                    Power::Unavailable => "unavailable".to_string(),
                    Power::Unknown(ending) => format!("unknown-{}", ending_name(ending)),
                };
                field(&mut state, "end", &end);
            }
        }
        state
    }

    /// Shown in the time zone row while it has no zone.
    fn zone_hint(&self) -> Option<&'static str> {
        match self.catalog {
            Catalog::Listed => None,
            Catalog::Unasked => Some("Reading the time zones\u{2026}"),
            Catalog::Refused(reason) => Some(reason),
        }
    }
}

/// The wizard and its connection: what keys and answers do to both.
struct Front {
    wizard: Wizard,
    /// The connection to the installation service, once asked.
    service: Option<Service>,
    /// When the service's state may next be asked for, in turn-loop time.
    next_poll: u64,
    /// The turn loop's time at the last poll.
    now: u64,
}

impl Front {
    fn new() -> Self {
        Self {
            wizard: Wizard::new(),
            service: None,
            next_poll: 0,
            now: 0,
        }
    }

    /// While the wizard follows an installation, asks for the service's
    /// state at most every `POLL` milliseconds, counted from each ask.
    fn poll(&mut self, now: u64) {
        self.now = now;
        if self.wizard.following() && now >= self.next_poll {
            self.wizard.poll_due = true;
            self.request(None);
        }
    }

    fn key(&mut self, chord: &str, intake: &Path) {
        match self.wizard.key(chord) {
            // A hung service cannot hold the window: the person leaves it.
            Action::Abandon => self.service = None,
            Action::None => self.request(Some(intake)),
        }
    }

    /// Sends the request the wizard wants, connecting first for disks;
    /// only welcome starts a connection, and every later request rides
    /// the one that listed the disks.
    fn request(&mut self, intake: Option<&Path>) {
        let Some(asked) = self.wizard.wanted() else {
            return;
        };
        let service = match (&mut self.service, intake) {
            (Some(service), _) => service,
            (None, Some(intake)) if asked == Asked::Disks => match Service::connect(intake) {
                Ok(service) => self.service.insert(service),
                Err(why) => return self.lost(&why),
            },
            (None, _) => return self.lost("the installer service connection ended"),
        };
        if service.pending() {
            return;
        }
        // A review execute or withdraw names, recorded once the request is
        // handed to the worker.
        let mut names = None;
        let sent = match asked {
            Asked::Disks => service.destinations(),
            Asked::Zones => service.timezones(),
            Asked::Review => match self.wizard.proposal.take() {
                Some((disk, settings)) => service.propose(disk, settings),
                None => return,
            },
            Asked::Withdraw => match self.wizard.withdraw.take() {
                Some(nonce) => {
                    names = Some(nonce);
                    service.withdraw(nonce)
                }
                None => return,
            },
            Asked::Execute => match self.wizard.execute.take() {
                Some(plan) => {
                    names = Some(*plan.nonce());
                    service.execute(*plan)
                }
                None => return,
            },
            Asked::Status => service.status(),
            Asked::End => match (self.wizard.completed, self.wizard.power) {
                (Some(nonce), Power::Wanted(ending)) => service.end(nonce, ending),
                _ => return,
            },
        };
        match sent {
            Ok(()) => {
                match asked {
                    Asked::Execute => self.wizard.executing = names,
                    Asked::Withdraw => self.wizard.withdrawing = names,
                    Asked::Status => self.next_poll = self.now.saturating_add(POLL),
                    Asked::End => {
                        if let Power::Wanted(ending) = self.wizard.power {
                            self.wizard.power = Power::Sent(ending);
                        }
                    }
                    _ => {}
                }
                self.wizard.sent(asked);
            }
            Err(why) => self.lost(&why),
        }
    }

    /// Takes the service's answer, if one arrived, and says whether it did.
    fn receive(&mut self) -> bool {
        let Some(answer) = self.service.as_mut().and_then(Service::poll) else {
            return false;
        };
        match answer {
            Err(why) => self.lost(&why),
            answer => {
                self.wizard.answered(answer);
                self.request(None);
            }
        }
        true
    }

    fn lost(&mut self, why: &str) {
        let _ = writeln!(std::io::stderr(), "td-setup: installer service: {why}");
        self.service = None;
        self.wizard.lost();
    }
}

fn keyboard_chord(event: KeyboardEvent) -> Result<Option<String>> {
    match event {
        KeyboardEvent::Key { stroke, .. } => Ok(Some(stroke.chord)),
        KeyboardEvent::Keymap(Err(detail)) => Err(format!("keyboard keymap: {detail}")),
        _ => Ok(None),
    }
}

impl Window {
    fn new(stream: UnixStream, temporary: PathBuf, proof: bool) -> Result<Self> {
        Ok(Self {
            client: Client::new(stream, temporary)?,
            font: td_ui::font::pinned()?,
            typeface: None,
            theme: Kept::default(),
            key_list: Overlay::default(),
            size: DEFAULT_SIZE,
            dirty: true,
            front: Front::new(),
            proof: proof.then(Proof::default),
        })
    }

    /// Says the shown page's state once the window holds the keyboard.
    /// The evidence is the oracle's, so a failed write is the oracle's to
    /// notice, never the wizard's.
    fn say(&mut self) {
        let Some(proof) = self.proof.as_mut() else {
            return;
        };
        // Focused, its modifiers current and its keymap loaded: a key the
        // compositor sends now is one the wizard can read.
        let input = self.client.input();
        let focused =
            self.client.focus_serial().is_some() && input.synchronized && input.map.is_some();
        if let Some(line) = proof.due(focused) {
            let _ = std::io::stderr().lock().write_all(line.as_bytes());
        }
    }

    /// Takes the service's answer, if one arrived.
    fn receive(&mut self) {
        if self.front.receive() {
            self.dirty = true;
        }
    }

    fn initialize(&mut self) -> Result<()> {
        self.client.set_title("Install td")?;
        self.client.set_app_id("td-setup")?;
        self.client.commit()
    }

    fn event(&mut self, message: Message) -> Result<()> {
        match self.client.handle(&message, 0)? {
            Handled::Done => Ok(()),
            Handled::FrameDone => {
                if let Some(proof) = self.proof.as_mut() {
                    proof.frame_done();
                }
                Ok(())
            }
            Handled::Bound => self.initialize(),
            Handled::Configure { size, serial } => {
                if let Some((width, height)) = size {
                    // A zero axis keeps the current extent; a negative one is
                    // protocol-illegal, so treat it the same rather than cast
                    // it to a huge width.
                    let (current_w, current_h) = self.size;
                    self.size = (
                        if width <= 0 {
                            current_w
                        } else {
                            width as usize
                        },
                        if height <= 0 {
                            current_h
                        } else {
                            height as usize
                        },
                    );
                    self.dirty = true;
                    let surface = self.surface()?;
                    self.key_list.lay_out(surface);
                }
                self.client.acknowledge(serial)
            }
            Handled::CloseRequested => {
                self.client.close();
                Ok(())
            }
            Handled::GlobalRemoved { required: true, .. } => {
                Err("required Wayland global was removed".into())
            }
            Handled::Capabilities {
                keyboard: false, ..
            }
            | Handled::SeatRemoved => Err("installer keyboard is unavailable".into()),
            // Only translated keyboard presses navigate the live pages.
            Handled::Keyboard(event) => match keyboard_chord(event)? {
                Some(chord) => self.chord(chord),
                None => Ok(()),
            },
            Handled::Pointer(event) => {
                self.pointer(event);
                Ok(())
            }
            Handled::GlobalRemoved { .. }
            | Handled::Capabilities { .. }
            | Handled::Clipboard(_)
            | Handled::Primary(_) => Ok(()),
            Handled::Unhandled => Err(format!(
                "unexpected Wayland event {}:{}",
                message.object, message.opcode
            )),
        }
    }

    /// The surface the window paints at its current extent.
    fn surface(&self) -> Result<Surface> {
        let (width, height) = self.size;
        Surface::new(width, height, Scale::new(1).map_err(error)?).map_err(error)
    }

    /// The pointer drives no page: a left press only closes an open key
    /// list, as its title bar says, and repaints; the rest is ignored.
    fn pointer(&mut self, event: pointer::Event) {
        if let pointer::Event::Button {
            button: BUTTON_LEFT,
            pressed: true,
            ..
        } = event
        {
            if self.key_list.press() != Step::Kept {
                self.dirty = true;
            }
        }
    }

    /// A chord from the seat's keyboard. The theme chord is the window's,
    /// never a page's, and asks the service for nothing; so is
    /// `keys::CHORD`, which opens the key list, and while that is open it
    /// takes every other chord and no page hears one.
    fn chord(&mut self, chord: String) -> Result<()> {
        if chord == td_ui::theme::CHORD {
            if let Err(why) = self.theme.advance() {
                let _ = writeln!(std::io::stderr(), "td-setup: {why}");
            }
        } else if self.key_list.is_open() {
            let surface = self.surface()?;
            if self.key_list.key(&chord, surface) == Step::Kept {
                return Ok(());
            }
        } else if chord == keys::CHORD {
            let surface = self.surface()?;
            self.key_list
                .open(self.front.wizard.key_sections(), surface);
        } else {
            self.front.key(&chord, Path::new(SOCKET));
        }
        self.dirty = true;
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        if !self.dirty || !self.client.can_present() {
            return Ok(());
        }
        let (width, height) = self.size;
        let surface = self.surface()?;
        let wizard = &mut self.front.wizard;
        let settings = match &wizard.page {
            Page::Settings => Some(
                wizard
                    .draft
                    .page(surface, wizard.zone_hint(), wizard.notice),
            ),
            _ => None,
        };
        // A detail page past the last, as the surface lays it out, shows
        // the last and is kept.
        let review = match (&wizard.page, &wizard.plan) {
            (Page::Review, Some(plan)) => {
                let view = ReviewPage::new(surface, plan, wizard.review_page)
                    .or_else(|| {
                        let (_, pages) = ReviewPage::new(surface, plan, 0)?.position();
                        ReviewPage::new(surface, plan, pages.checked_sub(1)?)
                    })
                    .map(|view| view.with_notice(wizard.review_notice));
                if let Some(view) = &view {
                    wizard.review_page = view.position().0;
                }
                Some(view)
            }
            _ => None,
        };
        // The destination step's page, built first so the list position
        // it settles on is kept for the next turn.
        let destination = match &wizard.page {
            Page::Welcome | Page::Settings | Page::Review | Page::Progress(_) | Page::Complete => {
                None
            }
            Page::Waiting => Some(DestinationPage::waiting(surface)),
            Page::Unavailable => Some(DestinationPage::unavailable(surface)),
            Page::Refused(reason) => Some(DestinationPage::refused(surface, reason)),
            Page::Destinations => Some(DestinationPage::new_with_first(
                surface,
                &wizard.disks,
                wizard.selected,
                wizard.first,
                wizard.detail,
            )),
        };
        if let (Page::Destinations, Some(Some(view))) = (&wizard.page, &destination) {
            wizard.first = view.first_visible();
            wizard.detail = view.detail_position().0;
        }
        let outcome: Option<Option<Box<dyn Composition>>> = match wizard.page {
            Page::Progress(progress) => Some(
                ProgressPage::new(surface, progress)
                    .map(|view| Box::new(view) as Box<dyn Composition>),
            ),
            Page::Complete => {
                let notice = match wizard.power {
                    Power::Offered => None,
                    Power::Wanted(ending) | Power::Sent(ending) => Some(asking(ending)),
                    Power::Accepted(ending) => Some(ending_notice(ending)),
                    Power::Refused(reason) => Some(reason),
                    Power::Unavailable => Some(POWER_UNAVAILABLE),
                    Power::Unknown(ending) => Some(unknown_notice(ending)),
                };
                let choice = wizard.endable().then_some(wizard.choice);
                Some(CompletionPage::new(surface).map(|view| {
                    Box::new(view.with_notice(notice).with_choice(choice)) as Box<dyn Composition>
                }))
            }
            _ => None,
        };
        let Window {
            client,
            font,
            typeface,
            theme,
            key_list,
            ..
        } = self;
        // Whether the frame shows the page, not the too-small ground.
        let mut page = true;
        let presented = client.present(width, height, &mut |pixels| {
            let mut raster = Raster::new(pixels, font, surface, width * 4)
                .map_err(error)?
                .with_typeface(typeface.as_mut())
                .with_theme(theme.theme());
            let painted = match (&outcome, &settings, &review, &destination) {
                (Some(view), _, _, _) => view
                    .as_ref()
                    .map(|view| raster.paint(view.as_ref(), surface.bounds()).map_err(error)),
                (None, Some(view), _, _) => view
                    .as_ref()
                    .map(|view| raster.paint(view, surface.bounds()).map_err(error)),
                (None, None, Some(view), _) => view
                    .as_ref()
                    .map(|view| raster.paint(view, surface.bounds()).map_err(error)),
                (None, None, None, None) => Welcome::new(surface)
                    .map(|view| raster.paint(&view, surface.bounds()).map_err(error)),
                (None, None, None, Some(view)) => view
                    .as_ref()
                    .map(|view| raster.paint(view, surface.bounds()).map_err(error)),
            };
            let result = match painted {
                Some(result) => result,
                // Too small for the page: a plain chrome ground, never garbage.
                None => {
                    page = false;
                    raster.draw(Draw {
                        clip: surface.bounds(),
                        primitive: Primitive::Fill {
                            rect: surface.bounds(),
                            color: CHROME,
                        },
                    });
                    Ok(())
                }
            };
            // The key list, when open, is the last thing painted.
            key_list.emit(surface, surface.bounds(), &mut |draw| raster.draw(draw));
            result
        })?;
        if presented {
            self.dirty = false;
            if let Some(proof) = self.proof.as_mut() {
                if page {
                    proof.drawn(self.front.wizard.state());
                } else {
                    proof.blank();
                }
            }
        }
        Ok(())
    }
}

impl App for Window {
    type Tag = Object;

    fn client(&mut self) -> &mut Client<Object> {
        &mut self.client
    }

    fn needs_descriptor(&self, _: &Message) -> Result<bool> {
        Ok(false)
    }

    fn descriptor_wait(&mut self) {}

    fn tick(&mut self, _now: u64) -> Result<()> {
        self.receive();
        Ok(())
    }

    fn event(&mut self, message: Message) -> Result<()> {
        Window::event(self, message)
    }

    /// Every turn ends here, idle or not, so an answer arriving while
    /// nothing else happens is still shown within a turn. A status sent
    /// from an answer counts its interval from this turn.
    fn end_turn(&mut self, now: u64, _idle: bool) -> Result<()> {
        self.front.now = now;
        self.receive();
        if self.front.wizard.executing.is_some() {
            self.front.poll(now);
        }
        self.say();
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        Window::draw(self)
    }
}

/// Opens the installer window: resolves the display endpoint from the
/// environment, connects, and runs the turn loop presenting welcome, the
/// destination step, the settings step, the review, consent and the
/// installation's progress and outcome.
pub fn run_window() -> std::io::Result<()> {
    let work = || -> Result<()> {
        let endpoint = endpoint(
            std::env::var_os("WAYLAND_SOCKET"),
            std::env::var_os("WAYLAND_DISPLAY"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )?;
        // The evidence is the oracle's: a command line it cannot read
        // leaves it off, never the wizard.
        let proof = evidence::enabled(Path::new(evidence::CMDLINE)).unwrap_or_else(|why| {
            let _ = writeln!(std::io::stderr(), "td-setup: no boot evidence: {why}");
            false
        });
        let stream = connect(endpoint)?;
        let mut window = Window::new(stream, std::env::temp_dir(), proof)?;
        window.typeface = td_ui::pinned_face::load_or_note(
            "td-setup",
            std::env::var_os(td_ui::pinned_face::SETTING).as_deref(),
        );
        let (theme, notice) = Kept::host("td-setup");
        window.theme = theme;
        if let Some(notice) = notice {
            let _ = writeln!(std::io::stderr(), "td-setup: {notice}");
        }
        run(&mut window)
    };
    work().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::service::tests::{holding, installing, intake, listing, silent, slow_catalog};
    use std::time::{Duration, Instant};
    use td_install::installation_plan::DestinationObservation;

    fn disk(name: &str) -> Destination {
        Destination::new(DestinationObservation {
            name,
            major: 254,
            minor: 0,
            sequence: 7,
            capacity: 16_000_000_000,
            sector: 512,
            removable: false,
            model: None,
            serial: None,
            wwid: None,
        })
        .unwrap()
    }

    /// Takes answers, as the turn loop's ends do, until `page`.
    fn settle(front: &mut Front, page: Page) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.page != page {
            assert!(Instant::now() < deadline, "{:?}", front.wizard.page);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn enter_asks_the_service_and_escape_returns() {
        let mut wizard = Wizard::new();
        assert_eq!(wizard.key("Escape"), Action::None);
        assert_eq!(wizard.page, Page::Welcome);
        assert_eq!(wizard.wanted(), None);
        assert_eq!(wizard.key("Return"), Action::None);
        assert_eq!(wizard.page, Page::Waiting);
        assert_eq!(wizard.wanted(), Some(Asked::Disks));
        wizard.asked = Some(Asked::Disks);
        assert_eq!(wizard.wanted(), None);
        // Leaving the wait abandons the request.
        assert_eq!(wizard.key("Escape"), Action::Abandon);
        assert_eq!((&wizard.page, wizard.asked), (&Page::Welcome, None));
        // An answer the wizard stopped waiting for is dropped.
        wizard.answered(Ok(Answer::Destinations(vec![disk("vda")])));
        assert_eq!(wizard.page, Page::Welcome);
        wizard.key("Return");
        wizard.asked = Some(Asked::Disks);
        wizard.answered(Ok(Answer::Refused("the installer service is busy")));
        assert_eq!(wizard.page, Page::Refused("the installer service is busy"));
        assert_eq!(wizard.key("Escape"), Action::None);
        assert_eq!(wizard.page, Page::Welcome);
        assert_eq!(
            keyboard_chord(KeyboardEvent::Keymap(Err("bad map".into()))),
            Err("keyboard keymap: bad map".into())
        );
    }

    /// A wizard listing `vda` and `vdb`.
    fn listed() -> Wizard {
        let mut wizard = Wizard::new();
        wizard.key("Return");
        wizard.asked = wizard.wanted();
        wizard.answered(Ok(Answer::Destinations(vec![disk("vda"), disk("vdb")])));
        wizard
    }

    #[test]
    fn the_listed_disks_are_navigated_with_the_toolkit_chords() {
        let mut wizard = listed();
        assert_eq!(wizard.page, Page::Destinations);
        assert_eq!(wizard.selected, None);
        // Nothing selected, nothing to continue with.
        wizard.key("Return");
        assert_eq!(wizard.page, Page::Destinations);
        wizard.key("Up");
        assert_eq!(wizard.selected, Some(0));
        wizard.key("Down");
        wizard.key("Down");
        assert_eq!(wizard.selected, Some(1));
        wizard.key("PageDown");
        wizard.key("PageDown");
        assert_eq!(wizard.detail, 2);
        wizard.key("PageUp");
        assert_eq!(wizard.detail, 1);
        // Past either end the selection, and its identity page, stay.
        wizard.key("Down");
        assert_eq!((wizard.selected, wizard.detail), (Some(1), 1));
        // Moving the selection starts its identity at the first page.
        wizard.key("Up");
        assert_eq!((wizard.selected, wizard.detail), (Some(0), 0));
        wizard.key("PageDown");
        wizard.key("Up");
        assert_eq!((wizard.selected, wizard.detail), (Some(0), 1));
        // A lost service takes its observations with it.
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Unavailable);
        assert!(wizard.disks.is_empty());
        assert_eq!((wizard.selected, wizard.first, wizard.detail), (None, 0, 0));
    }

    #[test]
    fn a_chosen_disk_leads_to_settings_which_ask_for_the_catalog_once() {
        let mut wizard = listed();
        wizard.key("Down");
        wizard.key("Return");
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.wanted(), Some(Asked::Zones));
        assert_eq!(wizard.zone_hint(), Some("Reading the time zones\u{2026}"));
        wizard.asked = Some(Asked::Zones);
        // Typing is the form's; Escape is back, keeping the drafts.
        for chord in ["a", "l", "Escape"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.page, Page::Destinations);
        assert_eq!(wizard.draft.values()[0], "al");
        // The catalog that arrives meanwhile is kept and asked no more.
        wizard.answered(Ok(Answer::Timezones(crate::service::tests::zones())));
        assert_eq!(wizard.catalog, Catalog::Listed);
        wizard.key("Return");
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.wanted(), None);
        assert_eq!(wizard.zone_hint(), None);
        assert_eq!(wizard.draft.values(), ["al", "", "us", "Etc/UTC"]);
        // Losing the service keeps the drafts and the catalog.
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Unavailable);
        assert_eq!(wizard.draft.values(), ["al", "", "us", "Etc/UTC"]);
        assert_eq!(wizard.catalog, Catalog::Listed);
    }

    #[test]
    fn a_catalog_refused_while_disks_wait_still_lets_them_be_asked() {
        let mut wizard = listed();
        wizard.key("Down");
        wizard.key("Return");
        wizard.asked = wizard.wanted();
        for chord in ["Escape", "Escape", "Return"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.page, Page::Waiting);
        assert_eq!(wizard.wanted(), None);
        wizard.answered(Ok(Answer::Refused("the time zones could not be read")));
        // The refusal is the catalog's, not the disks'.
        assert_eq!(wizard.page, Page::Waiting);
        assert_eq!(
            wizard.catalog,
            Catalog::Refused("the time zones could not be read")
        );
        assert_eq!(wizard.wanted(), Some(Asked::Disks));
    }

    #[test]
    fn leaving_disks_queued_behind_the_catalog_abandons_both() {
        let mut wizard = listed();
        wizard.key("Down");
        wizard.key("Return");
        wizard.asked = wizard.wanted();
        for chord in ["Escape", "Escape", "Return"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.key("Escape"), Action::Abandon);
        assert_eq!((&wizard.page, wizard.asked), (&Page::Welcome, None));
        assert_eq!(wizard.catalog, Catalog::Unasked);
        // A new connection asks for disks first, then the catalog afresh.
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::Disks));
    }

    /// A wizard on settings for `vdb` with every field filled and the
    /// time zone focused.
    fn filled() -> Wizard {
        let mut wizard = listed();
        wizard.key("Down");
        wizard.key("Down");
        wizard.key("Return");
        wizard.asked = wizard.wanted();
        wizard.answered(Ok(Answer::Timezones(crate::service::tests::zones())));
        for chord in ["a", "l", "Tab", "h", "Tab", "Tab"] {
            wizard.key(chord);
        }
        wizard
    }

    fn reviewed(wizard: &Wizard) -> Box<Plan> {
        let (disk, settings) = wizard.proposal.as_ref().unwrap();
        Box::new(crate::service::tests::plan(disk, settings))
    }

    #[test]
    fn the_evidence_states_each_page_and_what_was_chosen() {
        let mut wizard = Wizard::new();
        assert_eq!(wizard.state(), "page=welcome");
        wizard.key("Return");
        assert_eq!(wizard.state(), "page=waiting");
        let mut wizard = listed();
        assert_eq!(wizard.state(), "page=destinations disks=2 selected=-");
        wizard.key("Down");
        wizard.key("Down");
        assert_eq!(wizard.state(), "page=destinations disks=2 selected=vdb");
        wizard.key("Return");
        assert_eq!(
            wizard.state(),
            "page=settings field=0 username= hostname= seek= zone= withdrawal=none"
        );
        let mut wizard = filled();
        wizard.key("e");
        wizard.key("u");
        assert_eq!(
            wizard.state(),
            "page=settings field=3 username=al hostname=h seek=eu zone=Europe/London \
             withdrawal=none"
        );
        wizard.key("Return");
        let plan = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        wizard.answered(Ok(Answer::Reviewed(plan)));
        assert_eq!(
            wizard.state(),
            "page=review disk=vdb username=al hostname=h zone=Europe/London"
        );
        // Back from review is pending until the service releases it.
        wizard.key("Escape");
        assert_eq!(
            wizard.state(),
            "page=settings field=3 username=al hostname=h seek=eu zone=Europe/London \
             withdrawal=pending"
        );
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
        wizard.withdrawing = wizard.withdraw.take();
        wizard.sent(Asked::Withdraw);
        assert!(wizard.state().ends_with(" withdrawal=pending"));
        wizard.answered(Ok(Answer::Withdrawn));
        assert!(wizard.state().ends_with(" withdrawal=none"));
        wizard.page = Page::Review;
        for (page, state) in [
            (Page::Progress(Progress::Consent), "page=consent"),
            (Page::Progress(Progress::Unknown), "page=unknown"),
            (Page::Complete, "page=complete choice=restart end=offered"),
            (Page::Unavailable, "page=unavailable"),
            (Page::Refused("busy"), "page=refused"),
        ] {
            wizard.page = page;
            assert_eq!(wizard.state(), state);
        }
    }

    #[test]
    fn enter_on_the_time_zone_proposes_the_completed_form() {
        let mut wizard = listed();
        wizard.key("Down");
        wizard.key("Return");
        wizard.asked = wizard.wanted();
        wizard.answered(Ok(Answer::Timezones(crate::service::tests::zones())));
        // Enter elsewhere moves on; on the time zone it asks for what is
        // missing first.
        wizard.key("Return");
        assert_eq!(wizard.draft.focused(), 1);
        wizard.key("Tab");
        wizard.key("Tab");
        wizard.key("Return");
        assert_eq!(wizard.notice, Some("Enter a username before review."));
        assert_eq!(wizard.wanted(), None);
        // An edit clears it.
        wizard.key("Tab");
        wizard.key("a");
        assert_eq!(wizard.notice, None);
        let mut wizard = filled();
        wizard.key("Return");
        assert_eq!(wizard.notice, Some(REVIEWING));
        assert_eq!(wizard.wanted(), Some(Asked::Review));
        let (disk, settings) = wizard.proposal.clone().unwrap();
        assert_eq!(Some(&disk), wizard.disks.get(1));
        assert_eq!(settings, Settings::new("al", "h", "us", "Etc/UTC").unwrap());
        let plan = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        // A second Enter while the service reviews proposes nothing more.
        wizard.key("Return");
        assert!(wizard.proposal.is_none());
        wizard.answered(Ok(Answer::Reviewed(plan.clone())));
        assert_eq!(wizard.page, Page::Review);
        assert_eq!(wizard.plan, Some(plan.clone()));
        assert_eq!(wizard.notice, None);
        wizard.key("PageDown");
        wizard.key("PageDown");
        wizard.key("PageUp");
        assert_eq!(wizard.review_page, 1);
        // Enter asks to execute; back before it is sent drops that.
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::Execute));
        assert_eq!(wizard.review_notice, Some(SEEKING));
        // Back releases the review and its claim.
        wizard.key("Escape");
        assert_eq!(wizard.execute, None);
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.plan, None);
        assert_eq!(wizard.withdraw, Some(*plan.nonce()));
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
        wizard.withdraw = None;
        wizard.asked = Some(Asked::Withdraw);
        wizard.answered(Ok(Answer::Withdrawn));
        assert_eq!((&wizard.page, wizard.wanted()), (&Page::Settings, None));
    }

    #[test]
    fn a_refused_proposal_is_said_on_settings() {
        let mut wizard = filled();
        wizard.key("Return");
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        // Typing while the service reviews keeps saying so.
        wizard.key("Up");
        assert_eq!(wizard.notice, Some(REVIEWING));
        wizard.answered(Ok(Answer::Refused("the selected disk changed")));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.notice, Some("the selected disk changed"));
        assert!(!wizard.reviewing());
        // It can be proposed again.
        wizard.key("Down");
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::Review));
    }

    #[test]
    fn a_review_no_longer_wanted_is_released() {
        // Left before it was sent: never sent.
        let mut wizard = filled();
        wizard.key("Return");
        wizard.key("Escape");
        assert_eq!(wizard.page, Page::Destinations);
        assert!(wizard.proposal.is_none());
        assert_eq!(wizard.wanted(), None);
        // Left while the service reviewed: released when it comes, even
        // after settings are shown again.
        let mut wizard = filled();
        wizard.key("Return");
        let plan = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        wizard.key("Escape");
        wizard.key("Return");
        assert_eq!(wizard.page, Page::Settings);
        wizard.answered(Ok(Answer::Reviewed(plan.clone())));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.plan, None);
        assert_eq!(wizard.withdraw, Some(*plan.nonce()));
        // The release goes before anything the page wants.
        wizard.catalog = Catalog::Unasked;
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
        // A lost service takes the review, and its claim, with it.
        let mut wizard = filled();
        wizard.key("Return");
        let plan = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        wizard.answered(Ok(Answer::Reviewed(plan)));
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Unavailable);
        assert_eq!((wizard.plan.is_none(), wizard.withdraw), (true, None));
        assert_eq!(wizard.draft.values(), ["al", "h", "us", "Etc/UTC"]);
    }

    #[test]
    fn a_review_given_up_never_answers_a_later_proposal() {
        let mut wizard = filled();
        wizard.key("Return");
        let first = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        // Leave, choose the other disk, and propose again at once.
        for chord in ["Escape", "Up", "Return", "Return"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.notice, Some(REVIEWING));
        let (disk, _) = wizard.proposal.clone().unwrap();
        assert_eq!(Some(&disk), wizard.disks.first());
        assert_eq!(wizard.wanted(), None);
        // The first review is released, not shown, and the second
        // proposal goes after the release.
        wizard.answered(Ok(Answer::Reviewed(first.clone())));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.withdraw, Some(*first.nonce()));
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
        wizard.withdraw = None;
        wizard.sent(Asked::Withdraw);
        wizard.answered(Ok(Answer::Withdrawn));
        assert_eq!(wizard.wanted(), Some(Asked::Review));
        let second = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        wizard.answered(Ok(Answer::Reviewed(second.clone())));
        assert_eq!(wizard.page, Page::Review);
        assert_eq!(wizard.plan, Some(second));
        // A refusal of a review given up is not said of a later proposal.
        let mut wizard = filled();
        wizard.key("Return");
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        for chord in ["Escape", "Return", "Return"] {
            wizard.key(chord);
        }
        wizard.answered(Ok(Answer::Refused("the selected disk changed")));
        assert_eq!(wizard.notice, Some(REVIEWING));
        assert_eq!(wizard.wanted(), Some(Asked::Review));
    }

    /// A wizard showing the service's review of `filled`.
    fn on_review() -> Wizard {
        let mut wizard = filled();
        wizard.key("Return");
        let plan = reviewed(&wizard);
        wizard.proposal = None;
        wizard.sent(Asked::Review);
        wizard.answered(Ok(Answer::Reviewed(plan)));
        assert_eq!(wizard.page, Page::Review);
        wizard
    }

    /// Sends the queued execute, as the front end does.
    fn send_execute(wizard: &mut Wizard) -> [u8; 32] {
        let plan = wizard.execute.take().unwrap();
        wizard.executing = Some(*plan.nonce());
        wizard.sent(Asked::Execute);
        *plan.nonce()
    }

    fn state(nonce: [u8; 32], stage: Stage) -> Answer {
        Answer::Standing(Standing::Review { nonce, stage })
    }

    /// Asks for and takes one state, as polling does.
    fn polled(wizard: &mut Wizard, answer: Answer) {
        wizard.poll_due = true;
        assert_eq!(wizard.wanted(), Some(Asked::Status));
        wizard.sent(Asked::Status);
        wizard.answered(Ok(answer));
    }

    #[test]
    fn an_executed_review_is_followed_from_consent_to_completion() {
        let mut wizard = on_review();
        wizard.key("Return");
        // A second Enter asks nothing more.
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.key("Return");
        assert_eq!(wizard.execute, None);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        assert_eq!(wizard.page, Page::Progress(Progress::Consent));
        // Only a due poll asks.
        assert_eq!(wizard.wanted(), None);
        polled(&mut wizard, state(nonce, Stage::AwaitingConsent));
        assert_eq!(wizard.page, Page::Progress(Progress::Consent));
        let running = Stage::Running(crate::outcome::Phase::WritingFilesystems);
        polled(&mut wizard, state(nonce, running));
        assert_eq!(
            wizard.page,
            Page::Progress(Progress::Running(crate::outcome::Phase::WritingFilesystems))
        );
        // Nothing a key does stops or leaves a running installation.
        for chord in ["Escape", "Return", "PageUp"] {
            wizard.key(chord);
        }
        assert_eq!((wizard.withdraw, wizard.executing), (None, Some(nonce)));
        polled(&mut wizard, state(nonce, Stage::Complete));
        assert_eq!(wizard.page, Page::Complete);
        assert_eq!((wizard.executing, wizard.plan.is_none()), (None, true));
        wizard.poll_due = true;
        assert_eq!(wizard.wanted(), None);
        // A completion reported stands when the service goes.
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Complete);
    }

    /// Return on completion asks, once, for the chosen ending of the
    /// review that completed, restart by default; a refusal is shown and
    /// Return asks again, and an accepted ending takes no more keys.
    #[test]
    fn a_complete_installation_restarts_or_powers_off_on_return() {
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::Complete)));
        assert_eq!(wizard.page, Page::Complete);
        assert_eq!(wizard.completed, Some(nonce));
        assert_eq!(wizard.state(), "page=complete choice=restart end=offered");
        assert_eq!(
            wizard.key_sections().first().map(|s| s.title),
            Some("Complete")
        );
        for chord in ["Escape", "PageUp", "a"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.wanted(), None);
        // Arrows choose a button, the same one again stays put, and Tab
        // and S-Tab move to the other.
        for (chord, choice) in [
            ("Up", "restart"),
            ("Left", "restart"),
            ("Down", "poweroff"),
            ("Right", "poweroff"),
            ("Down", "poweroff"),
            ("Tab", "restart"),
            ("S-Tab", "poweroff"),
            ("Left", "restart"),
            ("Right", "poweroff"),
        ] {
            wizard.key(chord);
            assert_eq!(
                wizard.state(),
                format!("page=complete choice={choice} end=offered")
            );
        }
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::End));
        assert_eq!(wizard.power, Power::Wanted(Ending::PowerOff));
        wizard.power = Power::Sent(Ending::PowerOff);
        wizard.sent(Asked::End);
        assert_eq!(
            wizard.state(),
            "page=complete choice=poweroff end=asked-poweroff"
        );
        // A request in flight takes no other key.
        for chord in ["Return", "Up"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.wanted(), None);
        assert_eq!(wizard.power, Power::Sent(Ending::PowerOff));
        let refused = "the computer could not be restarted or powered off";
        wizard.answered(Ok(Answer::Refused(refused)));
        assert_eq!(wizard.power, Power::Refused(refused));
        assert_eq!(wizard.state(), "page=complete choice=poweroff end=refused");
        // After a refusal the other ending may be chosen.
        wizard.key("Left");
        wizard.key("Return");
        assert_eq!(wizard.power, Power::Wanted(Ending::Restart));
        wizard.power = Power::Sent(Ending::Restart);
        wizard.sent(Asked::End);
        wizard.answered(Ok(Answer::Ending(Ending::Restart)));
        assert_eq!(
            wizard.state(),
            "page=complete choice=restart end=accepted-restart"
        );
        for chord in ["Return", "Down"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.wanted(), None);
        assert_eq!(wizard.choice, Ending::Restart);
        assert!(wizard.key_sections().first().map(|s| s.title) != Some("Complete"));
        // The session ends with the connection; the page stands.
        wizard.lost();
        assert_eq!(wizard.page, Page::Complete);
        assert_eq!(wizard.power, Power::Accepted(Ending::Restart));
        // A connection lost before the answer, or before Return, leaves
        // nothing to ask: the page says to end the session by other means.
        for (choice, before, after, end) in [
            (
                Ending::Restart,
                Power::Offered,
                Power::Unavailable,
                "unavailable",
            ),
            (
                Ending::PowerOff,
                Power::Refused("no"),
                Power::Unavailable,
                "unavailable",
            ),
            (
                Ending::PowerOff,
                Power::Sent(Ending::PowerOff),
                Power::Unknown(Ending::PowerOff),
                "unknown-poweroff",
            ),
        ] {
            wizard.choice = choice;
            wizard.power = before;
            wizard.lost();
            assert_eq!(wizard.page, Page::Complete);
            assert_eq!(wizard.power, after);
            assert_eq!(
                wizard.state(),
                format!("page=complete choice={} end={end}", ending_name(choice))
            );
            wizard.key("Return");
            assert_eq!(wizard.wanted(), None);
        }
    }

    #[test]
    fn only_the_executed_review_reports_its_outcome() {
        type Report = fn([u8; 32]) -> Answer;
        let cases: [(Report, Page); 4] = [
            (
                |nonce| state(nonce, Stage::Failed(crate::outcome::Failure::WriteFailed)),
                Page::Progress(Progress::Failed(crate::outcome::Failure::WriteFailed)),
            ),
            (
                |_| Answer::Standing(Standing::Idle),
                Page::Progress(Progress::Unknown),
            ),
            (
                |_| state([5; 32], Stage::Complete),
                Page::Progress(Progress::Unknown),
            ),
            (
                |nonce| state(nonce, Stage::Reviewed),
                Page::Progress(Progress::Unknown),
            ),
        ];
        for (answer, page) in cases {
            let mut wizard = on_review();
            wizard.key("Return");
            let nonce = send_execute(&mut wizard);
            wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
            polled(&mut wizard, answer(nonce));
            assert_eq!(wizard.page, page);
            assert_eq!(wizard.executing, None);
            wizard.poll_due = true;
            assert_eq!(wizard.wanted(), None);
        }
        // Declined at the prompt: the claim is released and settings say
        // why, ready for a new review.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        polled(&mut wizard, state(nonce, Stage::Abandoned("declined")));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.notice, Some("declined"));
        assert_eq!((wizard.plan.is_none(), wizard.executing), (true, None));
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::Review));
    }

    #[test]
    fn an_execute_refused_or_abandoned_at_once_is_said() {
        let mut wizard = on_review();
        wizard.key("Return");
        send_execute(&mut wizard);
        wizard.answered(Ok(Answer::Refused("trusted consent is unavailable")));
        assert_eq!(wizard.page, Page::Review);
        assert_eq!(wizard.review_notice, Some("trusted consent is unavailable"));
        assert_eq!(wizard.executing, None);
        // The review is still held: it may be executed again or left.
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::Execute));
        // The recheck found the disk changed: the claim is released.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(
            nonce,
            Stage::Abandoned("the selected disk changed"),
        )));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.notice, Some("the selected disk changed"));
        assert_eq!(wizard.withdraw, None);
        // Left while execute was with the service: the release follows,
        // unless the execute already ended the review.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.key("Escape");
        assert_eq!(wizard.withdraw, Some(nonce));
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.key("Escape");
        wizard.answered(Ok(state(
            nonce,
            Stage::Abandoned("the selected disk changed"),
        )));
        assert_eq!((wizard.withdraw, wizard.executing), (None, None));
        assert_eq!(wizard.page, Page::Settings);
    }

    #[test]
    fn leaving_the_prompt_withdraws_and_keeps_the_outcome_until_released() {
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        // A state already asked for when the prompt is left.
        wizard.poll_due = true;
        wizard.sent(Asked::Status);
        wizard.key("Escape");
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.withdraw, Some(nonce));
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
        // Consent given meanwhile: the release fails and the connection
        // ends, so the outcome is unknown, not merely unavailable.
        let mut lost = Wizard {
            asked: None,
            ..on_review()
        };
        lost.key("Return");
        let nonce = send_execute(&mut lost);
        lost.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        lost.key("Escape");
        lost.withdraw = None;
        lost.sent(Asked::Withdraw);
        lost.answered(Err(
            "the installer service did not release the review".into()
        ));
        assert_eq!(lost.page, Page::Progress(Progress::Unknown));
        // Released: the outcome is settled.
        wizard.withdrawing = wizard.withdraw.take();
        wizard.sent(Asked::Withdraw);
        wizard.answered(Ok(Answer::Withdrawn));
        assert_eq!(wizard.executing, None);
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Unavailable);
    }

    #[test]
    fn abandoning_a_connection_with_an_execute_unsettled_is_unknown() {
        // Escape at the prompt, back to welcome and on to the disk wait
        // while the release is still out; leaving that wait drops the
        // connection, and consent may have won.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        wizard.key("Escape");
        wizard.withdrawing = wizard.withdraw.take();
        wizard.sent(Asked::Withdraw);
        for chord in ["Escape", "Escape", "Return"] {
            wizard.key(chord);
        }
        assert_eq!(wizard.page, Page::Waiting);
        assert_eq!(wizard.key("Escape"), Action::Abandon);
        assert_eq!(wizard.page, Page::Progress(Progress::Unknown));
        assert_eq!(wizard.executing, None);
        // Without an execute out, leaving the wait is just leaving.
        let mut wizard = listed();
        wizard.key("Escape");
        wizard.key("Return");
        assert_eq!(wizard.key("Escape"), Action::Abandon);
        assert_eq!(wizard.page, Page::Welcome);
    }

    #[test]
    fn a_reported_failure_stands_when_the_service_goes() {
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        let failed = crate::outcome::Failure::VerificationFailed;
        polled(&mut wizard, state(nonce, Stage::Failed(failed)));
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Progress(Progress::Failed(failed)));
    }

    #[test]
    fn an_execute_for_a_review_not_held_returns_to_settings() {
        let mut wizard = on_review();
        wizard.key("Return");
        send_execute(&mut wizard);
        wizard.answered(Ok(Answer::Unheld("the review is out of date")));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.notice, Some("the review is out of date"));
        assert_eq!((wizard.plan.is_none(), wizard.executing), (true, None));
        // Left meanwhile: there is no review to release either.
        let mut wizard = on_review();
        wizard.key("Return");
        send_execute(&mut wizard);
        wizard.key("Escape");
        assert!(wizard.withdraw.is_some());
        wizard.answered(Ok(Answer::Unheld("there is no review")));
        assert_eq!((wizard.withdraw, wizard.executing), (None, None));
        // Another review held: the one shown is released, so a claim
        // cannot outlive what the window shows.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(Answer::Stale("the review is out of date")));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!((wizard.withdraw, wizard.executing), (Some(nonce), None));
        assert_eq!(wizard.wanted(), Some(Asked::Withdraw));
    }

    #[test]
    fn an_end_reported_after_the_prompt_was_left_settles_it() {
        // Declined or expired as the prompt was left: nothing to release.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        wizard.poll_due = true;
        wizard.sent(Asked::Status);
        wizard.key("Escape");
        wizard.answered(Ok(state(nonce, Stage::Abandoned("declined"))));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!((wizard.withdraw, wizard.executing), (None, None));
        assert_eq!(wizard.wanted(), None);
        // Consent won and the installation ended: its outcome is shown.
        let mut wizard = on_review();
        wizard.key("Return");
        let nonce = send_execute(&mut wizard);
        wizard.answered(Ok(state(nonce, Stage::AwaitingConsent)));
        wizard.poll_due = true;
        wizard.sent(Asked::Status);
        wizard.key("Escape");
        wizard.answered(Ok(state(nonce, Stage::Complete)));
        assert_eq!(wizard.page, Page::Complete);
        assert_eq!((wizard.withdraw, wizard.executing), (None, None));
    }

    #[test]
    fn a_service_lost_after_execute_leaves_the_outcome_unknown() {
        let mut wizard = on_review();
        wizard.key("Return");
        send_execute(&mut wizard);
        wizard.answered(Err("the installer service connection ended".into()));
        assert_eq!(wizard.page, Page::Progress(Progress::Unknown));
        assert_eq!(wizard.executing, None);
    }

    #[test]
    fn a_refused_catalog_is_said_and_asked_again_on_return() {
        let mut wizard = listed();
        wizard.key("Down");
        wizard.key("Return");
        wizard.asked = wizard.wanted();
        wizard.answered(Ok(Answer::Refused("the time zones could not be read")));
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(wizard.wanted(), None);
        assert_eq!(wizard.zone_hint(), Some("the time zones could not be read"));
        assert_eq!(wizard.draft.values()[3], "");
        wizard.key("Escape");
        wizard.key("Return");
        assert_eq!(wizard.wanted(), Some(Asked::Zones));
    }

    #[test]
    fn an_absent_intake_shows_the_service_unavailable_at_once() {
        let mut front = Front::new();
        front.key("Return", Path::new("/nonexistent/td-setup/setup"));
        assert_eq!(front.wizard.page, Page::Unavailable);
        assert!(front.service.is_none());
        front.key("Escape", Path::new("/nonexistent/td-setup/setup"));
        assert_eq!(front.wizard.page, Page::Welcome);
    }

    #[test]
    fn the_window_lists_the_service_disks_and_asks_again_on_return() {
        let intake = intake("front-list", usize::MAX, listing);
        let mut front = Front::new();
        front.key("Return", &intake);
        assert_eq!(front.wizard.page, Page::Waiting);
        settle(&mut front, Page::Destinations);
        assert_eq!(front.wizard.disks, [crate::service::tests::disk()]);
        // Escape keeps the connection; Enter asks it again.
        front.key("Escape", &intake);
        assert!(front.service.is_some());
        front.key("Return", &intake);
        assert!(front.service.as_ref().unwrap().pending());
        settle(&mut front, Page::Destinations);
        // A chosen disk's settings ask the same connection for time zones.
        front.key("Down", &intake);
        front.key("Return", &intake);
        assert_eq!(front.wizard.page, Page::Settings);
        assert_eq!(front.wizard.asked, Some(Asked::Zones));
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.catalog != Catalog::Listed {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(front.wizard.draft.values()[3], "Etc/UTC");
        assert_eq!(front.wizard.asked, None);
    }

    #[test]
    fn the_window_shows_the_service_review_and_withdraws_it_on_escape() {
        let intake = intake("front-review", usize::MAX, listing);
        let mut front = Front::new();
        front.key("Return", &intake);
        settle(&mut front, Page::Destinations);
        front.key("Down", &intake);
        front.key("Return", &intake);
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.catalog != Catalog::Listed {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        for chord in ["a", "l", "Tab", "h", "Tab", "Tab", "Return"] {
            front.key(chord, &intake);
        }
        assert_eq!(front.wizard.asked, Some(Asked::Review));
        settle(&mut front, Page::Review);
        let plan = front.wizard.plan.clone().unwrap();
        assert_eq!(plan.destination(), &crate::service::tests::disk());
        assert_eq!(plan.settings().username(), "al");
        front.key("Escape", &intake);
        assert_eq!(front.wizard.page, Page::Settings);
        assert_eq!(front.wizard.asked, Some(Asked::Withdraw));
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.asked.is_some() {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(front.service.is_some());
        assert_eq!(front.wizard.page, Page::Settings);
    }

    #[test]
    fn a_review_proposed_again_at_once_follows_the_release_of_the_last() {
        // This service is busy while it holds a review, as the core is.
        let intake = intake("front-hold", usize::MAX, holding());
        let mut front = Front::new();
        front.key("Return", &intake);
        settle(&mut front, Page::Destinations);
        front.key("Down", &intake);
        front.key("Return", &intake);
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.catalog != Catalog::Listed {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        for chord in ["a", "l", "Tab", "h", "Tab", "Tab", "Return"] {
            front.key(chord, &intake);
        }
        settle(&mut front, Page::Review);
        // Back and straight on again: the release must go first.
        front.key("Escape", &intake);
        front.key("Return", &intake);
        settle(&mut front, Page::Review);
        assert_eq!(front.wizard.notice, None);
    }

    #[test]
    fn the_window_follows_an_installation_to_its_completion() {
        let intake = intake("front-install", usize::MAX, installing());
        let mut front = Front::new();
        front.key("Return", &intake);
        settle(&mut front, Page::Destinations);
        front.key("Down", &intake);
        front.key("Return", &intake);
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.catalog != Catalog::Listed {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        for chord in ["a", "l", "Tab", "h", "Tab", "Tab", "Return"] {
            front.key(chord, &intake);
        }
        settle(&mut front, Page::Review);
        front.key("Return", &intake);
        // The turn loop's clock drives polling.
        let mut now = 0;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while front.wizard.page != Page::Complete {
            assert!(Instant::now() < deadline, "{seen:?}");
            front.receive();
            if seen.last() != Some(&front.wizard.page) {
                seen.push(front.wizard.page.clone());
            }
            if front.wizard.executing.is_some() {
                front.poll(now);
            }
            now += POLL;
            std::thread::sleep(Duration::from_millis(1));
        }
        // The execute may be answered before the first look.
        seen.retain(|page| *page != Page::Review);
        assert_eq!(
            seen,
            [
                Page::Progress(Progress::Consent),
                Page::Progress(Progress::Running(crate::outcome::Phase::PreparingDisk)),
                Page::Complete,
            ]
        );
        // Return restarts the installation that completed, on the same
        // connection.
        front.key("Return", &intake);
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.power != Power::Accepted(Ending::Restart) {
            assert!(Instant::now() < deadline, "{:?}", front.wizard.power);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(front.wizard.completed, Some([7; 32]));
    }

    /// A front end at the consent view of an `installing` service.
    fn at_consent(name: &str) -> (Front, PathBuf) {
        let intake = intake(name, usize::MAX, installing());
        let mut front = Front::new();
        front.key("Return", &intake);
        settle(&mut front, Page::Destinations);
        front.key("Down", &intake);
        front.key("Return", &intake);
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.catalog != Catalog::Listed {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
        for chord in ["a", "l", "Tab", "h", "Tab", "Tab", "Return"] {
            front.key(chord, &intake);
        }
        settle(&mut front, Page::Review);
        front.key("Return", &intake);
        settle(&mut front, Page::Progress(Progress::Consent));
        (front, intake)
    }

    /// Takes the outstanding answer.
    fn answer_taken(front: &mut Front) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while front.wizard.asked.is_some() {
            assert!(Instant::now() < deadline);
            front.receive();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn status_is_asked_at_most_every_poll_interval_from_each_ask() {
        let (mut front, _intake) = at_consent("front-poll");
        front.poll(10_000);
        assert_eq!(front.wizard.asked, Some(Asked::Status));
        answer_taken(&mut front);
        // Answered at once: the next ask still waits out the interval.
        front.poll(10_000 + POLL - 1);
        assert_eq!(front.wizard.asked, None);
        front.poll(10_000 + POLL);
        assert_eq!(front.wizard.asked, Some(Asked::Status));
    }

    #[test]
    fn the_window_withdraws_at_the_prompt_over_its_connection() {
        let (mut front, intake) = at_consent("front-consent");
        front.key("Escape", &intake);
        assert_eq!(front.wizard.page, Page::Settings);
        assert_eq!(front.wizard.asked, Some(Asked::Withdraw));
        answer_taken(&mut front);
        assert_eq!(front.wizard.executing, None);
        assert!(front.service.is_some());
        assert_eq!(front.wizard.page, Page::Settings);
    }

    #[test]
    fn disks_asked_while_the_catalog_is_awaited_are_asked_after_it() {
        let intake = intake("front-queue", usize::MAX, slow_catalog);
        let mut front = Front::new();
        front.key("Return", &intake);
        settle(&mut front, Page::Destinations);
        front.key("Down", &intake);
        front.key("Return", &intake);
        assert_eq!(front.wizard.asked, Some(Asked::Zones));
        front.key("Escape", &intake);
        front.key("Escape", &intake);
        front.key("Return", &intake);
        // Waiting on disks while the catalog is still outstanding: the
        // disks are asked once it arrives, on the same connection.
        assert_eq!(front.wizard.page, Page::Waiting);
        assert_eq!(front.wizard.asked, Some(Asked::Zones));
        settle(&mut front, Page::Destinations);
        assert_eq!(front.wizard.catalog, Catalog::Listed);
        assert_eq!(front.wizard.asked, None);
    }

    #[test]
    fn leaving_a_silent_service_abandons_its_connection() {
        let intake = intake("front-silent", usize::MAX, silent);
        let mut front = Front::new();
        front.key("Return", &intake);
        assert!(front.service.as_ref().unwrap().pending());
        front.key("Escape", &intake);
        assert_eq!(front.wizard.page, Page::Welcome);
        assert!(front.service.is_none());
    }

    #[test]
    fn a_service_that_goes_while_listed_takes_its_disks() {
        let intake = intake("front-gone", 1, listing);
        let mut front = Front::new();
        front.key("Return", &intake);
        settle(&mut front, Page::Destinations);
        front.key("Down", &intake);
        // The fake closes after one reply; the idle worker notices.
        settle(&mut front, Page::Unavailable);
        assert!(front.wizard.disks.is_empty());
        assert!(front.service.is_none());
    }

    fn titles(wizard: &Wizard) -> Vec<&'static str> {
        wizard
            .key_sections()
            .iter()
            .map(|section| section.title)
            .collect()
    }

    #[test]
    #[allow(clippy::indexing_slicing)]
    fn the_key_list_leads_with_the_shown_page_and_derives_the_form() {
        let mut wizard = Wizard::new();
        let order = [
            "Welcome",
            "Disks",
            "Settings",
            "Time zone",
            "Review",
            "Consent",
            "Complete",
        ];
        assert_eq!(titles(&wizard), order);
        let sections = wizard.key_sections();
        assert_eq!(
            sections[0].rows,
            [keys::row((
                "Return",
                "ask the installer service for the eligible disks"
            ))]
        );
        // The form's rows are the drafts' own tables, after the wizard's.
        let settings = &sections[2];
        assert_eq!(settings.rows[0].keys, "Escape");
        let fields: Vec<_> = crate::settings::FIELD_KEYS
            .iter()
            .copied()
            .map(keys::row)
            .collect();
        assert_eq!(settings.rows[1..], fields[..]);
        let zone = &sections[3];
        assert_eq!(zone.rows[0].keys, "Return");
        let zones: Vec<_> = crate::settings::ZONE_KEYS
            .iter()
            .copied()
            .map(keys::row)
            .collect();
        assert_eq!(zone.rows[2..], zones[..]);

        wizard.key("Return");
        assert_eq!(titles(&wizard)[..2], ["Disks", "Welcome"]);
        let mut wizard = listed();
        assert_eq!(titles(&wizard)[..2], ["Disks", "Welcome"]);
        assert!(wizard.key_sections()[0]
            .rows
            .contains(&keys::row(("Up/Down", "select the disk above or below"))));
        wizard.key("Down");
        wizard.key("Return");
        assert_eq!(wizard.page, Page::Settings);
        assert_eq!(titles(&wizard)[..3], ["Settings", "Welcome", "Disks"]);
        let wizard = filled();
        assert_eq!(wizard.draft.focused(), TIME_ZONE);
        assert_eq!(
            titles(&wizard),
            [
                "Time zone",
                "Settings",
                "Welcome",
                "Disks",
                "Review",
                "Consent",
                "Complete"
            ]
        );
        let mut wizard = on_review();
        assert_eq!(titles(&wizard)[0], "Review");
        wizard.key("Return");
        send_execute(&mut wizard);
        wizard.follow(Standing::Review {
            nonce: wizard.executing.unwrap(),
            stage: Stage::AwaitingConsent,
        });
        assert_eq!(wizard.page, Page::Progress(Progress::Consent));
        assert_eq!(titles(&wizard)[0], "Consent");
        wizard.page = Page::Complete;
        assert_eq!(titles(&wizard)[0], "Welcome");
        wizard.completed = Some([1; 32]);
        assert_eq!(titles(&wizard)[0], "Complete");
        wizard.page = Page::Progress(Progress::Unknown);
        assert_eq!(titles(&wizard), order);
    }

    #[test]
    fn every_page_key_list_is_spelled_as_the_keymap_spells_it() {
        let checked = |wizard: &Wizard| {
            let problems = keys::check(&wizard.key_sections());
            assert!(problems.is_empty(), "{:?}: {problems:#?}", wizard.page);
        };
        let mut wizard = Wizard::new();
        checked(&wizard);
        wizard.key("Return");
        checked(&wizard);
        let mut wizard = listed();
        checked(&wizard);
        wizard.key("Down");
        wizard.key("Return");
        assert_eq!(wizard.page, Page::Settings);
        checked(&wizard);
        let wizard = filled();
        assert_eq!(wizard.draft.focused(), TIME_ZONE);
        checked(&wizard);
        let mut wizard = on_review();
        checked(&wizard);
        wizard.key("Return");
        send_execute(&mut wizard);
        wizard.follow(Standing::Review {
            nonce: wizard.executing.unwrap(),
            stage: Stage::AwaitingConsent,
        });
        assert_eq!(wizard.page, Page::Progress(Progress::Consent));
        checked(&wizard);
        wizard.page = Page::Complete;
        checked(&wizard);
    }

    /// A window over one end of a socket pair: no compositor reads it.
    fn window() -> (Window, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        (
            Window::new(ours, std::env::temp_dir(), false).unwrap(),
            theirs,
        )
    }

    #[test]
    fn f1_opens_the_key_list_which_keeps_every_key_from_the_pages() {
        let (mut window, _peer) = window();
        window.front.wizard = listed();
        window.dirty = false;
        window.chord(keys::CHORD.into()).unwrap();
        assert!(window.key_list.is_open());
        assert!(window.dirty);
        assert_eq!(window.key_list.help().first(), 0);
        // A reading key moves the list, not the disk selection.
        window.dirty = false;
        window.chord("Down".into()).unwrap();
        assert!(window.dirty);
        assert_eq!(window.key_list.help().first(), 1);
        assert_eq!(window.front.wizard.selected, None);
        // A key the page binds, and one nobody does, change nothing and
        // paint nothing.
        window.dirty = false;
        for chord in ["Return", "Tab", "Left", "x"] {
            window.chord(chord.into()).unwrap();
        }
        assert!(!window.dirty);
        assert!(window.key_list.is_open());
        assert_eq!(window.front.wizard.page, Page::Destinations);
        assert_eq!(window.front.wizard.detail, 0);
        // Escape closes the list and goes nowhere else.
        window.chord("Escape".into()).unwrap();
        assert!(!window.key_list.is_open());
        assert!(window.dirty);
        assert_eq!(window.front.wizard.page, Page::Destinations);
        // F1 closes it as it opens it; the page hears neither.
        window.chord(keys::CHORD.into()).unwrap();
        window.chord(keys::CHORD.into()).unwrap();
        assert!(!window.key_list.is_open());
        assert_eq!(window.front.wizard.page, Page::Destinations);
        // Closed, the page has its keys again.
        window.chord("Down".into()).unwrap();
        assert_eq!(window.front.wizard.selected, Some(0));
    }

    #[test]
    fn a_left_press_closes_the_key_list_and_the_pointer_drives_nothing_else() {
        let button = |button, pressed| pointer::Event::Button {
            serial: 1,
            button,
            pressed,
        };
        let (mut window, _peer) = window();
        window.front.wizard = listed();
        // Closed, a press is nobody's.
        window.dirty = false;
        window.pointer(button(BUTTON_LEFT, true));
        window.pointer(button(BUTTON_LEFT, false));
        assert!(!window.dirty);
        assert!(!window.key_list.is_open());
        window.chord(keys::CHORD.into()).unwrap();
        assert!(window.key_list.is_open());
        // Motion, the wheel, a right press and a release leave it open.
        window.dirty = false;
        for event in [
            pointer::Event::Motion(256, 256),
            pointer::Event::Axis(0, 2560),
            pointer::Event::Frame,
            button(273, true),
            button(273, false),
            button(BUTTON_LEFT, false),
        ] {
            window.pointer(event);
        }
        assert!(window.key_list.is_open());
        assert!(!window.dirty);
        // A left press closes it and repaints; the page hears nothing.
        window.pointer(button(BUTTON_LEFT, true));
        assert!(!window.key_list.is_open());
        assert!(window.dirty);
        window.pointer(button(BUTTON_LEFT, false));
        assert_eq!(window.front.wizard.page, Page::Destinations);
        assert_eq!(window.front.wizard.selected, None);
    }
}
