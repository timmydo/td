//! One prompt as td-ui widgets: the request's explanation from the top,
//! the error row, the labelled field (two when the text is typed twice)
//! and the buttons across the foot. Pure: the window hands it inputs and
//! the clock and paints what it composes, so its tests need no
//! compositor.

use td_ui::chrome::{Buttons, TextEntry, ROW};
use td_ui::entry_model::{Action, EntryModel, Outcome};
use td_ui::keys::Section;
use td_ui::raster::{
    Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Surface, INK, PAPER, WARNING,
};
use td_ui::window::{Clipboard, Flow, Input, PointerPhase};
use td_ui::{CELL_HEIGHT, CELL_WIDTH};

use crate::request::{Answer, Kind, Request, Secret, MAX_SECRET};

/// What a keyboard refusal shows, naming no key. td-ui reports a key
/// it cannot type and a layout it refuses whole alike, so this says
/// either.
pub const KEY_REFUSED: &str =
    "The keyboard refused a key or its layout; only printable ASCII without AltGr types.";

/// How td-ui's window begins every keyboard notice
/// (`td_ui::window`, `KeyboardEvent::Refused` and `Keymap(Err)`).
pub const KEYBOARD: &str = "keyboard: ";

/// How long the caret shows, then hides.
const BLINK_MS: u64 = 500;
/// The longest the window sleeps between polls, so a caller that hangs
/// up is noticed this soon.
const MAX_WAIT_MS: u64 = 100;

/// Which field has the keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    First,
    Repeat,
}

/// What a button answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Choice {
    Ok,
    NotOk,
    Cancel,
}

/// Where each part lies on the surface; a part the surface has no room
/// for is `None` and is neither painted nor hit.
struct Layout<'a> {
    description: Vec<String>,
    description_top: i64,
    error_top: Option<i64>,
    remark_top: Option<i64>,
    label_top: Option<i64>,
    first: Option<TextEntry>,
    repeat_label_top: Option<i64>,
    repeat: Option<TextEntry>,
    buttons: Buttons<'a>,
}

pub struct Dialog {
    request: Request,
    surface: Surface,
    first: EntryModel,
    repeat: Option<EntryModel>,
    focus: Field,
    window_focused: bool,
    caret_visible: bool,
    blink_at: u64,
    /// The button a press armed; its release over the same button acts.
    armed: Option<usize>,
    /// The button Return presses on a question, moved by the arrows and
    /// Tab; a text's Return is always OK.
    selected: usize,
    /// Whether the person has typed, which stops the timeout.
    typed: bool,
    dragging: Option<Field>,
    /// The error row: the request's error, or two fields that differ.
    error: Option<String>,
    /// td-pinentry's own row over it: a refused key, paste or typed
    /// character, which never hides the agent's error.
    remark: Option<String>,
    answer: Option<Answer>,
    started: Option<u64>,
    /// The clock at the last tick.
    now: u64,
    redraw: bool,
    scrub: bool,
}

impl Dialog {
    pub fn new(request: Request, surface: Surface) -> Result<Self, String> {
        let field =
            || EntryModel::new(MAX_SECRET).map_err(|refusal| format!("the field: {refusal}"));
        let mut first = field()?;
        let mut repeat = None;
        if let Kind::Text {
            masked,
            repeat: second,
            ..
        } = &request.kind
        {
            first.set_masked(*masked);
            if second.is_some() {
                let mut model = field()?;
                model.set_masked(*masked);
                repeat = Some(model);
            }
        }
        Ok(Self {
            error: request.error.clone(),
            remark: None,
            request,
            surface,
            first,
            repeat,
            focus: Field::First,
            window_focused: false,
            caret_visible: true,
            blink_at: 0,
            armed: None,
            selected: 0,
            typed: false,
            dragging: None,
            answer: None,
            started: None,
            now: 0,
            redraw: true,
            scrub: false,
        })
    }

    pub fn title(&self) -> &str {
        &self.request.title
    }

    /// How the prompt ended, once it has.
    pub fn take_answer(&mut self) -> Option<Answer> {
        self.answer.take()
    }

    pub fn needs_redraw(&self) -> bool {
        self.redraw
    }

    pub fn take_scrub(&mut self) -> bool {
        std::mem::take(&mut self.scrub)
    }

    fn texts(&self) -> bool {
        matches!(self.request.kind, Kind::Text { .. })
    }

    /// The buttons, left to right, and what each answers: OK alone on a
    /// message, the not-OK button only on a question that has Cancel too.
    fn buttons(&self) -> Vec<(&str, Choice)> {
        let mut buttons = vec![(self.request.ok.as_str(), Choice::Ok)];
        match self.request.kind {
            Kind::Confirm { one_button: true } => return buttons,
            Kind::Confirm { one_button: false } => {
                if let Some(not_ok) = &self.request.not_ok {
                    buttons.push((not_ok.as_str(), Choice::NotOk));
                }
            }
            Kind::Text { .. } => {}
        }
        buttons.push((self.request.cancel.as_str(), Choice::Cancel));
        buttons
    }

    /// A window notice, and what of it may go to standard error. The
    /// keyboard's refusals (`keyboard: ...`) can name the key pressed, a
    /// character of the secret, so one becomes `KEY_REFUSED` there and in
    /// the remark row, where the person looks. Any other notice goes to
    /// standard error alone.
    pub fn notice<'a>(&mut self, message: &'a str) -> &'a str {
        if message.starts_with(KEYBOARD) {
            self.remark(KEY_REFUSED);
            return KEY_REFUSED;
        }
        message
    }

    fn layout<'a>(&self, labels: &'a [&'a str]) -> Layout<'a> {
        let surface = self.surface;
        let s = surface.scale.value() as i64;
        let row = ROW as i64 * s;
        let cell = CELL_WIDTH as i64 * s;
        let width = surface.width as i64;
        let height = surface.height as i64;
        let columns = usize::try_from((width - 2 * cell) / cell).unwrap_or(0);
        let probe = Buttons::new(surface, 0, labels);
        let band_rows = i64::try_from(probe.rows()).unwrap_or(1);
        let band_top = (height - band_rows * row).max(0);
        let buttons = Buttons::new(surface, band_top, labels);

        // From the band up: the fields, their labels, the error row, the
        // remark row.
        let mut bottom = band_top - row / 2;
        let field_rect = |top: i64| Rect {
            x: cell,
            y: top,
            width: u32::try_from(width - 2 * cell).unwrap_or(0),
            height: u32::try_from(row).unwrap_or(0),
        };
        let labelled = !self.request.prompt.is_empty();
        let mut take = |rows: i64| {
            bottom -= rows * row;
            bottom
        };
        let (mut repeat, mut repeat_label_top) = (None, None);
        if let Kind::Text {
            repeat: Some(label),
            ..
        } = &self.request.kind
        {
            repeat = TextEntry::new(surface, field_rect(take(1)));
            if !label.is_empty() {
                repeat_label_top = Some(take(1));
            }
        }
        let (mut first, mut label_top) = (None, None);
        if self.texts() {
            first = TextEntry::new(surface, field_rect(take(1)));
            if labelled {
                label_top = Some(take(1));
            }
        }
        // The agent's error is nearer the field, so it is the last row a
        // short window gives up.
        let error_top = self.error.as_ref().map(|_| take(1));
        let remark_top = self.remark.as_ref().map(|_| take(1));
        let description_top = row / 2;
        let room = usize::try_from((bottom - description_top) / row).unwrap_or(0);
        let mut description = wrap(&self.request.description, columns);
        shorten(&mut description, room, columns);
        Layout {
            description,
            description_top,
            error_top: error_top.filter(|top| *top >= 0),
            remark_top: remark_top.filter(|top| *top >= 0),
            label_top: label_top.filter(|top| *top >= 0),
            first,
            repeat_label_top: repeat_label_top.filter(|top| *top >= 0),
            repeat,
            buttons,
        }
    }

    fn model(&mut self, field: Field) -> Option<&mut EntryModel> {
        match field {
            Field::First => Some(&mut self.first),
            Field::Repeat => self.repeat.as_mut(),
        }
    }

    fn entry(&self, field: Field) -> Option<TextEntry> {
        let buttons = self.buttons();
        let labels: Vec<&str> = buttons.iter().map(|(label, _)| *label).collect();
        let layout = self.layout(&labels);
        match field {
            Field::First => layout.first,
            Field::Repeat => layout.repeat,
        }
    }

    fn reveal(&mut self) {
        for field in [Field::First, Field::Repeat] {
            let entry = self.entry(field);
            if let (Some(entry), Some(model)) = (entry, self.model(field)) {
                model.reveal(entry);
            }
        }
    }

    /// Shows the caret at once after an edit or a move, so it is never
    /// hidden while the person types.
    fn touched(&mut self) {
        self.caret_visible = true;
        self.blink_at = self.now.saturating_add(BLINK_MS);
        self.reveal();
        self.redraw = true;
    }

    fn finish(&mut self, answer: Answer) -> Flow {
        self.first.clear();
        if let Some(repeat) = self.repeat.as_mut() {
            repeat.clear();
        }
        self.answer = Some(answer);
        self.scrub = true;
        self.redraw = true;
        Flow::Quit
    }

    fn say(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
        self.redraw = true;
    }

    fn remark(&mut self, remark: impl Into<String>) {
        self.remark = Some(remark.into());
        self.redraw = true;
    }

    fn choose(&mut self, choice: Choice) -> Flow {
        match choice {
            Choice::Cancel => self.finish(Answer::Cancelled),
            Choice::NotOk => self.finish(Answer::Declined),
            Choice::Ok if !self.texts() => self.finish(Answer::Confirmed),
            Choice::Ok => self.submit(),
        }
    }

    /// OK on a text: the first field's text, once both fields agree when
    /// it is typed twice.
    fn submit(&mut self) -> Flow {
        if let Some(repeat) = &self.repeat {
            if repeat.text() != self.first.text() {
                let error = match &self.request.kind {
                    Kind::Text { repeat_error, .. } => repeat_error.clone(),
                    Kind::Confirm { .. } => String::new(),
                };
                if let Some(repeat) = self.repeat.as_mut() {
                    repeat.clear();
                }
                self.focus = Field::Repeat;
                self.say(error);
                self.touched();
                return Flow::Continue;
            }
        }
        let secret = Secret::copy(self.first.text());
        self.finish(Answer::Text(secret))
    }

    pub fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) -> Flow {
        if self.answer.is_some() {
            return Flow::Quit;
        }
        match input {
            Input::Key { chord, .. } => self.key(chord, clipboard),
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => self.pointer(phase, x, y, extend),
            Input::CancelPointer => {
                self.armed = None;
                self.dragging = None;
                Flow::Continue
            }
            Input::Resize(surface) => {
                self.surface = surface;
                self.armed = None;
                self.dragging = None;
                self.touched();
                Flow::Continue
            }
            Input::Focus(focused) => {
                self.window_focused = focused;
                self.redraw = true;
                Flow::Continue
            }
            Input::Close => self.finish(Answer::Cancelled),
            Input::Paste(text) => {
                let (focus, texts) = (self.focus, self.texts());
                let pasted = match self.model(focus) {
                    Some(model) if texts => Some(model.paste(text)),
                    _ => None,
                };
                match pasted {
                    Some(Ok(outcome)) => {
                        self.typed |= outcome == Outcome::Changed;
                        self.touched();
                    }
                    Some(Err(refusal)) => self.remark(format!("Not pasted: {refusal}.")),
                    None => {}
                }
                Flow::Continue
            }
            Input::Wheel { .. } | Input::Hover(_) | Input::Context { .. } => Flow::Continue,
        }
    }

    fn key(&mut self, chord: &str, clipboard: &mut dyn Clipboard) -> Flow {
        if !self.texts() {
            return self.question_key(chord);
        }
        match chord {
            "Escape" => return self.finish(Answer::Cancelled),
            "Return" => {
                if self.focus == Field::First && self.repeat.is_some() {
                    self.focus = Field::Repeat;
                    self.touched();
                    return Flow::Continue;
                }
                return self.choose(Choice::Ok);
            }
            "Tab" | "S-Tab" => {
                if self.repeat.is_some() {
                    self.focus = match self.focus {
                        Field::First => Field::Repeat,
                        Field::Repeat => Field::First,
                    };
                    self.touched();
                }
                return Flow::Continue;
            }
            "C-v" | "S-Insert" => {
                if let Err(refusal) = clipboard.paste() {
                    self.remark(format!("Not pasted: {refusal}."));
                }
                return Flow::Continue;
            }
            _ => {}
        }
        let Some(action) = Action::from_chord(chord) else {
            return Flow::Continue;
        };
        let focus = self.focus;
        let Some(model) = self.model(focus) else {
            return Flow::Continue;
        };
        match model.act(action) {
            Ok(Outcome::Ignored) => {}
            Ok(outcome) => {
                self.typed |= outcome == Outcome::Changed;
                self.touched();
            }
            Err(refusal) => self.remark(format!("Not typed: {refusal}.")),
        }
        Flow::Continue
    }

    /// A question's keys: Return presses the selected button, the arrows
    /// and Tab move the selection, Escape cancels.
    fn question_key(&mut self, chord: &str) -> Flow {
        let count = self.buttons().len().max(1);
        let selected = self.selected.min(count - 1);
        self.selected = match chord {
            "Escape" => return self.finish(Answer::Cancelled),
            "Return" => {
                let choice = self.buttons().get(selected).map(|(_, choice)| *choice);
                return self.choose(choice.unwrap_or(Choice::Ok));
            }
            "Right" | "Tab" => (selected + 1) % count,
            "Left" | "S-Tab" => (selected + count - 1) % count,
            _ => return Flow::Continue,
        };
        self.redraw = true;
        Flow::Continue
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) -> Flow {
        let buttons = self.buttons();
        let labels: Vec<&str> = buttons.iter().map(|(label, _)| *label).collect();
        let choices: Vec<Choice> = buttons.iter().map(|(_, choice)| *choice).collect();
        let layout = self.layout(&labels);
        let hit = layout.buttons.hit(x, y);
        let (first, repeat) = (layout.first, layout.repeat);
        match phase {
            PointerPhase::Press => {
                self.armed = hit;
                let field = [(Field::First, first), (Field::Repeat, repeat)]
                    .into_iter()
                    .find_map(|(field, entry)| {
                        entry
                            .filter(|entry| entry.rect().contains(x, y))
                            .map(|e| (field, e))
                    });
                if let Some((field, entry)) = field {
                    self.focus = field;
                    self.dragging = Some(field);
                    if let Some(model) = self.model(field) {
                        model.place(entry, x, y, extend);
                    }
                    self.touched();
                }
                Flow::Continue
            }
            PointerPhase::Move => {
                let dragged = match self.dragging {
                    Some(Field::First) => first.map(|entry| (Field::First, entry)),
                    Some(Field::Repeat) => repeat.map(|entry| (Field::Repeat, entry)),
                    None => None,
                };
                if let Some((field, entry)) = dragged {
                    if let Some(model) = self.model(field) {
                        if model.drag(entry, x, y) != Outcome::Ignored {
                            self.touched();
                        }
                    }
                }
                Flow::Continue
            }
            PointerPhase::Release => {
                self.dragging = None;
                let armed = self.armed.take();
                match armed.filter(|armed| Some(*armed) == hit) {
                    Some(index) => match choices.get(index) {
                        Some(choice) => self.choose(*choice),
                        None => Flow::Continue,
                    },
                    None => Flow::Continue,
                }
            }
        }
    }

    /// The caller went away: nothing is answered.
    pub fn hang_up(&mut self) -> Flow {
        if self.answer.is_some() {
            return Flow::Quit;
        }
        self.finish(Answer::Hangup)
    }

    /// The clock, in milliseconds since the window's loop began: the
    /// caret's blink and the request's timeout.
    pub fn tick(&mut self, now: u64) -> Flow {
        if self.answer.is_some() {
            return Flow::Quit;
        }
        self.now = now;
        let started = *self.started.get_or_insert(now);
        if let Some(seconds) = self.request.timeout.filter(|_| !self.typed) {
            if now.saturating_sub(started) >= seconds.saturating_mul(1000) {
                return self.finish(Answer::TimedOut);
            }
        }
        if now >= self.blink_at {
            if self.blink_at != 0 {
                self.caret_visible = !self.caret_visible;
                if self.texts() && self.window_focused {
                    self.redraw = true;
                }
            }
            self.blink_at = now.saturating_add(BLINK_MS);
        }
        Flow::Continue
    }

    pub fn wait_ms(&self, now: u64) -> u64 {
        let mut wait = MAX_WAIT_MS.min(self.blink_at.saturating_sub(now));
        let timeout = self.request.timeout.filter(|_| !self.typed);
        if let (Some(seconds), Some(started)) = (timeout, self.started) {
            let end = started.saturating_add(seconds.saturating_mul(1000));
            wait = wait.min(end.saturating_sub(now));
        }
        wait.max(1)
    }

    pub fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.surface {
            self.surface = surface;
            self.reveal();
        }
        self.redraw = false;
        raster
            .paint(&Frame { dialog: self }, surface.bounds())
            .map_err(|error| error.to_string())
    }

    /// The keys the prompt binds, above the window's own in the key list.
    pub fn keys(&self) -> Vec<Section> {
        keys()
    }
}

pub fn keys() -> Vec<Section> {
    vec![Section::new(
        "Prompt",
        &[
            (
                "Return",
                "answer with what the field holds, or press the selected button",
            ),
            ("Escape", "cancel, as closing the window does"),
            ("Tab/S-Tab", "move between the two fields when asked twice"),
            ("Left/Right/Tab/S-Tab", "select a question's button"),
            ("C-v/S-Insert", "paste into the field"),
            ("click", "press a button, or place the caret in a field"),
        ],
    )]
}

struct Frame<'a> {
    dialog: &'a Dialog,
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

/// One row of text from a cell in from the left, in `ink` on paper, a
/// control character shown as a space.
fn text(
    surface: Surface,
    top: i64,
    line: &str,
    ink: u32,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let s = surface.scale.value() as i64;
    let (cell, row) = (CELL_WIDTH as i64 * s, ROW as i64 * s);
    let rect = Rect {
        x: 0,
        y: top,
        width: u32::try_from(surface.width).unwrap_or(0),
        height: u32::try_from(row).unwrap_or(0),
    };
    let Some(clip) = rect.intersection(damage) else {
        return;
    };
    let y = top + (row - CELL_HEIGHT as i64 * s) / 2;
    let columns = usize::try_from((surface.width as i64 - 2 * cell) / cell).unwrap_or(0);
    let style = GlyphStyle::medium(ink, PAPER);
    for (column, scalar) in line.chars().take(columns).enumerate() {
        sink(Draw {
            clip,
            primitive: Primitive::Glyph {
                x: cell * (column as i64 + 1),
                y,
                scalar: if scalar.is_control() { ' ' } else { scalar },
                style,
            },
        });
    }
}

impl Composition for Frame<'_> {
    fn surface(&self) -> Surface {
        self.dialog.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let dialog = self.dialog;
        let surface = dialog.surface;
        let row = ROW as i64 * surface.scale.value() as i64;
        fill(surface.bounds(), PAPER, damage, sink);
        let buttons = dialog.buttons();
        let labels: Vec<&str> = buttons.iter().map(|(label, _)| *label).collect();
        let layout = dialog.layout(&labels);
        for (index, line) in layout.description.iter().enumerate() {
            let top = layout.description_top + row * index as i64;
            text(surface, top, line, INK, damage, sink);
        }
        if let (Some(top), Some(error)) = (layout.error_top, &dialog.error) {
            text(surface, top, error, WARNING, damage, sink);
        }
        if let (Some(top), Some(remark)) = (layout.remark_top, &dialog.remark) {
            text(surface, top, remark, WARNING, damage, sink);
        }
        if let Some(top) = layout.label_top {
            text(surface, top, &dialog.request.prompt, INK, damage, sink);
        }
        if let (
            Some(top),
            Kind::Text {
                repeat: Some(label),
                ..
            },
        ) = (layout.repeat_label_top, &dialog.request.kind)
        {
            text(surface, top, label, INK, damage, sink);
        }
        let caret = dialog.caret_visible;
        let focused = |field| dialog.window_focused && dialog.focus == field;
        if let Some(entry) = layout.first {
            let field = dialog.first.field("", focused(Field::First), caret);
            entry.emit(field, damage, sink);
        }
        if let (Some(entry), Some(model)) = (layout.repeat, &dialog.repeat) {
            entry.emit(model.field("", focused(Field::Repeat), caret), damage, sink);
        }
        // What Return presses shows selected: OK on a text, the chosen
        // button on a question.
        let texts = dialog.texts();
        let states = buttons.iter().enumerate().map(|(index, (_, choice))| {
            let selected = if texts {
                *choice == Choice::Ok
            } else {
                index == dialog.selected
            };
            (selected, true)
        });
        layout.buttons.emit(states, damage, sink);
    }
}

/// `text` as rows of at most `columns` characters: each line of it broken
/// at spaces, a word longer than a row split across rows, and an empty
/// line kept as an empty row. No columns is one row per line.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.split('\n') {
        if columns == 0 {
            rows.push(line.to_owned());
            continue;
        }
        let mut row = String::new();
        let mut width = 0;
        for word in line.split_whitespace() {
            let length = word.chars().count();
            if width > 0 && width + 1 + length <= columns {
                row.push(' ');
                row.push_str(word);
                width += 1 + length;
                continue;
            }
            if width > 0 {
                rows.push(std::mem::take(&mut row));
            }
            let mut rest = word;
            while let Some((at, _)) = rest.char_indices().nth(columns) {
                let Some((head, tail)) = rest.split_at_checked(at) else {
                    break;
                };
                rows.push(head.to_owned());
                rest = tail;
            }
            row.push_str(rest);
            width = rest.chars().count();
        }
        rows.push(row);
    }
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    rows
}

/// Keeps the first `keep` of `rows`, the last kept ending in an ellipsis
/// when any were dropped.
fn shorten(rows: &mut Vec<String>, keep: usize, columns: usize) {
    if rows.len() <= keep {
        return;
    }
    rows.truncate(keep);
    if let Some(last) = rows.last_mut() {
        if columns > 0 && last.chars().count() >= columns {
            last.pop();
        }
        last.push('…');
    }
}

#[cfg(test)]
mod tests;
