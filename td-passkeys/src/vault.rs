use super::{
    credential::{self, Account, Approval, Credential},
    field, put, take, Error, Plain, Result, Secret,
};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;
use td_fido::web_request::{Create, Get};
use td_primitives::{chacha20_poly1305 as aead, random};
use td_protector::{
    lockout::DA_PARAMETERS,
    pin::{Pin, SALT_LEN},
};
use td_tpm::{Client, Transport};

const MAGIC: &[u8; 8] = b"TDPASSK1";
const MAX_RECORD: usize = 32 + 1 + 3 * 2 + 253 + 64 + 256 + 32;
const MAX_RECORDS: usize = 256;
const MAX_BYTES: usize = MAX_RECORDS * (MAX_RECORD + 2) + 8192;
/// A trusted enrollment UI must distinguish dismissing the warning from
/// knowingly creating a vault with this one unrecoverable protector.
pub enum Enrollment {
    Cancelled,
    AcceptLossOfOriginalTpm,
}
struct Place {
    _directory: File,
    path: PathBuf,
    owner: u32,
}
impl Place {
    // Root supplies the private vault directory by descriptor. Open a new
    // description so concurrent calls with duplicated descriptors compete.
    fn lock(directory: &File) -> Result<Self> {
        let link = td_alloc::format(format_args!("/proc/self/fd/{}/.", directory.as_raw_fd()))
            .map_err(|_| Error::Capacity)?;
        let handle = File::open(link)?;
        let meta = handle.metadata()?;
        if !meta.is_dir() || meta.mode() & 0o7777 != 0o700 {
            return Err(Error::Refused);
        }
        handle.try_lock().map_err(|_| Error::Locked)?;
        let path = PathBuf::from(
            td_alloc::format(format_args!(
                "/proc/self/fd/{}/passkeys",
                handle.as_raw_fd()
            ))
            .map_err(|_| Error::Capacity)?,
        );
        Ok(Self {
            _directory: handle,
            path,
            owner: meta.uid(),
        })
    }
    fn load(&self) -> Result<Vec<u8>> {
        let (mut f, md) = td_fs::open_real_file(&self.path, "passkeys")?;
        if md.mode() & 0o7777 != 0o600 || md.uid() != self.owner || md.nlink() != 1 {
            return Err(Error::Refused);
        }
        let n = usize::try_from(md.len()).map_err(|_| Error::Full)?;
        if n > MAX_BYTES {
            return Err(Error::Full);
        }
        let mut data = td_alloc::filled(n, 0)?;
        f.read_exact(&mut data)?;
        if f.read(&mut [0])? != 0 {
            return Err(Error::Stale);
        }
        Ok(data)
    }
}

struct Header<'a> {
    salt: &'a [u8; SALT_LEN],
    name: &'a [u8],
    public: &'a [u8],
    private: &'a [u8],
}
fn header(mut input: &[u8]) -> Result<(Header<'_>, &[u8])> {
    if take(&mut input, 8)? != MAGIC {
        return Err(Error::Invalid);
    }
    let salt = take(&mut input, SALT_LEN)?
        .try_into()
        .map_err(|_| Error::Invalid)?;
    let name = field(&mut input, 34)?;
    let public = field(&mut input, 1024)?;
    let private = field(&mut input, 2048)?;
    if name.len() != 34 || private.is_empty() {
        return Err(Error::Invalid);
    }
    Ok((
        Header {
            salt,
            name,
            public,
            private,
        },
        input,
    ))
}

pub struct Vault {
    place: Place,
    header: Vec<u8>,
    key: Secret<32>,
    entries: Plain,
    failed: bool,
}

fn policy<T: Transport>(client: &mut Client<T>) -> Result<()> {
    let da = client.dictionary_attack().map_err(|_| Error::Refused)?;
    if !da.lockout_auth_set
        || da.in_lockout
        || da.counter >= da.max_tries
        || da.max_tries > DA_PARAMETERS.max_tries
        || da.interval < DA_PARAMETERS.recovery_time
        || da.recovery < DA_PARAMETERS.lockout_recovery
    {
        return Err(Error::Refused);
    }
    Ok(())
}

impl Vault {
    /// The caller obtains the primary Name from td's proven lockout
    /// provisioning, independent of the metadata in this file.
    pub fn enroll<T: Transport>(
        directory: &File,
        mut client: Client<T>,
        primary: &[u8],
        pin: &Pin,
        choice: Enrollment,
    ) -> Result<Self> {
        if !matches!(choice, Enrollment::AcceptLossOfOriginalTpm) {
            return Err(Error::Refused);
        }
        let place = Place::lock(directory)?;
        match fs::symlink_metadata(&place.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Ok(_) => return Err(Error::Excluded),
            Err(e) => return Err(e.into()),
        }
        policy(&mut client)?;
        let mut salt = [0; SALT_LEN];
        random::fill(&mut salt)?;
        let mut key = Secret([0; 32]);
        random::fill(&mut key.0)?;
        let mut to_seal = Secret(key.0);
        let sealed = client
            .seal_device_pin(primary, pin.auth_value(&salt).expose(), &mut to_seal.0)
            .map_err(|_| Error::Refused)?;
        let mut header = td_alloc::copy(MAGIC)?;
        td_alloc::extend_from_slice(&mut header, &salt)?;
        put(&mut header, &sealed.primary_name)?;
        put(&mut header, &sealed.object.public)?;
        put(&mut header, &sealed.object.private)?;
        let mut vault = Self {
            place,
            header,
            key,
            entries: Plain(Vec::new()),
            failed: false,
        };
        vault.persist()?;
        Ok(vault)
    }
    pub fn unlock<T: Transport>(
        directory: &File,
        mut client: Client<T>,
        primary: &[u8],
        pin: &Pin,
    ) -> Result<Self> {
        let place = Place::lock(directory)?;
        let blob = place.load()?;
        let (h, mut rest) = header(&blob)?;
        if h.name != primary {
            return Err(Error::Refused);
        }
        let header = td_alloc::copy(blob.get(..blob.len() - rest.len()).ok_or(Error::Invalid)?)?;
        let nonce = take(&mut rest, 12)?
            .try_into()
            .map_err(|_| Error::Invalid)?;
        let end = rest.len().checked_sub(16).ok_or(Error::Invalid)?;
        let (ciphertext, tag) = rest.split_at_checked(end).ok_or(Error::Invalid)?;
        policy(&mut client)?;
        let unsealed = Plain(
            client
                .unseal_device_pin(h.name, pin.auth_value(h.salt).expose(), h.public, h.private)
                .map_err(|_| Error::Refused)?,
        );
        let key = Secret(
            unsealed
                .0
                .as_slice()
                .try_into()
                .map_err(|_| Error::Invalid)?,
        );
        let mut entries = Plain(td_alloc::copy(ciphertext)?);
        aead::open(
            &key.0,
            nonce,
            &header,
            &mut entries.0,
            tag.try_into().map_err(|_| Error::Invalid)?,
        )?;
        let result = Self {
            place,
            header,
            key,
            entries,
            failed: false,
        };
        result.visit(|_| Ok(()))?;
        Ok(result)
    }
    fn visit(&self, mut f: impl FnMut(Credential<'_>) -> Result<()>) -> Result<usize> {
        if self.failed {
            return Err(Error::Stale);
        }
        let mut left = self.entries.0.as_slice();
        let mut count = 0;
        while !left.is_empty() {
            if count >= MAX_RECORDS {
                return Err(Error::Full);
            }
            f(Credential::read(field(&mut left, MAX_RECORD)?)?)?;
            count += 1;
        }
        Ok(count)
    }
    pub fn list(&self, mut f: impl FnMut(Account<'_>) -> Result<()>) -> Result<()> {
        self.visit(|item| f(item.account)).map(|_| ())
    }
    fn persist(&mut self) -> Result<()> {
        // An uncertain rename or directory sync requires a fresh load.
        self.failed = true;
        let mut nonce = [0u8; 12];
        random::fill(&mut nonce)?;
        let mut sealed = Plain(td_alloc::copy(&self.entries.0)?);
        let tag = aead::seal(&self.key.0, &nonce, &self.header, &mut sealed.0)?;
        let mut blob = td_alloc::copy(&self.header)?;
        td_alloc::extend_from_slice(&mut blob, &nonce)?;
        td_alloc::extend_from_slice(&mut blob, &sealed.0)?;
        td_alloc::extend_from_slice(&mut blob, &tag)?;
        td_fs::replace_with(&self.place.path, &blob, 0o600, |file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            let meta = file.metadata()?;
            if meta.uid() != self.place.owner || meta.nlink() != 1 || meta.mode() & 0o7777 != 0o600
            {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            Ok(())
        })?;
        self.failed = false;
        Ok(())
    }
    pub fn register(&mut self, request: &Create<'_>, approval: Approval) -> Result<Vec<u8>> {
        approval.flags(request.verify_user() || request.discoverable())?;
        let mut duplicate = None;
        let count = self.visit(|c| {
            if c.account.rp == request.rp() {
                if request.exclude().ids().contains(&c.account.id.as_slice()) {
                    return Err(Error::Excluded);
                }
                if request.discoverable()
                    && c.account.discoverable
                    && c.account.user == request.user_id()
                {
                    duplicate = Some(*c.account.id);
                }
            }
            Ok(())
        })?;
        if count >= MAX_RECORDS && duplicate.is_none() {
            return Err(Error::Full);
        }
        let (created, reply) = credential::make(request, approval)?;
        let mut next = Plain(self.without(duplicate.as_ref())?);
        put(&mut next.0, &created.0)?;
        self.entries = next;
        self.persist()?;
        Ok(reply)
    }
    /// Check known handles for Firefox's empty-client-data list filter.
    pub fn preflight(&self, request: &Get<'_>) -> Result<Vec<u8>> {
        let mut reply = None;
        self.visit(|credential| {
            if reply.is_none()
                && credential.account.rp == request.rp()
                && request
                    .allow()
                    .ids()
                    .contains(&credential.account.id.as_slice())
            {
                reply = Some(credential.preflight(request)?);
            }
            Ok(())
        })?;
        reply.ok_or(Error::Missing)
    }
    /// The UI selects an ID for this request. RP and allow-list checks run
    /// again here even if it supplied an unrelated selection.
    pub fn sign(&self, request: &Get<'_>, id: &[u8; 32], approval: Approval) -> Result<Vec<u8>> {
        let mut reply = None;
        self.visit(|c| {
            if c.account.id == id {
                reply = Some(c.assertion(request, approval)?);
            }
            Ok(())
        })?;
        reply.ok_or(Error::Missing)
    }
    fn without(&self, id: Option<&[u8; 32]>) -> Result<Vec<u8>> {
        if self.failed {
            return Err(Error::Stale);
        }
        let mut out = Plain(td_alloc::with_capacity(
            self.entries.0.len() + MAX_RECORD + 2,
        )?);
        let mut rest = self.entries.0.as_slice();
        let mut found = false;
        while !rest.is_empty() {
            let record = field(&mut rest, MAX_RECORD)?;
            if Some(Credential::read(record)?.account.id) == id {
                found = true;
            } else {
                put(&mut out.0, record)?;
            }
        }
        if id.is_some() && !found {
            return Err(Error::Missing);
        }
        Ok(std::mem::take(&mut out.0))
    }
    pub fn delete(&mut self, id: &[u8; 32]) -> Result<()> {
        self.entries = Plain(self.without(Some(id))?);
        self.persist()
    }
}
