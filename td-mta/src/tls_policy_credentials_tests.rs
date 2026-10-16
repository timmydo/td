use super::super::relay_credentials_digest;
use super::*;
use crate::ports::{IoProgress, Transport};

fn configuration(password: &str) -> ResolvedText {
    let _lock = text::TEST_CONSTRUCTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut scratch = vec![0; stream::SCRATCH_BYTES];
    let source = source(false);
    let loaded = load::read(
        storage::Storage::try_new().unwrap(),
        &mut stanza::Pending::new(),
        &mut scratch,
        &mut Cursor::new(source.as_bytes()),
    )
    .unwrap();
    materialize::read_text(loaded, &mut scratch, |_| {
        Ok::<_, Error>(Cursor::new(password.as_bytes()))
    })
    .unwrap()
}

#[test]
fn relay_credential_digest_is_framed_exact_and_bounded() {
    assert_eq!(
        relay_credentials_digest("private-relay", "password").unwrap(),
        [
            88, 2, 88, 209, 113, 94, 128, 118, 22, 49, 163, 105, 137, 207, 231, 244, 131, 226, 100,
            177, 97, 219, 0, 156, 204, 155, 215, 52, 144, 48, 175, 136
        ]
    );
    assert_ne!(
        relay_credentials_digest("ab", "c").unwrap(),
        relay_credentials_digest("a", "bc").unwrap()
    );
    let reference = relay_credentials_digest("user", "secret").unwrap();
    for (username, password) in [
        ("User", "secret"),
        ("user ", "secret"),
        ("user", "Secret"),
        ("user", "secret "),
    ] {
        assert_ne!(
            relay_credentials_digest(username, password).unwrap(),
            reference
        );
    }
    let username = "u".repeat(outbound::MAX_USERNAME_BYTES);
    let password = "p".repeat(crate::config::material::MAX_PASSWORD_BYTES);
    assert!(relay_credentials_digest(&username, &password).is_ok());
    assert_eq!(
        relay_credentials_digest(&(username + "u"), "password"),
        Err(Error::Invalid)
    );
    assert_eq!(
        relay_credentials_digest("user", &(password + "p")),
        Err(Error::Invalid)
    );
    for (username, password) in [
        ("", "secret"),
        ("user", ""),
        ("user\0", "secret"),
        ("user", "sec\tret"),
        ("user", "secret\r\n"),
        ("usér", "secret"),
    ] {
        assert_eq!(
            relay_credentials_digest(username, password),
            Err(Error::Invalid)
        );
    }
}

#[test]
fn relay_credentials_match_the_retained_policy_across_rotation_without_output() {
    let _serial = serial();
    let material = Material::new();
    let mut set = GenerationSet::at_startup();
    let publish = |set: &mut GenerationSet<TlsPolicies>, password: &str| {
        let prepared = TlsPolicies::prepare(
            set.reserve().unwrap(),
            &configuration(password),
            material.clock.clone(),
            |request| material.open(request),
        )
        .unwrap();
        drop(set.publish(prepared).unwrap());
        set.current().unwrap()
    };
    let old = publish(&mut set, "old-password");
    let pool = HandshakePool::new(2).unwrap();
    let (mut old_client, mut old_peer) = io_tests::pair(&material, &old, &pool, "smtp");
    assert_eq!(
        old_client.check_relay_credentials("private-relay", "old-password"),
        Err(Error::Tls)
    );
    io_tests::complete(&mut old_client, &mut old_peer).unwrap();
    let new = publish(&mut set, "new-password");
    let (mut new_client, mut new_peer) = io_tests::pair(&material, &new, &pool, "smtp");
    io_tests::complete(&mut new_client, &mut new_peer).unwrap();
    for (client, right, wrong) in [
        (&old_client, "old-password", "new-password"),
        (&new_client, "new-password", "old-password"),
    ] {
        assert_eq!(
            client.check_relay_credentials("private-relay", right),
            Ok(())
        );
        assert_eq!(
            client.check_relay_credentials("private-relay", wrong),
            Err(Error::Forbidden)
        );
        assert_eq!(
            client.check_relay_credentials("other-relay", right),
            Err(Error::Forbidden)
        );
        assert_eq!(
            client.check_relay_credentials("private-relay", "bad\nsecret"),
            Err(Error::Invalid)
        );
    }
    assert_eq!(
        old_peer.check_relay_credentials("private-relay", "old-password"),
        Err(Error::Tls)
    );
    for _ in 0..32 {
        assert_eq!(old_peer.read(&mut [0; 16]).unwrap(), IoProgress::Pending);
        assert_eq!(new_peer.read(&mut [0; 16]).unwrap(), IoProgress::Pending);
    }
    drop(old_client.into_buffers().unwrap());
    drop(old_peer.into_buffers().unwrap());
    drop(new_client.into_buffers().unwrap());
    drop(new_peer.into_buffers().unwrap());
}

#[test]
fn relay_credentials_never_accept_a_verified_acme_connection() {
    let _serial = serial();
    let material = Material::new();
    let lease = material.published(&client_tests::acme_source());
    let pool = HandshakePool::new(2).unwrap();
    let (mut client, mut peer) = io_tests::pair_for_policy(
        &material,
        &lease,
        &pool,
        TlsPolicies::acme(&lease).unwrap(),
        "smtp",
    );
    io_tests::complete(&mut client, &mut peer).unwrap();
    assert_eq!(
        client.check_relay_credentials("private-relay", "password"),
        Err(Error::Forbidden)
    );
    drop(client.into_buffers().unwrap());
    drop(peer.into_buffers().unwrap());
}
