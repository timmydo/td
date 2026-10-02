//! `td-agent conversation <id>`: one conversation's process (DESIGN.md
//! §2). The window process starts it over a socketpair handed to it as
//! its standard input and output. It locks the conversation's directory,
//! the only writer of it while it runs, replays the log up the socketpair,
//! and then serves the window's messages until the socketpair closes,
//! when it exits.
//!
//! A turn is one exchange with the model (DESIGN.md §5), with no tools:
//! the human's message is logged, the request is reserved against the
//! turn's, the conversation's and, through the window, the day's limits,
//! logged as started and synced, and only then sent through the fetch
//! service as a stream. Its reply is drawn in the window as it arrives
//! and logged whole when it ends, with its usage; a stream that breaks
//! off, fails or is interrupted logs what it had brought, marked
//! incomplete. A rate-limited request is asked again after a bounded
//! wait; any other failure ends the turn and says why, with a retry
//! action where asking again may succeed. After a conversation's first
//! exchange its title comes from `title_model`, reserved like any request
//! and asked for whole, not streamed.
//!
//! The window's frames are read on a thread of their own into a channel,
//! so that the window's writes never wait on a request in flight; what
//! arrives during a turn waits its turn. A stream is read on a thread of
//! its own into the same channel, so an interrupt from the window is heard
//! between any two of its frames.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::accounts;
use crate::assemble::Assembly;
use crate::client::{self, Completion, Failure, Params};
use crate::config::Client;
use crate::cost;
use crate::frame;
use crate::key::Secret;
use crate::models::{Model, Models};
use crate::protocol::{Down, Up, MAX_TEXT};
use crate::sse::{self, Fault};
use crate::store::{
    Basis, Conversation, Effect, Event, Id, Kind, Purpose, Role, StateDir, LOCK_WAIT,
};
use crate::td_fetch;

/// What a turn ends with when there is no window settings to make a
/// request with: the window sends them first, so only a harness that
/// does not sees this.
pub const NO_SETTINGS: &str = "no settings from the window";
/// How long a request waits for the window to answer its reservation.
const RESERVE_WAIT: Duration = Duration::from_secs(30);

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

/// What the reader thread, and a stream's thread, hand on.
enum Inbound {
    Down(Down),
    Closed,
    Broken(String),
    /// What the stream of request `request` (its sequence number) brought.
    Fetch {
        request: u64,
        item: Fetched,
    },
}

/// One step of a streamed request, as its thread read it.
#[derive(Debug)]
enum Fetched {
    /// The reply's head.
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// A frame of its body.
    Chunk(Vec<u8>),
    /// The service said the body is whole.
    End,
    /// The request, or the stream, failed.
    Failed(td_fetch::Error),
}

/// What a streamed request came to.
enum Streamed {
    Replied(Completion),
    /// It failed, or was interrupted; `partial` is what it had brought.
    Failed {
        failure: Failure,
        partial: Option<Completion>,
    },
}

/// A streamed reply being read.
struct Reading {
    head: Option<(u16, Vec<(String, String)>)>,
    /// A reply that is no event stream, read whole: an error status's
    /// body, or a 200 that came as one JSON object.
    plain: Option<Vec<u8>>,
    reader: sse::Reader,
    reply: Assembly,
}

impl Reading {
    /// What the reply had brought, when anything worth keeping.
    fn partial(&self) -> Option<Completion> {
        (!self.reply.is_empty()).then(|| self.reply.completion())
    }

    /// The stream broke off: charged and offered again.
    fn broken(&self, message: String) -> Streamed {
        Streamed::Failed {
            failure: Failure::Retryable {
                status: None,
                message,
                usage: self.reply.usage(),
            },
            partial: self.partial(),
        }
    }
}

/// The conversation over `stream`: opened, replayed, then served.
pub fn serve(
    stream: UnixStream,
    state: &StateDir,
    id: &Id,
    create: Option<Role>,
) -> Result<(), String> {
    let (conversation, load) = Conversation::open(state, id, create, LOCK_WAIT)?;
    if let Some(bytes) = load.torn {
        eprintln!("td-agent: conversation {id}: dropped a torn final log line of {bytes} bytes");
    }
    let reader = stream
        .try_clone()
        .map_err(|e| format!("the socketpair: {e}"))?;
    let (sender, inbox) = listen(reader)?;
    let mut session = Session {
        conversation,
        writer: stream,
        inbox,
        sender,
        live: Arc::new(AtomicU64::new(0)),
        interrupt: false,
        queue: VecDeque::new(),
        setup: None,
        state: state.root().to_path_buf(),
        // Random, so no two processes of one conversation share an id,
        // and the window's ledger never takes one's grant for another's.
        next_reservation: u64::from_str_radix(&crate::store::random_hex(6)?, 16)
            .map_err(|e| format!("a reservation id: {e}"))?,
        gone: false,
    };
    session.send(&Up::Hello {
        role: session.conversation.meta().role,
        title: session.conversation.meta().title.clone(),
        torn: load.torn,
        interrupted: load.interrupted,
    });
    for event in session.conversation.events().to_vec() {
        session.send(&Up::Event(event));
    }
    session.serve()
}

/// Reads the window's frames into a channel until the socketpair ends;
/// the channel's sender is kept for the streams to hand on through.
fn listen(mut reader: UnixStream) -> Result<(Sender<Inbound>, Receiver<Inbound>), String> {
    let (send, inbox) = mpsc::channel();
    let sender = send.clone();
    std::thread::Builder::new()
        .name("td-agent-window".into())
        .spawn(move || loop {
            let inbound = match frame::read(&mut reader) {
                Ok(Some(bytes)) => match Down::decode(&bytes) {
                    Ok(down) => Inbound::Down(down),
                    Err(e) => Inbound::Broken(format!("a malformed message: {e}")),
                },
                // A window that closes with our frames unread resets the
                // socketpair: closed all the same.
                Ok(None) => Inbound::Closed,
                Err(frame::Error::Io(e)) if e.kind() == io::ErrorKind::ConnectionReset => {
                    Inbound::Closed
                }
                Err(e) => Inbound::Broken(e.to_string()),
            };
            let last = !matches!(inbound, Inbound::Down(_));
            if send.send(inbound).is_err() || last {
                break;
            }
        })
        .map_err(|e| format!("the reader thread: {e}"))?;
    Ok((sender, inbox))
}

/// A stream's thread: sends `body` as a streamed request and hands on its
/// head and each frame as they come, for as long as `live` names it.
/// Returning drops the stream, which closes its connection, and with it
/// the service's to the origin: an interrupt takes effect at the frame
/// after it, which the service's idle deadline bounds (DESIGN.md §5).
fn fetch(
    url: &str,
    headers: &[(&'static str, String)],
    body: &[u8],
    request: u64,
    live: &AtomicU64,
    send: &Sender<Inbound>,
) {
    let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    let hand = |item: Fetched| send.send(Inbound::Fetch { request, item }).is_ok();
    let mut stream = match td_fetch::post_stream(url, &headers, body, Some(client::MAX_STREAM)) {
        Ok(stream) => stream,
        Err(e) => {
            hand(Fetched::Failed(e));
            return;
        }
    };
    let head = Fetched::Head {
        status: stream.status,
        headers: stream.headers.clone(),
    };
    if !hand(head) {
        return;
    }
    while live.load(Ordering::SeqCst) == request {
        let item = match stream.next_chunk() {
            Ok(Some(bytes)) => Fetched::Chunk(bytes.to_vec()),
            Ok(None) => Fetched::End,
            Err(e) => Fetched::Failed(e),
        };
        let last = !matches!(item, Fetched::Chunk(_));
        if !hand(item) || last {
            return;
        }
    }
}

/// How a turn ended: what the window shows, whether the human may ask
/// for it again, and whether the model replied.
struct Outcome {
    text: String,
    retry: bool,
    replied: bool,
}

impl Outcome {
    fn stop(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            retry: false,
            replied: false,
        }
    }
}

/// What a request was charged, and how that was known.
fn charge(
    usage: Option<client::Usage>,
    pricing: Option<cost::Pricing>,
    reserved: u64,
) -> (u64, Basis) {
    match (usage, pricing) {
        (
            Some(client::Usage {
                cost: Some(cost), ..
            }),
            _,
        ) => (cost, Basis::Reported),
        (Some(usage), Some(pricing)) => (pricing.charge(&usage.tokens), Basis::Computed),
        _ => (reserved, Basis::Reserved),
    }
}

struct Session {
    conversation: Conversation,
    writer: UnixStream,
    inbox: Receiver<Inbound>,
    /// The inbox's sender, which each stream's thread hands on through.
    sender: Sender<Inbound>,
    /// The request whose stream is still wanted, 0 for none: a stream's
    /// thread reads on only while this names its request.
    live: Arc<AtomicU64>,
    /// The window asked to interrupt the turn under way.
    interrupt: bool,
    /// Messages that came while a turn waited on the window.
    queue: VecDeque<Down>,
    setup: Option<(Result<Secret, String>, Client)>,
    state: PathBuf,
    /// The last reservation id asked for.
    next_reservation: u64,
    /// The window has closed its end: the turn under way finishes in the
    /// log, and then the process exits.
    gone: bool,
}

impl Session {
    /// Sends `up` to the window; a window gone is noted, not an error, so
    /// a turn under way still finishes whole in the log.
    fn send(&mut self, up: &Up) {
        if self.gone {
            return;
        }
        if let Err(e) = frame::write(&mut self.writer, &up.encode()) {
            if !matches!(
                e.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ) {
                eprintln!("td-agent: the window: {e}");
            }
            self.gone = true;
        }
    }

    /// Appends `kind` to the log and sends it up.
    fn log(&mut self, kind: Kind) -> Result<Event, String> {
        let event = self.conversation.append(kind)?.clone();
        self.send(&Up::Event(event.clone()));
        Ok(event)
    }

    fn sync(&mut self) -> Result<(), String> {
        self.conversation.sync()
    }

    fn next(&mut self) -> Result<Option<Down>, String> {
        if let Some(down) = self.queue.pop_front() {
            return Ok(Some(down));
        }
        loop {
            match self.inbox.recv() {
                Ok(Inbound::Down(down)) => return Ok(Some(down)),
                Ok(Inbound::Closed) | Err(_) => return Ok(None),
                Ok(Inbound::Broken(e)) => return Err(format!("the window: {e}")),
                // A stream given up on, still reading to its next frame.
                Ok(Inbound::Fetch { .. }) => {}
            }
        }
    }

    fn serve(&mut self) -> Result<(), String> {
        while !self.gone {
            let Some(down) = self.next()? else {
                return Ok(());
            };
            match down {
                Down::Setup { key, client } => self.setup = Some((key, client)),
                Down::User { delivery, text } => self.user(delivery, text)?,
                Down::Retry => self.retry()?,
                // A reservation granted after its request gave up waiting:
                // nothing was sent, so the window's hold is released.
                Down::Reservation { id, refusal: None } => self.spent(id, 0),
                // Between turns there is nothing to interrupt.
                Down::Reservation { .. } | Down::Interrupt => {}
            }
            if !self.gone {
                let _ = self.writer.flush();
            }
        }
        Ok(())
    }

    fn user(&mut self, delivery: String, text: String) -> Result<(), String> {
        if self.conversation.delivered(&delivery) {
            // A message the window sent again after a restart: logged
            // once, acknowledged each time.
            self.send(&Up::Delivered { delivery });
            return Ok(());
        }
        let refused = if text.len() > MAX_TEXT {
            Some(format!("a message is at most {MAX_TEXT} bytes"))
        } else if text.trim().is_empty() {
            Some("an empty message".to_string())
        } else if !self.conversation.has_room(text.len()) {
            Some("the conversation's log is full; start another conversation".to_string())
        } else {
            None
        };
        if let Some(reason) = refused {
            self.send(&Up::Refused { delivery, reason });
            return Ok(());
        }
        // Titled by its first message, or by the next one should the
        // title not have been written then, until a title model's.
        let untitled = self.conversation.meta().role == Role::Conversation
            && self.conversation.meta().title == Role::Conversation.first_title();
        let user = self.conversation.append(Kind::User {
            delivery: delivery.clone(),
            text: text.clone(),
        })?;
        let of = user.seq;
        let user = user.clone();
        let started = self
            .conversation
            .append(Kind::Started {
                effect: Effect::Turn,
                of,
            })?
            .clone();
        // The started record is durable before the turn runs.
        self.sync()?;
        self.send(&Up::Event(user));
        self.send(&Up::Event(started.clone()));
        self.send(&Up::Delivered { delivery });
        if untitled {
            self.conversation.retitle(&text)?;
            let title = self.conversation.meta().title.clone();
            self.send(&Up::Title { title });
        }
        self.turn(started.seq)
    }

    /// The last turn again, when it ended in a failure that may pass.
    fn retry(&mut self) -> Result<(), String> {
        let events = self.conversation.events();
        let last = events.iter().rev().find_map(|e| match e.kind {
            Kind::Started {
                effect: Effect::Turn,
                of,
            } => Some((e.seq, of)),
            _ => None,
        });
        let retryable = last.filter(|(started, _)| {
            events.iter().any(
                |e| matches!(e.kind, Kind::Finished { started: s, retry: true, .. } if s == *started),
            )
        });
        let Some((_, of)) = retryable else {
            // Said, so the window stops counting this process busy.
            self.send(&Up::Refused {
                delivery: String::new(),
                reason: "there is no failed turn to ask again".into(),
            });
            return Ok(());
        };
        let started = self.log(Kind::Started {
            effect: Effect::Turn,
            of,
        })?;
        self.sync()?;
        self.turn(started.seq)
    }

    /// Runs turn `turn` to its end in the log.
    fn turn(&mut self, turn: u64) -> Result<(), String> {
        self.interrupt = false;
        let outcome = self.exchange(turn)?;
        if outcome.replied && self.first_reply() {
            self.title(turn)?;
        }
        self.log(Kind::Finished {
            started: turn,
            outcome: outcome.text,
            retry: outcome.retry,
        })?;
        // A turn boundary.
        self.sync()
    }

    /// Whether the conversation has had exactly one whole reply, and no
    /// title from a model yet.
    fn first_reply(&self) -> bool {
        if self.conversation.meta().role != Role::Conversation {
            return false;
        }
        let events = self.conversation.events();
        let mut turns = Vec::new();
        let mut replies = 0usize;
        for event in events {
            match &event.kind {
                Kind::Title { .. } => return false,
                Kind::Request {
                    purpose: Purpose::Turn,
                    ..
                } => turns.push(event.seq),
                Kind::Assistant {
                    request,
                    incomplete: false,
                    ..
                } if turns.contains(request) => replies += 1,
                _ => {}
            }
        }
        replies == 1
    }

    /// The models cache and the entry for `model`, the configuration's
    /// `key`; a refusal says why a request cannot be made with them.
    fn model(&self, key: &str, model: &str, client: &Client) -> Result<Option<Model>, String> {
        let models = Models::load(&self.state)?;
        let entry = models.as_ref().and_then(|m| m.find(model)).cloned();
        if models.is_some() && entry.is_none() {
            return Err(format!(
                "{model} is not in the provider's models list; set `{key}` to one that is"
            ));
        }
        if client.limits.any() && entry.as_ref().and_then(|m| m.pricing).is_none() {
            let why = if models.is_none() {
                " (the models list has not been fetched yet)"
            } else {
                ""
            };
            return Err(format!(
                "no price is known for {model}{why}; a model without pricing is refused while a cost limit is set"
            ));
        }
        Ok(entry)
    }

    /// Asks the window to reserve `amount` against the day: the request's
    /// id when granted, or why not.
    fn reserve(&mut self, amount: u64) -> Result<u64, String> {
        self.next_reservation = self.next_reservation.wrapping_add(1);
        let id = self.next_reservation;
        self.send(&Up::Reserve { id, amount });
        if self.gone {
            return Err("the window has closed".into());
        }
        let deadline = Instant::now() + RESERVE_WAIT;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(wait) {
                Ok(Inbound::Down(Down::Reservation {
                    id: answered,
                    refusal,
                })) if answered == id => return refusal.map_or(Ok(id), Err),
                // An earlier request's grant, come after it gave up.
                Ok(Inbound::Down(Down::Reservation {
                    id: late,
                    refusal: None,
                })) => self.spent(late, 0),
                // Said while waiting: the request is not sent.
                Ok(Inbound::Down(Down::Interrupt)) => self.interrupt = true,
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Closed | Inbound::Broken(_)) | Err(RecvTimeoutError::Disconnected) => {
                    self.gone = true;
                    return Err("the window has closed".into());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "the window did not answer a reservation in {} s",
                        RESERVE_WAIT.as_secs()
                    ))
                }
            }
        }
    }

    /// Tells the window what request `id` cost.
    fn spent(&mut self, id: u64, amount: u64) {
        self.send(&Up::Spent { id, amount });
    }

    /// Logs a request's end: its usage, then its finish.
    fn settle(
        &mut self,
        request: u64,
        usage: Option<client::Usage>,
        cost: (u64, Basis),
        outcome: String,
    ) -> Result<(), String> {
        self.log(Kind::Usage {
            request,
            tokens: usage.map(|u| u.tokens).unwrap_or_default(),
            cost: cost.0,
            basis: cost.1,
        })?;
        self.log(Kind::Finished {
            started: request,
            outcome,
            retry: false,
        })?;
        Ok(())
    }

    /// The turn's exchange with the model, retrying a rate limit.
    fn exchange(&mut self, turn: u64) -> Result<Outcome, String> {
        let Some((key, client)) = self.setup.clone() else {
            return Ok(Outcome::stop(NO_SETTINGS));
        };
        let key = match key {
            Ok(key) => key,
            Err(why) => return Ok(Outcome::stop(why)),
        };
        let role = self.conversation.meta().role;
        let name = client.model_for(role).to_string();
        let setting = match role {
            Role::Orchestrator => "orchestrator_model",
            Role::Conversation => "model",
        };
        let model = match self.model(setting, &name, &client) {
            Ok(model) => model,
            Err(why) => return Ok(Outcome::stop(why)),
        };
        let pricing = model.as_ref().and_then(|m| m.pricing);
        // The prefix this program writes; a conversation begun by another
        // (an empty one from before the model client, say) takes it as an
        // event, at the cost of one cache miss.
        let expected = crate::prompt::prefix(role);
        if client::current_prefix(self.conversation.events(), self.conversation.prefix_file()).1
            != expected
        {
            self.log(Kind::Prefix { text: expected })?;
        }
        let mut attempt = 0u32;
        loop {
            let events = self.conversation.events();
            let (prefix, prefix_text) =
                client::current_prefix(events, self.conversation.prefix_file());
            let messages = client::messages(events);
            let mut max_tokens = model
                .as_ref()
                .and_then(|m| m.max_completion_tokens)
                .map_or(client::MAX_TOKENS, |m| m.min(client::MAX_TOKENS))
                .max(1);
            let effort = model
                .as_ref()
                .is_none_or(|m| m.supports("reasoning"))
                .then_some(client.reasoning_effort.as_str());
            let build = |max_tokens: u64| -> Result<(String, String), String> {
                let head = client::head(&Params {
                    model: &name,
                    max_tokens,
                    effort,
                    client: &client,
                });
                let body = client::turn_body(&head, prefix_text, &messages)?;
                Ok((head, body))
            };
            let (mut head, mut body) = build(max_tokens)?;
            let estimate = client::estimate(events, body.len() as u64);
            if let Some(context) = model.as_ref().and_then(|m| m.context_length) {
                if estimate >= context {
                    return Ok(Outcome::stop(format!(
                        "the conversation is about {estimate} tokens, past {name}'s context of {context}; start another conversation (compaction comes later)"
                    )));
                }
                if estimate.saturating_add(max_tokens) > context {
                    max_tokens = context - estimate;
                    (head, body) = build(max_tokens)?;
                }
            }
            let reserved = pricing.map_or(0, |p| p.reserve(estimate, max_tokens));
            let events = self.conversation.events();
            if let Err(why) = cost::within(
                "max_cost_per_turn",
                client.limits.turn,
                accounts::turn_spent(events, turn),
                reserved,
            )
            .and_then(|()| {
                cost::within(
                    "max_cost_per_conversation",
                    client.limits.conversation,
                    accounts::spent(events),
                    reserved,
                )
            }) {
                return Ok(Outcome::stop(why));
            }
            let room = (body.len() as u64).saturating_add(client::MAX_REPLY.saturating_mul(2));
            if !self.conversation.has_room_for(room) {
                return Ok(Outcome::stop(
                    "the conversation's log is full; start another conversation",
                ));
            }
            let id = match self.reserve(reserved) {
                Ok(id) => id,
                // Nothing was sent; a turn the closing window cut short
                // may be asked again when the conversation is reopened.
                Err(why) => {
                    return Ok(Outcome {
                        text: why,
                        retry: self.gone,
                        replied: false,
                    })
                }
            };
            if self.interrupt {
                // Interrupted while the window reserved it: never sent.
                self.spent(id, 0);
                return Ok(Outcome {
                    text: "interrupted before its request was sent".into(),
                    retry: true,
                    replied: false,
                });
            }
            let request = self.log(Kind::Request {
                turn,
                purpose: Purpose::Turn,
                prefix,
                head,
                bytes: body.len() as u64,
                reserved,
            })?;
            // Logged as started and synced before it is sent: a restart
            // that finds it unfinished never sends it again.
            self.sync()?;
            let failure = match self.stream(request.seq, &client, &key, body) {
                Streamed::Replied(completion) => {
                    let cost = charge(completion.usage, pricing, reserved);
                    let text = self.reply(request.seq, &completion, cost)?;
                    self.spent(id, cost.0);
                    return Ok(Outcome {
                        text,
                        retry: false,
                        replied: true,
                    });
                }
                Streamed::Failed { failure, partial } => {
                    if let Some(partial) = partial {
                        self.partial(request.seq, partial)?;
                    }
                    failure
                }
            };
            let outcome = failure.outcome();
            match failure {
                Failure::RateLimited { message, wait } => {
                    self.settle(request.seq, None, (0, Basis::Nothing), outcome)?;
                    self.sync()?;
                    self.spent(id, 0);
                    if attempt >= client::RETRIES {
                        return Ok(Outcome::stop(format!(
                            "error 429: {message}; still rate-limited after {} retries",
                            client::RETRIES
                        )));
                    }
                    let Some(wait) = client::backoff(attempt, wait) else {
                        return Ok(Outcome::stop(format!(
                            "error 429: {message}; the provider asks for a wait longer than {} s",
                            client::MAX_WAIT.as_secs()
                        )));
                    };
                    if self.pause(wait) {
                        let why = if self.gone {
                            "the window closed"
                        } else {
                            "interrupted"
                        };
                        return Ok(Outcome {
                            text: format!("error 429: {message}; {why} before asking again"),
                            retry: true,
                            replied: false,
                        });
                    }
                    attempt += 1;
                }
                Failure::Stop { .. } => {
                    self.settle(request.seq, None, (0, Basis::Nothing), outcome.clone())?;
                    self.spent(id, 0);
                    return Ok(Outcome::stop(outcome));
                }
                Failure::Retryable { usage, .. } | Failure::Interrupted { usage } => {
                    let cost = match usage {
                        Some(client::Usage {
                            cost: Some(cost), ..
                        }) => (cost, Basis::Reported),
                        _ => (reserved, Basis::Reserved),
                    };
                    self.settle(request.seq, usage, cost, outcome.clone())?;
                    self.spent(id, cost.0);
                    return Ok(Outcome {
                        text: outcome,
                        retry: true,
                        replied: false,
                    });
                }
            }
        }
    }

    /// Logs a completion: the assistant message, its usage and the
    /// request's finish. The outcome is the turn's.
    fn reply(
        &mut self,
        request: u64,
        completion: &Completion,
        cost: (u64, Basis),
    ) -> Result<String, String> {
        let assistant = Kind::Assistant {
            request,
            content: completion.content.clone(),
            reasoning: completion.reasoning.clone(),
            details: completion.details.clone(),
            finish: completion.finish.clone(),
            incomplete: false,
        };
        if let Err(e) = self.log(assistant) {
            // A reply past what a log line holds: its cost still counts.
            let outcome = format!("the reply could not be logged: {e}");
            self.settle(request, completion.usage, cost, outcome.clone())?;
            return Ok(outcome);
        }
        let outcome = match completion.finish.as_str() {
            "stop" => "replied".to_string(),
            "length" => "replied, cut short at max_tokens".to_string(),
            "content_filter" => "stopped by the provider's content filter".to_string(),
            "tool_calls" => "asked for a tool; this conversation has none yet".to_string(),
            other => format!("replied ({other})"),
        };
        self.settle(request, completion.usage, cost, completion.finish.clone())?;
        Ok(outcome)
    }

    /// Logs what a stream that did not finish had brought, marked
    /// incomplete; one past what a log line holds is noted instead.
    fn partial(&mut self, request: u64, partial: Completion) -> Result<(), String> {
        let logged = self.log(Kind::Assistant {
            request,
            content: partial.content,
            reasoning: partial.reasoning,
            details: partial.details,
            finish: partial.finish,
            incomplete: true,
        });
        if let Err(e) = logged {
            self.log(Kind::Notice {
                text: format!("the incomplete reply could not be logged: {e}"),
            })?;
        }
        Ok(())
    }

    /// Waits out a rate limit, still hearing the window: whether the
    /// human interrupted the turn meanwhile, or the window closed, when
    /// the next request could not be reserved.
    fn pause(&mut self, wait: Duration) -> bool {
        if self.gone {
            return true;
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.inbox.recv_timeout(left) {
                Ok(Inbound::Down(Down::Interrupt)) => return true,
                Ok(Inbound::Down(Down::Reservation { id, refusal: None })) => self.spent(id, 0),
                Ok(Inbound::Down(down)) => self.queue.push_back(down),
                Ok(Inbound::Fetch { .. }) => {}
                Ok(Inbound::Closed | Inbound::Broken(_)) => {
                    self.gone = true;
                    return true;
                }
                Err(_) => return false,
            }
        }
    }

    /// Sends turn request `request` as a stream and reads its reply as it
    /// comes, drawing what each frame brings in the window, until the
    /// reply ends or fails or the window interrupts it. A window that
    /// closes meanwhile leaves the reply to be read and logged whole.
    fn stream(&mut self, request: u64, client: &Client, key: &Secret, body: String) -> Streamed {
        self.live.store(request, Ordering::SeqCst);
        let url = format!("{}/chat/completions", client.base_url);
        let headers = client::headers(key.expose());
        let (send, live) = (self.sender.clone(), self.live.clone());
        let spawned = std::thread::Builder::new()
            .name("td-agent-stream".into())
            .spawn(move || fetch(&url, &headers, body.as_bytes(), request, &live, &send));
        if let Err(e) = spawned {
            return Streamed::Failed {
                failure: Failure::Stop {
                    status: None,
                    message: format!("the stream's thread: {e}"),
                },
                partial: None,
            };
        }
        let mut reading = Reading {
            head: None,
            plain: None,
            reader: sse::Reader::new(sse::MAX_EVENT, client::MAX_STREAM),
            reply: Assembly::default(),
        };
        let end = loop {
            let inbound = match self.inbox.recv() {
                Ok(inbound) => inbound,
                // The session holds a sender, so this does not happen.
                Err(_) => break reading.broken("the conversation's channel closed".into()),
            };
            match inbound {
                Inbound::Fetch { request: of, item } if of == request => {
                    if let Some(end) = self.fetched(request, item, &mut reading) {
                        break end;
                    }
                }
                Inbound::Fetch { .. } => {}
                Inbound::Down(Down::Interrupt) => {
                    break Streamed::Failed {
                        failure: Failure::Interrupted {
                            usage: reading.reply.usage(),
                        },
                        partial: reading.partial(),
                    }
                }
                Inbound::Down(Down::Reservation { id, refusal: None }) => self.spent(id, 0),
                Inbound::Down(down) => self.queue.push_back(down),
                Inbound::Closed | Inbound::Broken(_) => self.gone = true,
            }
        };
        // Its thread reads no further than the frame it is waiting for.
        self.live.store(0, Ordering::SeqCst);
        end
    }

    /// One step of request `request`'s stream: what it ended in, when it
    /// has ended.
    fn fetched(&mut self, request: u64, item: Fetched, reading: &mut Reading) -> Option<Streamed> {
        match item {
            Fetched::Head { status, headers } => {
                let json = headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("content-type")
                        && value
                            .trim_start()
                            .get(..16)
                            .is_some_and(|kind| kind.eq_ignore_ascii_case("application/json"))
                });
                if status != 200 || json {
                    reading.plain = Some(Vec::new());
                }
                reading.head = Some((status, headers));
                None
            }
            Fetched::Chunk(bytes) => {
                if let Some(plain) = reading.plain.as_mut() {
                    if plain.len().saturating_add(bytes.len()) > client::MAX_REPLY as usize {
                        return Some(
                            reading.broken(format!("a reply past {} bytes", client::MAX_REPLY)),
                        );
                    }
                    plain.extend_from_slice(&bytes);
                    return None;
                }
                let reply = &mut reading.reply;
                let fed = reading.reader.feed(&bytes, &mut |event| match event {
                    sse::Event::Data(text) => reply.event(text),
                    sse::Event::Done => Ok(()),
                });
                let (reasoning, content) = reading.reply.fresh();
                // A frame's events may complete one begun frames before,
                // so what they bring is sent in pieces a frame holds
                // however JSON escapes it.
                let reasoning = pieces(reasoning, DELTA_PIECE).into_iter().map(|r| (r, ""));
                let content = pieces(content, DELTA_PIECE).into_iter().map(|c| ("", c));
                for (reasoning, content) in reasoning.chain(content) {
                    self.send(&Up::Delta {
                        request,
                        reasoning: reasoning.to_string(),
                        content: content.to_string(),
                    });
                }
                match fed {
                    Err(Fault::Sink(failure)) => Some(Streamed::Failed {
                        failure,
                        partial: reading.partial(),
                    }),
                    Err(Fault::Reader(e)) => Some(reading.broken(e.to_string())),
                    // `[DONE]` with no finish is a stream cut short, as a
                    // counted reply with no choice is.
                    Ok(()) if reading.reader.done() && reading.reply.finished() => {
                        Some(Streamed::Replied(reading.reply.completion()))
                    }
                    Ok(()) if reading.reader.done() => {
                        Some(reading.broken("the stream ended before the reply finished".into()))
                    }
                    Ok(()) => None,
                }
            }
            Fetched::End => match (reading.plain.take(), reading.head.take()) {
                (Some(body), Some((status, headers))) => Some(whole(status, headers, body)),
                // Whole without `[DONE]` only once a finish has come.
                _ if reading.reply.finished() => {
                    Some(Streamed::Replied(reading.reply.completion()))
                }
                _ => Some(reading.broken("the stream ended before the reply finished".into())),
            },
            Fetched::Failed(e) => match (reading.plain.take(), reading.head.take()) {
                // Before the head: refused or unsent as a counted request
                // would have been.
                (_, None) => Some(match client::classify(Err(e)) {
                    Ok(completion) => Streamed::Replied(completion),
                    Err(failure) => Streamed::Failed {
                        failure,
                        partial: None,
                    },
                }),
                // An error status's body cut short: the status decides.
                (Some(body), Some((status, headers))) if status != 200 => {
                    Some(whole(status, headers, body))
                }
                _ => Some(reading.broken(e.to_string())),
            },
        }
    }

    /// The conversation's title from `title_model`, after its first
    /// exchange (DESIGN.md §13). Reserved like any request; whatever goes
    /// wrong leaves the first message's line as the title and says why.
    fn title(&mut self, turn: u64) -> Result<(), String> {
        let Some((Ok(key), client)) = self.setup.clone() else {
            return Ok(());
        };
        let events = self.conversation.events();
        let first = events.iter().find_map(|e| match &e.kind {
            Kind::User { text, .. } => Some(text.clone()),
            _ => None,
        });
        let reply = events.iter().rev().find_map(|e| match &e.kind {
            Kind::Assistant {
                content,
                incomplete: false,
                ..
            } => Some(content.clone().unwrap_or_default()),
            _ => None,
        });
        let (Some(first), Some(reply)) = (first, reply) else {
            return Ok(());
        };
        let model = match self.model("title_model", &client.title_model, &client) {
            Ok(model) => model,
            Err(why) => {
                self.log(Kind::Notice {
                    text: format!("no title: {why}"),
                })?;
                return Ok(());
            }
        };
        let pricing = model.as_ref().and_then(|m| m.pricing);
        let head = client::title_head(&client, &first, &reply);
        let bytes = head.len() as u64 + 2;
        let reserved = pricing.map_or(0, |p| p.reserve(bytes.div_ceil(4), client::TITLE_TOKENS));
        let events = self.conversation.events();
        let within = cost::within(
            "max_cost_per_turn",
            client.limits.turn,
            accounts::turn_spent(events, turn),
            reserved,
        )
        .and_then(|()| {
            cost::within(
                "max_cost_per_conversation",
                client.limits.conversation,
                accounts::spent(events),
                reserved,
            )
        })
        .and_then(|()| self.reserve(reserved));
        let id = match within {
            Ok(id) => id,
            Err(why) => {
                self.log(Kind::Notice {
                    text: format!("no title: {why}"),
                })?;
                return Ok(());
            }
        };
        let request = self.log(Kind::Request {
            turn,
            purpose: Purpose::Title,
            prefix: 0,
            head: head.clone(),
            bytes,
            reserved,
        })?;
        self.sync()?;
        let body = format!("{{{head}}}");
        let (usage, cost, outcome) = match client::classify(post(&client, &key, &body)) {
            Ok(completion) => {
                let cost = charge(completion.usage, pricing, reserved);
                match client::title(completion.content.as_deref().unwrap_or_default()) {
                    Some(title) => {
                        self.log(Kind::Title {
                            request: request.seq,
                            text: title.clone(),
                        })?;
                        self.conversation.retitle(&title)?;
                        let title = self.conversation.meta().title.clone();
                        self.send(&Up::Title { title });
                        (completion.usage, cost, "titled".to_string())
                    }
                    None => (completion.usage, cost, "no title in the reply".to_string()),
                }
            }
            Err(failure) => {
                let (usage, cost) = match &failure {
                    Failure::Retryable { usage, .. } => match usage {
                        Some(client::Usage {
                            cost: Some(cost), ..
                        }) => (*usage, (*cost, Basis::Reported)),
                        _ => (*usage, (reserved, Basis::Reserved)),
                    },
                    _ => (None, (0, Basis::Nothing)),
                };
                (usage, cost, failure.outcome())
            }
        };
        self.settle(request.seq, usage, cost, outcome)?;
        self.spent(id, cost.0);
        Ok(())
    }
}

/// The most text one `delta` frame carries in a field: six times it,
/// escaped at its longest, is well within `frame::MAX_FRAME`.
const DELTA_PIECE: usize = 64 * 1024;

/// `text` in pieces of at most `most` bytes, cut at character boundaries.
fn pieces(text: &str, most: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(most.max(4));
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (Some(piece), Some(tail)) = (rest.get(..cut), rest.get(cut..)) else {
            break;
        };
        out.push(piece);
        rest = tail;
    }
    out
}

/// A streamed request's reply that came as one body: as a counted reply
/// is read.
fn whole(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Streamed {
    let response = td_fetch::Response {
        status,
        headers,
        body,
    };
    match client::classify(Ok(response)) {
        Ok(completion) => Streamed::Replied(completion),
        Err(failure) => Streamed::Failed {
            failure,
            partial: None,
        },
    }
}

/// A title request through the fetch service, counted.
fn post(client: &Client, key: &Secret, body: &str) -> Result<td_fetch::Response, td_fetch::Error> {
    let headers = client::headers(key.expose());
    let headers: Vec<(&str, &str)> = headers.iter().map(|(n, v)| (*n, v.as_str())).collect();
    td_fetch::post(
        &format!("{}/chat/completions", client.base_url),
        &headers,
        body.as_bytes(),
        Some(client::MAX_REPLY),
    )
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

    /// The settings with no key: a turn ends at once, saying so.
    fn keyless(stream: &mut UnixStream) {
        let down = Down::Setup {
            key: Err("no API key: write one".into()),
            client: Client::default(),
        };
        frame::write(stream, &down.encode()).unwrap();
    }

    fn finished(up: &Up) -> Option<&str> {
        match up {
            Up::Event(Event {
                kind: Kind::Finished { outcome, .. },
                ..
            }) => Some(outcome),
            _ => None,
        }
    }

    const D1: &str = "11111111111111111111111111111111";
    const D2: &str = "22222222222222222222222222222222";

    #[test]
    fn a_message_is_logged_echoed_and_its_turn_finished_without_a_key() {
        let scratch = Scratch::new("serve");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Conversation)))
        };
        assert!(matches!(next(&mut window), Up::Hello { torn: None, .. }));
        keyless(&mut window);
        say(&mut window, D1, "first words\nand more");
        let Up::Event(user) = next(&mut window) else {
            panic!("no user event")
        };
        assert!(
            matches!(user.kind, Kind::User { ref text, .. } if text == "first words\nand more")
        );
        assert!(matches!(
            next(&mut window),
            Up::Event(Event {
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
        assert_eq!(
            next(&mut window),
            Up::Title {
                title: "first words".into()
            }
        );
        assert_eq!(finished(&next(&mut window)), Some("no API key: write one"));
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
    fn a_delta_is_cut_into_pieces_a_frame_holds() {
        assert!(pieces("", 4).is_empty());
        assert_eq!(pieces("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(
            pieces(&"\u{e9}".repeat(5), 5),
            ["\u{e9}\u{e9}", "\u{e9}\u{e9}", "\u{e9}"]
        );
        // The longest piece, escaped at its longest, fits a frame.
        let worst = "\u{1}".repeat(DELTA_PIECE * 2 + 1);
        let cut = pieces(&worst, DELTA_PIECE);
        assert_eq!(cut.len(), 3);
        for piece in cut {
            let delta = Up::Delta {
                request: 1,
                reasoning: piece.into(),
                content: piece.into(),
            };
            assert!(delta.encode().len() <= frame::MAX_FRAME);
        }
    }

    #[test]
    fn a_turn_without_settings_says_so() {
        let scratch = Scratch::new("nosettings");
        let state = scratch.state();
        let id = Id::random().unwrap();
        let (mut window, theirs) = UnixStream::pair().unwrap();
        let served = {
            let (state, id) = (state.clone(), id.clone());
            std::thread::spawn(move || serve(theirs, &state, &id, Some(Role::Orchestrator)))
        };
        assert!(matches!(next(&mut window), Up::Hello { .. }));
        say(&mut window, D1, "hello");
        let outcome = loop {
            if let Some(outcome) = finished(&next(&mut window)) {
                break outcome.to_string();
            }
        };
        assert_eq!(outcome, NO_SETTINGS);
        drop(window);
        served.join().unwrap().unwrap();
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
        keyless(&mut window);
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
        keyless(&mut window);
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
            keyless(&mut window);
            say(&mut window, delivery, "hi");
            for _ in 0..4 {
                next(&mut window);
            }
            drop(window);
            served.join().unwrap().unwrap();
        }
    }
}
