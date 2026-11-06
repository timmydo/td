#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! git-remote-https end to end: the host's git, the built td-net as its only
//! HTTP helper, and td-test-https's Git origin over real TLS on loopback.
//! `GIT_EXEC_PATH` names a directory holding only this helper, so git has no
//! curl helper to find; a test that reached one would fail its refusal checks.

#[path = "../../td-fs/src/temp_dir.rs"]
mod temp_dir;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TD_NET: &str = env!("CARGO_BIN_EXE_td-net");

struct Site {
    dir: temp_dir::TempDir,
    exec_path: PathBuf,
}

impl Site {
    fn new(tag: &str) -> Self {
        let dir = temp_dir::TempDir::new(&format!("td-git-remote-{tag}")).unwrap();
        let exec_path = dir.path().join("exec");
        std::fs::create_dir(&exec_path).unwrap();
        for name in ["git-remote-http", "git-remote-https", "td-test-https"] {
            symlink(TD_NET, exec_path.join(name)).unwrap();
        }
        std::fs::create_dir(dir.path().join("mint")).unwrap();
        let minted = Command::new(exec_path.join("td-test-https"))
            .arg("mint")
            .arg(dir.path().join("mint"))
            .status()
            .unwrap();
        assert!(minted.success());
        std::fs::create_dir(dir.path().join("root")).unwrap();
        std::fs::create_dir(dir.path().join("home")).unwrap();
        Self { dir, exec_path }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn ca(&self) -> PathBuf {
        self.path("mint/ca.pem")
    }

    /// git with no configuration of its own and no way to prompt.
    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_EXEC_PATH", &self.exec_path)
            .env("HOME", self.path("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        command
    }

    /// git that trusts this site's authority.
    fn trusting(&self) -> Command {
        let mut command = self.command();
        command.env("GIT_SSL_CAINFO", self.ca());
        command
    }

    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let output = self
            .trusting()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {}", said(&output));
        String::from_utf8(output.stdout).unwrap()
    }

    /// A bare repository `NAME.git` at the root with one commit on `main`.
    fn repository(&self, name: &str) -> PathBuf {
        let work = self.path(&format!("seed-{name}"));
        std::fs::create_dir(&work).unwrap();
        self.git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("file"), "first\n").unwrap();
        self.commit(&work, "first");
        let bare = self.path("root").join(format!("{name}.git"));
        let output = self
            .command()
            .args(["clone", "-q", "--bare"])
            .arg(&work)
            .arg(&bare)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", said(&output));
        bare
    }

    fn commit(&self, work: &Path, message: &str) {
        self.git(work, &["add", "."]);
        self.git(
            work,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "-m",
                message,
            ],
        );
    }

    fn serve(&self, extra: &[&str]) -> Origin {
        self.serve_on("127.0.0.1:0", extra)
    }

    fn serve_on(&self, listen: &str, extra: &[&str]) -> Origin {
        let mut child = Command::new(self.exec_path.join("td-test-https"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.path("home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("serve-git")
            .args(["--listen", listen, "--cert"])
            .arg(self.path("mint/server.pem"))
            .arg("--key")
            .arg(self.path("mint/server.key"))
            .arg("--root")
            .arg(self.path("root"))
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let bound = line.trim().strip_prefix("listening ").unwrap().to_string();
        let port = bound.rsplit(':').next().unwrap().to_string();
        Origin { child, port }
    }
}

struct Origin {
    child: Child,
    port: String,
}

impl Origin {
    fn url(&self, path: &str) -> String {
        format!("https://localhost:{}/{path}", self.port)
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn expect_failure(output: &Output, text: &str) {
    let all = said(output);
    assert!(!output.status.success(), "unexpected success: {all}");
    assert!(all.contains(text), "wanted {text:?} in: {all}");
}

#[test]
fn a_clone_fetch_and_ls_remote_run_over_protocol_v2() {
    let site = Site::new("clone");
    let bare = site.repository("r");
    let log = site.path("origin.log");
    let origin = site.serve(&["--log", log.to_str().unwrap()]);
    let url = origin.url("r.git");

    let listed = site.git(site.dir.path(), &["ls-remote", &url]);
    assert!(listed.contains("refs/heads/main"), "{listed}");
    assert!(listed.contains("HEAD"), "{listed}");

    let clone = site.path("clone");
    site.git(
        site.dir.path(),
        &["clone", "-q", &url, clone.to_str().unwrap()],
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("file")).unwrap(),
        "first\n"
    );

    // The same over a response in chunks.
    let chunked = site.path("chunked");
    site.git(
        site.dir.path(),
        &[
            "clone",
            "-q",
            &origin.url("chunked/r.git"),
            chunked.to_str().unwrap(),
        ],
    );
    assert!(chunked.join("file").exists());

    // A bundle URI cannot be fetched; the clone goes on without it, as Git's
    // own design has it for a bundle that fails.
    let bundled = site.path("bundled");
    let output = site
        .trusting()
        .args([
            "clone",
            "-q",
            &format!("--bundle-uri={}", origin.url("no-such.bundle")),
            &url,
            bundled.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    assert!(bundled.join("file").exists());
    assert!(!said(&output).contains("usage:"), "{}", said(&output));

    // A later commit arrives through an incremental fetch.
    let work = site.path("second");
    site.git(
        site.dir.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    std::fs::write(work.join("file"), "second\n").unwrap();
    site.commit(&work, "second");
    site.git(&work, &["push", "-q", "origin", "main"]);
    site.git(&clone, &["pull", "-q", "--ff-only"]);
    assert_eq!(
        std::fs::read_to_string(clone.join("file")).unwrap(),
        "second\n"
    );

    // A shallow clone: an option Git itself carries over the connection.
    let shallow = site.path("shallow");
    site.git(
        site.dir.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &url,
            shallow.to_str().unwrap(),
        ],
    );
    let count = site.git(&shallow, &["rev-list", "--count", "HEAD"]);
    assert_eq!(count.trim(), "1");

    let log = std::fs::read_to_string(&log).unwrap();
    assert!(
        log.contains("GET /r.git/info/refs?service=git-upload-pack auth=false chunked=false protocol=version=2 agent=git/"),
        "{log}"
    );
    assert!(log.contains("POST /r.git/git-upload-pack?"), "{log}");
}

#[test]
fn a_small_and_a_large_push_land() {
    let site = Site::new("push");
    let bare = site.repository("r");
    let log = site.path("origin.log");
    let origin = site.serve(&["--log", log.to_str().unwrap()]);
    let url = origin.url("r.git");
    let clone = site.path("clone");
    site.git(
        site.dir.path(),
        &["clone", "-q", &url, clone.to_str().unwrap()],
    );

    std::fs::write(clone.join("file"), "pushed\n").unwrap();
    site.commit(&clone, "small");
    site.git(&clone, &["push", "-q", "origin", "main"]);
    let head = site.git(&clone, &["rev-parse", "HEAD"]);
    assert_eq!(site.git(&bare, &["rev-parse", "main"]), head);
    assert!(std::fs::read_to_string(&log)
        .unwrap()
        .contains("chunked=false"));

    // Past the post buffer, and incompressible, so it must go chunked.
    let mut blob = vec![0_u8; 3 * 1024 * 1024];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut blob)
        .unwrap();
    std::fs::write(clone.join("large"), &blob).unwrap();
    site.commit(&clone, "large");
    site.git(&clone, &["push", "-q", "origin", "main"]);
    let head = site.git(&clone, &["rev-parse", "HEAD"]);
    assert_eq!(site.git(&bare, &["rev-parse", "main"]), head);
    let shown = site.git(&bare, &["cat-file", "-s", "main:large"]);
    assert_eq!(shown.trim(), blob.len().to_string());
    let log_path = log;
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("POST /r.git/git-receive-pack? auth=false chunked=true"),
        "{log}"
    );
    // Asked to buffer more, the same size of request goes with a length.
    blob.truncate(2 * 1024 * 1024);
    std::fs::write(clone.join("large-2"), &blob).unwrap();
    site.commit(&clone, "buffered");
    site.git(
        &clone,
        &["-c", "http.postBuffer=16m", "push", "-q", "origin", "main"],
    );
    let all = std::fs::read_to_string(&log_path).unwrap();
    let buffered = all.get(log.len()..).unwrap();
    assert!(buffered.contains("git-receive-pack? "), "{buffered}");
    assert!(
        !buffered.contains("chunked=true"),
        "http.postBuffer did not hold the request: {buffered}"
    );
    // The empty request that finds out about credentials before streaming.
    assert!(
        log.lines().any(|line| {
            line.starts_with("POST /r.git/git-receive-pack? ")
                && line.contains("chunked=false")
                && line.ends_with(" bytes=4")
        }),
        "{log}"
    );

    // A rejected update is Git's to report, through the helper's status.
    site.git(&bare, &["config", "receive.denyNonFastForwards", "true"]);
    site.git(&clone, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(clone.join("file"), "diverged\n").unwrap();
    site.commit(&clone, "diverged");
    let output = site
        .trusting()
        .arg("-C")
        .arg(&clone)
        .args(["push", "--force", "origin", "main"])
        .output()
        .unwrap();
    expect_failure(&output, "rejected");
    // Git said why; the helper adds nothing of its own.
    assert!(!said(&output).contains("send-pack"), "{}", said(&output));

    // A dry run and a new branch's full report.
    site.git(
        &clone,
        &["push", "-q", "--dry-run", "origin", "main:refs/heads/dry"],
    );
    assert!(site.git(&bare, &["branch", "--list", "dry"]).is_empty());
    let pushed = site
        .trusting()
        .arg("-C")
        .arg(&clone)
        .args(["push", "origin", "main:refs/heads/topic"])
        .output()
        .unwrap();
    assert!(pushed.status.success(), "{}", said(&pushed));
    assert!(said(&pushed).contains("new branch"), "{}", said(&pushed));
    assert_eq!(
        site.git(&bare, &["branch", "--list", "topic"]).trim(),
        "topic"
    );
    // And deleting it, which sends no pack.
    site.git(&clone, &["push", "-q", "origin", ":refs/heads/topic"]);
    assert!(site.git(&bare, &["branch", "--list", "topic"]).is_empty());
}

#[test]
fn trust_comes_from_the_bundle_and_the_name_must_match() {
    let site = Site::new("trust");
    site.repository("r");
    let origin = site.serve_on("0.0.0.0:0", &[]);
    let url = origin.url("r.git");

    // The site's authority is in no default store, and td-net's compiled-in
    // roots do not vouch for it. Point the default at a bundle without it.
    let other_mint = site.path("other");
    std::fs::create_dir(&other_mint).unwrap();
    assert!(Command::new(site.exec_path.join("td-test-https"))
        .arg("mint")
        .arg(&other_mint)
        .status()
        .unwrap()
        .success());
    let output = site
        .command()
        .env("GIT_SSL_CAINFO", other_mint.join("ca.pem"))
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "invalid peer certificate");

    // A file with no certificate in it fails closed.
    let empty = site.path("empty.pem");
    std::fs::write(&empty, "nothing here\n").unwrap();
    let output = site
        .command()
        .env("GIT_SSL_CAINFO", &empty)
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "holds no usable certificate");
    let output = site
        .command()
        .env("GIT_SSL_CAINFO", site.path("absent.pem"))
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "CA bundle");

    // The right authority, the wrong name: the certificate says 127.0.0.1
    // and localhost, not 127.0.0.2.
    let wrong = format!("https://127.0.0.2:{}/r.git", origin.port);
    let output = site
        .trusting()
        .args(["ls-remote", &wrong])
        .output()
        .unwrap();
    expect_failure(&output, "invalid peer certificate");
    let output = site.trusting().args(["ls-remote", &url]).output().unwrap();
    assert!(output.status.success(), "{}", said(&output));
}

#[test]
fn what_the_transport_does_not_do_is_refused_by_name() {
    let site = Site::new("refused");
    site.repository("r");
    let origin = site.serve(&[]);
    let url = origin.url("r.git");

    let output = site
        .trusting()
        .env("GIT_SSL_NO_VERIFY", "1")
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "GIT_SSL_NO_VERIFY is set");
    for (setting, name) in [
        ("http.sslVerify=false", "sslVerify"),
        ("http.sslCert=/x", "http.sslcert"),
        ("http.extraHeader=X-A: b", "http.extraheader"),
        ("http.cookieFile=/x", "http.cookiefile"),
    ] {
        let output = site
            .trusting()
            .args(["-c", setting, "ls-remote", &url])
            .output()
            .unwrap();
        expect_failure(&output, name);
    }
    // A URL-scoped setting for another host is not this request's business.
    let output = site
        .trusting()
        .args([
            "-c",
            "http.https://other.test.sslCert=/x",
            "ls-remote",
            &url,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    // A proxy it cannot use must not become going direct.
    let output = site
        .trusting()
        .args(["-c", "http.proxy=http://[bad", "ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "not a valid proxy URL");
}

#[test]
fn old_protocols_and_dumb_servers_say_so() {
    let site = Site::new("old");
    site.repository("r");
    let origin = site.serve(&[]);
    let output = site
        .trusting()
        .args([
            "-c",
            "protocol.version=0",
            "ls-remote",
            &origin.url("r.git"),
        ])
        .output()
        .unwrap();
    expect_failure(&output, "needs Git protocol v2");
    let output = site
        .trusting()
        .args(["ls-remote", &origin.url("v0/r.git")])
        .output()
        .unwrap();
    expect_failure(&output, "needs Git protocol v2");
    let output = site
        .trusting()
        .args(["ls-remote", &origin.url("dumb/r.git")])
        .output()
        .unwrap();
    expect_failure(&output, "does not speak smart HTTP");
    // A push does not need v2: receive-pack has never spoken it.
    let clone = site.path("clone");
    site.git(
        site.dir.path(),
        &["clone", "-q", &origin.url("r.git"), clone.to_str().unwrap()],
    );
    site.git(
        &clone,
        &[
            "-c",
            "protocol.version=0",
            "push",
            "-q",
            "origin",
            "main:refs/heads/v0",
        ],
    );
}

#[test]
fn server_answers_read_as_git_writes_them() {
    let site = Site::new("answers");
    site.repository("r");
    let origin = site.serve(&[]);
    let output = site
        .trusting()
        .args(["ls-remote", &origin.url("absent.git")])
        .output()
        .unwrap();
    let missing = format!("repository '{}/' not found", origin.url("absent.git"));
    expect_failure(&output, &missing);
    expect_failure(&output, "remote: no such repository");
    let output = site
        .trusting()
        .args(["ls-remote", &origin.url("forbidden/r.git")])
        .output()
        .unwrap();
    expect_failure(&output, "remote: the fixture forbids this");
    expect_failure(&output, "The requested URL returned error: 403");
}

/// A credential helper that answers from its script and logs what it is told.
fn credential_helper(site: &Site, user: &str, password: &str) -> String {
    let script = site.path("credential-helper");
    let log = site.path("credential.log");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             echo \"$1\" >> '{}'\n\
             if [ \"$1\" = get ]; then echo username={user}; echo password={password}; fi\n",
            log.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    script.to_str().unwrap().to_string()
}

#[test]
fn credentials_come_from_git_and_are_approved_or_rejected() {
    let site = Site::new("auth");
    site.repository("r");
    let log = site.path("origin.log");
    let origin = site.serve(&["--auth", "me:right", "--log", log.to_str().unwrap()]);
    let url = origin.url("r.git");

    // No credential and no way to ask.
    let output = site.trusting().args(["ls-remote", &url]).output().unwrap();
    expect_failure(&output, "Authentication failed for");

    // A helper with the right password; the store hears it worked.
    let helper = credential_helper(&site, "me", "right");
    let output = site
        .trusting()
        .args([
            "-c",
            &format!("credential.helper={helper}"),
            "ls-remote",
            &url,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    let heard = std::fs::read_to_string(site.path("credential.log")).unwrap();
    assert_eq!(heard, "get\nstore\n");

    // A push re-uses it past discovery rather than asking per request.
    let clone = site.path("clone");
    site.git(
        site.dir.path(),
        &[
            "-c",
            &format!("credential.helper={helper}"),
            "clone",
            "-q",
            &url,
            clone.to_str().unwrap(),
        ],
    );
    site.git(
        &clone,
        &[
            "-c",
            &format!("credential.helper={helper}"),
            "push",
            "-q",
            "origin",
            "main:refs/heads/authed",
        ],
    );

    // The wrong password is erased from the store and named as a failure.
    std::fs::remove_file(site.path("credential.log")).unwrap();
    let helper = credential_helper(&site, "me", "wrong");
    let output = site
        .trusting()
        .args([
            "-c",
            &format!("credential.helper={helper}"),
            "ls-remote",
            &url,
        ])
        .output()
        .unwrap();
    expect_failure(&output, "Authentication failed for");
    expect_failure(&output, "remote: who are you");
    let heard = std::fs::read_to_string(site.path("credential.log")).unwrap();
    assert_eq!(heard, "get\nerase\n");
}

#[test]
fn a_url_s_password_is_used_kept_from_messages_and_kept_from_the_store() {
    let site = Site::new("embedded");
    site.repository("r");
    let origin = site.serve(&["--auth", "me:s3cret"]);
    let helper = credential_helper(&site, "unused", "unused");
    let url = format!("https://me:s3cret@localhost:{}/r.git", origin.port);
    let output = site
        .trusting()
        .args([
            "-c",
            &format!("credential.helper={helper}"),
            "ls-remote",
            &url,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    assert!(!said(&output).contains("s3cret"), "{}", said(&output));
    assert!(!site.path("credential.log").exists());

    let wrong = format!("https://me:n0tit@localhost:{}/r.git", origin.port);
    let output = site
        .trusting()
        .args(["ls-remote", &wrong])
        .output()
        .unwrap();
    expect_failure(&output, "Authentication failed for");
    // Git names the URL in its own messages by its anonymized form too.
    assert!(!said(&output).contains("n0tit"), "{}", said(&output));
}

#[test]
fn redirects_move_the_base_keep_credentials_home_and_never_downgrade() {
    let site = Site::new("redirect");
    site.repository("r");
    let far_log = site.path("far.log");
    let far = site.serve(&["--log", far_log.to_str().unwrap()]);
    let elsewhere = format!("--elsewhere=https://127.0.0.1:{}", far.port);
    let (flag, value) = elsewhere.split_once('=').unwrap();
    let near = site.serve(&["--auth", "me:right", flag, value]);
    let log = site.path("open.log");
    let open = site.serve(&["--log", log.to_str().unwrap()]);

    // A move on the same origin: the POSTs go to the new place.
    let output = site
        .trusting()
        .args(["ls-remote", &open.url("moved/r.git")])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    assert!(
        said(&output).contains("redirecting to"),
        "{}",
        said(&output)
    );
    let heard = std::fs::read_to_string(&log).unwrap();
    assert!(heard.contains("POST /r.git/git-upload-pack?"), "{heard}");
    assert!(!heard.contains("POST /moved/"), "{heard}");

    // Moved before the server asks: the URL's password still answers, with no
    // prompt or helper to fall back on.
    let url = format!("https://me:right@localhost:{}/moved/r.git", near.port);
    let output = site
        .trusting()
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));

    // To another origin: it answers without ever seeing the password.
    let url = format!("https://me:right@localhost:{}/elsewhere/r.git", near.port);
    let output = site.trusting().args(["ls-remote", &url]).output().unwrap();
    assert!(output.status.success(), "{}", said(&output));
    let heard = std::fs::read_to_string(&far_log).unwrap();
    assert!(heard.contains("info/refs"), "{heard}");
    assert!(!heard.contains("auth=true"), "{heard}");

    // From https down to http: refused before anything is sent.
    let output = site
        .trusting()
        .args(["ls-remote", &open.url("to-http/r.git")])
        .output()
        .unwrap();
    expect_failure(&output, "from https to plain http");

    // Redirects turned off.
    let output = site
        .trusting()
        .args([
            "-c",
            "http.followRedirects=false",
            "ls-remote",
            &open.url("moved/r.git"),
        ])
        .output()
        .unwrap();
    expect_failure(&output, "redirects are off");
}

/// A CONNECT proxy that tunnels to loopback and records what it was asked.
struct Proxy {
    port: u16,
    heard: Arc<Mutex<Vec<String>>>,
}

fn proxy() -> Proxy {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let record = heard.clone();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { return };
            let record = record.clone();
            std::thread::spawn(move || tunnel(client, &record));
        }
    });
    Proxy { port, heard }
}

fn tunnel(client: TcpStream, heard: &Mutex<Vec<String>>) {
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    heard.lock().unwrap().push(line.trim().to_string());
    let target = line.split(' ').nth(1).unwrap().to_string();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
    }
    let upstream = TcpStream::connect(target).unwrap();
    let mut answer = client.try_clone().unwrap();
    answer.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
    let mut up_write = upstream.try_clone().unwrap();
    let mut up_read = upstream;
    let mut down_write = client;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut up_write);
        let _ = up_write.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut up_read, &mut down_write);
    let _ = down_write.shutdown(Shutdown::Write);
}

#[test]
fn a_proxy_is_used_as_git_and_curl_would_and_no_proxy_bypasses_it() {
    let site = Site::new("proxy");
    site.repository("r");
    let origin = site.serve(&[]);
    let url = origin.url("r.git");
    let tunnel = proxy();
    let via = format!("http://127.0.0.1:{}", tunnel.port);

    let output = site
        .trusting()
        .env("https_proxy", &via)
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    let asked = tunnel.heard.lock().unwrap().clone();
    let target = format!("CONNECT localhost:{} HTTP/1.1", origin.port);
    assert!(asked.contains(&target), "{asked:?}");

    // A remote's own proxy setting, under a name that needs regex quoting.
    tunnel.heard.lock().unwrap().clear();
    let clone = site.path("clone");
    site.git(
        site.dir.path(),
        &["clone", "-q", &url, clone.to_str().unwrap()],
    );
    site.git(&clone, &["remote", "add", "my-fork.x", &url]);
    site.git(&clone, &["config", "remote.my-fork.x.proxy", &via]);
    site.git(&clone, &["ls-remote", "my-fork.x"]);
    assert!(!tunnel.heard.lock().unwrap().is_empty());

    // Configuration chooses over the environment, as Git does.
    tunnel.heard.lock().unwrap().clear();
    let output = site
        .trusting()
        .env("https_proxy", "http://127.0.0.1:9")
        .args(["-c", &format!("http.proxy={via}"), "ls-remote", &url])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    assert!(!tunnel.heard.lock().unwrap().is_empty());

    // NO_PROXY goes direct past a proxy that would refuse the connection.
    let output = site
        .trusting()
        .env("https_proxy", "http://127.0.0.1:9")
        .env("NO_PROXY", "localhost,127.0.0.1,::1")
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));

    // The proxy is chosen for each origin: a redirect out of no_proxy's reach
    // goes through it, not past it on the first origin's direct path.
    tunnel.heard.lock().unwrap().clear();
    let far = format!("https://127.0.0.1:{}", origin.port);
    let near = site.serve(&["--elsewhere", &far]);
    let output = site
        .trusting()
        .env("https_proxy", &via)
        .env("no_proxy", "localhost")
        .args(["ls-remote", &near.url("elsewhere/r.git")])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", said(&output));
    let asked = tunnel.heard.lock().unwrap().clone();
    let target = format!("CONNECT 127.0.0.1:{} HTTP/1.1", origin.port);
    assert!(asked.contains(&target), "{asked:?}");
    let first = format!("CONNECT localhost:{} HTTP/1.1", near.port);
    assert!(!asked.contains(&first), "{asked:?}");

    // SOCKS is refused by name, from either place a proxy is chosen.
    let output = site
        .trusting()
        .env("all_proxy", "socks5://127.0.0.1:9")
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "SOCKS");
    let output = site
        .trusting()
        .args(["-c", "http.proxy=socks5h://127.0.0.1:9", "ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "SOCKS");

    // A proxy that is down is a failure, never a quiet direct connection.
    let output = site
        .trusting()
        .env("https_proxy", "http://127.0.0.1:9")
        .args(["ls-remote", &url])
        .output()
        .unwrap();
    expect_failure(&output, "unable to access");
}

#[test]
fn a_trickle_below_the_rate_ends_though_no_read_stalls() {
    let site = Site::new("trickle");
    let origin = site.serve(&[]);
    let started = Instant::now();
    let output = site
        .trusting()
        .args([
            "-c",
            "http.lowSpeedLimit=1k",
            "-c",
            "http.lowSpeedTime=2",
            "ls-remote",
            &origin.url("trickle/r.git"),
        ])
        .output()
        .unwrap();
    expect_failure(&output, "stayed below 1024 bytes a second for 2 seconds");
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn a_stalled_server_trips_the_low_speed_deadline() {
    let site = Site::new("stall");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // Accept and say nothing, handshake included.
        let held: Vec<_> = listener.incoming().take(4).collect();
        std::thread::sleep(Duration::from_secs(30));
        drop(held);
    });
    let started = Instant::now();
    let url = format!("https://localhost:{port}/r.git");
    let output = site
        .trusting()
        .args([
            "-c",
            "http.lowSpeedLimit=1",
            "-c",
            "http.lowSpeedTime=1",
            "ls-remote",
            &url,
        ])
        .output()
        .unwrap();
    expect_failure(&output, "unable to access");
    assert!(started.elapsed() < Duration::from_secs(20));
}
