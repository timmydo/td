#![allow(clippy::unwrap_used)]
use super::*;
use std::{
    io::{Read, Write},
    net::{Ipv6Addr, Shutdown, SocketAddrV6, TcpStream},
    time::Duration,
};

#[test]
#[allow(deprecated)]
fn wildcard_ipv6_is_separate_from_ipv4_and_rebinds_after_use() {
    let v6 = bind("[::]:0".parse().unwrap()).unwrap();
    assert!(v6.only_v6().unwrap());
    let port = v6.local_addr().unwrap().port();
    let v4 = bind(SocketAddr::from(([0, 0, 0, 0], port))).unwrap();
    for (listener, address, family) in [
        (&v6, SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port), true),
        (&v4, SocketAddr::from(([127, 0, 0, 1], port)), false),
    ] {
        let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        let (mut accepted, peer) = listener.accept().unwrap();
        assert_eq!(peer.is_ipv6(), family);
        accepted
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client.write_all(b"mail").unwrap();
        let mut bytes = [0; 4];
        accepted.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"mail");
        accepted.shutdown(Shutdown::Both).unwrap();
    }
    drop(v4);
    assert!(TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(1)
    )
    .is_err());
    drop(v6);
    let replacement = bind(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port)).unwrap();
    assert!(replacement.only_v6().unwrap());
}

#[test]
fn scoped_flow_and_mapped_binds_are_refused() {
    for address in [
        SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 0, 1),
        SocketAddrV6::new(Ipv6Addr::LOCALHOST, 0, 1, 0),
        SocketAddrV6::new("::ffff:127.0.0.1".parse().unwrap(), 0, 0, 0),
    ] {
        assert_eq!(
            bind(address.into()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
