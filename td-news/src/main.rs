//! td-news: an RSS and Atom reader in a td-ui window.
//!
//! The crate is `std` and td's own crates: td-ui, the shared toolkit
//! whose widget window it draws in, and the shared library crates td-civil,
//! td-fetch-client, td-html, td-json, td-kv and td-toml. The `xml` module
//! is td-news's own reader, carried whole from its master; what td-news
//! does not call of it stays, allowed on its `mod` line rather than
//! trimmed, since a binary crate exports nothing and `dead_code` fires here.
//!
//! `unsafe` is forbidden for the whole crate: the terminal surface the
//! reader once carried (UNSAFE.md §18) went with the terminal, and the
//! Wayland transport is td-ui's (§19).

#![forbid(unsafe_code)]

mod backend;
mod cache;
mod cli;
mod config;
mod feed;
mod keybindings;
mod log;
#[cfg(test)]
mod testing;
mod ui;
#[allow(dead_code)]
mod xml;

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;

fn output(stream: impl AsFd) -> io::Result<std::fs::File> {
    Ok(std::fs::File::from(stream.as_fd().try_clone_to_owned()?))
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "td-news: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match CliOptions::parse(&args) {
        Ok(opts) => opts,
        Err(e) => {
            let _ = writeln!(io::stderr().lock(), "{}", e);
            let _ = writeln!(io::stderr().lock(), "Use --help for usage.");
            std::process::exit(2);
        }
    };

    if opts.help || opts.help_config || opts.help_cli {
        let mut out = output(io::stderr())?;
        if opts.help {
            return writeln!(out, "{USAGE}");
        }
        if opts.help_config {
            return print_help_config(&mut out);
        }
        return writeln!(out, "{}", cli::help_text());
    }

    if opts.log || opts.news_log {
        let path = if opts.log {
            log::debug_log_path()
        } else {
            log::news_log_path()
        };
        let Some(path) = path else {
            return writeln!(
                output(io::stderr())?,
                "No log: {}",
                td_ui::xdg::Base::Cache.missing()
            );
        };
        return match std::fs::read_to_string(&path) {
            Ok(contents) => output(io::stdout())?.write_all(contents.as_bytes()),
            Err(e) => writeln!(
                output(io::stderr())?,
                "No log at {} ({})",
                path.display(),
                e
            ),
        };
    }

    if let Err(e) = log::init() {
        let _ = writeln!(io::stderr().lock(), "Failed to initialize logging: {}", e);
    } else {
        log::info("td-news starting");
        log::news("td-news session started");
    }

    if opts.clear_cache {
        if let Some(path) = opts.cache_path.clone() {
            cache::Cache::clear_at(path);
        } else if let Err(e) = cache::Cache::clear() {
            let _ = writeln!(io::stderr().lock(), "td-news: {e}");
            std::process::exit(1);
        }
        log::info("cache cleared");
        return writeln!(output(io::stderr())?, "Cache cleared.");
    }

    let cache = if let Some(path) = opts.cache_path.clone() {
        match cache::Cache::open_at(path) {
            Ok(c) => c,
            Err(e) => {
                log::error(format!("failed to open cache: {}", e));
                let _ = writeln!(io::stderr().lock(), "Failed to open cache: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match cache::Cache::open() {
            Ok(c) => c,
            Err(e) => {
                log::error(format!("failed to open cache: {}", e));
                let _ = writeln!(io::stderr().lock(), "Failed to open cache: {}", e);
                std::process::exit(1);
            }
        }
    };

    if opts.cli {
        if let Err(e) = cli::run(&cache, output(io::stdout())?) {
            log::error(format!("cli error: {}", e));
            let _ = writeln!(io::stderr().lock(), "CLI error: {}", e);
            std::process::exit(1);
        }
        log::info("td-news cli shutdown");
        log::news("td-news cli session ended");
        return Ok(());
    }

    let config = match config::Config::load(opts.config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            log::error(format!("config load failed: {}", e));
            let _ = writeln!(io::stderr().lock(), "Error loading config: {}", e);
            std::process::exit(1);
        }
    };

    if opts.fetch_and_quit {
        let mut report_output = output(io::stderr())?;
        let (cmd_tx, resp_rx) = backend::spawn(&config, cache.clone());
        if let Err(e) = cmd_tx.send(backend::BackendCommand::FetchAllFeeds) {
            log::error(format!("failed to request fetch-all: {}", e));
            let _ = writeln!(io::stderr().lock(), "Failed to start fetch: {}", e);
            std::process::exit(1);
        }

        let mut had_errors = false;
        loop {
            match resp_rx.recv() {
                Ok(backend::BackendResponse::RefreshCompleted {
                    manual: true,
                    reports,
                }) => {
                    let reports = reports.map_err(io::Error::other)?;
                    for report in reports {
                        if let Some(error) = report.error {
                            had_errors = true;
                            let _ = writeln!(
                                io::stderr().lock(),
                                "Fetch error {}: {}",
                                report.feed_url,
                                error
                            );
                        } else {
                            had_errors |= writeln!(
                                report_output,
                                "Fetched {} ({}) new={} total={} unread={}",
                                report.feed_name,
                                report.feed_url,
                                report.new_articles,
                                report.total,
                                report.unread
                            )
                            .is_err();
                        }
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    log::error(format!("fetch-and-quit response channel error: {}", e));
                    let _ = writeln!(io::stderr().lock(), "Fetch failed: {}", e);
                    let _ = cmd_tx.send(backend::BackendCommand::Shutdown);
                    std::process::exit(1);
                }
            }
        }

        let _ = cmd_tx.send(backend::BackendCommand::Shutdown);
        if had_errors {
            std::process::exit(1);
        }
        return Ok(());
    }

    let offline = opts.offline;
    let (cmd_tx, resp_rx) = backend::spawn(&config, cache.clone());
    let window_failed = match ui::run(&config, &cache, &cmd_tx, &resp_rx, offline) {
        Ok(()) => false,
        Err(e) => {
            log::error(format!("window error: {}", e));
            let _ = writeln!(io::stderr().lock(), "Window error: {}", e);
            true
        }
    };

    let _ = cmd_tx.send(backend::BackendCommand::Shutdown);
    log::info("td-news shutdown");
    log::news("td-news session ended");
    // After the shutdown and the log lines, so the record is complete;
    // a launcher still learns that the session did not end on purpose.
    if window_failed {
        std::process::exit(1);
    }
    Ok(())
}

#[derive(Default)]
struct CliOptions {
    help: bool,
    help_config: bool,
    help_cli: bool,
    cli: bool,
    log: bool,
    news_log: bool,
    clear_cache: bool,
    offline: bool,
    fetch_and_quit: bool,
    config_path: Option<String>,
    cache_path: Option<PathBuf>,
}

impl CliOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut opts = Self::default();
        let mut i = 0usize;
        while let Some(arg) = args.get(i) {
            match arg.as_str() {
                "--help" | "-h" => opts.help = true,
                "--help-config" => opts.help_config = true,
                "--help-cli" => opts.help_cli = true,
                "--cli" => {
                    opts.cli = true;
                    opts.offline = true;
                }
                "--log" => opts.log = true,
                "--news-log" => opts.news_log = true,
                "--clear-cache" => opts.clear_cache = true,
                "--offline" => opts.offline = true,
                "--fetch-and-quit" => opts.fetch_and_quit = true,
                "--config" => {
                    i += 1;
                    let value = args
                        .get(i)
                        .ok_or_else(|| "--config requires a path".to_string())?;
                    opts.config_path = Some(value.clone());
                }
                "--cache" => {
                    i += 1;
                    let value = args
                        .get(i)
                        .ok_or_else(|| "--cache requires a path".to_string())?;
                    opts.cache_path = Some(PathBuf::from(value));
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--config=") {
                        opts.config_path = Some(value.to_string());
                    } else if let Some(value) = arg.strip_prefix("--cache=") {
                        opts.cache_path = Some(PathBuf::from(value));
                    } else {
                        return Err(format!("unknown argument: {}", arg));
                    }
                }
            }
            i += 1;
        }
        Ok(opts)
    }
}

fn print_help_config(out: &mut impl Write) -> io::Result<()> {
    match config::default_path() {
        Ok(path) => writeln!(out, "Configuration file: {}", path.display())?,
        Err(why) => writeln!(out, "Configuration file: none ({why})")?,
    }
    writeln!(out, "{CONFIG_HELP}")
}

const USAGE: &str = r#"Usage: td-news [OPTIONS]

Options:
  --help          Show this help message
  --help-config   Show configuration file documentation
  --help-cli      Show CLI mode command documentation
  --cli           Run newline-delimited JSON CLI mode
  --log           View debug log file
  --news-log      View news log file
  --clear-cache   Delete all cache files
  --offline       Browse cached articles only
  --fetch-and-quit  Fetch all configured feeds into cache, then exit
  --config=PATH   Use a custom config file
  --cache=PATH    Use a custom cache DB path"#;

const CONFIG_HELP: &str = r#"
[ui]
  mouse = true                 # enable mouse support (default: true)
  sync_interval_secs = 300     # auto-refresh interval in seconds (default: 300; at least 30; 0 turns it off)
  browser = "command {url}"     # browser command; {url} is replaced with the URL
                               # if no {url}, URL is appended as argument
                               # run directly, no shell; falls back to $BROWSER, xdg-open
  scrolloff = 0                # rows kept shown past the selection on each side (default: 0)
  # page_size and a [theme] table are the terminal reader's:
  # read as unknown keys and ignored

[[feed]]
  name = "Feed Name"            # display name for the feed
  url = "https://example/rss"   # RSS or Atom feed URL"#;
