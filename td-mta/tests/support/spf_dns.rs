#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use crate::{dns_wire::MAX_QUERY_BYTES, ports::Clock};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, Ipv6Addr, TcpListener, UdpSocket},
    thread,
    time::Duration,
};

struct Random(u16);
impl Entropy for Random {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), crate::ports::CryptoError> {
        self.0 += 1;
        output.copy_from_slice(&self.0.to_be_bytes());
        Ok(())
    }
}

fn udp() -> UdpSocket {
    let server = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    server
}

fn with_resolver(servers: &[SocketAddr], run: impl FnOnce(&mut Resolver<'_>)) {
    let clock = RuntimeClock::new();
    let deadline = Deadline::after(clock.monotonic().unwrap(), 5_000).unwrap();
    let mut packet = vec![0; dns_transport::PACKET_BYTES];
    let mut random = Random(0);
    let mut resolver = Resolver::new(servers, &clock, deadline, &mut random, &mut packet).unwrap();
    run(&mut resolver);
}

fn rr(owner: &[u8], kind: Query, data: &[u8]) -> Vec<u8> {
    raw_rr(owner, code(kind), data)
}

fn raw_rr(owner: &[u8], kind: u16, data: &[u8]) -> Vec<u8> {
    let mut rr = owner.to_vec();
    rr.extend_from_slice(&kind.to_be_bytes());
    rr.extend_from_slice(&[0, 1, 0, 0, 0, 8]);
    rr.extend_from_slice(&(data.len() as u16).to_be_bytes());
    rr.extend_from_slice(data);
    rr
}

fn answer(query: &[u8], flags: u16, answers: &[Vec<u8>]) -> Vec<u8> {
    let mut result = query.to_owned();
    result[2..4].copy_from_slice(&flags.to_be_bytes());
    result[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
    for answer in answers {
        result.extend_from_slice(answer);
    }
    result
}

#[test]
fn reads_requested_rrsets_and_alias_continuations_from_configured_udp() {
    let server = udp();
    let endpoint = server.local_addr().unwrap();
    thread::scope(|scope| {
        scope.spawn(|| {
            let cases = [
                (
                    Query::Txt,
                    "sender.test",
                    vec![raw_rr(&[0xc0, 12], 5, b"\x04_spf\xc0\x0c")],
                ),
                (
                    Query::Txt,
                    "_spf.sender.test",
                    vec![
                        rr(&[0xc0, 12], Query::Txt, b"\x06v=spf1\x05 -all"),
                        rr(&[0xc0, 12], Query::Txt, b"\x05other"),
                        rr(b"\x09unrelated\x00", Query::Txt, b"\x04data"),
                    ],
                ),
                (
                    Query::Mx,
                    "other.test",
                    vec![
                        rr(&[0xc0, 12], Query::Mx, b"\x00\x64\x02mx\xc0\x0c"),
                        rr(&[0xc0, 12], Query::Mx, b"\x00\x00\x00"),
                    ],
                ),
                (
                    Query::A,
                    "mx.other.test",
                    vec![rr(&[0xc0, 12], Query::A, &[192, 0, 2, 7])],
                ),
                (
                    Query::Aaaa,
                    "mx.other.test",
                    vec![rr(&[0xc0, 12], Query::Aaaa, &Ipv6Addr::LOCALHOST.octets())],
                ),
                (
                    Query::Ptr,
                    "7.2.0.192.in-addr.arpa",
                    vec![rr(&[0xc0, 12], Query::Ptr, b"\x02mx\x05other\x04test\x00")],
                ),
            ];
            for (index, (kind, name, data)) in cases.into_iter().enumerate() {
                let mut bytes = [0; MAX_QUERY_BYTES];
                let (count, peer) = server.recv_from(&mut bytes).unwrap();
                let name = Name::from_labels(name).unwrap();
                let question = WireQuestion {
                    name: &name,
                    kind: code(kind),
                    id: (index + 1) as u16,
                };
                assert_eq!(
                    &bytes[..count],
                    question.write(&mut [0; MAX_QUERY_BYTES]).unwrap()
                );
                server
                    .send_to(&answer(&bytes[..count], 0x8180, &data), peer)
                    .unwrap();
            }
        });
        with_resolver(&[endpoint], |dns| {
            let txt = dns.lookup("Sender.Test.", Query::Txt).unwrap().unwrap();
            assert!(
                matches!(&txt[..], [Record::Text(a),Record::Text(b)] if a == b"v=spf1 -all" && b == b"other")
            );
            let mx = dns.lookup("other.test", Query::Mx).unwrap().unwrap();
            assert!(
                matches!(&mx[..],[Record::Name(a),Record::Name(b)] if a=="mx.other.test" && b==".")
            );
            let a = dns.lookup("mx.other.test", Query::A).unwrap().unwrap();
            assert!(matches!(&a[..],[Record::Address(ip)] if *ip == Ipv4Addr::new(192,0,2,7)));
            let aaaa = dns.lookup("mx.other.test", Query::Aaaa).unwrap().unwrap();
            assert!(matches!(&aaaa[..],[Record::Address(ip)] if *ip == Ipv6Addr::LOCALHOST));
            let ptr = dns
                .lookup("7.2.0.192.in-addr.arpa", Query::Ptr)
                .unwrap()
                .unwrap();
            assert!(matches!(&ptr[..],[Record::Name(host)] if host=="mx.other.test"));
        });
    });
}

#[test]
fn tcp_fallback_returns_txt_and_configured_servers_distinguish_missing_and_error() {
    let first = udp();
    let second = udp();
    let endpoints = [first.local_addr().unwrap(), second.local_addr().unwrap()];
    let tcp = TcpListener::bind(endpoints[0]).unwrap();
    tcp.set_nonblocking(true).unwrap();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut bytes = [0; MAX_QUERY_BYTES];
            let (count, peer) = first.recv_from(&mut bytes).unwrap();
            let query = &bytes[..count];
            first.send_to(&answer(query, 0x8380, &[]), peer).unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match tcp.accept() {
                    Ok((s, _)) => break s,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < until =>
                    {
                        thread::sleep(Duration::from_millis(1))
                    }
                    other => panic!("TCP fallback: {other:?}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut length = [0; 2];
            stream.read_exact(&mut length).unwrap();
            assert_eq!(u16::from_be_bytes(length) as usize, count);
            let mut wire = vec![0; count];
            stream.read_exact(&mut wire).unwrap();
            assert_eq!(wire, query);
            let mut second_text = [0; 256];
            second_text[0] = 255;
            let data = [
                rr(&[0xc0, 12], Query::Txt, &[255; 256]),
                rr(&[0xc0, 12], Query::Txt, &second_text),
            ];
            let reply = answer(query, 0x8180, &data);
            assert!(reply.len() > 512);
            stream
                .write_all(&(reply.len() as u16).to_be_bytes())
                .unwrap();
            stream.write_all(&reply).unwrap();
            for (server, rcode) in [
                (&first, 2),
                (&second, 3),
                (&first, 0),
                (&first, 2),
                (&second, 5),
            ] {
                let (n, peer) = server.recv_from(&mut bytes).unwrap();
                server
                    .send_to(&answer(&bytes[..n], 0x8180 + rcode, &[]), peer)
                    .unwrap();
            }
        });
        with_resolver(&endpoints, |dns| {
            let txt = dns.lookup("test.example", Query::Txt).unwrap().unwrap();
            assert!(
                matches!(&txt[..],[Record::Text(a),Record::Text(b)] if a.len()==255 && b.len()==255)
            );
            assert!(dns.lookup("absent.example", Query::Txt).unwrap().is_none());
            assert!(dns.lookup("empty.example", Query::Ptr).unwrap().is_none());
            assert!(matches!(
                dns.lookup("error.example", Query::Mx),
                Err(DnsError::Temporary)
            ));
        });
    });
}

#[test]
fn bounded_lookups_reject_invalid_names_and_expired_or_exhausted_budgets() {
    let server = udp();
    server
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    with_resolver(&[server.local_addr().unwrap()], |dns| {
        assert!(matches!(
            dns.lookup("one..example", Query::Txt),
            Err(DnsError::Permanent)
        ));
        dns.meter = Meter::new(dns.deadline, Charge::default());
        assert!(matches!(
            dns.lookup("example.test", Query::A),
            Err(DnsError::Unavailable)
        ));
        dns.deadline = Deadline::after(crate::ports::Tick(0), 0).unwrap();
        assert!(matches!(
            dns.lookup("example.test", Query::Txt),
            Err(DnsError::Unavailable)
        ));
    });
    assert!(server.recv_from(&mut [0; 512]).is_err());
}

#[test]
fn response_names_keep_labels_and_txt_never_accepts_partial_data() {
    let name = Name::from_labels("%20.+@!_.x.test.").unwrap();
    assert_eq!(presentation(&name).unwrap(), "%20.+@!_.x.test");
    assert!(Name::from_labels(&format!("{}.test", "x".repeat(64))).is_err());
    let binary = Name::read(b"\x03x.y\x00", 0).unwrap().0;
    assert!(presentation(&binary).is_err());
    assert_eq!(text(b"\x00").unwrap(), b"");
    assert!(text(b"\x05abcde\x03x").is_err());
    let error = td_alloc::with_reservation_limit(0, || text(b"\x03xyz"));
    assert_eq!(error, Err(WireError::Capacity));
}

#[test]
fn a_cross_packet_alias_cycle_cannot_publish_terminal_data() {
    let server = udp();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut bytes = [0; MAX_QUERY_BYTES];
            let (count, peer) = server.recv_from(&mut bytes).unwrap();
            let first = raw_rr(&[0xc0, 12], 5, b"\x03mid\x04test\x00");
            server
                .send_to(&answer(&bytes[..count], 0x8180, &[first]), peer)
                .unwrap();
            let (count, peer) = server.recv_from(&mut bytes).unwrap();
            let start = b"\x05start\x04test\x00";
            let rows = [
                raw_rr(&[0xc0, 12], 5, start),
                rr(start, Query::Txt, b"\x06v=spf1"),
            ];
            server
                .send_to(&answer(&bytes[..count], 0x8180, &rows), peer)
                .unwrap();
        });
        with_resolver(&[server.local_addr().unwrap()], |dns| {
            assert!(matches!(
                dns.lookup("start.test", Query::Txt),
                Err(DnsError::Temporary)
            ));
        });
    });
}

#[test]
fn local_rrset_allocation_failure_is_unavailable_even_for_ptr() {
    let server = udp();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut bytes = [0; MAX_QUERY_BYTES];
            let (count, peer) = server.recv_from(&mut bytes).unwrap();
            let names = [rr(&[0xc0, 12], Query::Ptr, b"\x02mx\x04test\x00")];
            server
                .send_to(&answer(&bytes[..count], 0x8180, &names), peer)
                .unwrap();
        });
        with_resolver(&[server.local_addr().unwrap()], |dns| {
            let result = td_alloc::with_reservation_limit(0, || {
                dns.lookup("2.0.0.127.in-addr.arpa", Query::Ptr)
            });
            assert!(matches!(result, Err(DnsError::Unavailable)));
        });
    });
}

#[test]
fn one_more_alias_than_the_lookup_budget_is_unavailable() {
    let server = udp();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut bytes = [0; MAX_QUERY_BYTES];
            let (count, peer) = server.recv_from(&mut bytes).unwrap();
            let mut current = Name::from_labels("start.test").unwrap();
            let mut rows = Vec::new();
            for i in 0..=MAX_ALIASES {
                let target = Name::from_labels(&format!("alias{i}.test")).unwrap();
                rows.push(raw_rr(current.wire(), 5, target.wire()));
                current = target;
            }
            rows.push(rr(current.wire(), Query::Txt, b"\x06v=spf1"));
            server
                .send_to(&answer(&bytes[..count], 0x8180, &rows), peer)
                .unwrap();
        });
        with_resolver(&[server.local_addr().unwrap()], |dns| {
            assert!(matches!(
                dns.lookup("start.test", Query::Txt),
                Err(DnsError::Unavailable)
            ));
        });
    });
}

#[test]
fn only_nxdomain_and_zero_answer_count_spend_void_budget() {
    use crate::spf::{evaluate, Outcome};

    for (last, expected) in [
        (vec![rr(&[0xc0, 12], Query::Txt, b"\x01x")], Outcome::Fail),
        (vec![], Outcome::Permerror),
    ] {
        let server = udp();
        thread::scope(|scope| {
            scope.spawn(|| {
                let policy = b"v=spf1 exists:one.test exists:two.test exists:three.test -all";
                let mut data = vec![policy.len() as u8];
                data.extend_from_slice(policy);
                let cname = raw_rr(&[0xc0, 12], 5, b"\x06absent\x04test\x00");
                for (flags, rows) in [
                    (0x8180, vec![rr(&[0xc0, 12], Query::Txt, &data)]),
                    (0x8183, vec![cname]), // NXDOMAIN can carry an answer.
                    (0x8180, vec![]),
                    (0x8180, last),
                ] {
                    let mut bytes = [0; MAX_QUERY_BYTES];
                    let (count, peer) = server.recv_from(&mut bytes).unwrap();
                    server
                        .send_to(&answer(&bytes[..count], flags, &rows), peer)
                        .unwrap();
                }
            });
            with_resolver(&[server.local_addr().unwrap()], |dns| {
                assert_eq!(
                    evaluate(
                        dns,
                        Ipv4Addr::LOCALHOST.into(),
                        "mx.test",
                        "sender@example.test"
                    ),
                    expected
                );
            });
        });
    }
}

#[test]
fn surplus_ptr_names_need_wire_framing_only() {
    let server = udp();
    thread::scope(|scope| {
        scope.spawn(|| {
            let mut bytes = [0; MAX_QUERY_BYTES];
            let (count, peer) = server.recv_from(&mut bytes).unwrap();
            let mut rows = Vec::new();
            for i in 0..10 {
                let target = Name::from_labels(&format!("mx{i}.test")).unwrap();
                rows.push(rr(&[0xc0, 12], Query::Ptr, target.wire()));
            }
            rows.push(rr(&[0xc0, 12], Query::Ptr, b"\x03x.y\x00"));
            server
                .send_to(&answer(&bytes[..count], 0x8180, &rows), peer)
                .unwrap();

            // The ignored name must still consume exactly its RDATA.
            *rows.last_mut().unwrap() = rr(&[0xc0, 12], Query::Ptr, b"\x01x\x00\x00");
            let name = Name::from_labels("1.0.0.127.in-addr.arpa").unwrap();
            let q = WireQuestion {
                name: &name,
                kind: code(Query::Ptr),
                id: 1,
            };
            assert!(matches!(
                decode(
                    &answer(&bytes[..count], 0x8180, &rows),
                    q,
                    Query::Ptr,
                    MAX_ALIASES
                ),
                Err(WireError::Invalid)
            ));
        });
        with_resolver(&[server.local_addr().unwrap()], |dns| {
            let ptr = dns
                .lookup("1.0.0.127.in-addr.arpa", Query::Ptr)
                .unwrap()
                .unwrap();
            assert_eq!(ptr.len(), 10);
            for (i, record) in ptr.iter().enumerate() {
                assert!(matches!(record, Record::Name(name) if *name == format!("mx{i}.test")));
            }
        });
    });
}
