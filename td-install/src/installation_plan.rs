//! Immutable review data, not a device claim, authenticated source or consent.

/// Input admission ceiling, checked before parsing or allocating fields.
/// The largest current valid encoding is 1191 bytes.
pub const MAX_BYTES: usize = 2048;
const MAGIC: &[u8; 8] = b"TDPLAN01";
const NAME_BYTES: usize = 64;
const LABEL_BYTES: usize = 256;
const USERNAME_BYTES: usize = 32;
const HOSTNAME_BYTES: usize = 63;
const KEYBOARD_BYTES: usize = 64;
const TIMEZONE_BYTES: usize = 64;
const ENCODED_BYTES: usize = 117
    + 2
    + NAME_BYTES
    + 3 * (3 + LABEL_BYTES)
    + 8
    + USERNAME_BYTES
    + HOSTNAME_BYTES
    + KEYBOARD_BYTES
    + TIMEZONE_BYTES;

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
        id.as_bytes().as_chunks::<2>().0.iter().zip(self.deployment).all(|([high, low], expected)| {
            let digit = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            digit(*high).zip(digit(*low))
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
        let d = &self.destination;
        out.extend_from_slice(&d.major.to_be_bytes());
        out.extend_from_slice(&d.minor.to_be_bytes());
        out.extend_from_slice(&d.sequence.to_be_bytes());
        out.extend_from_slice(&d.capacity.to_be_bytes());
        out.extend_from_slice(&d.sector.to_be_bytes());
        out.push(u8::from(d.removable));
        put(&mut out, &d.name);
        for label in [&d.model, &d.serial, &d.wwid] {
            out.push(u8::from(label.is_some()));
            if let Some(label) = label {
                put(&mut out, label);
            }
        }
        for value in [
            &self.settings.username,
            &self.settings.hostname,
            &self.settings.keyboard,
            &self.settings.timezone,
        ] {
            put(&mut out, value);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_BYTES {
            return Err("installation plan exceeds wire bound".into());
        }
        let mut r = Reader(bytes);
        if &r.array::<8>()? != MAGIC {
            return Err("unsupported installation plan version".into());
        }
        let nonce = r.array()?;
        let deployment = r.array()?;
        let uuid = r.array()?;
        let major = u32::from_be_bytes(r.array()?);
        let minor = u32::from_be_bytes(r.array()?);
        let sequence = u64::from_be_bytes(r.array()?);
        let capacity = u64::from_be_bytes(r.array()?);
        let sector = u32::from_be_bytes(r.array()?);
        let removable = r.flag()?;
        let name = r.string(NAME_BYTES)?;
        let model = r.optional()?;
        let serial = r.optional()?;
        let wwid = r.optional()?;
        let username = r.string(USERNAME_BYTES)?;
        let hostname = r.string(HOSTNAME_BYTES)?;
        let keyboard = r.string(KEYBOARD_BYTES)?;
        let timezone = r.string(TIMEZONE_BYTES)?;
        if !r.0.is_empty() {
            return Err("trailing installation plan bytes".into());
        }
        let destination = Destination::new(DestinationObservation {
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
        })?;
        let settings = Settings::new(username, hostname, keyboard, timezone)?;
        Self::new(nonce, destination, deployment, uuid, settings)
    }
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
struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], String> {
        let (value, remaining) = self
            .0
            .split_at_checked(count)
            .ok_or("truncated installation plan")?;
        self.0 = remaining;
        Ok(value)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "invalid fixed plan field".into())
    }
    fn flag(&mut self) -> Result<bool, String> {
        match self.array::<1>()? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err("invalid plan flag".into()),
        }
    }
    fn string(&mut self, limit: usize) -> Result<&'a str, String> {
        let count = usize::from(u16::from_be_bytes(self.array()?));
        if count > limit {
            return Err("invalid plan string length".into());
        }
        std::str::from_utf8(self.take(count)?).map_err(|_| "invalid plan text encoding".into())
    }
    fn optional(&mut self) -> Result<Option<&'a str>, String> {
        if self.flag()? {
            self.string(LABEL_BYTES).map(Some)
        } else {
            Ok(None)
        }
    }
}
