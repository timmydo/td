//! A conversation's MCP servers as its turns meet them (DESIGN.md §12,
//! MCP servers): started at a turn that finds the workspace ready, ended
//! or started again when what they run with changes, their listings and
//! failures logged, and a failure told to the model; the tools a turn
//! offers, and a call of one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use td_json::Json;

use super::{quoted, ruled, Beside, Inbound, Judged, Outcome, Ruling, Session, WORKSPACE_GONE};
use crate::config::McpServer;
use crate::host;
use crate::mcp::{Failure, Offered};
use crate::protocol::Down;
use crate::servers::{Ask, Failed, Launch, Pipe, Report, Told};
use crate::store::{self, Kind, Served};
use crate::workspace::Workspace;

/// How much longer than a start's own time a turn waits for its report.
const WAIT_SLACK: Duration = Duration::from_secs(2);
/// How often the wait looks at what came.
const WAIT_SLICE: Duration = Duration::from_millis(100);

impl Session {
    /// The servers the conversation's template names, once its workspace
    /// is ready: all of them, none for a removed workspace, and nothing
    /// to change without settings, a workspace, or before a repository
    /// workspace's worktrees are prepared.
    fn mcp_wanted(&self) -> Option<Vec<McpServer>> {
        let meta = self.conversation.meta();
        let workspace = meta.workspace.as_ref()?;
        if meta.removed {
            return Some(Vec::new());
        }
        let (_, client) = self.setup.as_ref()?;
        if let Workspace::Repositories(repositories) = workspace {
            let prepared = repositories
                .entries
                .iter()
                .all(|entry| meta.prepared.contains(&entry.repository));
            if !prepared {
                return None;
            }
        }
        Some(client.mcp_for(workspace).to_vec())
    }

    /// The servers at a turn's start (DESIGN.md §12, Lifetime): one no
    /// longer named, changed, or launched with other mounts or network
    /// policy is ended; one named and not running is started, one that
    /// failed within its restarts; and the turn waits for a first start's
    /// listing, and for the listing again of a server whose tools
    /// changed. An outcome when an interrupt ended the wait, which ends
    /// the turn.
    pub(super) fn mcp_turn(&mut self) -> Result<Option<Outcome>, String> {
        // What the servers told before the turn, first, so a failure is
        // started again and a change listed again at this turn.
        self.hear();
        self.log_mcp(&mut Vec::new())?;
        let Some(wanted) = self.mcp_wanted() else {
            return Ok(None);
        };
        // With no key the turn ends at its first step, so nothing starts.
        let Some((Ok(_), client)) = self.setup.clone() else {
            return Ok(None);
        };
        // Prepared as the turn's requests are, so a server runs with what
        // its commands would; one that cannot be is the request's to say.
        if !wanted.is_empty() && self.placed(&client, |_, _| ()).is_err() {
            return Ok(None);
        }
        let launch = self.bench.launch();
        for server in self.servers.plan(&wanted, &launch) {
            if let Err(why) = self.mcp_start(&server, &launch) {
                self.servers.unstarted(&server, &launch);
                let failed = Failed {
                    why: format!("it could not be started: {why}"),
                    tail: String::new(),
                };
                self.mcp_failed(&server, failed)?;
            }
        }
        let mut awaited = Vec::new();
        for server in &wanted {
            let relisted = self.servers.relist(&server.name);
            let Some(running) = self.servers.get(&server.name) else {
                continue;
            };
            // A start not yet listed, while no listing of the server's for
            // its command is logged, is its first, whichever turn began it:
            // one an interrupt ended does not leave the next unwaiting.
            let events = self.conversation.events();
            let first = running.protocol.is_none()
                && store::mcp_listing(events, &server.name, &server.command, false).is_none();
            if relisted || first {
                awaited.push((server.name.clone(), running.generation));
            }
        }
        self.mcp_wait(awaited)
    }

    /// Starts `server` in an instance of its own, launched here, on the
    /// main thread, as every instance is: its generation.
    fn mcp_start(&mut self, server: &McpServer, launch: &Launch) -> Result<u64, String> {
        let (program, args) = server.command.split_first().ok_or("its command is empty")?;
        let call = host::Call::Serve {
            program: program.clone(),
            args: args.to_vec(),
            workdir: None,
        };
        let mut client = self.bench.take(&call, 0)?;
        let id = client.call(call)?;
        let inbox = self.sender.clone();
        let name = server.name.clone();
        let tell = move |generation, report| {
            let _ = inbox.send(Inbound::Mcp(Told {
                server: name.clone(),
                generation,
                report,
            }));
        };
        self.servers
            .add(server.clone(), launch.clone(), Pipe::new(client, id), tell)
    }

    /// Waits for a report of each of `awaited`, a start's time and a
    /// little more at most, logging what comes: an outcome when an
    /// interrupt ended the wait.
    fn mcp_wait(&mut self, mut awaited: Vec<(String, u64)>) -> Result<Option<Outcome>, String> {
        let until = Instant::now() + crate::mcp::START_TIME + WAIT_SLACK;
        loop {
            self.log_mcp(&mut awaited)?;
            if awaited.is_empty() || self.gone {
                return Ok(None);
            }
            // One said with the turn's message, before it began, too.
            if let Some(at) = self
                .queue
                .iter()
                .position(|down| matches!(down, Down::Interrupt))
            {
                self.queue.remove(at);
                self.interrupt = true;
            }
            if self.interrupt {
                return Ok(Some(Outcome::again(
                    "interrupted while waiting for the MCP servers to start",
                )));
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.inbox.recv_timeout(left.min(WAIT_SLICE)) {
                Ok(inbound) => self.heard(inbound),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.gone = true,
            }
        }
    }

    /// Logs what the servers told, each of `awaited` whose report came
    /// no longer awaited.
    pub(super) fn log_mcp(&mut self, awaited: &mut Vec<(String, u64)>) -> Result<(), String> {
        for (server, failed) in std::mem::take(&mut self.mcp_hung) {
            self.mcp_failed(&server, failed)?;
        }
        while let Some(told) = self.mcp_reports.pop_front() {
            if !matches!(told.report, Report::Changed) {
                awaited.retain(|(name, generation)| {
                    *name != told.server || *generation != told.generation
                });
            }
            self.mcp_report(told)?;
        }
        Ok(())
    }

    /// One report, logged; a report of a start since ended is dropped.
    fn mcp_report(&mut self, told: Told) -> Result<(), String> {
        let Told {
            server,
            generation,
            report,
        } = told;
        let Some(running) = self.servers.current(&server, generation) else {
            return Ok(());
        };
        let config = running.config.clone();
        match report {
            Report::Changed => {
                running.changed = true;
                Ok(())
            }
            Report::Started(Ok(started)) => {
                running.protocol = Some(started.protocol.clone());
                self.mcp_listed(&config, started.protocol, &started.tools, started.more)
            }
            Report::Listed(Ok((tools, more))) => {
                let protocol = running.protocol.clone().unwrap_or_default();
                self.mcp_listed(&config, protocol, &tools, more)
            }
            Report::Started(Err(failed)) | Report::Listed(Err(failed)) | Report::Failed(failed) => {
                self.servers.fail(&server);
                self.mcp_failed(&config, failed)
            }
        }
    }

    /// `server`'s listing, logged as its tools would be offered, or as
    /// the same as its last for its command (DESIGN.md §12, The prefix).
    fn mcp_listed(
        &mut self,
        server: &McpServer,
        protocol: String,
        tools: &[Json],
        more: bool,
    ) -> Result<(), String> {
        let (offered, mut left) = crate::mcp::offer(&server.name, tools, server.tools.as_deref());
        let (kept, fitted) = crate::mcp::fit(vec![offered]);
        left.extend(fitted.into_iter().flatten());
        if more {
            left.push(format!(
                "what it listed past {} pages or {} tools",
                crate::mcp::MAX_PAGES,
                crate::mcp::MAX_LISTED
            ));
        }
        if self.servers.relists_spent(&server.name) {
            left.push(
                "a change it says of its tools from here on, until the conversation's process starts again"
                    .into(),
            );
        }
        let tools: Vec<String> = kept
            .into_iter()
            .flatten()
            .map(|tool| tool.definition.to_string())
            .collect();
        let events = self.conversation.events();
        let outcome = match store::mcp_listing(events, &server.name, &server.command, false) {
            Some(last) if last.protocol == protocol && last.tools == tools && last.left == left => {
                Served::Same { of: last.seq }
            }
            _ => Served::Listed {
                protocol,
                tools,
                left,
            },
        };
        self.log(Kind::Mcp {
            server: server.name.clone(),
            command: server.command.clone(),
            outcome,
        })?;
        Ok(())
    }

    /// `server`'s failure, logged with its tail, and told to the model
    /// unless it says what the last told of it said (DESIGN.md §12,
    /// Failures).
    fn mcp_failed(&mut self, server: &McpServer, failed: Failed) -> Result<(), String> {
        let Failed { why, tail } = failed;
        let text = format!("MCP server {} failed: {}", server.name, quoted(&why));
        self.log(Kind::Mcp {
            server: server.name.clone(),
            command: server.command.clone(),
            outcome: Served::Failed {
                why,
                tail: (!tail.is_empty()).then_some(tail),
            },
        })?;
        if !self.mcp_told(&server.name, &text) {
            self.log(Kind::Notification { text, mcp: true })?;
        }
        Ok(())
    }

    /// Whether the last news told of `server` is `text`.
    fn mcp_told(&self, server: &str, text: &str) -> bool {
        let about = about(server);
        self.conversation
            .events()
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                Kind::Notification { text, mcp: true } if text.starts_with(&about) => Some(text),
                _ => None,
            })
            .is_some_and(|last| last == text)
    }
}

impl Session {
    /// Fixes the MCP tools the turn offers (DESIGN.md §12, The prefix):
    /// for each server the template names, in its order, once its
    /// workspace is ready, the tools of its last listing for its command
    /// that its `tools` keeps now, unless a failure of it is logged after;
    /// all of them within `mcp::fit`, and then within a log line beside
    /// the rest of the prefix, the last server's tools left out first.
    /// What is left out is told, once while it says the same.
    pub(super) fn mcp_offer(&mut self) -> Result<(), String> {
        self.mcp_offered.clear();
        let events = self.conversation.events();
        let mut named = Vec::new();
        let mut listed = Vec::new();
        for server in self.mcp_wanted().unwrap_or_default() {
            let Some(listing) = store::mcp_listing(events, &server.name, &server.command, true)
            else {
                continue;
            };
            let tools = kept(&server, listing.tools);
            named.push(server);
            listed.push(tools);
        }
        let (fitted, said) = crate::mcp::fit(listed);
        let mut left: Vec<(String, Vec<String>)> = Vec::new();
        let mut groups = Vec::new();
        for ((server, tools), said) in named.into_iter().zip(fitted).zip(said) {
            if !said.is_empty() {
                left.push((server.name.clone(), said));
            }
            if !tools.is_empty() {
                groups.push((server, tools));
            }
        }
        // With no settings no request is made, so none is measured; a
        // prefix that cannot be made is the request's to say.
        if let Some((_, client)) = self.setup.clone().filter(|_| !groups.is_empty()) {
            let skills = self.skills_index();
            let fitted = self.placed(&client, |created, place| {
                let mut groups = groups.clone();
                let mut dropped = Vec::new();
                while !groups.is_empty() {
                    let mcp = definitions(&groups);
                    let prefix =
                        crate::prompt::prefix_with(created, place, skills.as_deref(), &mcp);
                    if fits_a_line(prefix) {
                        break;
                    }
                    if let Some((server, _)) = groups.pop() {
                        dropped.push(server.name);
                    }
                }
                (groups, dropped)
            });
            if let Ok((fitted, dropped)) = fitted {
                groups = fitted;
                for server in dropped {
                    // One news of a server: what `fit` cut of it is gone
                    // with the rest.
                    left.retain(|(name, _)| *name != server);
                    let why = "all of them: the request's prefix with them would be past what a log line holds".to_string();
                    left.push((server, vec![why]));
                }
            }
        }
        self.mcp_offered = groups;
        for (server, said) in left {
            let text = omitted(&server, &said);
            if !self.mcp_told(&server, &text) {
                self.log(Kind::Notification { text, mcp: true })?;
            }
        }
        Ok(())
    }

    /// The server and the tool `name` names, when it is one of the turn's
    /// MCP tools and `kit` is a conversation's, which a sub-agent's is not
    /// (DESIGN.md §12, `task`).
    pub(super) fn mcp_tool(
        &self,
        kit: crate::tools::Kit,
        name: &str,
    ) -> Option<(McpServer, String)> {
        if matches!(kit, crate::tools::Kit::Task | crate::tools::Kit::Explore) {
            return None;
        }
        self.mcp_offered.iter().find_map(|(server, tools)| {
            let tool = tools.iter().find(|tool| tool.name == name)?;
            Some((server.clone(), tool.tool.clone()))
        })
    }

    /// The turn's MCP tools' names, when it offers any.
    pub(super) fn mcp_names(&self) -> Option<String> {
        let names: Vec<&str> = self
            .mcp_offered
            .iter()
            .flat_map(|(_, tools)| tools.iter().map(|tool| tool.name.as_str()))
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    }

    /// A call of MCP server `server`'s tool `tool`, the `ToolCall` record
    /// `started` (DESIGN.md §12, Approval, Results): decided as a `shell`
    /// call is, its arguments passed as the model wrote them, a JSON
    /// object within `MAX_ARGUMENTS`, and its result cut as a command's
    /// output is. An interrupt cancels it; a window gone does not, so the
    /// call ends whole in the log.
    pub(super) fn mcp_call(
        &mut self,
        started: u64,
        server: &McpServer,
        tool: &str,
        arguments: &str,
        repeated: bool,
    ) -> Result<(Result<String, String>, Beside), String> {
        let failed = |why: String| Ok((Err(why), Beside::default()));
        if self.conversation.meta().removed {
            return failed(WORKSPACE_GONE.into());
        }
        if arguments.len() > crate::tools::MAX_ARGUMENTS {
            return failed(format!(
                "the arguments are {} bytes, past the {}-byte bound",
                arguments.len(),
                crate::tools::MAX_ARGUMENTS
            ));
        }
        // An empty text is the no-arguments call some providers send.
        let text = if arguments.trim().is_empty() {
            "{}"
        } else {
            arguments
        };
        let arguments = match td_json::parse(text) {
            Ok(object @ Json::Obj(_)) => object,
            Ok(_) => return failed("the arguments are not a JSON object".into()),
            Err(e) => return failed(format!("the arguments are not JSON: {e}")),
        };
        let judged = Judged::Mcp {
            server: &server.name,
            tool,
            repeated,
        };
        let ruled_at = self.human.0;
        let ruling = self.ruling(&judged);
        if let Ruling::Deny(why) = &ruling {
            self.log(Kind::Approval {
                call: started,
                outcome: "deny".into(),
                by: "rule".into(),
                probabilities: None,
                reason: Some(why.clone()),
            })?;
            return failed(ruled(why));
        }
        let card = || {
            crate::tools::mcp_card(
                &server.name,
                &server.command,
                tool,
                &arguments,
                server.timeout,
            )
        };
        let rule = format!("mcp {} {tool}", server.name);
        let proposed = crate::rules::Always::checked(true, vec![rule]).ok();
        if let Err(why) = self.sanction(
            started, &judged, ruling, ruled_at, repeated, &card, proposed,
        )? {
            return failed(why);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let begun = Arc::new(AtomicBool::new(false));
        let (answer, answered) = mpsc::channel();
        let asked = Instant::now();
        let until = asked + Duration::from_secs(server.timeout);
        let ask = Ask::Call {
            tool: tool.to_string(),
            arguments,
            until,
            stop: Arc::clone(&stop),
            begun: Arc::clone(&begun),
            answer,
        };
        if !self.servers.ask(&server.name, ask) {
            return failed(format!(
                "MCP server {} is not running: it ended since this turn began, or it failed, as its news says",
                server.name
            ));
        }
        // Past the call's own time and a little once its thread began it,
        // or past a start's and a listing's besides before, its server is
        // taken to hang, and is ended as failed.
        let hung_begun = until + CALL_SLACK;
        let hung = until + crate::mcp::START_TIME * 2 + CALL_SLACK;
        let mut stopped: Option<Instant> = None;
        let outcome = loop {
            self.hear();
            // A connection a command waits on is asked of here, as a
            // command's wait does.
            self.classify_links()?;
            if self.interrupt && stopped.is_none() {
                stop.store(true, Ordering::Relaxed);
                stopped = Some(Instant::now());
            }
            match answered.recv_timeout(WAIT_SLICE) {
                Ok(outcome) => break outcome,
                Err(RecvTimeoutError::Disconnected) => {
                    break Err(Failure::Broke("it ended".into()))
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            // A thread still starting, or blocked, is left to end the
            // call by itself; the person's interrupt is not left waiting.
            if stopped.is_some_and(|at| now >= at + STOP_GRACE) {
                break Err(Failure::Stopped);
            }
            if now >= hung || (begun.load(Ordering::Relaxed) && now >= hung_begun) {
                let why = "it did not answer in its time";
                self.servers.fail(&server.name);
                // Its news waits for the turn's next step: none comes
                // between a call and its result (DESIGN.md §12, Failures).
                self.mcp_hung.push((
                    server.clone(),
                    Failed {
                        why: why.to_string(),
                        tail: String::new(),
                    },
                ));
                break Err(Failure::Broke(why.into()));
            }
        };
        let text = match outcome {
            Ok(Ok(result)) => {
                let (text, error) = crate::mcp::result_text(&result);
                let (shown, kept) = cut(&text);
                let beside = Beside {
                    kept,
                    ..Beside::default()
                };
                return Ok((if error { Err(shown) } else { Ok(shown) }, beside));
            }
            Ok(Err(error)) => error.to_string(),
            Err(Failure::Broke(why)) => format!("MCP server {} failed: {why}", server.name),
            Err(failure) => format!("not answered: {failure}"),
        };
        failed(text)
    }
}

/// How much longer than a call's own time its wait lasts before its
/// server is taken to hang.
const CALL_SLACK: Duration = Duration::from_secs(5);
/// How long an interrupted call's wait lasts for its thread to cancel it.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The tools of `server`'s listing, `listed` their definitions as the log
/// holds them, that its `tools` keeps now: a listing logged before its
/// `tools` was narrowed offers no tool it no longer keeps.
fn kept(server: &McpServer, listed: &[String]) -> Vec<Offered> {
    listed
        .iter()
        .filter_map(|text| offered(&server.name, text))
        .filter(|tool| {
            server
                .tools
                .as_ref()
                .is_none_or(|keep| keep.contains(&tool.tool))
        })
        .collect()
}

/// The news of `server`'s tools that are not offered, `said` why: it
/// begins as all of a server's news does, so `mcp_told` finds it.
fn omitted(server: &str, said: &[String]) -> String {
    format!(
        "{}has tools not offered: {}",
        about(server),
        quoted(&said.join("; "))
    )
}

/// How each news of `server` begins.
fn about(server: &str) -> String {
    format!("MCP server {server} ")
}

/// A tool of `server`'s as the log's listing holds it, `text` its
/// definition: none when it is not one the prefix could carry.
fn offered(server: &str, text: &str) -> Option<Offered> {
    let definition = td_json::parse(text).ok()?;
    let name = definition
        .get("function")?
        .get("name")?
        .as_str()?
        .to_string();
    let tool = name.strip_prefix(&format!("mcp__{server}__"))?.to_string();
    Some(Offered {
        name,
        tool,
        definition,
    })
}

/// The definitions of `groups`' tools, in order, as the prefix carries
/// them.
pub(super) fn definitions(groups: &[(McpServer, Vec<Offered>)]) -> Vec<Json> {
    groups
        .iter()
        .flat_map(|(_, tools)| tools.iter().map(|tool| tool.definition.clone()))
        .collect()
}

/// Whether `prefix` fits a log line at the store's worst case.
fn fits_a_line(prefix: String) -> bool {
    let event = store::Event {
        seq: u64::MAX,
        time: u64::MAX,
        kind: Kind::Prefix { text: prefix },
    };
    event.to_json().to_string().len() <= store::MAX_LINE
}

/// A result's text as a `shell` call's output is cut (DESIGN.md §12): what
/// the model is shown, its head and tail, and what the log keeps beyond
/// it when that is more.
fn cut(text: &str) -> (String, Option<String>) {
    let mut kept = crate::shell::Kept::new(crate::shell::KEPT_HEAD, crate::shell::KEPT_TAIL);
    kept.push(text.as_bytes());
    let shown = kept
        .shown(crate::shell::SHOWN_HEAD, crate::shell::SHOWN_TAIL)
        .text();
    let whole = kept.text();
    let beyond = (whole != shown).then_some(whole);
    (shown, beyond)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn listed(names: &[&str]) -> Vec<String> {
        names
            .iter()
            .map(|name| format!(r#"{{"type":"function","function":{{"name":"mcp__gh__{name}"}}}}"#))
            .collect()
    }

    /// A logged listing offers only what the server's `tools` keeps now,
    /// so narrowing it takes effect before the server lists again.
    #[test]
    fn a_listing_offers_only_what_tools_keeps_now() {
        let names = |server: &McpServer| -> Vec<String> {
            kept(server, &listed(&["list", "delete"]))
                .into_iter()
                .map(|tool| tool.tool)
                .collect()
        };
        let every = crate::config::mcp_server("gh", vec!["gh-mcp".into()], None, None).unwrap();
        assert_eq!(names(&every), ["list", "delete"]);
        let narrowed =
            crate::config::mcp_server("gh", vec!["gh-mcp".into()], Some(vec!["list".into()]), None)
                .unwrap();
        assert_eq!(names(&narrowed), ["list"]);
        // Another server's tool is none of this one's.
        let other = crate::config::mcp_server("web", vec!["w".into()], None, None).unwrap();
        assert!(kept(&other, &listed(&["list"])).is_empty());
    }

    /// The news of tools left out begins as `mcp_told` reads a server's
    /// news, and only that server's.
    #[test]
    fn news_of_tools_left_out_is_found_as_the_servers() {
        let text = omitted("gh", &["list: past 128 tools".to_string()]);
        assert!(text.starts_with(&about("gh")), "{text}");
        assert!(!text.starts_with(&about("g")), "{text}");
        assert!(text.contains("list: past 128 tools"), "{text}");
    }
}
