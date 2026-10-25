//! The host's Flathub feed, served to one boot that starts the system's
//! applications (APPLICATIONS.md §W.9). Their packages carry no application
//! bytes, so such a guest's first launch fetches them: from this feed, which
//! `td-recipe-eval warm --flathub` filled with `td-feed export flathub`, and
//! from Flathub only for a ref the feed lacks.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// The token that names a boot's feed to the deployment init.
const FEED_TOKEN: &str = td_engine::flathub_app::BOOT_FEED_TOKEN;

/// Whether a boot starts the system's applications, and so needs a feed:
/// every boot whose line carries the autotest token, unless its source
/// starts none — a live medium's session opens the installer alone, and a
/// headless profile ships no applications.
pub(super) fn serves(applications: bool, extra_append: &str) -> bool {
    applications
        && extra_append
            .split_ascii_whitespace()
            .any(|token| token == td_recipe::shared::AUTOTEST_CMDLINE_TOKEN)
}

/// `extra_append` with the feed's token after it.
pub(super) fn append(extra_append: &str, endpoint: &str) -> String {
    if extra_append.is_empty() {
        format!("{FEED_TOKEN}{endpoint}")
    } else {
        format!("{extra_append} {FEED_TOKEN}{endpoint}")
    }
}

/// A private `td-feed serve` of the host's feed store for one boot. The
/// guest reaches it as `http://10.0.2.2:PORT` over QEMU's user network, and
/// it dies with the guard, which the boot drops after reaping QEMU.
pub(super) struct FeedServer {
    child: Child,
    endpoint: String,
}

impl FeedServer {
    pub(super) fn start() -> Result<Self, String> {
        let root = std::env::current_dir().map_err(|error| format!("current dir: {error}"))?;
        let builder = crate::check_runner::find_td_builder_self(&root)?;
        let td_net = crate::warm::resolve_td_net(&builder)?;
        let store = feed_store();
        let child = Command::new(&td_net)
            .arg("feed")
            .arg("serve")
            .arg(&store)
            .arg("127.0.0.1:0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("spawn {} feed serve: {error}", td_net.display()))?;
        let mut server = Self {
            child,
            endpoint: String::new(),
        };
        let stdout = server
            .child
            .stdout
            .take()
            .ok_or("td-feed serve has no stdout")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("read td-feed serve's address: {error}"))?;
        let port = announced_port(&line)
            .ok_or_else(|| format!("td-feed serve announced no loopback address: {line:?}"))?;
        // Later output is drained, so a full pipe never stalls the server.
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut reader, &mut std::io::sink());
        });
        server.endpoint = format!("http://10.0.2.2:{port}");
        println!(
            "   [qemu] serving the host feed {} to the guest as {}",
            store.display(),
            server.endpoint
        );
        Ok(server)
    }

    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for FeedServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where `td-feed export flathub` publishes: `$TD_FEED_DIR/store`, else
/// `~/.td/feed/store`, as td-feed's `resolve_feed_dir` resolves it.
fn feed_store() -> PathBuf {
    let dir = std::env::var("TD_FEED_DIR")
        .ok()
        .map(|dir| dir.trim().to_string())
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".td/feed")
        });
    dir.join("store")
}

/// The port of `td-feed: serving STORE on http://127.0.0.1:PORT/`.
fn announced_port(line: &str) -> Option<u16> {
    let address = line.trim_end().rsplit_once(" on http://")?.1;
    let port = address.strip_suffix('/')?.strip_prefix("127.0.0.1:")?;
    port.parse().ok().filter(|port| *port != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_boot_that_starts_the_applications_is_told_its_feed() {
        let autotest = td_recipe::shared::AUTOTEST_CMDLINE_TOKEN;
        assert!(serves(true, autotest));
        assert!(serves(true, &format!("td.wait=1 {autotest}")));
        assert!(
            !serves(false, autotest),
            "a live session or headless profile starts no applications"
        );
        assert!(!serves(true, ""));
        assert!(!serves(true, &format!("x{autotest}")));
        assert_eq!(append("", "http://10.0.2.2:1"), "td.feed=http://10.0.2.2:1");
        assert_eq!(
            append(autotest, "http://10.0.2.2:1"),
            format!("{autotest} td.feed=http://10.0.2.2:1")
        );
    }

    #[test]
    fn only_a_loopback_announcement_names_the_port() {
        assert_eq!(
            announced_port("td-feed: serving /h/.td/feed/store on http://127.0.0.1:4321/\n"),
            Some(4321)
        );
        assert_eq!(
            announced_port("td-feed: serving /s on http://0.0.0.0:4321/\n"),
            None
        );
        assert_eq!(
            announced_port("td-feed: serving /s on http://127.0.0.1:0/"),
            None
        );
        assert_eq!(announced_port("bind 127.0.0.1:0: refused"), None);
    }
}
