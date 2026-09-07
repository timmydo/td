#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

#[test]
fn received_credential_is_immutable_and_offset_independent() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    prepare_receiver(&receiver).unwrap();
    let mut file = create_credential().unwrap();
    file.write_all(b"credential bytes").unwrap();
    assert!(require_sealed(&file).is_err());
    seal_credential(&file).unwrap();
    assert!(file.write_all(b"changed").is_err());
    assert!(file.set_len(0).is_err());
    send_descriptor(&sender, b"frame", &file).unwrap();
    let mut bytes = [0; 5];
    let (count, received) = receive(&receiver, &mut bytes).unwrap();
    assert_eq!(count, 5);
    assert_eq!(&bytes, b"frame");
    assert_eq!(received.credentials.uid, peer_uid(&sender).unwrap());
    alive(received.pidfd.as_fd()).unwrap();
    let received = received.descriptor.unwrap();
    require_sealed(&received).unwrap();
    let mut secret = [0; 16];
    received.read_exact_at(&mut secret, 0).unwrap();
    assert_eq!(&secret, b"credential bytes");
    drop(file);
    received.read_exact_at(&mut secret, 0).unwrap();
}

#[test]
fn every_fragment_has_a_sender_but_only_one_has_the_descriptor() {
    let (sender, receiver) = UnixStream::pair().unwrap();
    prepare_receiver(&receiver).unwrap();
    let file = create_credential().unwrap();
    seal_credential(&file).unwrap();
    send_descriptor(&sender, b"abcdef", &file).unwrap();
    let mut byte = [0];
    let mut descriptors = 0;
    for expected in b"abcdef" {
        let (count, received) = receive(&receiver, &mut byte).unwrap();
        assert_eq!(count, 1);
        assert_eq!(byte, [*expected]);
        alive(received.pidfd.as_fd()).unwrap();
        descriptors += usize::from(received.descriptor.is_some());
    }
    assert_eq!(descriptors, 1);
}

#[test]
fn missing_sender_and_extra_rights_are_refused_after_ownership() {
    use std::io::Read;
    let (mut first, first_right) = UnixStream::pair().unwrap();
    let (mut second, second_right) = UnixStream::pair().unwrap();
    let rights = vec![OwnedFd::from(first_right), OwnedFd::from(second_right)];
    let (mut third, receiver) = UnixStream::pair().unwrap();
    let fake_pidfd = OwnedFd::from(receiver);
    assert!(admit(1, 128, 0, (Some(Credentials { pid: 1, uid: 1000, gid: 1000 }), vec![fake_pidfd], rights, true)).is_err());
    for peer in [&mut first, &mut second, &mut third] {
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "refusal leaked an installed owner");
    }
    assert!(admit(1, 0, 0, (None, vec![], vec![], true)).is_err());
    assert!(require_sealed(&File::open("/dev/null").unwrap()).is_err());
}
