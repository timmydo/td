#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The cull controller in-process: the action table held to the seam's
//! grammar and to its own alignment, scripted sessions held to `state`,
//! the text read-back and the frame digest, and the built binary's
//! `--replay` over a temporary roll, writing through the sidecar.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use td_photo::library::{self, Filter, Flag, Key, Sidecar};
use td_photo::look;
use td_photo::ui::{self, Action, Controller, Effect, Photo, View, BINDINGS};
use td_ui::chrome::DISABLED;
use td_ui::control::{frame, hex, valid_code, Decoder, ErrorCode};
use td_ui::driven::{self, Input, Outcome, PointerPhase};
use td_ui::finder;
use td_ui::raster::{Composition, Primitive, Rect, Scale, Surface, CHROME, MISSPELLED, SELECTED};
use td_ui::CELL_HEIGHT;

#[path = "support/synth_nef.rs"]
mod synth_nef;

const MODE: usize = 0;
const SHOWN: usize = 3;
const POSITION: usize = 4;
const VIEW: usize = 6;
const NAME: usize = 7;
const FLAG: usize = 8;
const EXPOSURE: usize = 9;
const CROP: usize = 10;
const LOOK: usize = 11;
const STATUS: usize = 12;
const JOBS: usize = 13;
const GENERATION: usize = 14;
const CHOOSER: usize = 15;
const STEPS: usize = 16;
const STEP: usize = 17;

fn surface(width: usize, height: usize) -> Surface {
    Surface::new(width, height, Scale::new(1).unwrap()).unwrap()
}

fn fields(controller: &Controller) -> Vec<String> {
    controller.state().split('\t').map(str::to_string).collect()
}

/// The wire's spelling of the `i`th test photo's name.
fn name(i: usize) -> String {
    hex(format!("DSC_{i:04}.NEF").as_bytes())
}

/// The `i`th test photo's file name: what an effect carries, since it
/// names the file the adapter writes, not the wire's hex.
fn file(i: usize) -> String {
    format!("DSC_{i:04}.NEF")
}

fn edits(text: &str) -> Sidecar {
    Sidecar::parse(text.as_bytes()).unwrap()
}

/// `count` photos: the second a pick, the third a reject, the fourth with
/// a refused sidecar, the rest with none.
fn photos(count: usize) -> Vec<Photo> {
    (0..count)
        .map(|i| {
            let mut photo = Photo {
                name: format!("DSC_{i:04}.NEF"),
                ..Photo::default()
            };
            match i {
                1 => photo.sidecar = Some(edits("td-photo edit 1\nflag pick\n")),
                2 => photo.sidecar = Some(edits("td-photo edit 1\nflag reject\n")),
                3 => photo.error = Some("malformed exposure value".to_string()),
                _ => {}
            }
            photo
        })
        .collect()
}

fn act(controller: &mut Controller, name: &str, arguments: &[&str]) -> Outcome {
    let (outcome, effects) = controller.action(name, arguments).unwrap();
    assert!(effects.is_empty(), "{name} asked for {effects:?}");
    outcome
}

fn key(controller: &mut Controller, chord: &str) -> Outcome {
    controller.input(Input::Key { chord }).unwrap().0
}

fn press(controller: &mut Controller, x: u32, y: u32) -> Outcome {
    controller
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y,
        })
        .unwrap()
        .0
}

fn drag_to(controller: &mut Controller, x: u32, y: u32) -> Outcome {
    controller
        .input(Input::Pointer {
            phase: PointerPhase::Move,
            x,
            y,
        })
        .unwrap()
        .0
}

/// A pointer release carries effects (a crop commit), so it returns the
/// full pair, unlike `press`/`drag_to` which never emit one.
fn release(controller: &mut Controller, x: u32, y: u32) -> (Outcome, Vec<Effect>) {
    controller
        .input(Input::Pointer {
            phase: PointerPhase::Release,
            x,
            y,
        })
        .unwrap()
}

#[test]
fn the_action_table_is_closed_aligned_and_reachable() {
    driven::check(&BINDINGS).unwrap();
    assert_eq!(Action::ALL.len(), BINDINGS.len());
    for (action, binding) in Action::ALL.iter().zip(BINDINGS.iter()) {
        assert_eq!(action.name(), binding.name);
        assert_eq!(Action::parse(binding.name), Some(*action));
        // An action without a key takes an argument or is the agent's; the
        // pointer reaches `select` by a press and `scroll` by the wheel.
        match binding.chord {
            None => {
                assert!(
                    ["open", "select", "scroll", "look", "crop", "aspect", "exposure"]
                        .contains(&binding.name),
                    "{} has no key",
                    binding.name
                );
                assert!(!binding.arguments.is_empty());
            }
            Some(_) => assert!(binding.arguments.is_empty(), "{}", binding.name),
        }
    }
    assert_eq!(Action::parse("colour"), None);
    // `LookAt` is bounded by name: no look-0, and none past the ninth.
    assert_eq!(Action::parse("look-0"), None);
    assert_eq!(Action::parse("look-10"), None);
    assert_eq!(Action::LookAt(0).name(), "look-0");
    assert_eq!(Action::LookAt(10).name(), "look-0");
    let help = driven::help(&BINDINGS);
    for binding in BINDINGS {
        assert!(help.contains(binding.name) && help.contains(binding.help));
    }
    for error in [
        ui::Error::NoRoll,
        ui::Error::NoPhoto,
        ui::Error::BadArgument,
        ui::Error::Refused,
        ui::Error::Transport(td_ui::control::Error::Limit),
    ] {
        assert!(valid_code(error.code()), "{error}");
    }
    assert_eq!(ui::Error::NoRoll.code(), "no-roll");
    assert_eq!(
        ui::Error::from(td_ui::control::Error::Protocol).code(),
        "protocol"
    );
    assert_eq!(ui::CELL_W, 176);
    assert_eq!(ui::CELL_H, 152);
    assert_eq!(ui::MAX_PHOTOS, 100_000);
    for (filter, word) in [
        (Filter::All, "all"),
        (Filter::Picks, "picks"),
        (Filter::Rejects, "rejects"),
        (Filter::Unflagged, "unflagged"),
    ] {
        assert_eq!(filter.word(), word);
    }
}

#[test]
fn a_session_walks_the_grid_and_reports_its_state() {
    let mut c = Controller::new(surface(800, 600));
    let layout = c.layout();
    assert_eq!((layout.columns, layout.rows), (4, 3));
    assert_eq!(
        c.state(),
        "cull\t-\t0\t0\t-\tall\tgrid\t-\t-\t-\t-\t-\t-\t0\t0\t-\t0\t-"
    );
    for name in ["next", "pick", "view", "first"] {
        assert_eq!(
            c.action(name, &[]).unwrap_err(),
            ui::Error::NoRoll,
            "{name}"
        );
    }
    assert_eq!(c.photo(0).unwrap_err(), ui::Error::NoRoll);
    assert_eq!(c.action("next", &["1"]).unwrap_err().code(), "protocol");
    assert_eq!(c.action("nothing", &[]).unwrap_err().code(), "protocol");
    // `open` is the agent's: the dispatcher asks the adapter to read it.
    let (outcome, effects) = c.action("open", &["2f72"]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(effects, [Effect::Open(b"/r".to_vec())]);
    assert_eq!(c.action("open", &["zz"]).unwrap_err().code(), "protocol");
    // The generation is the adapter's `open` to bump, not the dispatch's.
    assert_eq!(fields(&c)[GENERATION], "0");

    c.open("roll", b"/r", photos(10)).unwrap();
    let s = fields(&c);
    assert_eq!(
        &s[MODE..=VIEW],
        ["cull", "2f72", "10", "10", "0", "all", "grid"]
    );
    assert_eq!(
        &s[NAME..=STATUS],
        [name(0).as_str(), "-", "-", "-", "-", "none"]
    );
    assert_eq!((s[JOBS].as_str(), s[GENERATION].as_str()), ("0", "1"));
    assert_eq!(c.cursor(), Some(0));
    assert_eq!(c.roll(), Some(&b"/r"[..]));

    // Walking: a step at an end is ignored; pages are a screen of rows.
    assert_eq!(act(&mut c, "next", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[POSITION..=FLAG][..1], ["1"]);
    assert_eq!(fields(&c)[FLAG], "pick");
    assert_eq!(fields(&c)[STATUS], "ok");
    assert_eq!(act(&mut c, "down", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "5");
    assert_eq!(act(&mut c, "up", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "1");
    assert_eq!(act(&mut c, "previous", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "previous", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "last", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "9");
    assert_eq!(act(&mut c, "last", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "first", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "page-down", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "9");
    assert_eq!(act(&mut c, "page-up", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "0");
    assert_eq!(act(&mut c, "select", &["3"]), Outcome::Changed);
    assert_eq!(act(&mut c, "select", &["3"]), Outcome::Ignored);
    assert_eq!(fields(&c)[STATUS], "error");
    assert_eq!(c.action("select", &["10"]).unwrap_err(), ui::Error::NoPhoto);
    assert_eq!(c.action("select", &["x"]).unwrap_err().code(), "protocol");
    assert_eq!(c.action("select", &[]).unwrap_err().code(), "protocol");
    assert_eq!(
        c.photo(0).unwrap(),
        format!("{}\t-\t-\t-\t-\tnone\t-", name(0))
    );
    assert_eq!(
        c.photo(3).unwrap(),
        format!(
            "{}\t-\t-\t-\t-\terror\t{}",
            name(3),
            hex(b"malformed exposure value")
        )
    );
    assert_eq!(c.photo(10).unwrap_err(), ui::Error::NoPhoto);

    // Filters: the cursor stays when still shown, else goes to the first
    // shown; the refused sidecar counts as unflagged.
    assert_eq!(act(&mut c, "picks", &[]), Outcome::Changed);
    assert_eq!(
        &fields(&c)[SHOWN..=NAME],
        ["1", "0", "picks", "grid", name(1).as_str()]
    );
    assert_eq!(act(&mut c, "picks", &[]), Outcome::Ignored);
    assert_eq!(c.photo(1).unwrap_err(), ui::Error::NoPhoto);
    assert_eq!(act(&mut c, "rejects", &[]), Outcome::Changed);
    assert_eq!(
        &fields(&c)[SHOWN..=NAME],
        ["1", "0", "rejects", "grid", name(2).as_str()]
    );
    assert_eq!(act(&mut c, "unflagged", &[]), Outcome::Changed);
    assert_eq!(
        &fields(&c)[SHOWN..=NAME],
        ["8", "0", "unflagged", "grid", name(0).as_str()]
    );
    assert_eq!(c.shown(), [0, 3, 4, 5, 6, 7, 8, 9]);
    assert_eq!(act(&mut c, "all", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[SHOWN], "10");

    // The single view and back, by action and by key; unbound keys are
    // ignored, bound ones dispatch.
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(c.view(), View::Single);
    assert_eq!(fields(&c)[VIEW], "single");
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(key(&mut c, "Return"), Outcome::Changed);
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!(c.view(), View::Grid);
    assert_eq!(key(&mut c, "z"), Outcome::Ignored);
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(key(&mut c, "Home"), Outcome::Changed);
    assert_eq!(key(&mut c, "3"), Outcome::Changed);
    assert_eq!(c.filter(), Filter::Rejects);
    assert_eq!(key(&mut c, "1"), Outcome::Changed);
    assert_eq!(act(&mut c, "quit", &[]), Outcome::Quit);
    assert_eq!(c.input(Input::Focus(true)).unwrap().0, Outcome::Ignored);
    assert_eq!(c.input(Input::Tick(5)).unwrap().0, Outcome::Ignored);

    // Every row fits on 800x600, so nothing scrolls; on 400x300 two
    // columns and one row show, and the grid scrolls without moving the
    // cursor, clamped to the roll.
    assert_eq!(act(&mut c, "scroll", &["1"]), Outcome::Ignored);
    assert_eq!(act(&mut c, "first", &[]), Outcome::Changed);
    let before = fields(&c)[GENERATION].clone();
    assert_eq!(
        c.input(Input::Resize {
            width: 400,
            height: 300,
            scale: 1
        })
        .unwrap()
        .0,
        Outcome::Changed
    );
    assert_ne!(fields(&c)[GENERATION], before);
    let before = fields(&c)[GENERATION].clone();
    assert_eq!(
        c.input(Input::Resize {
            width: 400,
            height: 300,
            scale: 1
        })
        .unwrap()
        .0,
        Outcome::Ignored
    );
    assert_eq!(fields(&c)[GENERATION], before);
    assert_eq!((c.layout().columns, c.layout().rows), (2, 1));
    assert_eq!(c.first_row(), 0);
    assert_eq!(act(&mut c, "scroll", &["1"]), Outcome::Changed);
    assert_eq!(c.first_row(), 1);
    assert_eq!(fields(&c)[POSITION], "0");
    assert_eq!(
        c.input(Input::Wheel {
            rows: -1,
            columns: 0
        })
        .unwrap()
        .0,
        Outcome::Changed
    );
    assert_eq!(c.first_row(), 0);
    assert_eq!(act(&mut c, "scroll", &["100"]), Outcome::Changed);
    assert_eq!(c.first_row(), 4);
    assert_eq!(act(&mut c, "scroll", &["-100"]), Outcome::Changed);
    assert_eq!(c.first_row(), 0);
    assert_eq!(
        c.action("scroll", &["16777217"]).unwrap_err(),
        ui::Error::BadArgument
    );
    // Moving the cursor reveals its row.
    assert_eq!(act(&mut c, "last", &[]), Outcome::Changed);
    assert_eq!(c.first_row(), 4);
    assert_eq!(act(&mut c, "first", &[]), Outcome::Changed);
    assert_eq!(c.first_row(), 0);
    for (width, height, scale) in [(0, 300, 1), (400, 0, 1), (400, 300, 0), (400, 300, 5)] {
        assert_eq!(
            c.input(Input::Resize {
                width,
                height,
                scale
            })
            .unwrap_err(),
            ui::Error::BadArgument
        );
    }

    // The pointer: a filter button sets the filter, a cell selects, the
    // rest is ignored, and only a press does anything; the gap between
    // two buttons and the strip's margin are no targets.
    assert_eq!(press(&mut c, 80, 36), Outcome::Changed);
    assert_eq!(c.filter(), Filter::Picks);
    assert_eq!(press(&mut c, 80, 36), Outcome::Ignored);
    assert_eq!(press(&mut c, 10, 36), Outcome::Changed);
    assert_eq!(c.filter(), Filter::All);
    assert_eq!(press(&mut c, 52, 36), Outcome::Ignored);
    assert_eq!(press(&mut c, 80, 25), Outcome::Ignored);
    assert_eq!(act(&mut c, "first", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 300, 100), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "1");
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(press(&mut c, 300, 290), Outcome::Ignored);
    // Off the 400x300 surface nothing is a target, not even a strip's row.
    assert_eq!(press(&mut c, 400, 36), Outcome::Ignored);
    assert_eq!(press(&mut c, 10, 300), Outcome::Ignored);
    assert_eq!(c.filter(), Filter::All);
    assert_eq!(
        c.input(Input::Pointer {
            phase: PointerPhase::Move,
            x: 10,
            y: 100
        })
        .unwrap()
        .0,
        Outcome::Ignored
    );
    // On a surface too short for all the bands the status row paints over
    // the strips, so a press there is the status row's, never a button's:
    // at 64 tall it covers the filter strip's lower half, at 40 the mode
    // strip's, and the part left showing is still a target.
    let mut short = Controller::new(surface(400, 64));
    short.open("roll", b"/r", photos(2)).unwrap();
    assert_eq!(press(&mut short, 80, 44), Outcome::Ignored);
    assert_eq!(short.filter(), Filter::All);
    assert_eq!(press(&mut short, 80, 30), Outcome::Changed);
    assert_eq!(short.filter(), Filter::Picks);
    let mut short = Controller::new(surface(400, 40));
    short.open("roll", b"/r", photos(2)).unwrap();
    assert_eq!(press(&mut short, 230, 20), Outcome::Ignored);
    assert_eq!(short.mode(), ui::Mode::Cull);
    assert_eq!(press(&mut short, 230, 10), Outcome::Changed);
    assert_eq!(short.mode(), ui::Mode::Develop);
    // In the single view a press anywhere in the area between the bands
    // returns to the grid, the picture's margin as much as the picture;
    // the status row does not.
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 300, 290), Outcome::Ignored);
    assert_eq!(c.view(), View::Single);
    assert_eq!(press(&mut c, 20, 60), Outcome::Changed);
    assert_eq!(c.view(), View::Grid);
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 200, 150), Outcome::Changed);
    assert_eq!(c.view(), View::Grid);
    // A press past the last shown photo selects nothing.
    assert_eq!(act(&mut c, "picks", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);

    let mut empty = Controller::new(surface(800, 600));
    empty.open("empty", b"/e", Vec::new()).unwrap();
    assert_eq!(empty.cursor(), None);
    assert_eq!(empty.action("next", &[]).unwrap_err(), ui::Error::NoPhoto);
    assert_eq!(empty.action("pick", &[]).unwrap_err(), ui::Error::NoPhoto);
    assert_eq!(empty.action("view", &[]).unwrap_err(), ui::Error::NoPhoto);
    assert_eq!(press(&mut empty, 100, 100), Outcome::Ignored);
    assert_eq!(
        empty
            .open("big", b"/b", photos(ui::MAX_PHOTOS + 1))
            .unwrap_err(),
        ui::Error::Refused
    );
}

/// The binary's adapter over the in-process model, for the tests that
/// carry effects by hand: each effect mutates the photo's sidecar as it
/// stands (the model's copy here stands in for the file), then settles it.
/// A refused sidecar is never rewritten; a mutation that leaves the sidecar
/// as it was writes nothing and is `Ignored` unless the settle itself
/// brought a change; anything else is `Changed`. The delta and clamp of
/// exposure live here, as they do in the binary.
fn apply(c: &mut Controller, effects: &[Effect]) -> Result<Outcome, ui::Error> {
    let mut outcome = Outcome::Changed;
    for effect in effects {
        let (index, name) = match effect {
            Effect::Flag { index, name, .. }
            | Effect::Edit { index, name, .. }
            | Effect::Expose { index, name, .. }
            | Effect::Reset { index, name }
            | Effect::Undo { index, name }
            | Effect::StepToggle { index, name, .. }
            | Effect::StepDelete { index, name, .. }
            | Effect::Export { index, name } => (*index, name.clone()),
            Effect::DeleteRejected => {
                // The adapter moves the rejects the files flag; here the
                // model's copies stand in for the files.
                let rejects: Vec<String> = c
                    .photos()
                    .iter()
                    .filter(|photo| photo.error.is_none() && photo.flag() == Some(Flag::Reject))
                    .map(|photo| photo.name.clone())
                    .collect();
                outcome = if c.remove(&rejects) {
                    Outcome::Changed
                } else {
                    Outcome::Ignored
                };
                continue;
            }
            Effect::Open(_) | Effect::List { .. } => panic!("{effect:?}"),
        };
        if let Effect::Export { .. } = effect {
            // The adapter writes no sidecar for an export: it reports.
            c.set_export(Some(format!("exported {name}")));
            outcome = Outcome::Changed;
            continue;
        }
        let photo = &c.photos()[index];
        if photo.error.is_some() {
            return Err(ui::Error::Refused);
        }
        let mut sidecar = photo.sidecar.clone().unwrap_or_default();
        let before = sidecar.clone();
        match effect {
            Effect::Flag { flag, .. } => sidecar.set(Key::Flag, flag.map(Flag::word)).unwrap(),
            Effect::Edit { key, value, .. } => sidecar.set(*key, value.as_deref()).unwrap(),
            Effect::Expose { delta, .. } => {
                let current = sidecar.exposure().unwrap_or(0);
                let next = current
                    .saturating_add(*delta)
                    .clamp(-library::MAX_EXPOSURE, library::MAX_EXPOSURE);
                sidecar
                    .set(Key::Exposure, Some(&library::exposure_text(next)))
                    .unwrap();
            }
            Effect::Reset { .. } => sidecar.reset(),
            Effect::Undo { .. } => {
                sidecar.undo();
            }
            Effect::StepToggle { step, .. } => {
                sidecar.toggle_step(*step);
            }
            Effect::StepDelete { step, .. } => {
                sidecar.delete_step(*step);
            }
            Effect::Open(_)
            | Effect::Export { .. }
            | Effect::DeleteRejected
            | Effect::List { .. } => {
                panic!("{effect:?}")
            }
        }
        let unchanged = sidecar == before;
        let changed = c.settle(
            index,
            Photo {
                name,
                sidecar: Some(sidecar),
                error: None,
            },
        );
        outcome = if unchanged && !changed {
            Outcome::Ignored
        } else {
            Outcome::Changed
        };
    }
    Ok(outcome)
}

/// Runs an action and carries its effects on the model as the adapter
/// would, returning the settled outcome. For the actions that emit
/// effects, where `act`, which forbids them, cannot be used.
fn carry(c: &mut Controller, name: &str, args: &[&str]) -> Outcome {
    let (_, effects) = c.action(name, args).unwrap();
    apply(c, &effects).unwrap()
}

/// `carry` for a key: the chord's effects carried on the model.
fn carry_key(c: &mut Controller, chord: &str) -> Outcome {
    let (_, effects) = c.input(Input::Key { chord }).unwrap();
    apply(c, &effects).unwrap()
}

#[test]
fn flags_change_the_sidecar_through_effects_and_never_a_refused_one() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    let text = |c: &Controller, index: usize| c.photos()[index].sidecar.as_ref().map(Sidecar::text);
    let (outcome, effects) = c.action("pick", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    // The effect carries the intent, not a sidecar: the adapter sets it on
    // the file as it is then, and the model waits to be settled.
    assert_eq!(
        effects,
        [Effect::Flag {
            index: 0,
            name: "DSC_0000.NEF".to_string(),
            flag: Some(Flag::Pick),
        }]
    );
    assert_eq!(text(&c, 0), None);
    assert_eq!(&fields(&c)[FLAG], "-");
    assert_eq!(&fields(&c)[GENERATION], "1");
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(text(&c, 0).as_deref(), Some("td-photo edit 1\nflag pick\n"));
    assert_eq!(&fields(&c)[FLAG], "pick");
    assert_eq!(&fields(&c)[GENERATION], "2");
    assert_eq!(c.photos()[0].flag(), Some(Flag::Pick));
    // The flag the model thinks the photo has is asked for all the same,
    // since the file may differ; the adapter calls it ignored when the
    // file holds it, and the model, unchanged, keeps its generation.
    let (outcome, effects) = c.action("pick", &[]).unwrap();
    assert_eq!((outcome, effects.len()), (Outcome::Changed, 1));
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Ignored);
    assert_eq!(&fields(&c)[GENERATION], "2");
    let (_, effects) = c.action("reject", &[]).unwrap();
    assert!(matches!(
        &effects[..],
        [Effect::Flag {
            index: 0,
            flag: Some(Flag::Reject),
            ..
        }]
    ));
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(
        text(&c, 0).as_deref(),
        Some("td-photo edit 1\nflag reject\n")
    );
    let (_, effects) = c.action("unflag", &[]).unwrap();
    assert!(matches!(
        &effects[..],
        [Effect::Flag {
            index: 0,
            flag: None,
            ..
        }]
    ));
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(text(&c, 0).as_deref(), Some("td-photo edit 1\n"));
    assert_eq!(fields(&c)[FLAG], "-");
    assert_eq!(fields(&c)[STATUS], "ok");
    // Through the keys, with an unknown line kept in place.
    assert_eq!(act(&mut c, "select", &["4"]), Outcome::Changed);
    c.settle(
        4,
        Photo {
            name: "DSC_0004.NEF".to_string(),
            sidecar: Some(edits("td-photo edit 1\nfuture 1\nexposure -0.33\n")),
            error: None,
        },
    );
    let (outcome, effects) = c.input(Input::Key { chord: "p" }).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert!(matches!(
        &effects[..],
        [Effect::Flag {
            index: 4,
            flag: Some(Flag::Pick),
            ..
        }]
    ));
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(
        text(&c, 4).as_deref(),
        Some("td-photo edit 1\nfuture 1\nexposure -0.33\nflag pick\nstep-1 on exposure -0.33\n")
    );
    assert_eq!(c.photos()[4].value(Key::Exposure), "-0.33");
    // A refused sidecar is never rewritten: the dispatch asks, since the
    // file may have been mended meanwhile, and the adapter refuses what it
    // cannot read, leaving the model as it was.
    assert_eq!(act(&mut c, "select", &["3"]), Outcome::Changed);
    let before = fields(&c)[GENERATION].clone();
    let (_, effects) = c.action("pick", &[]).unwrap();
    assert_eq!(apply(&mut c, &effects).unwrap_err(), ui::Error::Refused);
    let (_, effects) = c.input(Input::Key { chord: "x" }).unwrap();
    assert_eq!(apply(&mut c, &effects).unwrap_err(), ui::Error::Refused);
    assert_eq!(c.photos()[3].sidecar, None);
    assert_eq!(c.photos()[3].status(), "error");
    assert_eq!(fields(&c)[GENERATION], before);
    // Under a filter, a flag that hides the photo moves the cursor on once
    // it is settled, and the single view ends with the last shown photo.
    assert_eq!(act(&mut c, "picks", &[]), Outcome::Changed);
    assert_eq!(c.shown(), [1, 4]);
    assert_eq!(fields(&c)[NAME], name(1));
    let (outcome, effects) = c.action("unflag", &[]).unwrap();
    assert_eq!((outcome, effects.len()), (Outcome::Changed, 1));
    assert_eq!((c.shown(), c.cursor()), (&[1usize, 4][..], Some(1)));
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(c.shown(), [4]);
    assert_eq!(
        &fields(&c)[SHOWN..=NAME],
        ["1", "0", "picks", "grid", name(4).as_str()]
    );
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    let (_, effects) = c.action("reject", &[]).unwrap();
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(c.shown(), []);
    assert_eq!(c.cursor(), None);
    assert_eq!((c.view(), fields(&c)[VIEW].as_str()), (View::Grid, "grid"));
    assert_eq!(fields(&c)[NAME], "-");
    assert_eq!(c.action("pick", &[]).unwrap_err(), ui::Error::NoPhoto);
    // A settle that brings what the model holds is no change, as after a
    // write the file refused; one that differs is, and a photo the filter
    // then shows takes the cursor.
    let before = fields(&c)[GENERATION].clone();
    assert!(!c.settle(4, c.photos()[4].clone()));
    assert_eq!(fields(&c)[GENERATION], before);
    assert!(c.settle(
        4,
        Photo {
            name: "DSC_0004.NEF".to_string(),
            sidecar: None,
            error: Some("Permission denied".to_string()),
        },
    ));
    assert_ne!(fields(&c)[GENERATION], before);
    assert_eq!((c.photos()[4].status(), c.cursor()), ("error", None));
    c.settle(
        4,
        Photo {
            name: "DSC_0004.NEF".to_string(),
            sidecar: Some(edits("td-photo edit 1\nflag pick\n")),
            error: None,
        },
    );
    assert_eq!(c.shown(), [4]);
    assert_eq!(
        &fields(&c)[SHOWN..=NAME],
        ["1", "0", "picks", "grid", name(4).as_str()]
    );
    // An index past the roll settles nothing.
    let before = fields(&c)[GENERATION].clone();
    assert!(!c.settle(99, Photo::default()));
    assert_eq!(c.photos().len(), 5);
    assert_eq!(fields(&c)[GENERATION], before);
    // The roll's sidecar budget is what the photos hold between them.
    assert_eq!(Photo::default().bytes(), 0);
    assert_eq!(photos(2)[1].bytes(), "flag".len() + "pick".len() + 2);
    let notes = |count: usize| {
        Some(edits(&format!(
            "td-photo edit 1\nnotes {}\n",
            "x".repeat(count)
        )))
    };
    let big = Photo {
        name: "big.NEF".to_string(),
        sidecar: notes(32_761),
        error: None,
    };
    assert_eq!(big.bytes(), 32_768);
    let mut roll = Controller::new(surface(800, 600));
    assert_eq!(
        roll.open("big", b"/b", vec![big.clone(); 2_049])
            .unwrap_err(),
        ui::Error::Refused
    );
    assert_eq!(roll.roll(), None);
    // Exactly the budget is admitted, and nothing more fits.
    roll.open("big", b"/b", vec![big.clone(); 2_048]).unwrap();
    assert_eq!(roll.photos().len(), 2_048);
    assert!(roll.fits(0, 32_768) && !roll.fits(0, 32_769));
    assert!(!roll.fits(2_048, 0));
    // The budget holds at every settle too: a sidecar that grew past it
    // since the roll opened is not held, and the photo is shown as refused.
    let bigger = Photo {
        sidecar: notes(32_762),
        ..big.clone()
    };
    assert!(roll.settle(0, bigger));
    assert_eq!(
        (
            roll.photos()[0].sidecar.as_ref(),
            roll.photos()[0].error.as_deref()
        ),
        (None, Some(ui::OVER_BUDGET))
    );
    assert!(roll.fits(0, 32_768) && !roll.fits(0, 32_769));
    assert!(roll.settle(0, big.clone()));
    assert_eq!(roll.photos()[0], big);
}

#[test]
fn develop_edits_act_on_the_cursor_only_in_develop_mode() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();

    // In the cull mode the develop edits are not this mode's: each is
    // ignored, asks for no effect, and the generation does not move.
    assert_eq!(c.mode(), ui::Mode::Cull);
    let quiet = fields(&c)[GENERATION].clone();
    for name in [
        "expose-in",
        "expose-out",
        "expose-in-fine",
        "expose-out-fine",
        "reset",
    ] {
        assert_eq!(act(&mut c, name, &[]), Outcome::Ignored, "{name}");
    }
    assert_eq!(act(&mut c, "look", &["portra"]), Outcome::Ignored);
    assert_eq!(
        act(&mut c, "crop", &["0.1", "0.1", "0.5", "0.5"]),
        Outcome::Ignored
    );
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], quiet);

    // Entering needs the cursor's photo; the mode word then leads the
    // state, and entering again, by verb or the `d` key, is ignored.
    assert_eq!(fields(&c)[POSITION], "0");
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_eq!(fields(&c)[MODE], "develop");
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Ignored);
    assert_eq!(key(&mut c, "d"), Outcome::Ignored);

    // Exposure is a delta the adapter adds to the file's value, not the
    // model's copy: the effect names only the photo and the step, and the
    // value shows once the photo is settled. The coarse step is a third of
    // a stop, the fine a tenth, in and out.
    let (outcome, effects) = c.action("expose-in", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Expose {
            index: 0,
            name: file(0),
            delta: ui::EXPOSURE_STEP,
        }]
    );
    assert_eq!(fields(&c)[EXPOSURE], "-");
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "0.33");
    assert_eq!(carry(&mut c, "expose-in-fine", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "0.43");
    assert_eq!(carry(&mut c, "expose-out", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "0.10");
    assert_eq!(carry(&mut c, "expose-out-fine", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "0.00");

    // A look is an absolute Edit; an unknown stem never reaches a write,
    // judged bad-argument here, and `-` clears it.
    let (_, effects) = c.action("look", &["portra"]).unwrap();
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Look,
            value: Some("portra".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[LOOK], "portra");
    assert_eq!(
        c.action("look", &["not a look"]).unwrap_err(),
        ui::Error::BadArgument
    );
    assert_eq!(carry(&mut c, "look", &["-"]), Outcome::Changed);
    assert_eq!(fields(&c)[LOOK], "-");

    // A crop is an absolute Edit; a box under the minimum edge or outside
    // the image is bad-argument, and the wrong arity is a protocol fault.
    let (_, effects) = c
        .action("crop", &["0.1000", "0.1000", "0.5000", "0.5000"])
        .unwrap();
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.1000 0.1000 0.5000 0.5000".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.1000 0.1000 0.5000 0.5000");
    assert_eq!(
        c.action("crop", &["0.1000", "0.1000", "0.0010", "0.5000"])
            .unwrap_err(),
        ui::Error::BadArgument
    );
    assert_eq!(
        c.action("crop", &["0.9000", "0.1000", "0.5000", "0.5000"])
            .unwrap_err(),
        ui::Error::BadArgument
    );
    assert!(c.action("crop", &["0.1000", "0.1000", "0.5000"]).is_err());

    // A flag is the cull decision still, set in develop too; reset then
    // clears the develop keys and keeps the flag.
    assert_eq!(carry(&mut c, "pick", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[FLAG], "pick");
    let (_, effects) = c.action("reset", &[]).unwrap();
    assert_eq!(
        effects,
        [Effect::Reset {
            index: 0,
            name: file(0),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(&fields(&c)[EXPOSURE..=LOOK], ["-", "-", "-"]);
    assert_eq!(fields(&c)[FLAG], "pick");

    // The cull filters and the single-view toggle are not develop's: each
    // is ignored and leaves the mode and the filter where they were.
    for name in ["all", "picks", "rejects", "unflagged", "view"] {
        assert_eq!(act(&mut c, name, &[]), Outcome::Ignored, "{name}");
    }
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_eq!(c.filter(), Filter::All);

    // Escape leaves develop for the cull grid, and `d` enters it again.
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Cull);
    assert_eq!(fields(&c)[MODE], "cull");
    assert_eq!(c.view(), View::Grid);
    assert_eq!(key(&mut c, "d"), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Develop);
}

#[test]
fn losing_the_cursor_drops_develop_back_to_the_cull_grid() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    // photos() flags photo 1 a pick; under the picks filter it is alone,
    // so develop it, then reject it: the filter no longer shows it, the
    // shown set empties, the cursor is lost, and develop gives way to the
    // cull grid.
    assert_eq!(act(&mut c, "picks", &[]), Outcome::Changed);
    assert_eq!(c.shown(), [1]);
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_eq!(carry(&mut c, "reject", &[]), Outcome::Changed);
    assert_eq!(c.shown(), []);
    assert_eq!(c.cursor(), None);
    assert_eq!(c.mode(), ui::Mode::Cull);
    assert_eq!(c.view(), View::Grid);
    assert_eq!(fields(&c)[MODE], "cull");
    // With no photo, entering is a no-photo fault and the develop edits are
    // ignored as they are in any cull view.
    assert_eq!(c.action("develop", &[]).unwrap_err(), ui::Error::NoPhoto);
    assert_eq!(act(&mut c, "expose-in", &[]), Outcome::Ignored);
    // With no roll at all, entering is a no-roll fault.
    let mut empty = Controller::new(surface(800, 600));
    assert_eq!(empty.action("develop", &[]).unwrap_err(), ui::Error::NoRoll);
}

#[test]
fn develop_entered_from_the_single_view_reports_the_grid() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(3)).unwrap();
    // Enter develop from the cull single view; the reported view is the
    // grid, since `grid`/Escape leaves develop for the grid, not the single
    // view it was entered from.
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[VIEW], "single");
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(
        (c.mode(), fields(&c)[VIEW].as_str()),
        (ui::Mode::Develop, "grid")
    );
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!((c.mode(), c.view()), (ui::Mode::Cull, View::Grid));
}

#[test]
fn the_scene_reads_back_as_text_and_paints_deterministically() {
    let mut c = Controller::new(surface(800, 600));
    let (rows, columns, text) = driven::text(&c.scene()).unwrap();
    assert_eq!((rows, columns), (37, 100));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0].trim(), "Roll Selection   Culling   Develop");
    assert_eq!(lines[1].trim(), "All   Picks   Rejects   Unflagged");
    let strip = lines[1].to_string();
    assert!(text.contains("No roll open."), "{text}");
    assert_eq!(lines.last().unwrap().trim(), "No roll open");

    c.open("roll", b"/r", photos(6)).unwrap();
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.last().unwrap().trim(),
        "roll | 6 photos, 6 shown | all | 1/6 DSC_0000.NEF unflagged"
    );
    // Four cells across: the names on one row, the badges above them.
    assert!(
        lines[11].contains("DSC_0000.NEF") && lines[11].contains("DSC_0003.NEF"),
        "{text}"
    );
    let second = lines
        .iter()
        .position(|line| line.contains("DSC_0004.NEF"))
        .unwrap();
    assert_eq!(second, 20, "{text}");
    assert!(lines[second].contains("DSC_0005.NEF"));
    assert_eq!(lines[3].trim(), "P                     X", "{}", lines[3]);
    let frame = driven::paint(&c.scene()).unwrap();
    assert_eq!((frame.surface.width, frame.surface.height), (800, 600));
    assert_eq!(frame.rgb.len(), 800 * 600 * 3);
    let digest = driven::fnv1a64(&frame.rgb);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        digest
    );
    // A flag once settled, a move and a filter each change the frame.
    let (_, effects) = c.action("pick", &[]).unwrap();
    apply(&mut c, &effects).unwrap();
    let picked = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    assert_ne!(picked, digest);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert_eq!(
        text.lines().nth(3).unwrap().trim(),
        "P                     P                     X"
    );
    act(&mut c, "next", &[]);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        picked
    );
    act(&mut c, "rejects", &[]);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1].trim(), "All   Picks   Rejects   Unflagged");
    // The buttons keep their places whichever filter is active: the
    // active one is styled, not marked in the text.
    for filter in ["picks", "unflagged", "all", "rejects"] {
        act(&mut c, filter, &[]);
        let (_, _, text) = driven::text(&c.scene()).unwrap();
        let row = text.lines().nth(1).unwrap().to_string();
        for header in ["All", "Picks", "Rejects", "Unflagged"] {
            assert_eq!(row.find(header), strip.find(header), "{filter} {header}");
        }
    }
    assert!(lines[11].contains("DSC_0002.NEF") && !lines[11].contains("DSC_0000.NEF"));
    assert_eq!(
        lines.last().unwrap().trim(),
        "roll | 6 photos, 1 shown | rejects | 1/1 DSC_0002.NEF reject"
    );
    // The single view: the name, the facts and the status say so.
    act(&mut c, "view", &[]);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[3].trim(), "DSC_0002.NEF");
    assert_eq!(
        lines[4].trim(),
        "reject | exposure - | crop - | look - | sidecar ok"
    );
    assert!(lines.last().unwrap().ends_with("| single"));
    act(&mut c, "grid", &[]);
    // Nothing shown, and nothing at all, each say so.
    act(&mut c, "select", &["0"]);
    let (_, effects) = c.action("unflag", &[]).unwrap();
    apply(&mut c, &effects).unwrap();
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(text.contains("No photo passes the filter."), "{text}");
    let mut empty = Controller::new(surface(800, 600));
    empty.open("empty", b"/e", Vec::new()).unwrap();
    let (_, _, text) = driven::text(&empty.scene()).unwrap();
    assert!(text.contains("The roll holds no photos."), "{text}");
    assert_eq!(
        text.lines().last().unwrap().trim(),
        "empty | 0 photos, 0 shown | all"
    );
    // A refused sidecar is said in the status row.
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(4)).unwrap();
    act(&mut c, "last", &[]);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert_eq!(
        text.lines().last().unwrap().trim(),
        "roll | 4 photos, 4 shown | all | 4/4 DSC_0003.NEF unflagged (sidecar refused)"
    );
    // Scale 2 doubles the cells; the read-back grid halves.
    c.input(Input::Resize {
        width: 800,
        height: 600,
        scale: 2,
    })
    .unwrap();
    assert_eq!((c.layout().columns, c.layout().rows), (2, 1));
    let (rows, columns, text) = driven::text(&c.scene()).unwrap();
    assert_eq!((rows, columns), (18, 50));
    assert!(
        text.contains("DSC_0002.NEF") && text.contains("DSC_0003.NEF"),
        "{text}"
    );
}

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-photo-ui-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn request(id: u64, words: &[&str]) -> Vec<u8> {
    frame(format!("1\t{id}\t{}", words.join("\t")).as_bytes()).unwrap()
}

/// One `td-photo --replay ARGS` session, driven in steps so the roll can
/// change on disk between them.
struct Replay {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    pending: VecDeque<u8>,
    decoder: Decoder,
}

impl Replay {
    fn start(args: &[&str]) -> Replay {
        let mut child = Command::new(env!("CARGO_BIN_EXE_td-photo"))
            .arg("--replay")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        Replay {
            child,
            stdin,
            stdout,
            pending: VecDeque::new(),
            decoder: Decoder::default(),
        }
    }

    /// Writes the requests (a session's replies are far under the pipe's
    /// buffer), then reads one reply per request, fewer if the session
    /// ended, each as its fields after `1`: the ID, `ok` or `error`, the
    /// body.
    fn send(&mut self, requests: &[Vec<u8>]) -> Vec<Vec<String>> {
        if let Some(stdin) = self.stdin.as_mut() {
            for request in requests {
                stdin.write_all(request).unwrap();
            }
        }
        self.read(requests.len())
    }

    fn read(&mut self, count: usize) -> Vec<Vec<String>> {
        let mut replies = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            while let Some(byte) = self.pending.pop_front() {
                self.decoder.push(&[byte]).unwrap();
                if self.decoder.payload().is_some() {
                    let payload = std::mem::take(&mut self.decoder).finish().unwrap();
                    let line = String::from_utf8(payload).unwrap();
                    let fields: Vec<String> = line.split('\t').map(str::to_string).collect();
                    assert_eq!(fields[0], "1");
                    replies.push(fields[1..].to_vec());
                    if replies.len() == count {
                        return replies;
                    }
                }
            }
            let n = self.stdout.read(&mut chunk).unwrap();
            if n == 0 {
                return replies;
            }
            self.pending.extend(&chunk[..n]);
        }
    }

    /// Closes the session and returns whether it exited well, the replies
    /// not yet read and its stderr.
    fn finish(mut self) -> (bool, Vec<Vec<String>>, String) {
        drop(self.stdin.take());
        let rest = self.read(usize::MAX);
        let mut err = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        let ok = self.child.wait().unwrap().success();
        (ok, rest, err)
    }
}

/// Runs `td-photo --replay ARGS` over the requests in one step.
fn replay(args: &[&str], requests: &[Vec<u8>]) -> (bool, Vec<Vec<String>>, String) {
    let mut session = Replay::start(args);
    let mut replies = session.send(requests);
    let (ok, rest, err) = session.finish();
    replies.extend(rest);
    (ok, replies, err)
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_binary_replays_the_cull_over_a_roll_and_writes_through_the_sidecar() {
    let temp = Temp::new("replay");
    let roll = temp.0.join("2026/2026-09-13");
    fs::create_dir_all(&roll).unwrap();
    for name in ["DSC_0001.NEF", "DSC_0002.NEF", "DSC_0003.NEF"] {
        fs::write(roll.join(name), b"not really a nef").unwrap();
    }
    fs::write(
        roll.join("DSC_0002.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    fs::write(
        roll.join("DSC_0003.NEF.edit"),
        "td-photo edit 1\nexposure 9.00\n",
    )
    .unwrap();
    fs::write(roll.join("notes.txt"), b"x").unwrap();
    let roll_s = roll.to_str().unwrap();
    let roll_hex = hex(roll_s.as_bytes());
    let missing_hex = hex(temp.0.join("missing").to_str().unwrap().as_bytes());
    let requests = [
        request(1, &["state"]),
        request(2, &["actions"]),
        request(3, &["action", "pick"]),
        request(4, &["photo", "0"]),
        request(5, &["photo", "2"]),
        request(6, &["action", "select", "2"]),
        request(7, &["action", "pick"]),
        request(8, &["key", &hex(b"Right")]),
        request(9, &["key", &hex(b"Home")]),
        request(10, &["text"]),
        request(11, &["frame"]),
        request(12, &["frame-page", "0", "16"]),
        request(13, &["wheel", "1", "0"]),
        request(14, &["resize", "800", "600", "1"]),
        request(15, &["action", "quit"]),
        request(16, &["bogus"]),
        request(17, &["action", "open", &missing_hex]),
        request(18, &["state", "extra"]),
        request(19, &["photo", "9"]),
        request(20, &["state"]),
        request(21, &["action", "pick"]),
        request(22, &["state"]),
    ];
    let (ok, replies, err) = replay(&["--size", "500x300", roll_s], &requests);
    assert!(ok, "{err}");
    assert_eq!(replies.len(), requests.len());
    let reply = |id: usize| -> &[String] { &replies[id - 1][1..] };
    assert_eq!(
        reply(1),
        [
            "ok",
            "cull",
            &roll_hex,
            "3",
            "3",
            "0",
            "all",
            "grid",
            &name(1),
            "-",
            "-",
            "-",
            "-",
            "none",
            "0",
            "1",
            "-",
            "0",
            "-"
        ]
    );
    assert_eq!(&reply(2)[..2], ["ok", "49"]);
    assert_eq!(reply(3), ["ok", "changed"]);
    assert_eq!(reply(4), ["ok", &name(1), "pick", "-", "-", "-", "ok", "-"]);
    assert_eq!(
        reply(5),
        [
            "ok",
            &name(3),
            "-",
            "-",
            "-",
            "-",
            "error",
            &hex(b"malformed exposure value")
        ]
    );
    assert_eq!(reply(6), ["ok", "changed"]);
    assert_eq!(reply(7)[..2], ["error", "refused"]);
    assert_eq!(reply(8), ["ok", "ignored"]);
    assert_eq!(reply(9), ["ok", "changed"]);
    assert_eq!(&reply(10)[..3], ["ok", "18", "62"]);
    let text = String::from_utf8(td_ui::control::unhex(&reply(10)[3]).unwrap()).unwrap();
    assert!(
        text.contains("DSC_0001.NEF") && text.contains("DSC_0002.NEF"),
        "{text}"
    );
    assert!(text.ends_with("| all | 1/3 DSC_0001.NEF pick"), "{text}");
    assert_eq!(&reply(11)[..4], ["ok", "500", "300", "1"]);
    assert_eq!(reply(11)[4].len(), 16);
    assert_eq!(&reply(12)[..4], ["ok", "500", "300", "0"]);
    assert_eq!(reply(12)[4].len(), 32);
    assert_eq!(reply(13), ["ok", "changed"]);
    assert_eq!(reply(14), ["ok", "changed"]);
    assert_eq!(reply(15), ["ok", "quit"]);
    assert_eq!(reply(16)[..2], ["error", "protocol"]);
    assert_eq!(reply(17)[..2], ["error", "refused"]);
    assert_eq!(reply(18)[..2], ["error", "protocol"]);
    assert_eq!(reply(19)[..2], ["error", "no-photo"]);
    assert_eq!(
        &reply(20)[..8],
        ["ok", "cull", &roll_hex, "3", "3", "0", "all", "grid"]
    );
    // The flag the file already holds is the adapter's `ignored`, and the
    // model, settled to a file it matched, keeps its generation.
    assert_eq!(reply(21), ["ok", "ignored"]);
    assert_eq!(reply(22)[15], reply(20)[15]);
    assert!(err.contains("missing"), "{err}");
    // The pick went through the sidecar; the refused one was left alone;
    // nothing else was written or unlinked.
    assert_eq!(
        fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        "td-photo edit 1\nflag pick\n"
    );
    assert_eq!(
        fs::read_to_string(roll.join("DSC_0003.NEF.edit")).unwrap(),
        "td-photo edit 1\nexposure 9.00\n"
    );
    assert_eq!(
        names(&roll),
        [
            "DSC_0001.NEF",
            "DSC_0001.NEF.edit",
            "DSC_0002.NEF",
            "DSC_0002.NEF.edit",
            "DSC_0003.NEF",
            "DSC_0003.NEF.edit",
            "notes.txt",
        ]
    );
    // The flag is set on the file as it is when the action arrives, not
    // on the copy the model took at open: an edit made meanwhile is kept,
    // and a sidecar that became malformed refuses the flag and settles the
    // model to what the file holds.
    let mut session = Replay::start(&["--size", "500x300", roll_s]);
    let replies = session.send(&[request(1, &["state"])]);
    assert_eq!(&replies[0][9..11], [name(1), "pick".to_string()]);
    fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure -0.50\n",
    )
    .unwrap();
    let replies = session.send(&[request(2, &["action", "reject"]), request(3, &["state"])]);
    assert_eq!(&replies[0][1..], ["ok", "changed"]);
    assert_eq!(&replies[1][9..12], [name(1).as_str(), "reject", "-0.50"]);
    assert_eq!(replies[1][14], "ok");
    assert_eq!(
        fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        "td-photo edit 1\nexposure -0.50\nflag reject\nstep-1 on exposure -0.50\n"
    );
    fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 9.00\n",
    )
    .unwrap();
    let replies = session.send(&[
        request(4, &["action", "pick"]),
        request(5, &["state"]),
        request(6, &["photo", "0"]),
    ]);
    assert_eq!(&replies[0][1..3], ["error", "refused"]);
    assert_eq!(&replies[1][9..11], [name(1).as_str(), "-"]);
    assert_eq!(replies[1][14], "error");
    assert_eq!(
        &replies[2][1..],
        [
            "ok",
            &name(1),
            "-",
            "-",
            "-",
            "-",
            "error",
            &hex(b"malformed exposure value")
        ]
    );
    let (ok, rest, err) = session.finish();
    assert!(ok && rest.is_empty(), "{err}");
    assert!(err.contains("DSC_0001.NEF.edit"), "{err}");
    assert_eq!(
        fs::read_to_string(roll.join("DSC_0001.NEF.edit")).unwrap(),
        "td-photo edit 1\nexposure 9.00\n"
    );
    assert!(!names(&roll).iter().any(|n| n.ends_with(".tmp")));

    // Opening later, by action; a write the file system refuses settles
    // the model to the file and puts the cursor back on the photo; a name
    // that is not ASCII goes over the wire in hex like any other.
    let accented = "Z\u{e9}.NEF";
    fs::write(roll.join(accented), b"not really a nef").unwrap();
    fs::write(roll.join("DSC_0002.NEF.edit.tmp"), b"stale").unwrap();
    let requests = [
        request(1, &["state"]),
        request(2, &["action", "open", &roll_hex]),
        request(3, &["state"]),
        request(4, &["action", "rejects"]),
        request(5, &["action", "unflag"]),
        request(6, &["state"]),
        request(7, &["action", "all"]),
        request(8, &["action", "select", "3"]),
        request(9, &["state"]),
        request(10, &["photo", "3"]),
    ];
    let (ok, replies, err) = replay(&[], &requests);
    assert!(ok, "{err}");
    assert_eq!(replies.len(), requests.len());
    let reply = |id: usize| -> &[String] { &replies[id - 1][1..] };
    assert_eq!(&reply(1)[..3], ["ok", "cull", "-"]);
    assert_eq!(reply(2), ["ok", "changed"]);
    assert_eq!(&reply(3)[..4], ["ok", "cull", &roll_hex, "4"]);
    assert_eq!(reply(4), ["ok", "changed"]);
    assert_eq!(&reply(5)[..2], ["error", "refused"]);
    assert_eq!(
        &reply(6)[..14],
        [
            "ok",
            "cull",
            &roll_hex,
            "4",
            "1",
            "0",
            "rejects",
            "grid",
            &name(2),
            "reject",
            "-",
            "-",
            "-",
            "ok"
        ]
    );
    // The refused flag left the generation where the filter put it.
    assert_eq!(reply(6)[15], "2");
    assert_eq!(reply(7), ["ok", "changed"]);
    assert_eq!(reply(8), ["ok", "changed"]);
    assert_eq!(&reply(9)[8..10], [hex(accented.as_bytes()).as_str(), "-"]);
    assert_eq!(&reply(10)[..2], ["ok", &hex(accented.as_bytes())]);
    assert!(err.contains("DSC_0002.NEF.edit.tmp"), "{err}");
    assert_eq!(
        fs::read(roll.join("DSC_0002.NEF.edit.tmp")).unwrap(),
        b"stale"
    );
    assert_eq!(
        fs::read_to_string(roll.join("DSC_0002.NEF.edit")).unwrap(),
        "td-photo edit 1\nflag reject\n"
    );
    // A roll that is not there, a bad size and a stray argument refuse
    // the session before it starts; `--help actions` is the table.
    for args in [
        &[temp.0.join("missing").to_str().unwrap()][..],
        &["--size", "0x0"],
        &["--size", "wide"],
        &["--size"],
        &["--bogus"],
        &[roll_s, roll_s],
    ] {
        // Nothing is sent: a session refused at its start reads no request,
        // and a write to it would race its exit.
        let (ok, replies, _) = Replay::start(args).finish();
        assert!(!ok && replies.is_empty(), "{args:?}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(["--help", "actions"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        driven::help(&BINDINGS)
    );
    // `--replay --help` is the help, as after every verb.
    let help = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("--help")
        .output()
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(["--replay", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success() && output.status.success());
    assert!(!help.stdout.is_empty());
    assert_eq!(output.stdout, help.stdout);
}

#[test]
fn the_binary_develops_a_photo_over_the_effects_and_writes_the_sidecar() {
    let temp = Temp::new("develop");
    let roll = temp.0.join("2026/2026-09-13");
    fs::create_dir_all(&roll).unwrap();
    for name in ["DSC_0001.NEF", "DSC_0002.NEF"] {
        fs::write(roll.join(name), b"not really a nef").unwrap();
    }
    // The second photo starts near the exposure ceiling and flagged, to
    // show the clamp and that a reset keeps the flag.
    fs::write(
        roll.join("DSC_0002.NEF.edit"),
        "td-photo edit 1\nexposure 4.90\nflag pick\n",
    )
    .unwrap();
    let roll_s = roll.to_str().unwrap();
    let edit_1 = roll.join("DSC_0001.NEF.edit");

    let mut session = Replay::start(&["--size", "500x300", roll_s]);
    let a = session.send(&[
        request(1, &["state"]),
        request(2, &["action", "develop"]),
        request(3, &["state"]),
        request(4, &["action", "expose-in"]),
        request(5, &["action", "expose-in-fine"]),
        request(6, &["action", "expose-out"]),
        request(7, &["action", "look", "portra"]),
        request(
            8,
            &["action", "crop", "0.1000", "0.1000", "0.5000", "0.5000"],
        ),
        request(9, &["state"]),
        request(10, &["action", "look", "not a look"]),
        request(
            11,
            &["action", "crop", "0.9000", "0.1000", "0.5000", "0.5000"],
        ),
        request(12, &["state"]),
    ]);
    // Cull at first, then develop: the mode word leads the state.
    assert_eq!(
        (a[0][2].as_str(), a[0][9].as_str()),
        ("cull", name(1).as_str())
    );
    assert_eq!(&a[1][1..], ["ok", "changed"]);
    assert_eq!((a[2][2].as_str(), a[2][11].as_str()), ("develop", "-"));
    // Three exposure nudges (one step, each taking the last's), a look and
    // a crop, each a change; the value is the file's, read back in the
    // state.
    for reply in &a[3..8] {
        assert_eq!(&reply[1..], ["ok", "changed"]);
    }
    assert_eq!(
        &a[8][11..14],
        ["0.10", "0.1000 0.1000 0.5000 0.5000", "portra"]
    );
    // An unknown look stem and a crop outside the image never reach a
    // write; the state is what it was.
    assert_eq!(&a[9][1..3], ["error", "bad-argument"]);
    assert_eq!(&a[10][1..3], ["error", "bad-argument"]);
    assert_eq!((a[11][11].as_str(), a[11][13].as_str()), ("0.10", "portra"));
    assert_eq!(
        fs::read_to_string(&edit_1).unwrap(),
        "td-photo edit 1\nexposure 0.10\nlook portra\ncrop 0.1000 0.1000 0.5000 0.5000\nstep-1 on exposure 0.10\nstep-2 on look portra\nstep-3 on crop 0.1000 0.1000 0.5000 0.5000\n"
    );

    // The exposure delta is added to the file's value, not the model's: an
    // edit made since the roll opened is added to, not overwritten.
    fs::write(&edit_1, "td-photo edit 1\nexposure 1.00\n").unwrap();
    let b = session.send(&[
        request(13, &["action", "expose-in"]),
        request(14, &["state"]),
        request(15, &["action", "reset"]),
        request(16, &["state"]),
        request(17, &["action", "next"]),
        request(18, &["state"]),
        request(19, &["action", "expose-in"]),
        request(20, &["state"]),
        request(21, &["action", "expose-in"]),
        request(22, &["state"]),
        request(23, &["action", "reset"]),
        request(24, &["state"]),
        request(25, &["action", "grid"]),
        request(26, &["state"]),
        request(27, &["action", "expose-in"]),
    ]);
    // 1.00 on the file, not 0.10 in the model, is the base for the delta.
    assert_eq!(&b[0][1..], ["ok", "changed"]);
    assert_eq!(&b[1][11..14], ["1.33", "-", "-"]);
    // Reset clears the develop keys and keeps the flag; here there is none.
    assert_eq!(&b[2][1..], ["ok", "changed"]);
    assert_eq!(&b[3][10..14], ["-", "-", "-", "-"]);
    // Develop follows the cursor: next moves to the flagged photo.
    assert_eq!(&b[4][1..], ["ok", "changed"]);
    assert_eq!(
        (
            b[5][2].as_str(),
            b[5][9].as_str(),
            b[5][10].as_str(),
            b[5][11].as_str()
        ),
        ("develop", name(2).as_str(), "pick", "4.90")
    );
    // A step over the ceiling clamps to it; a further step is no change.
    assert_eq!(&b[6][1..], ["ok", "changed"]);
    assert_eq!(b[7][11], "5.00");
    assert_eq!(&b[8][1..], ["ok", "ignored"]);
    assert_eq!(b[9][11], "5.00");
    assert_eq!(b[9][16], b[7][16]);
    // Reset on the flagged photo clears its exposure but keeps the flag,
    // through the adapter onto the file.
    assert_eq!(&b[10][1..], ["ok", "changed"]);
    assert_eq!((b[11][10].as_str(), b[11][11].as_str()), ("pick", "-"));
    // Escape or grid leaves develop, where the edits are ignored again.
    assert_eq!(&b[12][1..], ["ok", "changed"]);
    assert_eq!(b[13][2], "cull");
    assert_eq!(&b[14][1..], ["ok", "ignored"]);

    let (ok, _rest, err) = session.finish();
    assert!(ok, "{err}");
    // Each photo's reset cleared its develop keys; the first had no flag,
    // the second's pick was kept; nothing else was written.
    assert_eq!(fs::read_to_string(&edit_1).unwrap(), "td-photo edit 1\n");
    assert_eq!(
        fs::read_to_string(roll.join("DSC_0002.NEF.edit")).unwrap(),
        "td-photo edit 1\nflag pick\n"
    );
    assert_eq!(
        names(&roll),
        [
            "DSC_0001.NEF",
            "DSC_0001.NEF.edit",
            "DSC_0002.NEF",
            "DSC_0002.NEF.edit",
        ]
    );
}

#[test]
fn the_window_helpers_place_thumbnails_and_report_jobs() {
    use td_photo::image::Rgb8;
    use td_ui::raster::Rect;
    // 800 by 600 holds four columns and three rows: twelve cells a screen.
    let mut c = Controller::new(surface(800, 600));
    assert!(c.visible().is_empty() && c.wanted().is_empty());
    c.open("roll", b"/r", photos(30)).unwrap();
    assert_eq!((c.layout().columns, c.layout().rows), (4, 3));
    let visible = c.visible();
    let indices = |visible: &[(usize, Rect)]| visible.iter().map(|(i, _)| *i).collect::<Vec<_>>();
    assert_eq!(indices(&visible), (0..12).collect::<Vec<_>>());
    // The box sits under the strips and the cell's padding; the fifth
    // cell begins the second row, the second the second column.
    let first = Rect {
        x: 8,
        y: 56,
        width: 160,
        height: 120,
    };
    assert_eq!(visible[0].1, first);
    assert_eq!(
        visible[1].1,
        Rect {
            x: 8 + 176,
            ..first
        }
    );
    assert_eq!(
        visible[4].1,
        Rect {
            y: 56 + 152,
            ..first
        }
    );
    // Two screens from the top row, then the screen above it.
    assert_eq!(c.wanted(), (0..24).collect::<Vec<_>>());
    act(&mut c, "scroll", &["2"]);
    assert_eq!(c.first_row(), 2);
    assert_eq!(indices(&c.visible()), (8..20).collect::<Vec<_>>());
    let wanted = c.wanted();
    assert_eq!(wanted[..22], (8..30).collect::<Vec<_>>()[..]);
    assert_eq!(wanted[22..], (0..8).collect::<Vec<_>>()[..]);
    // The single view paints no thumbnail and wants the grid's.
    act(&mut c, "select", &["9"]);
    act(&mut c, "view", &[]);
    assert!(c.visible().is_empty());
    assert_eq!(c.wanted(), wanted);
    act(&mut c, "grid", &[]);
    // Under a filter the boxes follow the shown positions, not the indices.
    let (_, effects) = c.action("pick", &[]).unwrap();
    apply(&mut c, &effects).unwrap();
    act(&mut c, "picks", &[]);
    assert_eq!(
        c.visible(),
        [
            (1, first),
            (
                9,
                Rect {
                    x: 8 + 176,
                    ..first
                }
            )
        ]
    );
    assert_eq!(c.wanted(), [1, 9]);
    act(&mut c, "all", &[]);
    // The job count is a fact for `state`, not a change; a touch is one.
    let generation = c.generation();
    c.set_jobs(3);
    assert_eq!(c.jobs(), 3);
    assert_eq!(fields(&c)[13], "3");
    assert_eq!(c.generation(), generation);
    c.touch();
    assert_eq!(c.generation(), generation + 1);
    assert_eq!(fields(&c)[14], (generation + 1).to_string());
    // A held key repeats the moves and the pages, nothing else.
    for action in td_photo::ui::Action::ALL {
        let moves = matches!(
            action.name(),
            "next" | "previous" | "down" | "up" | "page-down" | "page-up"
        );
        assert_eq!(action.repeats(), moves, "{}", action.name());
    }

    // The blitter: centred in the box, BGRX, clipped to the box and the
    // surface, the middle of an image larger than its box.
    let surface = surface(20, 10);
    let stride = 20 * 4;
    let image = Rgb8 {
        width: 4,
        height: 2,
        data: (0..24).collect(),
    };
    let mut pixels = vec![0xAAu8; stride * 10];
    let at = |pixels: &[u8], x: usize, y: usize| {
        pixels[y * stride + x * 4..y * stride + x * 4 + 4].to_vec()
    };
    let r#box = Rect {
        x: 2,
        y: 1,
        width: 8,
        height: 4,
    };
    ui::blit(
        &mut pixels,
        surface,
        stride,
        surface.bounds(),
        r#box,
        &image,
    )
    .unwrap();
    assert_eq!(at(&pixels, 4, 2), [2, 1, 0, 0]);
    assert_eq!(at(&pixels, 5, 2), [5, 4, 3, 0]);
    assert_eq!(at(&pixels, 7, 3), [23, 22, 21, 0]);
    for (x, y) in [(3, 2), (8, 2), (4, 1), (4, 4)] {
        assert_eq!(at(&pixels, x, y), [0xAA; 4], "({x}, {y})");
    }
    let mut pixels = vec![0u8; stride * 10];
    let r#box = Rect {
        x: -2,
        y: -1,
        width: 4,
        height: 2,
    };
    ui::blit(
        &mut pixels,
        surface,
        stride,
        surface.bounds(),
        r#box,
        &image,
    )
    .unwrap();
    assert_eq!(at(&pixels, 0, 0), [20, 19, 18, 0]);
    assert_eq!(at(&pixels, 1, 0), [23, 22, 21, 0]);
    assert_eq!(at(&pixels, 2, 0), [0; 4]);
    assert_eq!(at(&pixels, 0, 1), [0; 4]);
    let r#box = Rect {
        x: 5,
        y: 5,
        width: 2,
        height: 2,
    };
    ui::blit(
        &mut pixels,
        surface,
        stride,
        surface.bounds(),
        r#box,
        &image,
    )
    .unwrap();
    assert_eq!(at(&pixels, 5, 5), [5, 4, 3, 0]);
    assert_eq!(at(&pixels, 6, 6), [20, 19, 18, 0]);
    assert_eq!(at(&pixels, 4, 5), [0; 4]);
    assert_eq!(at(&pixels, 7, 6), [0; 4]);
    let r#box = Rect {
        x: 30,
        y: 30,
        width: 4,
        height: 4,
    };
    ui::blit(
        &mut pixels,
        surface,
        stride,
        surface.bounds(),
        r#box,
        &image,
    )
    .unwrap();
    // Clipped to the given area as well: a box that runs under it leaves
    // the rows past it alone; a box at an axis's end is nothing to paint.
    let mut pixels = vec![0u8; stride * 10];
    let r#box = Rect {
        x: 0,
        y: 6,
        width: 4,
        height: 2,
    };
    let clip = Rect {
        x: 0,
        y: 0,
        width: 20,
        height: 7,
    };
    ui::blit(&mut pixels, surface, stride, clip, r#box, &image).unwrap();
    assert_eq!(at(&pixels, 0, 6), [2, 1, 0, 0]);
    assert_eq!(at(&pixels, 3, 6), [11, 10, 9, 0]);
    assert!(pixels[stride * 7..].iter().all(|b| *b == 0));
    let far = Rect {
        x: i64::MIN,
        y: 0,
        width: 1,
        height: 1,
    };
    let line = Rgb8 {
        width: 3,
        height: 1,
        data: vec![9; 9],
    };
    ui::blit(&mut pixels, surface, stride, surface.bounds(), far, &line).unwrap();
    assert!(pixels[..stride * 6].iter().all(|b| *b == 0));
    // A stride or a frame short of the surface, or an image not its size.
    assert!(matches!(
        ui::blit(
            &mut pixels,
            surface,
            stride - 4,
            surface.bounds(),
            r#box,
            &image
        ),
        Err(ui::Error::BadArgument)
    ));
    assert!(matches!(
        ui::blit(
            &mut pixels[..stride * 9],
            surface,
            stride,
            surface.bounds(),
            r#box,
            &image
        ),
        Err(ui::Error::BadArgument)
    ));
    let short = Rgb8 {
        width: 4,
        height: 2,
        data: vec![0; 23],
    };
    assert!(matches!(
        ui::blit(
            &mut pixels,
            surface,
            stride,
            surface.bounds(),
            r#box,
            &short
        ),
        Err(ui::Error::BadArgument)
    ));

    // The in-memory shrink keeps what fits and fits the rest by the
    // thumbnail rule's rounding, never enlarging.
    let same = Rgb8 {
        width: 160,
        height: 107,
        data: vec![7; 160 * 107 * 3],
    };
    assert_eq!(
        td_photo::develop::shrink(same.clone(), 160, 120, 1).unwrap(),
        same
    );
    let tall = Rgb8 {
        width: 107,
        height: 160,
        data: vec![9; 107 * 160 * 3],
    };
    let fitted = td_photo::develop::shrink(tall, 160, 120, 1).unwrap();
    assert_eq!((fitted.width, fitted.height), (80, 120));
    assert!(fitted.data.iter().all(|v| *v == 9));
    let wide = Rgb8 {
        width: 400,
        height: 100,
        data: vec![0; 400 * 100 * 3],
    };
    let fitted = td_photo::develop::shrink(wide, 160, 120, 1).unwrap();
    assert_eq!((fitted.width, fitted.height), (160, 40));
    let square = Rgb8 {
        width: 300,
        height: 300,
        data: vec![0; 300 * 300 * 3],
    };
    let fitted = td_photo::develop::shrink(square, 160, 120, 1).unwrap();
    assert_eq!((fitted.width, fitted.height), (120, 120));
    let odd = Rgb8 {
        width: 2,
        height: 2,
        data: vec![0; 3],
    };
    assert!(td_photo::develop::shrink(odd, 1, 1, 1).is_err());
    // A box axis past the image ceiling is the ceiling, not an overflow.
    let tiny = Rgb8 {
        width: 2,
        height: 2,
        data: vec![0; 12],
    };
    let fitted = td_photo::develop::shrink(tiny, usize::MAX, 1, 1).unwrap();
    assert_eq!((fitted.width, fitted.height), (1, 1));

    // The badges alone: over the scene's own frame they change nothing;
    // over a thumbnail that covered one they paint it back and leave the
    // rest of the thumbnail as blitted.
    use td_ui::raster::{Raster, Scale, Surface};
    let big = Surface::new(400, 300, Scale::default()).unwrap();
    let mut c = Controller::new(big);
    c.open("roll", b"/r", photos(3)).unwrap();
    let big_stride = 400 * 4;
    let font = td_ui::font::pinned().unwrap();
    let mut pixels = vec![0u8; big_stride * 300];
    Raster::new(&mut pixels, &font, big, big_stride)
        .unwrap()
        .paint(&c.scene(), big.bounds())
        .unwrap();
    let scene_only = pixels.clone();
    Raster::new(&mut pixels, &font, big, big_stride)
        .unwrap()
        .paint(&c.badges(), c.layout().area)
        .unwrap();
    assert_eq!(pixels, scene_only);
    let (index, r#box) = c.visible()[1];
    assert_eq!(index, 1, "the pick is the second cell");
    let pixel = |pixels: &[u8], x: i64, y: i64| {
        let at = y as usize * big_stride + x as usize * 4;
        pixels[at..at + 4].to_vec()
    };
    let corner = pixel(&scene_only, r#box.x, r#box.y);
    assert_ne!(corner, [0xc7, 0xd1, 0xd6, 0], "the badge is at the corner");
    let flat = Rgb8 {
        width: 160,
        height: 120,
        data: vec![0x40; 160 * 120 * 3],
    };
    ui::blit(&mut pixels, big, big_stride, big.bounds(), r#box, &flat).unwrap();
    assert_eq!(pixel(&pixels, r#box.x, r#box.y), [0x40, 0x40, 0x40, 0]);
    Raster::new(&mut pixels, &font, big, big_stride)
        .unwrap()
        .paint(&c.badges(), c.layout().area)
        .unwrap();
    // The badge's glyph cell is back and the thumbnail past it stays.
    let (bw, bh) = ((td_ui::CELL_WIDTH + 2) as i64, td_ui::CELL_HEIGHT as i64);
    for y in r#box.y..r#box.y + i64::from(r#box.height) {
        for x in r#box.x..r#box.x + i64::from(r#box.width) {
            let expected = if x < r#box.x + bw && y < r#box.y + bh {
                pixel(&scene_only, x, y)
            } else {
                vec![0x40, 0x40, 0x40, 0]
            };
            assert_eq!(pixel(&pixels, x, y), expected, "({x}, {y})");
        }
    }
    // On a surface too short for a cell the status band covers the badge's
    // lower rows; painted within the grid's area, as the window paints
    // them, the badges leave the band as the scene left it, and painted
    // over the whole surface they would not.
    let short = Surface::new(400, 84, Scale::default()).unwrap();
    let mut c = Controller::new(short);
    c.open("roll", b"/r", photos(3)).unwrap();
    let area = c.layout().area;
    assert_eq!((area.y, area.height), (48, 12));
    assert_eq!(c.visible()[1].0, 1, "the pick is on the one row");
    let mut pixels = vec![0u8; big_stride * 84];
    Raster::new(&mut pixels, &font, short, big_stride)
        .unwrap()
        .paint(&c.scene(), short.bounds())
        .unwrap();
    let scene_only = pixels.clone();
    Raster::new(&mut pixels, &font, short, big_stride)
        .unwrap()
        .paint(&c.badges(), area)
        .unwrap();
    assert_eq!(pixels, scene_only);
    Raster::new(&mut pixels, &font, short, big_stride)
        .unwrap()
        .paint(&c.badges(), short.bounds())
        .unwrap();
    assert_ne!(pixels, scene_only, "unclipped, the badge takes the band");
}

#[test]
fn the_binary_waits_previews_and_refuses_a_bad_socket() {
    use std::os::unix::fs::PermissionsExt;
    let temp = Temp::new("window");
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o700)).unwrap();
    let roll = temp.0.join("roll");
    fs::create_dir_all(&roll).unwrap();
    for name in ["DSC_0001.NEF", "DSC_0002.NEF"] {
        fs::write(roll.join(name), b"not really a nef").unwrap();
    }
    fs::write(
        roll.join("DSC_0002.NEF.edit"),
        "td-photo edit 1\nflag pick\n",
    )
    .unwrap();
    let roll_s = roll.to_str().unwrap();
    // The replay has nothing outstanding: `wait-idle` is idle at once, its
    // argument judged all the same.
    let requests = [
        request(1, &["wait-idle", "0"]),
        request(2, &["wait-idle", "4000"]),
        request(3, &["wait-idle", "4001"]),
        request(4, &["wait-idle"]),
        request(5, &["wait-idle", "1", "2"]),
        request(6, &["wait-idle", "soon"]),
        request(7, &["state"]),
    ];
    let (ok, replies, err) = replay(&["--size", "400x300", roll_s], &requests);
    assert!(ok, "{err}");
    assert_eq!(replies.len(), requests.len());
    let reply = |id: usize| -> &[String] { &replies[id - 1][1..] };
    assert_eq!(reply(1), ["ok", "idle"]);
    assert_eq!(reply(2), ["ok", "idle"]);
    assert_eq!(&reply(3)[..2], ["error", "bad-argument"]);
    assert_eq!(&reply(4)[..2], ["error", "protocol"]);
    assert_eq!(&reply(5)[..2], ["error", "protocol"]);
    assert_eq!(reply(6)[0], "error");
    assert_eq!(reply(7)[14], "0");

    // `--preview` without a roll is the seam's frame of the empty window;
    // with one, the scene over it, a thumbnail that cannot be made noted
    // and its box left the placeholder.
    let bin = env!("CARGO_BIN_EXE_td-photo");
    let output = Command::new(bin)
        .args(["--preview", "400x300"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        driven::paint(&Controller::new(surface(400, 300)).scene())
            .unwrap()
            .ppm()
    );
    let output = Command::new(bin)
        .args(["--preview", "400x300", roll_s])
        .env("XDG_CACHE_HOME", temp.0.join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut c = Controller::new(surface(400, 300));
    c.open(
        "roll",
        roll_s.as_bytes(),
        vec![
            Photo {
                name: "DSC_0001.NEF".to_string(),
                ..Photo::default()
            },
            Photo {
                name: "DSC_0002.NEF".to_string(),
                sidecar: Some(edits("td-photo edit 1\nflag pick\n")),
                error: None,
            },
        ],
    )
    .unwrap();
    assert_eq!(output.stdout, driven::paint(&c.scene()).unwrap().ppm());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("DSC_0001.NEF") && err.contains("DSC_0002.NEF"),
        "{err}"
    );
    let missing = temp.0.join("missing");
    let missing = missing.to_str().unwrap();
    for args in [
        &["--preview"][..],
        &["--preview", "0x0"],
        &["--preview", "wide"],
        &["--preview", "400x300", roll_s, roll_s],
        &["--preview", "400x300", missing],
    ] {
        let output = Command::new(bin).args(args).output().unwrap();
        assert!(
            !output.status.success() && output.stdout.is_empty(),
            "{args:?}"
        );
    }

    // `open` refuses a socket path that is relative, missing or given
    // twice, a second roll and a stray flag before it looks for a display;
    // a good path is bound, then taken away when the display is not there.
    let socket = temp.0.join("control");
    let socket_s = socket.to_str().unwrap();
    for args in [
        &["open", "--control-socket", "relative"][..],
        &["open", "--control-socket"],
        &[
            "open",
            roll_s,
            "--control-socket",
            socket_s,
            "--control-socket",
            socket_s,
        ],
        &["open", roll_s, roll_s],
        &["open", "--bogus"],
        &["open", missing],
    ] {
        let output = Command::new(bin).args(args).env_clear().output().unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(!socket.exists(), "{args:?}");
    }
    let output = Command::new(bin)
        .args(["open", roll_s, "--control-socket", socket_s])
        .env_clear()
        .env("XDG_RUNTIME_DIR", &temp.0)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.is_empty());
    assert!(!socket.exists(), "the socket was left behind");
}

#[test]
fn the_develop_box_is_the_preview_box_only_in_develop_mode() {
    let mut c = Controller::new(surface(800, 600));
    // No box before a roll, nor in the cull grid or single view: the develop
    // box is the window's and preview's blit target, only while developing.
    assert_eq!(c.develop_box(), None);
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(c.develop_box(), None);
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(c.view(), View::Single);
    assert_eq!(c.develop_box(), None);
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);

    // In develop mode it is the layout's preview box, the same rectangle the
    // scene fills with a placeholder.
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.develop_box(), c.layout().develop_box());
    assert!(c.develop_box().is_some());
    // Right of the history pane: the region's box, not the whole area's.
    let region = c.layout().develop_region();
    assert_eq!(
        (region.x, region.width),
        (ui::PANE_W as i64, 800 - ui::PANE_W as u32)
    );
    assert_ne!(c.develop_box(), c.layout().preview_box());
    // Under the two bands and above the filmstrip: the box is the height
    // left, 3:2, centred in the region's width.
    assert_eq!(
        c.develop_box(),
        Some(Rect {
            x: 268,
            y: 120,
            width: 480,
            height: 320,
        })
    );

    // A surface too small for a box has none, even in develop mode.
    let mut small = Controller::new(surface(400, 40));
    small.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut small, "develop", &[]), Outcome::Changed);
    assert_eq!(small.layout().preview_box(), None);
    assert_eq!(small.develop_box(), None);
}

#[test]
fn a_crop_drag_over_the_preview_selects_a_sub_region() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.crop_drag(), None);

    // The adapter reports the developed image's fitted rectangle as a fact,
    // the drag's canvas; like the job count it never bumps the generation,
    // only the marquee the model derives from it does.
    let canvas = Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    };
    let quiet = fields(&c)[GENERATION].clone();
    c.set_preview_fit(Some(canvas));
    assert_eq!(fields(&c)[GENERATION], quiet);
    assert_eq!(c.crop_drag(), None);

    // A press inside the canvas arms a zero-size marquee: invisible, so no
    // frame change and no generation bump yet.
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 400,
            y: 150,
            width: 0,
            height: 0,
        })
    );
    assert_eq!(fields(&c)[GENERATION], quiet);

    // A move rubber-bands the marquee: the frame changes, so a bump.
    assert_eq!(drag_to(&mut c, 600, 300), Outcome::Changed);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 400,
            y: 150,
            width: 200,
            height: 150,
        })
    );
    assert_ne!(fields(&c)[GENERATION], quiet);

    // The release commits the selected fractions of the canvas as the crop,
    // through the same Edit the `crop` action emits, and the marquee is gone.
    // dx/dy = 100/50 of 400x300, dw/dh = 200/150: x=0.2500, y=0.1666 (floor),
    // w=h=0.5000 of the whole (uncropped) image.
    let (outcome, effects) = release(&mut c, 600, 300);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.1666 0.5000 0.5000".to_string()),
        }]
    );
    assert_eq!(c.crop_drag(), None);

    // Settling the effect records the crop; the state reads it back.
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.2500 0.1666 0.5000 0.5000");
}

#[test]
fn a_crop_drag_composes_with_the_current_crop() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);

    // Start from a crop already on the photo: a centred half.
    assert_eq!(
        carry(&mut c, "crop", &["0.2500", "0.1666", "0.5000", "0.5000"]),
        Outcome::Changed
    );
    assert_eq!(fields(&c)[CROP], "0.2500 0.1666 0.5000 0.5000");

    let canvas = Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(canvas));

    // A marquee over the canvas selects a sub-region of the *current* crop,
    // not the whole image. From the canvas top-left, 200x150 of 400x300 is
    // the top-left quarter-area; composed with the current crop (w=h=0.5000)
    // that leaves x=0.2500, y=0.1666 and shrinks w=h to 0.2500 -- a subset.
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 250), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 500, 250);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.1666 0.2500 0.2500".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.2500 0.1666 0.2500 0.2500");
}

#[test]
fn a_crop_drag_refuses_clicks_tiny_marquees_and_off_canvas_presses() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let canvas = Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(canvas));

    // A press off the canvas arms no drag: the far edges are exclusive.
    assert_eq!(press(&mut c, 250, 150), Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);
    assert_eq!(press(&mut c, 700, 200), Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // A move or release with no drag armed is inert.
    assert_eq!(drag_to(&mut c, 500, 250), Outcome::Ignored);
    assert_eq!(release(&mut c, 500, 250).0, Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // A plain click -- press and release with no move -- is a zero-size
    // marquee: nothing selected, no effect, no change.
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    let (outcome, effects) = release(&mut c, 400, 150);
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(c.crop_drag(), None);

    // A marquee that maps under the minimum edge selects nothing, but a
    // visible marquee that then vanishes is still a frame change: Changed
    // with no effect. 12px of 400 is 0.0300, under the 0.0500 minimum edge.
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 412, 165), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 412, 165);
    assert_eq!(outcome, Outcome::Changed);
    assert!(effects.is_empty());
    assert_eq!(c.crop_drag(), None);
    assert_eq!(fields(&c)[CROP], "-");
}

#[test]
fn the_crop_drag_uses_the_develop_box_when_no_fit_is_reported() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();

    // In the cull grid the pointer culls, never crops: a move and release
    // are inert and no marquee is ever armed.
    assert_eq!(c.crop_drag(), None);
    assert_eq!(drag_to(&mut c, 300, 250), Outcome::Ignored);
    assert_eq!(release(&mut c, 300, 250).0, Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // In develop mode with no fitted rectangle reported, the develop box is
    // the fallback canvas: a press at its centre arms a marquee.
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let box_rect = c.develop_box().unwrap();
    let cx = u32::try_from(box_rect.x + i64::from(box_rect.width) / 2).unwrap();
    let cy = u32::try_from(box_rect.y + i64::from(box_rect.height) / 2).unwrap();
    assert_eq!(press(&mut c, cx, cy), Outcome::Ignored);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: i64::from(cx),
            y: i64::from(cy),
            width: 0,
            height: 0,
        })
    );

    // Leaving develop for the grid drops the drag.
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(c.crop_drag(), None);
}

#[test]
fn the_crop_marquee_is_drawn_into_the_scene_frame() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));

    // The scene paints the develop placeholder; the marquee is witnessed by
    // the frame, not `state`, so it lands in the seam's own paint (the replay
    // `frame` and the `--preview` PPM), not just the model.
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // Arming a zero-size marquee draws nothing: the frame is unchanged.
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

    // Rubber-banding it outlines a rectangle over the preview: the frame
    // differs.
    assert_eq!(drag_to(&mut c, 600, 300), Outcome::Changed);
    let marked = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    assert_ne!(marked, bare);

    // The release takes the drag (its crop is an effect the adapter settles,
    // not the scene): the marquee is gone and the frame is the bare one again.
    let (_, effects) = release(&mut c, 600, 300);
    assert!(!effects.is_empty());
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
}

#[test]
fn a_horizontal_or_vertical_crop_drag_paints_nothing() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // A press then a purely horizontal move: the marquee has zero height, so
    // both painters suppress it. It is invisible, so no frame change -- the
    // move is Ignored, the generation holds, and the frame is the bare one,
    // even though the raw marquee (which crop_drag reports) did change.
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    let quiet = fields(&c)[GENERATION].clone();
    assert_eq!(drag_to(&mut c, 550, 150), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], quiet);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 400,
            y: 150,
            width: 150,
            height: 0,
        })
    );

    // Growing the height makes it visible: now a frame change.
    assert_eq!(drag_to(&mut c, 550, 250), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
}

#[test]
fn a_second_press_during_a_drag_clears_the_visible_marquee() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));

    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 600, 300), Outcome::Changed);
    let visible = fields(&c)[GENERATION].clone();
    let marked = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // A second press (the replay/socket vocabulary permits one during a drag)
    // arms a fresh zero-size marquee: the visible outline left the frame, so
    // that is a change even though the new marquee is itself invisible.
    assert_eq!(press(&mut c, 450, 200), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], visible);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        marked
    );
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 450,
            y: 200,
            width: 0,
            height: 0,
        })
    );
}

#[test]
fn a_release_committing_the_current_crop_still_clears_the_marquee() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));

    // A drag over the whole canvas composes to exactly the full crop; commit
    // it so the sidecar holds it.
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 700, 400), Outcome::Changed);
    let (_, effects) = release(&mut c, 700, 400);
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    let crop = fields(&c)[CROP].clone();
    assert_ne!(crop, "-");

    // Drag the whole canvas again: the composed crop equals the one already in
    // the sidecar, so settling it changes nothing. The marquee still left the
    // frame, so the release must move the generation on its own -- otherwise
    // the outline would stay painted until an unrelated change.
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 700, 400), Outcome::Changed);
    let visible = fields(&c)[GENERATION].clone();
    let (outcome, effects) = release(&mut c, 700, 400);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some(crop.clone()),
        }]
    );
    assert_ne!(fields(&c)[GENERATION], visible);
    assert_eq!(c.crop_drag(), None);
    // Settling the same crop is a no-op, proving the release's own bump is what
    // witnessed the marquee removal.
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Ignored);
}

#[test]
fn a_crop_drag_holds_the_canvas_it_started_on() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));

    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 600, 300), Outcome::Changed);

    // A new fit arrives mid-drag (an in-flight develop lands at a different
    // size): the gesture keeps mapping against the canvas it started on, not
    // the new one, so the committed crop cannot escape the current crop.
    c.set_preview_fit(Some(Rect {
        x: 400,
        y: 100,
        width: 200,
        height: 300,
    }));
    let (_, effects) = release(&mut c, 600, 300);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.1666 0.5000 0.5000".to_string()),
        }]
    );
}

#[test]
fn a_press_then_release_with_no_move_selects_from_the_two_points() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));

    // The replay/socket adapter can name a rubber-band by its two corners: a
    // press at one, a release at the other with no move between. The release
    // uses its own point, so the marquee spans the two and commits a crop.
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    let (outcome, effects) = release(&mut c, 600, 300);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.1666 0.5000 0.5000".to_string()),
        }]
    );
    assert_eq!(c.crop_drag(), None);
}

// ---- crop drag handles: the crop-adjust sub-mode (5(e) third slice) ----

/// Enters develop, sets the given crop, turns on crop-adjust and reports the
/// shared 400x300 canvas. The centred-half crop maps to Rect{200,175,200,150}.
fn adjusting(crop: &[&str]) -> Controller {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    if !crop.is_empty() {
        assert_eq!(carry(&mut c, "crop", crop), Outcome::Changed);
    }
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    // The toggle drops the held fit; the window reports the uncropped
    // frame's at the turn's end.
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));
    c
}

#[test]
fn adjust_crop_toggles_only_in_develop_and_escapes_in_layers() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();

    // In cull the crop-adjust toggle is not this mode's: Ignored, no overlay.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Ignored);
    assert!(!c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);

    // In develop it toggles the sub-mode; the overlay appears once the
    // window reports the uncropped frame's fit (the box alone, showing
    // the cropped frame meanwhile, is no canvas here) and leaves with the
    // sub-mode, and the tighten marquee is not this mode's.
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);
    assert!(c.scene().status_line().ends_with(" | develop crop-adjust"));
    assert_eq!(press(&mut c, 400, 300), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 400), Outcome::Ignored);
    assert_eq!(release(&mut c, 500, 400), (Outcome::Ignored, vec![]));
    let fit = Rect {
        x: 300,
        y: 150,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(fit));
    assert_eq!(c.crop_adjust_rect(), Some(fit));
    assert_eq!(c.crop_drag(), None);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(!c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);
    c.set_preview_fit(None);

    // The `c` key toggles it too.
    assert_eq!(key(&mut c, "c"), Outcome::Changed);
    assert!(c.adjusting());

    // Escape (grid) backs out of crop-adjust first, staying in develop, then
    // leaves develop for the grid.
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert!(!c.adjusting());
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Cull);
}

#[test]
fn crop_adjust_maps_the_crop_onto_the_canvas() {
    // With no crop the overlay is the whole canvas.
    let c = adjusting(&[]);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 300,
            y: 100,
            width: 400,
            height: 300,
        })
    );

    // With a centred-half crop it is the mapped sub-rectangle.
    let c = adjusting(&["0.2500", "0.2500", "0.5000", "0.5000"]);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 400,
            y: 175,
            width: 200,
            height: 150,
        })
    );
}

#[test]
fn a_corner_handle_grows_the_crop() {
    let mut c = adjusting(&["0.2500", "0.2500", "0.5000", "0.5000"]);

    // Grab the NW corner (200,175) and drag it to the canvas top-left: the crop
    // grows from a centred half to the top-left three-quarters -- a growth the
    // tighten marquee can never do. Grabbing paints nothing new (Ignored).
    assert_eq!(press(&mut c, 400, 175), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 300, 100), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 300, 100);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.0000 0.0000 0.7500 0.7500".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.0000 0.0000 0.7500 0.7500");
    assert_eq!(c.crop_drag(), None);
}

#[test]
fn the_interior_handle_moves_the_crop() {
    let mut c = adjusting(&["0.2500", "0.2500", "0.5000", "0.5000"]);

    // Press inside the crop rectangle and drag: the whole rectangle translates,
    // keeping its width and height.
    assert_eq!(press(&mut c, 500, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 520, 260), Outcome::Changed);
    let (_, effects) = release(&mut c, 520, 260);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.3000 0.2833 0.5000 0.5000".to_string()),
        }]
    );
}

#[test]
fn an_edge_handle_shrinks_to_the_minimum_and_grows_to_clear_the_crop() {
    // Dragging the east edge far in clamps to the minimum edge (0.0500), never
    // past it.
    let mut c = adjusting(&["0.2500", "0.2500", "0.5000", "0.5000"]);
    assert_eq!(press(&mut c, 600, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 410, 250), Outcome::Changed);
    let (_, effects) = release(&mut c, 410, 250);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.2500 0.0500 0.5000".to_string()),
        }]
    );

    // Dragging an edge out to the canvas so the rectangle covers the whole
    // image clears the crop (value None), not a redundant full-frame crop.
    let mut c = adjusting(&["0.0000", "0.0000", "0.5000", "1.0000"]);
    assert_eq!(press(&mut c, 500, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 700, 250), Outcome::Changed);
    let (_, effects) = release(&mut c, 700, 250);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: None,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "-");
}

#[test]
fn crop_adjust_is_witnessed_by_the_frame_not_state() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let canvas = Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    };

    // set_preview_fit is a fact, like the job count: no generation bump.
    let quiet = fields(&c)[GENERATION].clone();
    c.set_preview_fit(Some(canvas));
    assert_eq!(fields(&c)[GENERATION], quiet);
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // Entering crop-adjust names the sub-mode: a frame change, digest differs.
    // The toggle drops the fit; the overlay waits for the uncropped frame's.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_eq!(c.crop_adjust_rect(), None);
    c.set_preview_fit(Some(canvas));
    assert_eq!(c.crop_adjust_rect(), Some(canvas));
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

    // Grabbing a handle paints the rectangle already shown: no bump. Moving it
    // does bump.
    let armed = fields(&c)[GENERATION].clone();
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], armed);
    assert_eq!(drag_to(&mut c, 400, 175), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], armed);

    // Leaving crop-adjust removes the overlay: back to the bare frame.
    let _ = release(&mut c, 400, 175);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(!c.adjusting());
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
}

#[test]
fn a_handle_grab_and_click_never_bumps_or_mutates_a_minimum_crop() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    // A minimum-edge crop (width 0.0500) whose pixel rectangle is floored on a
    // canvas whose width is not a multiple of the crop grid: mapping the whole
    // rectangle back through pixels would drift it, so the commit works in the
    // crop's own units and a grab or click that does not move changes nothing.
    assert_eq!(
        carry(&mut c, "crop", &["0.2500", "0.2500", "0.0500", "0.5000"]),
        Outcome::Changed
    );
    c.set_preview_fit(Some(Rect {
        x: 0,
        y: 0,
        width: 250,
        height: 200,
    }));
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);

    // The east edge sits at x = 62 + 12 = 74 (floored from 12.5 px).
    let armed = fields(&c)[GENERATION].clone();
    assert_eq!(press(&mut c, 74, 100), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], armed);
    let (outcome, effects) = release(&mut c, 74, 100);
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(fields(&c)[GENERATION], armed);
    assert_eq!(fields(&c)[CROP], "0.2500 0.2500 0.0500 0.5000");
}

#[test]
fn toggling_crop_adjust_over_a_matching_marquee_is_a_frame_change() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let canvas = Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(canvas));

    // A tighten marquee spanning the whole canvas (no crop yet).
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 700, 400), Outcome::Changed);
    assert_eq!(c.crop_drag(), Some(canvas));
    let marquee_gen = fields(&c)[GENERATION].clone();
    let marquee_digest = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // Entering crop-adjust shows the SAME rectangle but drawn with handle marks:
    // the rectangle does not move, yet the painted frame differs, so the toggle
    // is a change and the generation bumps -- the overlay's kind is part of its
    // identity, not only its rectangle.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    // The toggle drops the held fit; the window reports it again at the
    // turn's end.
    c.set_preview_fit(Some(canvas));
    assert_eq!(c.crop_adjust_rect(), Some(canvas));
    assert_ne!(fields(&c)[GENERATION], marquee_gen);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        marquee_digest
    );
}

#[test]
fn opening_a_roll_clears_the_crop_adjust_sub_mode() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.adjusting());

    // Opening another roll returns to the cull grid with the sub-mode cleared,
    // so a later single view cannot paint a stale overlay over the new roll.
    c.open("roll2", b"/r2", photos(3)).unwrap();
    assert!(!c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);
    assert_eq!(c.mode(), ui::Mode::Cull);
}

// ---- look list overlay (5(e) fifth slice) ----

/// The look stems a palette test lists, sorted as the adapter reports them.
fn some_looks() -> Vec<String> {
    ["contrast-boost", "mono", "velvia-like"]
        .iter()
        .map(|stem| stem.to_string())
        .collect()
}

#[test]
fn the_look_palette_toggles_in_develop_and_escapes_in_layers() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    c.set_looks(some_looks());

    // In cull the palette toggle is not this mode's: Ignored, nothing listed.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
    assert_eq!(c.look_palette(), None);

    // In develop it toggles the sub-mode; the palette lists the looks with no
    // current one marked, and the crop overlays are not this mode's.
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let looks = some_looks();
    assert_eq!(c.look_palette(), Some((looks.as_slice(), None)));
    assert!(!c.adjusting());
    assert_eq!(c.crop_drag(), None);
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert_eq!(c.look_palette(), None);

    // The `l` key toggles it too.
    assert_eq!(key(&mut c, "l"), Outcome::Changed);
    assert!(c.look_palette().is_some());

    // Escape (grid) backs out of the palette first, staying in develop, then
    // leaves develop for the grid.
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(c.look_palette(), None);
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Cull);
}

#[test]
fn the_look_palette_is_a_fact_and_witnessed_by_the_frame() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));

    // An empty look list: opening the palette paints nothing, so it is Ignored
    // and the frame does not move.
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    let quiet = fields(&c)[GENERATION].clone();
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
    assert_eq!(c.look_palette(), None);
    assert_eq!(fields(&c)[GENERATION], quiet);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

    // set_looks is a fact absent from `state`, but the look band lists the
    // looks, so a list that differs is a frame change: the generation
    // moves once, and the same list again leaves it alone.
    c.set_looks(some_looks());
    assert_ne!(fields(&c)[GENERATION], quiet);
    let listed = fields(&c)[GENERATION].clone();
    let with_band = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    assert_ne!(with_band, bare);
    c.set_looks(some_looks());
    assert_eq!(fields(&c)[GENERATION], listed);
    let bare = with_band;

    // Opening the palette over the non-empty list shows it: a frame change.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

    // Closing it returns to the bare frame.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
}

#[test]
fn the_look_palette_marks_the_current_look_and_picks_with_the_pointer() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());
    assert_eq!(carry(&mut c, "look", &["mono"]), Outcome::Changed);

    // The palette marks the cursor photo's current look (mono, index 1).
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let looks = some_looks();
    assert_eq!(c.look_palette(), Some((looks.as_slice(), Some(1))));

    // The rows the palette paints, top-down on its panel (the box's rows
    // across the develop region): the top pad, then one CELL_HEIGHT-high
    // row each from a pad in. `at(i)` is the middle of row `i`.
    let panel = c.look_panel().unwrap();
    let region = c.layout().develop_region();
    let r#box = c.develop_box().unwrap();
    assert_eq!(
        panel,
        Rect {
            x: region.x,
            width: region.width,
            ..r#box
        }
    );
    let pad = ui::CELL_PAD as i64;
    let row = CELL_HEIGHT as i64;
    let rows = c.look_rows().unwrap();
    for (index, line) in rows.iter().enumerate() {
        assert_eq!(
            *line,
            Some(Rect {
                x: panel.x + pad,
                y: panel.y + pad + row * index as i64,
                width: ("contrast-boost".len() * td_ui::CELL_WIDTH) as u32,
                height: row as u32,
            })
        );
    }
    let at = |index: i64| Input::Pointer {
        phase: PointerPhase::Press,
        x: (panel.x + pad + 2) as u32,
        y: (panel.y + pad + row * index + row / 2) as u32,
    };

    // A press on the current look (mono, row 1) is a no-op: Ignored, no effect,
    // no crop drag, the mark unmoved.
    let (outcome, effects) = c.input(at(1)).unwrap();
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(c.crop_drag(), None);
    assert_eq!(c.look_palette(), Some((looks.as_slice(), Some(1))));

    // A press on another look (velvia-like, row 2) picks it: one look edit, and
    // once it settles the mark moves there. The palette stays open.
    let (outcome, effects) = c.input(at(2)).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(effects.len(), 1);
    assert!(matches!(&effects[0], Effect::Edit { key: Key::Look, .. }));
    assert_eq!(c.crop_drag(), None);
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(c.look_palette(), Some((looks.as_slice(), Some(2))));

    // A press off the names -- the left padding -- picks nothing.
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: panel.x as u32,
            y: (panel.y + pad + row / 2) as u32,
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());

    // Move and release are inert; the palette stays open throughout.
    assert_eq!(drag_to(&mut c, 260, 240), Outcome::Ignored);
    let (outcome, effects) = release(&mut c, 260, 240);
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert!(c.look_palette().is_some());
}

/// A list taller than the panel goes on in a second column beside the
/// first, each column as wide as its longest name: the built-in set on a
/// 640 by 480 surface, where one column holds fewer than the fourteen,
/// lays the rest beside them and a press there picks the last.
#[test]
fn the_look_palette_lays_a_tall_list_in_columns() {
    let mut c = Controller::new(surface(640, 480));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let mut stems: Vec<String> = look::BUILTIN
        .iter()
        .map(|(stem, _)| stem.to_string())
        .collect();
    stems.sort();
    c.set_looks(stems.clone());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let panel = c.look_panel().unwrap();
    let rows = c.look_rows().unwrap();
    assert_eq!(rows.len(), 14);
    assert!(rows.iter().all(Option::is_some), "{panel:?} {rows:?}");
    let first = rows[0].unwrap();
    let last = rows[13].unwrap();
    // The tool band takes two rows here (the slider on its own) and the
    // look band six, so the box under the name row is 276 by 184: eleven
    // rows a column.
    let per_column =
        ((i64::from(panel.height) - ui::CELL_PAD as i64) / CELL_HEIGHT as i64) as usize;
    assert_eq!(c.layout().tool_band().height, 48);
    assert_eq!(c.layout().look_band().height, 6 * 24);
    assert_eq!(per_column, 11);
    let longest = stems[..per_column].iter().map(|s| s.len()).max().unwrap() as i64;
    assert_eq!(
        first,
        Rect {
            x: panel.x + ui::CELL_PAD as i64,
            y: panel.y + ui::CELL_PAD as i64,
            width: (longest * td_ui::CELL_WIDTH as i64) as u32,
            height: CELL_HEIGHT as u32,
        }
    );
    assert_eq!(
        last.y,
        first.y + CELL_HEIGHT as i64 * (13 % per_column) as i64
    );
    assert_eq!(
        last.x,
        first.x + i64::from(first.width) + ui::CELL_PAD as i64
    );
    let second: &[String] = &stems[per_column..];
    let widest = second.iter().map(|s| s.len()).max().unwrap();
    assert_eq!(last.width, (widest * td_ui::CELL_WIDTH) as u32);
    assert!(
        last.x + i64::from(last.width) <= panel.x + i64::from(panel.width) - ui::CELL_PAD as i64
    );
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: (last.x + 2) as u32,
            y: (last.y + 2) as u32,
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert!(matches!(
        &effects[..],
        [Effect::Edit { key: Key::Look, value: Some(stem), .. }] if *stem == stems[13]
    ));
    // A surface without a develop box lays none.
    c.input(Input::Resize {
        width: 640,
        height: 100,
        scale: 1,
    })
    .unwrap();
    assert_eq!(c.develop_box(), None);
    assert_eq!(c.look_rows(), None);
}

#[test]
fn the_look_palette_pick_needs_a_develop_box() {
    // On a surface too small for a develop box the palette opens as a sub-mode
    // (the status row names it) but paints no list, so a press picks
    // nothing: no row exists to hit.
    let mut c = Controller::new(surface(400, 40));
    c.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert!(c.look_palette().is_some());
    assert_eq!(c.develop_box(), None);

    // Left of the mode strip's first button, off every band's target.
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: 4,
            y: 10,
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(c.crop_drag(), None);
}

#[test]
fn picking_a_look_named_dash_sets_it_not_clears() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    // `-` is the `look` action's clear sentinel but a valid look stem: a user
    // `-.look` lists in the palette, and a press there must set that look, not
    // clear the current one.
    c.set_looks(vec!["-".to_string(), "mono".to_string()]);
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);

    let first = c.look_rows().unwrap()[0].unwrap();
    // Row 0 is the `-` look; the edit sets it (value `Some("-")`), not `None`.
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: (first.x + 2) as u32,
            y: (first.y + i64::from(first.height) / 2) as u32,
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Look,
            value: Some("-".to_string()),
        }]
    );
}

#[test]
fn the_look_palette_pick_rejects_the_padding_and_clipped_rows() {
    // A panel 424 wide (640 by 480): a column of 60-character names (480
    // pixels) starts within it and is cut at its right, and the next column
    // starts past it and is not laid.
    let mut c = Controller::new(surface(640, 480));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let r#box = c.develop_box().unwrap();
    let pad = ui::CELL_PAD as i64;
    let row = CELL_HEIGHT as i64;
    let fit = (i64::from(r#box.height) - pad) / row;
    assert!(fit >= 2);
    let looks: Vec<String> = (0..fit + 2).map(|i| format!("look-{i:0>55}")).collect();
    c.set_looks(looks);
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let panel = c.look_panel().unwrap();
    let rows = c.look_rows().unwrap();
    assert_eq!(rows.len() as i64, fit + 2);
    assert!(rows[..fit as usize].iter().all(Option::is_some));
    assert!(rows[fit as usize..].iter().all(Option::is_none));
    // The cut column's rows run to the pad short of the panel's right, not
    // to the names' full width.
    let width = i64::from(rows[0].unwrap().width);
    assert_eq!(
        panel.x + pad + width,
        panel.x + i64::from(panel.width) - pad
    );
    assert!(width < 60 * td_ui::CELL_WIDTH as i64);

    let press = |c: &mut Controller, x: i64, y: i64| {
        c.input(Input::Pointer {
            phase: PointerPhase::Press,
            x: x as u32,
            y: y as u32,
        })
        .unwrap()
    };
    let inert = |(outcome, effects): (Outcome, Vec<Effect>)| {
        assert_eq!(outcome, Outcome::Ignored);
        assert!(effects.is_empty());
    };
    // The top padding, the left padding, the pad past the names, where a
    // row past the foot would go, and where the column past the right
    // would start are none of them a name: a press picks nothing.
    inert(press(&mut c, panel.x + pad + 2, panel.y + pad - 1));
    inert(press(&mut c, panel.x, panel.y + pad + row / 2));
    inert(press(
        &mut c,
        panel.x + pad + width,
        panel.y + pad + row / 2,
    ));
    inert(press(
        &mut c,
        panel.x + pad + 2,
        panel.y + pad + row * fit + row / 2,
    ));
    inert(press(
        &mut c,
        panel.x + pad + 60 * td_ui::CELL_WIDTH as i64 + pad + 2,
        panel.y + pad + row / 2,
    ));
    // But the last row of the cut column (index fit - 1) picks, at its
    // cut end too.
    let (outcome, effects) = press(
        &mut c,
        panel.x + pad + width - 1,
        panel.y + pad + row * (fit - 1) + row / 2,
    );
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(effects.len(), 1);
}

#[test]
fn the_look_palette_and_crop_adjust_are_mutually_exclusive() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));
    c.set_looks(some_looks());

    // Opening the palette while adjusting closes crop-adjust.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.adjusting());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert!(c.look_palette().is_some());
    assert!(!c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);

    // Entering crop-adjust while the palette is open closes the palette.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.adjusting());
    assert_eq!(c.look_palette(), None);
    c.set_preview_fit(Some(Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));
    assert!(c.crop_adjust_rect().is_some());
}

#[test]
fn the_look_palette_drops_on_a_photo_switch() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert!(c.look_palette().is_some());

    // Moving to the next photo ends the palette, as it ends a drag or
    // crop-adjust; the look list (a fact) survives the switch.
    assert_eq!(act(&mut c, "next", &[]), Outcome::Changed);
    assert_eq!(c.look_palette(), None);
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let looks = some_looks();
    assert_eq!(c.look_palette(), Some((looks.as_slice(), None)));
}

#[test]
fn the_look_palette_is_witnessed_by_the_status_row_without_a_develop_box() {
    // A surface too small for a develop box: nothing paints the palette,
    // but the status row names the sub-mode, so its toggle is a frame
    // change all the same, and the generation moves with the frame (a
    // row wide enough to show the word; a narrower one clips it as it
    // clips any of its text).
    let mut c = Controller::new(surface(800, 40));
    c.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());
    assert_eq!(c.develop_box(), None);

    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    let quiet = fields(&c)[GENERATION].clone();
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert!(c.look_palette().is_some());
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
    assert!(c.scene().status_line().ends_with(" | develop looks"));
    // Closing it again returns the frame.
    let open = fields(&c)[GENERATION].clone();
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], open);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
    // Escape closes it the same way.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let open = fields(&c)[GENERATION].clone();
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert!(c.look_palette().is_none());
    assert_ne!(fields(&c)[GENERATION], open);
    assert!(c.scene().status_line().ends_with(" | develop"));
    // Crop-adjust is named the same way, with no box to draw handles in.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.scene().status_line().ends_with(" | develop crop-adjust"));
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
    // An empty look list has nothing to open: Ignored, as before.
    c.set_looks(Vec::new());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
}

#[test]
fn a_touch_behind_the_open_palette_does_not_bump() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());

    // Outside the palette a touch -- a develop image landing, a thumbnail
    // arriving -- is a new generation.
    let before = fields(&c)[GENERATION].clone();
    c.touch();
    assert_ne!(fields(&c)[GENERATION], before);

    // With the palette open its opaque panel covers the develop box, so a
    // touch behind it changes nothing on screen: no bump.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    let open = fields(&c)[GENERATION].clone();
    c.touch();
    assert_eq!(fields(&c)[GENERATION], open);

    // Closing the palette bumps, so the image held meanwhile is shown then.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], open);
}

// ---- crop aspect presets (5(e) fourth slice) ----

/// Opens a roll, enters develop, sets an optional crop, and reports `canvas`
/// as the develop preview's fitted rectangle. Crop-adjust is left off.
fn develop_at(canvas: Rect, crop: &[&str]) -> Controller {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    if !crop.is_empty() {
        assert_eq!(carry(&mut c, "crop", crop), Outcome::Changed);
    }
    c.set_preview_fit(Some(canvas));
    c
}

/// `develop_at`, then crop-adjust on with `canvas` reported again after the
/// toggle, as the window does at the turn's end (the toggle drops the fit).
fn adjusting_at(canvas: Rect, crop: &[&str]) -> Controller {
    let mut c = develop_at(canvas, crop);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    c.set_preview_fit(Some(canvas));
    c
}

#[test]
fn a_locked_corner_drag_maps_the_ratio_in_pixel_space() {
    // A 4:3-pixel canvas: a 3:2-pixel lock is a 9:8 box in fractions, so this
    // proves the ratio is applied in the canvas's pixel space, not in fractions.
    // The crop starts 4:3 in pixels (200x150), not the locked 3:2.
    let mut c = adjusting_at(
        Rect {
            x: 220,
            y: 96,
            width: 400,
            height: 300,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    // Picking a ratio only arms the lock; it never reshapes the crop on its own
    // (an immediate snap could read a stale cropped fit as the whole image's
    // aspect and commit a wrong-ratio crop). Reshaping flows through the drag.
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Grabbing the south-east corner paints the crop already shown, not a
    // reshaped ratio box: a zero-delta grab keeps the free path.
    assert_eq!(press(&mut c, 520, 321), Outcome::Ignored);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 320,
            y: 171,
            width: 200,
            height: 150,
        })
    );
    // Dragging the corner out holds 3:2 in pixels: 300x200 on screen, a 9:8
    // box in the crop's fractions; the live overlay is already the committed
    // box.
    assert_eq!(drag_to(&mut c, 620, 396), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 320,
            y: 171,
            width: 300,
            height: 200,
        })
    );
    let (outcome, effects) = release(&mut c, 620, 396);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.2500 0.7500 0.6667".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.2500 0.2500 0.7500 0.6667");
}

#[test]
fn a_locked_grab_without_moving_neither_reshapes_nor_commits() {
    // A lock armed while the crop does not match it (here armed in plain
    // develop, before entering crop-adjust, over the full frame). Grabbing a
    // handle must paint the crop already shown -- not reshape to the ratio on
    // the press and then discard it on a stationary release.
    let mut c = develop_at(
        Rect {
            x: 220,
            y: 96,
            width: 600,
            height: 400,
        },
        &[],
    );
    assert_eq!(act(&mut c, "aspect", &["1:1"]), Outcome::Ignored);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    let full = Rect {
        x: 220,
        y: 96,
        width: 600,
        height: 400,
    };
    c.set_preview_fit(Some(full));
    assert_eq!(c.crop_adjust_rect(), Some(full));
    let crop_before = fields(&c)[CROP].clone();
    // The grab does not reshape the full-frame overlay to a square.
    assert_eq!(press(&mut c, 220, 96), Outcome::Ignored);
    assert_eq!(c.crop_adjust_rect(), Some(full));
    // Releasing without moving commits nothing and leaves the crop untouched.
    let (outcome, effects) = release(&mut c, 220, 96);
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(fields(&c)[CROP], crop_before);
}

#[test]
fn a_corner_drag_holds_the_locked_ratio() {
    let mut c = adjusting_at(
        Rect {
            x: 220,
            y: 96,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.0000", "0.5000", "0.5000"],
    );
    // Arming the lock paints nothing; the crop starts 3:2 in pixels (300x200)
    // and a locked drag holds that ratio.
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Drag the south-east corner out to the canvas: the box grows holding 3:2,
    // capped by the canvas edge; the live overlay is already the committed box.
    assert_eq!(press(&mut c, 670, 296), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 820, 496), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 370,
            y: 96,
            width: 450,
            height: 300,
        })
    );
    let (outcome, effects) = release(&mut c, 820, 496);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.0000 0.7500 0.7500".to_string()),
        }]
    );
}

#[test]
fn an_edge_drag_under_a_lock_adjusts_the_orthogonal_dimension() {
    let mut c = adjusting_at(
        Rect {
            x: 220,
            y: 96,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Drag the east edge inward: the width shrinks and the height follows to
    // hold 3:2, centred on the crop's old horizontal midline.
    assert_eq!(press(&mut c, 670, 296), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 610, 296), Outcome::Changed);
    let (_, effects) = release(&mut c, 610, 296);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.3000 0.4000 0.4000".to_string()),
        }]
    );
}

#[test]
fn a_one_to_one_lock_keeps_a_square_in_pixels() {
    let mut c = adjusting_at(
        Rect {
            x: 220,
            y: 96,
            width: 400,
            height: 400,
        },
        &[],
    );
    // Arming the lock paints nothing (the full image is already 1:1 here).
    assert_eq!(act(&mut c, "aspect", &["1:1"]), Outcome::Ignored);
    // Drag the north-west corner in: the box stays square in pixels.
    assert_eq!(press(&mut c, 220, 96), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 320, 196), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 320,
            y: 196,
            width: 300,
            height: 300,
        })
    );
    let (_, effects) = release(&mut c, 320, 196);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.2500 0.7500 0.7500".to_string()),
        }]
    );
}

#[test]
fn switching_the_lock_back_to_free_releases_the_constraint() {
    let mut c = adjusting_at(
        Rect {
            x: 220,
            y: 96,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Back to free: an east-edge drag now changes the width alone, leaving the
    // height at 0.5000 (the pre-lock free behaviour).
    assert_eq!(act(&mut c, "aspect", &["free"]), Outcome::Ignored);
    assert_eq!(press(&mut c, 670, 296), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 610, 296), Outcome::Changed);
    let (_, effects) = release(&mut c, 610, 296);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.2500 0.4000 0.5000".to_string()),
        }]
    );
}

#[test]
fn a_locked_edge_drag_clamps_to_the_minimum_holding_the_ratio() {
    let mut c = adjusting_at(
        Rect {
            x: 220,
            y: 96,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Drag the east edge far in: both edges pin to the minimum (0.0500) while
    // holding 3:2, never past it.
    assert_eq!(press(&mut c, 670, 296), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 320, 296), Outcome::Changed);
    let (_, effects) = release(&mut c, 320, 296);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.4750 0.0500 0.0500".to_string()),
        }]
    );
}

#[test]
fn a_locked_tighten_marquee_snaps_the_selection() {
    let mut c = develop_at(
        Rect {
            x: 220,
            y: 96,
            width: 600,
            height: 400,
        },
        &[],
    );
    // Not in crop-adjust: the lock arms the tighten marquee too, and survives
    // the sub-mode boundary.
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    assert_eq!(press(&mut c, 220, 96), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 820, 396), Outcome::Changed);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 220,
            y: 96,
            width: 450,
            height: 300,
        })
    );
    let (_, effects) = release(&mut c, 820, 396);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.0000 0.0000 0.7500 0.7500".to_string()),
        }]
    );
}

#[test]
fn the_lock_is_dropped_on_a_photo_switch() {
    let mut c = adjusting_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &[],
    );
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    assert!(c.adjusting());
    // Moving to another photo ends the sub-mode and the lock.
    assert_eq!(act(&mut c, "next", &[]), Outcome::Changed);
    assert!(!c.adjusting());
    // A fresh tighten marquee is unconstrained: a square drag stays square, so
    // the 3:2 lock did not survive the switch.
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 600, 400), Outcome::Changed);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 300,
            y: 100,
            width: 300,
            height: 300,
        })
    );
}

#[test]
fn a_bad_aspect_token_or_wrong_mode_is_refused() {
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &[],
    );
    // In develop, an unknown ratio token is a bad argument, judged before any
    // write.
    assert!(matches!(
        c.action("aspect", &["2:0"]),
        Err(ui::Error::BadArgument)
    ));
    assert!(matches!(
        c.action("aspect", &["square"]),
        Err(ui::Error::BadArgument)
    ));
    // In cull the aspect action is not this mode's: Ignored, no effect (and no
    // argument judgement).
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Cull);
    let (outcome, effects) = c.action("aspect", &["3:2"]).unwrap();
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
}

// ------------------------------------------------------------------ export

#[test]
fn delete_rejected_takes_the_rejects_out_of_the_model_and_keeps_the_cursor_near() {
    let mut c = Controller::new(surface(800, 600));
    assert_eq!(
        c.action("delete-rejected", &[]).unwrap_err(),
        ui::Error::NoRoll
    );
    // Six photos: the third a reject, the fourth refused. The dispatch asks
    // whenever a roll is open, by verb and by `Delete`, and moves nothing
    // itself.
    c.open("roll", b"/r", photos(6)).unwrap();
    let before = fields(&c)[GENERATION].clone();
    let (outcome, effects) = c.action("delete-rejected", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(effects, [Effect::DeleteRejected]);
    let (_, by_key) = c.input(Input::Key { chord: "Delete" }).unwrap();
    assert_eq!(by_key, effects);
    assert!(!Action::DeleteRejected.repeats());
    assert_eq!(fields(&c)[GENERATION], before);
    assert_eq!(c.photos().len(), 6);
    // Applied with the cursor on the reject: the reject leaves, the cursor
    // takes its position among the shown, and the generation moves.
    assert_eq!(act(&mut c, "select", &["2"]), Outcome::Changed);
    let before = fields(&c)[GENERATION].clone();
    assert_eq!(apply(&mut c, &effects), Ok(Outcome::Changed));
    assert_ne!(fields(&c)[GENERATION], before);
    let f = fields(&c);
    assert_eq!(
        (&f[2], &f[3], &f[4], &f[7]),
        (
            &"5".to_string(),
            &"5".to_string(),
            &"2".to_string(),
            &name(3)
        )
    );
    let names: Vec<&str> = c.photos().iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, [file(0), file(1), file(3), file(4), file(5)]);
    // Nothing left to move: `remove` of names not held changes nothing, and
    // the adapter says `ignored`.
    let before = fields(&c)[GENERATION].clone();
    assert!(!c.remove(&[file(2)]));
    assert!(!c.remove(&[]));
    assert_eq!(apply(&mut c, &effects), Ok(Outcome::Ignored));
    assert_eq!(fields(&c)[GENERATION], before);
    // The cursor on a reject at the end clamps to the new end, keeping its
    // photo when that stays.
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(3)).unwrap();
    assert_eq!(act(&mut c, "select", &["2"]), Outcome::Changed);
    assert_eq!(apply(&mut c, &effects), Ok(Outcome::Changed));
    assert_eq!(fields(&c)[7], name(1));
    assert_eq!(act(&mut c, "select", &["0"]), Outcome::Changed);
    let mut roll = c.photos().to_vec();
    roll[1].sidecar = Some(edits("td-photo edit 1\nflag reject\n"));
    c.open("roll", b"/r", roll).unwrap();
    assert_eq!(apply(&mut c, &effects), Ok(Outcome::Changed));
    assert_eq!(fields(&c)[7], name(0));
    // Under the rejects filter, in the single view: nothing is left shown,
    // the cursor leaves and the view ends with it.
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(3)).unwrap();
    assert_eq!(act(&mut c, "rejects", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(apply(&mut c, &effects), Ok(Outcome::Changed));
    let f = fields(&c);
    assert_eq!(
        (&f[2], &f[3], &f[4], &f[6]),
        (
            &"2".to_string(),
            &"0".to_string(),
            &"-".to_string(),
            &"grid".to_string()
        )
    );
    // In the single view of a reject with others left: the view stays, on
    // the photo that takes the reject's place.
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(4)).unwrap();
    assert_eq!(act(&mut c, "select", &["2"]), Outcome::Changed);
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(apply(&mut c, &effects), Ok(Outcome::Changed));
    let f = fields(&c);
    assert_eq!(
        (&f[4], &f[6], &f[7]),
        (&"2".to_string(), &"single".to_string(), &name(3))
    );
    // Develop mode ignores it, as it does the filters.
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(3)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(
        c.action("delete-rejected", &[]).unwrap(),
        (Outcome::Ignored, Vec::new())
    );
    assert_eq!(key(&mut c, "Delete"), Outcome::Ignored);
    assert_eq!(c.photos().len(), 3);
}

#[test]
fn export_asks_for_the_cursor_photo_in_either_mode_and_notes_the_status_row() {
    let mut c = Controller::new(surface(800, 600));
    assert_eq!(c.action("export", &[]).unwrap_err(), ui::Error::NoRoll);
    c.open("roll", b"/r", Vec::new()).unwrap();
    assert_eq!(c.action("export", &[]).unwrap_err(), ui::Error::NoPhoto);
    c.open("roll", b"/r", photos(3)).unwrap();
    // In the cull grid: the cursor's photo, by verb and by `e`; the outcome
    // is the adapter's to settle, so the dispatch itself is `changed` with
    // the effect and moves nothing in the model.
    let before = fields(&c)[GENERATION].clone();
    let (outcome, effects) = c.action("export", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Export {
            index: 0,
            name: file(0)
        }]
    );
    assert_eq!(fields(&c)[GENERATION], before);
    let (_, by_key) = c.input(Input::Key { chord: "e" }).unwrap();
    assert_eq!(by_key, effects);
    assert!(!Action::Export.repeats());
    // The second photo, and in develop mode too: an export is the roll's,
    // not a develop edit.
    assert_eq!(act(&mut c, "next", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let (_, effects) = c.action("export", &[]).unwrap();
    assert_eq!(
        effects,
        [Effect::Export {
            index: 1,
            name: file(1)
        }]
    );
    let (_, by_key) = c.input(Input::Key { chord: "e" }).unwrap();
    assert_eq!(by_key, effects);
    // The note: set by the adapter, shown at the end of the status row, a
    // new generation when it differs and none when it is the same; cleared
    // by opening a roll.
    let quiet = fields(&c)[GENERATION].clone();
    assert!(c.export_note().is_none());
    assert!(!c.scene().status_line().contains("export"));
    c.set_export(Some("exporting DSC_0002.NEF".to_string()));
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_eq!(c.export_note(), Some("exporting DSC_0002.NEF"));
    assert!(c
        .scene()
        .status_line()
        .ends_with("| develop | exporting DSC_0002.NEF"));
    let noted = fields(&c)[GENERATION].clone();
    c.set_export(Some("exporting DSC_0002.NEF".to_string()));
    assert_eq!(fields(&c)[GENERATION], noted);
    c.set_export(Some("exported DSC_0002.jpg".to_string()));
    assert_ne!(fields(&c)[GENERATION], noted);
    assert!(c
        .scene()
        .status_line()
        .ends_with("| develop | exported DSC_0002.jpg"));
    // Not a `state` field: the state is the same with and without the note
    // but for the generation, which the row's repaint moves.
    let without_generation = |c: &Controller| fields(c)[..GENERATION].to_vec();
    let with_note = without_generation(&c);
    c.set_export(None);
    assert_eq!(without_generation(&c), with_note);
    c.set_export(Some("x".to_string()));
    c.open("roll", b"/r", photos(3)).unwrap();
    assert!(c.export_note().is_none());
    // Carried by the test adapter: the settled outcome is `changed`.
    assert_eq!(carry(&mut c, "export", &[]), Outcome::Changed);
    assert_eq!(
        c.export_note(),
        Some(format!("exported {}", file(0)).as_str())
    );
}

#[test]
fn the_binary_deletes_the_rejects_over_the_replay() {
    let temp = Temp::new("delete");
    let roll = temp.0.join("roll");
    fs::create_dir_all(&roll).unwrap();
    for i in 1..=3 {
        fs::write(roll.join(format!("DSC_000{i}.NEF")), format!("photo {i}")).unwrap();
    }
    let first = "td-photo edit 1\nflag reject\nexposure 0.33\n";
    fs::write(roll.join("DSC_0001.NEF.edit"), first).unwrap();
    fs::write(
        roll.join("DSC_0003.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    let roll_s = roll.to_str().unwrap();
    let mut session = Replay::start(&[roll_s]);
    let a = session.send(&[
        request(1, &["action", "delete-rejected"]),
        request(2, &["state"]),
        request(3, &["key", &hex(b"Delete")]),
    ]);
    // The rejects and their sidecars are in `rejected/` when the reply is,
    // the model holds the one photo left, and a second asks moves nothing.
    assert_eq!(&a[0][1..], ["ok", "changed"]);
    assert_eq!(
        (&a[1][4], &a[1][5], &a[1][9]),
        (&"1".to_string(), &"1".to_string(), &name(2))
    );
    assert_eq!(&a[2][1..], ["ok", "ignored"]);
    assert_eq!(names(&roll), ["DSC_0002.NEF", "rejected"]);
    assert_eq!(
        names(&roll.join("rejected")),
        [
            "DSC_0001.NEF",
            "DSC_0001.NEF.edit",
            "DSC_0003.NEF",
            "DSC_0003.NEF.edit"
        ]
    );
    assert_eq!(
        fs::read(roll.join("rejected/DSC_0001.NEF")).unwrap(),
        b"photo 1"
    );
    assert_eq!(
        fs::read_to_string(roll.join("rejected/DSC_0001.NEF.edit")).unwrap(),
        first
    );
    // A reject added since the roll opened moves too, `changed` though the
    // model never held it.
    fs::write(roll.join("DSC_0004.NEF"), b"photo 4").unwrap();
    fs::write(
        roll.join("DSC_0004.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    let b = session.send(&[
        request(4, &["action", "delete-rejected"]),
        request(5, &["state"]),
    ]);
    assert_eq!(&b[0][1..], ["ok", "changed"]);
    assert_eq!((&b[1][4], &b[1][9]), (&"1".to_string(), &name(2)));
    assert_eq!(names(&roll), ["DSC_0002.NEF", "rejected"]);
    assert!(roll.join("rejected/DSC_0004.NEF.edit").is_file());
    // A mixed batch: a reject whose name is taken in `rejected/` is kept
    // and one that is free moves; `refused`, the reason on stderr, the
    // model holding what the roll lists.
    fs::write(roll.join("rejected/DSC_0002.NEF"), b"taken").unwrap();
    fs::write(roll.join("DSC_0005.NEF"), b"photo 5").unwrap();
    fs::write(
        roll.join("DSC_0005.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    let c = session.send(&[
        request(6, &["action", "reject"]),
        request(7, &["action", "delete-rejected"]),
        request(8, &["state"]),
    ]);
    assert_eq!(&c[0][1..], ["ok", "changed"]);
    assert_eq!(c[1][1..3], ["error", "refused"]);
    assert_eq!(
        (&c[2][4], &c[2][9], &c[2][10]),
        (&"1".to_string(), &name(2), &"reject".to_string())
    );
    assert_eq!(
        names(&roll),
        ["DSC_0002.NEF", "DSC_0002.NEF.edit", "rejected"]
    );
    assert_eq!(
        fs::read(roll.join("rejected/DSC_0002.NEF")).unwrap(),
        b"taken"
    );
    assert_eq!(
        fs::read(roll.join("rejected/DSC_0005.NEF")).unwrap(),
        b"photo 5"
    );
    assert!(roll.join("rejected/DSC_0005.NEF.edit").is_file());
    let (ok, _, err) = session.finish();
    assert!(ok, "{err}");
    assert!(
        err.contains("DSC_0002.NEF") && err.contains("already exists"),
        "{err}"
    );
}

#[test]
fn the_binary_exports_over_the_replay_and_notes_the_status_row() {
    let temp = Temp::new("export");
    let roll = temp.0.join("roll");
    fs::create_dir_all(&roll).unwrap();
    // One decodable frame, one that is not a NEF, and one whose sidecar the
    // reader refuses.
    let (w, h) = (64usize, 48usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    fs::write(
        roll.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();
    fs::write(roll.join("DSC_0002.NEF"), b"not really a nef").unwrap();
    fs::write(roll.join("DSC_0003.NEF"), b"not really a nef").unwrap();
    fs::write(
        roll.join("DSC_0003.NEF.edit"),
        "td-photo edit 1\nflag maybe\n",
    )
    .unwrap();
    let roll_s = roll.to_str().unwrap();
    let text_of = |reply: &[String]| -> String {
        String::from_utf8(td_ui::control::unhex(&reply[3]).unwrap()).unwrap()
    };

    // Wide enough that the status row is not cut before the note.
    let mut session = Replay::start(&["--size", "1100x300", roll_s]);
    let a = session.send(&[
        request(1, &["action", "export"]),
        request(2, &["text"]),
        request(3, &["state"]),
        request(4, &["key", &hex(b"e")]),
        request(5, &["text"]),
    ]);
    // The replay runs the export on the request: the JPEG is there when the
    // reply is, the status row says so, and a second takes the next name.
    assert_eq!(&a[0][1..], ["ok", "changed"]);
    assert!(
        text_of(&a[1][1..]).ends_with("| all | 1/3 DSC_0001.NEF unflagged | exported DSC_0001.jpg"),
        "{}",
        text_of(&a[1][1..])
    );
    // The job count stays zero: nothing is outstanding in the replay.
    assert_eq!(a[2][15], "0");
    assert_eq!(&a[3][1..], ["ok", "changed"]);
    assert!(
        text_of(&a[4][1..]).ends_with("| exported DSC_0001-2.jpg"),
        "{}",
        text_of(&a[4][1..])
    );
    assert_eq!(
        names(&roll.join("exported")),
        ["DSC_0001-2.jpg", "DSC_0001.jpg"]
    );
    let first = fs::read(roll.join("exported/DSC_0001.jpg")).unwrap();
    let head = td_photo::jpeg::header(&first).unwrap();
    assert_eq!((head.width, head.height), (60, 44));

    // A frame that cannot be decoded fails on the request: `refused`, the
    // reason on stderr, the row saying it failed, and nothing written; a
    // refused sidecar is refused before anything is read.
    let b = session.send(&[
        request(6, &["action", "select", "1"]),
        request(7, &["action", "export"]),
        request(8, &["text"]),
        request(9, &["action", "select", "2"]),
        request(10, &["action", "export"]),
        request(11, &["text"]),
    ]);
    assert_eq!(&b[0][1..], ["ok", "changed"]);
    assert_eq!(b[1][1..3], ["error", "refused"]);
    assert!(
        text_of(&b[2][1..]).ends_with("| export of DSC_0002.NEF failed"),
        "{}",
        text_of(&b[2][1..])
    );
    assert_eq!(b[4][1..3], ["error", "refused"]);
    assert!(
        text_of(&b[5][1..]).ends_with("| export of DSC_0003.NEF refused"),
        "{}",
        text_of(&b[5][1..])
    );
    assert_eq!(
        names(&roll.join("exported")),
        ["DSC_0001-2.jpg", "DSC_0001.jpg"]
    );
    let (ok, _, err) = session.finish();
    assert!(ok, "{err}");
    assert!(
        err.contains("DSC_0002.NEF") && err.contains("DSC_0003.NEF.edit"),
        "{err}"
    );
}

/// A bare `td-photo` is the window, not the help: without a compositor it
/// is refused with nothing on stdout, the refusal naming what was resolved
/// (the path the display and runtime directory made, or the inherited
/// descriptor, never a display the endpoint did not consult) or, when no
/// endpoint could be made, why, and pointing at `--help`; `open` refuses
/// the same.
#[test]
fn a_bare_invocation_is_the_window_and_without_a_compositor_names_what_it_tried() {
    let refused = |args: &[&str], socket: Option<&str>, display: Option<&str>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-photo"));
        command.args(args);
        command.env_remove("WAYLAND_SOCKET");
        command.env_remove("WAYLAND_DISPLAY");
        command.env("XDG_RUNTIME_DIR", "/nonexistent/td-photo-test-runtime");
        if let Some(socket) = socket {
            command.env("WAYLAND_SOCKET", socket);
        }
        if let Some(display) = display {
            command.env("WAYLAND_DISPLAY", display);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        String::from_utf8(output.stderr).unwrap()
    };
    let names = |err: &str, tried: &str| {
        assert!(
            err.contains(&format!("Wayland {tried}: ")) && err.contains("see --help"),
            "{err}"
        );
    };
    let absolute = "/nonexistent/td-photo-test-display";
    names(
        &refused(&[], None, Some(absolute)),
        &format!("display {absolute}"),
    );
    names(
        &refused(&["open"], None, Some(absolute)),
        &format!("display {absolute}"),
    );
    // Unset is the default display under the runtime directory, and a
    // relative one joins it: the path tried is named, not the bare name.
    names(
        &refused(&[], None, None),
        "display /nonexistent/td-photo-test-runtime/wayland-0",
    );
    names(
        &refused(&[], None, Some("wayland-td-photo-test")),
        "display /nonexistent/td-photo-test-runtime/wayland-td-photo-test",
    );
    // The descriptor wins over the display, and the refusal says so rather
    // than naming a display that was never consulted.
    let socket = refused(&[], Some("2147483647"), Some(absolute));
    names(&socket, "socket descriptor 2147483647");
    assert!(!socket.contains(absolute), "{socket}");
    // No endpoint at all: the reason stands alone, naming no display.
    let invalid = refused(&[], Some("abc"), Some(absolute));
    assert!(
        invalid.contains("Wayland: invalid WAYLAND_SOCKET; see --help")
            && !invalid.contains(absolute),
        "{invalid}"
    );
}

/// A folder listing for the chooser, as the adapter would make it.
fn listing(path: &str, folders: &[&str], files: &[&str]) -> finder::Listing {
    let mut entries = Vec::new();
    for name in folders {
        entries.push(finder::Entry::new(name, "", finder::Kind::Folder, true).unwrap());
    }
    for name in files {
        entries.push(finder::Entry::new(name, "original", finder::Kind::File, false).unwrap());
    }
    finder::Listing::new(path, entries, false).unwrap()
}

#[test]
fn the_roll_chooser_opens_beside_the_roll_and_walks_the_folders_through_the_adapter() {
    let mut c = Controller::new(surface(800, 600));
    c.open("2026-a", b"/photos/2026-a", photos(3)).unwrap();
    let generation = fields(&c)[GENERATION].clone();
    // `choose` asks the adapter for the roll's parent with the roll
    // selected (the parent is the adapter's to find from the roll's own
    // path); nothing opens until the listing is installed.
    let (outcome, effects) = c.action("choose", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::List {
            folder: Some(b"/photos/2026-a".to_vec()),
            parent: true,
        }]
    );
    assert_eq!(fields(&c)[CHOOSER], "-");
    assert_eq!(fields(&c)[GENERATION], generation);
    c.set_listing(
        b"/photos".to_vec(),
        listing("/photos", &["2025", "2026-a", "2026-b"], &["DSC_0009.NEF"]),
        Some("2026-a"),
    )
    .unwrap();
    assert_eq!(fields(&c)[CHOOSER], hex(b"/photos"));
    assert_ne!(fields(&c)[GENERATION], generation);
    let (_, finder) = c.chooser().unwrap();
    assert_eq!(finder.selected_entry().unwrap().name(), "2026-a");
    // The scene shows the finder in place of the grid, the status row the
    // prompt, and the window gets no boxes to blit over it.
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(
        text.contains("/photos") && text.contains("2026-b") && text.contains("Filter"),
        "{text}"
    );
    assert!(!text.contains("DSC_0000.NEF"), "{text}");
    assert!(
        text.lines().last().unwrap().contains("Choose a roll"),
        "{text}"
    );
    assert!(c.visible().is_empty());
    // The keys are the finder's: a letter filters rather than flags, and
    // the roll behind is untouched.
    let (outcome, effects) = c.input(Input::Key { chord: "p" }).unwrap();
    assert_eq!((outcome, effects.len()), (Outcome::Changed, 0));
    assert_eq!(c.chooser().unwrap().1.query(), "p");
    assert_eq!(fields(&c)[FLAG], "-");
    assert_eq!(key(&mut c, "Backspace"), Outcome::Changed);
    assert_eq!(c.chooser().unwrap().1.query(), "");
    // The filter round trip left the selection on the first shown. Return
    // descends into the folder under the cursor; the adapter lists it, and
    // Backspace on an empty filter asks for the parent with this folder
    // selected.
    assert_eq!(
        c.chooser().unwrap().1.selected_entry().unwrap().name(),
        "2025"
    );
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    let generation = fields(&c)[GENERATION].clone();
    let (outcome, effects) = c.input(Input::Key { chord: "Return" }).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::List {
            folder: Some(b"/photos/2026-b".to_vec()),
            parent: false,
        }]
    );
    // The frame is the adapter's to change: asking moves no generation.
    assert_eq!(fields(&c)[GENERATION], generation);
    c.set_listing(
        b"/photos/2026-b".to_vec(),
        listing("/photos/2026-b", &["rejected"], &["DSC_0100.NEF"]),
        None,
    )
    .unwrap();
    assert_eq!(fields(&c)[CHOOSER], hex(b"/photos/2026-b"));
    let generation = fields(&c)[GENERATION].clone();
    let (outcome, effects) = c.input(Input::Key { chord: "Backspace" }).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::List {
            folder: Some(b"/photos/2026-b".to_vec()),
            parent: true,
        }]
    );
    assert_eq!(fields(&c)[GENERATION], generation);
    // M-Up and ^ ascend too, filter or none, so a person who has typed
    // one still has a way up.
    for chord in ["M-Up", "^"] {
        assert_eq!(key(&mut c, "r"), Outcome::Changed);
        let (outcome, effects) = c.input(Input::Key { chord }).unwrap();
        assert_eq!(outcome, Outcome::Changed, "{chord}");
        assert_eq!(
            effects,
            [Effect::List {
                folder: Some(b"/photos/2026-b".to_vec()),
                parent: true,
            }],
            "{chord}"
        );
        assert_eq!(key(&mut c, "Backspace"), Outcome::Changed);
    }
    // A listing the adapter could not make is noted in the finder's
    // status row and changes the frame; the folder stays; the same note
    // again changes nothing; a long note keeps its tail, where the reason
    // is, and a control character is blanked.
    c.note_listing("/photos: permission denied");
    assert_ne!(fields(&c)[GENERATION], generation);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(text.contains("/photos: permission denied"), "{text}");
    assert_eq!(fields(&c)[CHOOSER], hex(b"/photos/2026-b"));
    let generation = fields(&c)[GENERATION].clone();
    c.note_listing("/photos: permission denied");
    assert_eq!(fields(&c)[GENERATION], generation);
    let long = format!("/{}: No such file or directory", "p".repeat(400));
    c.note_listing(&long);
    assert_ne!(fields(&c)[GENERATION], generation);
    let note = c.chooser().unwrap().1.note().to_string();
    assert!(note.starts_with('\u{2026}') && note.ends_with("ppp: No such file or directory"));
    assert!(note.len() <= finder::NOTE_BYTES);
    c.note_listing("a\tb");
    assert_eq!(c.chooser().unwrap().1.note(), "a b");
    // The prompt fits the default width whole.
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert_eq!(
        text.lines().last().unwrap().trim(),
        "Choose a roll: Return enter, Backspace or ^ up, C-Return open here, Escape cancel; type to filter"
    );
    // Return on an original (listed disabled, for what the folder is)
    // descends nowhere; the window's actions are behind the chooser but
    // `scroll` (the finder's wheel), `open`, `quit` and `choose`, which
    // closes it.
    assert_eq!(key(&mut c, "End"), Outcome::Changed);
    assert!(!c.chooser().unwrap().1.selected_entry().unwrap().enabled());
    assert_eq!(key(&mut c, "Return"), Outcome::Ignored);
    assert_eq!(act(&mut c, "pick", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "next", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "delete-rejected", &[]), Outcome::Ignored);
    assert_eq!(fields(&c)[POSITION], "0");
    assert_eq!(act(&mut c, "scroll", &["-1"]), Outcome::Ignored);
    assert_eq!(act(&mut c, "quit", &[]), Outcome::Quit);
    assert_eq!(fields(&c)[CHOOSER], hex(b"/photos/2026-b"));
    let (outcome, effects) = c.action("open", &[&hex(b"/photos/2025")]).unwrap();
    assert_eq!(
        (outcome, effects),
        (
            Outcome::Changed,
            vec![Effect::Open(b"/photos/2025".to_vec())]
        )
    );
    // Space filters, and a held key repeats the moves, a typed character
    // and Backspace while there is a filter, never Return or Escape.
    assert!(c.chooser_repeats("Down") && c.chooser_repeats("PageUp"));
    assert!(c.chooser_repeats("a") && c.chooser_repeats(" "));
    assert!(!c.chooser_repeats("Backspace"));
    assert!(!c.chooser_repeats("M-Up"));
    assert!(!c.chooser_repeats("^"));
    assert!(!c.chooser_repeats("Return") && !c.chooser_repeats("C-Return"));
    assert!(!c.chooser_repeats("Escape") && !c.chooser_repeats("M-x"));
    assert_eq!(key(&mut c, " "), Outcome::Changed);
    assert_eq!(c.chooser().unwrap().1.query(), " ");
    assert!(c.chooser_repeats("Backspace"));
    assert!(!c.chooser_repeats("M-Up") && !c.chooser_repeats("^"));
    assert_eq!(act(&mut c, "choose", &[]), Outcome::Changed);
    assert!(!c.chooser_repeats("Down"));
    assert_eq!(fields(&c)[CHOOSER], "-");
    assert!(!c.visible().is_empty());
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(text.contains("DSC_0000.NEF"), "{text}");
}

#[test]
fn the_roll_chooser_opens_the_folder_in_view_and_closes_on_escape_a_roll_or_a_small_surface() {
    let mut c = Controller::new(surface(800, 600));
    // Without a roll the adapter's working directory is asked for.
    let (outcome, effects) = c.input(Input::Key { chord: "o" }).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::List {
            folder: None,
            parent: false,
        }]
    );
    c.set_listing(
        b"/home/tester".to_vec(),
        listing("/home/tester", &["photos", "videos"], &[]),
        None,
    )
    .unwrap();
    // C-Return opens the folder in view, whatever the selection rests on;
    // the chooser is gone when the reply is.
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    let (outcome, effects) = c.input(Input::Key { chord: "C-Return" }).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(effects, [Effect::Open(b"/home/tester".to_vec())]);
    assert_eq!(fields(&c)[CHOOSER], "-");
    // The keys are the window's again (no roll here, since the test does
    // not carry the open out).
    assert_eq!(
        c.input(Input::Key { chord: "Down" }).unwrap_err(),
        ui::Error::NoRoll
    );
    // Escape closes it with nothing asked for; a roll opening closes it.
    c.action("choose", &[]).unwrap();
    c.set_listing(b"/".to_vec(), listing("/", &["home"], &[]), None)
        .unwrap();
    assert_eq!(fields(&c)[CHOOSER], hex(b"/"));
    // Nothing above the root.
    assert_eq!(key(&mut c, "Backspace"), Outcome::Ignored);
    let (outcome, effects) = c.input(Input::Key { chord: "Escape" }).unwrap();
    assert_eq!((outcome, effects.len()), (Outcome::Changed, 0));
    assert_eq!(fields(&c)[CHOOSER], "-");
    c.action("choose", &[]).unwrap();
    c.set_listing(b"/".to_vec(), listing("/", &["home"], &[]), None)
        .unwrap();
    c.open("roll", b"/r", photos(2)).unwrap();
    assert_eq!(fields(&c)[CHOOSER], "-");
    // The parent is asked for by the roll's own path, absolute or not:
    // the adapter finds it.
    let (_, effects) = c.action("choose", &[]).unwrap();
    assert_eq!(
        effects,
        [Effect::List {
            folder: Some(b"/r".to_vec()),
            parent: true,
        }]
    );
    c.open("roll", b"roll", photos(2)).unwrap();
    let (_, effects) = c.action("choose", &[]).unwrap();
    assert_eq!(
        effects,
        [Effect::List {
            folder: Some(b"roll".to_vec()),
            parent: true,
        }]
    );
    c.set_listing(
        b"/cwd".to_vec(),
        listing("/cwd", &["roll"], &[]),
        Some("roll"),
    )
    .unwrap();
    // The pointer and the wheel are the finder's: a press on its row
    // selects, one on the filter strip sets no filter, the wheel scrolls
    // its list; a resize lays it out again, and a surface too small for
    // it closes it.
    let list = c.chooser().unwrap().1.list_rect();
    assert_eq!(
        press(&mut c, list.x as u32 + 8, list.y as u32 + 8),
        Outcome::Ignored
    );
    assert_eq!(press(&mut c, 80, 36), Outcome::Ignored);
    assert_eq!(fields(&c)[5], "all");
    assert_eq!(
        c.input(Input::Wheel {
            rows: 1,
            columns: 0
        })
        .unwrap()
        .0,
        Outcome::Ignored
    );
    assert_eq!(
        c.input(Input::Resize {
            width: 640,
            height: 400,
            scale: 1
        })
        .unwrap()
        .0,
        Outcome::Changed
    );
    assert_eq!(fields(&c)[CHOOSER], hex(b"/cwd"));
    assert_eq!(c.chooser().unwrap().1.rect(), c.layout().area);
    assert_eq!(
        c.input(Input::Resize {
            width: 100,
            height: 100,
            scale: 1
        })
        .unwrap()
        .0,
        Outcome::Changed
    );
    assert_eq!(fields(&c)[CHOOSER], "-");
    // A listing refused by the model (the area too small for a finder)
    // opens nothing.
    assert_eq!(
        c.set_listing(b"/cwd".to_vec(), listing("/cwd", &["roll"], &[]), None)
            .unwrap_err(),
        ui::Error::Refused
    );
    assert_eq!(fields(&c)[CHOOSER], "-");
}

#[test]
fn the_roll_chooser_covers_develop_and_its_overlays_and_gives_them_back() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    c.set_looks(some_looks());
    act(&mut c, "develop", &[]);
    act(&mut c, "adjust-crop", &[]);
    c.set_preview_fit(c.develop_box());
    assert!(c.develop_box().is_some() && c.crop_adjust_rect().is_some());
    c.action("choose", &[]).unwrap();
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    // Nothing of develop's shows through: no box, no handles, no palette;
    // the text is the finder's and the mode is still develop.
    assert_eq!(c.develop_box(), None);
    assert_eq!(c.crop_adjust_rect(), None);
    assert_eq!(c.look_palette(), None);
    assert_eq!(fields(&c)[MODE], "develop");
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(
        text.contains("Filter") && !text.contains("develop |"),
        "{text}"
    );
    // The develop keys are the finder's: `l` filters, `d` filters.
    assert_eq!(key(&mut c, "l"), Outcome::Changed);
    assert_eq!(c.look_palette(), None);
    assert_eq!(key(&mut c, "Backspace"), Outcome::Changed);
    // Escape closes the chooser, not develop: the box and the handles are
    // back as they were.
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!(fields(&c)[CHOOSER], "-");
    assert_eq!(fields(&c)[MODE], "develop");
    assert!(c.develop_box().is_some() && c.crop_adjust_rect().is_some());
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(text.contains("develop"), "{text}");
}

#[test]
fn the_binary_lists_links_leaves_out_what_is_not_text_and_cuts_a_long_folder_short() {
    use std::os::unix::ffi::OsStrExt;
    let temp = Temp::new("listing");
    let root = temp.0.join("root");
    let roll = root.join("roll");
    fs::create_dir_all(&roll).unwrap();
    fs::write(roll.join("DSC_0001.NEF"), b"photo 1").unwrap();
    fs::create_dir_all(temp.0.join("elsewhere")).unwrap();
    std::os::unix::fs::symlink(temp.0.join("elsewhere"), root.join("linked")).unwrap();
    std::os::unix::fs::symlink(roll.join("DSC_0001.NEF"), root.join("DSC_0002.NEF")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/td-photo", root.join("dangling")).unwrap();
    fs::create_dir(root.join(std::ffi::OsStr::from_bytes(b"caf\xe9"))).unwrap();
    fs::write(root.join("notes.txt"), b"x").unwrap();
    let mut session = Replay::start(&[roll.to_str().unwrap()]);
    let replies = session.send(&[
        request(1, &["action", "choose"]),
        request(2, &["text"]),
        request(3, &["state"]),
    ]);
    assert_eq!(&replies[0][1..], ["ok", "changed"]);
    let text = String::from_utf8(td_ui::control::unhex(&replies[1][4]).unwrap()).unwrap();
    // The linked folder is listed as one marked `link`; a link to a file,
    // a dangling link, a name that is not text and a file that is no
    // original are left out; the roll is selected by name.
    assert!(text.contains("linked") && text.contains("link"), "{text}");
    for absent in ["DSC_0002", "dangling", "caf", "notes"] {
        assert!(!text.contains(absent), "{absent}: {text}");
    }
    assert!(text.contains("2 entries"), "{text}");
    assert_eq!(replies[2][2 + CHOOSER], hex(root.as_os_str().as_bytes()));
    let replies = session.send(&[
        request(4, &["key", &hex(b"C-Return")]),
        request(5, &["state"]),
    ]);
    // Accept opens the folder in view; `roll` was merely selected.
    assert_eq!(&replies[0][1..], ["ok", "changed"]);
    assert_eq!(replies[1][3], hex(root.as_os_str().as_bytes()));
    assert_eq!(replies[1][4], "0");
    // A folder of more entries than the finder holds is cut short, and
    // the status row says so; a relative roll's parent is found too.
    let many = temp.0.join("many");
    fs::create_dir_all(&many).unwrap();
    for i in 0..(finder::ENTRIES + 1) {
        fs::create_dir(many.join(format!("f{i:05}"))).unwrap();
    }
    let replies = session.send(&[
        request(
            6,
            &[
                "action",
                "open",
                &hex(many.join("f00000").as_os_str().as_bytes()),
            ],
        ),
        request(7, &["action", "choose"]),
        request(8, &["text"]),
    ]);
    assert_eq!(&replies[0][1..], ["ok", "changed"]);
    assert_eq!(&replies[1][1..], ["ok", "changed"]);
    let text = String::from_utf8(td_ui::control::unhex(&replies[2][4]).unwrap()).unwrap();
    assert!(
        text.contains(&format!("{} entries, cut short", finder::ENTRIES)),
        "{text}"
    );
    // The bound counts what is listed: a folder of as many subfolders as
    // the finder holds and some sidecars besides is whole.
    let full = temp.0.join("full");
    fs::create_dir_all(&full).unwrap();
    for i in 0..finder::ENTRIES {
        fs::create_dir(full.join(format!("f{i:05}"))).unwrap();
    }
    fs::write(full.join("DSC_0001.xmp"), b"sidecar").unwrap();
    fs::write(full.join("notes.txt"), b"x").unwrap();
    let replies = session.send(&[
        request(
            9,
            &[
                "action",
                "open",
                &hex(full.join("f00000").as_os_str().as_bytes()),
            ],
        ),
        request(10, &["action", "choose"]),
        request(11, &["text"]),
    ]);
    assert_eq!(&replies[0][1..], ["ok", "changed"]);
    assert_eq!(&replies[1][1..], ["ok", "changed"]);
    let text = String::from_utf8(td_ui::control::unhex(&replies[2][4]).unwrap()).unwrap();
    assert!(
        text.contains(&format!("{} entries", finder::ENTRIES)),
        "{text}"
    );
    assert!(!text.contains("cut short"), "{text}");
    let (ok, _, err) = session.finish();
    assert!(ok, "{err}");
}

#[test]
fn the_binary_chooses_a_roll_over_the_replay() {
    let temp = Temp::new("choose");
    let photos = temp.0.join("photos");
    let a = photos.join("2026-a");
    let b = photos.join("2026-b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::create_dir_all(photos.join("gone")).unwrap();
    fs::write(a.join("DSC_0001.NEF"), b"photo 1").unwrap();
    fs::write(b.join("DSC_0002.NEF"), b"photo 2").unwrap();
    fs::write(b.join("DSC_0003.NEF"), b"photo 3").unwrap();
    fs::write(photos.join("notes.txt"), b"not an original").unwrap();
    let photos_hex = hex(photos.as_os_str().as_encoded_bytes());
    let mut session = Replay::start(&[a.to_str().unwrap()]);
    let replies = session.send(&[
        request(1, &["action", "choose"]),
        request(2, &["state"]),
        request(3, &["text"]),
        request(4, &["key", &hex(b"Down")]),
        request(5, &["key", &hex(b"Down")]),
    ]);
    // The chooser lists the roll's parent with the roll selected; the
    // text shows the folders and not the file that is no original.
    assert_eq!(&replies[0][1..], ["ok", "changed"]);
    assert_eq!(replies[1][2 + CHOOSER], photos_hex);
    let text = String::from_utf8(td_ui::control::unhex(&replies[2][4]).unwrap()).unwrap();
    assert!(
        text.contains("2026-a") && text.contains("2026-b") && text.contains("gone"),
        "{text}"
    );
    assert!(!text.contains("notes.txt"), "{text}");
    assert!(text.contains("3 entries"), "{text}");
    // Down twice from 2026-a lands on `gone`, removed meanwhile: the
    // descent is refused, the reason noted in the finder, the folder kept.
    assert_eq!(&replies[3][1..], ["ok", "changed"]);
    assert_eq!(&replies[4][1..], ["ok", "changed"]);
    fs::remove_dir(photos.join("gone")).unwrap();
    let replies = session.send(&[
        request(6, &["key", &hex(b"Return")]),
        request(7, &["state"]),
    ]);
    assert_eq!(&replies[0][1..3], ["error", "refused"]);
    assert_eq!(replies[1][2 + CHOOSER], photos_hex);
    let (b_hex, roll_hex) = (
        hex(b.as_os_str().as_encoded_bytes()),
        hex(a.as_os_str().as_encoded_bytes()),
    );
    let replies = session.send(&[
        request(8, &["text"]),
        request(9, &["key", &hex(b"Up")]),
        request(10, &["key", &hex(b"Return")]),
        request(11, &["state"]),
        request(12, &["key", &hex(b"Backspace")]),
        request(13, &["state"]),
        request(14, &["key", &hex(b"Return")]),
        request(15, &["key", &hex(b"C-Return")]),
        request(16, &["state"]),
        request(17, &["key", &hex(b"Down")]),
        request(18, &["state"]),
    ]);
    let text = String::from_utf8(td_ui::control::unhex(&replies[0][4]).unwrap()).unwrap();
    assert!(
        text.contains("gone") && text.contains("No such file"),
        "{text}"
    );
    // Up to 2026-b, Return lists it (its original shown), Backspace lists
    // the parent again with 2026-b selected, so Return goes back down and
    // C-Return opens it: the roll is 2026-b and the chooser is gone, the
    // keys the window's again.
    assert_eq!(&replies[1][1..], ["ok", "changed"]);
    assert_eq!(&replies[2][1..], ["ok", "changed"]);
    assert_eq!(replies[3][2 + CHOOSER], b_hex);
    assert_eq!(replies[3][3], roll_hex);
    assert_eq!(&replies[4][1..], ["ok", "changed"]);
    assert_eq!(replies[5][2 + CHOOSER], photos_hex);
    assert_eq!(&replies[6][1..], ["ok", "changed"]);
    assert_eq!(&replies[7][1..], ["ok", "changed"]);
    assert_eq!(replies[8][3], b_hex);
    assert_eq!(replies[8][2 + CHOOSER], "-");
    assert_eq!(
        (&replies[8][4], &replies[8][9]),
        (&"2".to_string(), &name(2))
    );
    assert_eq!(&replies[9][1..], ["ok", "changed"]);
    assert_eq!(replies[10][9], name(3));
    let (ok, _, err) = session.finish();
    assert!(ok, "{err}");
    assert!(
        err.contains("gone") && err.contains("No such file"),
        "{err}"
    );
}

/// The mode strip: which mode is in view and which can be pressed, and
/// a press changing the mode from whatever is open.
#[test]
fn the_mode_strip_names_the_mode_in_view_and_changes_it() {
    let mut c = Controller::new(surface(800, 600));
    let mode_press = |c: &mut Controller, x: u32| {
        c.input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y: 12,
        })
        .unwrap()
    };
    // Before a roll only the chooser can be pressed and no mode is in
    // view; the strip reads back by its labels, the filters are enabled.
    assert_eq!(
        c.mode_states(),
        [(false, true), (false, false), (false, false)]
    );
    assert!(c.filter_states().iter().all(|state| state.1));
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert_eq!(
        text.lines().next().unwrap().trim(),
        "Roll Selection   Culling   Develop"
    );
    assert_eq!(press(&mut c, 150, 12), Outcome::Ignored);
    assert_eq!(press(&mut c, 230, 12), Outcome::Ignored);
    // Roll Selection asks the adapter to list the working directory; the
    // chooser is in view once the listing is installed, the filters and
    // the other modes disabled with no roll behind.
    let (outcome, effects) = mode_press(&mut c, 20);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::List {
            folder: None,
            parent: false,
        }]
    );
    c.set_listing(
        b"/photos".to_vec(),
        listing("/photos", &["2025"], &[]),
        None,
    )
    .unwrap();
    assert_eq!(
        c.mode_states(),
        [(true, true), (false, false), (false, false)]
    );
    assert!(c.filter_states().iter().all(|state| !state.1));
    assert_eq!(press(&mut c, 20, 12), Outcome::Ignored);
    assert_eq!(press(&mut c, 150, 12), Outcome::Ignored);
    assert!(c.chooser().is_some());
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    // With a roll open, Culling is in view and Develop can be pressed:
    // it develops the cursor's photo, the filters disabled and inert.
    c.open("roll", b"/r", photos(3)).unwrap();
    assert_eq!(
        c.mode_states(),
        [(false, true), (true, true), (false, true)]
    );
    assert_eq!(press(&mut c, 150, 12), Outcome::Ignored);
    // From the single view, as from the grid: develop reports the grid
    // as the view it will return to.
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 230, 12), Outcome::Changed);
    assert_eq!((c.mode(), c.view()), (ui::Mode::Develop, View::Grid));
    assert_eq!(
        c.mode_states(),
        [(false, true), (false, true), (true, true)]
    );
    assert!(c.filter_states().iter().all(|state| !state.1));
    assert_eq!(press(&mut c, 80, 36), Outcome::Ignored);
    assert_eq!(c.filter(), Filter::All);
    assert_eq!(c.crop_drag(), None);
    assert_eq!(press(&mut c, 230, 12), Outcome::Ignored);
    // Culling leaves develop whole, its palette with it.
    c.set_looks(some_looks());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 150, 12), Outcome::Changed);
    assert_eq!((c.mode(), c.view()), (ui::Mode::Cull, View::Grid));
    assert!(c.look_palette().is_none());
    // Through the chooser: Roll Selection lists beside the roll; Develop
    // closes the chooser and develops; from develop, Culling closes it
    // and leaves develop.
    let (outcome, effects) = mode_press(&mut c, 20);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::List {
            folder: Some(b"/r".to_vec()),
            parent: true,
        }]
    );
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    assert_eq!(press(&mut c, 230, 12), Outcome::Changed);
    assert!(c.chooser().is_none());
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_eq!(mode_press(&mut c, 20).0, Outcome::Changed);
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    assert_eq!(
        c.mode_states(),
        [(true, true), (false, true), (false, true)]
    );
    assert_eq!(press(&mut c, 150, 12), Outcome::Changed);
    assert!(c.chooser().is_none());
    assert_eq!((c.mode(), c.view()), (ui::Mode::Cull, View::Grid));
    // The gap between two buttons and the strip's margin are no targets.
    assert_eq!(press(&mut c, 140, 12), Outcome::Ignored);
    assert_eq!(press(&mut c, 20, 1), Outcome::Ignored);
}

/// The mode strip's generations and its remaining transitions: a change
/// bumps once, an ignored press and a chooser request never (the listing
/// is the adapter's change), the chooser over cull to Culling and over
/// develop to Develop, an empty roll's Develop disabled, and Culling out
/// of crop-adjust and out of a drag in progress.
#[test]
fn the_mode_strip_bumps_once_a_change_and_never_a_request() {
    let mut c = Controller::new(surface(800, 600));
    let generation = |c: &Controller| fields(c)[GENERATION].clone();
    let mode_press = |c: &mut Controller, x: u32| {
        c.input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y: 12,
        })
        .unwrap()
    };
    // An empty roll: Culling in view, Develop disabled with no cursor.
    c.open("roll", b"/r", photos(0)).unwrap();
    assert_eq!(
        c.mode_states(),
        [(false, true), (true, true), (false, false)]
    );
    let before = generation(&c);
    assert_eq!(press(&mut c, 230, 12), Outcome::Ignored);
    assert_eq!(press(&mut c, 150, 12), Outcome::Ignored);
    assert_eq!(generation(&c), before);
    // Roll Selection asks; the generation waits for the listing, which
    // moves it once; Culling then closes the chooser over the grid, once.
    c.open("roll", b"/r", photos(3)).unwrap();
    let before = generation(&c);
    assert_eq!(mode_press(&mut c, 20).0, Outcome::Changed);
    assert_eq!(generation(&c), before);
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    let listed = generation(&c);
    assert_ne!(listed, before);
    assert_eq!(press(&mut c, 150, 12), Outcome::Changed);
    assert!(c.chooser().is_none());
    assert_eq!((c.mode(), c.view()), (ui::Mode::Cull, View::Grid));
    let after = generation(&c);
    assert_ne!(after, listed);
    // Develop from the grid bumps once; the chooser over develop, then
    // Develop again: the chooser closes, develop stays, one bump.
    assert_eq!(press(&mut c, 230, 12), Outcome::Changed);
    let developed = generation(&c);
    assert_ne!(developed, after);
    assert_eq!(mode_press(&mut c, 20).0, Outcome::Changed);
    assert_eq!(generation(&c), developed);
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    let listed = generation(&c);
    assert_eq!(press(&mut c, 230, 12), Outcome::Changed);
    assert!(c.chooser().is_none());
    assert_eq!(c.mode(), ui::Mode::Develop);
    assert_ne!(generation(&c), listed);
    // Culling out of crop-adjust drops the sub-mode with the mode.
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.adjusting());
    assert_eq!(press(&mut c, 150, 12), Outcome::Changed);
    assert!(!c.adjusting() && c.mode() == ui::Mode::Cull);
    // And out of a marquee in progress: the drag is dropped, no crop
    // effect made.
    assert_eq!(press(&mut c, 230, 12), Outcome::Changed);
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));
    assert_eq!(press(&mut c, 400, 200), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 300), Outcome::Changed);
    assert!(c.crop_drag().is_some());
    let (outcome, effects) = mode_press(&mut c, 150);
    assert_eq!((outcome, effects.len()), (Outcome::Changed, 0));
    assert_eq!(c.crop_drag(), None);
    assert_eq!((c.mode(), c.view()), (ui::Mode::Cull, View::Grid));
}

/// The chords the chooser names are the ones td-ui's keymap makes, so a
/// key the prompt names does what it says: the keymap spells its command
/// keys its own way (`Backspace`, not X's `BackSpace`), and a chord the
/// chooser matched by another spelling would reach it never.
#[test]
fn the_chooser_names_its_chords_as_the_keymap_spells_them() {
    let map =
        td_ui::keyboard::Keymap::parse(include_str!("../../td-ui/tests/fixtures/us.xkb")).unwrap();
    let chord = |code: u32, mask: u32| {
        let modifiers = td_ui::keyboard::Modifiers {
            depressed: mask,
            ..Default::default()
        };
        map.translate(code, modifiers).unwrap().unwrap().chord
    };
    let mut c = Controller::new(surface(800, 600));
    c.set_listing(
        b"/photos/2026-b".to_vec(),
        listing("/photos/2026-b", &["rejected"], &["DSC_0100.NEF"]),
        None,
    )
    .unwrap();
    // Each key by its evdev code with a modifier mask (1 shift, 4
    // control, 8 alt): the moves are consumed or change the
    // finder, Return asks for the folder under the cursor, Backspace with
    // no filter, M-Up and ^ ask for the parent, a space types a filter
    // character and Escape closes.
    for (code, mask, expected, asks) in [
        (108, 0, "Down", None),
        (103, 0, "Up", None),
        (109, 0, "PageDown", None),
        (104, 0, "PageUp", None),
        (107, 0, "End", None),
        (102, 0, "Home", None),
        (28, 0, "Return", Some(false)),
        (14, 0, "Backspace", Some(true)),
        (103, 8, "M-Up", Some(true)),
        (7, 1, "^", Some(true)),
    ] {
        let chord = chord(code, mask);
        assert_eq!(chord, expected);
        let (outcome, effects) = c.input(Input::Key { chord: &chord }).unwrap();
        assert_ne!(outcome, Outcome::Ignored, "{chord}");
        let asked = effects.iter().find_map(|e| match e {
            Effect::List { parent, .. } => Some(*parent),
            _ => None,
        });
        assert_eq!(asked, asks, "{chord}");
        assert!(c.chooser().is_some(), "{chord}");
    }
    // A plain space is the character itself; `Space` is the keymap's name
    // for it under a modifier.
    assert_eq!(chord(57, 0), " ");
    assert_eq!(key(&mut c, " "), Outcome::Changed);
    assert_eq!(c.chooser().unwrap().1.query(), " ");
    assert_eq!(chord(1, 0), "Escape");
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert!(c.chooser().is_none());
    c.set_listing(
        b"/photos/2026-b".to_vec(),
        listing("/photos/2026-b", &["rejected"], &["DSC_0100.NEF"]),
        None,
    )
    .unwrap();
    assert_eq!(chord(28, 4), "C-Return");
    let (outcome, effects) = c.input(Input::Key { chord: "C-Return" }).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(effects, [Effect::Open(b"/photos/2026-b".to_vec())]);
    assert!(c.chooser().is_none());
}

/// Develops the first photo and gives it three history steps: a run of
/// two exposure nudges (one step, the second nudge taking the first's
/// step), a look, and a nudge back behind it.
fn with_history() -> Controller {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["0", "-"]);
    assert_eq!(carry(&mut c, "expose-in", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["1", "0"]);
    assert_eq!(carry(&mut c, "expose-in", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["1", "0"]);
    assert_eq!(fields(&c)[EXPOSURE], "0.66");
    assert_eq!(carry(&mut c, "look", &["portra"]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["2", "1"]);
    assert_eq!(carry(&mut c, "expose-out", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["3", "2"]);
    assert_eq!(c.steps().len(), 3);
    assert_eq!(fields(&c)[EXPOSURE], "0.33");
    c
}

/// A run taking up the last step selects that step, as a step added
/// does, wherever the selection was.
#[test]
fn a_run_taken_up_selects_its_step() {
    let mut c = with_history();
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["3", "0"]);
    assert_eq!(carry(&mut c, "expose-out", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["3", "2"]);
    assert_eq!(fields(&c)[EXPOSURE], "0.00");
    // A clear is a step of its own, not the run's: Uncrop after a crop
    // leaves the crop's step, and undo brings the crop back.
    assert_eq!(
        carry(&mut c, "crop", &["0.1000", "0.1000", "0.5000", "0.5000"]),
        Outcome::Changed
    );
    assert_eq!(&fields(&c)[STEPS..=STEP], ["4", "3"]);
    assert_eq!(carry(&mut c, "uncrop", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["5", "4"]);
    assert_eq!(fields(&c)[CROP], "-");
    assert_eq!(carry(&mut c, "undo", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.1000 0.1000 0.5000 0.5000");
}

#[test]
fn the_history_records_the_develop_steps_and_undoes_toggles_and_deletes_them() {
    let mut c = with_history();

    // The selection is the newest step; Up and Down walk it, clamped, and
    // are the history's in develop rather than grid-row moves.
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(fields(&c)[STEP], "1");
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(fields(&c)[STEP], "0");
    assert_eq!(key(&mut c, "Up"), Outcome::Ignored);
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    assert_eq!(fields(&c)[STEP], "2");
    assert_eq!(key(&mut c, "Down"), Outcome::Ignored);
    assert_eq!(fields(&c)[POSITION], "0");

    // Toggling the selected step off takes its key out of the settings in
    // force and leaves the step, and the selection, in place; toggling
    // again brings it back.
    let (outcome, effects) = c.action("step-toggle", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::StepToggle {
            index: 0,
            name: file(0),
            step: 2,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "0.66");
    assert_eq!(&fields(&c)[STEPS..=STEP], ["3", "2"]);
    assert!(!c.steps()[2].on);
    assert_eq!(carry(&mut c, "step-toggle", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "0.33");
    assert!(c.steps()[2].on);

    // A step off in the middle: its key falls back to the earlier step's
    // value; set again, the new value is a new step, the off one kept.
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(carry_key(&mut c, "t"), Outcome::Changed);
    assert_eq!(fields(&c)[LOOK], "-");
    assert_eq!(carry(&mut c, "look", &["velvia"]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["4", "3"]);
    assert_eq!(fields(&c)[LOOK], "velvia");
    assert_eq!(
        c.steps()
            .iter()
            .map(|step| (step.on, step.key, step.value.clone()))
            .collect::<Vec<_>>(),
        [
            (true, Key::Exposure, Some("0.66".to_string())),
            (false, Key::Look, Some("portra".to_string())),
            (true, Key::Exposure, Some("0.33".to_string())),
            (true, Key::Look, Some("velvia".to_string())),
        ]
    );

    // Deleting the selected step closes the later ones up; the selection
    // stays at its index, clamped to the end.
    let (_, effects) = c.action("step-delete", &[]).unwrap();
    assert_eq!(
        effects,
        [Effect::StepDelete {
            index: 0,
            name: file(0),
            step: 3,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["3", "2"]);
    assert_eq!(fields(&c)[LOOK], "-");
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(carry_key(&mut c, "Backspace"), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["2", "1"]);
    assert_eq!(fields(&c)[EXPOSURE], "0.33");
    assert_eq!(fields(&c)[LOOK], "-");

    // Undo takes the last step back; with none left it asks all the same
    // and the adapter, finding nothing to take, settles it ignored.
    let (_, effects) = c.action("undo", &[]).unwrap();
    assert_eq!(
        effects,
        [Effect::Undo {
            index: 0,
            name: file(0),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["1", "0"]);
    assert_eq!(fields(&c)[EXPOSURE], "0.66");
    assert_eq!(carry_key(&mut c, "z"), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["0", "-"]);
    assert_eq!(fields(&c)[EXPOSURE], "-");
    assert_eq!(carry_key(&mut c, "z"), Outcome::Ignored);
    // Without a step, Up and Down have nothing to move, and the step
    // actions nothing to act on: each is ignored and asks for nothing.
    assert_eq!(key(&mut c, "Up"), Outcome::Ignored);
    assert_eq!(key(&mut c, "Down"), Outcome::Ignored);
    assert_eq!(act(&mut c, "step-toggle", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "step-delete", &[]), Outcome::Ignored);

    // Reset clears the history with the keys.
    assert_eq!(carry(&mut c, "expose-in", &[]), Outcome::Changed);
    assert_eq!(carry(&mut c, "look", &["portra"]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["2", "1"]);
    assert_eq!(carry(&mut c, "reset", &[]), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["0", "-"]);

    // The history is the cursor photo's: Right moves to a photo without
    // one, Left back to this one, its newest step selected again; and
    // leaving develop leaves the history behind, where undo and the step
    // actions are not the mode's.
    assert_eq!(carry(&mut c, "expose-in", &[]), Outcome::Changed);
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["0", "-"]);
    assert_eq!(key(&mut c, "Left"), Outcome::Changed);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["1", "0"]);
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!(c.mode(), ui::Mode::Cull);
    assert_eq!(&fields(&c)[STEPS..=STEP], ["0", "-"]);
    for name in ["undo", "step-toggle", "step-delete"] {
        assert_eq!(act(&mut c, name, &[]), Outcome::Ignored, "{name}");
    }
    // Down in cull is the grid-row move it was.
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "4");
}

#[test]
fn the_history_pane_takes_the_pointer_and_is_in_the_scene_frame() {
    let mut c = with_history();
    let layout = c.layout();
    let list = layout.history().expect("a history pane on 800x600");
    assert_eq!(list.rect().x, 0);
    assert_eq!(list.rect().y, layout.area.y);
    assert_eq!(list.rect().width as usize, ui::PANE_W);
    let buttons = layout.history_buttons();
    let button = |i: usize| buttons[i].expect("a pane button").rect();
    // The buttons sit on the band under the list, from a cell in, inside
    // the pane's width; the develop region begins where the pane ends.
    assert_eq!(button(0).x, 8);
    assert_eq!(button(0).width, 64);
    assert_eq!(button(2).x + i64::from(button(2).width), 200);
    assert_eq!(
        button(0).y,
        list.rect().y + i64::from(list.rect().height) + 2
    );
    assert_eq!(layout.develop_region().x, ui::PANE_W as i64);

    // A press on a step selects it; on the selected one, nothing; the
    // rows below the steps are chrome, as are a move and a release.
    let row = |i: usize| {
        let r = list.row(i).unwrap();
        (
            (r.x + i64::from(r.width) / 2) as u32,
            (r.y + i64::from(r.height) / 2) as u32,
        )
    };
    let (x, y) = row(0);
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    assert_eq!(fields(&c)[STEP], "0");
    assert_eq!(press(&mut c, x, y), Outcome::Ignored);
    let (x2, y2) = row(5);
    assert_eq!(press(&mut c, x2, y2), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, x, y), Outcome::Ignored);
    assert_eq!(release(&mut c, x, y).0, Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // The buttons ask for the selected step toggled or deleted, or the
    // last step back, the same effects the keys make.
    let centre = |r: Rect| {
        (
            (r.x + i64::from(r.width) / 2) as u32,
            (r.y + i64::from(r.height) / 2) as u32,
        )
    };
    let (bx, by) = centre(button(0));
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: bx,
            y: by,
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::StepToggle {
            index: 0,
            name: file(0),
            step: 0,
        }]
    );
    let (bx, by) = centre(button(1));
    let (_, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: bx,
            y: by,
        })
        .unwrap();
    assert!(matches!(&effects[..], [Effect::StepDelete { step: 0, .. }]));
    let (bx, by) = centre(button(2));
    let (_, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: bx,
            y: by,
        })
        .unwrap();
    assert!(matches!(&effects[..], [Effect::Undo { index: 0, .. }]));
    // The gap between two buttons is chrome.
    let gap = (button(0).x + i64::from(button(0).width) + 4) as u32;
    assert_eq!(press(&mut c, gap, by), Outcome::Ignored);

    // The pane is in the scene: its steps read back as text, a step off
    // paints differently (dimmed) from one on, and a selection move is a
    // frame change.
    // The pane's columns, not the facts row, list the steps.
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    let pane_has = |text: &str, what: &str| {
        text.lines().any(|line| {
            line.chars()
                .take(ui::PANE_W / 8)
                .collect::<String>()
                .contains(what)
        })
    };
    assert!(
        pane_has(&text, "exposure 0.33") && pane_has(&text, "exposure 0.66"),
        "{text}"
    );
    assert!(pane_has(&text, "look portra"), "{text}");
    assert!(
        pane_has(&text, "Toggle") && pane_has(&text, "Delete") && pane_has(&text, "Undo"),
        "{text}"
    );
    // A crop step shows in whole percents, so it fits the pane's row.
    assert_eq!(
        carry(&mut c, "crop", &["0.1250", "0.0500", "0.7500", "0.9000"]),
        Outcome::Changed
    );
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(pane_has(&text, "crop 13,5 75x90"), "{text}");
    assert_eq!(carry_key(&mut c, "z"), Outcome::Changed);
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    assert_eq!(key(&mut c, "Up"), Outcome::Changed);
    // A step off is painted in the disabled ink within the pane, which no
    // step on is; the facts row changing too is not what this pins.
    let dimmed = |c: &Controller| {
        let mut count = 0;
        let list = c.layout().history().unwrap().rect();
        c.scene().emit(c.surface().bounds(), &mut |draw| {
            if let Primitive::Glyph { x, y, style, .. } = draw.primitive {
                if style.ink == DISABLED && list.contains(x, y) {
                    count += 1;
                }
            }
        });
        count
    };
    assert_eq!(dimmed(&c), 0);
    assert_eq!(carry(&mut c, "step-toggle", &[]), Outcome::Changed);
    // The row's two-cell mark prefix is dimmed with its label.
    assert_eq!(dimmed(&c), "  exposure 0.33".len());
    let off = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    assert_eq!(key(&mut c, "Down"), Outcome::Changed);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        off
    );
    // A drag begun over the preview ends on its release over the pane.
    c.set_preview_fit(Some(Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    }));
    assert_eq!(press(&mut c, 400, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 250), Outcome::Changed);
    assert!(c.crop_drag().is_some());
    let (outcome, effects) = release(&mut c, x, y);
    assert_eq!(outcome, Outcome::Changed);
    assert!(matches!(
        &effects[..],
        [Effect::Edit { key: Key::Crop, .. }]
    ));
    assert_eq!(c.crop_drag(), None);
    // The band's gaps and margins are the pane's, not the preview's: a
    // press there starts no drag.
    let gap_y = (button(0).y + i64::from(button(0).height) + 1) as u32;
    assert_eq!(press(&mut c, gap, gap_y), Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // Outside develop there is no pane and the area's left is the grid's.
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(!text.contains("Toggle"), "{text}");

    // A surface too short for a row has no pane and no buttons, and the
    // pointer over where one would be starts nothing.
    let mut small = Controller::new(surface(800, 80));
    small.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut small, "develop", &[]), Outcome::Changed);
    assert!(small.layout().history().is_none());
    assert!(small.layout().history_buttons().iter().all(Option::is_none));
    assert_eq!(press(&mut small, 40, 50), Outcome::Ignored);
}

/// The tool band's buttons on an 800x600 surface in develop, and the
/// slider after them: `TOOL_BUTTONS` from a cell into the region, each
/// asking for what the key does, enabled as the photo's state allows.
#[test]
fn the_tool_band_drives_the_develop_edits() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    // Outside develop the bands and the new actions are not the mode's.
    assert_eq!(act(&mut c, "uncrop", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "exposure", &["1.00"]), Outcome::Ignored);
    assert_eq!(act(&mut c, "look-1", &[]), Outcome::Ignored);
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let layout = c.layout();
    assert_eq!(
        layout.tool_band(),
        Rect {
            x: 216,
            y: 48,
            width: 584,
            height: 24
        }
    );
    assert_eq!(
        layout.look_band(),
        Rect {
            x: 216,
            y: 72,
            width: 584,
            height: 24
        }
    );
    assert_eq!(layout.develop_view().y, 96);
    let tools = layout.tools();
    let button = |i: usize| tools.buttons[i].expect("a tool button").rect();
    assert_eq!(
        button(0),
        Rect {
            x: 224,
            y: 50,
            width: 48,
            height: 20
        }
    );
    assert_eq!(button(1).x, 224 + 48 + 8);
    assert_eq!(
        button(5).x + i64::from(button(5).width),
        224 + 48 + 64 + 48 + 56 + 24 + 24 + 5 * 8
    );
    let slider = tools.slider.expect("a slider after the buttons");
    assert_eq!(
        slider.rect().x,
        button(5).x + i64::from(button(5).width) + 8
    );
    assert_eq!(slider.rect().x + i64::from(slider.rect().width), 800 - 8);
    assert_eq!(slider.rect().y, 48);
    assert!(slider.travel() as usize >= ui::EXPOSURE_STEPS);
    let centre = |r: Rect| {
        (
            (r.x + i64::from(r.width) / 2) as u32,
            (r.y + i64::from(r.height) / 2) as u32,
        )
    };
    let click = |c: &mut Controller, r: Rect| {
        let (x, y) = centre(r);
        c.input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y,
        })
        .unwrap()
    };
    // Crop toggles crop-adjust and shows selected; the status row names
    // the sub-mode.
    assert_eq!(c.tool_states(0), [true, false, false, false, true, true]);
    assert_eq!(click(&mut c, button(0)), (Outcome::Changed, vec![]));
    assert!(c.adjusting());
    assert!(c.scene().status_line().ends_with(" | develop crop-adjust"));
    assert_eq!(click(&mut c, button(0)), (Outcome::Changed, vec![]));
    assert!(!c.adjusting());
    assert!(c.scene().status_line().ends_with(" | develop"));
    // Uncrop, Undo and Reset are disabled without a crop or a step: a
    // press on them is inert.
    for i in 1..=3 {
        assert_eq!(click(&mut c, button(i)), (Outcome::Ignored, vec![]));
    }
    // The exposure steps ask for the nudges the keys make.
    let (outcome, effects) = click(&mut c, button(5));
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Expose {
            index: 0,
            name: file(0),
            delta: ui::EXPOSURE_STEP,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    let (_, effects) = click(&mut c, button(4));
    assert_eq!(
        effects,
        [Effect::Expose {
            index: 0,
            name: file(0),
            delta: -ui::EXPOSURE_STEP,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    // The two nudges are one step, the second taking the first's.
    assert_eq!(&fields(&c)[STEPS..=STEP], ["1", "0"]);
    // With a step, Undo and Reset are enabled and ask for what the keys do.
    assert_eq!(c.tool_states(0), [true, false, true, true, true, true]);
    let (_, effects) = click(&mut c, button(2));
    assert_eq!(
        effects,
        [Effect::Undo {
            index: 0,
            name: file(0),
        }]
    );
    let (_, effects) = click(&mut c, button(3));
    assert_eq!(
        effects,
        [Effect::Reset {
            index: 0,
            name: file(0),
        }]
    );
    // The states are the asked-about photo's, not the cursor's.
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(c.tool_states(0), [true, false, true, true, true, true]);
    assert_eq!(c.tool_states(1), [true, false, false, false, true, true]);
    assert_eq!(key(&mut c, "Left"), Outcome::Changed);
    // A crop enables Uncrop, which clears it through the crop Edit; `C`
    // is its key.
    assert_eq!(
        carry(&mut c, "crop", &["0.1000", "0.1000", "0.5000", "0.5000"]),
        Outcome::Changed
    );
    assert!(c.tool_states(0)[1]);
    let (_, effects) = click(&mut c, button(1));
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: None,
        }]
    );
    let (_, effects) = c.input(Input::Key { chord: "C" }).unwrap();
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: None,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "-");
    assert!(!c.tool_states(0)[1]);
    // `exposure` is an absolute in the sidecar's spelling; other spellings
    // are bad-argument before any write.
    let (_, effects) = c.action("exposure", &["-1.25"]).unwrap();
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Exposure,
            value: Some("-1.25".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "-1.25");
    for bad in ["1.5", "5.01", "+1.00", "x"] {
        assert_eq!(
            c.action("exposure", &[bad]).unwrap_err(),
            ui::Error::BadArgument,
            "{bad}"
        );
    }
    // The bands' chrome, and a move or release over them, are inert; a
    // press on a band never starts a crop drag.
    let gap = (button(0).x + i64::from(button(0).width) + 4) as u32;
    assert_eq!(press(&mut c, gap, 60), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 300, 60), Outcome::Ignored);
    assert_eq!(release(&mut c, 300, 60).0, Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);
    // The facts row is gone from develop: the name row alone leads the box.
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(!text.contains("| sidecar "), "{text}");
    assert!(text.contains("DSC_0000.NEF"), "{text}");
    // A surface too narrow for the buttons on one row wraps them: at 500,
    // `+` would end at 528 and starts the second row, and the slider
    // follows it on that row; the band is two rows.
    let mut narrow = Controller::new(surface(500, 600));
    narrow.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut narrow, "develop", &[]), Outcome::Changed);
    let tools = narrow.layout().tools();
    assert!(tools.buttons[0].is_some());
    assert_eq!(
        tools.buttons[4].unwrap().rect(),
        Rect {
            x: 472,
            y: 50,
            width: 24,
            height: 20
        }
    );
    assert_eq!(
        tools.buttons[5].unwrap().rect(),
        Rect {
            x: 224,
            y: 74,
            width: 24,
            height: 20
        }
    );
    assert_eq!(
        tools.slider.unwrap().rect(),
        Rect {
            x: 256,
            y: 72,
            width: 492 - 256,
            height: 24
        }
    );
    assert_eq!(narrow.layout().tool_band().height, 48);
    assert_eq!(narrow.layout().look_band().y, 96);
    assert_eq!(press(&mut narrow, 700, 60), Outcome::Ignored);
    // Room for the buttons but not a column per step on their row puts
    // the slider on a row of its own across the band; one column more,
    // and it is after them with its hundred steps. The buttons (264 wide,
    // six cells between and one before) end at 528, the slider starts a
    // cell on at 536 and ends at width - 8, so its travel is width - 544
    // - KNOB_WIDTH: 100 at 656.
    for (width, own_row) in [(655, true), (656, false)] {
        let c = Controller::new(surface(width, 600));
        let tools = c.layout().tools();
        assert!(tools.buttons.iter().all(Option::is_some), "{width}");
        let slider = tools.slider.unwrap();
        if own_row {
            assert_eq!(slider.rect().y, 72, "{width}");
            assert_eq!(slider.rect().x, 224, "{width}");
            assert_eq!(c.layout().tool_band().height, 48, "{width}");
        } else {
            assert_eq!(slider.rect().y, 48, "{width}");
            assert_eq!(slider.rect().x, 536, "{width}");
            assert_eq!(slider.travel() as usize, ui::EXPOSURE_STEPS);
            assert_eq!(c.layout().tool_band().height, 24, "{width}");
        }
    }
    // Too narrow for a column a step even on its own row (the travel is
    // the width less 244 and the knob), no slider.
    let tiny = Controller::new(surface(304, 600));
    let tools = tiny.layout().tools();
    assert!(tools.buttons.iter().all(Option::is_some));
    assert!(tools.slider.is_none());
    // At scale two the bands and the box double with the rows.
    let mut two = Controller::new(surface(800, 600));
    two.open("roll", b"/r", photos(1)).unwrap();
    two.input(Input::Resize {
        width: 1600,
        height: 1200,
        scale: 2,
    })
    .unwrap();
    assert_eq!(act(&mut two, "develop", &[]), Outcome::Changed);
    let layout = two.layout();
    assert_eq!(
        layout.tool_band(),
        Rect {
            x: 432,
            y: 96,
            width: 1168,
            height: 48
        }
    );
    assert_eq!(
        layout.look_band(),
        Rect {
            x: 432,
            y: 144,
            width: 1168,
            height: 48
        }
    );
    assert_eq!(layout.develop_view().y, 192);
    let tools = layout.tools();
    assert_eq!(
        tools.buttons[0].unwrap().rect(),
        Rect {
            x: 448,
            y: 100,
            width: 96,
            height: 40
        }
    );
    // Six labels of 6, 8, 6, 7, 3 and 3 cells at 16 each, a cell between.
    assert_eq!(tools.slider.unwrap().rect().x, 448 + 528 + 96);
}

/// The exposure slider: the knob at the sidecar's exposure, a press and
/// drag moving it (frame changes, no write), the release committing the
/// exposure at the pointer's value as the `exposure` action does, unless
/// it is the value in force.
#[test]
fn the_exposure_slider_commits_on_release() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let slider = c.layout().tools().slider.unwrap();
    let mid = |c: &Controller, value: usize| {
        let knob = c
            .layout()
            .tools()
            .slider
            .unwrap()
            .knob(value, ui::EXPOSURE_STEPS);
        (
            (knob.x + i64::from(knob.width) / 2) as u32,
            (knob.y + i64::from(knob.height) / 2) as u32,
        )
    };
    assert_eq!(ui::exposure_value(0), 50);
    assert_eq!(ui::exposure_value(-500), 0);
    assert_eq!(ui::exposure_value(500), 100);
    assert_eq!(ui::exposure_value(33), 53);
    assert_eq!(ui::exposure_value(35), 54);
    assert_eq!(ui::exposure_at(50), 0);
    assert_eq!(ui::exposure_at(0), -500);
    assert_eq!(ui::exposure_at(100), 500);
    assert_eq!(ui::exposure_at(200), 500);
    assert_eq!(c.slider_value(), 50);
    // A press on the knob's own centre moves nothing; releasing there
    // writes nothing.
    let (x, y) = mid(&c, 50);
    assert_eq!(press(&mut c, x, y), Outcome::Ignored);
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    // A press elsewhere on the track jumps the knob there: a frame change
    // and no write; the drag takes it further, no write either; the
    // release commits, and is a frame change on its own (the knob paints
    // the held value again until the settle) as well as a bump.
    let (x, y) = mid(&c, 70);
    let before = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    let pressed = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y,
        })
        .unwrap();
    assert_eq!(pressed, (Outcome::Changed, vec![]));
    assert_eq!(c.slider_value(), 70);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        before
    );
    let (x, _) = mid(&c, 80);
    let moved = c
        .input(Input::Pointer {
            phase: PointerPhase::Move,
            x,
            y: 300,
        })
        .unwrap();
    assert_eq!(moved, (Outcome::Changed, vec![]));
    assert_eq!(c.slider_value(), 80);
    assert_eq!(drag_to(&mut c, x, 300), Outcome::Ignored);
    let dragged = fields(&c)[GENERATION].clone();
    let (outcome, effects) = release(&mut c, x, 300);
    assert_eq!(outcome, Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], dragged);
    assert_eq!(c.slider_value(), 50);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Exposure,
            value: Some("3.00".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "3.00");
    assert_eq!(c.slider_value(), 80);
    // A drag back to the value in force releases without a write, and the
    // knob returning is a frame change only if the drag had moved it.
    let (x, y) = mid(&c, 60);
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    let (x, _) = mid(&c, 80);
    assert_eq!(drag_to(&mut c, x, y), Outcome::Changed);
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    assert_eq!(c.slider_value(), 80);
    // ... and a release straight back there, with the knob still away,
    // is the frame change that returns it, still without a write.
    let (x, y) = mid(&c, 60);
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    let (x, _) = mid(&c, 80);
    let quiet = fields(&c)[GENERATION].clone();
    assert_eq!(release(&mut c, x, y), (Outcome::Changed, vec![]));
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_eq!(c.slider_value(), 80);
    // The drag owns the pointer over the history pane: a move there
    // follows the column, and a release there commits.
    let (x, y) = mid(&c, 60);
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    assert_eq!(drag_to(&mut c, 100, 300), Outcome::Changed);
    assert_eq!(c.slider_value(), 0);
    let (outcome, effects) = release(&mut c, 100, 300);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Exposure,
            value: Some("-5.00".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "-5.00");
    // And the pane is its own again afterwards: a press on the Undo
    // button asks for the undo, not a slider step; the two commits were
    // one step, so the undo takes both back.
    let undo = c.layout().history_buttons()[2].unwrap().rect();
    let (_, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: (undo.x + i64::from(undo.width) / 2) as u32,
            y: (undo.y + i64::from(undo.height) / 2) as u32,
        })
        .unwrap();
    assert_eq!(
        effects,
        [Effect::Undo {
            index: 0,
            name: file(0),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[EXPOSURE], "-");
    assert_eq!(c.slider_value(), 50);
    // The ends clamp: the slider's last column is the last step, and a
    // drag off the surface's edge stays there.
    let right = (slider.rect().x + i64::from(slider.rect().width) - 1) as u32;
    assert_eq!(press(&mut c, right, y), Outcome::Changed);
    assert_eq!(c.slider_value(), 100);
    assert_eq!(drag_to(&mut c, 799, y), Outcome::Ignored);
    let (_, effects) = release(&mut c, 799, y);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Exposure,
            value: Some("5.00".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    // A press outside the slider is not its drag.
    assert_eq!(press(&mut c, 799, y), Outcome::Ignored);
    // A photo switch drops the drag: the release writes nothing.
    let (x, y) = mid(&c, 40);
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(c.slider_value(), 50);
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    // So does leaving develop, a resize, and the chooser opening.
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.slider_value(), 50);
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    c.input(Input::Resize {
        width: 801,
        height: 600,
        scale: 1,
    })
    .unwrap();
    assert_eq!(c.slider_value(), 50);
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    c.set_listing(b"/".to_vec(), listing("/", &["a"], &[]), None)
        .unwrap();
    assert_eq!(c.slider_value(), 50);
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert!(c.chooser().is_none());
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    // A crop drag in progress owns the pointer over the bands: a move
    // over the slider moves no knob, and the release there is the crop's.
    let r#box = c.develop_box().unwrap();
    assert_eq!(
        press(&mut c, (r#box.x + 40) as u32, (r#box.y + 40) as u32),
        Outcome::Ignored
    );
    assert_eq!(drag_to(&mut c, x, y), Outcome::Changed);
    assert_eq!(c.slider_value(), 50);
    let (outcome, effects) = release(&mut c, x, y);
    assert_eq!(outcome, Outcome::Changed);
    match &effects[..] {
        [Effect::Edit {
            index: 1,
            name,
            key: Key::Crop,
            value: Some(_),
        }] if *name == file(1) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(fields(&c)[EXPOSURE], "-");
    assert_eq!(c.crop_drag(), None);
}

/// The look band: `None` then the available looks, the current one
/// selected; a press sets that look as the `look` action does; `F1`..`F9`
/// pick the first nine, and a key past the list is ignored.
#[test]
fn the_look_band_and_the_f_keys_pick_looks() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(key(&mut c, "F1"), Outcome::Ignored);
    c.set_looks(vec![
        "alpha".to_string(),
        "beta".to_string(),
        "gamma".to_string(),
    ]);
    let buttons =
        c.layout()
            .look_buttons(&["alpha".to_string(), "beta".to_string(), "gamma".to_string()]);
    assert_eq!(buttons.len(), 4);
    assert!(buttons.iter().all(Option::is_some));
    assert_eq!(
        buttons[0].unwrap().rect(),
        Rect {
            x: 224,
            y: 74,
            width: 48,
            height: 20
        }
    );
    assert_eq!(buttons[1].unwrap().rect().x, 224 + 48 + 8);
    let click = |c: &mut Controller, r: Rect| {
        c.input(Input::Pointer {
            phase: PointerPhase::Press,
            x: (r.x + i64::from(r.width) / 2) as u32,
            y: (r.y + i64::from(r.height) / 2) as u32,
        })
        .unwrap()
    };
    let (outcome, effects) = click(&mut c, buttons[2].unwrap().rect());
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Look,
            value: Some("beta".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[LOOK], "beta");
    let (_, effects) = c.input(Input::Key { chord: "F3" }).unwrap();
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Look,
            value: Some("gamma".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[LOOK], "gamma");
    assert_eq!(key(&mut c, "F4"), Outcome::Ignored);
    assert_eq!(key(&mut c, "F9"), Outcome::Ignored);
    let (_, effects) = click(&mut c, buttons[0].unwrap().rect());
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Look,
            value: None,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[LOOK], "-");
    // The band reads back with the looks, and the palette's sub-mode word.
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(
        text.contains("None") && text.contains("alpha") && text.contains("gamma"),
        "{text}"
    );
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert!(c.scene().status_line().ends_with(" | develop looks"));
    // A band too narrow for every look on one row wraps: from 224, None
    // (48 wide) then 128-wide names with 8 between, so the fourth name
    // would end at 816, past the band's 800, and starts the next row at
    // 224; four names a row from then on, six rows for the twenty, and
    // the view moves down under them.
    let long: Vec<String> = (0..20).map(|i| format!("look-number-{i:02}")).collect();
    c.set_looks(long.clone());
    let laid = c.layout().look_buttons(&long);
    assert_eq!(laid.len(), 21);
    assert!(laid.iter().all(Option::is_some));
    assert_eq!(laid[3].unwrap().rect().x + 128, 680);
    assert_eq!(
        laid[4].unwrap().rect(),
        Rect {
            x: 224,
            y: 72 + 24 + 2,
            width: 128,
            height: 20
        }
    );
    assert_eq!(laid[20].unwrap().rect().y, 72 + 5 * 24 + 2);
    assert_eq!(c.layout().look_band().height, 6 * 24);
    assert_eq!(c.layout().develop_view().y, 72 + 6 * 24);
    // A press past the row's last name is the band's chrome; one on the
    // fifth name (the sixth button, second on its row), picks it.
    assert_eq!(press(&mut c, 700, 84), Outcome::Ignored);
    assert_eq!(fields(&c)[LOOK], "-");
    assert_eq!(laid[5].unwrap().rect().x, 360);
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: 366,
            y: 108,
        })
        .unwrap();
    assert_eq!(outcome, Outcome::Changed);
    assert!(matches!(
        &effects[..],
        [Effect::Edit { key: Key::Look, value: Some(stem), .. }] if stem == "look-number-04"
    ));
    // A surface too short for every row keeps the view a name row and a
    // thumbnail-tall box: at 400 by 400 the region is 184 wide, the tool
    // band three rows (the slider on its own) and a look a row, and the
    // look band is cut to the four rows that leave that room; the looks
    // past it are not laid, and the palette (and `F5`) reach them.
    let mut short = Controller::new(surface(400, 400));
    short.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut short, "develop", &[]), Outcome::Changed);
    short.set_looks(long.clone());
    let layout = short.layout();
    assert_eq!(layout.tool_band().height, 3 * 24);
    assert_eq!(layout.look_band().height, 4 * 24);
    assert_eq!(layout.develop_view().height, 400 - 72 - 7 * 24);
    assert!(layout.develop_box().is_some());
    let laid = layout.look_buttons(&long);
    assert_eq!(laid.iter().filter(|b| b.is_some()).count(), 4);
    assert!(laid[4..].iter().all(Option::is_none));
    assert_eq!(act(&mut short, "looks", &[]), Outcome::Changed);
    assert!(short.look_rows().is_some());
    let (_, effects) = short.input(Input::Key { chord: "F5" }).unwrap();
    assert!(matches!(
        &effects[..],
        [Effect::Edit { key: Key::Look, value: Some(stem), .. }] if stem == "look-number-04"
    ));
    // `F5` picks the fifth too: the keys are the list's.
    let (_, effects) = c.input(Input::Key { chord: "F5" }).unwrap();
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Look,
            value: Some("look-number-04".to_string()),
        }]
    );
}

/// The filmstrip under the develop preview: the shown photos around the
/// cursor in thumbnail boxes, the cursor's outlined and kept centred as
/// the ends allow, `Left` and `Right` moving along it and a press on a box
/// selecting its photo; the window blits the strip's thumbnails as it
/// blits the grid's, and wants them around the cursor.
#[test]
fn the_filmstrip_shows_the_shown_photos_under_the_preview() {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    // Nothing in cull: the grid's cells are the visible boxes.
    assert!(c.film().is_empty());
    assert_eq!(c.visible().len(), 5);
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let layout = c.layout();
    // The band is the foot of the region under the bands, a thumbnail and
    // its padding tall; the view above keeps the name row and the box.
    let band = layout.film_band().unwrap();
    assert_eq!(
        band,
        Rect {
            x: 216,
            y: 448,
            width: 584,
            height: 128
        }
    );
    assert_eq!(layout.develop_view().height, 352);
    assert_eq!(ui::FILM_H, 128);
    // Three boxes fit the band whole: from a cell in, a cell between.
    let r#box = |n: i64| Rect {
        x: 224 + n * 168,
        y: 452,
        width: 160,
        height: 120,
    };
    assert_eq!(c.film(), [(0, r#box(0)), (1, r#box(1)), (2, r#box(2))]);
    assert_eq!(c.visible(), c.film());
    // The strip's boxes, then the shown after them, then before.
    assert_eq!(c.wanted(), [0, 1, 2, 3, 4]);
    // The cursor is kept centred once it can be: at 2 the strip shows 1..3,
    // at the end it shows the last three.
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(c.film(), [(1, r#box(0)), (2, r#box(1)), (3, r#box(2))]);
    assert_eq!(c.wanted(), [1, 2, 3, 4, 0]);
    assert_eq!(key(&mut c, "End"), Outcome::Changed);
    assert_eq!(c.film(), [(2, r#box(0)), (3, r#box(1)), (4, r#box(2))]);
    assert_eq!(c.wanted(), [2, 3, 4, 0, 1]);
    // The cursor's box is outlined: the frame moves with the cursor, and
    // the outline sits in the box's padding.
    let end = driven::paint(&c.scene()).unwrap();
    let at = |frame: &driven::Frame, x: i64, y: i64| {
        let i = (y as usize * 800 + x as usize) * 3;
        (frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2])
    };
    let selected = (
        (SELECTED >> 16) as u8,
        (SELECTED >> 8) as u8,
        SELECTED as u8,
    );
    let chrome = ((CHROME >> 16) as u8, (CHROME >> 8) as u8, CHROME as u8);
    let last = r#box(2);
    assert_eq!(at(&end, last.x - 3, last.y - 3), selected);
    assert_eq!(at(&end, last.x - 4, last.y + 10), selected);
    assert_eq!(at(&end, last.x - 6, last.y + 10), chrome);
    let first = r#box(0);
    assert_eq!(at(&end, first.x - 3, first.y - 3), chrome);
    assert_eq!(at(&end, first.x - 4, first.y + 10), chrome);
    // A press on a box selects its photo: a change, the strip recentring
    // on it; on the cursor's own box nothing; on the band's chrome, and a
    // move or release over it, nothing.
    let centre = |r: Rect| ((r.x + 80) as u32, (r.y + 60) as u32);
    let (x, y) = centre(r#box(0));
    let generation = fields(&c)[GENERATION].clone();
    assert_eq!(
        c.input(Input::Pointer {
            phase: PointerPhase::Press,
            x,
            y
        })
        .unwrap(),
        (Outcome::Changed, vec![])
    );
    assert_eq!(fields(&c)[POSITION], "2");
    assert_ne!(fields(&c)[GENERATION], generation);
    assert_eq!(c.film(), [(1, r#box(0)), (2, r#box(1)), (3, r#box(2))]);
    let (x, y) = centre(r#box(1));
    assert_eq!(press(&mut c, x, y), Outcome::Ignored);
    // The gutters, the band's padding above and below the boxes, and the
    // room past the last box.
    assert_eq!(press(&mut c, 220, 460), Outcome::Ignored);
    assert_eq!(press(&mut c, 388, 460), Outcome::Ignored);
    assert_eq!(press(&mut c, 300, 449), Outcome::Ignored);
    assert_eq!(press(&mut c, 300, 574), Outcome::Ignored);
    assert_eq!(press(&mut c, 750, 460), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, x, y), Outcome::Ignored);
    assert_eq!(release(&mut c, x, y), (Outcome::Ignored, vec![]));
    assert_eq!(c.crop_drag(), None);
    // With the look palette open a press on a box still selects, and the
    // palette closes with the photo switch.
    c.set_looks(some_looks());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);
    assert!(c.look_palette().is_some());
    let (x, y) = centre(r#box(2));
    assert_eq!(press(&mut c, x, y), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "3");
    assert!(c.look_palette().is_none());
    assert_eq!(key(&mut c, "Left"), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "2");
    // A crop drag from the preview released over the strip is the crop's:
    // it commits the marquee, and selects nothing.
    let preview = c.develop_box().unwrap();
    assert_eq!(
        press(&mut c, (preview.x + 40) as u32, (preview.y + 40) as u32),
        Outcome::Ignored
    );
    let (x, y) = centre(r#box(2));
    assert_eq!(drag_to(&mut c, x, y), Outcome::Changed);
    let (outcome, effects) = release(&mut c, x, y);
    assert_eq!(outcome, Outcome::Changed);
    // From (308, 160) to (640, 512), the foot clamped to the box's 440:
    // 40/480 and 40/320 in, to 372/480 and 320/320.
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 2,
            name: file(2),
            key: Key::Crop,
            value: Some("0.0833 0.1250 0.6917 0.8750".to_string()),
        }]
    );
    assert_eq!(fields(&c)[POSITION], "2");
    // The badges over the strip are the flags', repainted by the window
    // over the thumbnails it blits there: the fixture's pick (photo 1) in
    // the first box, its reject (photo 2) in the middle one, and the
    // unflagged third bare.
    let badges = driven::paint(&c.badges()).unwrap();
    let mid = r#box(1);
    let reject = (
        (MISSPELLED >> 16) as u8,
        (MISSPELLED >> 8) as u8,
        MISSPELLED as u8,
    );
    assert_eq!(at(&badges, first.x + 1, first.y + 1), selected);
    assert_eq!(at(&badges, mid.x + 1, mid.y + 1), reject);
    assert_ne!(at(&badges, last.x + 1, last.y + 1), selected);
    assert_ne!(at(&badges, last.x + 1, last.y + 1), reject);
    // Unflagging takes a badge with it: the reject's, then the pick's.
    let (_, effects) = c.action("unflag", &[]).unwrap();
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    let badges = driven::paint(&c.badges()).unwrap();
    assert_ne!(at(&badges, mid.x + 1, mid.y + 1), reject);
    assert_eq!(at(&badges, first.x + 1, first.y + 1), selected);
    assert_eq!(key(&mut c, "Left"), Outcome::Changed);
    let (_, effects) = c.action("unflag", &[]).unwrap();
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    let badges = driven::paint(&c.badges()).unwrap();
    assert_ne!(at(&badges, first.x + 1, first.y + 1), selected);
    // A surface too short to keep a name row and a row for the box above
    // the strip lays none: the view keeps the foot, and nothing is wanted.
    // Under the strips, the status row and the two bands, 399 leaves 279
    // for the view: a row short of the strip and the name row and
    // thumbnail-tall box it keeps.
    let mut short = Controller::new(surface(800, 399));
    short.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut short, "develop", &[]), Outcome::Changed);
    assert_eq!(short.layout().film_band(), None);
    assert_eq!(short.layout().develop_view().height, 279);
    assert!(short.film().is_empty() && short.visible().is_empty());
    assert!(short.wanted().is_empty());
    assert_eq!(press(&mut short, 300, 250), Outcome::Ignored);
    // One more row and it is laid, with its boxes.
    let mut tall = Controller::new(surface(800, 400));
    tall.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut tall, "develop", &[]), Outcome::Changed);
    assert_eq!(tall.layout().film_band().map(|b| b.y), Some(248));
    assert_eq!(tall.layout().develop_view().height, 152);
    // The box above is at least a thumbnail tall: 152 less the name row
    // and its padding, 120 high, 180 wide.
    assert_eq!(
        tall.develop_box().map(|b| (b.width, b.height)),
        Some((180, 120))
    );
    let lifted = |n: i64| Rect { y: 252, ..r#box(n) };
    assert_eq!(
        tall.film(),
        [(0, lifted(0)), (1, lifted(1)), (2, lifted(2))]
    );
    // A region narrower than a box and its cells lays no band: the view
    // keeps the foot (under the tool band's three rows here, the buttons
    // wrapping on 175). A cell wider, and it holds its one box.
    let mut narrow = Controller::new(surface(216 + 175, 600));
    narrow.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut narrow, "develop", &[]), Outcome::Changed);
    assert_eq!(narrow.layout().film_band(), None);
    assert_eq!(narrow.layout().tool_band().height, 72);
    assert_eq!(narrow.layout().develop_view().height, 432);
    assert!(narrow.film().is_empty() && narrow.wanted().is_empty());
    let mut one = Controller::new(surface(216 + 176, 600));
    one.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut one, "develop", &[]), Outcome::Changed);
    assert_eq!(act(&mut one, "select", &["3"]), Outcome::Changed);
    assert_eq!(one.film(), [(3, r#box(0))]);
    assert_eq!(one.wanted(), [3, 4, 2]);
    // Fewer shown than boxes: the strip holds them all from the left.
    let mut few = Controller::new(surface(800, 600));
    few.open("roll", b"/r", photos(2)).unwrap();
    assert_eq!(act(&mut few, "develop", &[]), Outcome::Changed);
    assert_eq!(key(&mut few, "Right"), Outcome::Changed);
    assert_eq!(few.film(), [(0, r#box(0)), (1, r#box(1))]);
    assert_eq!(few.wanted(), [0, 1]);
    // At scale two the band, its boxes and their padding double.
    let mut two = Controller::new(surface(800, 600));
    two.open("roll", b"/r", photos(5)).unwrap();
    two.input(Input::Resize {
        width: 1600,
        height: 1200,
        scale: 2,
    })
    .unwrap();
    assert_eq!(act(&mut two, "develop", &[]), Outcome::Changed);
    assert_eq!(
        two.layout().film_band(),
        Some(Rect {
            x: 432,
            y: 896,
            width: 1168,
            height: 256
        })
    );
    let doubled = |n: i64| Rect {
        x: 448 + n * 336,
        y: 904,
        width: 320,
        height: 240,
    };
    assert_eq!(
        two.film(),
        [(0, doubled(0)), (1, doubled(1)), (2, doubled(2))]
    );
    let frame = driven::paint(&two.scene()).unwrap();
    let at2 = |x: i64, y: i64| {
        let i = (y as usize * 1600 + x as usize) * 3;
        (frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2])
    };
    // The outline is four pixels wide in the doubled padding.
    assert_eq!(at2(doubled(0).x - 5, doubled(0).y + 20), selected);
    assert_eq!(at2(doubled(0).x - 8, doubled(0).y + 20), selected);
    assert_eq!(at2(doubled(0).x - 9, doubled(0).y + 20), chrome);
    assert_eq!(at2(doubled(1).x - 5, doubled(1).y + 20), chrome);
    // The chooser over develop hides the strip as it hides the grid.
    c.set_listing(b"/".to_vec(), listing("/", &["a"], &[]), None)
        .unwrap();
    assert!(c.film().is_empty() && c.visible().is_empty());
    // The wants stand under it, as the grid's do: the cursor is at 1.
    assert_eq!(fields(&c)[POSITION], "1");
    assert_eq!(c.wanted(), [0, 1, 2, 3, 4]);
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!(c.film(), [(0, r#box(0)), (1, r#box(1)), (2, r#box(2))]);
    // On a long roll the wants are bounded: the strip's three, the three
    // after, the three before.
    let mut long = Controller::new(surface(800, 600));
    long.open("roll", b"/r", photos(20)).unwrap();
    assert_eq!(act(&mut long, "develop", &[]), Outcome::Changed);
    assert_eq!(act(&mut long, "select", &["10"]), Outcome::Changed);
    assert_eq!(
        long.film().iter().map(|(i, _)| *i).collect::<Vec<_>>(),
        [9, 10, 11]
    );
    assert_eq!(long.wanted(), [9, 10, 11, 12, 13, 14, 6, 7, 8]);
    assert_eq!(act(&mut long, "select", &["0"]), Outcome::Changed);
    assert_eq!(long.wanted(), [0, 1, 2, 3, 4, 5]);
    assert_eq!(act(&mut long, "select", &["19"]), Outcome::Changed);
    assert_eq!(long.wanted(), [17, 18, 19, 14, 15, 16]);
    // An even count of boxes: the cursor's sits right of centre, and the
    // ends clamp as before. 900 wide holds four.
    let mut even = Controller::new(surface(900, 600));
    even.open("roll", b"/r", photos(10)).unwrap();
    assert_eq!(act(&mut even, "develop", &[]), Outcome::Changed);
    let shown = |c: &Controller| c.film().iter().map(|(i, _)| *i).collect::<Vec<_>>();
    assert_eq!(shown(&even), [0, 1, 2, 3]);
    assert_eq!(act(&mut even, "select", &["5"]), Outcome::Changed);
    assert_eq!(shown(&even), [3, 4, 5, 6]);
    assert_eq!(act(&mut even, "select", &["2"]), Outcome::Changed);
    assert_eq!(shown(&even), [0, 1, 2, 3]);
    assert_eq!(act(&mut even, "select", &["8"]), Outcome::Changed);
    assert_eq!(shown(&even), [6, 7, 8, 9]);
}

/// The crop tool in crop-adjust: a press off the crop's rectangle (or
/// inside a crop that is the whole frame) draws a fresh crop over the
/// uncropped frame, shown with its handles as the crop-to-be, and its
/// release commits the marquee's fractions of the whole frame -- an
/// absolute crop, not a sub-region of the current one.
#[test]
fn a_press_off_the_crop_in_crop_adjust_draws_a_fresh_crop() {
    // No crop: the rectangle is the whole canvas, whose interior would move
    // nothing, so a press there draws.
    let mut c = adjusting(&[]);
    let canvas = Rect {
        x: 300,
        y: 100,
        width: 400,
        height: 300,
    };
    assert_eq!(c.crop_adjust_rect(), Some(canvas));
    let armed = fields(&c)[GENERATION].clone();
    let shown = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    // The press is a point: nothing new painted, no bump.
    assert_eq!(press(&mut c, 400, 175), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], armed);
    // The drag rubber-bands the crop-to-be with handles: the sub-mode's
    // rectangle, not the tighten marquee.
    assert_eq!(drag_to(&mut c, 600, 325), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], armed);
    let drawn = Rect {
        x: 400,
        y: 175,
        width: 200,
        height: 150,
    };
    assert_eq!(c.crop_adjust_rect(), Some(drawn));
    assert_eq!(c.crop_drag(), None);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        shown
    );
    // Its overlay is the handled rectangle, as a committed crop of that
    // shape paints: the plain outline the tighten marquee draws at the
    // same rectangle, plus the handle marks (the overlay alone, so the
    // sub-mode's status word and button play no part).
    let handled = driven::paint(&c.marquee()).unwrap();
    let mut plain = develop_at(canvas, &[]);
    assert_eq!(press(&mut plain, 400, 175), Outcome::Ignored);
    assert_eq!(drag_to(&mut plain, 600, 325), Outcome::Changed);
    assert_eq!(plain.crop_drag(), Some(drawn));
    let outline = driven::paint(&plain.marquee()).unwrap();
    assert_ne!(handled.rgb, outline.rgb);
    let at = |frame: &driven::Frame, x: i64, y: i64| {
        let i = (y as usize * 800 + x as usize) * 3;
        (frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2])
    };
    let mark = (
        (SELECTED >> 16) as u8,
        (SELECTED >> 8) as u8,
        SELECTED as u8,
    );
    // The north-west handle mark: three pixels in from the corner, inside
    // the outline's two-pixel edge for the handled rectangle only.
    assert_eq!(at(&handled, 402, 177), mark);
    assert_ne!(at(&outline, 402, 177), mark);
    // The edge itself is drawn by both.
    assert_eq!(at(&handled, 500, 175), mark);
    assert_eq!(at(&outline, 500, 175), mark);
    // The release commits the fractions of the whole frame. The drawn
    // rectangle leaves with the drag and the crop the model holds (the
    // whole frame) returns until the settle: a frame change on its own,
    // so a refused write cannot leave the drawn rectangle on screen.
    let drawing = fields(&c)[GENERATION].clone();
    let (outcome, effects) = release(&mut c, 600, 325);
    assert_eq!(outcome, Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], drawing);
    assert_eq!(c.crop_adjust_rect(), Some(canvas));
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.2500 0.5000 0.5000".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "0.2500 0.2500 0.5000 0.5000");
    assert_eq!(c.crop_adjust_rect(), Some(drawn));
    assert!(c.adjusting());

    // With a crop, a press off its rectangle draws afresh over the whole
    // frame, replacing it rather than tightening it: the new crop's
    // fractions are the canvas's, not the old crop's.
    assert_eq!(press(&mut c, 320, 120), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 260), Outcome::Changed);
    assert_eq!(c.crop_drag(), None);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 320,
            y: 120,
            width: 180,
            height: 140,
        })
    );
    let (outcome, effects) = release(&mut c, 500, 260);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.0500 0.0666 0.4500 0.4667".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    // The settled crop maps back onto the rectangle drawn, to the pixel
    // the crop's floored unit lands on (0.0666 of 300 is 19.98: 119).
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 320,
            y: 119,
            width: 180,
            height: 140,
        })
    );
    // Its handles still grab: the interior moves it.
    assert_eq!(press(&mut c, 410, 190), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 430, 190), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 430, 190);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.1000 0.0666 0.4500 0.4667".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);

    // A click off the crop draws nothing and commits nothing; the overlay
    // stays the crop.
    let held = c.crop_adjust_rect();
    let quiet = fields(&c)[GENERATION].clone();
    assert_eq!(press(&mut c, 650, 380), Outcome::Ignored);
    assert_eq!(release(&mut c, 650, 380), (Outcome::Ignored, vec![]));
    assert_eq!(c.crop_adjust_rect(), held);
    assert_eq!(fields(&c)[GENERATION], quiet);
    // A sub-minimum marquee likewise: the drawn rectangle leaves the frame
    // and the crop's returns, a change, but no write.
    assert_eq!(press(&mut c, 650, 380), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 660, 390), Outcome::Changed);
    assert_eq!(release(&mut c, 660, 390), (Outcome::Changed, vec![]));
    assert_eq!(c.crop_adjust_rect(), held);
    // A marquee dragged over the whole frame clears the crop.
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 799, 599), Outcome::Changed);
    assert_eq!(c.crop_adjust_rect(), Some(canvas));
    let (outcome, effects) = release(&mut c, 799, 599);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: None,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "-");
    // The other way too: the far corner cannot be pressed (`contains`
    // excludes the far edges), but a press within a handle's reach of the
    // canvas's edge anchors on the edge, so the drag reaches the frame's.
    assert_eq!(
        carry(&mut c, "crop", &["0.2500", "0.2500", "0.5000", "0.5000"]),
        Outcome::Changed
    );
    assert_eq!(press(&mut c, 695, 395), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 300, 100), Outcome::Changed);
    assert_eq!(c.crop_adjust_rect(), Some(canvas));
    let (outcome, effects) = release(&mut c, 300, 100);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: None,
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    // A press further in anchors where it is: a marquee to the far edge
    // reaches the crop's far edge exactly, the edges mapped one by one.
    assert_eq!(
        carry(&mut c, "crop", &["0.2500", "0.2500", "0.5000", "0.5000"]),
        Outcome::Changed
    );
    assert_eq!(press(&mut c, 310, 110), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 799, 599), Outcome::Changed);
    let (_, effects) = release(&mut c, 799, 599);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.0250 0.0333 0.9750 0.9667".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    assert_eq!(carry(&mut c, "uncrop", &[]), Outcome::Changed);
    assert_eq!(fields(&c)[CROP], "-");
    // The whole frame's edges and corners are still its handles: a press
    // within reach of the canvas's edge grabs, the drag shrinks the crop
    // by its own travel (97 of 400) rather than drawing one.
    assert_eq!(press(&mut c, 303, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 400, 250), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 400, 250);
    assert_eq!(outcome, Outcome::Changed);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2425 0.0000 0.7575 1.0000".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    // A crop that is the whole frame in one axis still moves in the other
    // from its interior: only the whole frame draws from there.
    assert_eq!(press(&mut c, 500, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 480, 250), Outcome::Changed);
    let (_, effects) = release(&mut c, 480, 250);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.1925 0.0000 0.7575 1.0000".to_string()),
        }]
    );
    assert_eq!(carry(&mut c, "uncrop", &[]), Outcome::Changed);
    // The lock shapes the fresh crop as it shapes the tighten marquee.
    assert_eq!(act(&mut c, "aspect", &["1:1"]), Outcome::Ignored);
    assert_eq!(press(&mut c, 400, 175), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 600, 275), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 400,
            y: 175,
            width: 100,
            height: 100,
        })
    );
    let (_, effects) = release(&mut c, 600, 275);
    assert_eq!(
        effects,
        [Effect::Edit {
            index: 0,
            name: file(0),
            key: Key::Crop,
            value: Some("0.2500 0.2500 0.2500 0.3333".to_string()),
        }]
    );
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    // Leaving the sub-mode drops a drag in progress and applies the crop
    // to the preview: the window develops the cropped frame again.
    assert_eq!(press(&mut c, 650, 380), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 690, 395), Outcome::Changed);
    assert_eq!(key(&mut c, "c"), Outcome::Changed);
    assert!(!c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);
    assert_eq!(c.crop_drag(), None);
    assert_eq!(release(&mut c, 690, 395), (Outcome::Ignored, vec![]));
    assert_eq!(fields(&c)[CROP], "0.2500 0.2500 0.2500 0.3333");
    // A press off the canvas draws nothing in the sub-mode either.
    assert_eq!(key(&mut c, "c"), Outcome::Changed);
    assert_eq!(press(&mut c, 250, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 400, 200), Outcome::Ignored);
    assert_eq!(release(&mut c, 400, 200), (Outcome::Ignored, vec![]));
}

/// The neighbours the window prefetches in develop: the shown photos
/// `PREFETCH_DEPTH` either side of the cursor, next before previous,
/// nearer first, cut at the roll's ends; none in cull, under the chooser
/// or without a cursor.
#[test]
fn the_neighbours_are_the_shown_photos_either_side_of_the_cursor_nearer_first() {
    let mut c = Controller::new(surface(800, 600));
    assert!(c.neighbours().is_empty());
    c.open("roll", b"/r", photos(5)).unwrap();
    assert!(c.neighbours().is_empty(), "cull wants none");
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.neighbours(), [1, 2]);
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(key(&mut c, "Right"), Outcome::Changed);
    assert_eq!(c.neighbours(), [3, 1, 4, 0]);
    assert_eq!(key(&mut c, "End"), Outcome::Changed);
    assert_eq!(c.neighbours(), [3, 2]);
    assert_eq!(ui::PREFETCH_DEPTH, 2);
    // The chooser over develop wants none; closed, the same again.
    let (outcome, _) = c.action("choose", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    assert!(c.neighbours().is_empty());
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert_eq!(c.neighbours(), [3, 2]);
    // Among the shown: the unflagged filter hides the pick and the reject
    // (photos 1 and 2), so the first photo's neighbours are the third and
    // fourth.
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(key(&mut c, "Home"), Outcome::Changed);
    assert_eq!(key(&mut c, "4"), Outcome::Changed);
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(c.neighbours(), [3, 4]);
}

/// Alt held shows each button's chord under its caption: the mode and
/// filter strips', the tool band's, the look band's (`F1` through `F9`
/// for the first nine looks, none for `None`) and the history pane's;
/// released, or the focus leaving, hides them again. The hints are the
/// frame's, not `state`'s, and only Alt shows them.
#[test]
fn alt_held_shows_each_buttons_chord_under_its_caption() {
    use td_ui::keyboard::Held;
    let mut c = with_history();
    c.set_looks(some_looks());
    let alt = Held {
        alt: true,
        ..Held::default()
    };
    let control = Held {
        control: true,
        ..Held::default()
    };
    let plain = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    let marks = |c: &Controller| {
        let mut out = Vec::new();
        c.scene().emit(c.surface().bounds(), &mut |draw| {
            if let Primitive::Mark { x, y, scalar, .. } = draw.primitive {
                out.push((x, y, scalar));
            }
        });
        out
    };
    assert!(marks(&c).is_empty());
    assert!(!c.hints());
    // Control alone shows nothing; Alt (with anything) does.
    assert_eq!(c.input(Input::Held(control)).unwrap().0, Outcome::Ignored);
    assert!(marks(&c).is_empty());
    let before = fields(&c)[GENERATION].clone();
    assert_eq!(c.input(Input::Held(alt)).unwrap().0, Outcome::Changed);
    assert!(c.hints());
    assert_ne!(fields(&c)[GENERATION], before);
    assert_eq!(c.input(Input::Held(alt)).unwrap().0, Outcome::Ignored);
    let hinted = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    assert_ne!(hinted, plain);
    // The text read back is the captions', not the hints'.
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    assert!(!text.contains("Backspace"), "{text}");
    // Each hint sits on its button, under the caption: the chord of the
    // action the button presses, in order along each band.
    let layout = c.layout();
    let under = |button: Option<td_ui::chrome::Button>| {
        let rect = button.unwrap().rect();
        let mut text: Vec<(i64, char)> = marks(&c)
            .into_iter()
            .filter(|(x, y, _)| rect.contains(*x, *y))
            .map(|(x, _, scalar)| (x, scalar))
            .collect();
        text.sort();
        text.into_iter()
            .map(|(_, scalar)| scalar)
            .collect::<String>()
    };
    let tools = layout.tools();
    assert_eq!(tools.buttons.map(under), ["c", "C", "z", "0", "-", "="]);
    let looks: Vec<String> = layout
        .look_buttons(&some_looks())
        .into_iter()
        .map(under)
        .collect();
    assert_eq!(looks, ["", "F1", "F2", "F3"]);
    assert_eq!(layout.history_buttons().map(under), ["t", "Backspace", "z"]);
    // The strips' hints read in the strips' order, a row each.
    let mut all = marks(&c);
    all.sort();
    let row: String = all
        .iter()
        .filter(|(_, y, _)| *y < 24)
        .map(|(_, _, scalar)| *scalar)
        .collect();
    assert_eq!(row, "oEscaped");
    let row: String = all
        .iter()
        .filter(|(_, y, _)| (24..48).contains(y))
        .map(|(_, _, scalar)| *scalar)
        .collect();
    assert_eq!(row, "1234");
    // Released, the hints go; the frame is the plain one again.
    assert_eq!(
        c.input(Input::Held(Held::default())).unwrap().0,
        Outcome::Changed
    );
    assert!(marks(&c).is_empty());
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        plain
    );
    // The focus leaving hides them too, once.
    assert_eq!(c.input(Input::Held(alt)).unwrap().0, Outcome::Changed);
    assert_eq!(c.input(Input::Focus(false)).unwrap().0, Outcome::Changed);
    assert_eq!(c.input(Input::Focus(false)).unwrap().0, Outcome::Ignored);
    assert!(!c.hints() && marks(&c).is_empty());
    // The chooser owns the keyboard while it is open, so no chord is
    // shown under a button, the hints kept for when it closes.
    assert_eq!(c.input(Input::Held(alt)).unwrap().0, Outcome::Changed);
    let (outcome, _) = c.action("choose", &[]).unwrap();
    assert_eq!(outcome, Outcome::Changed);
    c.set_listing(b"/".to_vec(), listing("/", &["r"], &[]), Some("r"))
        .unwrap();
    assert!(c.hints() && marks(&c).is_empty());
    assert_eq!(key(&mut c, "Escape"), Outcome::Changed);
    assert!(c.chooser().is_none() && !marks(&c).is_empty());
    assert_eq!(
        c.input(Input::Held(Held::default())).unwrap().0,
        Outcome::Changed
    );
    // In the cull grid the strips still hint, and the develop bands are
    // not there to.
    assert_eq!(act(&mut c, "grid", &[]), Outcome::Changed);
    assert_eq!(c.input(Input::Held(alt)).unwrap().0, Outcome::Changed);
    let mut all = marks(&c);
    all.sort_by_key(|(x, y, _)| (*y, *x));
    let text: String = all.iter().map(|(_, _, scalar)| *scalar).collect();
    assert_eq!(text, "oEscaped1234");
}
