#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The semantic seam driven end to end by a toy consumer: a counter with a
//! four-row action table, a two-line composition and one verb of its own.
//! Everything a real consumer relies on is exercised here without a display:
//! the table's grammar, every generic verb and its refusals, the text read
//! back from the glyph stream, the frame digest and its pages, and the
//! whole thing behind the replay runner.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use td_ui::control::{self, frame, hex, ErrorCode};
use td_ui::control_socket::Socket;
use td_ui::control_worker::{Job, Worker};
use td_ui::driven::{self, Binding, Controller, Input, Outcome, Payload, PointerPhase};
use td_ui::keyboard::Held;
use td_ui::raster::{
    Composition, Draw, GlyphStyle, Primitive, Rect, Scale, Surface, Weight, INK, PAPER,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Error {
    Wire(control::Error),
    Busy,
    Range,
    /// A shape the router let through that the toy does not know: it
    /// marks exactly what reached the consumer.
    Unknown,
}

impl From<control::Error> for Error {
    fn from(error: control::Error) -> Self {
        Self::Wire(error)
    }
}

impl ErrorCode for Error {
    fn code(&self) -> &'static str {
        match self {
            Self::Wire(error) => error.code(),
            Self::Busy => "busy",
            Self::Range => "range",
            Self::Unknown => "unknown",
        }
    }
}

const BINDINGS: &[Binding] = &[
    Binding {
        name: "increment",
        chord: Some("Up"),
        arguments: "",
        help: "Count one up.",
    },
    Binding {
        name: "decrement",
        chord: Some("Down"),
        arguments: "",
        help: "Count one down.",
    },
    Binding {
        name: "add",
        chord: None,
        arguments: "N",
        help: "Add N to the count, |N| at most 1000.",
    },
    Binding {
        name: "quit",
        chord: Some("q"),
        arguments: "",
        help: "Leave.",
    },
];

struct Counter {
    count: i64,
    held: Held,
    focused: bool,
    surface: Surface,
    pointer: Option<(u32, u32)>,
    ticks: u64,
    busy: bool,
    cover: Option<Rect>,
}

impl Counter {
    fn new() -> Self {
        Self {
            count: 0,
            held: Held::default(),
            focused: false,
            surface: Surface::new(160, 48, Scale::new(1).unwrap()).unwrap(),
            pointer: None,
            ticks: 0,
            busy: false,
            cover: None,
        }
    }

    fn glyphs(&self, damage: Rect, row: i64, column: i64, text: &str, sink: &mut dyn FnMut(Draw)) {
        let scale = self.surface.scale.value() as i64;
        for (index, scalar) in text.chars().enumerate() {
            sink(Draw {
                clip: damage,
                primitive: Primitive::Glyph {
                    x: (column + index as i64) * 8 * scale,
                    y: row * 16 * scale,
                    scalar,
                    style: GlyphStyle {
                        ink: INK,
                        background: PAPER,
                        weight: Weight::Regular,
                    },
                },
            });
        }
    }
}

impl Composition for Counter {
    fn surface(&self) -> Surface {
        self.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        sink(Draw {
            clip: damage,
            primitive: Primitive::Fill {
                rect: self.surface.bounds(),
                color: PAPER,
            },
        });
        self.glyphs(damage, 0, 0, &format!("count={}", self.count), sink);
        // A mark is a hint's pixel, not text: `text` reads through it to
        // the count's first cell.
        sink(Draw {
            clip: damage,
            primitive: Primitive::Mark {
                x: 0,
                y: 0,
                scalar: 'M',
                ink: PAPER,
            },
        });
        // A glyph whose clip excludes it entirely is not shown, so `text`
        // must not report it.
        let hidden = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };
        self.glyphs(hidden, 1, 0, "X", sink);
        // Later draws over earlier ones in a cell: the first letter of the
        // focus word is overpainted with a marker.
        let word = if self.focused { "focused" } else { "blurred" };
        self.glyphs(damage, 2, 1, word, sink);
        self.glyphs(damage, 2, 1, "*", sink);
        // An opaque panel over the text, when a test places one.
        if let Some(rect) = self.cover {
            sink(Draw {
                clip: damage,
                primitive: Primitive::Fill { rect, color: PAPER },
            });
        }
    }
}

impl Controller for Counter {
    type Error = Error;

    fn bindings(&self) -> &'static [Binding] {
        BINDINGS
    }

    fn action(&mut self, name: &str, arguments: &[&str]) -> Result<Outcome, Error> {
        if self.busy {
            return Err(Error::Busy);
        }
        match (name, arguments) {
            ("increment", []) => {
                self.count += 1;
                Ok(Outcome::Changed)
            }
            ("decrement", []) => {
                self.count -= 1;
                Ok(Outcome::Changed)
            }
            ("add", [amount]) => {
                let amount: i64 = amount.parse().map_err(|_| control::Error::Protocol)?;
                if amount.abs() > 1000 {
                    return Err(Error::Range);
                }
                self.count += amount;
                Ok(Outcome::Changed)
            }
            ("quit", []) => Ok(Outcome::Quit),
            _ => Err(Error::Unknown),
        }
    }

    fn input(&mut self, input: Input<'_>) -> Result<Outcome, Error> {
        match input {
            Input::Key { chord } => match driven::bound(BINDINGS, chord) {
                Some(binding) => self.action(binding.name, &[]),
                None => Ok(Outcome::Ignored),
            },
            Input::Held(held) => {
                self.held = held;
                Ok(Outcome::Changed)
            }
            Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
            } => {
                self.pointer = Some((x, y));
                Ok(Outcome::Changed)
            }
            Input::Pointer { .. } => Ok(Outcome::Ignored),
            Input::Wheel { rows, .. } => {
                self.count += i64::from(rows);
                Ok(Outcome::Changed)
            }
            Input::Resize {
                width,
                height,
                scale,
            } => {
                let scale = Scale::new(scale).map_err(|_| control::Error::Protocol)?;
                self.surface =
                    Surface::new(width, height, scale).map_err(|_| control::Error::Limit)?;
                Ok(Outcome::Changed)
            }
            Input::Focus(focused) => {
                self.focused = focused;
                Ok(Outcome::Changed)
            }
            Input::Tick(now) => {
                self.ticks = now;
                Ok(Outcome::Ignored)
            }
        }
    }

    fn state(&self) -> Result<String, Error> {
        Ok(format!(
            "count={}\tfocus={}\tpointer={}\tticks={}",
            self.count,
            u8::from(self.focused),
            self.pointer
                .map_or("-".to_string(), |(x, y)| format!("{x},{y}")),
            self.ticks
        ))
    }

    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, Error> {
        Ok(view(self))
    }

    fn request(&mut self, name: &str, arguments: &[&str]) -> Result<String, Error> {
        match (name, arguments) {
            ("double", []) => {
                self.count *= 2;
                Ok(self.count.to_string())
            }
            _ => Err(Error::Unknown),
        }
    }
}

fn ask(counter: &mut Counter, line: &str) -> String {
    driven::request(counter, line.as_bytes())
}

fn refusal(id: u64, code: &str) -> String {
    format!("1\t{id}\terror\t{code}\t{}", hex(code.as_bytes()))
}

#[test]
fn the_table_is_held_to_its_grammar_and_printed_aligned() {
    driven::check(BINDINGS).unwrap();
    let row = |name, chord, arguments, help| Binding {
        name,
        chord,
        arguments,
        help,
    };
    for (table, message) in [
        (
            vec![row("Up", None, "", "x")],
            "action name `Up` is outside the code grammar",
        ),
        (vec![row("a", None, "", "")], "action `a` has no help line"),
        (
            vec![row("a", None, "N\tM", "x")],
            "action `a` arguments is not printable ASCII",
        ),
        (
            vec![row("a", None, "", "caf\u{e9}")],
            "action `a` help is not printable ASCII",
        ),
        (
            vec![row("a", Some(""), "", "x")],
            "action `a` chord `` is outside the key bound",
        ),
        (
            vec![row("a", None, "", "x"), row("a", None, "", "y")],
            "action name `a` is bound twice",
        ),
        (
            vec![row("a", Some("q"), "", "x"), row("b", Some("q"), "", "y")],
            "chord `q` binds both `a` and `b`",
        ),
    ] {
        assert_eq!(driven::check(&table).unwrap_err(), message);
    }
    // Two unbound actions do not collide on the absent chord.
    driven::check(&[row("a", None, "", "x"), row("b", None, "", "y")]).unwrap();
    // Columns are padded in characters, so a multi-byte chord lines up.
    assert_eq!(
        driven::help(&[
            row("a", Some("\u{2192}"), "", "x"),
            row("bb", Some("Up"), "", "y")
        ]),
        "a   \u{2192}   -  x\nbb  Up  -  y\n"
    );
    assert_eq!(driven::bound(BINDINGS, "Up").unwrap().name, "increment");
    assert!(driven::bound(BINDINGS, "Left").is_none());
    assert_eq!(
        driven::help(BINDINGS),
        concat!(
            "increment  Up    -  Count one up.\n",
            "decrement  Down  -  Count one down.\n",
            "add        -     N  Add N to the count, |N| at most 1000.\n",
            "quit       q     -  Leave.\n",
        )
    );
}

#[test]
fn every_generic_verb_routes_through_the_controller_and_refuses_in_the_envelope() {
    let mut c = Counter::new();
    assert_eq!(
        ask(&mut c, "1\t1\tstate"),
        "1\t1\tok\tcount=0\tfocus=0\tpointer=-\tticks=0"
    );
    assert_eq!(ask(&mut c, "1\t2\taction\tincrement"), "1\t2\tok\tchanged");
    assert_eq!(ask(&mut c, "1\t3\taction\tadd\t5"), "1\t3\tok\tchanged");
    assert_eq!(ask(&mut c, "1\t4\taction\tadd\t5000"), refusal(4, "range"));
    assert_eq!(ask(&mut c, "1\t5\taction\tadd\tx"), refusal(5, "protocol"));
    assert_eq!(ask(&mut c, "1\t6\taction\tnope"), refusal(6, "protocol"));
    assert_eq!(
        ask(&mut c, "1\t7\taction\tincrement\textra"),
        refusal(7, "unknown")
    );
    assert_eq!(ask(&mut c, "1\t8\taction"), refusal(8, "protocol"));
    assert_eq!(ask(&mut c, "1\t9\taction\tquit"), "1\t9\tok\tquit");
    assert_eq!(c.count, 6);
    // Keys go through the consumer's own binding lookup.
    assert_eq!(
        ask(&mut c, &format!("1\t10\tkey\t{}", hex(b"Down"))),
        "1\t10\tok\tchanged"
    );
    assert_eq!(
        ask(&mut c, &format!("1\t11\tkey\t{}", hex(b"x"))),
        "1\t11\tok\tignored"
    );
    for bad in [
        "-",
        &hex(b"a\nb"),
        &hex(&[b'k'; 33]),
        &hex(&[0xff]),
        "zz",
        &"zz".repeat(driven::KEY_BYTES + 1),
    ] {
        assert_eq!(
            ask(&mut c, &format!("1\t12\tkey\t{bad}")),
            refusal(12, "protocol"),
            "{bad}"
        );
    }
    assert_eq!(
        ask(&mut c, "1\t13\tpointer\tpress\t3\t4"),
        "1\t13\tok\tchanged"
    );
    assert_eq!(
        ask(&mut c, "1\t14\tpointer\tmove\t5\t6"),
        "1\t14\tok\tignored"
    );
    assert_eq!(
        ask(&mut c, "1\t15\tpointer\tdrag\t5\t6"),
        refusal(15, "protocol")
    );
    assert_eq!(
        ask(&mut c, "1\t16\tpointer\tpress\t-1\t6"),
        refusal(16, "protocol")
    );
    assert_eq!(
        ask(&mut c, "1\t17\tpointer\tpress\t4294967296\t6"),
        refusal(17, "protocol")
    );
    assert_eq!(ask(&mut c, "1\t18\twheel\t-2\t0"), "1\t18\tok\tchanged");
    assert_eq!(c.count, 3);
    assert_eq!(
        ask(&mut c, "1\t19\twheel\t16777216\t-16777216"),
        "1\t19\tok\tchanged"
    );
    assert_eq!(
        ask(&mut c, "1\t20\twheel\t-16777216\t16777216"),
        "1\t20\tok\tchanged"
    );
    assert_eq!(c.count, 3);
    assert_eq!(
        ask(&mut c, "1\t21\twheel\t16777217\t0"),
        refusal(21, "protocol")
    );
    assert_eq!(
        ask(&mut c, "1\t22\twheel\t0\t-16777217"),
        refusal(22, "protocol")
    );
    assert_eq!(ask(&mut c, "1\t23\twheel\t+1\t0"), refusal(23, "protocol"));
    assert_eq!(
        ask(&mut c, "1\t24\tresize\t320\t96\t2"),
        "1\t24\tok\tchanged"
    );
    assert_eq!(
        c.surface,
        Surface::new(320, 96, Scale::new(2).unwrap()).unwrap()
    );
    assert_eq!(
        ask(&mut c, "1\t25\tresize\t0\t96\t1"),
        refusal(25, "protocol")
    );
    assert_eq!(
        ask(&mut c, "1\t26\tresize\t9000\t96\t1"),
        refusal(26, "protocol")
    );
    assert_eq!(
        ask(&mut c, "1\t27\tresize\t8192\t8192\t1"),
        refusal(27, "limit")
    );
    assert_eq!(
        ask(&mut c, "1\t28\tresize\t320\t96\t5"),
        refusal(28, "protocol")
    );
    assert_eq!(ask(&mut c, "1\t29\tfocus\t1"), "1\t29\tok\tchanged");
    // The held roles as a chord prefix, in the prefix's own order only.
    assert_eq!(ask(&mut c, "1\t29\theld\tM-"), "1\t29\tok\tchanged");
    assert_eq!(
        c.held,
        Held {
            alt: true,
            ..Held::default()
        }
    );
    assert_eq!(ask(&mut c, "1\t29\theld\tC-M-S-"), "1\t29\tok\tchanged");
    assert_eq!(
        c.held,
        Held {
            control: true,
            alt: true,
            shift: true
        }
    );
    assert_eq!(ask(&mut c, "1\t29\theld\t-"), "1\t29\tok\tchanged");
    assert_eq!(c.held, Held::default());
    for bad in ["M-C-", "M", "", "C--", "X-"] {
        assert_eq!(
            ask(&mut c, &format!("1\t29\theld\t{bad}")),
            refusal(29, "protocol"),
            "{bad}"
        );
    }
    assert_eq!(ask(&mut c, "1\t30\tfocus\t2"), refusal(30, "protocol"));
    assert_eq!(ask(&mut c, "1\t31\ttick\t500"), "1\t31\tok\tignored");
    assert_eq!(
        ask(&mut c, "1\t32\tstate"),
        "1\t32\tok\tcount=3\tfocus=1\tpointer=3,4\tticks=500"
    );
    // The consumer's own verb, and the closed set around it.
    assert_eq!(ask(&mut c, "1\t33\tdouble"), "1\t33\tok\t6");
    assert_eq!(ask(&mut c, "1\t34\tdouble\t2"), refusal(34, "unknown"));
    assert_eq!(ask(&mut c, "1\t35\ttriple"), refusal(35, "unknown"));
    // A generic verb with fields it does not take is the router's
    // refusal; nothing reaches the toy, which would have said `unknown`.
    assert_eq!(ask(&mut c, "1\t36\tstate\textra"), refusal(36, "protocol"));
    for bad in [
        "actions\t1",
        "key",
        "held",
        "held\tM-\t1",
        "pointer\tpress\t1",
        "wheel\t1",
        "resize\t320\t96",
        "focus",
        "tick",
        "text\t1",
        "frame\t1",
        "frame-page\t0",
    ] {
        assert_eq!(
            ask(&mut c, &format!("1\t36\t{bad}")),
            refusal(36, "protocol"),
            "{bad}"
        );
    }
    assert_eq!(
        ask(&mut c, "1\t37\taction\tadd\t1\t2\t3\t4\t5\t6\t7\t8"),
        refusal(37, "protocol")
    );
    assert_eq!(
        ask(&mut c, "1\t38\taction\tadd\t1\t2\t3\t4\t5\t6\t7"),
        refusal(38, "unknown")
    );
    assert_eq!(ask(&mut c, "2\t39\tstate"), refusal(0, "protocol"));
    assert_eq!(ask(&mut c, "1\t40\tstáte"), refusal(0, "protocol"));
    // A consumer refusal keeps its own code; the transport's two map through.
    c.busy = true;
    assert_eq!(ask(&mut c, "1\t41\taction\tincrement"), refusal(41, "busy"));
    assert_eq!(
        ask(&mut c, &format!("1\t42\tkey\t{}", hex(b"Up"))),
        refusal(42, "busy")
    );
    c.busy = false;
    assert_eq!(
        ask(&mut c, "1\t43\tactions"),
        format!(
            "1\t43\tok\t4\tincrement\t{}\t-\t{}\tdecrement\t{}\t-\t{}\tadd\t-\t{}\t{}\tquit\t{}\t-\t{}",
            hex(b"Up"),
            hex(b"Count one up."),
            hex(b"Down"),
            hex(b"Count one down."),
            hex(b"N"),
            hex(b"Add N to the count, |N| at most 1000."),
            hex(b"q"),
            hex(b"Leave.")
        )
    );
}

#[test]
fn text_reads_the_glyph_grid_and_frames_are_digested_and_paged() {
    let mut c = Counter::new();
    assert_eq!(
        driven::text(&c).unwrap(),
        (3, 20, "count=0\n\n *lurred".to_string())
    );
    c.focused = true;
    c.count = 42;
    assert_eq!(driven::text(&c).unwrap().2, "count=42\n\n *ocused");
    assert_eq!(
        ask(&mut c, "1\t1\ttext"),
        format!("1\t1\tok\t3\t20\t{}", hex(b"count=42\n\n *ocused"))
    );
    // Scale doubles every cell; the grid is the same.
    c.surface = Surface::new(320, 96, Scale::new(2).unwrap()).unwrap();
    assert_eq!(
        driven::text(&c).unwrap(),
        (3, 20, "count=42\n\n *ocused".into())
    );
    // A surface smaller than the text clips it to the cells that exist.
    c.surface = Surface::new(40, 20, Scale::new(1).unwrap()).unwrap();
    assert_eq!(driven::text(&c).unwrap(), (1, 5, "count".into()));
    // Narrower than one cell: no columns, nothing to read.
    c.surface = Surface::new(4, 16, Scale::new(1).unwrap()).unwrap();
    assert_eq!(driven::text(&c).unwrap(), (1, 0, String::new()));
    // ROWS is the grid's height; the text has at most that many lines.
    c.surface = Surface::new(160, 160, Scale::new(1).unwrap()).unwrap();
    assert_eq!(
        driven::text(&c).unwrap(),
        (10, 20, "count=42\n\n *ocused".to_string())
    );
    c.surface = Surface::new(160, 48, Scale::new(1).unwrap()).unwrap();
    // An opaque fill hides the cells it wholly covers: the whole third
    // row, then exactly the marker's cell; a partial cover hides nothing.
    c.cover = Some(Rect {
        x: 0,
        y: 32,
        width: 160,
        height: 16,
    });
    assert_eq!(driven::text(&c).unwrap().2, "count=42");
    c.cover = Some(Rect {
        x: 8,
        y: 32,
        width: 8,
        height: 16,
    });
    assert_eq!(driven::text(&c).unwrap().2, "count=42\n\n  ocused");
    c.cover = Some(Rect {
        x: 8,
        y: 32,
        width: 4,
        height: 16,
    });
    assert_eq!(driven::text(&c).unwrap().2, "count=42\n\n *ocused");
    c.cover = None;

    let frame = driven::paint(&c).unwrap();
    assert_eq!(frame.rgb.len(), 160 * 48 * 3);
    assert!(frame.ppm().starts_with(b"P6\n160 48\n255\n"));
    assert_eq!(
        frame.ppm().len(),
        b"P6\n160 48\n255\n".len() + frame.rgb.len()
    );
    let digest = format!("{:016x}", driven::fnv1a64(&frame.rgb));
    assert_eq!(
        ask(&mut c, "1\t2\tframe"),
        format!("1\t2\tok\t160\t48\t1\t{digest}")
    );
    // The same state paints the same frame; a change paints another.
    assert_eq!(
        ask(&mut c, "1\t3\tframe"),
        format!("1\t3\tok\t160\t48\t1\t{digest}")
    );
    c.count = 43;
    assert_ne!(
        ask(&mut c, "1\t4\tframe"),
        format!("1\t4\tok\t160\t48\t1\t{digest}")
    );
    c.count = 42;
    // Pages reassemble the body exactly, the last one short.
    let mut body = Vec::new();
    let mut offset = 0;
    loop {
        let reply = ask(&mut c, &format!("1\t5\tframe-page\t{offset}\t10000"));
        let fields: Vec<&str> = reply.split('\t').collect();
        assert_eq!(
            &fields[..6],
            &["1", "5", "ok", "160", "48", &offset.to_string()]
        );
        let page = control::unhex(fields[6]).unwrap();
        if page.is_empty() {
            break;
        }
        body.extend_from_slice(&page);
        offset += page.len();
    }
    assert_eq!(body, frame.rgb);
    assert_eq!(
        ask(&mut c, &format!("1\t6\tframe-page\t{}\t1", frame.rgb.len())),
        format!("1\t6\tok\t160\t48\t{}\t-", frame.rgb.len())
    );
    assert_eq!(
        ask(
            &mut c,
            &format!("1\t7\tframe-page\t{}\t1", frame.rgb.len() + 1)
        ),
        refusal(7, "protocol")
    );
    assert_eq!(
        ask(
            &mut c,
            &format!("1\t8\tframe-page\t0\t{}", driven::PAGE_BYTES + 1)
        ),
        refusal(8, "limit")
    );
    assert_eq!(ask(&mut c, "1\t9\tframe-page\t0"), refusal(9, "protocol"));
    assert_eq!(
        ask(&mut c, "1\t9\tframe-page\t0\t0"),
        refusal(9, "protocol")
    );
    // The largest page, hex-doubled, still frames under the ceiling.
    c.surface = Surface::new(640, 160, Scale::new(1).unwrap()).unwrap();
    let reply = ask(
        &mut c,
        &format!("1\t10\tframe-page\t0\t{}", driven::PAGE_BYTES),
    );
    assert!(reply.starts_with("1\t10\tok\t640\t160\t0\t"));
    let page = control::unhex(reply.rsplit('\t').next().unwrap()).unwrap();
    assert_eq!(page.len(), driven::PAGE_BYTES);
    assert!(control::frame(reply.as_bytes()).is_ok());
}

#[test]
fn the_seam_runs_behind_the_replay_runner() {
    let mut c = Counter::new();
    let mut stream = Vec::new();
    for request in [
        &b"1\t1\taction\tadd\t7"[..],
        b"1\t2\tstate",
        b"1\t3\tnope",
        b"1\t4\ttext",
    ] {
        stream.extend_from_slice(&frame(request).unwrap());
    }
    let mut output = Vec::new();
    td_ui::replay::run(&mut stream.as_slice(), &mut output, |bytes| {
        driven::request(&mut c, bytes)
    })
    .unwrap();
    let mut replies = Vec::new();
    let mut rest = output.as_slice();
    while !rest.is_empty() {
        let length = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        replies.push(String::from_utf8(rest[4..4 + length].to_vec()).unwrap());
        rest = &rest[4 + length..];
    }
    assert_eq!(
        replies,
        [
            "1\t1\tok\tchanged".to_string(),
            "1\t2\tok\tcount=7\tfocus=0\tpointer=-\tticks=0".to_string(),
            refusal(3, "unknown"),
            format!("1\t4\tok\t3\t20\t{}", hex(b"count=7\n\n *lurred")),
        ]
    );
}

/// A private directory for a socket, as a window's runtime directory is.
struct Directory(std::path::PathBuf);

impl Directory {
    fn new() -> Self {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tdui-driven-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn next_job(worker: &Worker<Payload>) -> Job<Payload> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(job) = worker.try_request().unwrap() {
            return job;
        }
        assert!(Instant::now() < until, "worker request timeout");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// One request on its own connection, as the worker serves them.
fn send(path: &std::path::Path, request: &[u8]) -> UnixStream {
    let mut peer = UnixStream::connect(path).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.write_all(&frame(request).unwrap()).unwrap();
    peer
}

fn reply(peer: &mut UnixStream) -> String {
    let mut length = [0u8; 4];
    peer.read_exact(&mut length).unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    peer.read_exact(&mut payload).unwrap();
    String::from_utf8(payload).unwrap()
}

#[test]
fn a_payload_is_judged_on_the_worker_thread_and_answered_on_the_turn() {
    use td_ui::control::Parse;
    // The envelope and the field count are refused before any turn.
    for (bad, id) in [
        ("2\t1\tstate", 0),
        ("1\tx\tstate", 0),
        ("1\t5", 5),
        ("1\t6\taction\tadd\t1\t2\t3\t4\t5\t6\t7\t8", 6),
    ] {
        let refused = Payload::parse(bad.as_bytes()).err().unwrap();
        assert_eq!(refused.response(), refusal(id, "protocol"), "{bad}");
    }
    let mut c = Counter::new();
    let payload = Payload::parse(b"1\t7\taction\tadd\t3").ok().unwrap();
    assert_eq!(payload.bytes(), b"1\t7\taction\tadd\t3");
    assert_eq!(
        driven::request(&mut c, payload.bytes()),
        "1\t7\tok\tchanged"
    );

    // The wiring a window uses: the worker parses, the turn answers.
    let dir = Directory::new();
    let path = dir.0.join("control");
    let worker = Worker::<Payload>::start(Socket::bind(&path).unwrap()).unwrap();
    let mut peer = send(&path, b"1\t8\taction\tincrement");
    let job = next_job(&worker);
    assert!(job
        .respond_with(|payload| driven::request(&mut c, payload.bytes()))
        .unwrap());
    assert_eq!(reply(&mut peer), "1\t8\tok\tchanged");
    // A generic verb's arity is the turn's refusal over the socket too.
    let mut peer = send(&path, b"1\t9\tstate\textra");
    let job = next_job(&worker);
    assert!(job
        .respond_with(|payload| driven::request(&mut c, payload.bytes()))
        .unwrap());
    assert_eq!(reply(&mut peer), refusal(9, "protocol"));
    // A malformed envelope never becomes a job.
    let mut peer = send(&path, b"2\t10\tstate");
    assert_eq!(reply(&mut peer), refusal(0, "protocol"));
    assert!(worker.try_request().unwrap().is_none());
    let mut peer = send(&path, b"1\t11\tstate");
    let job = next_job(&worker);
    assert_eq!(job.request().bytes(), b"1\t11\tstate");
    assert!(job
        .respond_with(|payload| driven::request(&mut c, payload.bytes()))
        .unwrap());
    assert_eq!(
        reply(&mut peer),
        "1\t11\tok\tcount=4\tfocus=0\tpointer=-\tticks=0"
    );
    worker.close().unwrap();
    assert!(!path.exists());
}
