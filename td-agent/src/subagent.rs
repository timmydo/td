//! `task`'s sub-agent (DESIGN.md §12): a loop of its own requests and
//! tool calls within the caller's turn, logged in the caller's log under
//! its `Task` event and never in the conversation's view, whose final
//! reply is the call's result.

use std::collections::BTreeMap;

use super::{
    charge, failed_cost, max_tokens, Asking, Session, Streamed, CALL_RECORDS, WRAP_TOKENS,
};
use crate::client::{self, Failure, Params};
use crate::cost;
use crate::store::{Basis, Effect, Kind, Purpose};
use crate::tools::SubAgent;

/// The most requests one sub-agent makes, its last asking for its
/// report.
const TASK_STEPS: usize = 64;

/// The most of a report the caller is given; history_read has the rest.
const MAX_REPORT: usize = 64 * 1024;

/// The most of a stopped sub-agent's last words its call's answer quotes.
const LAST_WORDS: usize = 4 * 1024;

/// What a sub-agent's call answers when it was stopped.
const INTERRUPTED: &str = "the sub-agent was interrupted before it reported";

/// What a call the sub-agent's last request made anyway answers.
const CALL_PAST_LAST: &str =
    "not run: the sub-agent's last request was for its report, and asked for no tool calls";

impl Session {
    /// Runs a sub-agent on `prompt` for the `task` call `started`: its
    /// final report, or why there is none. It keeps its own digests,
    /// from none, so the caller's writes still match only what the caller
    /// read or wrote; and whatever it changed stays when it fails.
    pub(super) fn task(
        &mut self,
        started: u64,
        prompt: String,
        agent: SubAgent,
    ) -> Result<Result<String, String>, String> {
        let theirs = self.bench.swap_digests(BTreeMap::new());
        let done = self.delegate(started, prompt, agent);
        self.bench.swap_digests(theirs);
        Ok(done?.map_err(|why| match agent {
            SubAgent::General => format!(
                "{why}. Whatever the sub-agent changed in the workspace stays as it left it."
            ),
            // It changes nothing.
            SubAgent::Explore => why,
        }))
    }

    /// What an explore sub-agent asks with: `explore_model` and
    /// `explore_routing` where they are set, else the conversation's own,
    /// checked as a turn's model is (DESIGN.md §12, `task`).
    fn explorer(&mut self, asking: Asking) -> Result<Asking, String> {
        let client = &asking.client;
        let routing = client
            .explore_routing
            .clone()
            .or_else(|| asking.routing.clone())
            .filter(|r| r != crate::config::DEFAULT_ROUTING);
        let Some(name) = client.explore_model.clone() else {
            if routing == asking.routing {
                return Ok(asking);
            }
            return self.explore_on(asking.name.clone(), "`explore_model`", routing, asking);
        };
        self.explore_on(name, "`explore_model`", routing, asking)
    }

    /// `asking` on model `name`, which `setting` names, and `routing`.
    fn explore_on(
        &mut self,
        name: String,
        setting: &str,
        routing: Option<String>,
        asking: Asking,
    ) -> Result<Asking, String> {
        let client = asking.client.clone();
        let model = self.model(setting, &name, &client)?;
        let model = match &routing {
            None => model,
            Some(routing) => {
                let reasoning = model.as_ref().is_none_or(|m| m.supports("reasoning"));
                // The conversation's hint names its own menu; this
                // routing may be `explore_routing`'s.
                Some(
                    self.routed(&client, &name, routing, reasoning)
                        .map_err(|e| {
                            format!(
                                "{e} (the explore sub-agent routes by `explore_routing` where it is set, else as the conversation does)"
                            )
                        })?,
                )
            }
        };
        if let Some(missing) = model
            .as_ref()
            .and_then(|m| ["tools", "max_tokens"].into_iter().find(|p| !m.supports(p)))
        {
            return Err(format!(
                "{name} takes no {missing} (the provider's models list gives it no `{missing}` parameter); set {setting} to a model that does"
            ));
        }
        Ok(Asking {
            name,
            model,
            routing,
            ..asking
        })
    }

    /// `task`'s work. Each request is reserved and limited as a turn's,
    /// counted against the caller's turn; its calls are the workspace's
    /// tools, run and approved as the caller's are. Its last request,
    /// at the step limit or when a limit refuses the next, offers no tools
    /// and is bounded as a turn's last is, so that it reports where the
    /// work stands.
    fn delegate(
        &mut self,
        started: u64,
        prompt: String,
        agent: SubAgent,
    ) -> Result<Result<String, String>, String> {
        let Some(turn) = self.current_turn() else {
            return Ok(Err("a sub-agent runs only within a turn".into()));
        };
        let asking = match self.asking()? {
            Ok(asking) => asking,
            Err(why) => return Ok(Err(why)),
        };
        let asking = match agent {
            SubAgent::General => asking,
            SubAgent::Explore => match self.explorer(asking) {
                Ok(asking) => asking,
                Err(why) => return Ok(Err(why)),
            },
        };
        let Asking {
            key,
            client,
            name,
            reasoning_effort,
            model,
            routing,
        } = asking;
        let prefix = match self.placed(&client, |created, place| {
            place.map(|place| crate::prompt::task_prefix(created, place, agent))
        }) {
            Ok(Some(prefix)) => prefix,
            Ok(None) => {
                return Ok(Err(
                    "a sub-agent works in a workspace, and this conversation has none".into(),
                ))
            }
            Err(why) => return Ok(Err(why)),
        };
        let task = self
            .log(Kind::Task {
                call: started,
                agent: agent.word().into(),
                model: name.clone(),
                prefix,
                prompt,
            })?
            .seq;
        self.sync()?;
        let pricing = model.as_ref().and_then(|m| m.pricing);
        let effort = model
            .as_ref()
            .is_none_or(|m| m.supports("reasoning"))
            .then_some(reasoning_effort.as_str());
        // Where `tool_choice` cannot be sent, the last request offers its
        // tools still, and a call it makes anyway is answered as not run.
        let choice = model.as_ref().is_some_and(|m| m.supports("tool_choice"));
        let (mut steps, mut attempt) = (0usize, 0u32);
        // Why the next request is the last, when a limit says so.
        let mut wrap: Option<String> = None;
        let mut noted = false;
        loop {
            self.hear();
            if self.interrupt {
                return Ok(Err(self.stopped(task, INTERRUPTED)));
            }
            let last = wrap.is_some() || steps + 1 >= TASK_STEPS;
            // The last request is told it is, in a note its view alone
            // carries, as a turn's wrap-up is (DESIGN.md §2).
            if last && !noted {
                let why = wrap.clone().unwrap_or_else(|| {
                    format!(
                        "you have made {steps} requests, one short of the most a sub-agent makes"
                    )
                });
                let files = match agent {
                    SubAgent::General => ", and the state of any file you changed that is not finished or not checked",
                    SubAgent::Explore => "",
                };
                self.log(Kind::TaskNote {
                    task,
                    text: format!("[td-agent: {why}, so your work stops here. Without calling a tool, report where it stands: what you found and did, what is left to do{files}.]"),
                })?;
                noted = true;
            }
            let events = self.conversation.events();
            let (prefix_text, messages) =
                client::task_view(events, task).ok_or("the sub-agent's start is not in the log")?;
            let mut max_tokens = max_tokens(model.as_ref());
            if last {
                max_tokens = max_tokens.min(WRAP_TOKENS);
            }
            let build = |max_tokens: u64| -> Result<(String, String), String> {
                let head = client::head(&Params {
                    model: &name,
                    max_tokens,
                    effort,
                    client: &client,
                    cache: true,
                    tools: !last || !choice,
                });
                let head = match &routing {
                    Some(mode) => crate::review_routing::routed(
                        &head,
                        &crate::review_routing::Route::conversation(mode, false, max_tokens),
                        pricing,
                        None,
                    )?,
                    None => head,
                };
                let body = client::turn_body(&head, prefix_text, &messages)?;
                Ok((head, body))
            };
            let (mut head, mut body) = build(max_tokens)?;
            let estimate = client::task_estimate(events, task, body.len() as u64);
            // Not compacted: a sub-agent past its model's context stops.
            if let Some(context) = model.as_ref().and_then(|m| m.context_length) {
                if estimate >= context {
                    let why = format!(
                        "the sub-agent's work is about {estimate} tokens, past {name}'s context of {context}"
                    );
                    return Ok(Err(self.stopped(task, &why)));
                }
                if estimate.saturating_add(max_tokens) > context {
                    max_tokens = context - estimate;
                    (head, body) = build(max_tokens)?;
                }
            }
            let reserved = pricing.map_or(0, |p| p.reserve(estimate, max_tokens));
            let within = cost::within(
                "max_cost_per_turn",
                client.limits.turn,
                crate::accounts::turn_spent(events, turn),
                reserved,
            )
            .and_then(|()| {
                cost::within(
                    "max_cost_per_conversation",
                    client.limits.conversation,
                    crate::accounts::spent(events),
                    reserved,
                )
            });
            if let Err(why) = within {
                // One request more, told to call no tool and bounded, for its
                // report, as a turn's last.
                if !last {
                    wrap = Some(why);
                    continue;
                }
                return Ok(Err(self.stopped(task, &why)));
            }
            // Room for the request, its reply and every call it may make,
            // and for every call of the caller's reply still to answer.
            let calls = (crate::assemble::MAX_ENTRIES as u64).saturating_mul(CALL_RECORDS);
            let room = (body.len() as u64)
                .saturating_add(client::MAX_REPLY.saturating_mul(2))
                .saturating_add(calls.saturating_mul(2));
            if !self.conversation.has_room_for(room) {
                return Ok(Err(self.stopped(task, "the conversation's log is full")));
            }
            let id = match self.reserve(reserved) {
                Ok(id) => id,
                Err(why) if !last && why.starts_with(&format!("{} ", cost::DAY)) => {
                    wrap = Some(why);
                    continue;
                }
                Err(why) => return Ok(Err(self.stopped(task, &why))),
            };
            if self.interrupt {
                self.spent(id, 0);
                return Ok(Err(self.stopped(task, INTERRUPTED)));
            }
            let request = self
                .log(Kind::Request {
                    turn,
                    purpose: Purpose::Task,
                    prefix: task,
                    head,
                    bytes: body.len() as u64,
                    reserved,
                })?
                .seq;
            self.sync()?;
            let failure = match self.stream(request, &client, &key, body) {
                Streamed::Replied(completion) => {
                    let cost = charge(completion.usage, pricing, reserved);
                    let outcome = self.reply(request, &completion, cost)?;
                    self.spent(id, cost.0);
                    steps += 1;
                    attempt = 0;
                    if let Some(reply) = outcome.calls {
                        if last {
                            self.unrun(reply, CALL_PAST_LAST)?;
                            let why = match &wrap {
                                Some(why) => format!("{why}, and its last request called tools instead of reporting"),
                                None => format!("the sub-agent took {TASK_STEPS} steps, the most one takes, and its last called tools instead of reporting"),
                            };
                            return Ok(Err(self.stopped(task, &why)));
                        }
                        if !self.answer_in(reply, agent.kit())? {
                            return Ok(Err(self.stopped(task, INTERRUPTED)));
                        }
                        continue;
                    }
                    if !outcome.replied {
                        let why = format!("the sub-agent's reply: {}", outcome.text);
                        return Ok(Err(self.stopped(task, &why)));
                    }
                    let report = completion.content.unwrap_or_default();
                    if report.trim().is_empty() {
                        let why =
                            format!("the sub-agent ended without a report ({})", outcome.text);
                        return Ok(Err(self.stopped(task, &why)));
                    }
                    return Ok(Ok(self.report(
                        request,
                        report,
                        &completion.finish,
                        wrap.as_deref(),
                        last,
                    )));
                }
                Streamed::Failed { failure, partial } => {
                    if let Some(partial) = partial {
                        self.partial(request, partial)?;
                    }
                    failure
                }
            };
            let outcome = failure.outcome();
            match failure {
                // Asked again after the provider's wait, as a turn's is.
                Failure::RateLimited { message, wait } => {
                    self.settle(request, None, (0, Basis::Nothing), outcome)?;
                    self.sync()?;
                    self.spent(id, 0);
                    if attempt >= client::RETRIES {
                        let why = format!(
                            "the sub-agent's request: error 429: {message}; still rate-limited after {} retries",
                            client::RETRIES
                        );
                        return Ok(Err(self.stopped(task, &why)));
                    }
                    let Some(wait) = client::backoff(attempt, wait) else {
                        let why = format!(
                            "the sub-agent's request: error 429: {message}; the provider asks for a wait longer than {} s",
                            client::MAX_WAIT.as_secs()
                        );
                        return Ok(Err(self.stopped(task, &why)));
                    };
                    // Its interrupt is the caller's turn's too.
                    if self.linger(wait) {
                        self.interrupt = true;
                        let why = if self.gone {
                            "the window closed"
                        } else {
                            INTERRUPTED
                        };
                        return Ok(Err(self.stopped(task, why)));
                    }
                    attempt += 1;
                }
                failure => {
                    // A stream the person stopped: the caller's turn stops
                    // with it (DESIGN.md §12, `task`).
                    if matches!(failure, Failure::Interrupted { .. }) {
                        self.interrupt = true;
                    }
                    let (usage, cost) = failed_cost(&failure, reserved);
                    self.settle(request, usage, cost, outcome.clone())?;
                    self.spent(id, cost.0);
                    let why = format!("the sub-agent's request {outcome}");
                    return Ok(Err(self.stopped(task, &why)));
                }
            }
        }
    }

    /// The report the sub-agent's whole final reply to `request` gives,
    /// bounded, with what td-agent says of how it ended after it.
    fn report(
        &self,
        request: u64,
        mut report: String,
        finish: &str,
        wrap: Option<&str>,
        last: bool,
    ) -> String {
        let mut notes = Vec::new();
        match finish {
            "stop" => {}
            "length" => notes.push("the report was cut short at max_tokens".to_string()),
            "content_filter" => {
                notes.push("the provider's content filter stopped the report".to_string())
            }
            other => notes.push(format!("the report ended ({other})")),
        }
        match wrap {
            Some(why) => notes.push(format!(
                "{why}, so the sub-agent was asked, without calling a tool, where the work stands"
            )),
            None if last => notes.push(format!(
                "the sub-agent took {TASK_STEPS} steps, the most one takes, and was asked, without calling a tool, where the work stands"
            )),
            None => {}
        }
        if report.len() > MAX_REPORT {
            let mut end = MAX_REPORT;
            while !report.is_char_boundary(end) {
                end -= 1;
            }
            let reply = self
                .conversation
                .events()
                .iter()
                .rev()
                .find_map(|e| match &e.kind {
                    Kind::Assistant { request: r, .. } if *r == request => Some(e.seq),
                    _ => None,
                })
                .unwrap_or(0);
            notes.push(format!(
                "the report is {} bytes, of which the first {end} are given; history_read reads it whole at #{reply}",
                report.len()
            ));
            report.truncate(end);
        }
        if !notes.is_empty() {
            report.push_str(&format!("\n\n[td-agent: {}.]", notes.join("; ")));
        }
        report
    }

    /// Why a sub-agent stopped, with the last text it wrote, if any, so
    /// that what it found is not lost.
    fn stopped(&self, task: u64, why: &str) -> String {
        let events = self.conversation.events();
        let mut requests = Vec::new();
        let mut words: Option<&str> = None;
        for event in events.iter().filter(|e| e.seq > task) {
            match &event.kind {
                Kind::Request {
                    purpose: Purpose::Task,
                    prefix,
                    ..
                } if *prefix == task => requests.push(event.seq),
                Kind::Assistant {
                    request,
                    content: Some(content),
                    ..
                } if requests.contains(request) && !content.trim().is_empty() => {
                    words = Some(content.as_str())
                }
                _ => {}
            }
        }
        match words {
            None => why.to_string(),
            Some(words) => {
                let mut end = words.len().min(LAST_WORDS);
                while !words.is_char_boundary(end) {
                    end -= 1;
                }
                let cut = if end < words.len() { " (cut)" } else { "" };
                format!(
                    "{why}; the last it wrote{cut}:\n{}",
                    words.get(..end).unwrap_or_default()
                )
            }
        }
    }

    /// The turn under way: its `Started` event's sequence number.
    fn current_turn(&self) -> Option<u64> {
        self.conversation
            .events()
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                Kind::Started {
                    effect: Effect::Turn,
                    ..
                } => Some(e.seq),
                _ => None,
            })
    }
}
