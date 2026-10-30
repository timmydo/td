//! The private td notebook worker. Only its root-created authority endpoint
//! carries requests; it has no listener and no ordinary-account fallback.

use super::host::{self, Entropy};
use super::lifecycle::{self, Directory, Session, Tokens};
use super::notebook::{Change, Text};
use super::{MailBinding, MAX_BODY, MAX_CREDENTIAL, MAX_ENVELOPE, MAX_PASSWORD, MAX_TITLE};
use crate::notebook_wire::{field, Bytes, Reader, Wire, IDLE};
use std::io::{self, Read};
use std::time::{Duration, Instant};

const SESSION_TIME: Duration = Duration::from_secs(15 * 60);

struct Random(Entropy);
impl Read for Random {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.0.fill(bytes).map_err(io::Error::other)?;
        Ok(bytes.len())
    }
}

struct Backend {
    directory: Directory,
    session: Option<Session>,
    expires: Option<Instant>,
}

impl Backend {
    fn lock(&mut self) {
        self.session = None;
        self.expires = None;
    }

    fn opened(&mut self, session: Session) -> Result<(), String> {
        let expires = Instant::now()
            .checked_add(SESSION_TIME)
            .ok_or("notebook session deadline overflow")?;
        self.session = Some(session);
        self.expires = Some(expires);
        Ok(())
    }

    fn session(&mut self) -> Result<&mut Session, String> {
        if self
            .expires
            .is_none_or(|deadline| Instant::now() >= deadline)
        {
            self.lock();
        }
        self.session
            .as_mut()
            .ok_or_else(|| "notebook is locked".into())
    }

    fn request(
        &mut self,
        bytes: &[u8],
        tokens: &mut impl Tokens,
        random: &mut impl Read,
    ) -> Result<Bytes, String> {
        if self
            .expires
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.lock();
        }
        let mut input = Reader::new(bytes);
        let tag = input.byte()?;
        let mut out = Bytes(vec![0]);
        match tag {
            1 => {
                input.finish()?;
                if lifecycle::exists(&self.directory).map_err(reason)? {
                    let keys = lifecycle::keys(&self.directory).map_err(reason)?;
                    out.push(u8::try_from(keys.len()).map_err(|_| "too many notebook keys")?)?;
                    for key in keys {
                        out.push(key.role as u8)?;
                        field(&mut out, &key.credential)?;
                    }
                } else {
                    out.push(0)?;
                }
                field(
                    &mut out,
                    self.session
                        .as_ref()
                        .map_or(&[], |session| session.credential()),
                )?;
            }
            2 => {
                let backup = input.boolean()?;
                input.finish()?;
                self.lock();
                let session =
                    lifecycle::create(&self.directory, tokens, random, backup).map_err(reason)?;
                self.opened(session)?;
            }
            3 => {
                let credential = input.field(MAX_CREDENTIAL)?;
                input.finish()?;
                self.lock();
                let session = lifecycle::unlock(&self.directory, credential, tokens, random)
                    .map_err(reason)?;
                self.opened(session)?;
            }
            4 => {
                input.finish()?;
                let session = self.session()?;
                out.extend(&session.revision().to_be_bytes())?;
                let count = u32::try_from(session.entries().len())
                    .map_err(|_| "too many notebook entries")?;
                out.extend(&count.to_be_bytes())?;
                for entry in session.entries() {
                    out.extend(&entry.id)?;
                    out.extend(&entry.revision.to_be_bytes())?;
                    field(&mut out, entry.title.as_bytes())?;
                }
            }
            5 => {
                let id = input.array()?;
                input.finish()?;
                let entry = self
                    .session()?
                    .entry(&id)
                    .ok_or("notebook entry is absent")?;
                out.extend(&entry.revision.to_be_bytes())?;
                field(&mut out, entry.title().as_bytes())?;
                field(&mut out, entry.body())?;
                field(&mut out, entry.password().unwrap_or_default())?;
                out.push(u8::try_from(entry.bindings().len()).map_err(|_| "too many bindings")?)?;
                for binding in entry.bindings() {
                    put_binding(&mut out, binding)?;
                }
            }
            6 => {
                let change = change(&mut input)?;
                input.finish()?;
                self.session()?;
                let session = self.session.as_mut().ok_or("notebook is locked")?;
                let result = session.apply(&self.directory, change, &|| tokens.revoked(), random);
                let committed = self.saved(result)?;
                out.extend(&committed.id)?;
                out.extend(&committed.revision.unwrap_or(0).to_be_bytes())?;
            }
            7 => {
                input.finish()?;
                field(&mut out, self.session()?.ciphertext().map_err(reason)?)?;
            }
            8 => {
                let credential = input.field(MAX_CREDENTIAL)?;
                let copy = input.field(MAX_ENVELOPE)?;
                input.finish()?;
                self.lock();
                let session = lifecycle::import(&self.directory, copy, credential, tokens, random)
                    .map_err(reason)?;
                self.opened(session)?;
            }
            9 => {
                input.finish()?;
                self.session()?;
                let session = self.session.as_mut().ok_or("notebook is locked")?;
                let result = session.add_key(&self.directory, tokens, random);
                self.saved(result)?;
            }
            10 => {
                let count = input.byte()?;
                if count == 0 || count > 7 {
                    return Err("invalid revoked key count".into());
                }
                let mut revoked = Vec::new();
                for _ in 0..count {
                    revoked.push(input.field(MAX_CREDENTIAL)?);
                }
                input.finish()?;
                self.session()?;
                let session = self.session.as_mut().ok_or("notebook is locked")?;
                let result = session.replace_keys(&self.directory, &revoked, tokens, random);
                self.saved(result)?;
            }
            11 => {
                input.finish()?;
                self.lock();
            }
            12 => {
                let binding = binding(&mut input)?;
                input.finish()?;
                let session = self.session()?;
                // An uncertain save cannot release a possibly revoked binding.
                session.ciphertext().map_err(reason)?;
                let entry = session
                    .entries()
                    .find_map(|summary| {
                        session
                            .entry(&summary.id)
                            .filter(|entry| entry.bindings().contains(&binding))
                    })
                    .ok_or("mail account has no approved notebook binding")?;
                field(
                    &mut out,
                    entry.password().ok_or("bound entry has no password")?,
                )?;
            }
            13 => {
                input.finish()?;
                out.push(u8::from(self.session.is_some()))?;
            }
            14 => {
                let id = input.array()?;
                let base = input.number()?;
                let allow = input.boolean()?;
                let binding = binding(&mut input)?;
                input.finish()?;
                self.session()?;
                let session = self.session.as_mut().ok_or("notebook is locked")?;
                let result = session.apply(
                    &self.directory,
                    Change::Binding {
                        id,
                        base,
                        binding,
                        allow,
                    },
                    &|| tokens.revoked(),
                    random,
                );
                let committed = self.saved(result)?;
                out.extend(&committed.revision.unwrap_or(0).to_be_bytes())?;
            }
            _ => return Err("unknown notebook request".into()),
        }
        Ok(out)
    }

    fn saved<T>(&mut self, result: Result<T, lifecycle::Error>) -> Result<T, String> {
        if matches!(&result, Err(lifecycle::Error::Uncertain)) {
            self.lock();
        }
        result.map_err(reason)
    }
}

fn reason(error: lifecycle::Error) -> String {
    // Lifecycle errors contain no PIN, key, notebook text or device response.
    format!("notebook operation refused: {error:?}")
}

fn binding(input: &mut Reader<'_>) -> Result<MailBinding, String> {
    let account = input.text(256)?;
    let server = input.text(2048)?;
    let username = input.text(1024)?;
    MailBinding::new(account.to_owned(), server.to_owned(), username.to_owned())
}

fn put_binding(out: &mut Bytes, binding: &MailBinding) -> Result<(), String> {
    for text in [binding.account(), binding.server(), binding.username()] {
        field(out, text.as_bytes())?;
    }
    Ok(())
}

fn change(input: &mut Reader<'_>) -> Result<Change, String> {
    match input.byte()? {
        1 => {
            let title = Text::new(input.text(MAX_TITLE)?.to_owned());
            let body = Text::new(input.text(MAX_BODY)?.to_owned());
            Ok(Change::Create { title, body })
        }
        2 => {
            let id = input.array()?;
            let base = input.number()?;
            let title = Text::new(input.text(MAX_TITLE)?.to_owned());
            let body = Text::new(input.text(MAX_BODY)?.to_owned());
            Ok(Change::Edit {
                id,
                base,
                title,
                body,
            })
        }
        3 => {
            let id = input.array()?;
            let base = input.number()?;
            let title = Text::new(input.text(MAX_TITLE)?.to_owned());
            Ok(Change::Rename { id, base, title })
        }
        4 => Ok(Change::Delete {
            id: input.array()?,
            base: input.number()?,
        }),
        5 => {
            let id = input.array()?;
            let base = input.number()?;
            let password = match input.text(MAX_PASSWORD)? {
                "" => None,
                text => Some(Text::new(text.to_owned())),
            };
            Ok(Change::Password { id, base, password })
        }
        _ => Err("unsupported notebook edit".into()),
    }
}

pub(crate) fn run(owner: u32) -> Result<(), String> {
    let registry = crate::principals::Registry::load()?;
    let uid = registry
        .notebooks()
        .find_map(|(human, uid)| (human == owner).then_some(uid))
        .ok_or("no notebook service reserved for this account")?;
    let stream = crate::operation::service_startup(uid)?;
    let protected = host::protect_service()?;
    let directory = host::directory(
        &std::path::PathBuf::from(format!("/var/lib/td/notebooks/{owner}")),
        uid,
    )
    .map_err(reason)?;
    let mut random = Random(Entropy::open()?);
    let mut backend = Backend {
        directory,
        session: None,
        expires: None,
    };
    let mut wire = Wire::new(stream)?;
    loop {
        let request = wire.receive(IDLE)?;
        protected.recheck().map_err(str::to_owned)?;
        let mut tokens = super::service_tokens::Remote::new(&mut wire);
        let result = backend.request(&request.0, &mut tokens, &mut random);
        if tokens.revoked() {
            return Err("notebook token authority failed".into());
        }
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => {
                let mut reply = Bytes(vec![1]);
                field(&mut reply, error.as_bytes())?;
                reply
            }
        };
        wire.send(&reply.0)?;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn service_replies_use_clearing_growth() {
        for source in [
            include_str!("portable_service.rs"),
            include_str!("portable_service_tokens.rs"),
        ] {
            let source = source.split("#[cfg(test)]").next().unwrap();
            assert!(!source.contains("out.0.extend"));
            assert!(!source.contains("out.0.push"));
        }
    }

    use super::super::lifecycle::tests::{Bench, Random as TestRandom};
    use super::*;
    use crate::test_temp::TempDir;
    use std::fs::File;
    use std::os::unix::fs::MetadataExt;

    fn fixture() -> (TempDir, Backend, Bench, TestRandom) {
        let temp = TempDir::new("td-notebook-service").unwrap();
        let file = File::open(temp.path()).unwrap();
        let owner = file.metadata().unwrap().uid();
        let backend = Backend {
            directory: Directory { file, owner },
            session: None,
            expires: None,
        };
        (temp, backend, Bench::new(2), TestRandom(0))
    }

    fn mail(server: &str) -> MailBinding {
        MailBinding::new("personal".into(), server.into(), "timmy".into()).unwrap()
    }

    #[test]
    fn service_admission_never_requires_root_only_shadow_reads() {
        let source = include_str!("portable_service.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!source.contains("verify_installed_accounts"));
        assert!(!source.contains("/etc/shadow"));
        assert!(!source.contains("Host::open"));
        assert!(source.contains("operation::service_startup(uid)?"));
    }

    #[test]
    #[ignore = "requires the disposable notebook-service QEMU guest"]
    fn qemu_reserved_identity_serves_with_root_only_shadow() {
        use std::os::fd::OwnedFd;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;
        use std::process::{Child, Command, Stdio};
        crate::fido_uhid::guard("notebook-service");
        // These fixed paths belong to this diskless guest, never a host or
        // persistent image. The harness destroys the whole guest afterwards.
        std::fs::create_dir_all("/etc").unwrap();
        for (path, text, mode) in [
            ("/etc/td-principals.tsv", "td-principals-v1\nsession\t1000\t993\t992\t991\nnotebook\t1000\t989\n", 0o444),
            ("/etc/passwd", "root:x:0:0:Root:/root:/bin/sh\ntester:x:1000:1000:Tester:/home/tester:/bin/sh\ntdv1000:x:989:989:Vault:/var/lib/td/notebooks/1000:/bin/false\n", 0o644),
            ("/etc/group", "root:x:0:\ntester:x:1000:\ntdv1000:x:989:\n", 0o644),
            ("/etc/shadow", "root:!:0:0:99999:7:::\ntester::0:0:99999:7:::\ntdv1000:!td-service:0:0:99999:7:::\n", 0o600),
        ] {
            std::fs::write(path, text).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        std::fs::create_dir_all("/var/lib/td/notebooks/1000").unwrap();
        std::fs::set_permissions(
            "/var/lib/td/notebooks/1000",
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::os::unix::fs::lchown("/var/lib/td/notebooks/1000", Some(989), Some(989)).unwrap();
        struct Process(Child);
        impl Drop for Process {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let (parent, child) = UnixStream::pair().unwrap();
        let mut process = Process(
            Command::new("/bin/td-login")
                .args([
                    "exec-service-as",
                    "tdv1000",
                    "--",
                    "/bin/td-secret",
                    "notebook-service",
                    "--uid",
                    "1000",
                ])
                .env_clear()
                .current_dir("/")
                .stdin(Stdio::from(OwnedFd::from(child)))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut wire = Wire::new(parent).unwrap();
        wire.send(&[13]).unwrap();
        assert_eq!(wire.receive(Duration::from_secs(5)).unwrap().0, [0, 0]);
        wire.send(&[1]).unwrap();
        assert_eq!(
            wire.receive(Duration::from_secs(5)).unwrap().0,
            [0, 0, 0, 0, 0, 0]
        );
        drop(wire);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if process.0.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "service did not retire after authority loss"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn credentials_require_unlock_and_exact_binding_and_survive_backup_unlock() {
        let (_temp, mut backend, mut tokens, mut random) = fixture();
        let mut retrieve = Bytes(vec![12]);
        put_binding(&mut retrieve, &mail("https://mail.example/jmap")).unwrap();
        assert!(backend
            .request(&retrieve.0, &mut tokens, &mut random)
            .is_err());
        tokens.present(&[0, 0, 1, 1]);
        backend.request(&[2, 1], &mut tokens, &mut random).unwrap();
        let backup = backend
            .session()
            .unwrap()
            .keys()
            .into_iter()
            .find(|k| k.role == super::super::Role::Backup)
            .unwrap()
            .credential;
        let mut create = Bytes(vec![6, 1]);
        field(&mut create, b"Mail").unwrap();
        field(&mut create, b"notes, never a password").unwrap();
        let reply = backend
            .request(&create.0, &mut tokens, &mut random)
            .unwrap();
        let id: [u8; 16] = reply.0[1..17].try_into().unwrap();
        let mut password = Bytes(vec![6, 5]);
        password.0.extend(id);
        password.0.extend(1u64.to_be_bytes());
        field(&mut password, b"explicit-token").unwrap();
        backend
            .request(&password.0, &mut tokens, &mut random)
            .unwrap();
        assert!(backend
            .request(&retrieve.0, &mut tokens, &mut random)
            .is_err());
        let mut bind = Bytes(vec![14]);
        bind.0.extend(id);
        bind.0.extend(2u64.to_be_bytes());
        bind.0.push(1);
        put_binding(&mut bind, &mail("https://mail.example/jmap")).unwrap();
        backend.request(&bind.0, &mut tokens, &mut random).unwrap();
        let result = backend
            .request(&retrieve.0, &mut tokens, &mut random)
            .unwrap();
        assert_eq!(&result.0[5..], b"explicit-token");
        assert_eq!(tokens.calls.len(), 4);
        for (account, server, username) in [
            ("other", "https://mail.example/jmap", "timmy"),
            ("personal", "https://evil.example/jmap", "timmy"),
            ("personal", "https://mail.example/jmap", "other"),
        ] {
            let mut request = Bytes(vec![12]);
            put_binding(
                &mut request,
                &MailBinding::new(account.into(), server.into(), username.into()).unwrap(),
            )
            .unwrap();
            assert!(backend
                .request(&request.0, &mut tokens, &mut random)
                .is_err());
        }
        backend.request(&[11], &mut tokens, &mut random).unwrap();
        assert!(backend
            .request(&retrieve.0, &mut tokens, &mut random)
            .is_err());
        let mut unlock = Bytes(vec![3]);
        field(&mut unlock, &backup).unwrap();
        tokens.present(&[1]);
        backend
            .request(&unlock.0, &mut tokens, &mut random)
            .unwrap();
        assert_eq!(
            &backend
                .request(&retrieve.0, &mut tokens, &mut random)
                .unwrap()
                .0[5..],
            b"explicit-token"
        );
        let mut read = Bytes(vec![5]);
        read.0.extend(id);
        let reply = backend.request(&read.0, &mut tokens, &mut random).unwrap();
        let mut input = Reader::new(&reply.0);
        assert_eq!(input.byte().unwrap(), 0);
        assert_eq!(input.number().unwrap(), 3);
        assert_eq!(input.text(MAX_TITLE).unwrap(), "Mail");
        assert_eq!(input.text(MAX_BODY).unwrap(), "notes, never a password");
        backend.expires = Some(Instant::now());
        assert!(backend
            .request(&retrieve.0, &mut tokens, &mut random)
            .is_err());
        assert!(backend.session.is_none());
    }

    #[test]
    fn malformed_creation_has_no_token_or_disk_effect_and_uncertainty_locks() {
        let (temp, mut backend, mut tokens, mut random) = fixture();
        for request in [&[][..], &[2], &[2, 2], &[2, 1, 0], &[255]] {
            assert!(backend.request(request, &mut tokens, &mut random).is_err());
        }
        assert!(tokens.calls.is_empty());
        assert!(!temp.join("vault").exists());
        tokens.present(&[0, 0]);
        backend.request(&[2, 0], &mut tokens, &mut random).unwrap();
        let before = std::fs::read(temp.join("vault")).unwrap();
        let mut create = Bytes(vec![6, 1]);
        field(&mut create, b"entry").unwrap();
        field(&mut create, b"body").unwrap();
        create.0.push(0);
        assert!(backend
            .request(&create.0, &mut tokens, &mut random)
            .is_err());
        assert_eq!(std::fs::read(temp.join("vault")).unwrap(), before);
        assert!(backend
            .saved::<()>(Err(lifecycle::Error::Uncertain))
            .is_err());
        assert!(backend.session.is_none());
        assert!(backend.expires.is_none());
    }
}
