use super::*;
use std::{
    io::{Read, Write},
    time::Duration,
};

#[test]
fn set_observes_wake_listener_and_stream_without_consuming_input() -> io::Result<()> {
    let (wake, mut signal) = UnixStream::pair()?;
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let (mut stream, mut writer) = UnixStream::pair()?;
    let until = || Instant::now() + Duration::from_secs(1);
    let interest = Interest {
        read: true,
        write: false,
    };
    let mut poll = PollSet::new(&wake);
    poll.add_tcp_listener(&listener)?;
    poll.add_connected(Connected::Unix(&stream), interest)?;
    let mut byte = [0; 1];
    let client = TcpStream::connect(listener.local_addr()?)?;
    poll.wait(until())?;
    let [wake_fd, listener_fd, stream_fd, ..] = &poll.fds;
    assert_eq!(wake_fd.revents, 0);
    assert_ne!(listener_fd.revents & IN, 0);
    assert_eq!(stream_fd.revents, 0);
    let (_accepted, peer) = listener.accept()?;
    assert_eq!(peer, client.local_addr()?);

    writer.write_all(b"a")?;
    signal.write_all(b"b")?;
    poll.wait(until())?;
    let [wake_fd, listener_fd, stream_fd, ..] = &poll.fds;
    assert_ne!(wake_fd.revents & IN, 0);
    assert_eq!(listener_fd.revents, 0);
    assert_ne!(stream_fd.revents & IN, 0);
    stream.read_exact(&mut byte)?;
    assert_eq!(&byte, b"a");
    Ok(())
}

#[test]
fn set_waits_through_idle_deadline_and_rejects_size_overrun() -> io::Result<()> {
    let (wake, _sender) = UnixStream::pair()?;
    let (socket, _peer) = UnixStream::pair()?;
    let mut poll = PollSet::new(&wake);
    let interest = Interest {
        read: true,
        write: false,
    };
    for _ in 1..HTTPS_FDS {
        poll.add_connected(Connected::Unix(&socket), interest)?;
    }
    assert_eq!(
        poll.add_connected(Connected::Unix(&socket), interest)
            .err()
            .map(|e| e.kind()),
        Some(io::ErrorKind::InvalidInput)
    );
    let until = Instant::now() + Duration::from_millis(8);
    poll.wait(until)?;
    assert!(Instant::now() >= until);
    Ok(())
}
