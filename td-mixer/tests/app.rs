//! The panel without a window or a daemon: snapshots in, keys and presses
//! in, the text it paints and the actions it asks for out.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use td_mixer::app::{App, Daemon, Key};
use td_mixer::model::{AppLevel, Job, Level, Output, Snapshot, Target};
use td_mixer::worker::Reply;
use td_ui::raster::{Scale, Surface};
use td_ui::window::{Input, PointerPhase};

fn surface() -> Surface {
    Surface::new(560, 420, Scale::default()).unwrap()
}

fn level(percent: u32, muted: bool) -> Level {
    Level { percent, muted }
}

fn app_level(name: &str, percent: u32) -> AppLevel {
    AppLevel {
        name: name.into(),
        label: name.into(),
        level: level(percent, false),
    }
}

fn snapshot(master: u32, firefox: u32) -> Snapshot {
    Snapshot {
        outputs: vec![
            Output {
                name: "intel-0".into(),
                label: "Built-in Audio".into(),
                playing: true,
            },
            Output {
                name: "audio-0".into(),
                label: "USB Headset".into(),
                playing: false,
            },
        ],
        master: Some(level(master, false)),
        apps: vec![app_level("Firefox", firefox)],
    }
}

fn answer(app: &mut App, snapshot: Snapshot, answered: usize) {
    app.reply(Reply::Snapshot { snapshot, answered });
}

fn text(app: &App) -> String {
    td_ui::driven::text(&app.frame()).unwrap().2
}

fn key(app: &mut App, chord: &str) {
    app.input(Input::Key {
        chord,
        repeat: false,
    });
}

fn held(app: &mut App, chord: &str) {
    app.input(Input::Key {
        chord,
        repeat: true,
    });
}

fn pointer(app: &mut App, phase: PointerPhase, x: i64, y: i64) {
    app.input(Input::Pointer {
        phase,
        x,
        y,
        extend: false,
        follow: false,
    });
}

/// What a flush hands a worker with room for everything.
fn sent(app: &mut App) -> Vec<Job> {
    let mut jobs = Vec::new();
    app.flush(|job| {
        jobs.push(job);
        Ok::<_, ()>(None)
    })
    .unwrap();
    jobs
}

fn live(master: u32, firefox: u32) -> App {
    let mut app = App::new(surface());
    answer(&mut app, snapshot(master, firefox), 0);
    app
}

/// The list's rows start 8 pixels down, 24 apart; the slider band sits a
/// margin above the status row, its track from the 16-cell label to the
/// 11-cell value column, x 136 to 464 at 560 wide.
const ROW: i64 = 24;
fn row_y(row: i64) -> i64 {
    8 + ROW * row + ROW / 2 + 1
}
const BAND_Y: i64 = 420 - 24 - 8 - 24 + 12;

#[test]
fn before_the_daemon_answers_the_panel_says_it_is_connecting() {
    let app = App::new(surface());
    assert_eq!(app.daemon(), Daemon::Waiting);
    let shown = text(&app);
    assert!(shown.contains("Connecting"), "{shown}");
    assert!(shown.contains("F1: keys"), "{shown}");
}

#[test]
fn the_rows_show_the_level_every_output_and_each_application() {
    let app = live(45, 30);
    let shown = text(&app);
    for expected in [
        "Volume",
        "45%",
        "Built-in Audio",
        "PLAYING",
        "USB Headset",
        "Firefox",
        "30%",
    ] {
        assert!(shown.contains(expected), "{expected} in {shown}");
    }
    // The level row comes first, then the outputs, then the applications.
    let at = |needle: &str| shown.find(needle).unwrap();
    assert!(at("Volume") < at("Built-in Audio"));
    assert!(at("USB Headset") < at("Firefox"));
    assert_eq!(app.focus(), &Key::Master);
}

/// With no output playing there is no level row; the outputs still list.
#[test]
fn with_no_output_playing_the_outputs_still_list() {
    let mut app = App::new(surface());
    let mut none = snapshot(45, 30);
    none.master = None;
    none.outputs[0].playing = false;
    answer(&mut app, none, 0);
    assert_eq!(app.focus(), &Key::Output("intel-0".into()));
    let shown = text(&app);
    assert!(
        !shown.contains("Volume") && !shown.contains("PLAYING"),
        "{shown}"
    );
    key(&mut app, "Right");
    assert!(sent(&mut app).is_empty());
}

#[test]
fn keys_step_the_focused_level_and_send_where_it_ended() {
    let mut app = live(47, 30);
    key(&mut app, "Right");
    held(&mut app, "Right");
    assert_eq!(app.snapshot().master, Some(level(55, false)));
    assert!(text(&app).contains("55%"));
    // A held key sends only where it ended.
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 55)]);
    key(&mut app, "-");
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 50)]);
    for _ in 0..30 {
        key(&mut app, "+");
    }
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 100)]);
}

/// A held key repeats a step, never a mute or a switch.
#[test]
fn a_held_key_does_not_toggle() {
    let mut app = live(45, 30);
    held(&mut app, "m");
    held(&mut app, "Return");
    key(&mut app, "Down");
    key(&mut app, "Down");
    held(&mut app, "Return");
    assert!(sent(&mut app).is_empty());
}

/// The level before the unmute, as the volume keys write them, so a muted
/// level is never heard at its old level.
#[test]
fn m_mutes_and_a_step_unmutes_after_setting_the_level() {
    let mut app = live(45, 30);
    key(&mut app, "m");
    assert_eq!(sent(&mut app), [Job::Mute(Target::Master, true)]);
    answer(&mut app, snapshot(45, 30), 1);
    assert_eq!(app.snapshot().master, Some(level(45, false)));
    let mut muted = snapshot(45, 30);
    muted.master = Some(level(45, true));
    answer(&mut app, muted, 0);
    assert!(text(&app).contains("MUTED 45%"));
    key(&mut app, "Right");
    assert_eq!(
        sent(&mut app),
        [
            Job::Level(Target::Master, 50),
            Job::Mute(Target::Master, false)
        ]
    );
    // Return on a level mutes it too.
    key(&mut app, "Return");
    assert_eq!(sent(&mut app), [Job::Mute(Target::Master, true)]);
}

#[test]
fn an_applications_level_is_its_own() {
    let mut app = live(45, 30);
    key(&mut app, "End");
    assert_eq!(app.focus(), &Key::App("Firefox".into()));
    key(&mut app, "Left");
    key(&mut app, "m");
    assert_eq!(
        sent(&mut app),
        [
            Job::Level(Target::App("Firefox".into()), 25),
            Job::Mute(Target::App("Firefox".into()), true)
        ]
    );
    assert_eq!(app.snapshot().master, Some(level(45, false)));
}

#[test]
fn return_on_an_output_makes_it_play() {
    let mut app = live(45, 30);
    key(&mut app, "Down");
    key(&mut app, "Return");
    assert!(sent(&mut app).is_empty(), "the playing output is chosen");
    key(&mut app, "Tab");
    assert_eq!(app.focus(), &Key::Output("audio-0".into()));
    key(&mut app, "Left");
    assert!(sent(&mut app).is_empty(), "an output row has no level");
    key(&mut app, " ");
    assert_eq!(sent(&mut app), [Job::Choose("audio-0".into())]);
    assert!(app.snapshot().outputs[1].playing);
    key(&mut app, "S-Tab");
    assert_eq!(app.focus(), &Key::Output("intel-0".into()));
}

/// The focus stays on its row when a snapshot moves the rows, and a
/// level changed elsewhere replaces what the panel showed.
#[test]
fn a_snapshot_keeps_the_focus_and_shows_changes_made_elsewhere() {
    let mut app = live(45, 30);
    key(&mut app, "End");
    let mut more = snapshot(45, 80);
    more.outputs.remove(1);
    answer(&mut app, more, 0);
    assert_eq!(app.focus(), &Key::App("Firefox".into()));
    let shown = text(&app);
    assert!(
        shown.contains("80%") && !shown.contains("USB Headset"),
        "{shown}"
    );
    // When its row goes, the focus takes the row now in its place.
    let mut gone = snapshot(45, 80);
    gone.apps.clear();
    answer(&mut app, gone, 0);
    assert_eq!(app.focus(), &Key::Output("audio-0".into()));
}

/// An answer to an older action does not undo a newer one still in
/// flight; once every action is answered the daemon's level stands.
#[test]
fn an_older_answer_does_not_undo_a_newer_action() {
    let mut app = live(45, 30);
    key(&mut app, "Right");
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 50)]);
    key(&mut app, "Right");
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 55)]);
    assert!(app.pending());
    answer(&mut app, snapshot(50, 30), 1);
    assert_eq!(app.snapshot().master, Some(level(55, false)));
    key(&mut app, "Right");
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 60)]);
    answer(&mut app, snapshot(60, 30), 2);
    assert!(!app.pending());
    // Answered: a level set elsewhere now shows.
    answer(&mut app, snapshot(20, 30), 0);
    assert_eq!(app.snapshot().master, Some(level(20, false)));
}

#[test]
fn a_refusal_and_an_absent_daemon_are_said_and_cleared() {
    let mut app = live(45, 30);
    app.reply(Reply::Refused(
        "Refused switching output: that output would not open".into(),
    ));
    assert!(text(&app).contains("would not open"));
    answer(&mut app, snapshot(45, 30), 0);
    assert!(
        text(&app).contains("would not open"),
        "kept until an action"
    );
    key(&mut app, "Right");
    assert!(!text(&app).contains("would not open"));
    app.reply(Reply::Unavailable {
        answered: 1,
        why: None,
    });
    assert!(!app.pending());
    assert_eq!(app.daemon(), Daemon::Unavailable);
    let shown = text(&app);
    assert!(
        shown.contains("not running") && !shown.contains("Firefox"),
        "{shown}"
    );
    key(&mut app, "Right");
    assert!(sent(&mut app).is_empty());
    answer(&mut app, snapshot(45, 30), 0);
    assert_eq!(app.daemon(), Daemon::Live);
    assert!(!text(&app).contains("not running"));
}

/// What the worker's full queue gives back waits, with every newer action
/// behind it, and a newer action for the same thing replaces it.
#[test]
fn a_full_queue_keeps_actions_in_order() {
    let mut app = live(45, 30);
    key(&mut app, "Right");
    key(&mut app, "m");
    let mut taken = Vec::new();
    app.flush(|job| {
        if taken.is_empty() {
            taken.push(job);
            Ok::<_, ()>(None)
        } else {
            Ok(Some(job))
        }
    })
    .unwrap();
    assert_eq!(taken, [Job::Level(Target::Master, 50)]);
    key(&mut app, "m");
    assert_eq!(sent(&mut app), [Job::Mute(Target::Master, false)]);
    // Once each is answered nothing is in flight: a job given back was
    // never tracked.
    answer(&mut app, snapshot(50, 30), 1);
    assert!(app.pending());
    answer(&mut app, snapshot(50, 30), 1);
    assert!(!app.pending());
    let mut gone = live(45, 30);
    key(&mut gone, "Right");
    assert_eq!(gone.flush(|_| Err("gone")), Err("gone"));
}

/// A level replaced while the worker's queue is full keeps its place
/// before the unmute asked after it.
#[test]
fn a_replaced_level_stays_before_the_unmute() {
    let mut app = App::new(surface());
    let mut muted = snapshot(100, 30);
    muted.master = Some(level(100, true));
    answer(&mut app, muted, 0);
    key(&mut app, "Left");
    app.flush(|job| Ok::<_, ()>(Some(job))).unwrap();
    key(&mut app, "Left");
    assert_eq!(
        sent(&mut app),
        [
            Job::Level(Target::Master, 90),
            Job::Mute(Target::Master, false)
        ]
    );
}

/// While a choice of output is unanswered the level shown is still the
/// output's being left: the playing output's level is not stepped from
/// it, and an application's is.
#[test]
fn no_master_step_while_a_choice_is_unanswered() {
    let mut app = live(20, 30);
    key(&mut app, "Down");
    key(&mut app, "Down");
    key(&mut app, "Return");
    key(&mut app, "Home");
    key(&mut app, "Right");
    key(&mut app, "m");
    assert_eq!(sent(&mut app), [Job::Choose("audio-0".into())]);
    key(&mut app, "End");
    key(&mut app, "Right");
    assert_eq!(
        sent(&mut app),
        [Job::Level(Target::App("Firefox".into()), 35)]
    );
    let mut chosen = snapshot(90, 35);
    chosen.outputs[0].playing = false;
    chosen.outputs[1].playing = true;
    answer(&mut app, chosen, 2);
    key(&mut app, "Home");
    key(&mut app, "Right");
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 95)]);
}

#[test]
fn a_press_on_an_output_chooses_it_and_below_the_rows_does_nothing() {
    let mut app = live(45, 30);
    pointer(&mut app, PointerPhase::Press, 100, row_y(2));
    pointer(&mut app, PointerPhase::Release, 100, row_y(2));
    assert_eq!(sent(&mut app), [Job::Choose("audio-0".into())]);
    let mut app = live(45, 30);
    pointer(&mut app, PointerPhase::Press, 100, row_y(8));
    pointer(&mut app, PointerPhase::Release, 100, row_y(8));
    assert!(sent(&mut app).is_empty());
    assert_eq!(app.focus(), &Key::Master);
    // The status row lists the keys.
    pointer(&mut app, PointerPhase::Press, 100, 410);
    assert!(app.take_show_keys());
}

#[test]
fn the_slider_drags_the_focused_level() {
    let mut app = live(45, 30);
    pointer(&mut app, PointerPhase::Press, 300, BAND_Y);
    pointer(&mut app, PointerPhase::Move, 9999, BAND_Y);
    pointer(&mut app, PointerPhase::Release, 9999, BAND_Y);
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 100)]);
    // On an output row the slider has nothing to drag.
    key(&mut app, "Down");
    pointer(&mut app, PointerPhase::Press, 300, BAND_Y);
    pointer(&mut app, PointerPhase::Release, 300, BAND_Y);
    assert!(sent(&mut app).is_empty());
}

/// A drag ends when its row goes: the row taking its place is not moved.
#[test]
fn a_drag_ends_when_its_row_goes() {
    let mut app = App::new(surface());
    let mut two = snapshot(45, 80);
    two.apps.push(app_level("mpv", 30));
    answer(&mut app, two, 0);
    key(&mut app, "End");
    assert_eq!(app.focus(), &Key::App("mpv".into()));
    pointer(&mut app, PointerPhase::Press, 200, BAND_Y);
    let jobs = sent(&mut app);
    assert!(
        matches!(&jobs[..], [Job::Level(Target::App(name), _)] if name == "mpv"),
        "{jobs:?}"
    );
    answer(&mut app, snapshot(45, 80), 1);
    assert_eq!(app.focus(), &Key::App("Firefox".into()));
    pointer(&mut app, PointerPhase::Move, 150, BAND_Y);
    pointer(&mut app, PointerPhase::Release, 150, BAND_Y);
    assert!(sent(&mut app).is_empty());
    assert_eq!(app.snapshot().apps[0].level, level(80, false));
}

/// More rows than fit: the list scrolls to keep the focus shown.
#[test]
fn the_list_scrolls_to_the_focus() {
    let mut app = App::new(surface());
    let mut many = snapshot(45, 30);
    for index in 0..30 {
        many.apps.push(app_level(&format!("app{index:02}"), 10));
    }
    answer(&mut app, many, 0);
    assert!(!text(&app).contains("app29"));
    key(&mut app, "End");
    assert_eq!(app.focus(), &Key::App("app29".into()));
    let shown = text(&app);
    assert!(
        shown.contains("app29") && !shown.contains("Built-in"),
        "{shown}"
    );
    app.input(Input::Wheel {
        rows: -100,
        columns: 0,
    });
    assert_eq!(app.focus(), &Key::Master);
    assert!(text(&app).contains("Built-in"));
}

/// As the window runs, each action flushed at once: with the choice of
/// output in flight, the playing output's step, mute and slider do
/// nothing, an unasked snapshot does not lift that, and the choice's
/// answer does.
#[test]
fn no_master_action_while_a_choice_is_in_flight() {
    let mut app = live(20, 30);
    key(&mut app, "Down");
    key(&mut app, "Down");
    key(&mut app, "Return");
    assert_eq!(sent(&mut app), [Job::Choose("audio-0".into())]);
    key(&mut app, "Home");
    key(&mut app, "Right");
    key(&mut app, "m");
    pointer(&mut app, PointerPhase::Press, 300, BAND_Y);
    pointer(&mut app, PointerPhase::Move, 400, BAND_Y);
    pointer(&mut app, PointerPhase::Release, 400, BAND_Y);
    assert!(sent(&mut app).is_empty());
    let mut unasked = snapshot(20, 30);
    unasked.outputs[0].playing = false;
    unasked.outputs[1].playing = true;
    answer(&mut app, unasked, 0);
    key(&mut app, "Right");
    assert!(sent(&mut app).is_empty());
    let mut chosen = snapshot(90, 30);
    chosen.outputs[0].playing = false;
    chosen.outputs[1].playing = true;
    answer(&mut app, chosen, 1);
    key(&mut app, "Right");
    assert_eq!(sent(&mut app), [Job::Level(Target::Master, 95)]);
}

#[test]
fn escape_closes_and_the_keys_are_listed() {
    let mut app = live(45, 30);
    assert_eq!(app.key_list().len(), 1);
    key(&mut app, "Escape");
    assert!(app.quitting());
}
