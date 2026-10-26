//! WPA2-Personal as a station (IEEE 802.11-2020 12.7): information-element
//! parsing, BSS choice, the PTK derivation, EAPOL-Key frames and the 4-way and
//! group key handshakes, with no I/O. Only RSN with AKM 00-0F-AC:2 (PSK),
//! CCMP-128 pairwise and group ciphers, key descriptor version 2 (HMAC-SHA1
//! MIC, AES key wrap) and no management frame protection is supported; an
//! open network needs none of it. Every field read from a frame is bounds
//! checked: the frames come off the air.

use crate::fido_aes;
use crate::sha1::{hmac_sha1, HmacSha1};

pub const CIPHER_CCMP: u32 = 0x000f_ac04;
pub const AKM_PSK: u32 = 0x000f_ac02;
const OUI: [u8; 3] = [0x00, 0x0f, 0xac];
const ELEMENT_SSID: u8 = 0;
const ELEMENT_RSN: u8 = 48;
const ELEMENT_VENDOR: u8 = 221;
/// RSN capabilities: management frame protection required.
const MFPR: u16 = 1 << 6;

/// The RSN element td-netd associates with and repeats in message 2:
/// version 1, CCMP group, one CCMP pairwise, one PSK AKM, no capabilities.
pub const STATION_RSN: [u8; 22] = [
    ELEMENT_RSN,
    20,
    1,
    0,
    0x00,
    0x0f,
    0xac,
    4,
    1,
    0,
    0x00,
    0x0f,
    0xac,
    4,
    1,
    0,
    0x00,
    0x0f,
    0xac,
    2,
    0,
    0,
];

/// The elements of an IE list as (id, body); stops at a truncated element.
pub fn elements(ies: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut rest = ies;
    std::iter::from_fn(move || {
        let id = *rest.first()?;
        let len = usize::from(*rest.get(1)?);
        let body = rest.get(2..2 + len)?;
        rest = rest.get(2 + len..).unwrap_or(&[]);
        Some((id, body))
    })
}

fn element(ies: &[u8], wanted: u8) -> Option<&[u8]> {
    let mut found = None;
    for (id, body) in elements(ies) {
        if id == wanted {
            found = Some(body);
            break;
        }
    }
    found
}

pub fn ssid(ies: &[u8]) -> Option<&[u8]> {
    element(ies, ELEMENT_SSID)
}

/// A parsed RSN element body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rsn {
    pub group: u32,
    pub pairwise: Vec<u32>,
    pub akm: Vec<u32>,
    pub capabilities: u16,
}

fn suite(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// Parse an RSN element body (id and length stripped). Optional trailing
/// fields take their defaults when absent, as 9.4.2.24 allows.
pub fn parse_rsn(body: &[u8]) -> Option<Rsn> {
    let version = u16::from_le_bytes(body.get(..2)?.try_into().ok()?);
    if version != 1 {
        return None;
    }
    let mut rest = body.get(2..)?;
    let group = if rest.is_empty() {
        CIPHER_CCMP
    } else {
        let g = suite(rest)?;
        rest = rest.get(4..)?;
        g
    };
    let list = |rest: &mut &[u8]| -> Option<Vec<u32>> {
        if rest.is_empty() {
            return None;
        }
        let count = usize::from(u16::from_le_bytes(rest.get(..2)?.try_into().ok()?));
        let mut out = Vec::with_capacity(count.min(16));
        for i in 0..count {
            out.push(suite(rest.get(2 + 4 * i..)?)?);
        }
        *rest = rest.get(2 + 4 * count..)?;
        Some(out)
    };
    let pairwise = list(&mut rest).unwrap_or_else(|| vec![CIPHER_CCMP]);
    let akm = if rest.is_empty() {
        vec![0x000f_ac01]
    } else {
        list(&mut rest)?
    };
    let capabilities = match rest.get(..2) {
        Some(bytes) => u16::from_le_bytes(bytes.try_into().ok()?),
        None => 0,
    };
    Some(Rsn {
        group,
        pairwise,
        akm,
        capabilities,
    })
}

/// What a BSS asks of a station, as td-netd can or cannot meet it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Security {
    Open,
    Wpa2Psk(Rsn),
    Unsupported(&'static str),
}

pub fn security(ies: &[u8]) -> Security {
    if let Some(body) = element(ies, ELEMENT_RSN) {
        let Some(rsn) = parse_rsn(body) else {
            return Security::Unsupported("malformed RSN element");
        };
        if !rsn.akm.contains(&AKM_PSK) {
            return Security::Unsupported("not WPA2-Personal (WPA3-only or enterprise)");
        }
        if !rsn.pairwise.contains(&CIPHER_CCMP) || rsn.group != CIPHER_CCMP {
            return Security::Unsupported("needs a cipher other than CCMP");
        }
        if rsn.capabilities & MFPR != 0 {
            return Security::Unsupported("requires management frame protection");
        }
        return Security::Wpa2Psk(rsn);
    }
    for (id, body) in elements(ies) {
        if id == ELEMENT_VENDOR && body.get(..4) == Some(&[0x00, 0x50, 0xf2, 0x01]) {
            return Security::Unsupported("WPA1 only");
        }
    }
    Security::Open
}

/// The strongest BSS advertising `wanted` whose security matches the
/// configuration (a PMK, or none for an open network), or why there is none.
pub fn choose<'a>(
    bsses: &'a [crate::genl::Bss],
    wanted: &[u8],
    protected: bool,
) -> Result<&'a crate::genl::Bss, String> {
    let mut best: Option<&crate::genl::Bss> = None;
    let mut refusal = None;
    for bss in bsses {
        if ssid(&bss.ies) != Some(wanted) {
            continue;
        }
        let fits = match (security(&bss.ies), protected) {
            (Security::Wpa2Psk(_), true) | (Security::Open, false) => true,
            (Security::Unsupported(why), _) => {
                refusal = Some(why.to_string());
                false
            }
            (Security::Open, true) => {
                refusal = Some("the network is open; join it with --open".into());
                false
            }
            (Security::Wpa2Psk(_), false) => {
                refusal = Some("the network needs a passphrase".into());
                false
            }
        };
        if fits && best.is_none_or(|b| bss.signal_mbm > b.signal_mbm) {
            best = Some(bss);
        }
    }
    best.ok_or_else(|| refusal.unwrap_or_else(|| "network not found".into()))
}

/// IEEE 802.11 PRF over HMAC-SHA1 (12.7.1.2): HMAC(K, A || 0 || B || i) for
/// i = 0, 1, ... concatenated and truncated to `out`.
pub fn prf(key: &[u8], label: &[u8], data: &[u8], out: &mut [u8]) {
    let mac = HmacSha1::new(key);
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let mut block = mac.mac(&[label, &[0], data, &[i as u8]]);
        for (dst, byte) in chunk.iter_mut().zip(&block) {
            *dst = *byte;
        }
        block.fill(0);
        std::hint::black_box(&mut block);
    }
}

/// The CCMP pairwise transient key: KCK, KEK and TK.
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 16],
}

impl Drop for Ptk {
    fn drop(&mut self) {
        self.kck.fill(0);
        self.kek.fill(0);
        self.tk.fill(0);
        std::hint::black_box(&mut self.kck);
        std::hint::black_box(&mut self.kek);
        std::hint::black_box(&mut self.tk);
    }
}

pub fn derive_ptk(
    pmk: &[u8; 32],
    aa: &[u8; 6],
    spa: &[u8; 6],
    anonce: &[u8; 32],
    snonce: &[u8; 32],
) -> Ptk {
    let (lo_mac, hi_mac) = if aa < spa { (aa, spa) } else { (spa, aa) };
    let (lo_nonce, hi_nonce) = if anonce < snonce {
        (anonce, snonce)
    } else {
        (snonce, anonce)
    };
    let mut data = [0u8; 76];
    for (dst, src) in data.iter_mut().zip(
        lo_mac
            .iter()
            .chain(hi_mac)
            .chain(lo_nonce.iter())
            .chain(hi_nonce.iter()),
    ) {
        *dst = *src;
    }
    let mut out = [[0u8; 16]; 3];
    prf(
        pmk,
        b"Pairwise key expansion",
        &data,
        out.as_flattened_mut(),
    );
    let [kck, kek, tk] = out;
    let ptk = Ptk { kck, kek, tk };
    out.as_flattened_mut().fill(0);
    std::hint::black_box(&mut out);
    ptk
}

// ── EAPOL-Key frames (802.1X-2020 11.3, 802.11-2020 12.7.2) ─────────────────

const EAPOL_KEY: u8 = 3;
const DESCRIPTOR_RSN: u8 = 2;
/// EAPOL header (4) plus the key descriptor up to the key data length.
const KEY_HEADER: usize = 4 + 95;
const MIC_AT: usize = 4 + 77;

pub const INFO_VERSION_MASK: u16 = 0x7;
pub const INFO_VERSION_AES_HMAC_SHA1: u16 = 2;
pub const INFO_PAIRWISE: u16 = 1 << 3;
pub const INFO_INSTALL: u16 = 1 << 6;
pub const INFO_ACK: u16 = 1 << 7;
pub const INFO_MIC: u16 = 1 << 8;
pub const INFO_SECURE: u16 = 1 << 9;
pub const INFO_ERROR: u16 = 1 << 10;
pub const INFO_REQUEST: u16 = 1 << 11;
pub const INFO_ENCRYPTED: u16 = 1 << 12;

/// A parsed EAPOL-Key frame; `data` is its key data field.
#[derive(Debug)]
pub struct KeyFrame<'a> {
    pub protocol: u8,
    pub info: u16,
    pub replay: u64,
    pub nonce: [u8; 32],
    pub rsc: [u8; 8],
    pub data: &'a [u8],
}

pub fn parse_key(frame: &[u8]) -> Option<KeyFrame<'_>> {
    if *frame.get(1)? != EAPOL_KEY || *frame.get(4)? != DESCRIPTOR_RSN {
        return None;
    }
    let body = usize::from(u16::from_be_bytes(frame.get(2..4)?.try_into().ok()?));
    let data_len = usize::from(u16::from_be_bytes(frame.get(97..99)?.try_into().ok()?));
    if body != 95 + data_len || frame.len() < KEY_HEADER + data_len {
        return None;
    }
    Some(KeyFrame {
        protocol: *frame.first()?,
        info: u16::from_be_bytes(frame.get(5..7)?.try_into().ok()?),
        replay: u64::from_be_bytes(frame.get(9..17)?.try_into().ok()?),
        nonce: frame.get(17..49)?.try_into().ok()?,
        rsc: frame.get(65..73)?.try_into().ok()?,
        data: frame.get(KEY_HEADER..KEY_HEADER + data_len)?,
    })
}

/// Build an EAPOL-Key frame with key length zero and, given a KCK, its MIC.
pub fn build_key(
    protocol: u8,
    info: u16,
    replay: u64,
    nonce: &[u8; 32],
    data: &[u8],
    kck: Option<&[u8; 16]>,
) -> Vec<u8> {
    let data_len = u16::try_from(data.len()).unwrap_or(u16::MAX);
    let mut frame = Vec::with_capacity(KEY_HEADER + data.len());
    frame.push(protocol);
    frame.push(EAPOL_KEY);
    frame.extend_from_slice(&(95 + data_len).to_be_bytes());
    frame.push(DESCRIPTOR_RSN);
    frame.extend_from_slice(&info.to_be_bytes());
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&replay.to_be_bytes());
    frame.extend_from_slice(nonce);
    // IV, RSC, reserved, MIC.
    frame.extend_from_slice(&[0u8; 16 + 8 + 8 + 16]);
    frame.extend_from_slice(&data_len.to_be_bytes());
    frame.extend_from_slice(data);
    if let Some(kck) = kck {
        let mic = hmac_sha1(kck, &[&frame]);
        if let (Some(slot), Some(mic)) = (frame.get_mut(MIC_AT..MIC_AT + 16), mic.get(..16)) {
            slot.copy_from_slice(mic);
        }
    }
    frame
}

/// Whether `frame`'s MIC is HMAC-SHA1-128 under `kck` over the frame with
/// the MIC field zeroed, compared in constant time.
pub fn verify_mic(frame: &[u8], kck: &[u8; 16]) -> bool {
    let Some(received) = frame.get(MIC_AT..MIC_AT + 16) else {
        return false;
    };
    let mut zeroed = frame.to_vec();
    if let Some(slot) = zeroed.get_mut(MIC_AT..MIC_AT + 16) {
        slot.fill(0);
    }
    let expected = hmac_sha1(kck, &[&zeroed]);
    let difference = received
        .iter()
        .zip(&expected)
        .fold(0u8, |d, (a, b)| d | (a ^ b));
    std::hint::black_box(difference) == 0
}

/// A group temporal key from a decrypted key data field's GTK KDE.
#[derive(Debug, PartialEq, Eq)]
pub struct Gtk {
    pub index: u8,
    pub key: Vec<u8>,
}

impl Drop for Gtk {
    fn drop(&mut self) {
        self.key.fill(0);
        std::hint::black_box(&mut self.key);
    }
}

/// The RSN element and GTK KDE in a decrypted key data field. Padding (a
/// 0xdd byte then zeros) ends it.
fn key_data(data: &[u8]) -> (Option<&[u8]>, Option<Gtk>) {
    let mut rsn = None;
    let mut gtk = None;
    let mut rest = data;
    while let (Some(&id), Some(&len)) = (rest.first(), rest.get(1)) {
        if id == ELEMENT_VENDOR && len == 0 {
            break;
        }
        let Some(body) = rest.get(2..2 + usize::from(len)) else {
            break;
        };
        if id == ELEMENT_RSN && rsn.is_none() {
            rsn = Some(body);
        } else if id == ELEMENT_VENDOR
            && body.get(..3) == Some(&OUI)
            && body.get(3) == Some(&1)
            && gtk.is_none()
        {
            if let (Some(flags), Some(key)) = (body.get(4), body.get(6..)) {
                gtk = Some(Gtk {
                    index: flags & 0x3,
                    key: key.to_vec(),
                });
            }
        }
        rest = rest.get(2 + usize::from(len)..).unwrap_or(&[]);
    }
    (rsn, gtk)
}

/// What the I/O layer must do for a handshake step, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Transmit an EAPOL frame to the authenticator; unencrypted when no
    /// pairwise key is installed yet.
    Send {
        frame: Vec<u8>,
        encrypt: bool,
    },
    PairwiseKey {
        tk: [u8; 16],
    },
    GroupKey {
        index: u8,
        key: Vec<u8>,
        rsc: [u8; 6],
    },
    /// Open the controlled port: the first handshake completed.
    Authorize,
}

/// One association's supplicant: the PMK, both addresses and the AP's RSN
/// element from its beacon, which message 3 must repeat.
pub struct Supplicant {
    pmk: [u8; 32],
    aa: [u8; 6],
    spa: [u8; 6],
    ap_rsn: Rsn,
    anonce: Option<[u8; 32]>,
    /// The PTK message 1 derived, with its SNonce, until message 3 proves it.
    pending: Option<(Ptk, [u8; 32])>,
    ptk: Option<Ptk>,
    /// The installed PTK's ANonce, and whether it came from the first
    /// handshake: a repeated message 3 for it is answered with message 4
    /// again, in the clear when the authenticator has no PTK yet.
    installed: Option<([u8; 32], bool)>,
    /// The installed GTK, so a repeat is not installed again with the
    /// frame's RSC (CVE-2017-13080 on drivers that would accept it).
    gtk: Option<(u8, Vec<u8>)>,
    replay: Option<u64>,
}

impl Drop for Supplicant {
    fn drop(&mut self) {
        self.pmk.fill(0);
        std::hint::black_box(&mut self.pmk);
        if let Some((_, key)) = &mut self.gtk {
            key.fill(0);
            std::hint::black_box(key);
        }
    }
}

impl Supplicant {
    pub fn new(pmk: [u8; 32], aa: [u8; 6], spa: [u8; 6], ap_rsn: Rsn) -> Self {
        Self {
            pmk,
            aa,
            spa,
            ap_rsn,
            anonce: None,
            pending: None,
            ptk: None,
            installed: None,
            gtk: None,
            replay: None,
        }
    }

    fn fresh(&self, replay: u64) -> bool {
        self.replay.is_none_or(|last| replay > last)
    }

    /// Message 2 went out and message 3 has not come: an authenticator
    /// that drops the association now has most likely refused message 2's
    /// MIC, which is what a wrong passphrase looks like from here.
    pub fn awaiting_message_3(&self) -> bool {
        self.pending.is_some() && self.ptk.is_none()
    }

    /// Handle one EAPOL frame from the authenticator, `encrypted` as the
    /// kernel received it. `snonce` must be 32 fresh random bytes; it is
    /// used only by message 1. A frame that is not for this handshake, or
    /// fails a check, is refused with why, and changes nothing but a
    /// pending message 1.
    pub fn receive(
        &mut self,
        frame: &[u8],
        encrypted: bool,
        snonce: [u8; 32],
    ) -> Result<Vec<Action>, &'static str> {
        // The MIC covers the EAPOL frame alone; padding after its body is
        // not part of it.
        let body = frame
            .get(2..4)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(|b| usize::from(u16::from_be_bytes(b)))
            .ok_or("not an RSN EAPOL-Key frame")?;
        let frame = frame
            .get(..4 + body)
            .ok_or("EAPOL frame shorter than its body")?;
        let key = parse_key(frame).ok_or("not an RSN EAPOL-Key frame")?;
        if key.info & INFO_VERSION_MASK != INFO_VERSION_AES_HMAC_SHA1 {
            return Err("unsupported key descriptor version");
        }
        if key.info & (INFO_ERROR | INFO_REQUEST) != 0 || key.info & INFO_ACK == 0 {
            return Err("not an authenticator's key message");
        }
        if !self.fresh(key.replay) {
            return Err("replayed key message");
        }
        let protocol = key.protocol.clamp(1, 2);
        let pairwise = key.info & INFO_PAIRWISE != 0;
        let mic = key.info & INFO_MIC != 0;
        // With a PTK installed the authenticator encrypts, except that its
        // repeat of the first handshake's message 3 (message 4 lost) comes
        // before it has the PTK; that one is still MIC-checked.
        let first_repeat = pairwise
            && mic
            && self.pending.is_none()
            && self.installed.is_some_and(|(_, first)| first);
        if self.ptk.is_some() && !encrypted && !first_repeat {
            return Err("unencrypted key message after the pairwise key");
        }
        match (pairwise, mic) {
            (true, false) => Ok(self.message_1(&key, protocol, snonce)),
            (true, true) => self.message_3(frame, &key, protocol),
            (false, true) => self.group_1(frame, &key, protocol),
            (false, false) => Err("group key message without a MIC"),
        }
    }

    /// Message 1 is unauthenticated. A repeat with the same ANonce (the
    /// authenticator retransmitting after a lost message 2) keeps the
    /// pending SNonce, so it derives the same PTK and cannot strand a
    /// message 3 already on its way.
    fn message_1(&mut self, key: &KeyFrame<'_>, protocol: u8, fresh: [u8; 32]) -> Vec<Action> {
        let snonce = match &self.pending {
            Some((_, snonce)) if self.anonce == Some(key.nonce) => *snonce,
            _ => fresh,
        };
        let ptk = derive_ptk(&self.pmk, &self.aa, &self.spa, &key.nonce, &snonce);
        let info = INFO_VERSION_AES_HMAC_SHA1 | INFO_PAIRWISE | INFO_MIC;
        let reply = build_key(
            protocol,
            info,
            key.replay,
            &snonce,
            &STATION_RSN,
            Some(&ptk.kck),
        );
        self.anonce = Some(key.nonce);
        self.pending = Some((ptk, snonce));
        vec![Action::Send {
            frame: reply,
            encrypt: self.ptk.is_some(),
        }]
    }

    fn message_3(
        &mut self,
        frame: &[u8],
        key: &KeyFrame<'_>,
        protocol: u8,
    ) -> Result<Vec<Action>, &'static str> {
        let wanted = INFO_INSTALL | INFO_SECURE | INFO_ENCRYPTED;
        if key.info & wanted != wanted {
            return Err("message 3 lacks install, secure or encrypted key data");
        }
        if self.pending.is_none() {
            return self.repeated_message_3(frame, key, protocol);
        }
        let (ptk, _) = self.pending.as_ref().ok_or("message 3 without message 1")?;
        if self.anonce != Some(key.nonce) {
            return Err("message 3 does not continue message 1");
        }
        if !verify_mic(frame, &ptk.kck) {
            return Err("message 3 MIC mismatch");
        }
        let mut plain = unwrap(&ptk.kek, key.data)?;
        let (rsn, gtk) = key_data(&plain);
        let rsn = rsn.and_then(parse_rsn);
        plain.fill(0);
        std::hint::black_box(&mut plain);
        let rsn = rsn.ok_or("message 3 carries no RSN element")?;
        if rsn.group != self.ap_rsn.group
            || rsn.pairwise != self.ap_rsn.pairwise
            || rsn.akm != self.ap_rsn.akm
        {
            return Err("message 3 RSN element differs from the beacon's (downgrade?)");
        }
        let gtk = gtk.ok_or("message 3 carries no GTK")?;
        if gtk.key.len() != 16 {
            return Err("GTK is not a CCMP-128 key");
        }
        let first = self.ptk.is_none();
        let info = INFO_VERSION_AES_HMAC_SHA1 | INFO_PAIRWISE | INFO_MIC | INFO_SECURE;
        let reply = build_key(protocol, info, key.replay, &[0; 32], &[], Some(&ptk.kck));
        let tk = ptk.tk;
        let mut rsc = [0u8; 6];
        rsc.copy_from_slice(key.rsc.get(..6).unwrap_or(&[0; 6]));
        self.replay = Some(key.replay);
        self.ptk = self.pending.take().map(|(ptk, _)| ptk);
        self.installed = Some((key.nonce, first));
        let mut actions = vec![
            Action::Send {
                frame: reply,
                encrypt: !first,
            },
            Action::PairwiseKey { tk },
        ];
        actions.extend(self.new_gtk(gtk.index, &gtk.key, rsc));
        if first {
            actions.push(Action::Authorize);
        }
        Ok(actions)
    }

    /// The authenticator lost message 4 and repeated message 3 for the
    /// PTK already installed: send message 4 again and install nothing.
    fn repeated_message_3(
        &mut self,
        frame: &[u8],
        key: &KeyFrame<'_>,
        protocol: u8,
    ) -> Result<Vec<Action>, &'static str> {
        let (Some(ptk), Some((anonce, first))) = (&self.ptk, self.installed) else {
            return Err("message 3 without message 1");
        };
        if anonce != key.nonce {
            return Err("message 3 does not continue message 1");
        }
        if !verify_mic(frame, &ptk.kck) {
            return Err("message 3 MIC mismatch");
        }
        let info = INFO_VERSION_AES_HMAC_SHA1 | INFO_PAIRWISE | INFO_MIC | INFO_SECURE;
        let reply = build_key(protocol, info, key.replay, &[0; 32], &[], Some(&ptk.kck));
        self.replay = Some(key.replay);
        Ok(vec![Action::Send {
            frame: reply,
            encrypt: !first,
        }])
    }

    /// Install a GTK unless it is the one already installed.
    fn new_gtk(&mut self, index: u8, key: &[u8], rsc: [u8; 6]) -> Option<Action> {
        if self
            .gtk
            .as_ref()
            .is_some_and(|(i, k)| *i == index && k.as_slice() == key)
        {
            return None;
        }
        if let Some((_, old)) = &mut self.gtk {
            old.fill(0);
        }
        self.gtk = Some((index, key.to_vec()));
        Some(Action::GroupKey {
            index,
            key: key.to_vec(),
            rsc,
        })
    }

    fn group_1(
        &mut self,
        frame: &[u8],
        key: &KeyFrame<'_>,
        protocol: u8,
    ) -> Result<Vec<Action>, &'static str> {
        let ptk = self
            .ptk
            .as_ref()
            .ok_or("group key message before the 4-way handshake")?;
        if key.info & (INFO_SECURE | INFO_ENCRYPTED) != INFO_SECURE | INFO_ENCRYPTED {
            return Err("group message lacks secure or encrypted key data");
        }
        if !verify_mic(frame, &ptk.kck) {
            return Err("group message MIC mismatch");
        }
        let mut plain = unwrap(&ptk.kek, key.data)?;
        let (_, gtk) = key_data(&plain);
        plain.fill(0);
        std::hint::black_box(&mut plain);
        let gtk = gtk.ok_or("group message carries no GTK")?;
        if gtk.key.len() != 16 {
            return Err("GTK is not a CCMP-128 key");
        }
        let info = INFO_VERSION_AES_HMAC_SHA1 | INFO_MIC | INFO_SECURE;
        let reply = build_key(protocol, info, key.replay, &[0; 32], &[], Some(&ptk.kck));
        let mut rsc = [0u8; 6];
        rsc.copy_from_slice(key.rsc.get(..6).unwrap_or(&[0; 6]));
        self.replay = Some(key.replay);
        let mut actions: Vec<Action> = self.new_gtk(gtk.index, &gtk.key, rsc).into_iter().collect();
        actions.push(Action::Send {
            frame: reply,
            encrypt: true,
        });
        Ok(actions)
    }
}

fn unwrap(kek: &[u8; 16], data: &[u8]) -> Result<Vec<u8>, &'static str> {
    let len = data.len().checked_sub(8).ok_or("key data too short")?;
    let mut plain = vec![0u8; len];
    fido_aes::key_unwrap_128(kek, data, &mut plain)?;
    Ok(plain)
}

#[cfg(test)]
mod tests;
