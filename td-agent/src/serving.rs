//! A conversation's MCP servers as its turns meet them (DESIGN.md §12,
//! MCP servers): started at a turn that finds the workspace ready, ended
//! or started again when what they run with changes, their listings and
//! failures logged, and a failure told to the model.

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use td_json::Json;

use super::{quoted, Inbound, Outcome, Session};
use crate::config::McpServer;
use crate::host;
use crate::protocol::Down;
use crate::servers::{Failed, Launch, Pipe, Report, Told};
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
        let about = format!("MCP server {server} ");
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
