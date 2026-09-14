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

use td_photo::library::{Filter, Flag, Key, Sidecar};
use td_photo::ui::{self, Action, Controller, Effect, Photo, View, BINDINGS};
use td_ui::control::{frame, hex, valid_code, Decoder, ErrorCode};
use td_ui::driven::{self, Input, Outcome, PointerPhase};
use td_ui::raster::{Scale, Surface};

const MODE: usize = 0;
const SHOWN: usize = 3;
const POSITION: usize = 4;
const VIEW: usize = 6;
const NAME: usize = 7;
const FLAG: usize = 8;
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
                    ["open", "select", "scroll"].contains(&binding.name),
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

/// What the adapter does with flag effects when the file holds what the
/// model does: refuses a sidecar it could not read, calls the flag the
/// file already holds ignored, else sets it and settles the photo.
fn apply(c: &mut Controller, effects: &[Effect]) -> Result<Outcome, ui::Error> {
    let mut outcome = Outcome::Changed;
    for effect in effects {
        let Effect::Flag { index, name, flag } = effect else {
            panic!("{effect:?}");
        };
        let photo = &c.photos()[*index];
        if photo.error.is_some() {
            return Err(ui::Error::Refused);
        }
        let mut sidecar = photo.sidecar.clone().unwrap_or_default();
        let held = sidecar.flag() == *flag;
        sidecar.set(Key::Flag, flag.map(Flag::word)).unwrap();
        let changed = c.settle(
            *index,
            Photo {
                name: name.clone(),
                sidecar: Some(sidecar),
                error: None,
            },
        );
        outcome = if held && !changed {
            Outcome::Ignored
        } else {
            Outcome::Changed
        };
    }
    Ok(outcome)
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
    assert_eq!(&reply(2)[..2], ["ok", "21"]);
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
