#![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
use super::*;

const REQUEST: &[u8] = b"POST /jmap?x=%2f HTTP/1.1\r\nHost: mail.example:8443\r\nAuthorization: Basic dTpw\r\nContent-Type: application/json\r\nContent-Length: 3\r\nExpect: 100-continue\r\nConnection: keep-alive, close\r\n\r\n";

#[test]
fn head_every_split_and_pipeline_tail() {
    for split in 0..=REQUEST.len() {
        let mut storage = [0; HEAD_BYTES];
        let mut reader = HeadReader::new(&mut storage);
        let first = reader.feed(&REQUEST[..split]).unwrap();
        assert_eq!(first.consumed, split);
        let mut tail = REQUEST[split..].to_vec();
        tail.extend_from_slice(b"abcGET /next HTTP/1.1\r\n");
        let last = reader.feed(&tail).unwrap();
        assert_eq!(last.consumed, REQUEST.len() - split);
        assert!(last.complete);
        let head = reader.head().unwrap();
        assert_eq!(head.method, "POST");
        assert_eq!(head.target, "/jmap?x=%2f");
        assert_eq!(head.host, "mail.example:8443");
        assert_eq!(head.authorization, Some("Basic dTpw"));
        assert_eq!(head.content_type, Some("application/json"));
        assert_eq!(head.framing, Framing::Length(3));
        assert!(head.close && head.expect_continue);
        assert_eq!(reader.feed(b"never consume").unwrap().consumed, 0);
        reader.eof().unwrap();
        drop(reader);
        assert!(storage.iter().all(|b| *b == 0));
    }
}

fn rejected(request: &[u8]) {
    for width in [1, 7, request.len()] {
        let mut storage = [0; HEAD_BYTES];
        let mut reader = HeadReader::new(&mut storage);
        let mut error = None;
        let mut offset = 0;
        while offset < request.len() {
            let end = request.len().min(offset + width);
            match reader.feed(&request[offset..end]) {
                Err(e) => {
                    error = Some(e);
                    break;
                }
                Ok(progress) => {
                    assert!(progress.consumed > 0, "unexpected completed request");
                    offset += progress.consumed;
                }
            }
        }
        let error = error.expect("request must fail");
        assert_eq!(reader.feed(REQUEST), Err(error));
        assert!(matches!(reader.head(), Err(e) if e == error));
        assert_eq!(reader.eof(), Err(error));
    }
}

#[test]
fn smuggling_and_ambiguous_headers_are_terminal() {
    for fields in [
        "Content-Length: 1\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length: 1\r\nContent-Length: 1\r\n",
        "Content-Length: 1, 1\r\n",
        "Content-Length: +1\r\n",
        "Content-Length: -1\r\n",
        "Content-Length: 1 0\r\n",
        "Content-Length: 18446744073709551616\r\n",
        "Transfer-Encoding: chunked, chunked\r\n",
        "Transfer-Encoding: gzip, chunked\r\n",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        "Content-Length : 1\r\n",
        " Content-Length: 1\r\n",
        "X: value\r\n secret: fold\r\n",
        "X: secret\nHidden: yes\r\n",
        "X: secret\rHidden: yes\r\n",
        "X: \u{7f}\r\n",
        "Host: other.example\r\n",
        "Authorization: secret\r\nAuthorization: secret\r\n",
        "Content-Type: a\r\nContent-Type: b\r\n",
        "Origin: https://evil\r\nOrigin: https://mail.example\r\n",
        "Content-Encoding: gzip\r\n",
        "Expect: something\r\n",
        "Upgrade: h2c\r\n",
        "Connection: authorization\r\n",
        "Connection: \r\n",
        "Trailer: Digest\r\n",
        "X: \0\r\n",
    ] {
        rejected(format!("POST / HTTP/1.1\r\nHost: mail.example\r\n{fields}\r\n").as_bytes());
    }
    for request in [
        "GET / HTTP/1.1\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: \r\n\r\n",
        "GET / HTTP/1.1\r\nHost: mail.example,other\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: user@mail.example\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: [::1]garbage\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: [fe80::1%25eth0]\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: mail.example:0\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: mail.example:65536\r\n\r\n",
    ] {
        rejected(request.as_bytes());
    }
    for line in [
        "GET / HTTP/1.0",
        "GET  / HTTP/1.1",
        "GET\t/ HTTP/1.1",
        "GET / HTTP/1.1 extra",
        "GET https://user@mail.example/ HTTP/1.1",
        "GET https://mail.example/#fragment HTTP/1.1",
        "GET ftp://mail.example/ HTTP/1.1",
        "CONNECT mail.example:443 HTTP/1.1",
        "GET /#fragment HTTP/1.1",
        "GET /\\evil HTTP/1.1",
        "GET /% HTTP/1.1",
        "GET /%0q HTTP/1.1",
    ] {
        rejected(format!("{line}\r\nHost: mail.example\r\n\r\n").as_bytes());
    }
}

#[test]
fn exact_head_limits_eof_and_quantum() {
    let prefix = "GET / HTTP/1.1\r\nHost: [::1]:443\r\nX: ";
    let wire = format!("{prefix}{}\r\n\r\n", "a".repeat(LINE_BYTES - 5));
    let mut storage = [0; HEAD_BYTES];
    let mut reader = HeadReader::new(&mut storage);
    assert!(reader.feed(wire.as_bytes()).unwrap().complete);
    assert_eq!(reader.head().unwrap().framing, Framing::Length(0));
    rejected(format!("{prefix}{}\r\n\r\n", "a".repeat(LINE_BYTES - 4)).as_bytes());
    let fields = "X: a\r\n".repeat(HEADER_COUNT);
    rejected(format!("GET / HTTP/1.1\r\nHost: h\r\n{fields}\r\n").as_bytes());
    for prefix in 0..REQUEST.len() {
        let mut small = vec![0; REQUEST.len()];
        let mut r = HeadReader::new(&mut small);
        r.feed(&REQUEST[..prefix]).unwrap();
        assert_eq!(r.eof(), Err(Error::Incomplete));
        assert_eq!(r.feed(REQUEST), Err(Error::Incomplete));
    }
    let large = format!(
        "GET / HTTP/1.1\r\nHost: h\r\n{}\r\n",
        format!("X: {}\r\n", "a".repeat(8000)).repeat(4)
    );
    let mut big = [0; HEAD_BYTES];
    let mut r = HeadReader::new(&mut big);
    let first = r.feed(large.as_bytes()).unwrap();
    assert_eq!(first.consumed, QUANTUM);
    assert!(!first.complete);
    assert!(
        r.feed(&large.as_bytes()[first.consumed..])
            .unwrap()
            .complete
    );
    let mut tiny = [0; 8];
    assert_eq!(HeadReader::new(&mut tiny).feed(REQUEST), Err(Error::Limit));
}

#[test]
fn aggregate_head_and_field_count_exact_boundaries() {
    let mut exact = String::from("GET / HTTP/1.1\r\nHost: h\r\n");
    for _ in 0..3 {
        exact.push_str(&format!("X: {}\r\n", "a".repeat(LINE_BYTES - 5)));
    }
    let remaining = HEAD_BYTES - exact.len() - 2;
    exact.push_str(&format!("X: {}\r\n\r\n", "a".repeat(remaining - 5)));
    assert_eq!(exact.len(), HEAD_BYTES);
    let mut storage = [0; HEAD_BYTES * 2];
    let mut reader = HeadReader::new(&mut storage);
    let first = reader.feed(exact.as_bytes()).unwrap();
    assert!(
        reader
            .feed(&exact.as_bytes()[first.consumed..])
            .unwrap()
            .complete
    );
    let overflow = exact.replacen("Host: h", "Host: hh", 1);
    rejected(overflow.as_bytes());
    let mut storage = [0; HEAD_BYTES * 2];
    let mut reader = HeadReader::new(&mut storage);
    let first = reader.feed(overflow.as_bytes()).unwrap();
    let second = reader.feed(&overflow.as_bytes()[first.consumed..]).unwrap();
    assert_eq!(
        reader.feed(&overflow.as_bytes()[first.consumed + second.consumed..]),
        Err(Error::Limit)
    );
    let mut count = String::from("GET / HTTP/1.1\r\nHost: h\r\n");
    count.push_str(&"X: a\r\n".repeat(HEADER_COUNT - 1));
    count.push_str("\r\n");
    let mut storage = [0; HEAD_BYTES];
    assert!(
        HeadReader::new(&mut storage)
            .feed(count.as_bytes())
            .unwrap()
            .complete
    );
}

fn decode(wire: &[u8], framing: Framing, width: usize, output_width: usize) -> (Vec<u8>, usize) {
    let mut reader = BodyReader::new(framing, 100, 1000).unwrap();
    let mut result = Vec::new();
    let mut consumed = 0;
    loop {
        let end = wire.len().min(consumed + width);
        let mut output = vec![0; output_width];
        let p = reader.feed(&wire[consumed..end], &mut output).unwrap();
        result.extend_from_slice(&output[..p.written]);
        consumed += p.consumed;
        if p.complete {
            break;
        }
        assert!(p.consumed > 0 || p.written > 0);
    }
    reader.eof().unwrap();
    assert_eq!(reader.feed(b"tail", &mut []).unwrap().consumed, 0);
    (result, consumed)
}

#[test]
fn bodies_fragmented_binary_backpressure_and_pipeline() {
    let wire = b"3; foo = \"a\\\";b\";flag\r\na\0b\r\n2\r\ncd\r\n0\r\n\r\nGET /next";
    for width in 1..wire.len() {
        for output in [1, 2, 8] {
            let (body, consumed) = decode(wire, Framing::Chunked, width, output);
            assert_eq!(body, b"a\0bcd");
            assert_eq!(&wire[consumed..], b"GET /next");
            let (body, consumed) = decode(b"a\0bcdGET /next", Framing::Length(5), width, output);
            assert_eq!(body, b"a\0bcd");
            assert_eq!(consumed, 5);
        }
    }
    let mut r = BodyReader::new(Framing::Length(1), 1, 1).unwrap();
    assert_eq!(
        r.feed(b"x", &mut []).unwrap(),
        Progress {
            consumed: 0,
            written: 0,
            complete: false
        }
    );
    assert!(r.feed(b"x", &mut [0]).unwrap().complete);
}

#[test]
fn body_refusals_limits_and_incomplete_are_sticky() {
    for wire in [
        &b"+1\r\nx\r\n0\r\n\r\n"[..],
        b" 1\r\nx\r\n0\r\n\r\n",
        b"1\nx",
        b"1\rx",
        b"1\r\nxX",
        b"1\r\nx\rX",
        b"0\r\nX: trailer\r\n\r\n",
        b"0\r\n\n",
        b"10000000000000000\r\n",
        b"1;\r\n",
        b"1;x=\r\n",
        b"1;x=\"bad\r\n",
        b"1;x=\"x\"garbage\r\n",
        b"1 \r\n",
        b"1;foo=bar \r\n",
        b"1;foo=\"bar\" \r\n",
        b"1;x=\x7f\r\n",
    ] {
        let mut r = BodyReader::new(Framing::Chunked, 100, 1000).unwrap();
        let error = r.feed(wire, &mut [0; 100]).unwrap_err();
        assert_eq!(r.feed(b"0\r\n\r\n", &mut [0; 100]), Err(error));
        assert_eq!(r.eof(), Err(error));
    }
    assert!(matches!(
        BodyReader::new(Framing::Length(11), 10, 100),
        Err(Error::Limit)
    ));
    assert!(matches!(
        BodyReader::new(Framing::Length(11), 100, 10),
        Err(Error::Limit)
    ));
    let wire = b"1\r\nx\r\n1\r\ny\r\n0\r\n\r\n";
    for (decoded, overhead) in [(1, 100), (2, wire.len() as u64 - 1)] {
        let mut r = BodyReader::new(Framing::Chunked, decoded, overhead).unwrap();
        assert_eq!(r.feed(wire, &mut [0; 10]), Err(Error::Limit));
    }
    let mut r = BodyReader::new(Framing::Chunked, 2, wire.len() as u64).unwrap();
    assert!(r.feed(wire, &mut [0; 2]).unwrap().complete);
    for end in 0..wire.len() {
        let mut r = BodyReader::new(Framing::Chunked, 2, 100).unwrap();
        r.feed(&wire[..end], &mut [0; 2]).unwrap();
        assert_eq!(r.eof(), Err(Error::Incomplete));
    }
    let huge = format!("1;x={}\r\n", "a".repeat(CHUNK_LINE_BYTES));
    let mut r = BodyReader::new(Framing::Chunked, 10, 2000).unwrap();
    assert_eq!(r.feed(huge.as_bytes(), &mut [0; 10]), Err(Error::Limit));
}

#[test]
fn chunked_head_origin_and_exact_chunk_line() {
    let request = b"POST /upload HTTP/1.1\r\nHost: mail.example\r\nOrigin: https://client.example\r\nTransfer-Encoding: ChUnKeD\r\n\r\n";
    let mut storage = [0; HEAD_BYTES];
    let mut reader = HeadReader::new(&mut storage);
    assert!(reader.feed(request).unwrap().complete);
    let head = reader.head().unwrap();
    assert_eq!(head.framing, Framing::Chunked);
    assert_eq!(head.origin, Some("https://client.example"));
    let wire = format!("1;x={}\r\nx\r\n0\r\n\r\n", "a".repeat(CHUNK_LINE_BYTES - 4));
    let mut body = BodyReader::new(head.framing, 1, wire.len() as u64).unwrap();
    let mut out = [0];
    assert!(body.feed(wire.as_bytes(), &mut out).unwrap().complete);
    assert_eq!(out, *b"x");
}

#[test]
fn request_line_syntax_is_distinct_from_unsupported_version() {
    for (line, expected) in [
        ("GET / HTTP/1.1 extra", Error::Syntax),
        ("GET / HTTP/1.1 ", Error::Syntax),
        ("GET /", Error::Syntax),
        ("GET / HTTP/10.0", Error::Syntax),
        ("GET / HTTP/1.x", Error::Syntax),
        ("GET / HTTP/1.0", Error::Unsupported),
        ("GET / HTTP/2.0", Error::Unsupported),
    ] {
        let wire = format!("{line}\r\nHost: mail.example\r\n\r\n");
        let mut storage = [0; HEAD_BYTES];
        assert_eq!(
            HeadReader::new(&mut storage).feed(wire.as_bytes()),
            Err(expected)
        );
    }
}

#[test]
fn absolute_targets_and_last_event_id_are_bounded_single_fields() {
    for (target, origin, path) in [
        (
            "https://mail.example/jmap/api?x=1",
            "https://mail.example",
            "/jmap/api?x=1",
        ),
        ("HTTPS://MAIL.EXAMPLE:443", "HTTPS://MAIL.EXAMPLE:443", "/"),
        ("http://mail.example?x=1", "http://mail.example", "?x=1"),
        ("https://[::1]:443/", "https://[::1]:443", "/"),
    ] {
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: other.example\r\nLast-Event-ID: opaque-id\r\n\r\n"
        );
        let mut storage = [0; HEAD_BYTES];
        let mut reader = HeadReader::new(&mut storage);
        assert!(reader.feed(request.as_bytes()).unwrap().complete);
        let head = reader.head().unwrap();
        assert_eq!(head.target_origin, Some(origin));
        assert_eq!(head.target, path);
        assert_eq!(head.host, "other.example");
        assert_eq!(head.last_event_id, Some("opaque-id"));
    }
    rejected(
        b"GET / HTTP/1.1\r\nHost: mail.example\r\nLast-Event-ID: one\r\nLast-Event-ID: two\r\n\r\n",
    );
}
