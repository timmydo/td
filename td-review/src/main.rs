//! td-review — the integrator's branch review and landing TUI. It replays the
//! branch's own commits onto the base (`r`, and the default headless mode) or
//! squashes them into one (`s`), then a separate `p`/`P` push, driven with
//! plumbing so each step's outcome is visible and testable. A `w` sweep removes
//! the worktrees of branches that have fully landed. The non-interactive modes
//! exist for scripting and for the integration test.
#![forbid(unsafe_code)]

mod app;
mod git;
mod land;
mod record;
mod term;
mod worktrees;

use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use app::{App, Flow};
use git::{now_unix, Git};
use land::Outcome;
use term::{scrub, scrub_lines, Terminal};

const USAGE: &str = "\
usage: td-review [options]

  Review remote branches against the base branch and land the approved ones.

options:
  -C, --repo <path>   work tree to operate on (default: current directory)
  -b, --base <name>   branch to review against and land into (default: main)
      --list          print the branch table and exit (no TUI)
      --preview <br>  print what landing <br> would stage, and exit
      --land <branch> land <branch> non-interactively, replaying its own
                      commits onto the base; needs --yes
      --squash        with --land, collapse them into one commit instead
      --push          with --land, also push the base to every remote (`P`;
                      the TUI's `p` pushes to the base's remote alone). A
                      remote with `remote.<name>.skipPushAll` set is left out
                      of this and of `P`, and of nothing else
      --expect <oid>  with --land, require the branch to still be at <oid>
      --expect-base <oid>
                      with --land, require the base to still be at <oid>
      --delete <br>   delete <br> from every pushable remote, skipPushAll or
                      not — it names a branch, not a push; needs --yes
      --delete-landed with --land --push, delete the branch once it is
                      published, from the remotes the push reached
      --prune-worktrees
                      show the worktrees whose branch has fully landed (clean,
                      unpushed, not -rolling); removes them with --yes
      --yes           confirm a non-interactive --land, --delete or sweep
  -h, --help          this text

keys (TUI):
  j/k move   enter review   r reload   / filter   D delete   w worktrees
  ? help   q quit
  f fetch the base's remote   F fetch every remote
  p push the base to its remote   P push the base to every remote that has
  not set remote.<name>.skipPushAll
  in review: j/k or space/b scroll, p pager, s squash + land, r rebase + land
  landing commits only; p publishes it afterwards, unconfirmed, and
  deletes the branches it published from the remotes it reached
";

struct Args {
    repo: PathBuf,
    base: String,
    list: bool,
    preview: Option<String>,
    land: Option<String>,
    squash: bool,
    push: bool,
    expect: Option<String>,
    expect_base: Option<String>,
    delete: Option<String>,
    delete_landed: bool,
    prune_worktrees: bool,
    yes: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        repo: env::current_dir().map_err(|e| format!("current directory: {e}"))?,
        base: "main".to_string(),
        list: false,
        preview: None,
        land: None,
        squash: false,
        push: false,
        expect: None,
        expect_base: None,
        delete: None,
        delete_landed: false,
        prune_worktrees: false,
        yes: false,
    };
    let mut it = env::args().skip(1);
    while let Some(arg) = it.next() {
        // Both spellings: `--base=main` otherwise lands in the unknown-argument
        // arm, which reads as "no such flag" rather than "wrong syntax".
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg, None),
        };
        // A value on a switch is a typo, not a request.
        let bare = || match inline {
            Some(_) => Err(format!("{flag} takes no value")),
            None => Ok(()),
        };
        let mut value = |what: &str| match inline.clone().or_else(|| it.next()) {
            Some(v) => Ok(v),
            None => Err(format!("{flag} needs {what}")),
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "-C" | "--repo" => args.repo = PathBuf::from(value("a path")?),
            "-b" | "--base" => args.base = value("a branch name")?,
            "--list" => {
                bare()?;
                args.list = true
            }
            "--preview" => args.preview = Some(value("a branch name")?),
            "--land" => args.land = Some(value("a branch name")?),
            "--squash" => {
                bare()?;
                args.squash = true
            }
            "--push" => {
                bare()?;
                args.push = true
            }
            "--expect" => args.expect = Some(value("an oid")?),
            "--expect-base" => args.expect_base = Some(value("an oid")?),
            "--delete" => args.delete = Some(value("a branch name")?),
            "--delete-landed" => {
                bare()?;
                args.delete_landed = true
            }
            "--prune-worktrees" => {
                bare()?;
                args.prune_worktrees = true
            }
            "--yes" => {
                bare()?;
                args.yes = true
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("td-review: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("td-review: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> io::Result<ExitCode> {
    let git = Git::discover(&args.repo)?;

    // The base must exist locally: it is both the review target and the branch
    // the squash commit lands on.
    if git.rev_parse(&format!("refs/heads/{}", args.base)).is_err() {
        return Err(io::Error::other(format!(
            "no local branch '{}' — check it out, or pass --base",
            args.base
        )));
    }

    let modes = u8::from(args.list)
        + u8::from(args.preview.is_some())
        + u8::from(args.land.is_some())
        + u8::from(args.delete.is_some())
        + u8::from(args.prune_worktrees);
    if modes > 1 {
        return Err(io::Error::other(
            "--list, --preview, --land, --delete and --prune-worktrees are mutually exclusive",
        ));
    }
    if args.land.is_none() && args.push {
        return Err(io::Error::other("--push is only meaningful with --land"));
    }
    if args.land.is_none() && args.squash {
        return Err(io::Error::other("--squash is only meaningful with --land"));
    }
    if args.land.is_none() && (args.expect.is_some() || args.expect_base.is_some()) {
        return Err(io::Error::other("--expect/--expect-base are only meaningful with --land"));
    }
    if args.land.is_none() && args.delete.is_none() && !args.prune_worktrees && args.yes {
        return Err(io::Error::other(
            "--yes is only meaningful with --land, --delete or --prune-worktrees",
        ));
    }
    if args.delete_landed && !(args.land.is_some() && args.push) {
        return Err(io::Error::other("--delete-landed needs --land <branch> --push"));
    }
    if args.list {
        return list_branches(&git, &args.base).map(|()| ExitCode::SUCCESS);
    }
    if args.prune_worktrees {
        return prune_worktrees(&git, &args.base, args.yes);
    }
    if let Some(branch) = args.preview.clone() {
        return show_preview(&git, &args.base, &branch).map(|()| ExitCode::SUCCESS);
    }
    if let Some(branch) = args.delete.clone() {
        // Hand-named on the command line, like `D` in the TUI: no landed oid to
        // filter by, so every pushable remote carrying it is a target.
        return delete_headless(&git, &args.base, &branch, args.yes, None);
    }
    if let Some(branch) = args.land.clone() {
        return land_headless(&git, &args.base, &branch, &args);
    }
    run_tui(git, args.base).map(|()| ExitCode::SUCCESS)
}

fn list_branches(git: &Git, base: &str) -> io::Result<()> {
    let branches = git.branches(base)?;
    // Resolved once for the whole run, as the TUI does: a base that moved
    // mid-listing would leave two rows describing different bases.
    let base_tree = git.resolve_base(base).ok();
    let now = now_unix();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for b in &branches {
        // Both halves of the verdict from the same two functions the TUI's rows
        // use — this column showing a record where the TUI showed what a
        // LANDING would find is how a branch already on the base came to read
        // `ok` in one of them. Per ROW rather than per run, so the listing
        // still streams: the merge behind each is a process, and a run of them
        // up front is a silent pause before the first line.
        let ready = app::readiness_of(git, base, b);
        let prospect = git.prospect_of(base_tree.as_ref(), b);
        // Refnames and subjects are untrusted; --list prints straight to a
        // terminal, so neutralise them here as the TUI's frame does.
        //
        // The TUI's floor width, not a hand-typed copy of it. Unlike the TUI
        // this cannot GROW to an outsized verdict (that needs every row first,
        // which is the pause above): `10/100!` overruns it and shifts its own
        // row, as it did when this column was narrower still.
        writeln!(
            out,
            "{:>5}  {:>7}  {:<ready$}  {}\t{}",
            b.age(now),
            app::counts_cell(b, prospect),
            app::ready_cell(b, Some(&ready), prospect).0,
            scrub(&b.refname),
            scrub(&b.subject),
            ready = app::READY_COL,
        )?;
    }
    Ok(())
}

/// Show what the worktree sweep would remove, and with `--yes` remove it. The
/// worktrees are agents' working directories, so the default is to say and do
/// nothing; every kept one prints the reason it was kept.
fn prune_worktrees(git: &Git, base: &str, yes: bool) -> io::Result<ExitCode> {
    let planned = worktrees::plan(git, base)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut removable = 0usize;
    for p in &planned {
        match &p.verdict {
            worktrees::Verdict::Remove => {
                removable += 1;
                writeln!(out, "remove  {}", scrub(&p.worktree.path))?;
            }
            worktrees::Verdict::Keep(why) => {
                writeln!(out, "keep    {}  ({})", scrub(&p.worktree.path), scrub(why))?;
            }
        }
    }
    if removable == 0 {
        writeln!(out, "\nnothing to sweep")?;
        return Ok(ExitCode::SUCCESS);
    }
    if !yes {
        writeln!(out, "\n{removable} worktree(s) would be removed; re-run with --yes")?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut failed = 0usize;
    for swept in worktrees::sweep(git, &planned) {
        match swept.error {
            None => writeln!(out, "removed {}", scrub(&swept.path))?,
            Some(e) => {
                failed += 1;
                writeln!(out, "FAILED  {}: {}", scrub(&swept.path), scrub(&e))?;
            }
        }
    }
    if failed > 0 {
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// Print what landing `branch` would stage, without staging it — the same
/// preview the review pane renders, for a look that does not open the TUI.
fn show_preview(git: &Git, base: &str, branch: &str) -> io::Result<()> {
    let p = land::preview(git, base, branch)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "branch {}", scrub(&p.branch_oid))?;
    writeln!(out, "base   {}", scrub(&p.base_oid))?;
    writeln!(out, "merge-base {}", scrub(&p.merge_base))?;
    if let Some(note) = &p.note {
        writeln!(out, "! {}", scrub(note))?;
    }
    writeln!(out, "\n{}", scrub_lines(p.message.trim()))?;
    writeln!(out, "\n{}", scrub_lines(p.stat.trim_end()))?;
    writeln!(out, "\n{}", scrub_lines(p.diff.trim_end()))?;
    Ok(())
}

/// Delete a branch from every pushable remote that carries it. Irreversible for
/// anyone who has not already fetched it, hence the mandatory `--yes`.
fn delete_headless(
    git: &Git,
    base: &str,
    branch: &str,
    yes: bool,
    landed: Option<(&str, &[String])>,
) -> io::Result<ExitCode> {
    if !yes {
        return Err(io::Error::other(
            "--delete removes a published ref; re-run with --yes to confirm",
        ));
    }
    let remotes = git.remote_names()?;
    let (named, short) = git::split_remote(branch, &remotes);
    // Post-land cleanup considers every remote — so it can SAY which ones it
    // leaves alone — and deletes only from those the push REACHED, as the
    // TUI's sweep does: a remote that did not take the landing must keep its
    // copy of the branch, which since `skipPushAll` includes one that was
    // never asked. The oid filter is what makes even the reached ones safe; an
    // ad-hoc `--delete origin/x` touches only the remote it names.
    let landed_oid = landed.map(|(oid, _)| oid);
    let only = match landed {
        Some(_) => None,
        None => named.map(|n| vec![n.to_string()]),
    };
    let (targets, diverged) = land::delete_plan(git, short, only.as_deref(), landed_oid)?;
    for d in &diverged {
        println!("{}/{} is not the landed commit — left alone", scrub(&d.remote), scrub(short));
    }
    let targets = match landed {
        Some((_, reached)) => {
            let (theirs, mine): (Vec<_>, Vec<_>) =
                targets.into_iter().partition(|t| !reached.contains(&t.remote));
            for t in &theirs {
                println!(
                    "{}/{} kept: the push did not reach {}",
                    scrub(&t.remote),
                    scrub(short),
                    scrub(&t.remote)
                );
            }
            mine
        }
        None => targets,
    };
    let deleted = land::delete_branch(git, base, short, &targets)?;
    for line in &deleted.log {
        println!("{}", scrub(&line.text));
    }
    if !deleted.all_ok {
        return Ok(ExitCode::FAILURE);
    }
    if deleted.count() == 0 {
        // After a landing, nothing to delete means the oid filter did its job.
        // Only a hand-typed --delete that matched nothing is a failure.
        if landed_oid.is_none() {
            eprintln!("td-review: no pushable remote carries '{}'", scrub(short));
            return Ok(ExitCode::FAILURE);
        }
        // Not "no pushable remote": one that sat the push-all out is pushable
        // and was simply never asked, and it is named above as kept.
        println!("no remote this push reached carries the landed {}", scrub(short));
    }
    Ok(ExitCode::SUCCESS)
}

fn land_headless(git: &Git, base: &str, branch: &str, args: &Args) -> io::Result<ExitCode> {
    if !args.yes {
        return Err(io::Error::other(
            "--land rewrites the base branch; re-run with --yes to confirm",
        ));
    }
    // Captured before the landing: the delete filter compares remotes against
    // the branch tip that was landed, not the new commit on the base.
    let landed_oid = git.branch_oid(branch).ok();
    // Replay by default: the workflow AGENTS.md documents lands each commit
    // verbatim, and a default that quietly squashes them would collapse the
    // per-commit records and checkpoints that whole model rests on.
    let mode = if args.squash { land::Mode::Squash } else { land::Mode::Rebase };
    let landing = land::land(
        git,
        base,
        branch,
        mode,
        args.expect.as_deref(),
        args.expect_base.as_deref(),
    )?;
    for line in &landing.log {
        println!("{}", scrub(&line.text));
    }
    match &landing.outcome {
        Outcome::Committed { .. } => {}
        Outcome::Conflict => {
            eprintln!("td-review: conflicts left in the work tree — resolve or `git reset --hard HEAD`");
            return Ok(ExitCode::FAILURE);
        }
        Outcome::Nothing => return Ok(ExitCode::FAILURE),
        Outcome::Blocked(why) | Outcome::Failed(why) => {
            eprintln!("td-review: {}", scrub_lines(why));
            return Ok(ExitCode::FAILURE);
        }
    }
    if args.push {
        let Outcome::Committed { sha } = &landing.outcome else {
            return Ok(ExitCode::FAILURE);
        };
        let land::PushAllTargets { targets: remotes, left_out } = land::push_all_targets(git)?;
        for (name, why) in &left_out {
            println!("{}", scrub(&why.line(name)));
        }
        // A push publishes every local commit on the base, not only the one
        // just landed; name them rather than let them ride along unannounced.
        let unpushed = git.unpushed(base)?;
        if unpushed.len() > 1 {
            println!("publishing {} commits on {base}:", unpushed.len());
            for c in &unpushed {
                println!("  {}", scrub(c));
            }
        }
        let pushed = land::push_all(git, base, sha, &remotes)?;
        for line in &pushed.log {
            println!("{}", scrub(&line.text));
        }
        if !pushed.all_ok {
            return Ok(ExitCode::FAILURE);
        }
        if pushed.count() == 0 {
            // Still a failure — `--push` was asked for and the commit is local
            // — but which reason it was matters to whoever reads the log: one
            // is a config nobody meant, the other is one they wrote.
            let why = if remotes.is_empty() && !left_out.is_empty() {
                "every remote asked to be left out"
            } else {
                "no remote was eligible"
            };
            eprintln!("td-review: {why} — the commit was not published");
            return Ok(ExitCode::FAILURE);
        }
        // Only ever after a verified publish: the branch is the sole copy of
        // that work until the base carrying it reaches the remotes.
        if args.delete_landed {
            let remotes = git.remote_names()?;
            let (_, short) = git::split_remote(branch, &remotes);
            // A rolling branch survives its landings by definition; sweeping it
            // would delete the branch the agent is still working on.
            if git::is_rolling(short) {
                println!("{} is a rolling branch — kept", scrub(short));
                return Ok(ExitCode::SUCCESS);
            }
            // A failed cleanup is not a failed landing: the work is published.
            // Report it distinctly rather than as "nothing landed".
            let landed = landed_oid.as_deref().map(|oid| (oid, pushed.reached.as_slice()));
            if delete_headless(git, base, branch, true, landed)? != ExitCode::SUCCESS {
                eprintln!("td-review: landed and published, but the branch delete failed");
                return Ok(ExitCode::from(3));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_tui(git: Git, base: String) -> io::Result<()> {
    let mut app = App::new(git, base);
    app.reload()?;
    let mut term = Terminal::open()?;
    loop {
        let (rows, cols) = term.size();
        term.draw(app.render(rows, cols))?;
        // The prompt is on screen now: drop anything typed while the last
        // command ran, before reading the answer.
        app.settle_prompt(&mut term)?;
        let keys = term.read_keys()?;
        if keys.is_empty() {
            return Ok(()); // tty closed
        }
        if matches!(app.feed(keys, &mut term)?, Flow::Quit) {
            return Ok(());
        }
    }
}
