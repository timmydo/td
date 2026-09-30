//! Immutable review data, not a device claim, authenticated source or consent.

/// Input admission ceiling, checked before parsing or allocating fields.
/// The largest current valid encoding is 1191 bytes.
pub const MAX_BYTES: usize = 2048;
const MAGIC: &[u8; 8] = b"TDPLAN01";
const CANDIDATES_MAGIC: &[u8; 8] = b"TDCAND01";
pub const MAX_CANDIDATES: usize = 64;
const NAME_BYTES: usize = 64;
const LABEL_BYTES: usize = 256;
const USERNAME_BYTES: usize = 32;
const HOSTNAME_BYTES: usize = 63;
const KEYBOARD_BYTES: usize = 64;
const TIMEZONE_BYTES: usize = 64;
const DESTINATION_BYTES: usize = 4 + 4 + 8 + 8 + 4 + 1 + 2 + NAME_BYTES + 3 * (1 + 2 + LABEL_BYTES);
const ENCODED_BYTES: usize = 8
    + 32
    + 32
    + 16
    + DESTINATION_BYTES
    + 4 * 2
    + USERNAME_BYTES
    + HOSTNAME_BYTES
    + KEYBOARD_BYTES
    + TIMEZONE_BYTES;
pub const MAX_CANDIDATE_BYTES: usize = 8 + 1 + MAX_CANDIDATES * DESTINATION_BYTES;

/// Unvalidated caller observations. Construction copies admitted values.
#[derive(Debug)]
pub struct DestinationObservation<'a> {
    pub name: &'a str,
    pub major: u32,
    pub minor: u32,
    pub sequence: u64,
    pub capacity: u64,
    pub sector: u32,
    pub removable: bool,
    pub model: Option<&'a str>,
    pub serial: Option<&'a str>,
    pub wwid: Option<&'a str>,
}

/// Observations captured by the authority, never proof of hardware identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Destination {
    name: String,
    major: u32,
    minor: u32,
    sequence: u64,
    capacity: u64,
    sector: u32,
    removable: bool,
    model: Option<String>,
    serial: Option<String>,
    wwid: Option<String>,
}

impl Destination {
    /// Wire admission only. The caller must establish whole-disk eligibility.
    pub fn new(observed: DestinationObservation<'_>) -> Result<Self, String> {
        let DestinationObservation {
            name,
            major,
            minor,
            sequence,
            capacity,
            sector,
            removable,
            model,
            serial,
            wwid,
        } = observed;
        if name.is_empty()
            || name.len() > NAME_BYTES
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err("invalid installation destination kernel name".into());
        }
        if major == 0
            || sequence == 0
            || capacity == 0
            || !matches!(sector, 512 | 4096)
            || !capacity.is_multiple_of(u64::from(sector))
        {
            return Err("invalid installation destination identity or geometry".into());
        }
        for label in [model, serial, wwid].into_iter().flatten() {
            if label.len() > LABEL_BYTES {
                return Err("installation destination label exceeds byte bound".into());
            }
        }
        Ok(Self {
            name: name.into(),
            major,
            minor,
            sequence,
            capacity,
            sector,
            removable,
            model: model.map(str::to_owned),
            serial: serial.map(str::to_owned),
            wwid: wwid.map(str::to_owned),
        })
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn number(&self) -> (u32, u32) {
        (self.major, self.minor)
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    pub fn capacity(&self) -> u64 {
        self.capacity
    }
    pub fn sector(&self) -> u32 {
        self.sector
    }
    pub fn removable(&self) -> bool {
        self.removable
    }
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }
    pub fn wwid(&self) -> Option<&str> {
        self.wwid.as_deref()
    }
}

/// Bounded advisory observations for the destination page. The root service
/// must independently establish eligibility, source identity and a held disk
/// claim before proposing or executing an installation. Decoding these bytes
/// grants no such authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidates {
    disks: Vec<Destination>,
}

impl Candidates {
    pub fn new(disks: Vec<Destination>) -> Result<Self, String> {
        if disks.len() > MAX_CANDIDATES {
            return Err("too many installation candidates".into());
        }
        for (index, disk) in disks.iter().enumerate() {
            if disks.iter().take(index).any(|other| {
                other.name() == disk.name()
                    || other.number() == disk.number()
                    || other.sequence() == disk.sequence()
            }) {
                return Err("duplicate installation candidate identity".into());
            }
        }
        Ok(Self { disks })
    }

    pub fn as_slice(&self) -> &[Destination] {
        &self.disks
    }

    /// Canonical bounded bytes for a read-only service reply.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 1 + self.disks.len() * DESTINATION_BYTES);
        out.extend_from_slice(CANDIDATES_MAGIC);
        out.push(self.disks.len() as u8);
        for disk in &self.disks {
            put_destination(&mut out, disk);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_CANDIDATE_BYTES {
            return Err("installation candidates exceed wire bound".into());
        }
        let mut reader = Reader(bytes, "candidates");
        if &reader.array::<8>()? != CANDIDATES_MAGIC {
            return Err("unsupported installation candidates version".into());
        }
        let [count] = reader.array::<1>()?;
        let count = usize::from(count);
        if count > MAX_CANDIDATES {
            return Err("too many installation candidates".into());
        }
        let mut disks = Vec::with_capacity(count);
        for _ in 0..count {
            disks.push(read_destination(&mut reader)?);
        }
        reader.finish()?;
        Self::new(disks)
    }
}

/// Bounded choices. Account policy and catalog membership are caller checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    username: String,
    hostname: String,
    keyboard: String,
    timezone: String,
}

impl Settings {
    pub fn new(
        username: &str,
        hostname: &str,
        keyboard: &str,
        timezone: &str,
    ) -> Result<Self, String> {
        for (value, limit) in [
            (username, USERNAME_BYTES),
            (hostname, HOSTNAME_BYTES),
            (keyboard, KEYBOARD_BYTES),
            (timezone, TIMEZONE_BYTES),
        ] {
            token(value, limit)?;
        }
        Ok(Self {
            username: username.into(),
            hostname: hostname.into(),
            keyboard: keyboard.into(),
            timezone: timezone.into(),
        })
    }
    pub fn username(&self) -> &str {
        &self.username
    }
    pub fn hostname(&self) -> &str {
        &self.hostname
    }
    pub fn keyboard(&self) -> &str {
        &self.keyboard
    }
    pub fn timezone(&self) -> &str {
        &self.timezone
    }
}

/// One proposed whole-disk, unencrypted, automatic-login installation.
/// A fresh nonce distinguishes otherwise equal proposals. Equality binds all
/// fields; cloning copies a proposal and never creates another authorization.
///
/// ```compile_fail,E0616
/// fn change(plan: &mut td_install::installation_plan::Plan) {
///     plan.nonce = [0; 32];
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Plan {
    nonce: [u8; 32],
    destination: Destination,
    deployment: [u8; 32],
    volume_uuid: [u8; 16],
    settings: Settings,
}

impl Plan {
    /// The authority generates the nonce and version-4 UUID and verifies the
    /// source and settings independently. No entropy or I/O occurs here.
    pub fn new(
        nonce: [u8; 32],
        destination: Destination,
        deployment: [u8; 32],
        volume_uuid: [u8; 16],
        settings: Settings,
    ) -> Result<Self, String> {
        if nonce == [0; 32]
            || volume_uuid.get(6).map(|b| b >> 4) != Some(4)
            || volume_uuid.get(8).map(|b| b >> 6) != Some(2)
        {
            return Err("installation plan requires a nonce and version-4 volume UUID".into());
        }
        Ok(Self {
            nonce,
            destination,
            deployment,
            volume_uuid,
            settings,
        })
    }
    pub fn nonce(&self) -> &[u8; 32] {
        &self.nonce
    }
    pub fn destination(&self) -> &Destination {
        &self.destination
    }
    pub fn deployment(&self) -> &[u8; 32] {
        &self.deployment
    }
    /// Compare with the canonical manifest ID printed by td-boot. This is
    /// data equality, not proof that the source was authenticated or retained.
    pub fn matches_deployment_id(&self, id: &str) -> bool {
        if id.len() != 64 {
            return false;
        }
        id.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(self.deployment)
            .all(|([high, low], expected)| {
                let digit = |byte| match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    _ => None,
                };
                digit(*high)
                    .zip(digit(*low))
                    .is_some_and(|(high, low)| high * 16 + low == expected)
            })
    }
    pub fn volume_uuid(&self) -> &[u8; 16] {
        &self.volume_uuid
    }
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Canonical bytes, independent of locale, host endianness or path lookup.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENCODED_BYTES);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.deployment);
        out.extend_from_slice(&self.volume_uuid);
        put_destination(&mut out, &self.destination);
        put_settings(&mut out, &self.settings);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("installation plan exceeds wire bound".into());
        }
        let mut r = Reader(bytes, "plan");
        if &r.array::<8>()? != MAGIC {
            return Err("unsupported installation plan version".into());
        }
        let nonce = r.array()?;
        let deployment = r.array()?;
        let uuid = r.array()?;
        let destination = read_destination(&mut r)?;
        let settings = read_settings(&mut r)?;
        r.finish()?;
        Self::new(nonce, destination, deployment, uuid, settings)
    }
}

pub(crate) fn put_settings(out: &mut Vec<u8>, settings: &Settings) {
    for value in [
        &settings.username,
        &settings.hostname,
        &settings.keyboard,
        &settings.timezone,
    ] {
        put(out, value);
    }
}

pub(crate) fn read_settings(reader: &mut Reader<'_>) -> Result<Settings, String> {
    let username = reader.string(USERNAME_BYTES)?;
    let hostname = reader.string(HOSTNAME_BYTES)?;
    let keyboard = reader.string(KEYBOARD_BYTES)?;
    let timezone = reader.string(TIMEZONE_BYTES)?;
    Settings::new(username, hostname, keyboard, timezone)
}

pub(crate) fn put_destination(out: &mut Vec<u8>, disk: &Destination) {
    out.extend_from_slice(&disk.major.to_be_bytes());
    out.extend_from_slice(&disk.minor.to_be_bytes());
    out.extend_from_slice(&disk.sequence.to_be_bytes());
    out.extend_from_slice(&disk.capacity.to_be_bytes());
    out.extend_from_slice(&disk.sector.to_be_bytes());
    out.push(u8::from(disk.removable));
    put(out, &disk.name);
    for label in [&disk.model, &disk.serial, &disk.wwid] {
        out.push(u8::from(label.is_some()));
        if let Some(label) = label {
            put(out, label);
        }
    }
}

pub(crate) fn read_destination(reader: &mut Reader<'_>) -> Result<Destination, String> {
    let major = u32::from_be_bytes(reader.array()?);
    let minor = u32::from_be_bytes(reader.array()?);
    let sequence = u64::from_be_bytes(reader.array()?);
    let capacity = u64::from_be_bytes(reader.array()?);
    let sector = u32::from_be_bytes(reader.array()?);
    let removable = reader.flag()?;
    let name = reader.string(NAME_BYTES)?;
    let model = reader.optional()?;
    let serial = reader.optional()?;
    let wwid = reader.optional()?;
    Destination::new(DestinationObservation {
        name,
        major,
        minor,
        sequence,
        capacity,
        sector,
        removable,
        model,
        serial,
        wwid,
    })
}

fn text(value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > limit || !value.bytes().all(|b| (32..=126).contains(&b)) {
        return Err(format!(
            "plan text requires 1..={limit} printable ASCII bytes"
        ));
    }
    Ok(())
}
fn token(value: &str, limit: usize) -> Result<(), String> {
    text(value, limit)?;
    if value.contains(' ') {
        return Err("plan choice cannot contain spaces".into());
    }
    Ok(())
}
fn put(out: &mut Vec<u8>, value: &str) {
    // Private constructors bound every string to 256 bytes.
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}
/// Positional reader over one bounded record; the label names it in errors.
pub(crate) struct Reader<'a>(&'a [u8], &'static str);
impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], label: &'static str) -> Self {
        Self(bytes, label)
    }
    pub(crate) fn finish(self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(format!("trailing installation {} bytes", self.1))
        }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let (value, remaining) = self
            .0
            .split_at_checked(count)
            .ok_or_else(|| format!("truncated installation {}", self.1))?;
        self.0 = remaining;
        Ok(value)
    }
    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| format!("invalid fixed {} field", self.1))
    }
    fn flag(&mut self) -> Result<bool, String> {
        match self.array::<1>()? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(format!("invalid {} flag", self.1)),
        }
    }
    fn string(&mut self, limit: usize) -> Result<&'a str, String> {
        let count = usize::from(u16::from_be_bytes(self.array()?));
        if count > limit {
            return Err(format!("invalid {} string length", self.1));
        }
        std::str::from_utf8(self.take(count)?)
            .map_err(|_| format!("invalid {} text encoding", self.1))
    }
    fn optional(&mut self) -> Result<Option<&'a str>, String> {
        if self.flag()? {
            self.string(LABEL_BYTES).map(Some)
        } else {
            Ok(None)
        }
    }
}
