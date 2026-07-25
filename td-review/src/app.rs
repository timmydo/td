//! TUI state machine: branch list -> branch review -> land log.

use std::io;

use crate::git::{self, now_unix, Branch, Git};
use crate::land::{self, Outcome, Preview};
use crate::term::{
    self, Frame, Key, Line, Style, Ui, CYAN, GRAY, GREEN, MAGENTA, RED, YELLOW,
};

pub enum Flow {
    Continue,
    Quit,
}

enum Screen {
    List,
    Review,
    Log,
    Help,
}

/// A pending yes/no decision, shown as a bar across the bottom row.
enum Prompt {
    Land,
    /// Publish the base. The remotes are pinned when the prompt is raised, so
    /// one added or retargeted while it is open cannot be pushed to unseen.
    Push { remotes: Vec<(String, String)> },
    Conflict,
    /// Delete a landed (or hand-picked) branch. The targets are pinned when the
    /// prompt is raised, so confirming cannot delete more than was shown.
    Delete { short: String, targets: Vec<land::DeleteTarget>, landed: bool },
}

/// Names of the remotes a push would actually reach.
fn pushable(remotes: &[(String, String)]) -> Vec<&str> {
    remotes
        .iter()
        .filter(|(_, url)| url.as_str() != git::NO_PUSH)
        .map(|(name, _)| name.as_str())
        .collect()
}

/// Widest the branch-name column may grow, however long the longest name is.
const MAX_NAME_COL: usize = 48;

/// How wide a delete casts. `D` on a row deletes from the remote that row names;
/// the post-land sweep spans every remote holding the commit that was landed.
enum Only {
    RefsRemote,
    EveryRemote,
}

struct Reviewing {
    refname: String,
    /// The tip the pane was rendered from. The landing refuses if the ref has
    /// moved since — worktrees share refs, so a concurrent fetch can move it.
    oid: String,
    /// The base tip the pane was rendered against, re-checked for the same reason.
    base_oid: String,
    range: String,
    lines: Vec<Line>,
    empty: bool,
}

pub struct App {
    git: Git,
    base: String,
    branches: Vec<Branch>,
    view: Vec<usize>,
    sel: usize,
    top: usize,
    filter: String,
    editing_filter: bool,
    screen: Screen,
    reviewing: Option<Reviewing>,
    scroll: usize,
    log: Vec<Line>,
    log_title: String,
    log_scroll: usize,
    help_scroll: usize,
    prompt: Option<Prompt>,
    just_prompted: bool,
    /// The commit `a` produced, held across the push confirmation so the push
    /// publishes exactly that and not whatever HEAD became meanwhile.
    committed: Option<String>,
    status: String,
    status_style: Style,
    now: i64,
    /// Upstream name and how far the local base trails it. Landing onto a
    /// stale base builds a commit every remote then rejects.
    base_stale: Option<(String, u32)>,
}

impl App {
    pub fn new(git: Git, base: String) -> App {
        App {
            git,
            base,
            branches: Vec::new(),
            view: Vec::new(),
            sel: 0,
            top: 0,
            filter: String::new(),
            editing_filter: false,
            screen: Screen::List,
            reviewing: None,
            scroll: 0,
            log: Vec::new(),
            log_title: String::new(),
            log_scroll: 0,
            help_scroll: 0,
            prompt: None,
            just_prompted: false,
            committed: None,
            status: String::new(),
            status_style: Style::PLAIN,
            now: now_unix(),
            base_stale: None,
        }
    }

    /// True when the key just handled raised a confirmation. The event loop
    /// drops the rest of the read batch so a paste or typeahead can never
    /// answer a prompt the user has not seen yet.
    pub fn just_prompted(&self) -> bool {
        self.just_prompted
    }

    /// Raise a confirmation. The pending typeahead is dropped by `settle_prompt`
    /// once the prompt is actually on screen.
    fn ask(&mut self, prompt: Prompt) {
        self.prompt = Some(prompt);
        self.just_prompted = true;
    }

    /// Drop typeahead now that the prompt is on screen. A failure here is fatal
    /// rather than a note: `drain_input` leaves the tty at VMIN=0 when its
    /// restore fails, and every later read would return empty — indistinguishable
    /// from a closed tty, so the loop would exit successfully mid-confirmation.
    pub fn settle_prompt(&mut self, term: &mut dyn Ui) -> io::Result<()> {
        if !self.just_prompted {
            return Ok(());
        }
        self.just_prompted = false;
        term.drain_input()
    }

    pub fn reload(&mut self) -> io::Result<()> {
        self.branches = self.git.branches(&self.base)?;
        self.base_stale = self.git.base_behind_upstream(&self.base).unwrap_or(None);
        self.now = now_unix();
        self.apply_filter();
        Ok(())
    }

    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.view = self
            .branches
            .iter()
            .enumerate()
            .filter(|(_, b)| needle.is_empty() || b.refname.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.sel >= self.view.len() {
            self.sel = self.view.len().saturating_sub(1);
        }
    }

    fn note(&mut self, text: impl Into<String>, style: Style) {
        self.status = text.into();
        self.status_style = style;
    }

    // ---------------------------------------------------------------- render

    pub fn render(&self, rows: usize, cols: usize) -> String {
        let mut f = Frame::new(rows, cols);
        match self.screen {
            Screen::List => self.render_list(&mut f),
            Screen::Review => self.render_review(&mut f),
            Screen::Log => self.render_log(&mut f),
            Screen::Help => self.render_help(&mut f),
        }
        f.finish()
    }

    fn title(&self, f: &mut Frame, text: &str) {
        f.push_text(text, Style::bar(CYAN));
    }

    /// Fill the frame up to its last row, then draw the status/prompt bar.
    fn footer(&self, f: &mut Frame, keys: &str) {
        while f.room() > 1 {
            f.push_blank();
        }
        if let Some(prompt) = &self.prompt {
            let text = match prompt {
                Prompt::Land => {
                    let name = self.reviewing.as_ref().map_or("?", |r| r.refname.as_str());
                    format!(" land {name} into {} ?  [y] squash + commit   [n] cancel", self.base)
                }
                Prompt::Push { remotes } => format!(
                    " push {} to {} ?  [y] push   [n] keep it local",
                    self.base,
                    pushable(remotes).join(", ")
                ),
                Prompt::Conflict => {
                    " unfinished squash:  [l]/Esc leave it and quit   [d] discard (reset --hard HEAD)"
                        .to_string()
                }
                Prompt::Delete { short, targets, landed } => {
                    // The bar clips from the right, so it reads warning first,
                    // then WHAT is being deleted and from WHERE. The key hints
                    // are last: they are constant, and the log pane above
                    // carries the full target list either way.
                    let first = targets.first().map(|t| &t.oid);
                    let mixed = targets.iter().any(|t| Some(&t.oid) != first);
                    format!(
                        "{}{} delete {short} from {} ?  [y] delete   [n] keep",
                        if *landed { " landed." } else { " NOT VERIFIED AS LANDED." },
                        if mixed { " REMOTES DIFFER." } else { "" },
                        targets.iter().map(|t| t.remote.as_str()).collect::<Vec<_>>().join(", ")
                    )
                }
            };
            f.push_text(&text, Style::bar(YELLOW));
            return;
        }
        if self.editing_filter {
            f.push_text(&format!(" filter: {}_", self.filter), Style::bar(MAGENTA));
            return;
        }
        if self.status.is_empty() {
            f.push_text(keys, Style::dim());
        } else {
            f.push_text(&format!(" {}", self.status), self.status_style.with_invert());
        }
    }

    fn render_list(&self, f: &mut Frame) {
        let filter_note = if self.filter.is_empty() {
            String::new()
        } else {
            format!("   filter:{}", self.filter)
        };
        let title = format!(
            " td-review   {}   base:{}   {} branch{}{}",
            self.git.repo().display(),
            self.base,
            self.view.len(),
            if self.view.len() == 1 { "" } else { "es" },
            filter_note
        );
        self.title(f, &title);
        if let Some((upstream, n)) = &self.base_stale {
            f.push_text(
                &format!(
                    " {} is {n} commit{} behind {upstream} — merge before landing, or every push will be rejected",
                    self.base,
                    if *n == 1 { "" } else { "s" }
                ),
                Style::bar(YELLOW),
            );
        }

        let name_width =
            name_column(self.view.iter().filter_map(|&i| self.branches.get(i)).map(|b| &b.refname));

        f.push_text(
            &format!(
                "  {:>4}  {:<7}  {:<width$}  {}",
                "AGE",
                "A/B",
                "BRANCH",
                "SUBJECT",
                width = name_width
            ),
            Style::dim(),
        );

        let height = f.room().saturating_sub(1);
        let top = scroll_top(self.sel, self.top, height);
        if self.view.is_empty() {
            f.push_blank();
            f.push_text("  no branches match", Style::fg(GRAY));
        }
        for (row, &idx) in self.view.iter().enumerate().skip(top).take(height) {
            let Some(b) = self.branches.get(idx) else { continue };
            let selected = row == self.sel;
            // Padded by measured columns: `{:<width$}` counts chars, so a
            // double-width name would shift the subject column right.
            let (name, cols) = clip_cols(&b.refname, name_width);
            let text = format!(
                "{} {:>4}  {:<7}  {name}{:pad$}  {}",
                if selected { ">" } else { " " },
                b.age(self.now),
                b.counts_label(),
                "",
                b.subject,
                pad = name_width.saturating_sub(cols)
            );
            let style = if selected {
                Style::PLAIN.with_invert()
            } else if b.nothing_ahead() {
                Style::dim()
            } else {
                Style::PLAIN
            };
            f.push_text(&text, style);
        }
        self.footer(
            f,
            " enter review · f fetch · r reload · / filter · D delete · ? help · q quit",
        );
    }

    fn render_review(&self, f: &mut Frame) {
        let Some(r) = &self.reviewing else {
            self.title(f, " td-review");
            self.footer(f, " q back");
            return;
        };
        let name = r.refname.as_str();
        let total = r.lines.len();
        let height = f.rows.saturating_sub(2);
        let pos = if total <= height {
            "all".to_string()
        } else {
            // Bottom of the viewport, not its top: the top caps at total-height,
            // which reads as 80% when the last line is already on screen.
            format!("{}%", (self.scroll.saturating_add(height).min(total) * 100) / total.max(1))
        };
        self.title(f, &format!(" review  {name}  vs {}   [{pos}]", self.base));
        for line in r.lines.iter().skip(self.scroll).take(height) {
            f.push(line);
        }
        self.footer(
            f,
            " j/k scroll · space/b page · g/G top/end · p pager · a approve+land · q back",
        );
    }

    fn render_log(&self, f: &mut Frame) {
        self.title(f, &format!(" {}", self.log_title));
        let height = f.rows.saturating_sub(2);
        let top = self.log_scroll.min(self.log.len().saturating_sub(height));
        for line in self.log.iter().skip(top).take(height) {
            f.push(line);
        }
        self.footer(f, " j/k scroll · q back to branches");
    }

    fn render_help(&self, f: &mut Frame) {
        self.title(f, " td-review — keys");
        let height = f.rows.saturating_sub(2);
        let lines = help_lines();
        let top = self.help_scroll.min(lines.len().saturating_sub(height));
        for line in lines.iter().skip(top).take(height) {
            f.push(line);
        }
        self.footer(f, " j/k scroll · q back");
    }

    // ----------------------------------------------------------------- input

    pub fn handle(&mut self, key: Key, term: &mut dyn Ui) -> io::Result<Flow> {
        self.status.clear();
        self.just_prompted = false;
        if self.prompt.is_some() {
            return self.handle_prompt(key, term);
        }
        if self.editing_filter {
            self.handle_filter(key);
            return Ok(Flow::Continue);
        }
        match self.screen {
            Screen::List => self.handle_list(key, term),
            Screen::Review => self.handle_review(key, term),
            Screen::Log => {
                match key {
                    Key::Char('q') | Key::Esc | Key::Ctrl('c') => {
                        self.screen = Screen::List;
                    }
                    _ => {
                        self.log_scroll =
                            scroll_by(self.log_scroll, key, self.log.len(), term.size().0);
                    }
                }
                Ok(Flow::Continue)
            }
            Screen::Help => {
                if matches!(key, Key::Char('q') | Key::Esc | Key::Char('?')) {
                    self.screen = Screen::List;
                } else {
                    self.help_scroll =
                        scroll_by(self.help_scroll, key, help_lines().len(), term.size().0);
                }
                Ok(Flow::Continue)
            }
        }
    }

    fn handle_filter(&mut self, key: Key) {
        match key {
            Key::Char(c) => self.filter.push(c),
            Key::Backspace => {
                self.filter.pop();
            }
            Key::Enter => self.editing_filter = false,
            Key::Esc | Key::Ctrl('c') => {
                self.filter.clear();
                self.editing_filter = false;
            }
            _ => return,
        }
        self.apply_filter();
    }

    fn handle_list(&mut self, key: Key, term: &mut dyn Ui) -> io::Result<Flow> {
        // Title, column header and status bar — plus the stale-base banner when
        // it is drawn, or a page would step past the last visible row.
        let chrome = if self.base_stale.is_some() { 4 } else { 3 };
        let height = term.size().0.saturating_sub(chrome).max(1);
        match key {
            Key::Char('q') | Key::Ctrl('c') => return Ok(Flow::Quit),
            Key::Char('?') => self.screen = Screen::Help,
            Key::Char('j') | Key::Down => self.sel = step(self.sel, 1, self.view.len()),
            Key::Char('k') | Key::Up => self.sel = self.sel.saturating_sub(1),
            Key::Char('g') | Key::Home => self.sel = 0,
            Key::Char('G') | Key::End => self.sel = self.view.len().saturating_sub(1),
            Key::PageDown | Key::Ctrl('f') | Key::Char(' ') => {
                self.sel = step(self.sel, height, self.view.len())
            }
            Key::PageUp | Key::Ctrl('b') => self.sel = self.sel.saturating_sub(height),
            Key::Char('/') => {
                self.editing_filter = true;
                self.filter.clear();
                self.apply_filter();
            }
            Key::Char('r') => {
                self.reload()?;
                self.note("reloaded", Style::fg(GREEN));
            }
            Key::Char('f') => {
                self.note("fetching all remotes…", Style::fg(YELLOW));
                self.redraw(term)?;
                let fetch = self.git.run(&["fetch", "--all", "--prune"])?;
                if fetch.ok {
                    self.reload()?;
                    self.note("fetched", Style::fg(GREEN));
                } else {
                    self.note(format!("fetch failed: {}", fetch.failure()), Style::fg(RED));
                }
            }
            Key::Char('D') => {
                if let Some(refname) =
                    self.view.get(self.sel).and_then(|&i| self.branches.get(i)).map(|b| b.refname.clone())
                {
                    self.log.clear();
                    self.offer_delete(&refname, Only::RefsRemote, None, term);
                }
            }
            Key::Enter | Key::Char('l') | Key::Right => self.open_review()?,
            _ => {}
        }
        self.top = scroll_top(self.sel, self.top, height);
        Ok(Flow::Continue)
    }

    fn handle_review(&mut self, key: Key, term: &mut dyn Ui) -> io::Result<Flow> {
        let rows = term.size().0;
        match key {
            Key::Char('q') | Key::Esc | Key::Char('h') | Key::Left => {
                self.screen = Screen::List;
            }
            Key::Ctrl('c') => return Ok(Flow::Quit),
            Key::Char('?') => self.screen = Screen::Help,
            Key::Char('p') => self.open_pager(term)?,
            Key::Char('a') => {
                let empty = self.reviewing.as_ref().is_some_and(|r| r.empty);
                if empty {
                    self.note(
                        "nothing to land: this branch changes nothing on top of the base",
                        Style::fg(RED),
                    );
                } else {
                    self.ask(Prompt::Land);
                }
            }
            _ => {
                let total = self.reviewing.as_ref().map_or(0, |r| r.lines.len());
                self.scroll = scroll_by(self.scroll, key, total, rows);
            }
        }
        Ok(Flow::Continue)
    }

    /// Run one read batch. A confirmation must be answered by a keystroke typed
    /// after it was seen, so the rest of the batch that raised one is dropped.
    pub fn feed(&mut self, keys: Vec<Key>, term: &mut dyn Ui) -> io::Result<Flow> {
        for key in keys {
            if matches!(self.handle(key, term)?, Flow::Quit) {
                return Ok(Flow::Quit);
            }
            if self.just_prompted() {
                break;
            }
        }
        Ok(Flow::Continue)
    }

    fn handle_prompt(&mut self, key: Key, term: &mut dyn Ui) -> io::Result<Flow> {
        match self.prompt {
            Some(Prompt::Land) => match key {
                Key::Char('y') | Key::Char('Y') => {
                    self.prompt = None;
                    self.run_land(term)?;
                }
                _ => {
                    self.prompt = None;
                    self.note("cancelled", Style::fg(GRAY));
                }
            },
            Some(Prompt::Push { .. }) => match key {
                Key::Char('y') | Key::Char('Y') => {
                    let Some(Prompt::Push { remotes }) = self.prompt.take() else {
                        return Ok(Flow::Continue);
                    };
                    self.run_push(&remotes, term)?;
                }
                _ => {
                    self.prompt = None;
                    self.log.push(Line::new(
                        "not pushed — the squash commit is local; push later with `git pushall`",
                        Style::fg(YELLOW),
                    ));
                    self.committed = None;
                    self.refresh_quietly();
                }
            },
            Some(Prompt::Delete { .. }) => {
                let Some(Prompt::Delete { short, targets, .. }) = self.prompt.take() else {
                    return Ok(Flow::Continue);
                };
                match key {
                    Key::Char('y') | Key::Char('Y') => self.run_delete(&short, &targets, term)?,
                    _ => self.note(format!("kept {short}"), Style::fg(GRAY)),
                }
            }
            Some(Prompt::Conflict) => match key {
                Key::Char('d') | Key::Char('D') => {
                    self.prompt = None;
                    let lines = land::discard(&self.git)?;
                    self.log.extend(lines);
                    self.log_to_end(term);
                }
                // Esc means "leave it" too: raw mode disables ISIG, so `d`
                // and `l` would otherwise be the ONLY ways out of this prompt
                // and the safe one must not need a guess.
                Key::Char('l') | Key::Char('L') | Key::Esc => return Ok(Flow::Quit),
                _ => {}
            },
            None => {}
        }
        Ok(Flow::Continue)
    }

    // ---------------------------------------------------------------- actions

    fn redraw(&self, term: &mut dyn Ui) -> io::Result<()> {
        let (rows, cols) = term.size();
        term.draw(self.render(rows, cols))
    }

    /// Park the log's tail against the bottom of the pane, the way a terminal
    /// does — the outcome of a step is the line that matters.
    fn log_to_end(&mut self, term: &dyn Ui) {
        let height = term.size().0.saturating_sub(2);
        self.log_scroll = self.log.len().saturating_sub(height);
    }

    fn open_review(&mut self) -> io::Result<()> {
        let Some(&idx) = self.view.get(self.sel) else {
            return Ok(());
        };
        let Some(branch) = self.branches.get(idx) else {
            return Ok(());
        };
        let refname = branch.refname.clone();
        let preview = match land::preview(&self.git, &self.base, &refname) {
            Ok(p) => p,
            Err(e) => {
                self.note(format!("{e}"), Style::fg(RED));
                return Ok(());
            }
        };
        let empty = preview.is_empty();
        let range = format!("{}..{}", preview.merge_base, preview.branch_oid);
        let lines = preview_lines(branch, &preview, &self.base, self.now);
        self.reviewing = Some(Reviewing {
            refname,
            oid: preview.branch_oid.clone(),
            base_oid: preview.base_oid.clone(),
            range,
            lines,
            empty,
        });
        self.scroll = 0;
        self.screen = Screen::Review;
        Ok(())
    }

    fn open_pager(&mut self, term: &mut dyn Ui) -> io::Result<()> {
        let Some(range) = self.reviewing.as_ref().map(|r| r.range.clone()) else {
            return Ok(());
        };
        let git = &self.git;
        let result = term.suspend_run(&mut || git.run_interactive(&["diff", &range]))?;
        if let Err(e) = result {
            self.note(format!("pager: {e}"), Style::fg(RED));
        }
        Ok(())
    }

    fn run_land(&mut self, term: &mut dyn Ui) -> io::Result<()> {
        let Some((refname, oid, base_oid)) = self
            .reviewing
            .as_ref()
            .map(|r| (r.refname.clone(), r.oid.clone(), r.base_oid.clone()))
        else {
            return Ok(());
        };
        self.log_title = format!("landing {refname} into {}", self.base);
        self.log = vec![Line::new(
            format!("squashing {refname} into {}…", self.base),
            Style::fg(YELLOW),
        )];
        self.log_scroll = 0;
        self.screen = Screen::Log;
        self.redraw(term)?;

        let landing =
            land::squash_land(&self.git, &self.base, &refname, Some(&oid), Some(&base_oid))?;
        self.log = landing.log;
        self.log_to_end(term);
        match landing.outcome {
            Outcome::Committed { sha } => {
                self.committed = Some(sha);
                let remotes = self.git.remotes().unwrap_or_default();
                // The bar is one clipped row, so the full plan goes in the pane.
                self.log.push(Line::new(
                    format!("will push {} to: {}", self.base, pushable(&remotes).join(", ")),
                    Style::fg(CYAN),
                ));
                // A push publishes every local commit on the base, not only the
                // one just reviewed.
                let unpushed = self.git.unpushed(&self.base).unwrap_or_default();
                if unpushed.len() > 1 {
                    self.log.push(Line::new(
                        format!("publishing {} commits on {}:", unpushed.len(), self.base),
                        Style::fg(YELLOW),
                    ));
                    for c in &unpushed {
                        self.log.push(Line::new(format!("  {c}"), Style::fg(YELLOW)));
                    }
                }
                self.log_to_end(term);
                self.ask(Prompt::Push { remotes });
            }
            // Both leave work in the index or tree, so both need the bail-out.
            Outcome::Conflict | Outcome::Failed(_) => {
                if self.git.dirty_entries().map(|d| !d.is_empty()).unwrap_or(true) {
                    self.ask(Prompt::Conflict);
                }
            }
            Outcome::Nothing | Outcome::Blocked(_) => {}
        }
        Ok(())
    }

    fn run_push(&mut self, remotes: &[(String, String)], term: &mut dyn Ui) -> io::Result<()> {
        let Some(sha) = self.committed.clone() else {
            return Ok(());
        };
        let landed = self
            .reviewing
            .as_ref()
            .map(|r| (r.refname.clone(), r.oid.clone()))
            .unwrap_or_default();
        self.log.push(Line::new(
            format!("pushing {} to every remote…", self.base),
            Style::fg(YELLOW),
        ));
        self.log_to_end(term);
        self.redraw(term)?;

        let pushed = land::push_all(&self.git, &self.base, &sha, remotes)?;
        self.log.extend(pushed.log);
        self.log.push(match (pushed.all_ok, pushed.count) {
            (true, 0) => Line::new(
                "landed locally — no remote was eligible, nothing was published",
                Style::fg(YELLOW).with_bold(),
            ),
            (true, n) => Line::new(
                format!("landed and published to {n} remote{}", if n == 1 { "" } else { "s" }),
                Style::fg(GREEN).with_bold(),
            ),
            (false, _) => Line::new(
                "some remotes rejected the push — the commit is still on the local base",
                Style::fg(RED).with_bold(),
            ),
        });
        self.committed = None;
        self.log_to_end(term);
        // A listing hiccup must not tear down the TUI after a successful land.
        self.refresh_quietly();
        // Only offer cleanup once the branch is genuinely published somewhere.
        if pushed.all_ok && pushed.count > 0 {
            self.offer_delete(&landed.0, Only::EveryRemote, Some(&landed.1), term);
        }
        Ok(())
    }

    /// Raise the delete confirmation for `refname`, naming the remotes that
    /// actually carry it. `only` limits it to one remote. Silent when none do.
    fn offer_delete(
        &mut self,
        refname: &str,
        only: Only,
        landed_oid: Option<&str>,
        term: &mut dyn Ui,
    ) {
        let remotes = self.git.remote_names().unwrap_or_default();
        let (named, short) = git::split_remote(refname, &remotes);
        let short = short.to_string();
        if short.is_empty() {
            return;
        }
        if short == self.base {
            self.note("refusing to delete the base branch", Style::fg(RED));
            return;
        }
        let only = match only {
            Only::RefsRemote => named,
            Only::EveryRemote => None,
        };
        let (targets, diverged) = match land::delete_plan(&self.git, &short, only, landed_oid) {
            Ok(t) => t,
            Err(e) => return self.note(format!("{e}"), Style::fg(RED)),
        };
        if targets.is_empty() {
            return self.note(format!("no pushable remote carries {short}"), Style::fg(GRAY));
        }
        // The prompt bar is one clipped row, so the pane must carry the record
        // of what `y` will delete — and be the pane on screen.
        self.screen = Screen::Log;
        self.log_title = format!("delete {short}?");
        for d in &diverged {
            self.log.push(Line::new(
                format!("{}/{short} is not the landed commit — left alone", d.remote),
                Style::fg(YELLOW),
            ));
        }
        self.log.push(Line::new(format!("will delete {short} from:"), Style::fg(CYAN)));
        for t in &targets {
            self.log.push(Line::new(
                format!("  {} at {}", t.remote, land::short(&t.oid)),
                Style::fg(GRAY),
            ));
        }
        self.log_to_end(term);
        self.ask(Prompt::Delete { short, targets, landed: landed_oid.is_some() })
    }

    fn run_delete(
        &mut self,
        short: &str,
        targets: &[land::DeleteTarget],
        term: &mut dyn Ui,
    ) -> io::Result<()> {
        self.log_title = format!("deleting {short} from the remotes");
        self.screen = Screen::Log;
        self.log.push(Line::new(format!("deleting {short}…"), Style::fg(YELLOW)));
        self.log_to_end(term);
        self.redraw(term)?;

        let deleted = land::delete_branch(&self.git, &self.base, short, targets)?;
        self.log.extend(deleted.log);
        self.log.push(match (deleted.all_ok, deleted.count) {
            (true, 0) => Line::new("nothing to delete", Style::fg(YELLOW)),
            (true, n) => Line::new(
                format!("deleted {short} from {n} remote{}", if n == 1 { "" } else { "s" }),
                Style::fg(GREEN).with_bold(),
            ),
            (false, _) => {
                Line::new("some deletes failed", Style::fg(RED).with_bold())
            }
        });
        self.log_to_end(term);
        self.refresh_quietly();
        Ok(())
    }

    /// Re-read the branch list, downgrading a failure to a status note.
    fn refresh_quietly(&mut self) {
        if let Err(e) = self.reload() {
            self.log.push(Line::new(
                format!("(branch list not refreshed: {e})"),
                Style::fg(YELLOW),
            ));
        }
    }
}

fn help_lines() -> Vec<Line> {
    let mut out = Vec::new();
    let mut section = |title: &str, rows: &[(&str, &str)]| {
        if !out.is_empty() {
            out.push(Line::blank());
        }
        out.push(Line::new(title.to_string(), Style::fg(CYAN).with_bold()));
        for (keys, what) in rows {
            out.push(Line::plain(format!("  {keys:<18}{what}")));
        }
    };
    section(
        "branch list",
        &[
            ("j / k", "move"),
            ("space / b", "page down / up"),
            ("g / G", "first / last"),
            ("enter", "review the selected branch against the base"),
            ("f", "git fetch --all --prune"),
            ("r", "re-read branches"),
            ("/", "filter by branch name (esc clears)"),
            ("D", "delete the selected branch from every pushable remote"),
            ("?", "this help"),
            ("q", "quit"),
        ],
    );
    section(
        "review",
        &[
            ("j / k, space / b", "scroll"),
            ("g / G", "top / end"),
            ("p", "open the same diff in your own pager"),
            ("a", "approve: squash into the base and commit"),
            ("q", "back to the list"),
        ],
    );
    section(
        "landing",
        &[
            ("a then y", "squash + commit, message from the branch's commits"),
            ("then y", "push the base to every remote (no_push skipped)"),
            ("then n", "keep the commit local"),
            ("then y", "delete the landed branch from the remotes (offered last)"),
        ],
    );
    for prose in [
        "The branch is pinned to the commit you reviewed: if the ref moves",
        "before you approve, the landing refuses rather than committing work",
        "you have not seen. The push publishes exactly that commit.",
        "",
        "Landing needs a clean work tree with the base branch checked out.",
    ] {
        out.push(Line::new(format!("  {prose}"), Style::dim()));
    }
    out
}

/// Width of the branch-name column: the longest name, measured in the same
/// display columns `clip_cols` and the row padding use — a char count would size
/// the column too narrow for a double-width name and clip it needlessly.
fn name_column<'a>(names: impl Iterator<Item = &'a String>) -> usize {
    names
        .map(|n| term::sanitize(n, MAX_NAME_COL).1)
        .max()
        .unwrap_or(20)
        .clamp(12, MAX_NAME_COL)
}

/// Clip to `max` display COLUMNS with an ellipsis, returning the text and the
/// columns it occupies. Char counts would let a CJK or emoji branch name spill
/// into the subject column.
fn clip_cols(text: &str, max: usize) -> (String, usize) {
    // One column of headroom tells truncation apart from an exact fit.
    let (probe, w) = term::sanitize(text, max.saturating_add(1));
    if w <= max {
        return (probe, w);
    }
    let (mut out, mut w) = term::sanitize(text, max.saturating_sub(1));
    if max > 0 {
        out.push('\u{2026}');
        w += 1;
    }
    (out, w)
}

/// Keep `sel` inside a window `height` tall that starts at `top`.
fn scroll_top(sel: usize, top: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    if sel < top {
        sel
    } else if sel >= top + height {
        sel + 1 - height
    } else {
        top
    }
}

fn step(current: usize, by: usize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current + by).min(len - 1)
}

/// Scroll a `total`-line buffer shown `rows - 2` lines at a time.
fn scroll_by(scroll: usize, key: Key, total: usize, rows: usize) -> usize {
    let height = rows.saturating_sub(2).max(1);
    let max = total.saturating_sub(height);
    let next = match key {
        Key::Char('j') | Key::Down | Key::Enter => scroll + 1,
        Key::Char('k') | Key::Up => scroll.saturating_sub(1),
        Key::Char(' ') | Key::PageDown | Key::Ctrl('f') => scroll + height,
        Key::Char('b') | Key::PageUp | Key::Ctrl('b') => scroll.saturating_sub(height),
        Key::Ctrl('d') => scroll + height / 2,
        Key::Ctrl('u') => scroll.saturating_sub(height / 2),
        Key::Char('g') | Key::Home => 0,
        Key::Char('G') | Key::End => max,
        _ => scroll,
    };
    next.min(max)
}

fn heading(text: &str) -> Line {
    Line::new(text, Style::fg(CYAN).with_bold())
}

/// Colour a diff line by its leading marker. `+++`/`---` are file headers and
/// must be matched before the single-character add/remove markers.
pub fn diff_style(line: &str) -> Style {
    const FILE_HEADERS: [&str; 7] = [
        "diff --git",
        "index ",
        "new file",
        "deleted file",
        "old mode",
        "new mode",
        "rename ",
    ];
    if FILE_HEADERS.iter().any(|p| line.starts_with(p))
        || line.starts_with("+++")
        || line.starts_with("---")
    {
        Style::bold()
    } else if line.starts_with("@@") {
        Style::fg(CYAN)
    } else if line.starts_with('+') {
        Style::fg(GREEN)
    } else if line.starts_with('-') {
        Style::fg(RED)
    } else {
        Style::PLAIN
    }
}

/// Assemble the scrollable review buffer: what will be squashed, the message it
/// will be committed with, then the diff itself.
fn preview_lines(branch: &Branch, p: &Preview, base: &str, now: i64) -> Vec<Line> {
    let mut out = Vec::new();
    out.push(heading("branch"));
    out.push(Line::plain(format!("  {}", branch.refname)));
    out.push(Line::plain(match branch.counts {
        Some((a, b)) => format!(
            "  {} · {} ago · {a} ahead / {b} behind {base}",
            branch.author,
            branch.age(now)
        ),
        None => format!("  {} · {} ago", branch.author, branch.age(now)),
    }));
    out.push(Line::new(
        format!("  tip {}  merge-base {}", short(&branch.commit), short(&p.merge_base)),
        Style::fg(GRAY),
    ));
    out.push(Line::blank());

    out.push(heading(&format!("commits to squash ({})", p.commits.len())));
    if p.commits.is_empty() {
        out.push(Line::new("  (none)", Style::fg(RED)));
    }
    for c in &p.commits {
        out.push(Line::plain(format!("  {c}")));
    }
    out.push(Line::blank());

    out.push(heading("squash commit message"));
    if p.message.trim().is_empty() {
        out.push(Line::new("  (empty)", Style::fg(RED)));
    }
    for l in p.message.lines() {
        out.push(Line::new(format!("  {l}"), Style::fg(GRAY)));
    }
    out.push(Line::blank());

    if let Some(note) = &p.note {
        out.push(Line::new(format!("  ! {note}"), Style::fg(RED).with_bold()));
        out.push(Line::blank());
    }

    out.push(heading("diffstat"));
    if p.stat.trim().is_empty() {
        out.push(Line::new("  (no changes vs the base)", Style::fg(RED)));
    }
    for l in p.stat.lines() {
        out.push(Line::plain(format!("  {l}")));
    }
    out.push(Line::blank());

    out.push(heading("diff"));
    for l in p.diff.lines() {
        out.push(Line::new(l.to_string(), diff_style(l)));
    }
    out
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A `Ui` that records frames instead of writing them. The state machine is
    /// only reachable from a test through this: `Terminal` needs a real tty.
    struct FakeUi {
        frames: Vec<String>,
        drains: usize,
        drain_fails: bool,
    }

    impl FakeUi {
        fn new() -> FakeUi {
            FakeUi { frames: Vec::new(), drains: 0, drain_fails: false }
        }

        fn last(&self) -> &str {
            self.frames.last().map_or("", String::as_str)
        }
    }

    impl Ui for FakeUi {
        fn size(&self) -> (usize, usize) {
            (24, 80)
        }

        fn draw(&mut self, frame: String) -> io::Result<()> {
            self.frames.push(frame);
            Ok(())
        }

        fn drain_input(&mut self) -> io::Result<()> {
            self.drains += 1;
            if self.drain_fails {
                return Err(io::Error::other("stty restore failed"));
            }
            Ok(())
        }

        fn suspend_run(
            &mut self,
            body: &mut dyn FnMut() -> io::Result<()>,
        ) -> io::Result<io::Result<()>> {
            Ok(body())
        }
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A work tree on `main` with one branch published to `origin`, present
    /// locally only as a remote-tracking ref.
    fn repo(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("td-review-app-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let origin = root.join("origin.git");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git_in(&root, &["init", "--bare", "-b", "main", &origin.to_string_lossy()]);
        git_in(&root, &["init", "-b", "main", &work.to_string_lossy()]);
        std::fs::write(work.join("f"), "one\n").unwrap();
        git_in(&work, &["add", "f"]);
        git_in(&work, &["commit", "-m", "base"]);
        git_in(&work, &["remote", "add", "origin", &origin.to_string_lossy()]);
        git_in(&work, &["push", "origin", "main"]);
        git_in(&work, &["checkout", "-b", "work-0001-feature"]);
        std::fs::write(work.join("f"), "two\n").unwrap();
        git_in(&work, &["commit", "-am", "feature: step"]);
        git_in(&work, &["push", "origin", "work-0001-feature"]);
        git_in(&work, &["checkout", "main"]);
        git_in(&work, &["branch", "-D", "work-0001-feature"]);
        (root, work)
    }

    fn app_on(work: &Path) -> App {
        let git = Git::discover(work).unwrap();
        let mut app = App::new(git, "main".to_string());
        app.reload().unwrap();
        app
    }

    /// `discard` runs right after a destructive reset, so a check it could not
    /// run must be reported as unverified rather than as a clean tree.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_discard_that_cannot_verify_the_tree_does_not_claim_it_is_clean() {
        let (root, work) = repo("discard-blind");
        // Breaks `git status` and nothing else — `reset --hard` still works.
        git_in(&work, &["config", "status.showUntrackedFiles", "bogus"]);
        let git = Git::discover(&work).unwrap();
        let log = land::discard(&git).unwrap();
        let text = log.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("could not be checked"), "{text}");
        assert!(!text.contains("back at HEAD"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The delete confirmation must be raised on the pane that carries the
    /// target list: the one-row prompt bar clips, so confirming from the list
    /// screen would mean confirming an irreversible remote delete having seen
    /// neither the remotes nor the full branch name.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn pressing_d_shows_what_will_be_deleted_on_the_visible_pane() {
        let (root, work) = repo("d-shows");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.handle(Key::Char('D'), &mut ui).unwrap();
        app.redraw(&mut ui).unwrap();

        let frame = ui.last();
        assert!(frame.contains("will delete"), "the pane must say what is at stake:\n{frame}");
        assert!(frame.contains("work-0001-feature"), "the branch must be named:\n{frame}");
        assert!(frame.contains("origin"), "the remote must be named:\n{frame}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The confirmation lists the remotes it will reach, so the push must go to
    /// those and not to whatever the config says a keystroke later.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_remote_added_after_the_confirmation_is_not_pushed_to() {
        let (root, work) = repo("late-remote");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();
        for key in [Key::Enter, Key::Char('a'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }
        let late = root.join("late.git");
        git_in(&root, &["init", "--bare", "-b", "main", &late.to_string_lossy()]);
        git_in(&work, &["remote", "add", "late", &late.to_string_lossy()]);

        app.handle(Key::Char('y'), &mut ui).unwrap();

        let refs = git_in(&late, &["for-each-ref"]);
        assert!(refs.trim().is_empty(), "pushed to a remote the prompt never showed: {refs}");
        assert!(
            !git_in(&work, &["rev-parse", "origin/main"]).trim().is_empty(),
            "origin should still have been pushed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The push publishes exactly the commit that was approved. If the base
    /// moved while the confirmation was open, nothing goes out.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_base_that_moved_since_the_approval_is_not_pushed() {
        let (root, work) = repo("stale-approval");
        let git = Git::discover(&work).unwrap();
        let remotes = git.remotes().unwrap();
        let approved = git_in(&work, &["rev-parse", "HEAD"]).trim().to_string();
        // The base is now something else than what the caller approved.
        std::fs::write(work.join("f"), "moved\n").unwrap();
        git_in(&work, &["commit", "-am", "someone else's commit"]);

        let pushed = land::push_all(&git, "main", &approved, &remotes).unwrap();
        let text = pushed.log.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(!pushed.all_ok, "{text}");
        assert_eq!(pushed.count, 0, "{text}");
        assert!(text.contains("refusing to push"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The push confirmation is the one prompt with no target list of its own,
    /// so the remotes it will reach have to be on the pane behind it.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn the_push_confirmation_names_the_remotes_it_would_reach() {
        let (root, work) = repo("push-names");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [Key::Enter, Key::Char('a'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }
        app.redraw(&mut ui).unwrap();

        let frame = ui.last();
        assert!(frame.contains("[y] push"), "the push prompt was not raised:\n{frame}");
        assert!(frame.contains("will push main to: origin"), "remotes not named:\n{frame}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The whole point of the flag: a batch that raises a prompt must not also
    /// answer it. `Dy` arriving in one read may not delete anything.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_batch_that_raises_a_prompt_does_not_also_answer_it() {
        let (root, work) = repo("batch-dy");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.feed(vec![Key::Char('D'), Key::Char('y')], &mut ui).unwrap();

        assert!(app.prompt.is_some(), "the prompt was consumed by its own batch");
        let refs = git_in(&work, &["ls-remote", "--heads", "origin"]);
        assert!(refs.contains("work-0001-feature"), "the branch was deleted: {refs}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Typeahead must not answer a confirmation the user has not seen. The loop
    /// stops feeding a read batch once a prompt is raised; this pins the flag
    /// that makes it stop.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_batched_keystroke_cannot_answer_the_prompt_it_raised() {
        let (root, work) = repo("batched");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.handle(Key::Char('D'), &mut ui).unwrap();
        assert!(app.just_prompted(), "D must raise a prompt the loop then stops on");
        assert!(app.prompt.is_some());

        // And the prompt is only answerable after the typeahead was dropped.
        app.settle_prompt(&mut ui).unwrap();
        assert_eq!(ui.drains, 1, "the tty must be drained before the answer is read");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A failed VMIN restore leaves every later read returning empty, which the
    /// loop cannot tell from a closed tty. It must surface as an error, not as
    /// a silent successful exit with a confirmation pending.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_failed_drain_is_an_error_not_a_quiet_exit() {
        let (root, work) = repo("drain-fail");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();
        ui.drain_fails = true;

        app.handle(Key::Char('D'), &mut ui).unwrap();
        assert!(app.settle_prompt(&mut ui).is_err(), "a failed drain must propagate");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The base branch is never a delete candidate, whatever row is selected.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn the_base_branch_is_refused_before_any_prompt() {
        let (root, work) = repo("base-refused");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.offer_delete("origin/main", Only::RefsRemote, None, &mut ui);
        assert!(app.prompt.is_none(), "no confirmation may be offered for the base");
        assert!(app.status.contains("base branch"), "status was {:?}", app.status);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_long_branch_name_is_clipped_to_its_column() {
        assert_eq!(clip_cols("short", 10), ("short".to_string(), 5));
        assert_eq!(clip_cols("origin/work-0001-a-very-long-name", 12).0, "origin/work\u{2026}");
        assert_eq!(clip_cols("exactlyten", 10), ("exactlyten".to_string(), 10));
        assert_eq!(clip_cols("origin/wörk-über-lang", 10).1, 10);
        assert_eq!(clip_cols("abc", 0), (String::new(), 0));
    }

    /// The name column is terminal cells, not chars: five double-width glyphs
    /// are ten columns wide and must be clipped like any other ten-column name.
    #[test]
    fn a_wide_branch_name_is_measured_in_columns() {
        let (text, cols) = clip_cols("日本語日本語日本語", 10);
        assert_eq!(cols, 10);
        assert!(text.chars().count() < 9, "clipped by columns, got {text:?}");
    }

    /// The column must be sized in the same unit it is clipped and padded in,
    /// or a double-width name is clipped to make room it did not need.
    #[test]
    fn the_name_column_is_sized_in_columns_too() {
        let wide = "origin/日本語日本語日本語".to_string();
        let width = name_column([&wide].into_iter());
        assert_eq!(width, term::sanitize(&wide, MAX_NAME_COL).1);
        assert_eq!(clip_cols(&wide, width).0, wide, "sized right, it should not clip");

        assert_eq!(name_column([].into_iter()), 20, "no branches: a sane default");
        assert_eq!(name_column([&"ab".to_string()].into_iter()), 12, "clamped up");
        assert_eq!(name_column([&"x".repeat(80)].into_iter()), MAX_NAME_COL, "clamped down");
    }

    #[test]
    fn scroll_top_follows_selection() {
        assert_eq!(scroll_top(0, 0, 10), 0);
        assert_eq!(scroll_top(15, 0, 10), 6);
        assert_eq!(scroll_top(3, 6, 10), 3);
        assert_eq!(scroll_top(7, 0, 10), 0);
        assert_eq!(scroll_top(5, 0, 0), 0);
    }

    #[test]
    fn scroll_by_clamps_to_content() {
        // 100 lines in a 22-row window => 20 visible, max scroll 80.
        assert_eq!(scroll_by(0, Key::Char('k'), 100, 22), 0);
        assert_eq!(scroll_by(0, Key::Char('j'), 100, 22), 1);
        assert_eq!(scroll_by(0, Key::Char(' '), 100, 22), 20);
        assert_eq!(scroll_by(0, Key::End, 100, 22), 80);
        assert_eq!(scroll_by(75, Key::Char(' '), 100, 22), 80);
        // Content shorter than the window never scrolls.
        assert_eq!(scroll_by(0, Key::End, 5, 22), 0);
    }

    #[test]
    fn step_clamps_to_last_index() {
        assert_eq!(step(0, 1, 3), 1);
        assert_eq!(step(2, 1, 3), 2);
        assert_eq!(step(0, 99, 3), 2);
        assert_eq!(step(0, 1, 0), 0);
    }

    #[test]
    fn diff_headers_beat_add_remove_markers() {
        assert_eq!(diff_style("+++ b/x"), Style::bold());
        assert_eq!(diff_style("--- a/x"), Style::bold());
        assert_eq!(diff_style("+added"), Style::fg(GREEN));
        assert_eq!(diff_style("-removed"), Style::fg(RED));
        assert_eq!(diff_style("@@ -1,2 +1,3 @@"), Style::fg(CYAN));
        assert_eq!(diff_style(" context"), Style::PLAIN);
    }
}
