//! The model picker (DESIGN.md §4): td-ui's finder over the cached models
//! list, modal over the window's body, as td-mail's attach chooser is.
//! Typing filters by the words of an id; `Return`, or a second press on
//! the same row soon after the first, chooses the selected model;
//! `Escape` closes it with nothing chosen. A model td-agent cannot drive,
//! one that does not list `tools` or `max_tokens` (DESIGN.md §5), is shown
//! and cannot be chosen. The conversation's model is marked and selected
//! when it opens. Each row's meta is the model's price in dollars per
//! million prompt and completion tokens.
//!
//! The same finder, opened by `Picker::templates`, is the template
//! chooser of a new conversation (DESIGN.md §7): Empty, Directory… and
//! the configured templates in the order given, Empty selected.

use std::time::{Duration, Instant};

use td_ui::chrome::List;
use td_ui::finder::{self, Choice, Choose, Controller, Entry, Kind, Listing, Outcome};
use td_ui::raster::{Draw, Rect, Surface};
use td_ui::window::{Input, PointerPhase};

use crate::cost::{Pricing, ONE};
use crate::models::Model;

/// The finder's title row, choosing for the open conversation.
pub const TITLE: &str =
    "Model for this conversation: type to filter, Return chooses, Escape cancels";
/// Its title row, choosing the default.
pub const DEFAULT_TITLE: &str =
    "Default model for new conversations: type to filter, Return chooses, Escape cancels";
/// Its title row, choosing a new conversation's workspace template.
pub const TEMPLATE_TITLE: &str =
    "New conversation: choose its workspace; type to filter, Return chooses, Escape cancels";
/// How soon a second press on a row chooses it, as td-mail's finder does.
const DOUBLE_PRESS: Duration = Duration::from_millis(400);

/// One model as the picker offers it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Offer {
    pub id: String,
    pub price: String,
    /// Whether td-agent can drive it: it lists `tools` and `max_tokens`.
    pub usable: bool,
    /// Whether it takes a reasoning effort.
    pub reasoning: bool,
}

impl Offer {
    pub fn of(model: &Model) -> Self {
        Self {
            id: model.id.clone(),
            price: model.pricing.map(price).unwrap_or_default(),
            usable: model.supports("tools") && model.supports("max_tokens"),
            reasoning: model.supports("reasoning"),
        }
    }
}

/// A price per million prompt and completion tokens, `$3/$15`, or `free`;
/// none when it would not fit a finder row's meta.
fn price(pricing: Pricing) -> String {
    if pricing.prompt == 0 && pricing.completion == 0 {
        return "free".into();
    }
    // Pico-credits a token to thousandths of a dollar a million tokens,
    // rounded, and a price under one shown as one, never as free.
    let mills = |pico: u64| {
        let mills = (u128::from(pico) * 1_000_000_000 + u128::from(ONE) / 2) / u128::from(ONE);
        if pico > 0 {
            mills.max(1)
        } else {
            mills
        }
    };
    let dollars = |pico: u64| {
        let mills = mills(pico);
        let fraction = format!("{:03}", mills % 1000);
        match fraction.trim_end_matches('0') {
            "" => format!("${}", mills / 1000),
            digits => format!("${}.{digits}", mills / 1000),
        }
    };
    let text = format!(
        "{}/{}",
        dollars(pricing.prompt),
        dollars(pricing.completion)
    );
    if text.len() > finder::META_BYTES {
        String::new()
    } else {
        text
    }
}

/// The finder's entries for `rows`, each a name, its meta and whether it
/// can be chosen, `current` marked, and how many were left out: past the
/// finder's bound in number or bytes, or a row it refused.
fn entries<'a>(
    rows: impl ExactSizeIterator<Item = (&'a str, &'a str, bool)>,
    current: &str,
) -> (Vec<Entry>, usize) {
    let total = rows.len();
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    for (name, meta, enabled) in rows.take(finder::ENTRIES) {
        bytes = bytes.saturating_add(name.len() + meta.len());
        if let Ok(entry) = Entry::new(name, meta, Kind::File, enabled) {
            if bytes <= finder::LISTING_BYTES {
                entries.push(entry.with_marked(!current.is_empty() && name == current));
            }
        }
    }
    let left_out = total.saturating_sub(entries.len());
    (entries, left_out)
}

/// What the picker made of an input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    /// Still open; whether it needs a paint.
    Stay(bool),
    /// Closed with nothing chosen, or unable to go on.
    Closed,
    /// Closed on this model.
    Chosen(String),
}

#[derive(Debug)]
pub struct Picker {
    finder: Controller,
    surface: Surface,
    press: Option<(Instant, usize)>,
}

impl Picker {
    /// The picker over `offers` inside `rect`, sorted by id, `current`
    /// marked and selected. A model whose row the finder cannot hold is
    /// left out, and the status row says how many were.
    pub fn open(
        surface: Surface,
        rect: Rect,
        title: &str,
        offers: &[Offer],
        current: &str,
    ) -> Result<Self, String> {
        let mut sorted: Vec<&Offer> = offers.iter().collect();
        sorted.sort_by(|a, b| a.id.cmp(&b.id));
        let rows = sorted
            .into_iter()
            .map(|offer| (offer.id.as_str(), offer.price.as_str(), offer.usable));
        let (entries, left_out) = entries(rows, current);
        if entries.is_empty() {
            return Err("no models are known yet: the models list has not been fetched".into());
        }
        let note = if left_out > 0 {
            let models = if left_out == 1 { "model" } else { "models" };
            format!("{left_out} {models} left out; greyed models take no tools or max_tokens")
        } else {
            "greyed models take no tools or max_tokens, which td-agent sends".to_string()
        };
        Self::over(surface, rect, title, entries, left_out, current, &note)
            .map_err(|e| format!("the model picker: {e}"))
    }

    /// The picker over a new conversation's workspace templates, `rows`
    /// each a name and its meta in the order given, the first selected.
    pub fn templates(
        surface: Surface,
        rect: Rect,
        rows: &[(String, String)],
        note: &str,
    ) -> Result<Self, String> {
        let first = rows.first().map(|(name, _)| name.as_str()).unwrap_or("");
        let (entries, left_out) = entries(
            rows.iter()
                .map(|(name, meta)| (name.as_str(), meta.as_str(), true)),
            "",
        );
        Self::over(
            surface,
            rect,
            TEMPLATE_TITLE,
            entries,
            left_out,
            first,
            note,
        )
        .map_err(|e| format!("the template chooser: {e}"))
    }

    fn over(
        surface: Surface,
        rect: Rect,
        title: &str,
        entries: Vec<Entry>,
        left_out: usize,
        selected: &str,
        note: &str,
    ) -> Result<Self, String> {
        let listing = Listing::new(title, entries, left_out > 0).map_err(|e| e.to_string())?;
        let mut finder = Controller::new(listing, Choose::File, surface, rect, Some(selected))
            .map_err(|e| e.to_string())?;
        let _ = finder.set_note(note);
        Ok(Self {
            finder,
            surface,
            press: None,
        })
    }

    /// The models listed, in their order.
    pub fn listing(&self) -> &Listing {
        self.finder.listing()
    }

    /// The selected model, while one is shown.
    pub fn selected(&self) -> Option<&str> {
        self.finder.selected_entry().map(Entry::name)
    }

    /// The filter as typed.
    pub fn query(&self) -> &str {
        self.finder.query()
    }

    /// Lays the picker out in `rect` of `surface`; false when the finder
    /// cannot fit there, and it is to be closed.
    pub fn resize(&mut self, surface: Surface, rect: Rect) -> bool {
        self.surface = surface;
        !matches!(
            self.finder.event(finder::Event::Resize { surface, rect }),
            Outcome::Closed(_)
        )
    }

    /// An input, which is the picker's while it is open: its keys, every
    /// other chord consumed, the pointer and the wheel over its list. A
    /// resize, a focus change and a paste are the window's.
    pub fn input(&mut self, input: &Input<'_>) -> Reply {
        let key = |key, repeated| finder::Event::Key { key, repeated };
        let event = match *input {
            Input::Key { chord, repeat } => match chord {
                "Up" => key(finder::Key::Up, repeat),
                "Down" => key(finder::Key::Down, repeat),
                "PageUp" => key(finder::Key::PageUp, repeat),
                "PageDown" => key(finder::Key::PageDown, repeat),
                "Home" => key(finder::Key::Home, repeat),
                "End" => key(finder::Key::End, repeat),
                "Return" | "C-Return" => key(finder::Key::Activate, repeat),
                "Backspace" => key(finder::Key::Backspace, repeat),
                "Escape" => key(finder::Key::Escape, repeat),
                "Space" => finder::Event::Insert(' '),
                _ => {
                    let mut chars = chord.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) if !c.is_control() => finder::Event::Insert(c),
                        _ => finder::Event::Other,
                    }
                }
            },
            Input::Pointer { phase, x, y, .. } => match phase {
                PointerPhase::Press => finder::Event::Press { x, y },
                PointerPhase::Move => finder::Event::Move { x, y },
                PointerPhase::Release => finder::Event::Release { x, y },
            },
            Input::Wheel { rows, .. } => {
                let list = self.finder.list_rect();
                finder::Event::Wheel {
                    x: list.x,
                    y: list.y,
                    rows,
                }
            }
            Input::Resize(_)
            | Input::Focus(_)
            | Input::Paste(_)
            | Input::CancelPointer
            | Input::Hover(_)
            | Input::Context { .. }
            | Input::Close => return Reply::Stay(false),
        };
        // The entry a press lands on, by the listing's index; any other
        // input between two presses makes the second a first.
        let hit = match event {
            finder::Event::Press { x, y } => List::new(self.surface, self.finder.list_rect())
                .and_then(|rows| rows.hit(x, y))
                .and_then(|row| self.finder.first().checked_add(row))
                .and_then(|position| self.finder.shown().get(position).copied()),
            _ => None,
        };
        let mut outcome = self.finder.event(event);
        match event {
            finder::Event::Press { .. } => {
                let now = Instant::now();
                let again = hit.is_some()
                    && self.press.is_some_and(|(at, entry)| {
                        Some(entry) == hit && now.duration_since(at) <= DOUBLE_PRESS
                    });
                self.press = if again {
                    None
                } else {
                    hit.map(|entry| (now, entry))
                };
                if again {
                    outcome = self.finder.event(key(finder::Key::Activate, false));
                }
            }
            finder::Event::Move { .. } | finder::Event::Release { .. } => {}
            _ => self.press = None,
        }
        match outcome {
            Outcome::Ignored | Outcome::Consumed => Reply::Stay(false),
            // There are no folders: a descent or an ascent is nothing.
            Outcome::Descend(_) | Outcome::Ascend => Reply::Stay(false),
            Outcome::Changed => Reply::Stay(true),
            Outcome::Closed(Choice::Entry(index)) => {
                match self.finder.listing().entries().get(index) {
                    Some(entry) if entry.enabled() => Reply::Chosen(entry.name().to_string()),
                    _ => Reply::Closed,
                }
            }
            Outcome::Closed(Choice::Here | Choice::Cancelled | Choice::Unavailable(_)) => {
                Reply::Closed
            }
        }
    }

    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        self.finder.emit(damage, sink);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    use td_ui::raster::Scale;

    fn surface() -> Surface {
        Surface::new(1024, 640, Scale::default()).unwrap()
    }

    fn offer(id: &str, usable: bool) -> Offer {
        Offer {
            id: id.into(),
            price: "$3/$15".into(),
            usable,
            reasoning: true,
        }
    }

    fn offers() -> Vec<Offer> {
        vec![
            offer("openai/gpt-6", true),
            offer("anthropic/claude-sonnet-5.5", true),
            offer("amazon/nova-pro-v1", false),
            offer("anthropic/claude-haiku-4.5", true),
        ]
    }

    fn open(current: &str) -> Picker {
        Picker::open(surface(), surface().bounds(), TITLE, &offers(), current).unwrap()
    }

    fn key(picker: &mut Picker, chord: &str) -> Reply {
        picker.input(&Input::Key {
            chord,
            repeat: false,
        })
    }

    #[test]
    fn it_lists_every_model_by_id_the_current_one_marked_and_selected() {
        let picker = open("anthropic/claude-sonnet-5.5");
        let names: Vec<&str> = picker.listing().entries().iter().map(Entry::name).collect();
        assert_eq!(
            names,
            [
                "amazon/nova-pro-v1",
                "anthropic/claude-haiku-4.5",
                "anthropic/claude-sonnet-5.5",
                "openai/gpt-6"
            ]
        );
        assert_eq!(picker.selected(), Some("anthropic/claude-sonnet-5.5"));
        let marked: Vec<&str> = picker
            .listing()
            .entries()
            .iter()
            .filter(|e| e.marked())
            .map(Entry::name)
            .collect();
        assert_eq!(marked, ["anthropic/claude-sonnet-5.5"]);
        assert!(!picker.listing().entries()[0].enabled(), "no tools");
    }

    #[test]
    fn typing_filters_return_chooses_and_escape_cancels() {
        let mut picker = open("anthropic/claude-sonnet-5.5");
        for c in ["g", "p", "t"] {
            assert_eq!(key(&mut picker, c), Reply::Stay(true));
        }
        assert_eq!(picker.query(), "gpt");
        assert_eq!(picker.selected(), Some("openai/gpt-6"));
        assert_eq!(
            key(&mut picker, "Return"),
            Reply::Chosen("openai/gpt-6".into())
        );
        let mut picker = open("anthropic/claude-sonnet-5.5");
        assert_eq!(key(&mut picker, "Up"), Reply::Stay(true));
        assert_eq!(
            key(&mut picker, "Return"),
            Reply::Chosen("anthropic/claude-haiku-4.5".into())
        );
        let mut picker = open("anthropic/claude-sonnet-5.5");
        assert_eq!(key(&mut picker, "Escape"), Reply::Closed);
        // Other chords are the picker's, consumed.
        let mut picker = open("anthropic/claude-sonnet-5.5");
        assert_eq!(key(&mut picker, "C-n"), Reply::Stay(false));
    }

    #[test]
    fn a_model_td_agent_cannot_drive_is_not_chosen() {
        let mut picker = open("anthropic/claude-sonnet-5.5");
        for c in ["n", "o", "v", "a"] {
            key(&mut picker, c);
        }
        assert_eq!(picker.selected(), Some("amazon/nova-pro-v1"));
        assert!(!matches!(key(&mut picker, "Return"), Reply::Chosen(_)));
    }

    #[test]
    fn a_price_is_dollars_per_million_tokens_or_none_past_a_row() {
        let pricing = |prompt: u64, completion: u64| Pricing {
            prompt,
            completion,
            request: 0,
            reasoning: 0,
            cache_write: 0,
            cache_read: 0,
        };
        // 0.000003 and 0.000015 credits a token, in pico-credits.
        assert_eq!(price(pricing(3_000_000, 15_000_000)), "$3/$15");
        assert_eq!(price(pricing(250_000, 1_250_000)), "$0.25/$1.25");
        assert_eq!(price(pricing(100_000, 400_000)), "$0.1/$0.4");
        assert_eq!(price(pricing(1_500, 2)), "$0.002/$0.001");
        assert_eq!(price(pricing(0, 0)), "free");
        assert_eq!(price(pricing(123_456_789_000, 987_654_321_000)), "");
        assert!(Picker::open(surface(), surface().bounds(), TITLE, &[], "x").is_err());
        // Every model past the finder's bound is counted as left out.
        let many: Vec<Offer> = (0..finder::ENTRIES + 3)
            .map(|n| offer(&format!("m/{n:05}"), true))
            .collect();
        let picker = Picker::open(surface(), surface().bounds(), TITLE, &many, "m/00000").unwrap();
        assert_eq!(picker.listing().entries().len(), finder::ENTRIES);
        assert!(picker.listing().truncated());
        assert!(picker.finder.note().starts_with("3 models left out"));
    }
}
