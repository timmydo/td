//! The window through td-ui's driven seam (td-ui/DESIGN.md, "The
//! semantic seam"; DESIGN.md §8, "Driving"): the window's own key and
//! pointer paths, its facts as tab-separated fields, and what it shows as
//! a composition, one paint of it recorded and replayed, so an agent or a
//! test operates the window it would see.

use std::cell::RefCell;

use td_alloc::{Budget, BudgetVec};
use td_ui::control::{self, Error, ErrorCode};
use td_ui::driven::{Binding, Controller, Input, Outcome, PointerPhase};
use td_ui::picture::Fitted;
use td_ui::raster::{Composition, Draw, Primitive, Rect, Scale, Surface};
use td_ui::window::{self, Flow, Handler, NoClipboard};

use super::{App, Canvas, Focus, PictureFit, PAGE_PICTURE};
use crate::document::Body;
use td_page::paint::MAX_PAINT;

/// The most draws one recorded frame holds: the page's `MAX_PAINT` and
/// the address and status rows', each a glyph a cell at most.
pub const MAX_RECORDED: usize = MAX_PAINT + 65_536;
/// A recording's first capacity, doubled as it fills.
const FIRST_RECORDED: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Protocol,
    Limit,
}

impl From<Error> for Refusal {
    fn from(error: Error) -> Self {
        match error {
            Error::Protocol => Self::Protocol,
            Error::Limit => Self::Limit,
        }
    }
}

impl ErrorCode for Refusal {
    fn code(&self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::Limit => "limit",
        }
    }
}

/// The window's actions, each delivered as its chord through the key path
/// but `open`, which goes to an address as the address row's `Return`
/// does. A chord means what it means where the focus is (DESIGN.md §8).
pub const BINDINGS: &[Binding] = &[
    Binding {
        name: "open",
        chord: None,
        arguments: "HEX_ADDRESS",
        help: "Go to an address, as the address row's Return takes it: a text with no scheme is taken as https.",
    },
    Binding {
        name: "address",
        chord: Some("C-l"),
        arguments: "",
        help: "Focus the address row, its text selected; type it as keys, Return goes, Escape returns to the page.",
    },
    Binding {
        name: "back",
        chord: Some("M-Left"),
        arguments: "",
        help: "Go back a page in the history.",
    },
    Binding {
        name: "forward",
        chord: Some("M-Right"),
        arguments: "",
        help: "Go forward a page in the history.",
    },
    Binding {
        name: "reload",
        chord: Some("C-r"),
        arguments: "",
        help: "Fetch the page again.",
    },
    Binding {
        name: "stop",
        chord: Some("Escape"),
        arguments: "",
        help: "On the page: stop loading, else the pictures to come, else a save fetching, else clear the focused link, find's marks and the selection.",
    },
    Binding {
        name: "next-link",
        chord: Some("Tab"),
        arguments: "",
        help: "On the page: focus the next link, clearing the selection.",
    },
    Binding {
        name: "previous-link",
        chord: Some("S-Tab"),
        arguments: "",
        help: "On the page: focus the previous link, clearing the selection.",
    },
    Binding {
        name: "follow",
        chord: Some("Return"),
        arguments: "",
        help: "On the page: follow the focused link. In the address row: go to its text.",
    },
    Binding {
        name: "find",
        chord: Some("C-f"),
        arguments: "",
        help: "Find in the page: type the query as keys, each match found as it is typed and the current one selected; Return and S-Return step, Escape returns to the page.",
    },
    Binding {
        name: "page-down",
        chord: Some("PageDown"),
        arguments: "",
        help: "On the page: scroll down a pane.",
    },
    Binding {
        name: "page-up",
        chord: Some("PageUp"),
        arguments: "",
        help: "On the page: scroll up a pane.",
    },
    Binding {
        name: "top",
        chord: Some("Home"),
        arguments: "",
        help: "On the page: scroll to the top.",
    },
    Binding {
        name: "bottom",
        chord: Some("End"),
        arguments: "",
        help: "On the page: scroll to the bottom.",
    },
    Binding {
        name: "select-all",
        chord: Some("C-a"),
        arguments: "",
        help: "On the page: select the page.",
    },
    Binding {
        name: "copy",
        chord: Some("C-c"),
        arguments: "",
        help: "Copy the selection, else the focused link's address, else the page's; the seam has no clipboard, so the status row says the copy was refused.",
    },
    Binding {
        name: "save",
        chord: Some("C-s"),
        arguments: "",
        help: "Save the document into the Downloads the human granted td-web.",
    },
    Binding {
        name: "body-face",
        chord: Some("F8"),
        arguments: "",
        help: "Body text in Noto Sans or Noto Serif, for this run.",
    },
    Binding {
        name: "page-styles",
        chord: Some("F9"),
        arguments: "",
        help: "The page without its own styles, or with them.",
    },
    Binding {
        name: "close",
        chord: Some("C-q"),
        arguments: "",
        help: "Close the window.",
    },
];

/// The window as the seam drives it. A copy has no press to be offered
/// at, so the seam's inputs reach the clipboard as no clipboard.
pub struct Remote<'a> {
    /// Borrowed mutably by `compose` alone, to record a paint, and only
    /// while no other borrow is out.
    app: RefCell<&'a mut App>,
}

impl<'a> Remote<'a> {
    pub fn new(app: &'a mut App) -> Self {
        Self {
            app: RefCell::new(app),
        }
    }

    /// `input` through the window's own path. The key list is opened
    /// from the live keyboard and pointer only (td-ui/DESIGN.md, "Key
    /// list"), so a `?` from the seam leaves its edge as it was.
    fn deliver(&mut self, input: window::Input<'_>) -> Outcome {
        let app = self.app.get_mut();
        let was = std::mem::replace(&mut app.redraw, false);
        let keys = app.show_keys;
        let flow = app.input(input, &mut NoClipboard);
        app.show_keys = keys;
        outcome(app, was, flow)
    }

    /// Where the pointer is with no button held, or that a button is
    /// held, as the window tells its handler after each pointer event.
    fn moved(&mut self, at: Option<(u32, u32)>) -> Outcome {
        let app = self.app.get_mut();
        let was = std::mem::replace(&mut app.redraw, false);
        app.motion(at.map(|(x, y)| (i64::from(x), i64::from(y))));
        let flow = app.flow();
        outcome(app, was, flow)
    }
}

/// Two dispatches' outcomes as one: `quit` over `changed` over `ignored`.
fn either(a: Outcome, b: Outcome) -> Outcome {
    match (a, b) {
        (Outcome::Quit, _) | (_, Outcome::Quit) => Outcome::Quit,
        (Outcome::Changed, _) | (_, Outcome::Changed) => Outcome::Changed,
        _ => Outcome::Ignored,
    }
}

/// What a dispatch did: the window asked to paint again, or to close.
fn outcome(app: &mut App, was: bool, flow: Flow) -> Outcome {
    let changed = app.redraw;
    app.redraw |= was;
    match flow {
        Flow::Quit => Outcome::Quit,
        _ if changed => Outcome::Changed,
        _ => Outcome::Ignored,
    }
}

impl Controller for Remote<'_> {
    type Error = Refusal;

    fn bindings(&self) -> &'static [Binding] {
        BINDINGS
    }

    fn action(&mut self, name: &str, arguments: &[&str]) -> Result<Outcome, Refusal> {
        if name == "open" {
            let [address] = arguments else {
                return Err(Refusal::Protocol);
            };
            let bytes = control::unhex(address)?;
            let address = std::str::from_utf8(&bytes).map_err(|_| Refusal::Protocol)?;
            let app = self.app.get_mut();
            let was = std::mem::replace(&mut app.redraw, false);
            app.message = None;
            app.open(address);
            app.redraw = true;
            let flow = app.flow();
            return Ok(outcome(app, was, flow));
        }
        if !arguments.is_empty() {
            return Err(Refusal::Protocol);
        }
        let chord = BINDINGS
            .iter()
            .find(|binding| binding.name == name)
            .and_then(|binding| binding.chord)
            .ok_or(Refusal::Protocol)?;
        Ok(self.deliver(window::Input::Key {
            chord,
            repeat: false,
        }))
    }

    fn input(&mut self, input: Input<'_>) -> Result<Outcome, Refusal> {
        let pointer = |phase, x: u32, y: u32| window::Input::Pointer {
            phase,
            x: i64::from(x),
            y: i64::from(y),
            extend: false,
            follow: false,
        };
        Ok(match input {
            Input::Key { chord } => self.deliver(window::Input::Key {
                chord,
                repeat: false,
            }),
            // As td-ui's window: a press holds the button and tells the
            // handler the pointer is no longer hovering; a move while it
            // is held is the drag's, else where the pointer is; a release
            // lets it go and tells where the pointer is. A press while
            // one is held, and a release with none, the window drops.
            Input::Pointer {
                phase: PointerPhase::Press,
                ..
            } if self.app.get_mut().driven_press => Outcome::Ignored,
            Input::Pointer {
                phase: PointerPhase::Release,
                ..
            } if !self.app.get_mut().driven_press => Outcome::Ignored,
            Input::Pointer {
                phase: PointerPhase::Press,
                x,
                y,
            } => {
                self.app.get_mut().driven_press = true;
                let pressed = self.deliver(pointer(window::PointerPhase::Press, x, y));
                either(pressed, self.moved(None))
            }
            Input::Pointer {
                phase: PointerPhase::Move,
                x,
                y,
            } => {
                if self.app.get_mut().driven_press {
                    self.deliver(pointer(window::PointerPhase::Move, x, y))
                } else {
                    self.moved(Some((x, y)))
                }
            }
            Input::Pointer {
                phase: PointerPhase::Release,
                x,
                y,
            } => {
                self.app.get_mut().driven_press = false;
                let released = self.deliver(pointer(window::PointerPhase::Release, x, y));
                either(released, self.moved(Some((x, y))))
            }
            Input::Wheel { rows, columns } => self.deliver(window::Input::Wheel {
                rows: rows as isize,
                columns: columns as isize,
            }),
            Input::Resize {
                width,
                height,
                scale,
            } => {
                let scale = Scale::new(scale).map_err(|_| Refusal::Protocol)?;
                let surface = Surface::new(width, height, scale).map_err(|_| Refusal::Limit)?;
                self.deliver(window::Input::Resize(surface))
            }
            Input::Focus(focused) => self.deliver(window::Input::Focus(focused)),
            Input::Tick(now) => {
                let app = self.app.get_mut();
                let was = std::mem::replace(&mut app.redraw, false);
                let flow = app.poll(now);
                outcome(app, was, flow)
            }
            // No hints to show: the roles held are nothing to it.
            Input::Held(_) => Outcome::Ignored,
        })
    }

    fn state(&self) -> Result<String, Refusal> {
        let app = self.app.try_borrow().map_err(|_| Refusal::Limit)?;
        facts(&app).map_err(|_| Refusal::Limit)
    }

    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, Refusal> {
        let draws = {
            let mut app = self.app.try_borrow_mut().map_err(|_| Refusal::Limit)?;
            let mut recording = Recording::new(app.renderer.budget())?;
            let surface = app.surface;
            app.frame(surface, &mut recording);
            recording.finish()?
        };
        let app = self.app.try_borrow().map_err(|_| Refusal::Limit)?;
        Ok(view(&Recorded {
            app: &app,
            draws: &draws,
        }))
    }
}

/// `state`'s body: `KEY=VALUE` fields, a tab between, a value's tabs,
/// line ends and other controls each a space.
fn facts(app: &App) -> Result<String, td_alloc::Error> {
    let mut out = String::new();
    let doc = &app.current.doc;
    let url = doc.url.as_ref().map(|url| url.text());
    fact(&mut out, "url", url.as_deref().unwrap_or("none"))?;
    fact(&mut out, "title", &app.title)?;
    let focus = match app.focus {
        Focus::Address => "address",
        Focus::Page => "page",
        Focus::Find => "find",
    };
    fact(&mut out, "focus", focus)?;
    fact(&mut out, "address", app.address.text())?;
    let loading = match &app.loading {
        Some(loading) if loading.styling => "styles",
        Some(_) => "page",
        None if app.current.pictures.as_ref().is_some_and(|p| p.busy()) => "pictures",
        None => "none",
    };
    fact(&mut out, "loading", loading)?;
    let body = match &doc.body {
        Body::Blank => "blank",
        Body::Page(_) => "page",
        Body::Picture(_) => "picture",
        Body::Resources => "refused",
    };
    fact(&mut out, "body", body)?;
    fact(&mut out, "status", &count(doc.status)?)?;
    fact(&mut out, "back", &count(app.back.len())?)?;
    fact(&mut out, "forward", &count(app.forward.len())?)?;
    let view = &app.current.view;
    let link = view.focus.and_then(|link| app.link_shown(link));
    fact(&mut out, "link", link.as_deref().unwrap_or("none"))?;
    let hover = app.hover.and_then(|link| app.link_shown(link));
    fact(&mut out, "hover", hover.as_deref().unwrap_or("none"))?;
    let selection = view.selection.and_then(|s| s.range()).is_some();
    fact(
        &mut out,
        "selection",
        if selection { "some" } else { "none" },
    )?;
    fact(&mut out, "query", app.query.text())?;
    fact(&mut out, "matches", &count(view.hits().len())?)?;
    fact(
        &mut out,
        "styles",
        if app.current.styles { "page" } else { "none" },
    )?;
    fact(&mut out, "face", if app.serif { "serif" } else { "sans" })?;
    fact(
        &mut out,
        "saving",
        if app.saving.is_some() { "1" } else { "0" },
    )?;
    fact(&mut out, "line", &app.status_text())?;
    Ok(out)
}

/// `n` in decimal.
fn count(n: impl TryInto<u64>) -> Result<String, td_alloc::Error> {
    let n: u64 = n.try_into().unwrap_or(u64::MAX);
    td_alloc::format(format_args!("{n}")).map_err(|_| td_alloc::Error)
}

fn fact(out: &mut String, key: &str, value: &str) -> Result<(), td_alloc::Error> {
    if !out.is_empty() {
        td_alloc::push_str(out, "\t")?;
    }
    td_alloc::push_str(out, key)?;
    td_alloc::push_str(out, "=")?;
    let mut buffer = [0u8; 4];
    for c in value.chars() {
        let c = if c.is_control() { ' ' } else { c };
        td_alloc::push_str(out, c.encode_utf8(&mut buffer))?;
    }
    Ok(())
}

/// One paint of the window, its draws kept in order in storage charged
/// to the window's root, at most `MAX_RECORDED`.
struct Recording {
    draws: BudgetVec<Draw>,
    refused: bool,
}

impl Recording {
    fn new(budget: &Budget) -> Result<Self, Refusal> {
        let draws = td_alloc::BudgetVec::with_capacity(budget, FIRST_RECORDED)
            .map_err(|_| Refusal::Limit)?;
        Ok(Self {
            draws,
            refused: false,
        })
    }

    fn keep(&mut self, draw: Draw) {
        if self.refused {
            return;
        }
        let draws = &mut self.draws;
        if draws.len() == draws.capacity() {
            let target = draws.capacity().saturating_mul(2).min(MAX_RECORDED);
            let grown = (target > draws.capacity())
                .then(|| td_alloc::BudgetVec::with_capacity(draws.budget(), target).ok())
                .flatten();
            let Some(mut grown) = grown else {
                self.refused = true;
                return;
            };
            for &kept in draws.iter() {
                if grown.push_within_capacity(kept).is_err() {
                    self.refused = true;
                    return;
                }
            }
            *draws = grown;
        }
        if draws.push_within_capacity(draw).is_err() {
            self.refused = true;
        }
    }

    /// The draws, or `limit` where one could not be kept.
    fn finish(self) -> Result<BudgetVec<Draw>, Refusal> {
        if self.refused {
            Err(Refusal::Limit)
        } else {
            Ok(self.draws)
        }
    }
}

impl Canvas for Recording {
    fn draw(&mut self, draw: Draw) {
        self.keep(draw);
    }
    /// The picture as its id, which `Recorded` hands over; td-ui draws a
    /// composition's picture at its fitted size, so one the window draws
    /// at a factor is drawn smaller.
    fn image(&mut self, clip: Rect, rect: Rect, id: u64, _: &Fitted, _: usize) {
        self.keep(Draw {
            clip,
            primitive: Primitive::Image { id, rect },
        });
    }
}

/// A recorded paint replayed within a damage, its pictures the window's.
struct Recorded<'r> {
    app: &'r App,
    draws: &'r [Draw],
}

impl Composition for Recorded<'_> {
    fn surface(&self) -> Surface {
        self.app.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        for draw in self.draws {
            if let Some(clip) = draw.clip.intersection(damage) {
                sink(Draw { clip, ..*draw });
            }
        }
    }

    fn picture(&self, id: u64, with: &mut dyn FnMut(&Fitted)) {
        if id == PAGE_PICTURE {
            if let PictureFit::Ready(fitted) = &self.app.current.fitted {
                with(fitted);
            }
        } else if let Some((fitted, _)) =
            self.app.current.laid().and_then(|laid| laid.fitted_by(id))
        {
            with(fitted);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use crate::app::tests::{app, base64, pictured, png, settle, site};
    use crate::fetch::tests::Table;
    use td_ui::driven;

    fn ask(app: &mut App, line: &str) -> String {
        driven::request(&mut Remote::new(app), line.as_bytes())
    }

    /// The body of an `ok` reply.
    fn body(reply: &str) -> &str {
        let mut fields = reply.splitn(4, '\t');
        assert_eq!(fields.nth(2), Some("ok"), "{reply}");
        fields.next().unwrap_or("")
    }

    fn open(app: &mut App, address: &str) {
        let line = format!("1\t1\taction\topen\t{}", control::hex(address.as_bytes()));
        assert_eq!(body(&ask(app, &line)), "changed");
        settle(app);
    }

    /// The window read back as text, a line a row.
    fn text(app: &mut App) -> Vec<String> {
        let reply = ask(app, "1\t2\ttext");
        let hex = body(&reply).rsplit('\t').next().unwrap();
        let bytes = control::unhex(hex).unwrap();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn state(app: &mut App) -> Vec<String> {
        let reply = ask(app, "1\t3\tstate");
        body(&reply).split('\t').map(str::to_string).collect()
    }

    fn has(facts: &[String], fact: &str) -> bool {
        facts.iter().any(|f| f == fact)
    }

    #[test]
    fn the_action_table_holds_to_the_grammar() {
        driven::check(BINDINGS).unwrap();
        let help = driven::help(BINDINGS);
        for binding in BINDINGS {
            assert!(help.contains(binding.name), "{}", binding.name);
        }
    }

    /// The page's words read back apart: the painter draws each space,
    /// which the seam's prose read-back needs.
    #[test]
    fn a_page_reads_back_as_its_words() {
        let mut app = app(site());
        open(&mut app, "https://example.org/");
        let lines = text(&mut app);
        assert!(
            lines[0].contains("https://example.org/"),
            "the address row: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.trim() == "to a mail"),
            "the page: {lines:?}"
        );
        let markup = "<pre>one\ttwo  three</pre><p>a&nbsp;b</p>";
        let mut app = super::super::tests::app(Table::default().page(
            "https://example.org/t",
            "text/html",
            markup,
        ));
        open(&mut app, "https://example.org/t");
        let lines = text(&mut app);
        // A tab reads as a space at its pen, then the run after its stop.
        assert!(
            lines.iter().any(|line| {
                let line = line.trim();
                line.starts_with("one ") && line.ends_with("two  three")
            }),
            "{lines:?}"
        );
        assert!(lines.iter().any(|line| line.trim() == "a b"), "{lines:?}");
    }

    #[test]
    fn state_follows_the_window_through_its_own_keys() {
        let mut app = app(site());
        let facts = state(&mut app);
        assert!(has(&facts, "url=none"), "{facts:?}");
        assert!(has(&facts, "focus=address"), "{facts:?}");
        assert!(has(&facts, "body=blank"), "{facts:?}");
        open(&mut app, "example.org");
        let facts = state(&mut app);
        for fact in [
            "url=https://example.org/",
            "title=Home - td-web",
            "focus=page",
            "loading=none",
            "body=page",
            "status=200",
            "back=0",
            "forward=0",
            "link=none",
            "selection=none",
            "styles=page",
            "face=sans",
        ] {
            assert!(has(&facts, fact), "{fact}: {facts:?}");
        }
        assert_eq!(body(&ask(&mut app, "1\t4\taction\tnext-link")), "changed");
        assert!(has(&state(&mut app), "link=https://example.org/a"));
        assert_eq!(body(&ask(&mut app, "1\t5\taction\tfollow")), "changed");
        settle(&mut app);
        let facts = state(&mut app);
        assert!(has(&facts, "title=A - td-web"), "{facts:?}");
        assert!(has(&facts, "back=1"), "{facts:?}");
        assert_eq!(body(&ask(&mut app, "1\t6\taction\tback")), "changed");
        settle(&mut app);
        let facts = state(&mut app);
        assert!(has(&facts, "title=Home - td-web"), "{facts:?}");
        assert!(has(&facts, "forward=1"), "{facts:?}");
        assert_eq!(body(&ask(&mut app, "1\t7\taction\tselect-all")), "changed");
        assert!(has(&state(&mut app), "selection=some"));
        // F8 and F9 as their actions.
        ask(&mut app, "1\t8\taction\tbody-face");
        ask(&mut app, "1\t9\taction\tpage-styles");
        let facts = state(&mut app);
        assert!(has(&facts, "face=serif"), "{facts:?}");
        assert!(has(&facts, "styles=none"), "{facts:?}");
        // A copy through the seam has no clipboard to reach.
        ask(&mut app, "1\t10\taction\tcopy");
        let facts = state(&mut app);
        assert!(
            has(
                &facts,
                "line=not copied: the clipboard refused the selection"
            ),
            "{facts:?}"
        );
    }

    #[test]
    fn a_fact_holds_no_tab_or_line_end() {
        let mut app = app(Table::default().page(
            "https://example.org/",
            "text/html",
            "<title>a\tb\nc</title><p>x",
        ));
        open(&mut app, "https://example.org/");
        let facts = state(&mut app);
        // The window's title, as the window names it.
        assert!(has(&facts, "title=a b c - td-web"), "{facts:?}");
    }

    #[test]
    fn the_pointer_over_a_link_says_where_it_goes_and_a_click_follows_it() {
        let mut app = app(site());
        open(&mut app, "https://example.org/");
        let page = app.page_rect();
        let (x, y) = (page.y..page.y + i64::from(page.height))
            .flat_map(|y| (0..i64::from(page.width)).map(move |x| (x, y)))
            .find(|&(x, y)| app.link_at(x, y) == Some(0))
            .unwrap();
        let line = format!("1\t1\tpointer\tmove\t{x}\t{y}");
        assert_eq!(body(&ask(&mut app, &line)), "changed");
        assert!(has(&state(&mut app), "hover=https://example.org/a"));
        // A press and its release there are a click, which follows it.
        ask(&mut app, &format!("1\t2\tpointer\tpress\t{x}\t{y}"));
        ask(&mut app, &format!("1\t3\tpointer\trelease\t{x}\t{y}"));
        settle(&mut app);
        assert!(has(&state(&mut app), "title=A - td-web"));
    }

    /// The seam's recording replays the window's draws: the paint
    /// recorded is the one `Handler::paint` draws, its pictures among
    /// it, here on the cell, with no prose faces on either side.
    #[test]
    fn the_seams_frame_is_the_windows() {
        let inline = png(6, 4, [0, 200, 0]);
        let markup = format!(
            "<p>a picture <img src=\"data:image/png;base64,{}\" alt=g> in a line",
            base64(&inline)
        );
        let table = Table::default()
            .page("https://example.org/p", "text/html", &markup)
            .picture("https://example.org/w.png", &png(40, 30, [200, 0, 0]));
        let mut app = app(table);
        open(&mut app, "https://example.org/p");
        pictured(&mut app);
        assert_frames(&mut app);
        open(&mut app, "https://example.org/w.png");
        assert!(has(&state(&mut app), "body=picture"));
        assert_frames(&mut app);
        assert_eq!(body(&ask(&mut app, "1\t1\tresize\t320\t200\t1")), "changed");
        assert_frames(&mut app);
        let frame = ask(&mut app, "1\t2\tframe");
        assert!(body(&frame).starts_with("320\t200\t1\t"), "{frame}");
    }

    /// `Handler::paint` into a raster over the pinned cell face, and the
    /// seam's paint of its recording, pixel for pixel.
    fn assert_frames(app: &mut App) {
        let font = td_ui::font::pinned().unwrap();
        let surface = app.surface;
        let stride = surface.width * 4;
        let mut pixels = vec![0u8; stride * surface.height];
        let mut raster = td_ui::raster::Raster::new(&mut pixels, &font, surface, stride).unwrap();
        app.paint(&mut raster, surface).unwrap();
        let window = td_ui::raster::rgb(&pixels, surface, stride).unwrap();
        let seam = Remote::new(app)
            .compose(|composition| driven::paint(composition).unwrap().rgb)
            .unwrap();
        assert!(window == seam, "the seam's frame differs from the window's");
    }

    #[test]
    fn fields_the_actions_do_not_take_are_refused() {
        let mut app = app(site());
        for line in [
            "1\t1\taction\topen",
            "1\t1\taction\topen\tzz",
            "1\t1\taction\topen\t61\t62",
            // Not UTF-8.
            "1\t1\taction\topen\tff",
            "1\t1\taction\treload\tx",
            "1\t1\taction\tnope",
        ] {
            let reply = ask(&mut app, line);
            assert!(
                reply.starts_with("1\t1\terror\tprotocol\t"),
                "{line}: {reply}"
            );
        }
        assert!(has(&state(&mut app), "url=none"));
    }

    #[test]
    fn close_is_reported_as_quit() {
        let mut app = app(site());
        assert_eq!(body(&ask(&mut app, "1\t1\taction\tclose")), "quit");
        assert!(app.quit);
    }

    /// `?` opens the key list from the window's own keyboard, and never
    /// through the seam.
    #[test]
    fn the_seam_never_opens_the_key_list() {
        let mut app = app(site());
        open(&mut app, "https://example.org/");
        assert_eq!(body(&ask(&mut app, "1\t1\tkey\t3f")), "changed");
        assert!(!app.take_show_keys());
        let chord = window::Input::Key {
            chord: "?",
            repeat: false,
        };
        app.input(chord, &mut NoClipboard);
        assert!(app.take_show_keys());
    }

    /// The first link's pixel and one well past it on the page.
    fn on_and_off_the_link(app: &mut App) -> ((i64, i64), (i64, i64)) {
        let page = app.page_rect();
        let on = (page.y..page.y + i64::from(page.height))
            .flat_map(|y| (0..i64::from(page.width)).map(move |x| (x, y)))
            .find(|&(x, y)| app.link_at(x, y) == Some(0))
            .unwrap();
        (on, (on.0 + 200, on.1 + 40))
    }

    /// A drag from a link through the seam leaves the window as the same
    /// drag delivered as td-ui's window delivers it: the hover let go
    /// at the press, the text selected, nothing followed.
    #[test]
    fn a_drag_through_the_seam_is_the_windows_drag() {
        let mut seam = app(site());
        open(&mut seam, "https://example.org/");
        let (on, off) = on_and_off_the_link(&mut seam);
        let mut window = app(site());
        open(&mut window, "https://example.org/");
        let pointer = |phase, (x, y): (i64, i64)| window::Input::Pointer {
            phase,
            x,
            y,
            extend: false,
            follow: false,
        };
        let steps = [("move", on), ("press", on), ("move", off), ("release", off)];
        for (step, (phase, (x, y))) in steps.into_iter().enumerate() {
            ask(&mut seam, &format!("1\t1\tpointer\t{phase}\t{x}\t{y}"));
            let board = &mut NoClipboard;
            match phase {
                "press" => {
                    window.input(pointer(window::PointerPhase::Press, (x, y)), board);
                    window.motion(None);
                }
                "release" => {
                    window.input(pointer(window::PointerPhase::Release, (x, y)), board);
                    window.motion(Some((x, y)));
                }
                _ if step == 0 => window.motion(Some((x, y))),
                _ => {
                    window.input(pointer(window::PointerPhase::Move, (x, y)), board);
                }
            }
            assert_eq!(state(&mut seam), state(&mut window), "after {phase} {step}");
        }
        let seam = state(&mut seam);
        assert!(has(&seam, "selection=some"), "{seam:?}");
        assert!(has(&seam, "hover=none"), "{seam:?}");
        assert!(has(&seam, "title=Home - td-web"), "{seam:?}");
    }

    /// A press in the address row holds the button too: a move over a
    /// link then is no hover.
    #[test]
    fn a_press_off_the_page_still_holds_the_button() {
        let mut app = app(site());
        open(&mut app, "https://example.org/");
        let ((x, y), _) = on_and_off_the_link(&mut app);
        ask(&mut app, "1\t1\tpointer\tpress\t5\t5");
        ask(&mut app, &format!("1\t2\tpointer\tmove\t{x}\t{y}"));
        assert!(has(&state(&mut app), "hover=none"));
        ask(&mut app, &format!("1\t3\tpointer\trelease\t{x}\t{y}"));
        assert!(has(&state(&mut app), "hover=https://example.org/a"));
        // A release with no press held, and a press while one is, are
        // the window's to drop.
        let release = format!("1\t4\tpointer\trelease\t{x}\t{y}");
        assert_eq!(body(&ask(&mut app, &release)), "ignored");
        ask(&mut app, "1\t5\tpointer\tpress\t5\t5");
        let press = format!("1\t6\tpointer\tpress\t{x}\t{y}");
        assert_eq!(body(&ask(&mut app, &press)), "ignored");
        assert!(app.press.is_none(), "the second press reached the window");
    }

    /// The wheel scrolls as the window's does, and `tick` is its poll,
    /// taking a page that has come.
    #[test]
    fn the_wheel_scrolls_and_a_tick_takes_what_has_come() {
        let lines: String = (0..200).map(|n| format!("<p>line {n}")).collect();
        let mut app = app(Table::default().page("https://example.org/", "text/html", &lines));
        let line = format!(
            "1\t1\taction\topen\t{}",
            control::hex(b"https://example.org/")
        );
        assert_eq!(body(&ask(&mut app, &line)), "changed");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while app.loading.is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the page did not come"
            );
            ask(&mut app, "1\t2\ttick\t0");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(has(&state(&mut app), "body=page"));
        let top = app.current.view.y;
        assert_eq!(body(&ask(&mut app, "1\t3\twheel\t5\t0")), "changed");
        assert!(app.current.view.y > top);
        // Nothing come and nothing to settle: a tick changes nothing.
        assert_eq!(body(&ask(&mut app, "1\t4\ttick\t0")), "ignored");
    }

    fn one_pixel() -> Draw {
        let rect = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        };
        Draw {
            clip: rect,
            primitive: Primitive::Fill { rect, color: 0 },
        }
    }

    /// A recording its budget cannot grow, or one past `MAX_RECORDED`
    /// draws, is refused whole, its charge returned.
    #[test]
    fn a_recording_past_its_room_is_refused() {
        let size = std::mem::size_of::<Draw>();
        // Room for the first capacity and not for the doubled one beside it.
        let budget = Budget::new(FIRST_RECORDED * size * 2).unwrap();
        let before = budget.used();
        let mut recording = Recording::new(&budget).unwrap();
        for _ in 0..=FIRST_RECORDED {
            recording.keep(one_pixel());
        }
        assert_eq!(recording.finish().err(), Some(Refusal::Limit));
        assert_eq!(budget.used(), before);
        let budget = Budget::new(MAX_RECORDED * size * 4).unwrap();
        let before = budget.used();
        let mut recording = Recording::new(&budget).unwrap();
        for _ in 0..MAX_RECORDED {
            recording.keep(one_pixel());
        }
        assert!(!recording.refused);
        recording.keep(one_pixel());
        assert_eq!(recording.finish().err(), Some(Refusal::Limit));
        assert_eq!(budget.used(), before);
    }
}
