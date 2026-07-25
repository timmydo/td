//! td-review — the integrator's branch review and landing TUI. It is
//! `git squash-in` then `git commit` then `git pushall`, driven with plumbing
//! so each step's outcome is visible and testable. The non-interactive modes
//! exist for scripting and for the integration test.
#![forbid(unsafe_code)]

mod app;
mod git;
mod land;
mod term;

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
      --land <branch> land <branch> non-interactively; needs --yes
      --push          with --land, also push the base to every remote
      --expect <oid>  with --land, require the branch to still be at <oid>
      --expect-base <oid>
                      with --land, require the base to still be at <oid>
      --delete <br>   delete <br> from every pushable remote; needs --yes
      --delete-landed with --land --push, delete the branch once it is published
      --yes           confirm a non-interactive --land or --delete
  -h, --help          this text

keys (TUI):
  j/k move   enter review   f fetch base's remote   F fetch all   r reload
  / filter   D delete   ? help   q quit
  in review: j/k or space/b scroll, p pager, a approve + land
";

struct Args {
    repo: PathBuf,
    base: String,
    list: bool,
    preview: Option<String>,
    land: Option<String>,
    push: bool,
    expect: Option<String>,
    expect_base: Option<String>,
    delete: Option<String>,
    delete_landed: bool,
    yes: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        repo: env::current_dir().map_err(|e| format!("current directory: {e}"))?,
        base: "main".to_string(),
        list: false,
        preview: None,
        land: None,
        push: false,
        expect: None,
        expect_base: None,
        delete: None,
        delete_landed: false,
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
        + u8::from(args.delete.is_some());
    if modes > 1 {
        return Err(io::Error::other(
            "--list, --preview, --land and --delete are mutually exclusive",
        ));
    }
    if args.land.is_none() && args.push {
        return Err(io::Error::other("--push is only meaningful with --land"));
    }
    if args.land.is_none() && (args.expect.is_some() || args.expect_base.is_some()) {
        return Err(io::Error::other("--expect/--expect-base are only meaningful with --land"));
    }
    if args.land.is_none() && args.delete.is_none() && args.yes {
        return Err(io::Error::other("--yes is only meaningful with --land or --delete"));
    }
    if args.delete_landed && !(args.land.is_some() && args.push) {
        return Err(io::Error::other("--delete-landed needs --land <branch> --push"));
    }
    if args.list {
        return list_branches(&git, &args.base).map(|()| ExitCode::SUCCESS);
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
    let now = now_unix();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for b in &branches {
        // Refnames and subjects are untrusted; --list prints straight to a
        // terminal, so neutralise them here as the TUI's frame does.
        writeln!(
            out,
            "{:>5}  {:>7}  {}\t{}",
            b.age(now),
            b.counts_label(),
            scrub(&b.refname),
            scrub(&b.subject)
        )?;
    }
    Ok(())
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
    landed_oid: Option<&str>,
) -> io::Result<ExitCode> {
    if !yes {
        return Err(io::Error::other(
            "--delete removes a published ref; re-run with --yes to confirm",
        ));
    }
    let remotes = git.remote_names()?;
    let (named, short) = git::split_remote(branch, &remotes);
    // Post-land cleanup sweeps every remote holding the commit we just took (the
    // oid filter is what makes that safe); an ad-hoc `--delete origin/x` touches
    // only the remote it names.
    let only = if landed_oid.is_some() { None } else { named };
    let (targets, diverged) = land::delete_plan(git, short, only, landed_oid)?;
    for d in &diverged {
        println!("{}/{} is not the landed commit — left alone", scrub(&d.remote), scrub(short));
    }
    let deleted = land::delete_branch(git, base, short, &targets)?;
    for line in &deleted.log {
        println!("{}", scrub(&line.text));
    }
    if !deleted.all_ok {
        return Ok(ExitCode::FAILURE);
    }
    if deleted.count == 0 {
        // After a landing, nothing to delete means the oid filter did its job.
        // Only a hand-typed --delete that matched nothing is a failure.
        if landed_oid.is_none() {
            eprintln!("td-review: no pushable remote carries '{}'", scrub(short));
            return Ok(ExitCode::FAILURE);
        }
        println!("no pushable remote carries the landed {}", scrub(short));
    }
    Ok(ExitCode::SUCCESS)
}

fn land_headless(git: &Git, base: &str, branch: &str, args: &Args) -> io::Result<ExitCode> {
    if !args.yes {
        return Err(io::Error::other(
            "--land rewrites the base branch; re-run with --yes to confirm",
        ));
    }
    // Captured before the squash: the delete filter compares remotes against the
    // branch tip that was landed, not the new commit on the base.
    let landed_oid = git.branch_oid(branch).ok();
    let landing = land::squash_land(
        git,
        base,
        branch,
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
        let remotes = git.remotes()?;
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
        if pushed.count == 0 {
            eprintln!("td-review: no remote was eligible — the commit was not published");
            return Ok(ExitCode::FAILURE);
        }
        // Only ever after a verified publish: the branch is the sole copy of
        // that work until the base carrying it reaches the remotes.
        if args.delete_landed {
            // A failed cleanup is not a failed landing: the work is published.
            // Report it distinctly rather than as "nothing landed".
            if delete_headless(git, base, branch, true, landed_oid.as_deref())?
                != ExitCode::SUCCESS
            {
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
