//! Warm td-mail inbox refresh and the serving network thread's idle scheduling.
use super::*;
use crate::{mail_client, serving_events};
use std::path::{Path, PathBuf};
use td_json::json;

fn network_task(pid: u32) -> PathBuf {
    let mut threads = fs::read_dir(format!("/proc/{pid}/task"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|task| {
            fs::read_to_string(task.join("comm"))
                .is_ok_and(|name| name.trim_end() == "https-network")
        });
    let task = threads.next().unwrap();
    assert!(threads.next().is_none(), "ambiguous network thread");
    task
}

fn voluntary_switches(task: &Path) -> u64 {
    fs::read_to_string(task.join("status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

pub(super) fn measure(
    fixture: &Fixture,
    local: &Path,
    desktop: &Service,
    jmap: &Jmap,
    smtp: SocketAddr,
) {
    let send = |index| {
        let (mut stream, _) = Stream::connect(smtp);
        assert!(matches!(
            stream.attempt(&message(1, index, SMALL_BYTES)),
            Attempt::Accepted(_)
        ));
    };
    send(0);
    let origin = format!("https://localhost:{}", jmap.address.port());
    let (config, _fetch) =
        mail_client::client_setup(fixture, local, &origin, jmap.device.password());
    let mut client = mail_client::Client::start(&config, local);
    let connected = client.ask(json!({"command":"connect","account":"test"}));
    assert!(connected["ok"].is_true(), "{connected}");
    let folders = client.ask(json!({"command":"list_mailboxes"}));
    assert!(folders["ok"].is_true(), "{folders}");
    let inbox = folders["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["role"].as_str() == Some("inbox"))
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let task = network_task(desktop.pid());
    let session = jmap.get("/.well-known/jmap");
    assert_eq!(session.status, 200);
    let session = td_json::parse_slice(&session.body).unwrap();
    let account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        .as_str()
        .unwrap();
    for push in [false, true] {
        let mut stream = push.then(|| {
            let mut stream = serving_events::open(
                &session,
                jmap.address,
                &jmap.ca,
                &jmap.authorization,
                "no",
                None,
            );
            let (_, initial) = serving_events::state(&mut stream, account);
            (stream, initial)
        });
        let started = Instant::now();
        let before = voluntary_switches(&task);
        thread::sleep(Duration::from_secs(10));
        let switches = voluntary_switches(&task) - before;
        let idle = started.elapsed().as_secs_f64();
        let mut latencies = Vec::new();
        for turn in 0..=32 {
            // Pacing stays outside the timer and below the per-peer auth limit.
            thread::sleep(Duration::from_secs(1));
            let request = json!({"command":"query_emails", "mailbox_id":inbox, "limit":50});
            let start = Instant::now();
            let result = client.ask(request);
            let elapsed = start.elapsed();
            assert!(result["ok"].is_true(), "{result}");
            let emails = result["emails"].as_array().unwrap();
            assert_eq!(emails.len(), 1, "{result}");
            assert_eq!(emails[0]["subject"].as_str(), Some("performance 1.0"));
            let thread = emails[0]["thread_id"].as_str().unwrap();
            let counts = &result["thread_counts"][thread];
            assert_eq!(counts["total"].as_u64(), Some(1), "{result}");
            assert_eq!(counts["unread"].as_u64(), Some(1), "{result}");
            if turn != 0 {
                latencies.push(elapsed.as_secs_f64() * 1000.0);
            }
        }
        if let Some((stream, initial)) = stream.as_mut() {
            send(1);
            assert_ne!(
                serving_events::state(stream, account).1,
                *initial,
                "live stream must survive measurement"
            );
        }
        report("https_inbox_refresh", &format!(
            "\"push\":{push},\"idle_seconds\":{idle:.3},\"network_voluntary_switches\":{switches},\"network_voluntary_switches_per_second\":{:.2},\"refresh_ms\":{}", switches as f64 / idle, distribution(&mut latencies)));
    }
}
