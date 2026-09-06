//! td news (tn): a terminal RSS and Atom reader.
//!
//! The crate is `std` and nothing else. Seven modules — `civil`, `html`,
//! `json`, `kv`, `term_sys`, `toml`, `xml` — are td's shared std modules,
//! copied in whole from one master each and never edited here, so the
//! import into td can diff them byte for byte against tmc's copies. What
//! tn does not call therefore stays, allowed on its `mod` line rather
//! than trimmed: a binary crate exports nothing, so `dead_code` fires
//! here and not in the module's own crate.
//!
//! `unsafe` is denied for the whole crate. `term_sys` carries the single
//! scoped allowance, over one `syscall` instruction, and its own tests
//! assert that this file is the only other place the lint is named.

#![deny(unsafe_code)]

mod backend;
mod cache;
// `wrong_self_convention` is clippy declining to rename an exported method;
// nothing is exported here, so it fires as `dead_code` does.
#[allow(dead_code, clippy::wrong_self_convention)]
mod civil;
mod cli;
mod config;
mod feed;
#[allow(dead_code)]
mod html;
#[allow(dead_code)]
mod json;
mod keybindings;
#[allow(dead_code)]
mod kv;
mod log;
// Shared with tmc, which reads response headers where tn does not.
#[allow(dead_code)]
mod td_fetch;
// The shared module carries a terminal surface wider than tn's one raw mode.
#[allow(dead_code)]
mod term_sys;
/// `tempfile`'s replacement, test-only and shared with the integration
/// tests through a `#[path]` include.
#[cfg(test)]
mod testing;
#[allow(dead_code)]
mod toml;
mod tui;
#[allow(dead_code)]
mod xml;

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match CliOptions::parse(&args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("{}", e);
            eprintln!("Use --help for usage.");
            std::process::exit(2);
        }
    };

    if opts.help {
        eprintln!("Usage: tn [OPTIONS]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --help          Show this help message");
        eprintln!("  --help-config   Show configuration file documentation");
        eprintln!("  --help-cli      Show CLI mode command documentation");
        eprintln!("  --cli           Run newline-delimited JSON CLI mode");
        eprintln!("  --log           View debug log file");
        eprintln!("  --news-log      View news log file");
        eprintln!("  --clear-cache   Delete all cache files");
        eprintln!("  --offline       Browse cached articles only");
        eprintln!("  --fetch-and-quit  Fetch all configured feeds into cache, then exit");
        eprintln!("  --config=PATH   Use a custom config file");
        eprintln!("  --cache=PATH    Use a custom cache DB path");
        return;
    }

    if opts.help_config {
        print_help_config();
        return;
    }

    if opts.help_cli {
        eprintln!("{}", cli::help_text());
        return;
    }

    if opts.log {
        let path = log::debug_log_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                print!("{}", contents);
            }
            Err(e) => {
                eprintln!("No log at {} ({})", path.display(), e);
            }
        }
        return;
    }

    if opts.news_log {
        let path = log::news_log_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                print!("{}", contents);
            }
            Err(e) => {
                eprintln!("No log at {} ({})", path.display(), e);
            }
        }
        return;
    }

    if let Err(e) = log::init() {
        eprintln!("Failed to initialize logging: {}", e);
    } else {
        log::info("tn starting");
        log::news("tn session started");
    }

    if opts.clear_cache {
        if let Some(path) = opts.cache_path.clone() {
            cache::Cache::clear_at(path);
        } else {
            cache::Cache::clear();
        }
        log::info("cache cleared");
        eprintln!("Cache cleared.");
        return;
    }

    let cache = if let Some(path) = opts.cache_path.clone() {
        match cache::Cache::open_at(path) {
            Ok(c) => c,
            Err(e) => {
                log::error(format!("failed to open cache: {}", e));
                eprintln!("Failed to open cache: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        match cache::Cache::open() {
            Ok(c) => c,
            Err(e) => {
                log::error(format!("failed to open cache: {}", e));
                eprintln!("Failed to open cache: {}", e);
                std::process::exit(1);
            }
        }
    };

    if opts.cli {
        if let Err(e) = cli::run(&cache) {
            log::error(format!("cli error: {}", e));
            eprintln!("CLI error: {}", e);
            std::process::exit(1);
        }
        log::info("tn cli shutdown");
        log::news("tn cli session ended");
        return;
    }

    let config = match config::Config::load(opts.config_path.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            log::error(format!("config load failed: {}", e));
            eprintln!("Error loading config: {}", e);
            std::process::exit(1);
        }
    };

    if opts.fetch_and_quit {
        let (cmd_tx, resp_rx) = backend::spawn(&config, cache.clone());
        if let Err(e) = cmd_tx.send(backend::BackendCommand::FetchAllFeeds) {
            log::error(format!("failed to request fetch-all: {}", e));
            eprintln!("Failed to start fetch: {}", e);
            std::process::exit(1);
        }

        let mut had_errors = false;
        loop {
            match resp_rx.recv() {
                Ok(backend::BackendResponse::RefreshCompleted { reports }) => {
                    for report in reports {
                        if let Some(error) = report.error {
                            had_errors = true;
                            eprintln!("Fetch error {}: {}", report.feed_url, error);
                        } else {
                            eprintln!(
                                "Fetched {} ({}) new={} total={} unread={}",
                                report.feed_name,
                                report.feed_url,
                                report.new_articles,
                                report.total,
                                report.unread
                            );
                        }
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    log::error(format!("fetch-and-quit response channel error: {}", e));
                    eprintln!("Fetch failed: {}", e);
                    let _ = cmd_tx.send(backend::BackendCommand::Shutdown);
                    std::process::exit(1);
                }
            }
        }

        let _ = cmd_tx.send(backend::BackendCommand::Shutdown);
        if had_errors {
            std::process::exit(1);
        }
        return;
    }

    let offline = opts.offline;
    let (cmd_tx, resp_rx) = backend::spawn(&config, cache.clone());
    if let Err(e) = tui::run(&config, &cache, &cmd_tx, &resp_rx, offline) {
        log::error(format!("tui error: {}", e));
        eprintln!("TUI error: {}", e);
    }

    let _ = cmd_tx.send(backend::BackendCommand::Shutdown);
    log::info("tn shutdown");
    log::news("tn session ended");
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

fn print_help_config() {
    let xdg = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.config", home)
    });
    eprintln!("Configuration file: {}/tn/config.toml", xdg);
    eprintln!();
    eprintln!("[ui]");
    eprintln!("  page_size = 100              # max articles per page (default: 100)");
    eprintln!(
        "  scrolloff = 0                # min context lines above/below selection (default: 0)"
    );
    eprintln!("  mouse = true                 # enable mouse support (default: true)");
    eprintln!("  sync_interval_secs = 300     # auto-refresh interval in seconds (default: 300; at least 30; 0 turns it off)");
    eprintln!(
        "  browser = \"command {{url}}\"     # browser command; {{url}} is replaced with the URL"
    );
    eprintln!("                               # if no {{url}}, URL is appended as argument");
    eprintln!(
        "                               # executed via sh -c; falls back to $BROWSER, xdg-open"
    );
    eprintln!();
    eprintln!("[theme]");
    eprintln!("  bg = \"#002b36\"               # background color (#RRGGBB)");
    eprintln!("  fg = \"#839496\"               # foreground color");
    eprintln!("  bold_fg = \"#93a1a1\"           # bold/unread text color");
    eprintln!("  selection_bg = \"#073642\"      # selected row background");
    eprintln!("  selection_fg = \"#eee8d5\"      # selected row foreground");
    eprintln!("  status_bg = \"#586e75\"         # status bar background");
    eprintln!("  status_fg = \"#eee8d5\"         # status bar foreground");
    eprintln!("  header_fg = \"#268bd2\"         # header text color");
    eprintln!();
    eprintln!("[[feed]]");
    eprintln!("  name = \"Feed Name\"            # display name for the feed");
    eprintln!("  url = \"https://example/rss\"   # RSS or Atom feed URL");
}
