#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use crate::store_fs::tests::Fixture;
const ACCOUNT: AccountId = AccountId::from_bytes([7; 16]);
struct Random(u8);
impl Entropy for Random {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
        self.0 = self.0.wrapping_add(1);
        output.fill(self.0);
        Ok(())
    }
}
#[test]
fn credentials_survive_restart_and_revocation_without_plaintext() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    assert!(DeviceStore::open(&mut root, ACCOUNT).is_err());
    let mut store = DeviceStore::open_or_create(&mut root, ACCOUNT).unwrap();
    let (id, password) = store.create("laptop α", 10, &mut Random(0)).unwrap();
    assert_eq!(store.authenticate(password.expose()).unwrap(), Some(id));
    let bytes = fs::read(fixture.path.join("records.v1")).unwrap();
    assert!(!bytes.windows(32).any(|part| part == [3; 32]));
    assert!(!bytes
        .windows(64)
        .any(|part| part == &password.expose()[33..]));
    let mut bad = password.expose().to_vec();
    bad[96] = b'0';
    assert_eq!(store.authenticate(&bad).unwrap(), None);
    for bad in [b"".as_slice(), b"bad", &[b'0'; 97]] {
        assert_eq!(store.authenticate(bad).unwrap(), None);
    }
    drop(store);
    assert!(DeviceStore::open(&mut root, AccountId::from_bytes([8; 16])).is_err());
    let mut store = DeviceStore::open(&mut root, ACCOUNT).unwrap();
    assert_eq!(store.authenticate(password.expose()).unwrap(), Some(id));
    store.revoke(id, 20).unwrap();
    store.revoke(id, 30).unwrap();
    assert_eq!(store.authenticate(password.expose()).unwrap(), None);
    drop(store);
    let store = DeviceStore::open(&mut root, ACCOUNT).unwrap();
    assert_eq!(store.authenticate(password.expose()).unwrap(), None);
    assert_eq!(store.records[0].revoked, Some(20));
    assert_eq!(store.records[0].label, "laptop α");
}
#[test]
fn failed_publication_retires_owner_and_restart_retains_old_authority() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let mut store = DeviceStore::open_or_create(&mut root, ACCOUNT).unwrap();
    let (id, password) = store.create("phone", 1, &mut Random(0)).unwrap();
    let pending = fixture.path.join("records.v1.pending");
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&pending)
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    assert_eq!(store.revoke(id, 2), Err(DeviceError::Unavailable));
    assert_eq!(
        store.authenticate(password.expose()),
        Err(DeviceError::Unavailable)
    );
    assert!(store.create("other", 3, &mut Random(5)).is_err());
    drop(store);
    let store = DeviceStore::open(&mut root, ACCOUNT).unwrap();
    assert_eq!(store.authenticate(password.expose()).unwrap(), Some(id));
    assert!(!pending.exists());
}
#[test]
fn damaged_and_untrusted_registry_files_fail_closed() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    drop(DeviceStore::open_or_create(&mut root, ACCOUNT).unwrap());
    let path = fixture.path.join("records.v1");
    let bytes = fs::read(&path).unwrap();
    for end in 0..bytes.len() {
        fs::write(&path, &bytes[..end]).unwrap();
        assert!(DeviceStore::open(&mut root, ACCOUNT).is_err());
    }
    fs::write(&path, &bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(DeviceStore::open(&mut root, ACCOUNT).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, fixture.path.join("alias")).unwrap();
    assert!(DeviceStore::open(&mut root, ACCOUNT).is_err());
    fs::remove_file(fixture.path.join("alias")).unwrap();
    fs::rename(&path, fixture.path.join("original")).unwrap();
    std::os::unix::fs::symlink("original", &path).unwrap();
    assert!(DeviceStore::open(&mut root, ACCOUNT).is_err());
}
#[test]
fn generation_failures_and_limits_do_not_publish() {
    struct Broken;
    impl Entropy for Broken {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
            output.fill(99);
            Err(td_crypto::Error::Entropy)
        }
    }
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let mut store = DeviceStore::open_or_create(&mut root, ACCOUNT).unwrap();
    let path = fixture.path.join("records.v1");
    let before = fs::read(&path).unwrap();
    assert!(store.create("phone", 0, &mut Broken).is_err());
    for label in ["", "bad\nlabel", &"x".repeat(129)] {
        assert!(store.create(label, 0, &mut Random(0)).is_err());
    }
    assert_eq!(fs::read(&path).unwrap(), before);
    let (_, password) = store.create("phone", 0, &mut Random(0)).unwrap();
    let before = fs::read(&path).unwrap();
    assert!(store.create("duplicate", 0, &mut Random(0)).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(store.authenticate(password.expose()).unwrap().is_some());
    let sample = store.records[0].clone();
    store.records.resize(DEVICE_CAPACITY, sample);
    assert!(matches!(
        store.create("overflow", 0, &mut Random(10)),
        Err(DeviceError::Capacity)
    ));
}

#[test]
fn uncertain_rename_reopens_revoked_and_retry_is_durable() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let mut store = DeviceStore::open_or_create(&mut root, ACCOUNT).unwrap();
    let (id, password) = store.create("phone", 1, &mut Random(0)).unwrap();
    store.fail_after_rename = true;
    assert_eq!(store.revoke(id, 2), Err(DeviceError::Unavailable));
    assert_eq!(
        store.authenticate(password.expose()),
        Err(DeviceError::Unavailable)
    );
    drop(store);
    let mut store = DeviceStore::open(&mut root, ACCOUNT).unwrap();
    store.revoke(id, 3).unwrap();
    assert_eq!(store.authenticate(password.expose()).unwrap(), None);
    assert_eq!(store.records[0].revoked, Some(2));
    assert_eq!(store.device_ids().unwrap().collect::<Vec<_>>(), vec![id]);
}

#[test]
fn canonical_tokens_and_checksummed_structural_corruption_are_refused() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let mut store = DeviceStore::open_or_create(&mut root, ACCOUNT).unwrap();
    let (_, password) = store.create("phone", 1, &mut Random(0xaa)).unwrap();
    let mut upper = password.expose().to_vec();
    upper[..32].make_ascii_uppercase();
    assert_eq!(store.authenticate(&upper).unwrap(), None);
    upper.copy_from_slice(password.expose());
    upper[33..].make_ascii_uppercase();
    assert_eq!(store.authenticate(&upper).unwrap(), None);
    let original = encode(&store.records, ACCOUNT).unwrap();
    let recheck = |mut bytes: Vec<u8>| {
        bytes.truncate(bytes.len() - 32);
        bytes.extend_from_slice(&checksum(&bytes).unwrap());
        assert!(decode(&bytes, ACCOUNT).is_err());
    };
    for (offset, byte) in [(0, b'X'), (24, 2), (114, 2), (122, 1), (125, b'\n')] {
        let mut bytes = original.clone();
        bytes[offset] = byte;
        recheck(bytes);
    }
    let mut trailing = original.clone();
    trailing.insert(trailing.len() - 32, 0);
    recheck(trailing);
    let duplicate = vec![store.records[0].clone(); 2];
    assert!(decode(&encode(&duplicate, ACCOUNT).unwrap(), ACCOUNT).is_err());
    let mut full = Vec::new();
    for n in 0..DEVICE_CAPACITY {
        let mut record = store.records[0].clone();
        record.id = DeviceId::from_bytes((n as u128).to_be_bytes());
        record.label = "x".repeat(DEVICE_LABEL_BYTES);
        full.push(record);
    }
    let bytes = encode(&full, ACCOUNT).unwrap();
    assert_eq!(bytes.len(), 58170);
    assert_eq!(decode(&bytes, ACCOUNT).unwrap().len(), DEVICE_CAPACITY);
}
