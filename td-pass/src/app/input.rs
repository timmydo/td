//! The window's input: the key prompt takes everything while it is up,
//! then the dialog, then the notebook's shortcuts, then the focused
//! field, list or pane. Clipboard offers are made within the input that
//! asked for them, as the window requires.

use super::*;

/// A chord the dialog understands.
fn dialog_key(chord: &str) -> Option<confirmations::Key> {
    use confirmations::Key;
    Some(match chord {
        "Tab" => Key::Tab,
        "S-Tab" => Key::BackTab,
        "Up" => Key::Up,
        "Down" => Key::Down,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Home" => Key::Home,
        "End" => Key::End,
        "Return" | "Space" | " " => Key::Activate,
        "Escape" => Key::Escape,
        _ => return None,
    })
}

impl App {
    /// One input; whether the window should close is `quitting`.
    pub fn input(&mut self, input: Input<'_>, clipboard: &mut dyn Clipboard) {
        match input {
            Input::Resize(surface) => self.resize(surface),
            Input::Focus(focused) => {
                self.window_focused = focused;
                if !focused {
                    if let Some((dialog, _)) = &mut self.dialog {
                        let outcome = dialog.event(
                            Some(self.dialog_revision),
                            true,
                            confirmations::Event::FocusLost,
                        );
                        self.dialog_outcome(outcome);
                    }
                }
                self.sync_focus();
                self.redraw = true;
            }
            Input::Close => {
                // Closing declines a waiting prompt and replaces an open
                // question with the question about closing.
                if self.prompt.is_some() {
                    self.answer_prompt(false);
                }
                if self.dialog.take().is_some() {
                    self.sync_focus();
                }
                self.request(Then::Quit, None);
            }
            Input::Key { chord, repeat } => self.key(chord, repeat, clipboard),
            Input::Pointer {
                phase,
                x,
                y,
                extend,
                ..
            } => self.pointer(phase, x, y, extend),
            Input::CancelPointer => {
                self.drag = None;
                let _ = self.pane.dispatch(Event::CancelPointer);
            }
            Input::Wheel { rows, .. } => self.wheel(rows),
            Input::Hover(_) => {}
            Input::Paste(text) => self.pasted(text),
        }
        self.reveal_fields();
    }

    fn key(&mut self, chord: &str, repeat: bool, clipboard: &mut dyn Clipboard) {
        if self.prompt.is_some() {
            return self.prompt_key(chord, clipboard);
        }
        if let Some((dialog, _)) = &mut self.dialog {
            let event = match dialog_key(chord) {
                Some(key) => confirmations::Event::Key {
                    key,
                    repeated: repeat,
                },
                None => confirmations::Event::Other,
            };
            let outcome = dialog.event(Some(self.dialog_revision), true, event);
            return self.dialog_outcome(outcome);
        }
        if chord == "C-q" {
            return self.request(Then::Quit, None);
        }
        match self.phase {
            Phase::Locked { .. } => self.locked_key(chord),
            Phase::Unlocked(_) => self.notebook_key(chord, clipboard),
            _ => {}
        }
    }

    fn locked_key(&mut self, chord: &str) {
        let surface = self.surface;
        let Phase::Locked { list, .. } = &mut self.phase else {
            return;
        };
        if let (Some(step), Some(view)) = (Step::from_chord(chord), layout::keys(surface)) {
            if list.step(step, view).any() {
                self.redraw = true;
            }
            return;
        }
        if chord == "Return" {
            self.unlock();
        }
    }

    fn notebook_key(&mut self, chord: &str, clipboard: &mut dyn Clipboard) {
        match chord {
            "C-s" => return self.save(None),
            "C-n" => return self.request(Then::New, None),
            "C-l" => return self.request(Then::Lock, None),
            "F2" => return self.rename(),
            "C-f" => return self.open_find(),
            "F6" => return self.cycle(true),
            "S-F6" => return self.cycle(false),
            _ => {}
        }
        match self.focus {
            Focus::Search => match chord {
                "Return" | "Down" | "Tab" => self.set_focus(Focus::List),
                "Escape" => {
                    if let Some(notebook) = self.notebook() {
                        notebook.search.clear();
                    }
                    self.refilter(None);
                }
                _ => {
                    if self.field_key(Focus::Search, chord, clipboard) {
                        self.refilter(None);
                    }
                }
            },
            Focus::List => self.list_key(chord),
            Focus::Title => match chord {
                "Return" | "Tab" => self.set_focus(Focus::Editor),
                "S-Tab" => self.set_focus(Focus::List),
                "Escape" => {
                    if let Some(notebook) = self.notebook() {
                        if let Some(open) = &notebook.open {
                            let _ = notebook.title.set_text(open.saved_title.as_str());
                        }
                    }
                    self.redraw = true;
                }
                _ => {
                    self.field_key(Focus::Title, chord, clipboard);
                }
            },
            Focus::Find => match chord {
                "Return" => self.find(false),
                "S-Return" => self.find(true),
                "Escape" => self.close_find(),
                _ => {
                    self.field_key(Focus::Find, chord, clipboard);
                }
            },
            Focus::Editor => self.pane_key(chord, clipboard),
            Focus::Keys => {}
        }
    }

    fn list_key(&mut self, chord: &str) {
        match chord {
            "Return" => return self.set_focus(Focus::Editor),
            "Tab" => return self.set_focus(Focus::Title),
            "S-Tab" | "Escape" => return self.set_focus(Focus::Search),
            "Delete" => return self.delete(None),
            _ => {}
        }
        let surface = self.surface;
        let Some(notebook) = self.notebook() else {
            return;
        };
        let (Some(step), Some(list)) = (
            Step::from_chord(chord),
            layout::panes(surface, notebook.finding).list,
        ) else {
            return;
        };
        let before = notebook.list.selected();
        let change = notebook.list.step(step, list);
        if change.any() {
            self.redraw = true;
        }
        if let Some(index) = self.selected_entry() {
            if before != self.notebook().and_then(|n| n.list.selected()) {
                self.choose(index, None);
            }
        }
    }

    /// The entry the list selects, as an index into `entries`.
    fn selected_entry(&self) -> Option<usize> {
        let Phase::Unlocked(notebook) = &self.phase else {
            return None;
        };
        notebook.shown.get(notebook.list.selected()?).copied()
    }

    /// Opens the entry at `index` in `entries`, asking first about the
    /// open entry's unsaved changes; the list keeps showing the open
    /// entry until the choice is made.
    fn choose(&mut self, index: usize, opener: Option<(i64, i64)>) {
        let open = self
            .notebook()
            .and_then(|notebook| notebook.open.as_ref().and_then(|open| open.id));
        let chosen = self
            .notebook()
            .and_then(|notebook| notebook.entries.get(index).map(|item| item.id));
        if chosen.is_some() && chosen == open {
            return;
        }
        if self.dirty() || self.busy.is_some() {
            // The list goes back to the open entry while the choice waits.
            self.refilter(open);
            if self.busy.is_some() {
                return self.say("Wait for the current operation to finish");
            }
        }
        self.request(Then::Select(index), opener);
    }

    /// A key for one of the notebook's text fields; whether it changed the
    /// text.
    fn field_key(&mut self, focus: Focus, chord: &str, clipboard: &mut dyn Clipboard) -> bool {
        match chord {
            "C-c" | "C-x" => {
                let Some(field) = self.field(focus) else {
                    return false;
                };
                let taken = if chord == "C-c" {
                    field.copy()
                } else {
                    field.cut()
                };
                match taken {
                    Ok(Some(text)) => {
                        let cut = chord == "C-x";
                        self.offer(text, clipboard);
                        self.redraw = true;
                        return cut;
                    }
                    Ok(None) => {}
                    Err(refusal) => self.say(refusal.to_string()),
                }
                false
            }
            "C-v" => {
                self.ask_paste(Target::Field(focus), clipboard);
                false
            }
            _ => {
                let Some(action) = Action::from_chord(chord) else {
                    return false;
                };
                let Some(field) = self.field(focus) else {
                    return false;
                };
                match field.act(action) {
                    Ok(Typed::Changed) => {
                        self.redraw = true;
                        true
                    }
                    Ok(Typed::Moved) => {
                        self.redraw = true;
                        false
                    }
                    Ok(Typed::Ignored) => false,
                    Err(refusal) => {
                        self.say(refusal.to_string());
                        false
                    }
                }
            }
        }
    }

    fn field(&mut self, focus: Focus) -> Option<&mut EntryModel> {
        let notebook = self.notebook()?;
        match focus {
            Focus::Search => Some(&mut notebook.search),
            Focus::Title => Some(&mut notebook.title),
            Focus::Find => Some(&mut notebook.find),
            _ => None,
        }
    }

    fn pane_key(&mut self, chord: &str, clipboard: &mut dyn Clipboard) {
        let Some((tab, revision)) = self.open_tab() else {
            return;
        };
        match self.pane.dispatch(Event::Key {
            tab,
            revision,
            chord,
        }) {
            Ok(Outcome::Changed | Outcome::Prefix) => self.redraw = true,
            Ok(Outcome::Request { name, .. }) => match name {
                "save" => self.save(None),
                "copy" => self.pane_copy(false, clipboard),
                "cut" => self.pane_copy(true, clipboard),
                "paste" => self.ask_paste(Target::Pane, clipboard),
                "find" => self.open_find(),
                "find-next" => self.find(false),
                "find-previous" => self.find(true),
                "quit" => self.request(Then::Quit, None),
                _ => self.say("That command is not part of the notebook"),
            },
            Ok(_) => {}
            Err(error) => self.say(error.to_string()),
        }
    }

    /// Copies or cuts exactly the selection, with the entry's own line
    /// endings.
    fn pane_copy(&mut self, cut: bool, clipboard: &mut dyn Clipboard) {
        let Some((tab, revision)) = self.open_tab() else {
            return;
        };
        let snapshot = match Snapshot::capture_selection(self.pane.editor(), tab, revision) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return,
            Err(error) => return self.say(error.to_string()),
        };
        let text = snapshot.text();
        if cut {
            match self.pane.dispatch(Event::Cut(snapshot)) {
                Ok(Outcome::Changed) => self.redraw = true,
                Ok(_) => return,
                Err(error) => return self.say(error.to_string()),
            }
        }
        self.offer(text, clipboard);
    }

    fn offer(&mut self, text: Arc<str>, clipboard: &mut dyn Clipboard) {
        if let Err(refusal) = clipboard.copy(text) {
            self.say(refusal.to_string());
        }
    }

    fn ask_paste(&mut self, target: Target, clipboard: &mut dyn Clipboard) {
        match clipboard.paste() {
            Ok(()) => self.paste = Some(target),
            Err(refusal) => self.say(refusal.to_string()),
        }
    }

    /// The clipboard's text, for the place that asked for it if it is
    /// still there.
    fn pasted(&mut self, text: &str) {
        let Some(target) = self.paste.take() else {
            return;
        };
        match target {
            Target::Pin => {
                if let Some(prompt) = &mut self.prompt {
                    if let Err(refusal) = prompt.pin.paste(text) {
                        self.say(refusal.to_string());
                    }
                }
            }
            Target::Field(focus) => {
                let Some(field) = self.field(focus) else {
                    return;
                };
                if let Err(refusal) = field.paste(text) {
                    return self.say(refusal.to_string());
                }
                if focus == Focus::Search {
                    self.refilter(None);
                }
            }
            Target::Pane => {
                let Some((tab, revision)) = self.open_tab() else {
                    return;
                };
                let paste =
                    Paste::begin(self.pane.editor(), tab, revision).and_then(|mut paste| {
                        paste.push(text.as_bytes())?;
                        Ok(paste)
                    });
                match paste.and_then(|paste| self.pane.dispatch(Event::Paste(paste))) {
                    Ok(_) => {}
                    Err(error) => self.say(error.to_string()),
                }
            }
        }
        self.redraw = true;
    }

    fn rename(&mut self) {
        let open = self
            .notebook()
            .is_some_and(|notebook| notebook.open.is_some());
        if open {
            self.set_focus(Focus::Title);
            if let Some(notebook) = self.notebook() {
                let _ = notebook.title.act(Action::SelectAll);
            }
            self.say("Rename: edit the title, then Save");
        } else {
            self.say("No entry is open");
        }
    }

    fn open_find(&mut self) {
        if self.open_tab().is_none() {
            return self.say("No entry is open");
        }
        if let Some(notebook) = self.notebook() {
            notebook.finding = true;
        }
        self.set_focus(Focus::Find);
        self.relayout();
    }

    fn close_find(&mut self) {
        if let Some(notebook) = self.notebook() {
            notebook.finding = false;
            notebook.find.clear();
        }
        self.set_focus(Focus::Editor);
        self.relayout();
    }

    /// Finds the find field's text, or the last query, in the entry.
    fn find(&mut self, backward: bool) {
        let Some((tab, revision)) = self.open_tab() else {
            return;
        };
        let query = self
            .notebook()
            .map(|notebook| Text::new(notebook.find.text().to_owned()))
            .filter(|query| !query.as_str().is_empty())
            .unwrap_or_else(|| Text::new(self.history.query().to_owned()));
        if query.as_str().is_empty() {
            return self.open_find();
        }
        self.history.observe(self.pane.editor());
        let found = self
            .history
            .find(&mut self.pane, tab, revision, query.as_str(), backward);
        self.say(match found {
            Ok(Found::Match) => "",
            Ok(Found::Wrapped) => "Found, wrapping past the end",
            Ok(Found::End) => "No more matches; find again to wrap",
            Ok(Found::Missing) => "Not found",
            Err(error) => return self.say(error.to_string()),
        });
    }

    fn cycle(&mut self, forward: bool) {
        let finding = self.notebook().is_some_and(|notebook| notebook.finding);
        let mut order = vec![Focus::Search, Focus::List, Focus::Title, Focus::Editor];
        if finding {
            order.push(Focus::Find);
        }
        let at = order.iter().position(|&f| f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % order.len()
        } else {
            (at + order.len() - 1) % order.len()
        };
        if let Some(&focus) = order.get(next) {
            self.set_focus(focus);
        }
    }

    fn prompt_key(&mut self, chord: &str, clipboard: &mut dyn Clipboard) {
        match chord {
            "Return" => self.answer_prompt(true),
            "Escape" => self.answer_prompt(false),
            "C-v" => {
                if self.prompt.as_ref().is_some_and(|p| p.ask.pin.is_some()) {
                    self.ask_paste(Target::Pin, clipboard);
                }
            }
            _ => {
                let Some(prompt) = &mut self.prompt else {
                    return;
                };
                if prompt.ask.pin.is_none() {
                    return;
                }
                if let Some(action) = Action::from_chord(chord) {
                    if let Ok(Typed::Changed | Typed::Moved) = prompt.pin.act(action) {
                        self.redraw = true;
                    }
                }
            }
        }
    }

    /// Answers the prompt: `proceed` gives the PIN or goes on with the
    /// connected key; otherwise the operation is declined.
    fn answer_prompt(&mut self, proceed: bool) {
        let Some(mut prompt) = self.prompt.take() else {
            return;
        };
        let answer = match (proceed, prompt.ask.pin) {
            (false, _) => Answer::Decline,
            (true, None) => Answer::Proceed,
            (true, Some(_)) if prompt.pin.text().is_empty() => {
                self.prompt = Some(prompt);
                return self.say("Type the key's PIN, or Cancel");
            }
            (true, Some(_)) => {
                let pin = Bytes::copy(prompt.pin.text().as_bytes());
                prompt.pin.clear();
                Answer::Pin(pin)
            }
        };
        let declined = matches!(answer, Answer::Decline);
        self.out.push(Out::Answer(prompt.op, answer));
        if self.paste == Some(Target::Pin) {
            self.paste = None;
        }
        self.say(if declined {
            "Cancelling"
        } else if prompt.ask.pin.is_some() {
            "Touch the key if it asks"
        } else {
            "Opening the key"
        });
    }

    pub(super) fn dialog_outcome(&mut self, outcome: confirmations::Outcome<Act, Focus>) {
        match outcome {
            confirmations::Outcome::Ignored | confirmations::Outcome::Consumed => {}
            confirmations::Outcome::Changed => self.redraw = true,
            confirmations::Outcome::Closed { choice, .. } => {
                let then = self.dialog.take().and_then(|(_, then)| then);
                self.sync_focus();
                self.redraw = true;
                match (choice, then) {
                    (Choice::Confirmed(Act::Delete), _) => self.delete_now(),
                    (Choice::Confirmed(Act::Save), Some(then)) => self.save(Some(then)),
                    (Choice::Confirmed(Act::Discard), Some(then)) => {
                        self.discard();
                        self.run(then);
                    }
                    (Choice::Unavailable(error), _) => self.say(error.to_string()),
                    _ => {}
                }
            }
        }
    }

    fn wheel(&mut self, rows: isize) {
        if self.prompt.is_some() {
            return;
        }
        let surface = self.surface;
        let pointer = self.pointer;
        if let Some((dialog, _)) = &mut self.dialog {
            let (x, y) = pointer.unwrap_or_default();
            let outcome = dialog.event(
                Some(self.dialog_revision),
                true,
                confirmations::Event::Wheel { x, y, rows },
            );
            return self.dialog_outcome(outcome);
        }
        match &mut self.phase {
            Phase::Unlocked(notebook) => {
                let panes = layout::panes(surface, notebook.finding);
                let over_list = pointer.is_some_and(|(x, y)| {
                    panes.list.is_some_and(|list| list.rect().contains(x, y))
                });
                if over_list {
                    if let Some(list) = panes.list {
                        if notebook.list.scroll(rows as i64, list).any() {
                            self.redraw = true;
                        }
                    }
                    return;
                }
                if let Some((tab, revision)) = self.open_tab() {
                    if let Ok(Outcome::Changed) = self.pane.dispatch(Event::Scroll {
                        tab,
                        revision,
                        rows,
                        columns: 0,
                    }) {
                        self.redraw = true;
                    }
                }
            }
            Phase::Locked { list, .. } => {
                if let Some(view) = layout::keys(surface) {
                    if list.scroll(rows as i64, view).any() {
                        self.redraw = true;
                    }
                }
            }
            _ => {}
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) {
        self.pointer = Some((x, y));
        if self.prompt.is_some() {
            return self.prompt_pointer(phase, x, y, extend);
        }
        if let Some((dialog, _)) = &mut self.dialog {
            let event = match phase {
                PointerPhase::Press => confirmations::Event::Press { x, y },
                PointerPhase::Move => confirmations::Event::Move { x, y },
                PointerPhase::Release => confirmations::Event::Release { x, y },
            };
            let outcome = dialog.event(Some(self.dialog_revision), true, event);
            return self.dialog_outcome(outcome);
        }
        match (phase, self.drag) {
            (PointerPhase::Press, _) => self.press(x, y, extend),
            (_, Some(Drag::Pane)) => {
                self.pane_pointer(phase, x, y, extend);
                if phase == PointerPhase::Release {
                    self.drag = None;
                }
            }
            (PointerPhase::Move, Some(Drag::Field(focus))) => {
                let surface = self.surface;
                let finding = self.notebook().is_some_and(|n| n.finding);
                let panes = layout::panes(surface, finding);
                let entry = match focus {
                    Focus::Search => panes.search,
                    Focus::Title => panes.title,
                    Focus::Find => panes.find,
                    _ => None,
                };
                if let (Some(entry), Some(field)) = (entry, self.field(focus)) {
                    if field.drag(entry, x, y) != Typed::Ignored {
                        self.redraw = true;
                    }
                }
            }
            (PointerPhase::Release, _) => self.drag = None,
            _ => {}
        }
    }

    fn press(&mut self, x: i64, y: i64, extend: bool) {
        let surface = self.surface;
        let opener = Some((x, y));
        match &self.phase {
            Phase::Locked { .. } => {
                match layout::strip(surface, &layout::LOCKED).hit(x, y) {
                    Some(0) => return self.unlock(),
                    Some(1) => return self.create(),
                    _ => {}
                }
                let Phase::Locked { list, .. } = &mut self.phase else {
                    return;
                };
                if let Some(view) = layout::keys(surface) {
                    if list.press(view, x, y).is_some() {
                        self.redraw = true;
                    }
                }
            }
            Phase::Unlocked(notebook) => {
                match layout::strip(surface, &layout::NOTEBOOK).hit(x, y) {
                    Some(0) => return self.request(Then::New, opener),
                    Some(1) => return self.rename(),
                    Some(2) => return self.delete(opener),
                    Some(3) => return self.save(None),
                    Some(4) => return self.open_find(),
                    Some(5) => return self.request(Then::Lock, opener),
                    _ => {}
                }
                let panes = layout::panes(surface, notebook.finding);
                if panes.pane.contains(x, y) {
                    self.set_focus(Focus::Editor);
                    self.drag = Some(Drag::Pane);
                    return self.pane_pointer(PointerPhase::Press, x, y, extend);
                }
                for (focus, entry) in [
                    (Focus::Search, panes.search),
                    (Focus::Title, panes.title),
                    (Focus::Find, panes.find),
                ] {
                    let Some(entry) = entry.filter(|entry| entry.rect().contains(x, y)) else {
                        continue;
                    };
                    self.set_focus(focus);
                    if let Some(field) = self.field(focus) {
                        field.place(entry, x, y, extend);
                    }
                    self.drag = Some(Drag::Field(focus));
                    self.redraw = true;
                    return;
                }
                let Some(list) = panes.list.filter(|list| list.rect().contains(x, y)) else {
                    return;
                };
                self.set_focus(Focus::List);
                let Some(notebook) = self.notebook() else {
                    return;
                };
                let before = notebook.list.selected();
                if notebook.list.press(list, x, y).is_some() {
                    self.redraw = true;
                    let after = self.notebook().and_then(|n| n.list.selected());
                    if let Some(index) = self.selected_entry().filter(|_| after != before) {
                        self.choose(index, opener);
                    }
                }
            }
            _ => {}
        }
    }

    fn pane_pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) {
        let Some((tab, revision)) = self.open_tab() else {
            return;
        };
        let phase = match phase {
            PointerPhase::Press => PanePhase::Press,
            PointerPhase::Move => PanePhase::Move,
            PointerPhase::Release => PanePhase::Release,
        };
        if let Ok(Outcome::Changed) = self.pane.dispatch(Event::Pointer {
            tab,
            revision,
            phase,
            x,
            cell_x: x,
            y,
            extend,
        }) {
            self.redraw = true;
        }
    }

    fn prompt_pointer(&mut self, phase: PointerPhase, x: i64, y: i64, extend: bool) {
        let Some(prompt) = &mut self.prompt else {
            return;
        };
        let view = layout::prompt(self.surface, prompt.ask.pin.is_some());
        match phase {
            PointerPhase::Press => {
                if let Some(entry) = view.pin.filter(|entry| entry.rect().contains(x, y)) {
                    prompt.pin.place(entry, x, y, extend);
                    self.drag = Some(Drag::Pin);
                    self.redraw = true;
                    return;
                }
                match view.buttons.hit(x, y) {
                    Some(0) => self.answer_prompt(true),
                    Some(1) => self.answer_prompt(false),
                    _ => {}
                }
            }
            PointerPhase::Move => {
                if let (Some(Drag::Pin), Some(entry)) = (self.drag, view.pin) {
                    if prompt.pin.drag(entry, x, y) != Typed::Ignored {
                        self.redraw = true;
                    }
                }
            }
            PointerPhase::Release => self.drag = None,
        }
    }

    /// Keeps each field's caret in view after an input.
    fn reveal_fields(&mut self) {
        let surface = self.surface;
        if let Some(prompt) = &mut self.prompt {
            if let Some(entry) = layout::prompt(surface, true).pin {
                prompt.pin.reveal(entry);
            }
        }
        if let Phase::Unlocked(notebook) = &mut self.phase {
            let panes = layout::panes(surface, notebook.finding);
            for (model, field) in [
                (&mut notebook.search, panes.search),
                (&mut notebook.title, panes.title),
                (&mut notebook.find, panes.find),
            ] {
                if let Some(field) = field {
                    model.reveal(field);
                }
            }
        }
    }
}
