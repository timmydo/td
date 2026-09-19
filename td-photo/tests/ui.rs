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
use td_photo::ui::{self, Action, Controller, Effect, Photo, View, BINDINGS};
use td_ui::control::{frame, hex, valid_code, Decoder, ErrorCode};
use td_ui::driven::{self, Input, Outcome, PointerPhase};
use td_ui::raster::{Rect, Scale, Surface};
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
                    ["open", "select", "scroll", "look", "crop", "aspect"].contains(&binding.name),
                    "{} has no key",
                    binding.name
                );
                assert!(!binding.arguments.is_empty());
            }
            Some(_) => assert!(binding.arguments.is_empty(), "{}", binding.name),
        }
    }
    assert_eq!(Action::parse("colour"), None);
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
        "cull\t-\t0\t0\t-\tall\tgrid\t-\t-\t-\t-\t-\t-\t0\t0"
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

    // The pointer: a bar header sets the filter, a cell selects, the rest
    // is ignored, and only a press does anything.
    assert_eq!(press(&mut c, 80, 10), Outcome::Changed);
    assert_eq!(c.filter(), Filter::Picks);
    assert_eq!(press(&mut c, 80, 10), Outcome::Ignored);
    assert_eq!(press(&mut c, 10, 10), Outcome::Changed);
    assert_eq!(c.filter(), Filter::All);
    assert_eq!(act(&mut c, "first", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 300, 100), Outcome::Changed);
    assert_eq!(fields(&c)[POSITION], "1");
    assert_eq!(press(&mut c, 300, 100), Outcome::Ignored);
    assert_eq!(press(&mut c, 300, 290), Outcome::Ignored);
    // Off the 400x300 surface nothing is a target, not even the bar's row.
    assert_eq!(press(&mut c, 400, 10), Outcome::Ignored);
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
    // On a surface too short for both bands the status row paints over the
    // bar, so a press there is the status row's, never a header's.
    let mut short = Controller::new(surface(400, 40));
    short.open("roll", b"/r", photos(2)).unwrap();
    assert_eq!(press(&mut short, 80, 20), Outcome::Ignored);
    assert_eq!(short.filter(), Filter::All);
    assert_eq!(press(&mut short, 80, 10), Outcome::Changed);
    assert_eq!(short.filter(), Filter::Picks);
    // In the single view a press anywhere in the area between the bands
    // returns to the grid, the picture's margin as much as the picture;
    // the status row does not.
    assert_eq!(act(&mut c, "view", &[]), Outcome::Changed);
    assert_eq!(press(&mut c, 300, 290), Outcome::Ignored);
    assert_eq!(c.view(), View::Single);
    assert_eq!(press(&mut c, 20, 30), Outcome::Changed);
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
            | Effect::Export { index, name } => (*index, name.clone()),
            Effect::Open(_) => panic!("{effect:?}"),
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
            Effect::Open(_) | Effect::Export { .. } => panic!("{effect:?}"),
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
        Some("td-photo edit 1\nfuture 1\nexposure -0.33\nflag pick\n")
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
    assert_eq!(lines[0].trim(), "[All]    Picks     Rejects     Unflagged");
    let bar = lines[0].to_string();
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
        lines[9].contains("DSC_0000.NEF") && lines[9].contains("DSC_0003.NEF"),
        "{text}"
    );
    assert!(lines[19].contains("DSC_0004.NEF") && lines[19].contains("DSC_0005.NEF"));
    assert_eq!(lines[2].trim(), "P                     X", "{}", lines[2]);
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
        text.lines().nth(2).unwrap().trim(),
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
    assert_eq!(lines[0].trim(), "All     Picks    [Rejects]    Unflagged");
    // The headers keep their places whichever filter is active.
    for filter in ["picks", "unflagged", "all", "rejects"] {
        act(&mut c, filter, &[]);
        let (_, _, text) = driven::text(&c.scene()).unwrap();
        let row = text.lines().next().unwrap().to_string();
        for header in ["All", "Picks", "Rejects", "Unflagged"] {
            assert_eq!(row.find(header), bar.find(header), "{filter} {header}");
        }
    }
    assert!(lines[9].contains("DSC_0002.NEF") && !lines[9].contains("DSC_0000.NEF"));
    assert_eq!(
        lines.last().unwrap().trim(),
        "roll | 6 photos, 1 shown | rejects | 1/1 DSC_0002.NEF reject"
    );
    // The single view: the name, the facts and the status say so.
    act(&mut c, "view", &[]);
    let (_, _, text) = driven::text(&c.scene()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1].trim(), "DSC_0002.NEF");
    assert_eq!(
        lines[2].trim(),
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
            "1"
        ]
    );
    assert_eq!(&reply(2)[..2], ["ok", "33"]);
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
        "td-photo edit 1\nexposure -0.50\nflag reject\n"
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
    // Four exposure steps, a look and a crop, each a change; the value is
    // the file's, read back in the state.
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
        "td-photo edit 1\nexposure 0.10\nlook portra\ncrop 0.1000 0.1000 0.5000 0.5000\n"
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
    // The box sits under the bar and the cell's padding; the fifth cell
    // begins the second row, the second the second column.
    let first = Rect {
        x: 8,
        y: 32,
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
            y: 32 + 152,
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
    let short = Surface::new(400, 60, Scale::default()).unwrap();
    let mut c = Controller::new(short);
    c.open("roll", b"/r", photos(3)).unwrap();
    let area = c.layout().area;
    assert_eq!((area.y, area.height), (24, 12));
    assert_eq!(c.visible()[1].0, 1, "the pick is on the one row");
    let mut pixels = vec![0u8; big_stride * 60];
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
    assert_eq!(c.develop_box(), c.layout().preview_box());
    assert!(c.develop_box().is_some());

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
        x: 100,
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
    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 200,
            y: 150,
            width: 0,
            height: 0,
        })
    );
    assert_eq!(fields(&c)[GENERATION], quiet);

    // A move rubber-bands the marquee: the frame changes, so a bump.
    assert_eq!(drag_to(&mut c, 400, 300), Outcome::Changed);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 200,
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
    let (outcome, effects) = release(&mut c, 400, 300);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(canvas));

    // A marquee over the canvas selects a sub-region of the *current* crop,
    // not the whole image. From the canvas top-left, 200x150 of 400x300 is
    // the top-left quarter-area; composed with the current crop (w=h=0.5000)
    // that leaves x=0.2500, y=0.1666 and shrinks w=h to 0.2500 -- a subset.
    assert_eq!(press(&mut c, 100, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 300, 250), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 300, 250);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(canvas));

    // A press off the canvas arms no drag: the far edges are exclusive.
    assert_eq!(press(&mut c, 50, 50), Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);
    assert_eq!(press(&mut c, 500, 200), Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // A move or release with no drag armed is inert.
    assert_eq!(drag_to(&mut c, 300, 250), Outcome::Ignored);
    assert_eq!(release(&mut c, 300, 250).0, Outcome::Ignored);
    assert_eq!(c.crop_drag(), None);

    // A plain click -- press and release with no move -- is a zero-size
    // marquee: nothing selected, no effect, no change.
    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    let (outcome, effects) = release(&mut c, 200, 150);
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(c.crop_drag(), None);

    // A marquee that maps under the minimum edge selects nothing, but a
    // visible marquee that then vanishes is still a frame change: Changed
    // with no effect. 12px of 400 is 0.0300, under the 0.0500 minimum edge.
    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 212, 165), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 212, 165);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));

    // The scene paints the develop placeholder; the marquee is witnessed by
    // the frame, not `state`, so it lands in the seam's own paint (the replay
    // `frame` and the `--preview` PPM), not just the model.
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // Arming a zero-size marquee draws nothing: the frame is unchanged.
    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

    // Rubber-banding it outlines a rectangle over the preview: the frame
    // differs.
    assert_eq!(drag_to(&mut c, 400, 300), Outcome::Changed);
    let marked = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    assert_ne!(marked, bare);

    // The release takes the drag (its crop is an effect the adapter settles,
    // not the scene): the marquee is gone and the frame is the bare one again.
    let (_, effects) = release(&mut c, 400, 300);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // A press then a purely horizontal move: the marquee has zero height, so
    // both painters suppress it. It is invisible, so no frame change -- the
    // move is Ignored, the generation holds, and the frame is the bare one,
    // even though the raw marquee (which crop_drag reports) did change.
    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    let quiet = fields(&c)[GENERATION].clone();
    assert_eq!(drag_to(&mut c, 350, 150), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], quiet);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 200,
            y: 150,
            width: 150,
            height: 0,
        })
    );

    // Growing the height makes it visible: now a frame change.
    assert_eq!(drag_to(&mut c, 350, 250), Outcome::Changed);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));

    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 400, 300), Outcome::Changed);
    let visible = fields(&c)[GENERATION].clone();
    let marked = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // A second press (the replay/socket vocabulary permits one during a drag)
    // arms a fresh zero-size marquee: the visible outline left the frame, so
    // that is a change even though the new marquee is itself invisible.
    assert_eq!(press(&mut c, 250, 200), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], visible);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        marked
    );
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 250,
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));

    // A drag over the whole canvas composes to exactly the full crop; commit
    // it so the sidecar holds it.
    assert_eq!(press(&mut c, 100, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 400), Outcome::Changed);
    let (_, effects) = release(&mut c, 500, 400);
    assert_eq!(apply(&mut c, &effects).unwrap(), Outcome::Changed);
    let crop = fields(&c)[CROP].clone();
    assert_ne!(crop, "-");

    // Drag the whole canvas again: the composed crop equals the one already in
    // the sidecar, so settling it changes nothing. The marquee still left the
    // frame, so the release must move the generation on its own -- otherwise
    // the outline would stay painted until an unrelated change.
    assert_eq!(press(&mut c, 100, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 400), Outcome::Changed);
    let visible = fields(&c)[GENERATION].clone();
    let (outcome, effects) = release(&mut c, 500, 400);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));

    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 400, 300), Outcome::Changed);

    // A new fit arrives mid-drag (an in-flight develop lands at a different
    // size): the gesture keeps mapping against the canvas it started on, not
    // the new one, so the committed crop cannot escape the current crop.
    c.set_preview_fit(Some(Rect {
        x: 200,
        y: 100,
        width: 200,
        height: 300,
    }));
    let (_, effects) = release(&mut c, 400, 300);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));

    // The replay/socket adapter can name a rubber-band by its two corners: a
    // press at one, a release at the other with no move between. The release
    // uses its own point, so the marquee spans the two and commits a crop.
    assert_eq!(press(&mut c, 200, 150), Outcome::Ignored);
    let (outcome, effects) = release(&mut c, 400, 300);
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

/// Enters develop, sets the given crop and the shared 400x300 canvas, and turns
/// on crop-adjust. The centred-half crop maps to Rect{200,175,200,150}.
fn adjusting(crop: &[&str]) -> Controller {
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    if !crop.is_empty() {
        assert_eq!(carry(&mut c, "crop", crop), Outcome::Changed);
    }
    c.set_preview_fit(Some(Rect {
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    }));
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
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

    // In develop it toggles the sub-mode; the overlay appears and leaves, and
    // the tighten marquee is not this mode's.
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(c.adjusting());
    assert!(c.crop_adjust_rect().is_some());
    assert_eq!(c.crop_drag(), None);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert!(!c.adjusting());
    assert_eq!(c.crop_adjust_rect(), None);

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
            x: 100,
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
            x: 200,
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
    assert_eq!(press(&mut c, 200, 175), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 100, 100), Outcome::Changed);
    let (outcome, effects) = release(&mut c, 100, 100);
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
    assert_eq!(press(&mut c, 300, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 320, 260), Outcome::Changed);
    let (_, effects) = release(&mut c, 320, 260);
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
    assert_eq!(press(&mut c, 400, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 210, 250), Outcome::Changed);
    let (_, effects) = release(&mut c, 210, 250);
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
    assert_eq!(press(&mut c, 300, 250), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 250), Outcome::Changed);
    let (_, effects) = release(&mut c, 500, 250);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };

    // set_preview_fit is a fact, like the job count: no generation bump.
    let quiet = fields(&c)[GENERATION].clone();
    c.set_preview_fit(Some(canvas));
    assert_eq!(fields(&c)[GENERATION], quiet);
    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // Entering crop-adjust shows the overlay: a frame change, digest differs.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], quiet);
    assert_ne!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

    // Grabbing a handle paints the rectangle already shown: no bump. Moving it
    // does bump.
    let armed = fields(&c)[GENERATION].clone();
    assert_eq!(press(&mut c, 100, 100), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], armed);
    assert_eq!(drag_to(&mut c, 200, 175), Outcome::Changed);
    assert_ne!(fields(&c)[GENERATION], armed);

    // Leaving crop-adjust removes the overlay: back to the bare frame.
    let _ = release(&mut c, 200, 175);
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
        x: 100,
        y: 100,
        width: 400,
        height: 300,
    };
    c.set_preview_fit(Some(canvas));

    // A tighten marquee spanning the whole canvas (no crop yet).
    assert_eq!(press(&mut c, 100, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 500, 400), Outcome::Changed);
    assert_eq!(c.crop_drag(), Some(canvas));
    let marquee_gen = fields(&c)[GENERATION].clone();
    let marquee_digest = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);

    // Entering crop-adjust shows the SAME rectangle but drawn with handle marks:
    // the rectangle does not move, yet the painted frame differs, so the toggle
    // is a change and the generation bumps -- the overlay's kind is part of its
    // identity, not only its rectangle.
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
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

    // set_looks is a fact, like the job count and the preview fit: no bump.
    c.set_looks(some_looks());
    assert_eq!(fields(&c)[GENERATION], quiet);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );

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

    // The rows the palette paints, top-down over the develop box: the top pad,
    // then one CELL_HEIGHT-high row each. `at(i)` is the middle of row `i`.
    let r#box = c.develop_box().unwrap();
    let pad = ui::CELL_PAD as i64;
    let row = CELL_HEIGHT as i64;
    let at = |index: i64| Input::Pointer {
        phase: PointerPhase::Press,
        x: (r#box.x + pad + 2) as u32,
        y: (r#box.y + pad + row * index + row / 2) as u32,
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
            x: r#box.x as u32,
            y: (r#box.y + pad + row / 2) as u32,
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

#[test]
fn the_look_palette_pick_needs_a_develop_box() {
    // On a surface too small for a develop box the palette opens as a sub-mode
    // but paints nothing, so a press picks nothing: no row exists to hit.
    let mut c = Controller::new(surface(400, 40));
    c.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
    assert!(c.look_palette().is_some());
    assert_eq!(c.develop_box(), None);

    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: 10,
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

    let r#box = c.develop_box().unwrap();
    let pad = ui::CELL_PAD as i64;
    let row = CELL_HEIGHT as i64;
    // Row 0 is the `-` look; the edit sets it (value `Some("-")`), not `None`.
    let (outcome, effects) = c
        .input(Input::Pointer {
            phase: PointerPhase::Press,
            x: (r#box.x + pad + 2) as u32,
            y: (r#box.y + pad + row / 2) as u32,
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
    let mut c = Controller::new(surface(800, 600));
    c.open("roll", b"/r", photos(5)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    let r#box = c.develop_box().unwrap();
    let pad = ui::CELL_PAD as i64;
    let row = CELL_HEIGHT as i64;
    let width = (i64::from(r#box.width) - 2 * pad).max(0);
    // Enough looks that a row falls past the box bottom.
    let fit = (i64::from(r#box.height) - pad) / row;
    assert!(fit >= 2);
    let looks: Vec<String> = (0..fit + 2).map(|i| format!("look-{i}")).collect();
    c.set_looks(looks);
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Changed);

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
    // The top padding, the left padding, the right padding past the names, and
    // a row clipped by the box bottom are none of them a name: a press picks
    // nothing.
    inert(press(&mut c, r#box.x + pad + 2, r#box.y + pad - 1));
    inert(press(&mut c, r#box.x, r#box.y + pad + row / 2));
    inert(press(
        &mut c,
        r#box.x + pad + width,
        r#box.y + pad + row / 2,
    ));
    inert(press(
        &mut c,
        r#box.x + pad + 2,
        r#box.y + pad + row * fit + row / 2,
    ));
    // But the last fully-shown row (index fit - 1) picks.
    let (outcome, effects) = press(
        &mut c,
        r#box.x + pad + 2,
        r#box.y + pad + row * (fit - 1) + row / 2,
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
fn the_look_palette_is_not_witnessed_without_a_develop_box() {
    // A surface too small for a develop box: the sub-mode can open, but with
    // no box nothing paints the palette, so its toggle is not a frame change.
    // The witness gates the generation on the box, as the paint does.
    let mut c = Controller::new(surface(400, 40));
    c.open("roll", b"/r", photos(1)).unwrap();
    assert_eq!(act(&mut c, "develop", &[]), Outcome::Changed);
    c.set_looks(some_looks());
    assert_eq!(c.develop_box(), None);

    let bare = driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb);
    let quiet = fields(&c)[GENERATION].clone();
    // Opening the sub-mode paints nothing over a boxless surface: Ignored, no
    // bump, and the frame does not move.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
    assert!(c.look_palette().is_some());
    assert_eq!(fields(&c)[GENERATION], quiet);
    assert_eq!(
        driven::fnv1a64(&driven::paint(&c.scene()).unwrap().rgb),
        bare
    );
    // Closing it again is likewise no frame change.
    assert_eq!(act(&mut c, "looks", &[]), Outcome::Ignored);
    assert_eq!(fields(&c)[GENERATION], quiet);
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

#[test]
fn a_locked_corner_drag_maps_the_ratio_in_pixel_space() {
    // A 4:3-pixel canvas: a 3:2-pixel lock is a 9:8 box in fractions, so this
    // proves the ratio is applied in the canvas's pixel space, not in fractions.
    // The crop starts 4:3 in pixels (200x150), not the locked 3:2.
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 300,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    // Picking a ratio only arms the lock; it never reshapes the crop on its own
    // (an immediate snap could read a stale cropped fit as the whole image's
    // aspect and commit a wrong-ratio crop). Reshaping flows through the drag.
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Grabbing the south-east corner paints the crop already shown, not a
    // reshaped ratio box: a zero-delta grab keeps the free path.
    assert_eq!(press(&mut c, 300, 225), Outcome::Ignored);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 100,
            y: 75,
            width: 200,
            height: 150,
        })
    );
    // Dragging the corner out holds 3:2 in pixels: 300x200 on screen, a 9:8
    // box in the crop's fractions; the live overlay is already the committed
    // box.
    assert_eq!(drag_to(&mut c, 400, 300), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 100,
            y: 75,
            width: 300,
            height: 200,
        })
    );
    let (outcome, effects) = release(&mut c, 400, 300);
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
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &[],
    );
    assert_eq!(act(&mut c, "aspect", &["1:1"]), Outcome::Ignored);
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    let full = Rect {
        x: 0,
        y: 0,
        width: 600,
        height: 400,
    };
    assert_eq!(c.crop_adjust_rect(), Some(full));
    let crop_before = fields(&c)[CROP].clone();
    // The grab does not reshape the full-frame overlay to a square.
    assert_eq!(press(&mut c, 0, 0), Outcome::Ignored);
    assert_eq!(c.crop_adjust_rect(), Some(full));
    // Releasing without moving commits nothing and leaves the crop untouched.
    let (outcome, effects) = release(&mut c, 0, 0);
    assert_eq!(outcome, Outcome::Ignored);
    assert!(effects.is_empty());
    assert_eq!(fields(&c)[CROP], crop_before);
}

#[test]
fn a_corner_drag_holds_the_locked_ratio() {
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.0000", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    // Arming the lock paints nothing; the crop starts 3:2 in pixels (300x200)
    // and a locked drag holds that ratio.
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Drag the south-east corner out to the canvas: the box grows holding 3:2,
    // capped by the canvas edge; the live overlay is already the committed box.
    assert_eq!(press(&mut c, 450, 200), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 600, 400), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 150,
            y: 0,
            width: 450,
            height: 300,
        })
    );
    let (outcome, effects) = release(&mut c, 600, 400);
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
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Drag the east edge inward: the width shrinks and the height follows to
    // hold 3:2, centred on the crop's old horizontal midline.
    assert_eq!(press(&mut c, 450, 200), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 390, 200), Outcome::Changed);
    let (_, effects) = release(&mut c, 390, 200);
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
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 400,
        },
        &[],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    // Arming the lock paints nothing (the full image is already 1:1 here).
    assert_eq!(act(&mut c, "aspect", &["1:1"]), Outcome::Ignored);
    // Drag the north-west corner in: the box stays square in pixels.
    assert_eq!(press(&mut c, 0, 0), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 100, 100), Outcome::Changed);
    assert_eq!(
        c.crop_adjust_rect(),
        Some(Rect {
            x: 100,
            y: 100,
            width: 300,
            height: 300,
        })
    );
    let (_, effects) = release(&mut c, 100, 100);
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
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Back to free: an east-edge drag now changes the width alone, leaving the
    // height at 0.5000 (the pre-lock free behaviour).
    assert_eq!(act(&mut c, "aspect", &["free"]), Outcome::Ignored);
    assert_eq!(press(&mut c, 450, 200), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 390, 200), Outcome::Changed);
    let (_, effects) = release(&mut c, 390, 200);
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
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &["0.2500", "0.2500", "0.5000", "0.5000"],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    // Drag the east edge far in: both edges pin to the minimum (0.0500) while
    // holding 3:2, never past it.
    assert_eq!(press(&mut c, 450, 200), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 100, 200), Outcome::Changed);
    let (_, effects) = release(&mut c, 100, 200);
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
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &[],
    );
    // Not in crop-adjust: the lock arms the tighten marquee too, and survives
    // the sub-mode boundary.
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    assert_eq!(press(&mut c, 0, 0), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 600, 300), Outcome::Changed);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 0,
            y: 0,
            width: 450,
            height: 300,
        })
    );
    let (_, effects) = release(&mut c, 600, 300);
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
    let mut c = develop_at(
        Rect {
            x: 0,
            y: 0,
            width: 600,
            height: 400,
        },
        &[],
    );
    assert_eq!(act(&mut c, "adjust-crop", &[]), Outcome::Changed);
    assert_eq!(act(&mut c, "aspect", &["3:2"]), Outcome::Ignored);
    assert!(c.adjusting());
    // Moving to another photo ends the sub-mode and the lock.
    assert_eq!(act(&mut c, "next", &[]), Outcome::Changed);
    assert!(!c.adjusting());
    // A fresh tighten marquee is unconstrained: a square drag stays square, so
    // the 3:2 lock did not survive the switch.
    assert_eq!(press(&mut c, 100, 100), Outcome::Ignored);
    assert_eq!(drag_to(&mut c, 400, 400), Outcome::Changed);
    assert_eq!(
        c.crop_drag(),
        Some(Rect {
            x: 100,
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
