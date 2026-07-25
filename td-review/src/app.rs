//! TUI state machine: branch list -> branch review -> land log.

use std::collections::VecDeque;
use std::io;

use crate::git::{self, now_unix, Branch, DefaultRemote, Git};
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
    /// Publish the base. The target list and the commit are pinned when the
    /// prompt is raised: a remote added while it is open cannot be pushed to
    /// unseen, and a base that moves meanwhile is refused by `push_all`. The
    /// push still goes by remote NAME, so a retargeted url is followed.
    Push { remotes: Vec<(String, String)>, sha: String },
    Conflict,
    /// Delete a landed (or hand-picked) branch. The targets are pinned when the
    /// prompt is raised, so confirming cannot delete more than was shown. The
    /// landing rides along: the answer decides whether it is done with (a
    /// remote it reached still carries the branch) or can be dropped.
    Delete { short: String, targets: Vec<land::DeleteTarget>, landing: Option<Landed> },
}

/// Names of the remotes a push would actually reach.
fn pushable(remotes: &[(String, String)]) -> Vec<&str> {
    remotes
        .iter()
        .filter(|(_, url)| url.as_str() != git::NO_PUSH)
        .map(|(name, _)| name.as_str())
        .collect()
}

/// A branch squash-landed this session. The tip is what its remote copies are
/// checked against before a delete; the landing commit is what a push has to
/// have published before that delete may be offered at all.
struct Landed {
    refname: String,
    oid: String,
    sha: String,
}

/// What one push put out, and where it reached.
struct Published {
    sha: String,
    remotes: Vec<String>,
    /// Landings already put to the user in this sweep. A delete that failed
    /// (a stale lease, a remote refusing its default branch) leaves the branch
    /// in place, and without this the same confirmation would come straight
    /// back.
    offered: Vec<String>,
}

/// How many of a target's missing commits the push pane names before it says
/// how many more there are.
const SHOWN_COMMITS: usize = 10;

/// Widest the branch-name column may grow, however long the longest name is.
const MAX_NAME_COL: usize = 48;

/// How wide a delete casts. `D` on a row deletes from the remote that row names;
/// the post-push sweep spans the remotes that push just reached — a mirror it
/// did not reach must not lose the branch before it has the commit replacing it.
enum Only {
    RefsRemote,
    Reached(Vec<String>),
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
    /// Branches squash-landed this session. Landing no longer publishes, so
    /// their remote copies are only offered for cleanup once a push has put the
    /// commit carrying that work on a remote.
    landed: VecDeque<Landed>,
    /// What the last push published, and the remotes it reached. Cleanup is
    /// offered only for landings that commit carries, and deletes only from
    /// those remotes.
    pushed: Option<Published>,
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
            landed: VecDeque::new(),
            pushed: None,
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
                Prompt::Push { remotes, sha } => format!(
                    " push {} at {} to {} ?  [y] push   [n] cancel",
                    self.base,
                    land::short(sha),
                    pushable(remotes).join(", ")
                ),
                Prompt::Conflict => {
                    " unfinished squash:  [l]/Esc leave it and quit   [d] discard (reset --hard HEAD)"
                        .to_string()
                }
                Prompt::Delete { short, targets, landing } => {
                    // The bar clips from the right, so it reads warning first,
                    // then WHAT is being deleted and from WHERE. The key hints
                    // are last: they are constant, and the log pane above
                    // carries the full target list either way.
                    let first = targets.first().map(|t| &t.oid);
                    let mixed = targets.iter().any(|t| Some(&t.oid) != first);
                    let where_ = if landing.is_some() {
                        // The pane above lists them in full and is on screen.
                        format!("{} remote{}", targets.len(), if targets.len() == 1 { "" } else { "s" })
                    } else {
                        targets.iter().map(|t| t.remote.as_str()).collect::<Vec<_>>().join(", ")
                    };
                    format!(
                        "{}{} delete {short} from {where_} ?  [y] delete  [n] keep{}",
                        if landing.is_some() { " landed." } else { " NOT VERIFIED AS LANDED." },
                        if mixed { " REMOTES DIFFER." } else { "" },
                        // Only when there is a rest to stop: the bar clips from
                        // the right, so an inapplicable hint costs a real one.
                        if landing.is_some() && !self.landed.is_empty() { "  [esc] stop" } else { "" }
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
            " enter review · f/F fetch · p/P push · / filter · D delete · ? help · q quit",
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
            Key::Char('f') => self.fetch(false, term)?,
            Key::Char('F') => self.fetch(true, term)?,
            Key::Char('p') => self.offer_push(false, term)?,
            Key::Char('P') => self.offer_push(true, term)?,
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

    /// `f` fetches only the base's remote; `F` sweeps every remote. A mirror
    /// that is unreachable would otherwise fail the ordinary refresh.
    fn fetch(&mut self, all: bool, term: &mut dyn Ui) -> io::Result<()> {
        let mut remote = None;
        if !all {
            match self.git.default_remote(&self.base) {
                Ok(DefaultRemote::Remote(name)) => remote = Some(name),
                Ok(DefaultRemote::NoRemotes) => {
                    self.note("no remotes configured", Style::fg(YELLOW));
                    return Ok(());
                }
                Ok(DefaultRemote::Ambiguous) => {
                    self.note("no default remote — F fetches all of them", Style::fg(YELLOW));
                    return Ok(());
                }
                // Which remote to fetch is a query, not the fetch: report it
                // like a failed fetch rather than ending the session.
                Err(e) => {
                    self.note(format!("fetch failed: {e}"), Style::fg(RED));
                    return Ok(());
                }
            }
        }
        let (args, what): (Vec<&str>, &str) = match &remote {
            Some(name) => (vec!["fetch", "--prune", "--end-of-options", name], name),
            None => (vec!["fetch", "--all", "--prune"], "all remotes"),
        };
        self.note(format!("fetching {what}…"), Style::fg(YELLOW));
        self.redraw(term)?;
        let fetch = self.git.run(&args)?;
        if fetch.ok {
            self.reload()?;
            self.note(format!("fetched {what}"), Style::fg(GREEN));
        } else {
            self.note(format!("fetch failed: {}", fetch.failure()), Style::fg(RED));
        }
        Ok(())
    }

    /// The remotes `p`/`P` would publish to, or `None` having said why there is
    /// nothing to publish to.
    fn push_targets(&mut self, all: bool) -> io::Result<Option<Vec<(String, String)>>> {
        let remotes = self.git.remotes()?;
        if all {
            return Ok(Some(remotes));
        }
        match self.git.default_remote(&self.base)? {
            DefaultRemote::Remote(name) => {
                Ok(Some(remotes.into_iter().filter(|(n, _)| n == &name).collect()))
            }
            DefaultRemote::NoRemotes => {
                self.note("no remotes configured", Style::fg(YELLOW));
                Ok(None)
            }
            DefaultRemote::Ambiguous => {
                self.note("no default remote — P pushes to all of them", Style::fg(YELLOW));
                Ok(None)
            }
        }
    }

    /// `p` publishes the base to its own remote, `P` to every remote. A push
    /// carries every local commit the target lacks, not only the last landing,
    /// so the pane names them per target before the confirmation.
    fn offer_push(&mut self, all: bool, term: &mut dyn Ui) -> io::Result<()> {
        let remotes = match self.push_targets(all) {
            Ok(Some(remotes)) => remotes,
            // push_targets said why on the status bar.
            Ok(None) => return Ok(()),
            // Naming the targets is a query, not the push: report it rather
            // than ending the session.
            Err(e) => {
                self.note(format!("push failed: {e}"), Style::fg(RED));
                return Ok(());
            }
        };
        if pushable(&remotes).is_empty() {
            let why = if remotes.is_empty() {
                "no remotes configured".to_string()
            } else {
                format!("every target is marked {} — nothing to push", git::NO_PUSH)
            };
            self.note(why, Style::fg(YELLOW));
            return Ok(());
        }
        let sha = match self.git.rev_parse(&format!("refs/heads/{}", self.base)) {
            Ok(sha) => sha,
            Err(e) => {
                self.note(format!("push failed: {e}"), Style::fg(RED));
                return Ok(());
            }
        };
        // The bar is one clipped row, so the full plan goes in the pane —
        // appended, since the landing record above it is worth keeping.
        self.screen = Screen::Log;
        self.log_title = format!("push {} ?", self.base);
        if !self.log.is_empty() {
            self.log.push(Line::blank());
        }
        self.log.push(Line::new(
            format!(
                "will push {} at {} to: {}",
                self.base,
                land::short(&sha),
                pushable(&remotes).join(", ")
            ),
            Style::fg(CYAN),
        ));
        if let Some((upstream, n)) = &self.base_stale {
            self.log.push(Line::new(
                format!(
                    "{} is {n} commit{} behind {upstream} — {upstream} will reject this push",
                    self.base,
                    if *n == 1 { "" } else { "s" }
                ),
                Style::fg(RED).with_bold(),
            ));
        }
        // Per target, since they need not be level. What each remote holds is
        // read from its tracking ref, so it is only true as of the last fetch.
        self.log.push(Line::new("what each target lacks, as of the last fetch:", Style::fg(GRAY)));
        for name in pushable(&remotes) {
            let lines = match self.git.unpushed_to(name, &self.base) {
                Ok(Some(commits)) if commits.is_empty() => {
                    vec![Line::new(format!("  {name} is already at this commit"), Style::fg(GRAY))]
                }
                Ok(Some(commits)) => {
                    let mut lines = vec![Line::new(
                        format!(
                            "  {name} lacks {} commit{}:",
                            commits.len(),
                            if commits.len() == 1 { "" } else { "s" }
                        ),
                        Style::fg(YELLOW),
                    )];
                    // Capped: a mirror thousands of commits behind would push
                    // the header off the pane, and the count above is the fact
                    // that matters.
                    lines.extend(
                        commits
                            .iter()
                            .take(SHOWN_COMMITS)
                            .map(|c| Line::new(format!("    {c}"), Style::fg(YELLOW))),
                    );
                    if let Some(rest) = commits.len().checked_sub(SHOWN_COMMITS).filter(|n| *n > 0) {
                        lines.push(Line::new(format!("    …and {rest} more"), Style::fg(GRAY)));
                    }
                    lines
                }
                // An empty list must never stand in for an unanswered question.
                Ok(None) => vec![Line::new(
                    format!("  no {name}/{} here — cannot say what it lacks", self.base),
                    Style::fg(YELLOW),
                )],
                Err(e) => vec![Line::new(
                    format!("  {name}: could not say what it lacks ({e})"),
                    Style::fg(YELLOW),
                )],
            };
            self.log.extend(lines);
        }
        self.log_scroll = 0;
        self.log_to_end(term);
        self.ask(Prompt::Push { remotes, sha });
        Ok(())
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
            // Scrolling the plan is not answering it: these panes list every
            // commit a push would publish and every remote a delete would
            // reach, and both can be longer than the screen.
            Some(Prompt::Push { .. }) | Some(Prompt::Delete { .. }) if is_scroll(key) => {
                self.log_scroll = scroll_by(self.log_scroll, key, self.log.len(), term.size().0);
            }
            Some(Prompt::Push { .. }) => match key {
                Key::Char('y') | Key::Char('Y') => {
                    let Some(Prompt::Push { remotes, sha }) = self.prompt.take() else {
                        return Ok(Flow::Continue);
                    };
                    self.run_push(&remotes, &sha, term)?;
                }
                _ => {
                    self.prompt = None;
                    self.screen = Screen::List;
                    self.note("not pushed", Style::fg(GRAY));
                }
            },
            Some(Prompt::Delete { .. }) => {
                let Some(Prompt::Delete { short, targets, landing }) = self.prompt.take() else {
                    return Ok(Flow::Continue);
                };
                let sweeping = landing.is_some();
                match key {
                    Key::Char('y') | Key::Char('Y') => {
                        self.run_delete(&short, &targets, term)?;
                        // Deleted where this push reached; a remote it did not
                        // reach may still carry the branch, so the landing goes
                        // back in the queue for the push that does reach it.
                        if let Some(entry) = landing {
                            if self.still_carried(&entry) {
                                self.landed.push_back(entry);
                            }
                        }
                    }
                    // One key has to end a sweep: a session that landed ten
                    // branches would otherwise ask ten times. The rest stay
                    // queued — this ends the sweep, not the session's cleanup.
                    Key::Esc if sweeping => {
                        self.pushed = None;
                        self.log.push(Line::new(format!("kept {short}"), Style::fg(GRAY)));
                        self.log_to_end(term);
                        self.note("sweep ended", Style::fg(GRAY));
                    }
                    // The note is hidden by the next prompt in a sweep, so the
                    // pane has to carry the record of what was kept.
                    Key::Char('n') | Key::Char('N') => {
                        self.log.push(Line::new(format!("kept {short}"), Style::fg(GRAY)));
                        self.log_to_end(term);
                        self.note(format!("kept {short}"), Style::fg(GRAY));
                    }
                    // Anything else is not an answer: keep the branch, and keep
                    // the landing too — a stray key must not be what makes a
                    // pending cleanup disappear for the rest of the session.
                    _ => {
                        if let Some(entry) = landing {
                            if self.still_carried(&entry) {
                                self.landed.push_back(entry);
                            }
                        }
                        self.log.push(Line::new(format!("kept {short}"), Style::fg(GRAY)));
                        self.log_to_end(term);
                        self.note(format!("kept {short}"), Style::fg(GRAY));
                    }
                }
                // A push can publish several landings at once; each one's
                // cleanup is offered in turn, and only after the one before it
                // has been answered.
                if sweeping {
                    self.offer_landed_cleanup(term);
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
                // The branch's remote copies stay put until a push publishes
                // the commit that now carries their work.
                self.landed.push_back(Landed { refname, oid, sha: sha.clone() });
                self.log.push(Line::new(
                    format!("landed {} on {} — nothing published yet", land::short(&sha), self.base),
                    Style::fg(GREEN).with_bold(),
                ));
                self.log.push(Line::new(
                    format!("q then p pushes {} to its remote, P to every remote", self.base),
                    Style::fg(CYAN),
                ));
                self.log_to_end(term);
                // A listing hiccup must not tear down the TUI after a good land.
                self.refresh_quietly();
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

    fn run_push(
        &mut self,
        remotes: &[(String, String)],
        sha: &str,
        term: &mut dyn Ui,
    ) -> io::Result<()> {
        self.log_title = format!("pushing {}", self.base);
        self.log.push(Line::new(
            format!("pushing {} to {}…", self.base, pushable(remotes).join(", ")),
            Style::fg(YELLOW),
        ));
        self.log_to_end(term);
        self.redraw(term)?;

        let mut pushed = land::push_all(&self.git, &self.base, sha, remotes)?;
        let (all_ok, count) = (pushed.all_ok, pushed.count());
        self.log.extend(std::mem::take(&mut pushed.log));
        self.log.push(match (all_ok, count) {
            (true, 0) => Line::new(
                "no remote was eligible — nothing was published",
                Style::fg(YELLOW).with_bold(),
            ),
            (true, n) => Line::new(
                format!(
                    "published {} to {n} remote{}",
                    self.base,
                    if n == 1 { "" } else { "s" }
                ),
                Style::fg(GREEN).with_bold(),
            ),
            (false, 0) => Line::new(
                "every remote rejected the push — the commits are still local",
                Style::fg(RED).with_bold(),
            ),
            // Published somewhere, so "still local" would be a lie — and the
            // cleanup offer stays down, since not every target has the work.
            (false, n) => Line::new(
                format!("published to {n}, but some remotes rejected the push"),
                Style::fg(RED).with_bold(),
            ),
        });
        self.log_to_end(term);
        // A listing hiccup must not tear down the TUI after a successful push.
        self.refresh_quietly();
        // Only offer cleanup once the work is genuinely published somewhere.
        // Cleanup rides on the remotes the push actually reached — a mirror
        // that rejected it must not lose the branch, but the ones that took it
        // are done with theirs.
        if !pushed.reached.is_empty() {
            self.pushed = Some(Published {
                sha: sha.to_string(),
                remotes: pushed.reached.clone(),
                offered: Vec::new(),
            });
            self.offer_landed_cleanup(term);
        }
        Ok(())
    }

    /// Offer to delete each branch landed this session, now that a push has
    /// published the commits carrying their work. One at a time: the
    /// confirmation names a single branch, so the next is offered only once
    /// this one has been answered.
    fn offer_landed_cleanup(&mut self, term: &mut dyn Ui) {
        let Some(Published { sha, remotes, mut offered }) = self.pushed.take() else {
            return;
        };
        // Bounded by the queue length: an entry this push did not publish goes
        // back for a later one rather than being dropped unasked.
        for _ in 0..self.landed.len() {
            let Some(entry) = self.landed.pop_front() else {
                break;
            };
            // Only a push carrying the commit that took over the branch's work
            // clears it for deletion: HEAD can have been rebuilt since.
            let published = self.git.contains(&sha, &entry.sha).unwrap_or(false)
                && !offered.contains(&entry.refname);
            if published && !self.still_carried(&entry) {
                continue; // no remote carries it any more: nothing left to ask
            }
            if published && self.carried_by(&entry, Some(&remotes)).unwrap_or(false) {
                let refname = entry.refname.clone();
                self.offer_delete(&refname, Only::Reached(remotes.clone()), Some(&entry.oid), term);
                // The prompt owns the landing from here: its answer decides
                // whether the entry is done with or waits for another push.
                if matches!(self.prompt, Some(Prompt::Delete { .. })) {
                    offered.push(refname);
                    if let Some(Prompt::Delete { landing, .. }) = self.prompt.as_mut() {
                        *landing = Some(entry);
                    }
                    self.pushed = Some(Published { sha, remotes, offered });
                    return;
                }
            }
            self.landed.push_back(entry);
        }
    }

    /// Whether any pushable remote still carries the landed branch at the tip
    /// the landing took — i.e. whether there is any cleanup left to offer. An
    /// unanswerable git reads as "yes": the answer decides whether to forget
    /// the landing, and a transient failure must not do that.
    fn still_carried(&self, entry: &Landed) -> bool {
        self.carried_by(entry, None).unwrap_or(true)
    }

    /// Whether the remotes `only` names carry it. Used to decide whether to
    /// raise the confirmation now, so here an unanswerable git reads as "no" —
    /// the entry then waits for a later push rather than being asked about.
    fn carried_by(&self, entry: &Landed, only: Option<&[String]>) -> io::Result<bool> {
        let names = self.git.remote_names()?;
        let (_, short) = git::split_remote(&entry.refname, &names);
        let (targets, _) = land::delete_plan(&self.git, short, only, Some(&entry.oid))?;
        Ok(!targets.is_empty())
    }

    /// Raise the delete confirmation for `refname`, naming the remotes that
    /// actually carry it. `only` limits which remotes it may reach. Silent when
    /// none of them carry it.
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
            Only::RefsRemote => named.map(|n| vec![n.to_string()]),
            Only::Reached(names) => Some(names),
        };
        let (targets, diverged) =
            match land::delete_plan(&self.git, &short, only.as_deref(), landed_oid) {
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
        self.ask(Prompt::Delete { short, targets, landing: None })
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

        let mut deleted = land::delete_branch(&self.git, &self.base, short, targets)?;
        let (all_ok, count) = (deleted.all_ok, deleted.count());
        self.log.extend(std::mem::take(&mut deleted.log));
        self.log.push(match (all_ok, count) {
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
            ("f", "fetch + prune the base's remote (else origin)"),
            ("F", "fetch + prune every remote, mirrors included"),
            ("p", "push the base to its remote (else origin)"),
            ("P", "push the base to every remote (no_push skipped)"),
            ("r", "re-read branches"),
            ("/", "filter by branch name (esc clears)"),
            ("D", "delete the selected branch from the remote its row names"),
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
            ("q, then p then y", "publish it: push the base (P = every remote)"),
            ("after the push", "y deletes the landed branch, n keeps it"),
            ("esc", "keep it and end the sweep (the rest wait for a push)"),
        ],
    );
    for prose in [
        "The branch is pinned to the commit you reviewed: if the ref moves",
        "before you approve, the landing refuses rather than committing work",
        "you have not seen. Landing only commits — the push is a separate,",
        "deliberate step, and it publishes every local commit on the base.",
        "Branches landed this session are offered for deletion after it.",
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

/// Keys that move a pane rather than answer the confirmation drawn over it.
/// Enter is deliberately absent: over a prompt it reads as an answer, and the
/// safe reading of an ambiguous answer is "no".
fn is_scroll(key: Key) -> bool {
    matches!(
        key,
        Key::Char('j')
            | Key::Char('k')
            | Key::Char(' ')
            | Key::Char('b')
            | Key::Char('g')
            | Key::Char('G')
            | Key::Up
            | Key::Down
            | Key::PageUp
            | Key::PageDown
            | Key::Home
            | Key::End
            | Key::Ctrl('f')
            | Key::Ctrl('b')
            | Key::Ctrl('d')
            | Key::Ctrl('u')
    )
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
        format!("  tip {}  merge-base {}", land::short(&branch.commit), land::short(&p.merge_base)),
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
        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q'), Key::Char('P')] {
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

    /// Landing commits and stops. Publishing is `p`, a separate deliberate
    /// step — so `a y` must leave every remote exactly where it was, and must
    /// not offer to delete a branch whose work is still only local.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn landing_commits_but_publishes_nothing_until_p() {
        let (root, work) = repo("land-no-push");
        let origin = root.join("origin.git");
        let before = git_in(&origin, &["rev-parse", "main"]).trim().to_string();
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [Key::Enter, Key::Char('a'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }

        assert!(app.prompt.is_none(), "landing must not raise a push or delete confirmation");
        assert_eq!(git_in(&origin, &["rev-parse", "main"]).trim(), before, "landing published");
        assert!(
            git_in(&origin, &["rev-parse", "--verify", "work-0001-feature"]).trim().len() >= 40,
            "the branch was deleted before its work was published"
        );
        let landed = git_in(&work, &["rev-parse", "main"]).trim().to_string();
        assert_ne!(landed, before, "landing did not commit");

        for key in [Key::Char('q'), Key::Char('p'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }

        assert_eq!(git_in(&origin, &["rev-parse", "main"]).trim(), landed, "p did not publish");
        let _ = std::fs::remove_dir_all(root);
    }

    /// `p` reaches the base's remote alone; the mirror waits for `P`. A mirror
    /// that is slower or down must not hold up publishing the landing.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn p_pushes_the_base_remote_only_and_capital_p_reaches_the_mirror() {
        let (root, work) = repo("push-targets");
        let origin = root.join("origin.git");
        let backup = root.join("backup.git");
        git_in(&root, &["init", "--bare", "-b", "main", &backup.to_string_lossy()]);
        git_in(&work, &["remote", "add", "backup", &backup.to_string_lossy()]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q')] {
            app.handle(key, &mut ui).unwrap();
        }
        let landed = git_in(&work, &["rev-parse", "main"]).trim().to_string();
        app.handle(Key::Char('p'), &mut ui).unwrap();
        app.handle(Key::Char('y'), &mut ui).unwrap();

        assert_eq!(git_in(&origin, &["rev-parse", "main"]).trim(), landed, "p missed origin");
        assert_ne!(git_in(&backup, &["rev-parse", "main"]).trim(), landed, "p reached the mirror");

        // The cleanup offer stands on the push; answer it, then sweep with P.
        app.handle(Key::Char('n'), &mut ui).unwrap();
        for key in [Key::Char('q'), Key::Char('P'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }

        assert_eq!(git_in(&backup, &["rev-parse", "main"]).trim(), landed, "P missed the mirror");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A push only clears a branch for deletion if it published THAT branch's
    /// landing. If the base was rebuilt in between, the push carries something
    /// else and the branch must survive it — then be offered once the landing
    /// really does go out.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_push_that_does_not_carry_the_landing_does_not_offer_its_branch() {
        let (root, work) = repo("cleanup-gate");
        let origin = root.join("origin.git");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q')] {
            app.handle(key, &mut ui).unwrap();
        }
        let landing = git_in(&work, &["rev-parse", "main"]).trim().to_string();
        // The landing is undone and the base rebuilt: what a push publishes now
        // is not the commit that took over the branch's work.
        git_in(&work, &["reset", "--hard", "HEAD~1"]);
        std::fs::write(work.join("other"), "unrelated\n").unwrap();
        git_in(&work, &["add", "other"]);
        git_in(&work, &["commit", "-m", "someone else's commit"]);

        for key in [Key::Char('p'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }

        assert!(app.prompt.is_none(), "cleanup was offered for an unpublished landing");
        assert!(
            git_in(&origin, &["rev-parse", "--verify", "work-0001-feature"]).trim().len() >= 40,
            "the branch was cleaned up though its landing never went out"
        );

        // Put the landing back and publish it: now the offer stands.
        git_in(&work, &["reset", "--hard", &landing]);
        git_in(&work, &["push", "--force", "origin", "main"]);
        for key in [Key::Char('q'), Key::Char('p'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }
        app.redraw(&mut ui).unwrap();

        assert!(
            ui.last().contains("delete work-0001-feature"),
            "the landing was forgotten by the push that skipped it:\n{}",
            ui.last()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Cleanup may only reach where the push did. `p` published to origin, so
    /// a mirror that does not have the commit replacing the branch must keep
    /// the branch — deleting a published ref is irreversible.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn cleanup_spares_the_branch_on_a_mirror_the_push_did_not_reach() {
        let (root, work) = repo("cleanup-reach");
        let backup = root.join("backup.git");
        git_in(&root, &["init", "--bare", "-b", "main", &backup.to_string_lossy()]);
        git_in(&work, &["remote", "add", "backup", &backup.to_string_lossy()]);
        git_in(&work, &["push", "backup", "main"]);
        git_in(
            &work,
            &["push", "backup", "refs/remotes/origin/work-0001-feature:refs/heads/work-0001-feature"],
        );
        git_in(&work, &["fetch", "backup"]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [
            Key::Enter,
            Key::Char('a'),
            Key::Char('y'),
            Key::Char('q'),
            Key::Char('p'),
            Key::Char('y'),
        ] {
            app.handle(key, &mut ui).unwrap();
        }
        app.redraw(&mut ui).unwrap();

        let frame = ui.last();
        assert!(frame.contains("will delete work-0001-feature"), "no cleanup offered:\n{frame}");
        // The prompt itself, not just what fits on the pane: these are the
        // remotes confirming would actually delete from.
        let targets = match &app.prompt {
            Some(Prompt::Delete { targets, .. }) => {
                targets.iter().map(|t| t.remote.clone()).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        assert_eq!(targets, vec!["origin".to_string()], "an unreached mirror is a delete target");

        app.handle(Key::Char('y'), &mut ui).unwrap();

        let heads = |dir: &Path| git_in(dir, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]);
        let origin_heads = heads(&root.join("origin.git"));
        let backup_heads = heads(&backup);
        assert!(!origin_heads.contains("work-0001-feature"), "origin kept it: {origin_heads}");
        assert!(backup_heads.contains("work-0001-feature"), "the mirror lost it: {backup_heads}");

        // The mirror still carries it, so the landing is not done with: the
        // push that does reach the mirror offers the rest of the cleanup.
        for key in [Key::Char('q'), Key::Char('P'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }
        let targets = match &app.prompt {
            Some(Prompt::Delete { targets, .. }) => {
                targets.iter().map(|t| t.remote.clone()).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        assert_eq!(targets, vec!["backup".to_string()], "the mirror's copy was orphaned");

        app.handle(Key::Char('y'), &mut ui).unwrap();
        assert!(
            !heads(&backup).contains("work-0001-feature"),
            "the mirror kept it: {}",
            heads(&backup)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The plan a confirmation is about can be longer than the screen, so the
    /// keys that read it must not count as answering it.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn scrolling_the_plan_does_not_answer_the_confirmation() {
        let (root, work) = repo("scroll-prompt");
        let origin = root.join("origin.git");
        let before = git_in(&origin, &["rev-parse", "main"]).trim().to_string();
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q'), Key::Char('p')] {
            app.handle(key, &mut ui).unwrap();
        }
        for key in [Key::Char('j'), Key::Char('k'), Key::PageDown, Key::Char('g')] {
            app.handle(key, &mut ui).unwrap();
            assert!(matches!(app.prompt, Some(Prompt::Push { .. })), "{key:?} answered the prompt");
        }
        assert_eq!(git_in(&origin, &["rev-parse", "main"]).trim(), before, "a scroll key pushed");

        // And a real answer still lands: the pane is readable, not inert.
        app.handle(Key::Char('n'), &mut ui).unwrap();
        assert!(app.prompt.is_none(), "n did not cancel the push");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A delete that the remote rejects leaves the branch in place, and the
    /// sweep must not walk straight back into the same confirmation.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_delete_the_remote_rejects_is_not_asked_again_in_the_same_sweep() {
        let (root, work) = repo("delete-rejected");
        let origin = root.join("origin.git");
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q'), Key::Char('p')] {
            app.handle(key, &mut ui).unwrap();
        }
        app.handle(Key::Char('y'), &mut ui).unwrap();
        assert!(app.prompt.is_some(), "cleanup was not offered");

        // The branch moves on the remote after the targets were pinned: the
        // delete's lease no longer matches and the remote refuses it.
        git_in(&origin, &["branch", "-f", "work-0001-feature", "main"]);
        app.handle(Key::Char('y'), &mut ui).unwrap();

        assert!(app.prompt.is_none(), "the rejected delete was asked again immediately");
        assert!(
            git_in(&origin, &["rev-parse", "--verify", "work-0001-feature"]).trim().len() >= 40,
            "the branch went despite the refusal"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A push some remotes reject still published to the others; those are
    /// done with their copy of the branch even though the push was not all_ok.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_partial_push_still_offers_cleanup_on_the_remotes_it_reached() {
        let (root, work) = repo("partial-push");
        git_in(&work, &["remote", "add", "backup", &root.join("gone.git").to_string_lossy()]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for key in [
            Key::Enter,
            Key::Char('a'),
            Key::Char('y'),
            Key::Char('q'),
            Key::Char('P'),
            Key::Char('y'),
        ] {
            app.handle(key, &mut ui).unwrap();
        }

        let targets = match &app.prompt {
            Some(Prompt::Delete { targets, .. }) => {
                targets.iter().map(|t| t.remote.clone()).collect::<Vec<_>>()
            }
            _ => Vec::new(),
        };
        assert_eq!(targets, vec!["origin".to_string()], "the reached remote was not offered");
        let _ = std::fs::remove_dir_all(root);
    }

    /// `p` needs one remote to mean. With several and none distinguished, or
    /// none reachable by a push at all, it must say so and raise nothing.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn p_says_why_when_there_is_no_single_remote_to_push_to() {
        let (root, work) = repo("push-refusals");
        let mirror = root.join("mirror.git");
        git_in(&root, &["init", "--bare", "-b", "main", &mirror.to_string_lossy()]);
        git_in(&work, &["remote", "add", "mirror", &mirror.to_string_lossy()]);
        git_in(&work, &["remote", "rename", "origin", "upstream"]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.handle(Key::Char('p'), &mut ui).unwrap();
        assert_eq!(app.status, "no default remote — P pushes to all of them");
        assert!(app.prompt.is_none(), "an ambiguous p must not raise a confirmation");

        // A push url of `no_push` is the "never push here" convention: with
        // every target marked, P has nowhere to go either.
        for name in ["upstream", "mirror"] {
            git_in(&work, &["remote", "set-url", "--push", name, "no_push"]);
        }
        app.handle(Key::Char('P'), &mut ui).unwrap();
        assert!(app.status.starts_with("every target is marked no_push"), "{}", app.status);
        assert!(app.prompt.is_none(), "there was nothing to confirm");

        for name in ["upstream", "mirror"] {
            git_in(&work, &["remote", "remove", name]);
        }
        app.handle(Key::Char('p'), &mut ui).unwrap();
        assert_eq!(app.status, "no remotes configured");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The typeahead guard covers the push too: `py` arriving in one read may
    /// not publish anything.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_batched_p_and_y_cannot_push_unseen() {
        let (root, work) = repo("batch-py");
        let origin = root.join("origin.git");
        let before = git_in(&origin, &["rev-parse", "main"]).trim().to_string();
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();
        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q')] {
            app.handle(key, &mut ui).unwrap();
        }

        app.feed(vec![Key::Char('p'), Key::Char('y')], &mut ui).unwrap();

        assert!(app.prompt.is_some(), "the push prompt was consumed by its own batch");
        assert_eq!(git_in(&origin, &["rev-parse", "main"]).trim(), before, "pushed unseen");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Cleanup rides on the push, not the land — and one push can publish
    /// several landings, so every branch waiting on it is offered in turn.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn every_branch_landed_before_the_push_is_offered_for_cleanup_after_it() {
        let (root, work) = repo("cleanup-queue");
        let origin = root.join("origin.git");
        // A second branch touching a different file, so landing both cannot
        // conflict, published the same way the first one is.
        git_in(&work, &["checkout", "-b", "work-0002-second"]);
        std::fs::write(work.join("g"), "gee\n").unwrap();
        git_in(&work, &["add", "g"]);
        git_in(&work, &["commit", "-m", "second: step"]);
        git_in(&work, &["push", "origin", "work-0002-second"]);
        git_in(&work, &["checkout", "main"]);
        git_in(&work, &["branch", "-D", "work-0002-second"]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for name in ["work-0001-feature", "work-0002-second"] {
            app.sel = app
                .view
                .iter()
                .position(|&i| app.branches.get(i).is_some_and(|b| b.refname.ends_with(name)))
                .unwrap();
            for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q')] {
                app.handle(key, &mut ui).unwrap();
            }
        }
        assert!(app.prompt.is_none(), "two landings must still not have prompted");

        app.handle(Key::Char('p'), &mut ui).unwrap();
        app.handle(Key::Char('y'), &mut ui).unwrap();
        app.redraw(&mut ui).unwrap();
        assert!(
            ui.last().contains("delete work-0001-feature"),
            "the first landing was not offered for cleanup:\n{}",
            ui.last()
        );

        app.handle(Key::Char('n'), &mut ui).unwrap();
        app.redraw(&mut ui).unwrap();
        assert!(
            ui.last().contains("delete work-0002-second"),
            "the second landing was forgotten:\n{}",
            ui.last()
        );

        app.handle(Key::Char('y'), &mut ui).unwrap();

        let heads = git_in(&origin, &["for-each-ref", "--format=%(refname:short)", "refs/heads"]);
        assert!(heads.contains("work-0001-feature"), "a kept branch was deleted: {heads}");
        assert!(!heads.contains("work-0002-second"), "the deleted branch is still there: {heads}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A sweep has to be endable in one key: esc keeps the branch it is asking
    /// about and stops asking about the others.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn esc_ends_the_cleanup_sweep_instead_of_moving_to_the_next_branch() {
        let (root, work) = repo("cleanup-esc");
        git_in(&work, &["checkout", "-b", "work-0002-second"]);
        std::fs::write(work.join("g"), "gee\n").unwrap();
        git_in(&work, &["add", "g"]);
        git_in(&work, &["commit", "-m", "second: step"]);
        git_in(&work, &["push", "origin", "work-0002-second"]);
        git_in(&work, &["checkout", "main"]);
        git_in(&work, &["branch", "-D", "work-0002-second"]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for name in ["work-0001-feature", "work-0002-second"] {
            app.sel = app
                .view
                .iter()
                .position(|&i| app.branches.get(i).is_some_and(|b| b.refname.ends_with(name)))
                .unwrap();
            for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q')] {
                app.handle(key, &mut ui).unwrap();
            }
        }
        for key in [Key::Char('p'), Key::Char('y'), Key::Esc] {
            app.handle(key, &mut ui).unwrap();
        }

        assert!(app.prompt.is_none(), "esc must not step to the next branch");
        let heads = git_in(&root.join("origin.git"), &["for-each-ref", "--format=%(refname:short)"]);
        assert!(heads.contains("work-0001-feature"), "esc deleted something: {heads}");
        assert!(heads.contains("work-0002-second"), "esc deleted something: {heads}");

        // Esc ended the sweep, not the session's cleanup: what it never asked
        // about comes back with the next push, as the help says it does.
        std::fs::write(work.join("later"), "more\n").unwrap();
        git_in(&work, &["add", "later"]);
        git_in(&work, &["commit", "-m", "a later commit on the base"]);
        for key in [Key::Char('q'), Key::Char('p'), Key::Char('y')] {
            app.handle(key, &mut ui).unwrap();
        }
        app.redraw(&mut ui).unwrap();
        assert!(
            ui.last().contains("delete work-0002-second"),
            "the branch esc skipped was forgotten:\n{}",
            ui.last()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The typeahead guard covers the sweep too: `yy` in one read may delete
    /// the branch it was raised for and nothing else.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn a_batched_yy_cannot_answer_the_next_branch_in_the_sweep() {
        let (root, work) = repo("batch-sweep");
        git_in(&work, &["checkout", "-b", "work-0002-second"]);
        std::fs::write(work.join("g"), "gee\n").unwrap();
        git_in(&work, &["add", "g"]);
        git_in(&work, &["commit", "-m", "second: step"]);
        git_in(&work, &["push", "origin", "work-0002-second"]);
        git_in(&work, &["checkout", "main"]);
        git_in(&work, &["branch", "-D", "work-0002-second"]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        for name in ["work-0001-feature", "work-0002-second"] {
            app.sel = app
                .view
                .iter()
                .position(|&i| app.branches.get(i).is_some_and(|b| b.refname.ends_with(name)))
                .unwrap();
            for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q')] {
                app.handle(key, &mut ui).unwrap();
            }
        }
        app.handle(Key::Char('p'), &mut ui).unwrap();

        app.feed(vec![Key::Char('y'), Key::Char('y')], &mut ui).unwrap();

        let heads = git_in(&root.join("origin.git"), &["for-each-ref", "--format=%(refname:short)"]);
        assert!(
            heads.contains("work-0002-second"),
            "the second branch was deleted by a key typed before its prompt: {heads}"
        );
        assert!(app.prompt.is_some(), "the sweep must still be waiting on an answer");
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
        assert_eq!(pushed.count(), 0, "{text}");
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

        for key in [Key::Enter, Key::Char('a'), Key::Char('y'), Key::Char('q'), Key::Char('p')] {
            app.handle(key, &mut ui).unwrap();
        }
        app.redraw(&mut ui).unwrap();

        let frame = ui.last();
        assert!(frame.contains("[y] push"), "the push prompt was not raised:\n{frame}");
        assert!(frame.contains("will push main at"), "the commit is not named:\n{frame}");
        assert!(frame.contains("to: origin"), "remotes not named:\n{frame}");
        assert!(frame.contains("as of the last fetch"), "the pane must date its data:\n{frame}");
        // The squash commit it is about to publish, named before the y.
        assert!(frame.contains("feature: step"), "the commits are not named:\n{frame}");
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

    /// The refresh an integrator reaches for constantly must not depend on every
    /// mirror being reachable: `f` goes to the base's remote alone, and only `F`
    /// reaches the one that is down.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn f_fetches_only_the_base_remote_and_capital_f_sweeps_them_all() {
        let (root, work) = repo("fetch-default");
        // A branch only a fetch of origin can discover, plus a mirror that is
        // not there — pointed at a path that was never initialised.
        git_in(&root.join("origin.git"), &["branch", "work-0002-later", "main"]);
        git_in(&work, &["remote", "add", "backup", &root.join("gone.git").to_string_lossy()]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.handle(Key::Char('f'), &mut ui).unwrap();

        assert_eq!(app.status, "fetched origin", "f must not have swept every remote");
        assert!(
            app.branches.iter().any(|b| b.refname == "origin/work-0002-later"),
            "f did not actually fetch origin"
        );

        app.handle(Key::Char('F'), &mut ui).unwrap();

        assert!(
            app.status.starts_with("fetch failed"),
            "F must reach the broken mirror: {}",
            app.status
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Which remote `f` means. Guessing wrong would refresh a mirror while the
    /// list keeps showing a stale copy of the remote that is landed to.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn the_default_remote_is_the_one_the_base_tracks() {
        let (root, work) = repo("default-remote");
        let git = Git::discover(&work).unwrap();
        let remote = |name: &str| DefaultRemote::Remote(name.to_string());
        assert_eq!(git.default_remote("main").unwrap(), remote("origin"));

        let mirror = root.join("mirror.git");
        git_in(&root, &["init", "--bare", "-b", "main", &mirror.to_string_lossy()]);
        git_in(&work, &["remote", "add", "mirror", &mirror.to_string_lossy()]);
        git_in(&work, &["config", "branch.main.remote", "mirror"]);
        assert_eq!(git.default_remote("main").unwrap(), remote("mirror"));

        // `.` means the base tracks a LOCAL branch: not a remote, so origin.
        git_in(&work, &["config", "branch.main.remote", "."]);
        assert_eq!(git.default_remote("main").unwrap(), remote("origin"));

        // A left-behind name (the remote was renamed or removed) is not a
        // remote either — and a URL, which bare `git fetch` would take, has no
        // tracking refs to prune.
        git_in(&work, &["config", "branch.main.remote", "gone"]);
        assert_eq!(git.default_remote("main").unwrap(), remote("origin"));
        git_in(&work, &["config", "branch.main.remote", &mirror.to_string_lossy()]);
        assert_eq!(git.default_remote("main").unwrap(), remote("origin"));

        // Several remotes, none of them origin and none tracked: nothing is
        // distinguished, so `f` says so rather than picking one.
        git_in(&work, &["config", "--unset", "branch.main.remote"]);
        git_in(&work, &["remote", "rename", "origin", "upstream"]);
        assert_eq!(git.default_remote("main").unwrap(), DefaultRemote::Ambiguous);

        git_in(&work, &["remote", "remove", "mirror"]);
        assert_eq!(git.default_remote("main").unwrap(), remote("upstream"));

        git_in(&work, &["remote", "remove", "upstream"]);
        assert_eq!(git.default_remote("main").unwrap(), DefaultRemote::NoRemotes);
        let _ = std::fs::remove_dir_all(root);
    }

    /// With no remote to single out, `f` must say so and run no fetch at all:
    /// falling through to every remote is exactly what `F` is for, and in a
    /// repo with no remotes `git fetch --all` succeeds having done nothing,
    /// which would paint a green "fetched" over a no-op.
    #[test]
    #[ignore = "drives a real git repo; the sandbox gate has no git, the host preflight does"]
    fn f_without_a_default_remote_fetches_nothing_and_says_which_case_it_is() {
        let (root, work) = repo("no-default");
        // A branch that only a fetch would bring in — none must arrive below.
        git_in(&root.join("origin.git"), &["branch", "work-0002-later", "main"]);
        let mirror = root.join("mirror.git");
        git_in(&root, &["init", "--bare", "-b", "main", &mirror.to_string_lossy()]);
        git_in(&work, &["remote", "add", "mirror", &mirror.to_string_lossy()]);
        git_in(&work, &["remote", "rename", "origin", "upstream"]);
        let mut app = app_on(&work);
        let mut ui = FakeUi::new();

        app.handle(Key::Char('f'), &mut ui).unwrap();

        assert_eq!(app.status, "no default remote — F fetches all of them");
        assert!(
            !app.branches.iter().any(|b| b.refname.ends_with("work-0002-later")),
            "f fetched despite having no default remote"
        );

        for name in ["mirror", "upstream"] {
            git_in(&work, &["remote", "remove", name]);
        }
        app.handle(Key::Char('f'), &mut ui).unwrap();

        assert_eq!(app.status, "no remotes configured", "a no-op must not read as a refresh");
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
