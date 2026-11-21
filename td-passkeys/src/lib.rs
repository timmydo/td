//! The device-bound passkey store used by td-pass.
#![forbid(unsafe_code)]
mod credential;
mod vault;

pub use credential::{Account, Approval};
use std::io;
pub use vault::{Enrollment, Vault};

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Invalid,
    Refused,
    Locked,
    Full,
    Missing,
    Excluded,
    Stale,
    Capacity,
}
type Result<T> = std::result::Result<T, Error>;

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<td_alloc::Error> for Error {
    fn from(_: td_alloc::Error) -> Self {
        Self::Capacity
    }
}
impl From<td_primitives::Refused> for Error {
    fn from(_: td_primitives::Refused) -> Self {
        Self::Refused
    }
}

struct Secret<const N: usize>([u8; N]);
impl<const N: usize> Drop for Secret<N> {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}
struct Plain(Vec<u8>);
impl Drop for Plain {
    fn drop(&mut self) {
        td_tpm::zero(&mut self.0);
    }
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    let (head, tail) = input.split_at_checked(n).ok_or(Error::Invalid)?;
    *input = tail;
    Ok(head)
}
fn field<'a>(input: &mut &'a [u8], max: usize) -> Result<&'a [u8]> {
    let n = u16::from_be_bytes(take(input, 2)?.try_into().map_err(|_| Error::Invalid)?) as usize;
    if n > max {
        return Err(Error::Invalid);
    }
    take(input, n)
}
fn put(out: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    let n = u16::try_from(data.len()).map_err(|_| Error::Invalid)?;
    td_alloc::extend_from_slice(out, &n.to_be_bytes())?;
    td_alloc::extend_from_slice(out, data)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use td_fido::{
        digest,
        fido_cbor::{decode, Encoder, Value},
        web_request::{parse, Request},
    };
    use td_primitives::ec::p256;
    use td_protector::{lockout::DA_PARAMETERS, pin::Pin};
    use td_tpm::{Client, LockoutAuth, Transport};

    const SITE: &str = "accounts.example";
    const USER: &[u8] = b"opaque-user-handle";

    fn registration() -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(5, 6).unwrap();
        e.head(0, 1).unwrap();
        e.bytes(&digest(
            br#"{"type":"webauthn.create","challenge":"AQ","origin":"https://accounts.example"}"#,
        ))
        .unwrap();
        e.head(0, 2).unwrap();
        e.head(5, 1).unwrap();
        e.text("id").unwrap();
        e.text(SITE).unwrap();
        e.head(0, 3).unwrap();
        e.head(5, 2).unwrap();
        e.text("id").unwrap();
        e.bytes(USER).unwrap();
        e.text("name").unwrap();
        e.text("Alice").unwrap();
        e.head(0, 4).unwrap();
        e.head(4, 1).unwrap();
        e.head(5, 2).unwrap();
        e.text("alg").unwrap();
        e.head(1, 6).unwrap();
        e.text("type").unwrap();
        e.text("public-key").unwrap();
        e.head(0, 6).unwrap();
        e.head(5, 1).unwrap();
        e.text("hmac-secret").unwrap();
        e.boolean(false).unwrap();
        e.head(0, 7).unwrap();
        e.head(5, 2).unwrap();
        e.text("rk").unwrap();
        e.boolean(true).unwrap();
        e.text("uv").unwrap();
        e.boolean(true).unwrap();
        let mut packet = vec![1];
        packet.extend(e.finish().unwrap());
        packet
    }

    fn get(site: &str, hash: &[u8; 32]) -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(5, 3).unwrap();
        e.head(0, 1).unwrap();
        e.text(site).unwrap();
        e.head(0, 2).unwrap();
        e.bytes(hash).unwrap();
        e.head(0, 5).unwrap();
        e.head(5, 1).unwrap();
        e.text("uv").unwrap();
        e.boolean(true).unwrap();
        let mut packet = vec![2];
        packet.extend(e.finish().unwrap());
        packet
    }

    fn probe(site: &str, hash: &[u8; 32], id: &[u8; 32]) -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(5, 4).unwrap();
        e.head(0, 1).unwrap();
        e.text(site).unwrap();
        e.head(0, 2).unwrap();
        e.bytes(hash).unwrap();
        e.head(0, 3).unwrap();
        e.head(4, 1).unwrap();
        e.head(5, 2).unwrap();
        e.text("id").unwrap();
        e.bytes(id).unwrap();
        e.text("type").unwrap();
        e.text("public-key").unwrap();
        e.head(0, 5).unwrap();
        e.head(5, 1).unwrap();
        e.text("up").unwrap();
        e.boolean(false).unwrap();
        let mut packet = vec![2];
        packet.extend(e.finish().unwrap());
        packet
    }
    fn key_from_registration(reply: &[u8]) -> ([u8; 32], Vec<u8>) {
        let att = decode(reply).unwrap();
        let auth = att.required(&Value::Unsigned(2)).unwrap().bytes().unwrap();
        assert_eq!(&auth[..32], &digest(SITE.as_bytes()));
        assert_eq!(auth[32], 0x45);
        assert_eq!(u16::from_be_bytes(auth[53..55].try_into().unwrap()), 32);
        let cose = decode(&auth[87..]).unwrap();
        let mut public = vec![4];
        for axis in [1, 2] {
            public.extend(
                cose.required(&Value::Negative(axis))
                    .unwrap()
                    .bytes()
                    .unwrap(),
            );
        }
        (auth[55..87].try_into().unwrap(), public)
    }

    fn verify(reply: &[u8], public: &[u8], hash: &[u8; 32]) {
        let response = decode(reply).unwrap();
        let auth = response
            .required(&Value::Unsigned(2))
            .unwrap()
            .bytes()
            .unwrap();
        let sig = response
            .required(&Value::Unsigned(3))
            .unwrap()
            .bytes()
            .unwrap();
        assert_eq!(auth.len(), 37);
        assert_eq!(&auth[..32], &digest(SITE.as_bytes()));
        assert_eq!(auth[32], 5);
        let mut message = auth.to_vec();
        message.extend(hash);
        p256::verify(public, &digest(&message), sig).unwrap();
        *message.last_mut().unwrap() ^= 0x80;
        assert!(p256::verify(public, &digest(&message), sig).is_err());
    }

    #[test]
    fn software_passkey_requires_approval_and_signs_for_its_registered_site() {
        let packet = registration();
        let Request::Create(r) = parse(&packet).unwrap() else {
            panic!("registration")
        };
        assert!(credential::make(&r, Approval::Cancelled).is_err());
        assert!(credential::make(&r, Approval::Present).is_err());
        let (record, reply) = credential::make(&r, Approval::Verified).unwrap();
        let (id, public) = key_from_registration(&reply);
        let cred = credential::Credential::read(&record.0).unwrap();
        assert_eq!(&id, cred.account.id);
        for (site, hash, handle, ok) in [
            (SITE, digest(b""), id, true),
            (SITE, digest(b"another challenge"), id, false),
            ("other.example", digest(b""), id, false),
            (SITE, digest(b""), [0; 32], false),
        ] {
            let packet = probe(site, &hash, &handle);
            let Request::Get(probe) = parse(&packet).unwrap() else {
                panic!("probe")
            };
            let result = cred.preflight(&probe);
            assert_eq!(result.is_ok(), ok);
            if let Ok(reply) = result {
                let reply = decode(&reply).unwrap();
                assert!(reply.get(&Value::Unsigned(4)).unwrap().is_none());
                let auth = reply
                    .required(&Value::Unsigned(2))
                    .unwrap()
                    .bytes()
                    .unwrap();
                assert_eq!(auth[32], 0);
                let mut message = auth.to_vec();
                message.extend(hash);
                p256::verify(
                    &public,
                    &digest(&message),
                    reply
                        .required(&Value::Unsigned(3))
                        .unwrap()
                        .bytes()
                        .unwrap(),
                )
                .unwrap();
            }
        }
        let mut packet = registration();
        // Keep rk=true and omit uv.
        packet.truncate(packet.len() - 4);
        let options = packet.len() - 5;
        packet[options] = 0xa1;
        let Request::Create(r) = parse(&packet).unwrap() else {
            panic!("registration")
        };
        assert!(r.discoverable() && !r.verify_user());
        assert!(credential::make(&r, Approval::Present).is_err());
        let hash = digest(b"fresh client data");
        let packet = get(SITE, &hash);
        let Request::Get(r) = parse(&packet).unwrap() else {
            panic!("assertion")
        };
        verify(
            &cred.assertion(&r, Approval::Verified).unwrap(),
            &public,
            &hash,
        );
        assert!(cred.assertion(&r, Approval::Present).is_err());
        let other = get("other.example", &hash);
        let Request::Get(r) = parse(&other).unwrap() else {
            panic!("assertion")
        };
        assert!(cred.assertion(&r, Approval::Verified).is_err());
    }

    struct Socket(UnixStream);
    impl Transport for Socket {
        fn exchange(&mut self, request: &[u8]) -> std::result::Result<Vec<u8>, String> {
            self.0.write_all(request).map_err(|e| e.to_string())?;
            let mut header = [0; 10];
            self.0.read_exact(&mut header).map_err(|e| e.to_string())?;
            let len = u32::from_be_bytes(header[2..6].try_into().unwrap()) as usize;
            assert!((10..=td_tpm::MAX_PACKET).contains(&len));
            let mut reply = header.to_vec();
            reply.resize(len, 0);
            self.0
                .read_exact(&mut reply[10..])
                .map_err(|e| e.to_string())?;
            Ok(reply)
        }
    }
    struct Process(Child);
    impl Drop for Process {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "requires pinned swtpm 0.10.1"]
    fn emulator_enroll_register_reopen_sign_and_delete_without_a_security_key() {
        let temp = td_fs::TempDir::new("td-passkeys").unwrap();
        let socket = temp.path().join("socket");
        let version = Command::new("swtpm").arg("--version").output().unwrap();
        assert!(
            version.status.success() && version.stdout.starts_with(b"TPM emulator version 0.10.1,")
        );
        let mut proc = Process(
            Command::new("swtpm")
                .args(["socket", "--tpm2", "--tpmstate"])
                .arg(format!("dir={},mode=0600", temp.path().display()))
                .arg("--server")
                .arg(format!("type=unixio,path={}", socket.display()))
                .args(["--flags", "not-need-init,startup-clear"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&socket).is_err() {
            assert!(proc.0.try_wait().unwrap().is_none() && Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let client = || {
            let io = UnixStream::connect(&socket).unwrap();
            io.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            io.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
            Client::new(Socket(io))
        };
        let name = client().storage_primary(None).unwrap().1;
        let path = temp.path().join("vault");
        td_fs::private_dir(&path).unwrap();
        let dir = File::open(&path).unwrap();
        let pin = Pin::parse(b"test PIN").unwrap();
        assert!(Vault::enroll(&dir, client(), &name, &pin, Enrollment::Cancelled).is_err());
        assert!(!path.join("passkeys").exists());
        assert!(Vault::enroll(
            &dir,
            client(),
            &name,
            &pin,
            Enrollment::AcceptLossOfOriginalTpm
        )
        .is_err());
        let auth = LockoutAuth::generate().unwrap();
        client().take_lockout(&name, &auth).unwrap();
        client()
            .set_dictionary_attack(&name, &auth, DA_PARAMETERS)
            .unwrap();
        let mut vault = Vault::enroll(
            &dir,
            client(),
            &name,
            &pin,
            Enrollment::AcceptLossOfOriginalTpm,
        )
        .unwrap();
        let request = registration();
        let Request::Create(r) = parse(&request).unwrap() else {
            panic!("registration")
        };
        let (id, public) = key_from_registration(&vault.register(&r, Approval::Verified).unwrap());
        assert!(matches!(
            Vault::unlock(&dir, client(), &name, &pin),
            Err(Error::Locked)
        ));
        drop(vault);
        let file = path.join("passkeys");
        let blob = std::fs::read(&file).unwrap();
        for plaintext in [USER, SITE.as_bytes(), b"Alice", &id] {
            assert!(!blob.windows(plaintext.len()).any(|w| w == plaintext));
        }
        assert!(Vault::enroll(
            &dir,
            client(),
            &name,
            &pin,
            Enrollment::AcceptLossOfOriginalTpm
        )
        .is_err());
        assert!(Vault::unlock(&dir, client(), &name, &Pin::parse(b"wrong PIN").unwrap()).is_err());
        assert_eq!(client().dictionary_attack().unwrap().counter, 1);
        let mut vault = Vault::unlock(&dir, client(), &name, &pin).unwrap();
        let hash = digest(
            br#"{"type":"webauthn.get","origin":"https://accounts.example","challenge":"Ag"}"#,
        );
        let request = get(SITE, &hash);
        let Request::Get(r) = parse(&request).unwrap() else {
            panic!("assertion")
        };
        let mut accounts = Vec::new();
        vault
            .list(|account| {
                accounts.push(*account.id);
                Ok(())
            })
            .unwrap();
        assert_eq!(accounts, vec![id]);
        verify(
            &vault.sign(&r, &id, Approval::Verified).unwrap(),
            &public,
            &hash,
        );
        vault.delete(&id).unwrap();
        drop(vault);
        let vault = Vault::unlock(&dir, client(), &name, &pin).unwrap();
        assert!(matches!(
            vault.sign(&r, &id, Approval::Verified),
            Err(Error::Missing)
        ));
        drop(vault);
        let mut damaged = blob;
        *damaged.last_mut().unwrap() ^= 1;
        std::fs::write(file, damaged).unwrap();
        assert!(Vault::unlock(&dir, client(), &name, &pin).is_err());
    }
}
