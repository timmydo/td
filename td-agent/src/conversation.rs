//! `td-agent conversation <id>`: one conversation's process (DESIGN.md
//! §2). The window process starts it over a socketpair handed to it as
//! its standard input and output. It locks the conversation's directory,
//! the only writer of it while it runs, replays the log up the socketpair,
//! and then serves the window's messages until the socketpair closes,
//! when it exits.
//!
//! There is no model yet, so a turn is local echo: the human's message is
//! logged and shown, and the turn finishes with the outcome "no model".

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;

use crate::frame;
use crate::protocol::{Down, Up, MAX_TEXT};
use crate::store::{Conversation, Effect, Id, Kind, Role, StateDir, LOCK_WAIT};

/// What a turn ends with until the model client lands.
pub const NO_MODEL: &str = "no model";

/// Runs the conversation over the socketpair on standard input and
/// output until it closes. An error is why it could not go on.
pub fn run(state: &StateDir, id: &Id, create: Option<Role>) -> Result<(), String> {
    // The socketpair is standard input and output: one socket, taken as
    // a stream by duplicating the descriptor, which needs no `unsafe`.
    let socket = io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("standard input: {e}"))?;
    serve(UnixStream::from(socket), state, id, create)
}

/// How serving ended other than by the window closing between turns.
enum End {
    /// The window closed its end mid-exchange: as orderly as between
    /// turns, since the log already holds the whole turn.
    Closed,
    Failed(String),
}

impl From<String> for End {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

/// Sends `up` to the window.
fn send(stream: &mut UnixStream, up: &Up) -> Result<(), End> {
    match frame::write(stream, &up.encode()) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) =>
        {
            Err(End::Closed)
        }
        Err(e) => Err(End::Failed(format!("the window: {e}"))),
    }
}

/// The conversation over `stream`: opened, replayed, then served.
pub fn serve(
    stream: UnixStream,
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
) -> Result<(), String> {
    match serve_until_closed(stream, state, id, create) {
        Ok(()) | Err(End::Closed) => Ok(()),
        Err(End::Failed(e)) => Err(e),
    }
}

fn serve_until_closed(
    mut stream: UnixStream,
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
) -> Result<(), End> {
    let (mut conversation, load) = Conversation::open(state, id, create, LOCK_WAIT)?;
    if let Some(bytes) = load.torn {
        eprintln!("td-agent: conversation {id}: dropped a torn final log line of {bytes} bytes");
    }
    send(
        &mut stream,
        &Up::Hello {
            role: conversation.meta().role,
            title: conversation.meta().title.clone(),
            torn: load.torn,
            interrupted: load.interrupted,
        },
    )?;
    for event in conversation.events() {
        send(&mut stream, &Up::Event(event.clone()))?;
    }
    loop {
        let bytes = match frame::read(&mut stream) {
            Ok(Some(bytes)) => bytes,
            // The window closed its end: nothing more will come.
            Ok(None) => return Ok(()),
            Err(e) => return Err(End::Failed(format!("the window: {e}"))),
        };
        let Down::User { delivery, text } = Down::decode(&bytes)?;
        if conversation.delivered(&delivery) {
            // A message the window sent again after a restart: logged
            // once, acknowledged each time.
            send(&mut stream, &Up::Delivered { delivery })?;
            continue;
        }
        let refused = if text.len() > MAX_TEXT {
            Some(format!("a message is at most {MAX_TEXT} bytes"))
        } else if text.trim().is_empty() {
            Some("an empty message".to_string())
        } else if !conversation.has_room(text.len()) {
            Some("the conversation's log is full; start another conversation".to_string())
        } else {
            None
        };
        if let Some(reason) = refused {
            send(&mut stream, &Up::Refused { delivery, reason })?;
            continue;
        }
        // Titled by its first message, or by the next one should the
        // title not have been written then.
        let untitled = conversation.meta().role == Role::Conversation
            && conversation.meta().title == Role::Conversation.first_title();
        // The turn runs to its end in the log before the window hears of
        // it, so a window that goes meanwhile interrupts nothing.
        let user = conversation
            .append(Kind::User {
                delivery: delivery.clone(),
                text: text.clone(),
            })?
            .clone();
        let started = conversation
            .append(Kind::Started {
                effect: Effect::Turn,
                of: user.seq,
            })?
            .clone();
        // The started record is durable before the turn runs.
        conversation.sync()?;
        let finished = conversation
            .append(Kind::Finished {
                started: started.seq,
                outcome: NO_MODEL.to_string(),
            })?
            .clone();
        // A turn boundary.
        conversation.sync()?;
        let mut outbox = vec![
            Up::Event(user),
            Up::Event(started),
            Up::Delivered { delivery },
            Up::Event(finished),
        ];
        if untitled {
            conversation.retitle(&text)?;
            outbox.push(Up::Title {
                title: conversation.meta().title.clone(),
            });
        }
        for up in &outbox {
            send(&mut stream, up)?;
        }
        stream
            .flush()
            .map_err(|e| End::Failed(format!("the window: {e}")))?;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::store::tests::Scratch;

    fn next(stream: &mut UnixStream) -> Up {
        Up::decode(&frame::read(stream).unwrap().unwrap()).unwrap()
    }

    fn say(stream: &mut UnixStream, delivery: &str, text: &str) {
        let down = Down::User {
            delivery: delivery.into(),
            text: text.into(),
        };
        frame::write(stream, &down.encode()).unwrap();
    }

    const D1: &str = "11111111111111111111111111111111";
    const D2: &str = "22222222222222222222222222222222";

    #[test]
    fn a_message_is_logged_echoed_and_its_turn_finished_with_no_model() {
        let scratch = Scratch::new("serve");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Conversation)))
        };
        assert!(matches!(next(&mut window), Up::Hello { torn: None, .. }));
        say(&mut window, D1, "first words\nand more");
        let Up::Event(user) = next(&mut window) else {
            panic!("no user event")
        };
        assert!(
            matches!(user.kind, Kind::User { ref text, .. } if text == "first words\nand more")
        );
        assert!(matches!(
            next(&mut window),
            Up::Event(crate::store::Event {
                kind: Kind::Started { of: 1, .. },
                ..
            })
        ));
        assert_eq!(
            next(&mut window),
            Up::Delivered {
                delivery: D1.into()
            }
        );
        assert!(matches!(
            next(&mut window),
            Up::Event(crate::store::Event { kind: Kind::Finished { started: 2, ref outcome }, .. })
                if outcome == NO_MODEL
        ));
        assert_eq!(
            next(&mut window),
            Up::Title {
                title: "first words".into()
            }
        );
        // The same delivery again is acknowledged, not logged again.
        say(&mut window, D1, "first words\nand more");
        assert_eq!(
            next(&mut window),
            Up::Delivered {
                delivery: D1.into()
            }
        );
        say(&mut window, D2, "   ");
        assert!(matches!(next(&mut window), Up::Refused { .. }));
        // Closing the window's end ends the process.
        drop(window);
        served.join().unwrap().unwrap();
        let (conversation, _) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert_eq!(conversation.events().len(), 3);
    }

    #[test]
    fn a_window_gone_mid_turn_leaves_the_turn_whole_and_the_process_orderly() {
        let scratch = Scratch::new("gone");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Conversation)))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        say(&mut window, D1, "said, then gone");
        drop(window);
        served.join().unwrap().unwrap();
        let (conversation, load) = Conversation::open(&state, &id, None, LOCK_WAIT).unwrap();
        assert!(load.interrupted.is_empty());
        let kinds: Vec<&Kind> = conversation.events().iter().map(|e| &e.kind).collect();
        assert!(
            matches!(
                kinds.as_slice(),
                [
                    Kind::User { .. },
                    Kind::Started { .. },
                    Kind::Finished { .. }
                ]
            ),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_conversation_still_untitled_takes_its_title_from_the_next_message() {
        let scratch = Scratch::new("untitled");
        let state = scratch.state();
        let id = Id::random().unwrap();
        {
            // A message logged whose title was never written.
            let (mut conversation, _) =
                Conversation::open(&state, &id, Some(Role::Conversation), LOCK_WAIT).unwrap();
            conversation
                .append(Kind::User {
                    delivery: D1.into(),
                    text: "lost title".into(),
                })
                .unwrap();
            conversation.sync().unwrap();
        }
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, None))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        // The replay: the message, the start load gave it, its interruption.
        for _ in 0..3 {
            next(&mut window);
        }
        say(&mut window, D2, "found title");
        let title = loop {
            if let Up::Title { title } = next(&mut window) {
                break title;
            }
        };
        assert_eq!(title, "found title");
        drop(window);
        served.join().unwrap().unwrap();
    }

    #[test]
    fn a_restart_replays_the_log_in_order() {
        let scratch = Scratch::new("restart");
        let state = scratch.state();
        let id = Id::random().unwrap();
        for (round, delivery) in [D1, D2].into_iter().enumerate() {
            let (mut window, theirs) = UnixStream::pair().unwrap();
            let served = {
                let (state, id) = (state.clone(), id.clone());
                let create = (round == 0).then_some(Role::Orchestrator);
                std::thread::spawn(move || serve(theirs, &state, &id, create))
            };
            assert!(matches!(next(&mut window), Up::Hello { .. }));
            // The earlier round's three events come back first.
            for seq in 1..=(round as u64 * 3) {
                match next(&mut window) {
                    Up::Event(event) => assert_eq!(event.seq, seq),
                    other => panic!("{other:?}"),
                }
            }
            say(&mut window, delivery, "hi");
            for _ in 0..4 {
                next(&mut window);
            }
            drop(window);
            served.join().unwrap().unwrap();
        }
    }
}
