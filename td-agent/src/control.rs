//! The window through td-ui's driven seam (td-ui/DESIGN.md, "The semantic
//! seam"; DESIGN.md §4): the same key and pointer paths the window's own
//! inputs take, its state as tab-separated facts, and what it shows as
//! a composition, so an agent or a test can operate it. The window
//! serves it on a control socket when started with one.

use td_ui::control::{Error, ErrorCode};
use td_ui::driven::{self, Binding, Controller, Input, PointerPhase};
use td_ui::raster::{Composition, Scale, Surface};
use td_ui::window::{self, NoClipboard};

use crate::ui::App;
#[cfg(test)]
use crate::ui::Request;

#[derive(Clone, Copy, Debug)]
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

/// The window's actions, each its default chord; `set-key`, `model`,
/// `default-model`, `export-diagnostics` and `delete-conversation` have
/// none, and are menu items.
pub const BINDINGS: &[Binding] = &[
    Binding {
        name: "new",
        chord: Some("C-n"),
        arguments: "",
        help: "File > New conversation...: open the template chooser (Return chooses, Escape cancels), or, when no workspace can be made, start a conversation with none.",
    },
    Binding {
        name: "messages",
        chord: Some("C-S-m"),
        arguments: "",
        help: "File > Messages...: open the Messages window, td-agent's notes whole and with their times; Escape closes it.",
    },
    Binding {
        name: "workspace",
        chord: Some("C-S-w"),
        arguments: "",
        help: "Conversation > Workspace card...: open the open conversation's repository workspace card, its project instructions and checkouts; Escape closes it.",
    },
    Binding {
        name: "previous",
        chord: Some("C-PageUp"),
        arguments: "",
        help: "Open the conversation above the open one.",
    },
    Binding {
        name: "next",
        chord: Some("C-PageDown"),
        arguments: "",
        help: "Open the conversation below the open one.",
    },
    Binding {
        name: "send",
        chord: Some("C-Return"),
        arguments: "",
        help: "Send the composer's text, as Return in the composer does; S-Return is a newline.",
    },
    Binding {
        name: "retry",
        chord: Some("C-r"),
        arguments: "",
        help: "Ask the open conversation's failed turn again.",
    },
    Binding {
        name: "interrupt",
        chord: Some("Escape"),
        arguments: "",
        help: "Interrupt the open conversation's running turn.",
    },
    Binding {
        name: "pause",
        chord: Some("C-S-p"),
        arguments: "",
        help: "Pause the open conversation, or resume it: paused, messages from other conversations wait.",
    },
    Binding {
        name: "todo",
        chord: Some("C-t"),
        arguments: "",
        help: "Show the open conversation's whole todo list, or collapse it to the item in progress.",
    },
    Binding {
        name: "clear-todo",
        chord: Some("C-S-t"),
        arguments: "",
        help: "Clear the open conversation's todo list.",
    },
    Binding {
        name: "focus-next",
        chord: Some("F6"),
        arguments: "",
        help: "Move the focus: list, transcript, composer.",
    },
    Binding {
        name: "focus-previous",
        chord: Some("S-F6"),
        arguments: "",
        help: "Move the focus back.",
    },
    Binding {
        name: "menu",
        chord: Some("F10"),
        arguments: "",
        help: "Open the File menu, or close it: arrows move, Return chooses, Escape closes.",
    },
    Binding {
        name: "row-menu",
        chord: Some("S-F10"),
        arguments: "",
        help: "Open a row's menu, as a right press on a row does: the selected row's while the list has the keyboard, else the open conversation's; Archive, or Unarchive, and Delete...",
    },
    Binding {
        name: "show-archived",
        chord: None,
        arguments: "",
        help: "Conversation > Show archived: show archived conversations in the list, or hide them again.",
    },
    Binding {
        name: "set-key",
        chord: None,
        arguments: "",
        help: "File > Set OpenRouter key...: open the dialog that stores the OpenRouter API key.",
    },
    Binding {
        name: "model",
        chord: None,
        arguments: "",
        help: "Conversation > Model...: open the picker of the open conversation's model: type to filter, Return chooses, Escape cancels.",
    },
    Binding {
        name: "default-model",
        chord: None,
        arguments: "",
        help: "Conversation > Default model...: open the picker for the model new conversations, and those with no model of their own, use.",
    },
    Binding {
        name: "delete-conversation",
        chord: None,
        arguments: "",
        help: "Conversation > Delete conversation...: ask whether to delete the open conversation for good; Cancel is focused first.",
    },
    Binding {
        name: "export-diagnostics",
        chord: None,
        arguments: "",
        help: "File > Export diagnostics: write td-agent's state and configuration, never the key file, to an archive in ~/Downloads, else the home directory.",
    },
];

/// The window as the seam drives it. A copy has no press to be offered
/// at, so the seam's inputs reach the clipboard as no clipboard.
pub struct Remote<'a> {
    pub app: &'a mut App,
}

impl Remote<'_> {
    /// Whether the input changed anything: the window asked for something
    /// or must paint again.
    fn outcome(&mut self, before: u64) -> driven::Outcome {
        if self.app.generation() != before || self.app.has_requests() {
            driven::Outcome::Changed
        } else {
            driven::Outcome::Ignored
        }
    }

    fn deliver(&mut self, input: window::Input<'_>) -> driven::Outcome {
        let before = self.app.generation();
        self.app.input(input, &mut NoClipboard);
        self.outcome(before)
    }
}

impl Controller for Remote<'_> {
    type Error = Refusal;

    fn bindings(&self) -> &'static [Binding] {
        BINDINGS
    }

    fn action(&mut self, name: &str, arguments: &[&str]) -> Result<driven::Outcome, Refusal> {
        if !arguments.is_empty() {
            return Err(Refusal::Protocol);
        }
        // Through the menu's own paths, as a choice of its item.
        if matches!(
            name,
            "set-key"
                | "model"
                | "export-diagnostics"
                | "delete-conversation"
                | "default-model"
                | "show-archived"
        ) {
            let before = self.app.generation();
            match name {
                "set-key" => self.app.open_key_dialog(),
                "model" => self.app.open_picker(),
                "delete-conversation" => self.app.open_delete(),
                "default-model" => self.app.open_default_picker(),
                "show-archived" => self.app.toggle_archived(),
                _ => self.app.export_diagnostics(),
            }
            return Ok(self.outcome(before));
        }
        let chord = BINDINGS
            .iter()
            .find(|b| b.name == name)
            .and_then(|b| b.chord)
            .ok_or(Refusal::Protocol)?;
        Ok(self.deliver(window::Input::Key {
            chord,
            repeat: false,
        }))
    }

    fn input(&mut self, input: Input<'_>) -> Result<driven::Outcome, Refusal> {
        Ok(match input {
            Input::Key { chord } => self.deliver(window::Input::Key {
                chord,
                repeat: false,
            }),
            Input::Pointer { phase, x, y } => self.deliver(window::Input::Pointer {
                phase: match phase {
                    PointerPhase::Press => window::PointerPhase::Press,
                    PointerPhase::Move => window::PointerPhase::Move,
                    PointerPhase::Release => window::PointerPhase::Release,
                },
                x: i64::from(x),
                y: i64::from(y),
                extend: false,
                follow: false,
            }),
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
                let before = self.app.generation();
                self.app.tick(now);
                self.outcome(before)
            }
            // No hints to show: the roles held are nothing to it.
            Input::Held(_) => driven::Outcome::Ignored,
        })
    }

    fn state(&self) -> Result<String, Refusal> {
        let app = &*self.app;
        let active = app.active();
        let state = active
            .and_then(|id| app.rows().iter().find(|r| &r.id == id))
            .map_or("none", |r| r.state.word());
        // The key dialog says where its keyboard is and how long its
        // entry is, never what it holds.
        let (dialog, entry) = app
            .dialog()
            .map_or(("none", 0), |dialog| (dialog.part(), dialog.length()));
        // The picker's selection and filter: model names, which hold no
        // secret, unlike the dialog's entry.
        let (picker, query) = app.picker().map_or(("none", ""), |picker| {
            (picker.selected().unwrap_or("nothing"), picker.query())
        });
        // The chooser's folder: a path of the human's, as the deletion
        // question would name it once chosen.
        let chooser = app.chooser().map_or_else(
            || "none".to_string(),
            |chooser| chooser.folder().display().to_string(),
        );
        Ok(format!(
            "conversations={}\tactive={}\tstate={state}\tfocus={}\tmessages={}\tcomposer={}\tmenu={}\tdialog={dialog}\tentry={entry}\tpicker={picker}\tpicking={}\tquery={query}\tconfirm={}\tmodel={}\teffort={}\tdefault={}\tchooser={}\tnotes={}\tunread={}\tnote={}\tarchived={}\tshown={}\trow-menu={}\tstatus={}\tcard={}",
            app.rows().len(),
            active.map_or("none", |id| id.as_str()),
            app.focus().word(),
            app.transcript().len(),
            app.composed().len(),
            if app.menu_open() { "open" } else { "closed" },
            app.picking(),
            app.confirm().map_or("none", |confirm| confirm.focus()),
            app.model(),
            app.effort(),
            app.default_model(),
            chooser.replace(['\t', '\n'], " "),
            if app.messages_window().is_some() {
                "open"
            } else {
                "closed"
            },
            app.unread(),
            app.notice().unwrap_or("").replace(['\t', '\n'], " "),
            app.rows().iter().filter(|r| r.archived).count(),
            if app.shows_archived() { "all" } else { "unarchived" },
            app.menu_row().map_or("none", |id| id.as_str()),
            app.status_line().replace(['\t', '\n'], " "),
            if app.workspace_card().is_some() {
                "open"
            } else {
                "closed"
            },
        ))
    }

    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, Refusal> {
        Ok(view(&*self.app))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_action_table_is_well_formed_and_drives_the_window() {
        driven::check(BINDINGS).unwrap();
        let mut app = crate::ui::tests::app();
        let mut remote = Remote { app: &mut app };
        assert_eq!(
            driven::request(&mut remote, b"1\t1\taction\tfocus-next"),
            "1\t1\tok\tchanged"
        );
        assert!(remote.state().unwrap().contains("focus=list"));
        // `new` opens the template chooser, or, with no workspaces,
        // starts a conversation with none.
        assert_eq!(
            driven::request(&mut remote, b"1\t2\taction\tnew"),
            "1\t2\tok\tchanged"
        );
        assert!(remote.state().unwrap().contains("picking=template"));
        assert!(remote.app.take_requests().is_empty());
        driven::request(
            &mut remote,
            format!("1\t5\tkey\t{}", hex("Escape")).as_bytes(),
        );
        assert!(remote.state().unwrap().contains("picking=none"));
        remote
            .app
            .set_no_workspaces(Some("no workspace jail".into()));
        assert_eq!(
            driven::request(&mut remote, b"1\t6\taction\tnew"),
            "1\t6\tok\tchanged"
        );
        assert_eq!(remote.app.take_requests(), [Request::New]);
        // Typed through the seam's key verb, then sent.
        driven::request(&mut remote, b"1\t3\taction\tfocus-next");
        driven::request(&mut remote, b"1\t4\taction\tfocus-next");
        for (n, hex) in ["6f", "6b"].iter().enumerate() {
            let line = format!("1\t{}\tkey\t{hex}", 10 + n);
            assert!(driven::request(&mut remote, line.as_bytes()).ends_with("changed"));
        }
        assert!(remote.state().unwrap().contains("composer=2"));
        driven::request(&mut remote, b"1\t20\taction\tsend");
        assert_eq!(remote.app.take_requests(), [Request::Send("ok".into())]);
        assert!(driven::request(&mut remote, b"1\t21\taction\tnope").contains("protocol"));
        assert!(driven::request(&mut remote, b"1\t22\taction\tnew\tx").contains("protocol"));
        let text = driven::request(&mut remote, b"1\t23\ttext");
        assert!(text.contains("\tok\t"), "{text}");
    }

    fn hex(text: &str) -> String {
        text.bytes().map(|b| format!("{b:02x}")).collect()
    }

    /// The open conversation's row menu and Show archived through the
    /// seam, the state counting the archived and naming the row menu's.
    #[test]
    fn the_row_menu_and_show_archived_are_driven() {
        let mut app = crate::ui::tests::app();
        let mut remote = Remote { app: &mut app };
        let state = remote.state().unwrap();
        assert!(
            state.contains("\tarchived=0\tshown=unarchived\trow-menu=none\t"),
            "{state}"
        );
        assert!(driven::request(&mut remote, b"1\t1\taction\trow-menu").ends_with("changed"));
        let active = remote.app.active().unwrap().to_string();
        let state = remote.state().unwrap();
        assert!(state.contains(&format!("\trow-menu={active}\t")), "{state}");
        assert!(state.contains("menu=open"));
        driven::request(
            &mut remote,
            format!("1\t2\tkey\t{}", hex("Return")).as_bytes(),
        );
        let archive = remote.app.take_requests();
        assert!(
            matches!(&archive[..], [Request::Archive { id, archived: true }] if id.as_str() == active),
            "{archive:?}"
        );
        assert!(remote.state().unwrap().contains("\trow-menu=none\t"));
        assert!(driven::request(&mut remote, b"1\t3\taction\tshow-archived").ends_with("changed"));
        assert!(remote.state().unwrap().contains("\tshown=all\t"));
    }

    /// The File menu and the key dialog through the seam: opened, typed
    /// into and saved, the snapshot and the text read-back never holding
    /// the key.
    #[test]
    fn the_menu_and_the_key_dialog_are_driven_and_never_show_the_key() {
        const KEY: &str = "sk-or-v1-0123456789abcdef";
        let mut app = crate::ui::tests::app();
        app.set_key_path(Some("/home/me/.config/td-agent/openrouter.key".into()));
        app.set_keyed(false);
        let mut remote = Remote { app: &mut app };
        assert!(remote.state().unwrap().contains("menu=closed\tdialog=none"));
        // `messages` opens the Messages window; the state says so, counts
        // the unread and carries the newest note whole.
        remote.app.note("a note\twith a tab");
        let state = remote.state().unwrap();
        assert!(
            state.contains("notes=closed\tunread=1\tnote=a note with a tab\t"),
            "{state}"
        );
        assert!(driven::request(&mut remote, b"1\t90\taction\tmessages").ends_with("changed"));
        assert!(remote.state().unwrap().contains("notes=open\tunread=0\t"));
        driven::request(
            &mut remote,
            format!("1\t91\tkey\t{}", hex("Escape")).as_bytes(),
        );
        assert!(remote.state().unwrap().contains("notes=closed"));
        assert!(remote.state().unwrap().contains("| no key: F10 |"));
        // `workspace` is `C-S-w`: with no repository workspace open, it
        // says so and opens nothing.
        driven::request(&mut remote, b"1\t92\taction\tworkspace");
        assert!(remote.state().unwrap().ends_with("\tcard=closed"));
        assert!(remote
            .app
            .notice()
            .unwrap()
            .contains("no repository workspace"));
        assert!(driven::request(&mut remote, b"1\t1\taction\tmenu").ends_with("changed"));
        assert!(remote.state().unwrap().contains("menu=open"));
        // Down to Set OpenRouter key…, below New conversation…, and
        // chosen.
        driven::request(
            &mut remote,
            format!("1\t4\tkey\t{}", hex("Down")).as_bytes(),
        );
        driven::request(
            &mut remote,
            format!("1\t5\tkey\t{}", hex("Return")).as_bytes(),
        );
        let state = remote.state().unwrap();
        assert!(
            state.contains("menu=closed\tdialog=entry\tentry=0"),
            "{state}"
        );
        for (n, c) in KEY.chars().enumerate() {
            let line = format!("1\t{}\tkey\t{}", 10 + n, hex(&c.to_string()));
            assert!(driven::request(&mut remote, line.as_bytes()).ends_with("changed"));
        }
        let state = remote.state().unwrap();
        assert!(state.contains(&format!("entry={}", KEY.len())), "{state}");
        let text = driven::request(&mut remote, b"1\t60\ttext");
        let shown =
            String::from_utf8(td_ui::control::unhex(text.rsplit('\t').next().unwrap()).unwrap())
                .unwrap();
        assert!(shown.contains(&"\u{2022}".repeat(KEY.len())), "{shown}");
        for leak in [state.as_str(), text.as_str(), shown.as_str()] {
            assert!(!leak.contains("0123456789abcdef"), "{leak}");
            assert!(!leak.contains(&hex("0123456789abcdef")), "{leak}");
        }
        driven::request(
            &mut remote,
            format!("1\t61\tkey\t{}", hex("Return")).as_bytes(),
        );
        assert_eq!(
            remote.app.take_requests(),
            [Request::SaveKey {
                secret: crate::key::Secret::new(KEY.into()),
                replace: false
            }]
        );
        // The session stored it: the dialog closes, emptied, and the
        // status row stops asking for a key.
        remote
            .app
            .key_saved("/home/me/.config/td-agent/openrouter.key");
        let state = remote.state().unwrap();
        assert!(state.contains("dialog=none\tentry=0"), "{state}");
        assert!(!state.contains("no key"), "{state}");
        assert!(state.contains("the key is stored in"), "{state}");
        // `set-key` opens the dialog directly.
        assert!(driven::request(&mut remote, b"1\t70\taction\tset-key").ends_with("changed"));
        assert!(remote.state().unwrap().contains("dialog=entry"));
        driven::request(
            &mut remote,
            format!("1\t71\tkey\t{}", hex("Escape")).as_bytes(),
        );
        assert!(remote.state().unwrap().contains("dialog=none"));
        // `export-diagnostics` asks the session for the archive.
        assert!(
            driven::request(&mut remote, b"1\t72\taction\texport-diagnostics").ends_with("changed")
        );
        assert_eq!(remote.app.take_requests(), [Request::Export]);
    }

    /// `default-model` opens the default's picker, which the state names.
    #[test]
    fn the_default_model_is_driven_through_its_picker() {
        let mut app = crate::ui::tests::app();
        let offer = |id: &str| crate::picker::Offer {
            id: id.into(),
            price: String::new(),
            usable: true,
            reasoning: true,
        };
        app.set_offers(vec![offer("m/conv"), offer("m/new")], "medium");
        let mut remote = Remote { app: &mut app };
        assert!(driven::request(&mut remote, b"1\t1\taction\tdefault-model").ends_with("changed"));
        assert!(remote
            .state()
            .unwrap()
            .contains("picker=m/conv\tpicking=default\t"));
        driven::request(
            &mut remote,
            format!("1\t2\tkey\t{}", hex("Down")).as_bytes(),
        );
        driven::request(
            &mut remote,
            format!("1\t3\tkey\t{}", hex("Return")).as_bytes(),
        );
        assert!(remote.state().unwrap().contains("picking=none"));
        assert_eq!(
            remote.app.take_requests(),
            [Request::SetDefault("m/new".into())]
        );
    }

    /// `delete-conversation` asks, and the state says where the
    /// question's keyboard is.
    #[test]
    fn deletion_is_driven_through_its_question() {
        let mut app = crate::ui::tests::app();
        app.set_active(crate::ui::tests::id(2));
        let mut remote = Remote { app: &mut app };
        assert!(
            driven::request(&mut remote, b"1\t1\taction\tdelete-conversation").ends_with("changed")
        );
        assert!(remote.state().unwrap().contains("confirm=cancel"));
        driven::request(&mut remote, format!("1\t2\tkey\t{}", hex("Tab")).as_bytes());
        assert!(remote.state().unwrap().contains("confirm=delete"));
        driven::request(
            &mut remote,
            format!("1\t3\tkey\t{}", hex("Return")).as_bytes(),
        );
        assert!(remote.state().unwrap().contains("confirm=none"));
        assert_eq!(
            remote.app.take_requests(),
            [Request::Delete(crate::ui::tests::id(2))]
        );
    }

    /// `model` opens the picker, whose selection and filter the state
    /// says; typed into and chosen, it asks for the conversation's model.
    #[test]
    fn the_model_picker_is_driven() {
        let mut app = crate::ui::tests::app();
        let offer = |id: &str| crate::picker::Offer {
            id: id.into(),
            price: String::new(),
            usable: true,
            reasoning: true,
        };
        app.set_offers(vec![offer("m/default"), offer("m/conv")], "medium");
        let mut remote = Remote { app: &mut app };
        let state = remote.state().unwrap();
        assert!(
            state.contains(
                "picker=none\tpicking=none\tquery=\tconfirm=none\tmodel=m/default\teffort=medium\tdefault=m/default\t"
            ),
            "{state}"
        );
        assert!(driven::request(&mut remote, b"1\t1\taction\tmodel").ends_with("changed"));
        assert!(remote
            .state()
            .unwrap()
            .contains("picker=m/default\tpicking=conversation\tquery="));
        for (n, c) in ["c", "o", "n", "v"].iter().enumerate() {
            let line = format!("1\t{}\tkey\t{}", 10 + n, hex(c));
            assert!(driven::request(&mut remote, line.as_bytes()).ends_with("changed"));
        }
        let state = remote.state().unwrap();
        assert!(
            state.contains("picker=m/conv\tpicking=conversation\tquery=conv"),
            "{state}"
        );
        driven::request(
            &mut remote,
            format!("1\t20\tkey\t{}", hex("Return")).as_bytes(),
        );
        assert!(remote.state().unwrap().contains("picker=none"));
        assert_eq!(
            remote.app.take_requests(),
            [Request::Choose {
                model: Some("m/conv".into()),
                effort: None
            }]
        );
    }
}
