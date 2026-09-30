#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use std::{
    net::TcpListener,
    time::{Duration, Instant},
};

fn pair() -> (TcpTransport, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    let (server, _) = listener.accept().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    server
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let adapter = TcpTransport::from_stream(client).unwrap();
    assert_eq!(adapter.peer_addr(), address);
    assert!(adapter.socket.as_ref().unwrap().nodelay().unwrap());
    (adapter, server)
}

fn progress(mut step: impl FnMut() -> IoProgress) -> IoProgress {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let result = step();
        if result != IoProgress::Pending {
            return result;
        }
        assert!(Instant::now() < until, "local socket did not progress");
        std::thread::yield_now();
    }
}

#[test]
fn tcp_bounded_io_actual_peer_and_unbuffered_flush() {
    let (mut adapter, mut peer) = pair();
    let mut empty = [];
    assert_eq!(adapter.read(&mut empty).unwrap(), IoProgress::Pending);
    assert_eq!(adapter.write(&[]).unwrap(), IoProgress::Pending);
    assert_eq!(adapter.read(&mut [0; 16]).unwrap(), IoProgress::Pending);
    assert_eq!(adapter.flush().unwrap(), FlushProgress::Complete);
    let payload = [0x5a; SOCKET_CHUNK + 1];
    let IoProgress::Bytes(count) = progress(|| adapter.write(&payload).unwrap()) else {
        panic!("nonempty write must consume bytes");
    };
    assert!(count > 0 && count <= SOCKET_CHUNK);
    let mut received = [0; SOCKET_CHUNK];
    peer.read_exact(&mut received[..count]).unwrap();
    assert_eq!(&received[..count], &payload[..count]);
    assert_eq!(adapter.flush().unwrap(), FlushProgress::Complete);

    peer.write_all(&payload).unwrap();
    let mut received = [0; SOCKET_CHUNK + 1];
    let mut used = 0;
    while used < received.len() {
        let IoProgress::Bytes(count) = progress(|| adapter.read(&mut received[used..]).unwrap())
        else {
            panic!("peer did not send its complete payload");
        };
        assert!(count > 0 && count <= SOCKET_CHUNK);
        used += count;
    }
    assert_eq!(received, payload);
    assert!(std::mem::size_of::<TcpTransport>() <= 128);
}

#[test]
fn tcp_peer_eof_preserves_write_half_and_local_close_preserves_read_half() {
    let (mut adapter, mut peer) = pair();
    peer.shutdown(Shutdown::Write).unwrap();
    assert_eq!(
        progress(|| adapter.read(&mut [0]).unwrap()),
        IoProgress::Closed
    );
    assert_eq!(adapter.read(&mut [0]).unwrap(), IoProgress::Closed);
    assert_eq!(
        progress(|| adapter.write(b"ok").unwrap()),
        IoProgress::Bytes(2)
    );
    let mut received = [0; 2];
    peer.read_exact(&mut received).unwrap();
    assert_eq!(&received, b"ok");
    assert_eq!(adapter.close().unwrap(), FlushProgress::Complete);
    assert_eq!(adapter.close().unwrap(), FlushProgress::Complete);
    assert_eq!(adapter.flush().unwrap(), FlushProgress::Complete);
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);

    let (mut adapter, mut peer) = pair();
    assert_eq!(adapter.close().unwrap(), FlushProgress::Complete);
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    peer.write_all(b"x").unwrap();
    let mut output = [0];
    assert_eq!(
        progress(|| adapter.read(&mut output).unwrap()),
        IoProgress::Bytes(1)
    );
    assert_eq!(&output, b"x");
    let error = Error::Io {
        kind: ErrorKind::BrokenPipe,
        os_code: None,
    };
    assert_eq!(adapter.write(b"late"), Err(error));
    assert_eq!(adapter.read(&mut output), Err(error));
    assert_eq!(adapter.close(), Err(error));
}

#[test]
fn tcp_abort_drop_and_errors_fence_both_directions() {
    let (mut adapter, mut peer) = pair();
    adapter.abort();
    adapter.abort();
    assert_eq!(adapter.read(&mut [0]), Err(Error::Invalid));
    assert_eq!(adapter.write(b"x"), Err(Error::Invalid));
    assert_eq!(adapter.flush(), Err(Error::Invalid));
    assert_eq!(adapter.close(), Err(Error::Invalid));
    assert_eq!(adapter.read(&mut []), Ok(IoProgress::Pending));
    assert_eq!(adapter.write(&[]), Ok(IoProgress::Pending));
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    let (adapter, mut peer) = pair();
    drop(adapter);
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);

    for reading in [false, true] {
        for kind in [ErrorKind::Interrupted, ErrorKind::WouldBlock] {
            let (mut adapter, _peer) = pair();
            assert_eq!(
                adapter.progress(Err(io::Error::from(kind)), reading),
                Ok(IoProgress::Pending)
            );
            assert!(adapter.socket.is_some());
            assert_eq!(adapter.flush(), Ok(FlushProgress::Complete));
        }
        let (mut adapter, mut peer) = pair();
        let injected = io::Error::from_raw_os_error(104);
        let error = Error::Io {
            kind: injected.kind(),
            os_code: injected.raw_os_error(),
        };
        assert_eq!(adapter.progress(Err(injected), reading), Err(error));
        assert!(adapter.socket.is_none());
        adapter.abort();
        assert_eq!(adapter.write(b"x"), Err(error));
        assert_eq!(adapter.read(&mut [0]), Err(error));
        assert_eq!(adapter.flush(), Err(error));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    }
    let (mut adapter, _peer) = pair();
    let error = Error::Io {
        kind: ErrorKind::WriteZero,
        os_code: None,
    };
    assert_eq!(adapter.progress(Ok(0), false), Err(error));
    assert_eq!(adapter.flush(), Err(error));
}
