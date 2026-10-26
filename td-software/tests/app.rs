//! The window's state driven as a person would, the compositor's answers
//! written by the test.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use td_software::app::{Action, App, Standing, POLL_ACTIVE_MS, POLL_IDLE_MS};
use td_software::registry::{self, Application};
use td_software::snapshot::{
    self, Admitted, CacheStatus, DeployStatus, GenerationStatus, Progress, Stage, Status,
};
use td_software::worker::{self, Failure, Job, Reply};
use td_ui::raster::{Scale, Surface};
use td_ui::window::{Input, PointerPhase};

const COMMIT: &str = "7985364b35b83cfe5aa17a00932df1c2d1dae69e5c592aa637a53ff1d53fdabc";
const RUNTIME_COMMIT: &str = "bd44a62300000000000000000000000000000000000000000000000000000000";
const REGISTRY: &str = "mail\t/td/store/x-mail/app.spec\nfirefox\t/td/store/x-firefox/app.spec\n";
const LAUNCHER: &str =
    "firefox\tFirefox\tweb browser\torg.mozilla.firefox\nmail\tMail\temail\ttd-mail\n";

fn applications() -> Vec<Application> {
    registry::applications(REGISTRY, Some(LAUNCHER)).unwrap()
}

fn app() -> App {
    let surface = Surface::new(1024, 768, Scale::default()).unwrap();
    App::new(applications(), None, surface)
}

fn generation(serves: bool) -> GenerationStatus {
    GenerationStatus {
        generation: 1,
        serves,
        application: DeployStatus {
            row: Admitted {
                exact_ref: "app/org.mozilla.firefox/x86_64/stable".into(),
                commit: COMMIT.into(),
                timestamp: 1_760_000_000,
            },
            bytes: Some(301_000_000),
        },
        runtime: DeployStatus {
            row: Admitted {
                exact_ref: "runtime/org.freedesktop.Platform/x86_64/25.08".into(),
                commit: RUNTIME_COMMIT.into(),
                timestamp: 1_759_000_000,
            },
            bytes: None,
        },
    }
}

fn firefox(current: Option<GenerationStatus>, progress: Option<Progress>) -> String {
    Status {
        name: "firefox".into(),
        cache: Some(CacheStatus {
            current,
            previous: None,
            checked: Some(1_760_000_100),
            refused: None,
            progress,
        }),
    }
    .render()
}

/// What the compositor answers to a status, as td-compositor writes it.
fn answer(operation: &str, last: &str, state: &str, record: &str) -> String {
    format!("ok\noperation={operation}\nlast={last}\nrecord={state}\n{record}")
}

fn status(app: &mut App, name: &str, text: &str) {
    let reply = worker::reply(Job::Status(name.into()), Ok(text.into()));
    app.reply(reply);
}

fn key(app: &mut App, chord: &str) {
    app.input(Input::Key {
        chord,
        repeat: false,
    });
}

/// The app's first asks, answered: mail built in, firefox not installed.
fn opened() -> App {
    let mut app = app();
    app.tick(0);
    let jobs = app.take_jobs();
    // Mail is built in: there is no cache to ask about.
    assert_eq!(jobs, [Job::Status("firefox".into())]);
    status(
        &mut app,
        "firefox",
        &answer("none", "none", "current", &firefox(None, None)),
    );
    app
}

#[test]
fn the_registry_lists_flathub_first_by_display_name() {
    let list = applications();
    let names: Vec<_> = list
        .iter()
        .map(|a| (a.name.as_str(), a.display.as_str()))
        .collect();
    assert_eq!(names, [("firefox", "Firefox"), ("mail", "Mail")]);
    assert_eq!(
        list[0].flathub,
        Some("app/org.mozilla.firefox/x86_64/stable")
    );
    assert_eq!(list[1].flathub, None);
    // Without the launcher table, the names stand.
    let bare = registry::applications(REGISTRY, None).unwrap();
    assert_eq!(bare[0].display, "firefox");
    // A display name with a control character is not shown.
    let odd = registry::applications(REGISTRY, Some("mail\tM\u{7}ail\tx\n")).unwrap();
    assert_eq!(odd[1].display, "mail");
    for bad in [
        "Firefox\t/x\n",
        "firefox\n",
        "\t/x\n",
        "firefox\t/x\nfirefox\t/y\n",
        "1app\t/x\n",
    ] {
        assert!(registry::applications(bad, None).is_err(), "{bad:?}");
    }
    let huge = "a\t/x\n".repeat(registry::MAX_TABLE_BYTES);
    assert!(registry::applications(&huge, None).is_err());
    assert!(registry::valid_name("org.example_app-2"));
    assert!(!registry::valid_name(&"a".repeat(65)));
}

#[test]
fn answers_parse_strictly() {
    assert_eq!(snapshot::answer("ok\nbody\n"), Ok("body\n"));
    assert_eq!(
        snapshot::answer("error firefox has no launcher entry\n"),
        Err(Failure {
            reason: "firefox has no launcher entry".into(),
            definite: true,
        })
    );
    assert_eq!(
        snapshot::answer("unavailable the program authority did not answer in time\n"),
        Err(Failure {
            reason: "the program authority did not answer in time".into(),
            definite: false,
        })
    );
    assert!(snapshot::answer("ok").is_err());
    assert!(snapshot::answer("maybe\n").is_err());
    let record = firefox(None, None);
    let parsed = snapshot::parse("firefox", &answer("install", "none", "older", &record)[3..]);
    let parsed = parsed.unwrap();
    assert_eq!(parsed.operation, Some(snapshot::Operation::Install));
    assert!(matches!(parsed.record, snapshot::Record::Older(_)));
    let failed = snapshot::parse(
        "firefox",
        "operation=none\nlast=remove\tfailed\tfirefox is running\nrecord=failed\n",
    )
    .unwrap();
    assert_eq!(
        failed.last,
        Some((
            snapshot::Operation::Remove,
            Some("firefox is running".into())
        ))
    );
    for bad in [
        "operation=none\nlast=none\nrecord=unread\nextra",
        "operation=launch\nlast=none\nrecord=unread\n",
        "operation=none\nlast=install\tmaybe\nrecord=unread\n",
        "operation=none\nlast=install\tfailed\t\nrecord=unread\n",
        "operation=none\nlast=none\nrecord=current\n",
        "last=none\noperation=none\nrecord=unread\n",
    ] {
        assert!(snapshot::parse("firefox", bad).is_err(), "{bad:?}");
    }
    // The record must be the asked application's.
    let mail = Status {
        name: "mail".into(),
        cache: None,
    }
    .render();
    let other = format!("operation=none\nlast=none\nrecord=current\n{mail}");
    assert!(snapshot::parse("firefox", &other).is_err());
}

#[test]
fn a_built_in_application_offers_nothing_and_a_flathub_one_install() {
    let mut app = opened();
    assert_eq!(app.names(), ["firefox", "mail"]);
    assert_eq!(app.metas(), ["not installed", "built in"]);
    assert_eq!(app.selected(), Some("firefox"));
    assert_eq!(app.standing(), Some(Standing::NotInstalled));
    assert_eq!(
        app.actions(),
        [(Action::Install, true), (Action::DeleteData, true)]
    );
    key(&mut app, "Down");
    assert_eq!(app.selected(), Some("mail"));
    assert_eq!(app.standing(), Some(Standing::BuiltIn));
    assert!(app.actions().is_empty());
    key(&mut app, "i");
    key(&mut app, "x");
    assert!(app.take_jobs().is_empty());
    assert!(!app.dialog_open());
    let details = app.details();
    assert!(details.iter().any(|(line, _)| line.starts_with("Built in")));
}

#[test]
fn install_runs_through_root_and_reports_its_end() {
    let mut app = opened();
    app.tick(1);
    assert!(app.take_jobs().is_empty(), "nothing is due yet");
    key(&mut app, "Return");
    assert_eq!(
        app.take_jobs(),
        [Job::Operation(
            snapshot::Operation::Install,
            "firefox".into()
        )]
    );
    // Asked: the buttons are off and the list says so before root answers.
    assert_eq!(app.metas()[0], "installing");
    assert_eq!(
        app.actions(),
        [(Action::Install, false), (Action::DeleteData, false)]
    );
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Ok(()),
    ));
    app.tick(2);
    assert_eq!(app.take_jobs(), [Job::Status("firefox".into())]);
    let progress = Progress {
        stage: Stage::Fetch,
        part: Some(snapshot::Part::Application),
        objects: 1200,
        objects_known: 4000,
        objects_bound: 1_000_000,
        bytes: 52_000_000,
        bytes_bound: 4_000_000_000,
    };
    status(
        &mut app,
        "firefox",
        &answer("install", "none", "current", &firefox(None, Some(progress))),
    );
    assert_eq!(app.metas()[0], "installing");
    let details: Vec<String> = app.details().into_iter().map(|(line, _)| line).collect();
    assert!(
        details.contains(&"Working: Fetching the application.".to_string()),
        "{details:?}"
    );
    assert!(
        details.iter().any(|line| line.starts_with(
            "1,200 of the 4,000 objects found so far (never more than 1,000,000), 52.0 MB"
        )),
        "{details:?}"
    );
    // A busy entry is asked again within the second.
    app.tick(2 + POLL_ACTIVE_MS - 1);
    assert!(app.take_jobs().is_empty());
    app.tick(2 + POLL_ACTIVE_MS);
    assert_eq!(app.take_jobs(), [Job::Status("firefox".into())]);
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "install\tok",
            "current",
            &firefox(Some(generation(true)), None),
        ),
    );
    assert_eq!(app.message(), "Firefox is installed");
    assert_eq!(app.standing(), Some(Standing::Installed { stale: false }));
    assert_eq!(
        app.actions(),
        [
            (Action::Launch, true),
            (Action::Update, true),
            (Action::Remove, true)
        ]
    );
    let details: Vec<String> = app.details().into_iter().map(|(line, _)| line).collect();
    assert!(
        details.contains(
            &"Size on disk: 301.0 MB for the application, unmeasured for its runtime".to_string()
        ),
        "{details:?}"
    );
    assert!(details
        .iter()
        .any(|line| line.starts_with("Version: application commit 7985364b35b8 of 2025-10-09")));
    // Idle now: asked again only after the idle interval.
    let now = 2 + POLL_ACTIVE_MS;
    app.tick(now + POLL_IDLE_MS - 1);
    assert!(app.take_jobs().is_empty());
    app.tick(now + POLL_IDLE_MS);
    assert_eq!(app.take_jobs(), [Job::Status("firefox".into())]);
    key(&mut app, "Return");
    assert_eq!(app.take_jobs(), [Job::Launch("firefox".into())]);
}

#[test]
fn a_refused_start_or_a_failed_operation_says_why() {
    let mut app = opened();
    key(&mut app, "i");
    app.take_jobs();
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Err(Failure {
            reason: "an operation on firefox is already running".into(),
            definite: true,
        }),
    ));
    assert_eq!(
        app.message(),
        "Root did not start the install: an operation on firefox is already running"
    );
    assert_eq!(app.actions()[0], (Action::Install, true));
    key(&mut app, "i");
    app.take_jobs();
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Ok(()),
    ));
    // It ended before the next status was read.
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "install\tfailed\tFlathub's summary signature does not verify",
            "current",
            &firefox(None, None),
        ),
    );
    assert_eq!(
        app.message(),
        "The install of Firefox failed: Flathub's summary signature does not verify"
    );
    assert_eq!(app.actions()[0], (Action::Install, true));
}

fn installed() -> App {
    let mut app = opened();
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "none",
            "current",
            &firefox(Some(generation(true)), None),
        ),
    );
    app
}

#[test]
fn remove_asks_first_and_escape_keeps_it() {
    let mut app = installed();
    key(&mut app, "x");
    assert!(app.dialog_open());
    let problems = td_ui::keys::check(&app.key_list());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    let (_, _, text) = td_ui::driven::text(&app.frame()).unwrap();
    assert!(text.contains("Remove Firefox?"), "{text}");
    // Cancel starts focused: Return keeps it.
    key(&mut app, "Return");
    assert!(!app.dialog_open());
    assert!(app.take_jobs().is_empty());
    assert_eq!(app.message(), "Nothing changed");
    key(&mut app, "x");
    key(&mut app, "Escape");
    assert!(!app.dialog_open());
    assert!(app.take_jobs().is_empty());
}

#[test]
fn remove_confirmed_asks_root_and_a_change_makes_the_question_stale() {
    // How many Tabs from where the question opens reach the button that
    // confirms: Return there asks root.
    let mut app = installed();
    key(&mut app, "x");
    let mut tabs = 0;
    loop {
        let mut trial = installed();
        key(&mut trial, "x");
        for _ in 0..tabs {
            key(&mut trial, "Tab");
        }
        key(&mut trial, "Return");
        let jobs = trial.take_jobs();
        if !jobs.is_empty() {
            assert_eq!(
                jobs,
                [Job::Operation(
                    snapshot::Operation::Remove,
                    "firefox".into()
                )]
            );
            break;
        }
        tabs += 1;
        assert!(tabs < 4, "no focus confirms");
    }
    for _ in 0..tabs {
        key(&mut app, "Tab");
    }
    // The status changes under the question: it closes at once,
    // unanswered.
    status(
        &mut app,
        "firefox",
        &answer(
            "update",
            "none",
            "current",
            &firefox(Some(generation(true)), None),
        ),
    );
    assert!(!app.dialog_open());
    assert!(app.take_jobs().is_empty());
    assert_eq!(app.message(), "The application changed; ask again");
}

#[test]
fn delete_data_is_offered_only_without_an_installed_version() {
    let mut app = installed();
    key(&mut app, "D");
    assert!(!app.dialog_open());
    let mut app = opened();
    key(&mut app, "D");
    assert!(app.dialog_open());
    let (_, _, text) = td_ui::driven::text(&app.frame()).unwrap();
    assert!(text.contains("Delete Firefox's data?"), "{text}");
    assert!(text.contains("cannot be undone"), "{text}");
}

#[test]
fn a_stale_installation_says_so() {
    let mut app = opened();
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "none",
            "current",
            &firefox(Some(generation(false)), None),
        ),
    );
    assert_eq!(app.standing(), Some(Standing::Installed { stale: true }));
    assert_eq!(app.metas()[0], "needs checking");
}

#[test]
fn an_unreachable_compositor_is_reported_and_retried() {
    let mut app = app();
    app.tick(0);
    for job in app.take_jobs() {
        app.reply(worker::run(None, job));
    }
    assert_eq!(app.metas(), ["unreachable", "built in"]);
    let details: Vec<String> = app.details().into_iter().map(|(line, _)| line).collect();
    assert!(details[1].starts_with("Its status cannot be read: no compositor control socket"));
    app.tick(1);
    assert!(app.take_jobs().is_empty());
    app.tick(td_software::app::POLL_RETRY_MS);
    assert_eq!(app.take_jobs(), [Job::Status("firefox".into())]);
}

#[test]
fn buttons_answer_a_press_and_release_on_them() {
    let mut app = opened();
    // Find the Install button by pressing and releasing across the lower
    // left of the window.
    let mut found = None;
    'scan: for y in (0..768).rev().step_by(4) {
        for x in (0..400).step_by(4) {
            let mut trial = opened();
            trial.input(Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
                extend: false,
                follow: false,
            });
            trial.input(Input::Pointer {
                phase: PointerPhase::Release,
                x,
                y,
                extend: false,
                follow: false,
            });
            let jobs = trial.take_jobs();
            if !jobs.is_empty() {
                assert_eq!(
                    jobs,
                    [Job::Operation(
                        snapshot::Operation::Install,
                        "firefox".into()
                    )]
                );
                found = Some((x, y));
                break 'scan;
            }
        }
    }
    let (x, y) = found.expect("an Install button");
    app.input(Input::Pointer {
        phase: PointerPhase::Press,
        x,
        y,
        extend: false,
        follow: false,
    });
    // Released elsewhere: nothing.
    app.input(Input::Pointer {
        phase: PointerPhase::Release,
        x: 900,
        y: 10,
        extend: false,
        follow: false,
    });
    assert!(app.take_jobs().is_empty());
}

#[test]
fn the_keys_are_spelled_as_the_keymap_spells_them() {
    let app = opened();
    let problems = td_ui::keys::check(&app.key_list());
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    let (_, _, text) = td_ui::driven::text(&app.frame()).unwrap();
    assert!(text.contains("Firefox"), "{text}");
    assert!(text.contains("not installed"), "{text}");
    assert!(text.contains("F1: keys"), "{text}");
}

#[test]
fn sizes_and_counts_read_as_a_person_reads_them() {
    use td_software::app::{count, size};
    assert_eq!(size(999), "999 bytes");
    assert_eq!(size(1000), "1.0 kB");
    assert_eq!(size(52_000_000), "52.0 MB");
    assert_eq!(size(u64::MAX), "18446744.1 TB");
    assert_eq!(count(0), "0");
    assert_eq!(count(1234567), "1,234,567");
    assert_eq!(count(100), "100");
}

#[test]
fn a_status_that_fails_later_is_shown_and_the_operation_stays_pending() {
    let mut app = opened();
    key(&mut app, "i");
    app.take_jobs();
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Ok(()),
    ));
    app.reply(Reply::Status(
        "firefox".into(),
        Box::new(Err("cannot reach the compositor: gone".into())),
    ));
    assert_eq!(app.metas()[0], "unreachable");
    let details: Vec<String> = app.details().into_iter().map(|(line, _)| line).collect();
    assert!(
        details
            .iter()
            .any(|line| line.starts_with("Its status cannot be read now")
                && line.ends_with("cannot reach the compositor: gone")),
        "{details:?}"
    );
    // Still pending: nothing can be started twice meanwhile.
    assert_eq!(app.actions()[0], (Action::Install, false));
}

#[test]
fn an_unanswered_start_is_settled_by_the_next_status() {
    let unavailable = || Failure {
        reason: "the program authority did not answer in time".into(),
        definite: false,
    };
    // It had started and ended: reported as ended.
    let mut app = opened();
    key(&mut app, "i");
    app.take_jobs();
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Err(unavailable()),
    ));
    assert!(app
        .message()
        .starts_with("The install of Firefox may not have started"));
    assert_eq!(app.actions()[0], (Action::Install, false));
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "install\tok",
            "current",
            &firefox(Some(generation(true)), None),
        ),
    );
    assert_eq!(app.message(), "Firefox is installed");
    // It never reached root: said so, and Install is offered again.
    let mut app = opened();
    key(&mut app, "i");
    app.take_jobs();
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Err(unavailable()),
    ));
    status(
        &mut app,
        "firefox",
        &answer("none", "none", "current", &firefox(None, None)),
    );
    assert_eq!(app.message(), "The install of Firefox did not start");
    assert_eq!(app.actions()[0], (Action::Install, true));
}

#[test]
fn a_press_arms_an_action_not_a_place() {
    let mut app = opened();
    let (x, y) = install_button(&app);
    app.input(Input::Pointer {
        phase: PointerPhase::Press,
        x,
        y,
        extend: false,
        follow: false,
    });
    // Installed by another path between press and release: Launch now
    // sits where Install was, and the release starts nothing.
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "none",
            "current",
            &firefox(Some(generation(true)), None),
        ),
    );
    app.input(Input::Pointer {
        phase: PointerPhase::Release,
        x,
        y,
        extend: false,
        follow: false,
    });
    assert!(app.take_jobs().is_empty());
}

#[test]
fn a_narrow_window_keeps_every_button_above_the_status_row() {
    let surface = Surface::new(200, 160, Scale::default()).unwrap();
    let mut app = installed();
    app.input(Input::Resize(surface));
    let (_, _, text) = td_ui::driven::text(&app.frame()).unwrap();
    for label in ["Launch", "Check now", "Remove"] {
        assert!(text.contains(label), "{label}:\n{text}");
    }
    // A press on the status row opens the keys and arms nothing.
    for x in (0..200).step_by(4) {
        app.input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y: 155,
            extend: false,
            follow: false,
        });
        app.input(Input::Pointer {
            phase: PointerPhase::Release,
            x,
            y: 155,
            extend: false,
            follow: false,
        });
    }
    assert!(app.take_jobs().is_empty());
    assert!(!app.dialog_open());
    assert!(app.take_show_keys());
}

#[test]
fn delete_data_is_not_offered_for_an_application_never_checked() {
    let mut app = opened();
    let never = Status {
        name: "firefox".into(),
        cache: Some(CacheStatus {
            current: None,
            previous: None,
            checked: None,
            refused: None,
            progress: None,
        }),
    }
    .render();
    status(
        &mut app,
        "firefox",
        &answer("none", "none", "current", &never),
    );
    assert_eq!(app.actions(), [(Action::Install, true)]);
}

#[test]
fn a_record_older_than_the_last_operation_holds_the_buttons() {
    let mut app = installed();
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "remove\tok",
            "older",
            &firefox(Some(generation(true)), None),
        ),
    );
    assert_eq!(
        app.actions(),
        [
            (Action::Launch, true),
            (Action::Update, false),
            (Action::Remove, false)
        ]
    );
}

#[test]
fn one_launch_at_a_time() {
    let mut app = installed();
    key(&mut app, "l");
    key(&mut app, "l");
    assert_eq!(app.take_jobs(), [Job::Launch("firefox".into())]);
    app.reply(Reply::Launch("firefox".into(), Ok(())));
    key(&mut app, "l");
    assert_eq!(app.take_jobs(), [Job::Launch("firefox".into())]);
}

#[test]
fn f5_asks_again_even_with_a_status_out() {
    let mut app = opened();
    app.tick(POLL_IDLE_MS);
    assert_eq!(app.take_jobs(), [Job::Status("firefox".into())]);
    // A status is out: F5 asks nothing twice, but the next comes at once
    // when it lands.
    key(&mut app, "F5");
    assert!(app.take_jobs().is_empty());
    status(
        &mut app,
        "firefox",
        &answer("none", "none", "current", &firefox(None, None)),
    );
    app.tick(POLL_IDLE_MS);
    assert_eq!(app.take_jobs(), [Job::Status("firefox".into())]);
}

/// Where Install is: found by pressing across the window's lower left.
fn install_button(_app: &App) -> (i64, i64) {
    for y in (0..768).rev().step_by(4) {
        for x in (0..400).step_by(4) {
            let mut trial = opened();
            for phase in [PointerPhase::Press, PointerPhase::Release] {
                trial.input(Input::Pointer {
                    phase,
                    x,
                    y,
                    extend: false,
                    follow: false,
                });
            }
            if !trial.take_jobs().is_empty() {
                return (x, y);
            }
        }
    }
    panic!("no Install button");
}

#[test]
fn a_failed_read_keeps_the_last_record_and_its_actions() {
    let mut app = installed();
    status(&mut app, "firefox", &answer("none", "none", "failed", ""));
    assert_eq!(app.standing(), Some(Standing::Installed { stale: false }));
    assert_eq!(
        app.actions(),
        [
            (Action::Launch, true),
            (Action::Update, true),
            (Action::Remove, true)
        ]
    );
    let details: Vec<String> = app.details().into_iter().map(|(line, _)| line).collect();
    assert!(details.contains(&"Root could not read its status this time.".to_string()));
}

#[test]
fn a_failed_read_shows_no_old_progress_and_leaves_actions_on() {
    let mut app = opened();
    let progress = Progress {
        stage: Stage::Fetch,
        part: Some(snapshot::Part::Application),
        objects: 1,
        objects_known: 2,
        objects_bound: 10,
        bytes: 3,
        bytes_bound: 40,
    };
    status(
        &mut app,
        "firefox",
        &answer("none", "none", "current", &firefox(None, Some(progress))),
    );
    assert_eq!(
        app.actions(),
        [(Action::Install, false), (Action::DeleteData, false)]
    );
    // A writer killed mid-download can leave a cache td-app cannot
    // read; what clears it must not wait on that writer.
    status(&mut app, "firefox", &answer("none", "none", "failed", ""));
    assert_eq!(app.metas()[0], "not installed");
    assert_eq!(
        app.actions(),
        [(Action::Install, true), (Action::DeleteData, true)]
    );
    let details: Vec<String> = app.details().into_iter().map(|(line, _)| line).collect();
    assert!(
        !details.iter().any(|line| line.starts_with("Working:")),
        "{details:?}"
    );
}

#[test]
fn a_cache_root_never_could_read_offers_remove() {
    let mut app = app();
    app.tick(0);
    app.take_jobs();
    status(&mut app, "firefox", &answer("none", "none", "failed", ""));
    assert_eq!(app.standing(), Some(Standing::Unknown));
    assert_eq!(app.metas()[0], "unreadable");
    assert_eq!(app.actions(), [(Action::Remove, true)]);
    key(&mut app, "x");
    assert!(app.dialog_open());
}

#[test]
fn asking_says_what_in_words() {
    let mut app = installed();
    key(&mut app, "c");
    assert_eq!(
        app.message(),
        "Asking root to check for an update to Firefox"
    );
}

#[test]
fn an_uncertain_start_is_not_credited_with_an_earlier_outcome() {
    let mut app = opened();
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "install\tfailed\tno route",
            "current",
            &firefox(None, None),
        ),
    );
    key(&mut app, "i");
    app.take_jobs();
    app.reply(Reply::Operation(
        snapshot::Operation::Install,
        "firefox".into(),
        Err(Failure {
            reason: "the program authority did not answer in time".into(),
            definite: false,
        }),
    ));
    status(
        &mut app,
        "firefox",
        &answer(
            "none",
            "install\tfailed\tno route",
            "current",
            &firefox(None, None),
        ),
    );
    assert_eq!(app.message(), "The install of Firefox did not start");
}

/// The launcher's Install row opens the window at an application: one the
/// image declares is selected, any other name leaves the first.
#[test]
fn a_selected_name_opens_the_window_at_its_entry() {
    let mut app = app();
    assert_eq!(app.selected(), Some("firefox"));
    assert!(app.select_name("mail"));
    assert_eq!(app.selected(), Some("mail"));
    assert!(!app.select_name("news"));
    assert_eq!(app.selected(), Some("mail"));
    assert!(app.select_name("firefox"));
    assert_eq!(app.selected(), Some("firefox"));
}
