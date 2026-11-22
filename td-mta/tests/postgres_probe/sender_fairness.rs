//! A sender whose DNS answers slowly must not spend another sender's
//! observation budget. Names under slow.example.test answer after 900 ms,
//! inside the resolver's one-second idle cap, so one SPF evaluation of that
//! domain needs three slow answers and outlives its two-second budget.
use super::{sql, Fixture};
use std::{
    io::Write,
    net::UdpSocket,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

const SLOW: &[u8] = b"\x04slow\x07example\x04test\x00";
const DELAY: Duration = Duration::from_millis(900);

pub(super) struct Dns {
    pub address: std::net::SocketAddr,
    done: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

fn record(name: &[u8]) -> Option<&'static str> {
    Some(match name {
        b"\x04pass\x07example\x04test\x00" => "v=spf1 +all",
        b"\x04slow\x07example\x04test\x00" => {
            "v=spf1 include:a.slow.example.test include:b.slow.example.test -all"
        }
        b"\x01a\x04slow\x07example\x04test\x00" | b"\x01b\x04slow\x07example\x04test\x00" => {
            "v=spf1 -all"
        }
        b"\x04slow\x0a_domainkey\x04slow\x07example\x04test\x00" => super::dkim_vectors::RSA_TXT,
        b"\x06_dmarc\x08football\x07example\x03com\x00" => "v=DMARC1; p=reject; adkim=s; aspf=s",
        b"\x04test\x0a_domainkey\x08football\x07example\x03com\x00" => super::dkim_vectors::RSA_TXT,
        b"\x08brisbane\x0a_domainkey\x08football\x07example\x03com\x00" => {
            super::dkim_vectors::ED_TXT
        }
        _ => return None,
    })
}

impl Dns {
    pub(super) fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let stop = done.clone();
        let thread = thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let mut packet = [0; 512];
                let (n, peer) = match socket.recv_from(&mut packet) {
                    Ok(received) => received,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(error) => panic!("{error}"),
                };
                let (_, name_bytes) = td_mta::dns_wire::Name::read(&packet[..n], 12).unwrap();
                let name_end = 12 + name_bytes;
                let question_end = name_end + 4;
                let name = &packet[12..name_end];
                let kind = u16::from_be_bytes(packet[name_end..name_end + 2].try_into().unwrap());
                let mut answer = packet[..question_end].to_vec();
                answer[2] |= 0x80;
                if let Some(text) = record(name).filter(|_| kind == 16) {
                    let size = u8::try_from(text.len()).unwrap();
                    answer[7] = 1;
                    answer.extend_from_slice(&[0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60]);
                    answer.extend_from_slice(&(u16::from(size) + 1).to_be_bytes());
                    answer.push(size);
                    answer.extend_from_slice(text.as_bytes());
                }
                // Echo the query's OPT record as the reply's additional section.
                answer.extend_from_slice(&packet[question_end..n]);
                if name.ends_with(SLOW) {
                    let delayed = socket.try_clone().unwrap();
                    thread::spawn(move || {
                        thread::sleep(DELAY);
                        let _ = delayed.send_to(&answer, peer);
                    });
                } else {
                    socket.send_to(&answer, peer).unwrap();
                }
            }
        });
        Self {
            address,
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for Dns {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !thread::panicking() {
                assert!(joined.is_ok());
            }
        }
    }
}

const SLOW_MESSAGE: &[u8] = concat!(
    "DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;\r\n",
    " d=slow.example.test; s=slow; t=1528637909; h=from : subject;\r\n",
    " bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r\n",
    " b=F45dVWDfMbQDGHJFlXUNB2HKfbCeLRyhDXgFpEL8GwpsRe0IeIixNTe3\r\n",
    " DhCVlUrSjV4BwcVcOF6+FF3Zo9Rpo1tFOeS9mPYQTnGdaSGsgeefOsk2Jz\r\n",
    " dA+L10TeYt9BgDfQNZtKdN1WO//KgIqXP7OdEFE4LjFYNcUxZQ4FADY+8=\r\n",
    "From: Slow <sender@slow.example.test>\r\n",
    "Subject: slow sender\r\n",
    "\r\n",
    "Slow DNS.\r\n",
)
.as_bytes();

fn stored(fixture: &Fixture, subject: &str) -> String {
    super::sql_bounded(
        fixture,
        &format!("SELECT convert_from(c.bytes,'UTF8') FROM td_mta.blob_chunks c WHERE c.ordinal=0 AND position(convert_to('Subject: {subject}','UTF8') in c.bytes)>0"),
        16384,
    )
}

/// Start the slow sender's observation, complete an ordinary delivery while
/// that observation still holds a worker, then finish the slow delivery.
pub(super) fn exercise(
    fixture: &Fixture,
    address: std::net::SocketAddr,
    clock: Arc<td_mta::clock::RuntimeClock>,
) {
    let initial: u64 = sql(fixture, "SELECT count(*) FROM td_mta.emails")
        .parse()
        .unwrap();
    let mut slow = super::smtp_transport::connect(fixture, address, false, clock.clone());
    // The slow SPF observation starts at MAIL and needs three 900 ms answers.
    let slow_started = std::time::Instant::now();
    super::smtp_transport::command(
        &mut slow,
        b"MAIL FROM:<sender@slow.example.test>\r\n",
        "250",
    );
    super::smtp_transport::command(&mut slow, b"RCPT TO:<main@example.test>\r\n", "250");
    super::smtp_transport::command(&mut slow, b"DATA\r\n", "354");
    slow.get_mut().write_all(SLOW_MESSAGE).unwrap();
    slow.get_mut().write_all(b".\r\n").unwrap();
    slow.get_mut().flush().unwrap();

    let mut normal = super::smtp_transport::connect(fixture, address, false, clock);
    super::smtp_transport::command(
        &mut normal,
        b"MAIL FROM:<sender@pass.example.test>\r\n",
        "250",
    );
    super::smtp_transport::command(&mut normal, b"RCPT TO:<main@example.test>\r\n", "250");
    super::smtp_transport::command(&mut normal, b"DATA\r\n", "354");
    normal
        .get_mut()
        .write_all(super::dkim_vectors::MESSAGE)
        .unwrap();
    super::smtp_transport::command(&mut normal, b".\r\n", "250");
    // The slow observation holds its worker for its whole 2 s budget, so an
    // ordinary delivery finished inside it really overlapped the slow one.
    assert!(slow_started.elapsed() < Duration::from_millis(2000));
    super::smtp_transport::reply(&mut slow, "250");
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.emails"),
        (initial + 2).to_string()
    );

    let ordinary = stored(fixture, "Is dinner ready?");
    let (trace, _) = ordinary.split_once("\r\nReceived: ").unwrap();
    assert!(
        trace.starts_with("Return-Path: <sender@pass.example.test>\r\nReceived-SPF: pass "),
        "{trace}"
    );
    assert!(
        trace.contains("\r\nX-TD-DKIM-Results: complete;"),
        "{trace}"
    );
    for signature in [
        "result=pass; a=rsa-sha256; bits=1024; d=football.example.com; s=test",
        "result=pass; a=ed25519-sha256; bits=256; d=football.example.com; s=brisbane",
    ] {
        assert!(trace.contains(signature), "{trace}");
    }
    assert!(
        trace.contains("\r\nX-TD-DMARC-Result: pass; author=football.example.com;"),
        "{trace}"
    );
    // The slow sender really did hold a worker past its own budget.
    let delayed = stored(fixture, "slow sender");
    assert!(
        delayed.starts_with("Return-Path: <sender@slow.example.test>\r\nReceived-SPF: temperror "),
        "{delayed}"
    );
    super::smtp_transport::command(&mut normal, b"QUIT\r\n", "221");
    super::smtp_transport::command(&mut slow, b"QUIT\r\n", "221");
}
