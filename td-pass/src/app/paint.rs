//! The frame: the action strip, the locked view, a copy's keys, the
//! notebook's panes or its keys, the status row, and the finder, prompt
//! or dialog over them. Titles
//! and entry text are painted only while unlocked; a lock clears what
//! holds them before the next frame.

use td_ui::chrome::{Item, Status};
use td_ui::raster::{
    Composition, Draw, GlyphStyle, Primitive, Rect, Surface, BORDER, CHROME, INK, PAPER,
};

use super::layout;
use super::{App, Focus, Phase};

pub struct Frame<'a> {
    pub app: &'a App,
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}

/// One line of text from a cell into `rect`, cut at its right edge.
fn line(
    surface: Surface,
    rect: Rect,
    text: &str,
    background: u32,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    fill(rect, background, damage, sink);
    let Some(clip) = rect.intersection(damage) else {
        return;
    };
    let cell = layout::cell(surface);
    let y = rect.y + (i64::from(rect.height) - layout::glyph_height(surface)) / 2;
    let style = GlyphStyle::medium(INK, background);
    let columns = (i64::from(rect.width) / cell - 2).max(0) as usize;
    for (column, scalar) in text.chars().take(columns).enumerate() {
        sink(Draw {
            clip,
            primitive: Primitive::Glyph {
                x: rect.x + cell * (column as i64 + 1),
                y,
                scalar: if scalar.is_control() { ' ' } else { scalar },
                style,
            },
        });
    }
}

impl Composition for Frame<'_> {
    fn surface(&self) -> Surface {
        self.app.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let surface = app.surface;
        fill(surface.bounds(), PAPER, damage, sink);
        match &app.phase {
            Phase::Unlocked(notebook) if notebook.keys.showing => {
                self.keys(&notebook.keys, damage, sink);
            }
            Phase::Unlocked(notebook) => {
                let open = notebook.open.is_some();
                let saved = notebook.open.as_ref().is_some_and(|open| open.id.is_some());
                let idle = app.busy.is_none();
                let states = [
                    idle,
                    open,
                    idle && saved,
                    idle && open,
                    app.open_tab().is_some(),
                    true,
                    true,
                ];
                layout::strip(surface, &layout::NOTEBOOK).emit(
                    states.map(|enabled| (false, enabled)),
                    damage,
                    sink,
                );
                self.notebook(notebook, damage, sink);
            }
            Phase::Locked { keys, list } => {
                let idle = app.busy.is_none();
                layout::strip(surface, &layout::LOCKED).emit(
                    [
                        (false, idle && keys.is_some()),
                        (false, idle && keys.is_none()),
                        (false, idle && keys.is_none()),
                    ],
                    damage,
                    sink,
                );
                let body = layout::body(surface, &layout::LOCKED);
                let row = layout::row(surface);
                let message = match keys {
                    Some(_) => "The notebook is locked. Unlock it with one of its keys:",
                    None => "This account has no notebook yet.",
                };
                line(
                    surface,
                    Rect {
                        height: row as u32,
                        ..body
                    },
                    message,
                    PAPER,
                    damage,
                    sink,
                );
                // Said before the first editing session, as the design asks.
                let host = if app.unwatched {
                    "The screen lock is not watched here: lock the notebook before leaving it."
                } else if !app.host_ready {
                    "Starting to watch the screen lock; unlocking waits for it."
                } else {
                    "A screen lock or sleep locks the notebook too, giving up unsaved edits."
                };
                line(
                    surface,
                    Rect {
                        y: body.y + row,
                        height: row as u32,
                        ..body
                    },
                    host,
                    PAPER,
                    damage,
                    sink,
                );
                if let (Some(keys), Some(view)) = (keys, layout::keys(surface)) {
                    let labels: Vec<String> = keys
                        .iter()
                        .map(|key| format!("{} key {}", key.role.name(), key.fingerprint))
                        .collect();
                    let window = list.window(view);
                    list.emit(
                        view,
                        labels
                            .get(window)
                            .unwrap_or_default()
                            .iter()
                            .map(|label| Item {
                                label,
                                meta: "",
                                enabled: true,
                                marked: false,
                            }),
                        damage,
                        sink,
                    );
                }
            }
            Phase::Importing { keys, list } => {
                let idle = app.busy.is_none();
                layout::strip(surface, &layout::IMPORT).emit(
                    [(false, idle && list.selected().is_some()), (false, true)],
                    damage,
                    sink,
                );
                let body = layout::body(surface, &layout::IMPORT);
                line(
                    surface,
                    Rect {
                        height: layout::row(surface) as u32,
                        ..body
                    },
                    "Import this encrypted copy as this account's notebook, with one of its keys:",
                    PAPER,
                    damage,
                    sink,
                );
                if let Some(view) = layout::copy_keys(surface) {
                    let labels: Vec<String> = keys
                        .iter()
                        .map(|key| format!("{} key {}", key.role.name(), key.fingerprint))
                        .collect();
                    let window = list.window(view);
                    list.emit(
                        view,
                        labels
                            .get(window)
                            .unwrap_or_default()
                            .iter()
                            .map(|label| Item {
                                label,
                                meta: "",
                                enabled: true,
                                marked: false,
                            }),
                        damage,
                        sink,
                    );
                }
            }
            Phase::Opening | Phase::Swap(_) | Phase::Locking | Phase::Refused(_) => {
                layout::strip(surface, &layout::LOCKED).emit(
                    [(false, false), (false, false), (false, false)],
                    damage,
                    sink,
                );
                if let Phase::Refused(text) = &app.phase {
                    let body = layout::body(surface, &layout::LOCKED);
                    line(
                        surface,
                        Rect {
                            height: layout::row(surface) as u32,
                            ..body
                        },
                        text,
                        PAPER,
                        damage,
                        sink,
                    );
                }
            }
        }
        if let Some(finder) = app.chooser.as_ref().and_then(|c| c.finder.as_ref()) {
            finder.emit(damage, sink);
        }
        Status::new(surface).emit(app.status.chars(), damage, sink);
        if let Some(prompt) = &app.prompt {
            let view = layout::prompt(surface, prompt.ask.pin.is_some());
            fill(view.rect, BORDER, damage, sink);
            let s = surface.scale.value() as i64;
            let inner = Rect {
                x: view.rect.x + s,
                y: view.rect.y + s,
                width: view.rect.width.saturating_sub(2 * s as u32),
                height: view.rect.height.saturating_sub(2 * s as u32),
            };
            fill(inner, CHROME, damage, sink);
            line(
                surface,
                view.title,
                prompt.ask.operation,
                CHROME,
                damage,
                sink,
            );
            line(
                surface,
                view.line,
                &super::asking(&prompt.ask),
                CHROME,
                damage,
                sink,
            );
            if let Some(entry) = view.pin {
                entry.emit(
                    prompt
                        .pin
                        .field("PIN", app.window_focused, app.caret_visible),
                    damage,
                    sink,
                );
            }
            view.buttons
                .emit([(true, true), (false, true)], damage, sink);
        }
        if let Some((dialog, _)) = &app.dialog {
            dialog.emit(damage, sink);
        }
    }
}

impl Frame<'_> {
    fn keys(&self, view: &super::KeyView, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let surface = app.surface;
        let idle = app.busy.is_none();
        let selected = view.list.selected();
        let count = view.keys.labels.len();
        layout::strip(surface, &layout::KEYS).emit(
            [
                (false, true),
                (
                    false,
                    idle && selected.is_some() && selected != view.keys.using,
                ),
                (false, idle),
                (false, idle && count > 1),
                (false, idle),
                (false, true),
            ],
            damage,
            sink,
        );
        let body = layout::body(surface, &layout::KEYS);
        let row = layout::row(surface);
        for (index, text) in [
            "The keys that open this notebook. Each one can unlock it alone.",
            "Space marks keys for Replace; Use for saves picks the key a save asks for.",
        ]
        .into_iter()
        .enumerate()
        {
            line(
                surface,
                Rect {
                    y: body.y + index as i64 * row,
                    height: row as u32,
                    ..body
                },
                text,
                PAPER,
                damage,
                sink,
            );
        }
        let Some(list) = layout::enrolled(surface) else {
            return;
        };
        let labels: Vec<String> = view
            .keys
            .labels
            .iter()
            .map(|key| format!("{} key {}", key.role.name(), key.fingerprint))
            .collect();
        let window = view.list.window(list);
        let first = window.start;
        view.list.emit(
            list,
            labels
                .get(window)
                .unwrap_or_default()
                .iter()
                .enumerate()
                .map(|(offset, label)| {
                    let index = first + offset;
                    Item {
                        label,
                        meta: if view.keys.using == Some(index) {
                            "authorizes saves"
                        } else {
                            ""
                        },
                        enabled: true,
                        marked: view.marked.get(index).copied().unwrap_or(false),
                    }
                }),
            damage,
            sink,
        );
    }

    fn notebook(&self, notebook: &super::Notebook, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let surface = app.surface;
        let panes = layout::panes(surface, notebook.finding);
        let caret = |focus: Focus| {
            app.window_focused && app.focus == focus && app.prompt.is_none() && app.dialog.is_none()
        };
        if let Some(entry) = panes.search {
            entry.emit(
                notebook
                    .search
                    .field("Search titles", caret(Focus::Search), app.caret_visible),
                damage,
                sink,
            );
        }
        if let Some(list) = panes.list {
            let open = notebook.open.as_ref().and_then(|open| open.id);
            let dirty = app.dirty();
            let window = notebook.list.window(list);
            let items = notebook
                .shown
                .get(window)
                .unwrap_or_default()
                .iter()
                .filter_map(|&index| notebook.entries.get(index))
                .map(|item| Item {
                    label: item.title.as_str(),
                    meta: "",
                    enabled: true,
                    marked: dirty && Some(item.id) == open,
                });
            notebook.list.emit(list, items, damage, sink);
        }
        fill(panes.divider, BORDER, damage, sink);
        if let Some(entry) = panes.title {
            let placeholder = if notebook.open.is_some() { "Title" } else { "" };
            entry.emit(
                notebook
                    .title
                    .field(placeholder, caret(Focus::Title), app.caret_visible),
                damage,
                sink,
            );
        }
        if app.open_tab().is_some() {
            if let Ok(scene) = app.pane.scene(&[]) {
                scene.emit(damage, sink);
            }
        } else {
            let text = if notebook.open.is_some() {
                "This entry's text cannot be shown here."
            } else if notebook.entries.is_empty() {
                "The notebook is empty. New starts an entry."
            } else {
                "Choose an entry, or New to start one."
            };
            line(
                surface,
                Rect {
                    height: layout::row(surface) as u32,
                    ..panes.pane
                },
                text,
                PAPER,
                damage,
                sink,
            );
        }
        if let Some(entry) = panes.find {
            entry.emit(
                notebook
                    .find
                    .field("Find in this entry", caret(Focus::Find), app.caret_visible),
                damage,
                sink,
            );
        }
    }
}
