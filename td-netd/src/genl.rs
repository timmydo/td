//! Generic netlink framing and the slice of nl80211 td-netd speaks: pure
//! encoding and bounded parsing, no I/O. Numbers are Linux 7.2.13's UAPI
//! (`linux/netlink.h`, `linux/genetlink.h`, `linux/nl80211.h`).

pub const NLMSG_HDR: usize = 16;
pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_DUMP: u16 = 0x300;

pub const GENL_ID_CTRL: u16 = 0x10;
pub const CTRL_CMD_GETFAMILY: u8 = 3;
pub const CTRL_ATTR_FAMILY_ID: u16 = 1;
pub const CTRL_ATTR_FAMILY_NAME: u16 = 2;
pub const CTRL_ATTR_MCAST_GROUPS: u16 = 7;
pub const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
pub const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

/// nl80211 commands.
pub const CMD_NEW_KEY: u8 = 11;
pub const CMD_SET_STATION: u8 = 18;
pub const CMD_GET_SCAN: u8 = 32;
pub const CMD_TRIGGER_SCAN: u8 = 33;
pub const CMD_NEW_SCAN_RESULTS: u8 = 34;
pub const CMD_SCAN_ABORTED: u8 = 35;
pub const CMD_DEAUTHENTICATE: u8 = 39;
pub const CMD_DISASSOCIATE: u8 = 40;
pub const CMD_CONNECT: u8 = 46;
pub const CMD_DISCONNECT: u8 = 48;
pub const CMD_CONTROL_PORT_FRAME: u8 = 129;

/// nl80211 attributes.
pub const ATTR_IFINDEX: u16 = 3;
pub const ATTR_MAC: u16 = 6;
pub const ATTR_WIPHY_FREQ: u16 = 38;
pub const ATTR_IE: u16 = 42;
pub const ATTR_SCAN_SSIDS: u16 = 45;
pub const ATTR_BSS: u16 = 47;
pub const ATTR_FRAME: u16 = 51;
pub const ATTR_SSID: u16 = 52;
pub const ATTR_AUTH_TYPE: u16 = 53;
pub const ATTR_REASON_CODE: u16 = 54;
pub const ATTR_TIMED_OUT: u16 = 65;
pub const ATTR_USE_MFP: u16 = 66;
pub const ATTR_STA_FLAGS2: u16 = 67;
pub const ATTR_CONTROL_PORT: u16 = 68;
pub const ATTR_PRIVACY: u16 = 70;
pub const ATTR_STATUS_CODE: u16 = 72;
pub const ATTR_CIPHER_SUITES_PAIRWISE: u16 = 73;
pub const ATTR_CIPHER_SUITE_GROUP: u16 = 74;
pub const ATTR_WPA_VERSIONS: u16 = 75;
pub const ATTR_AKM_SUITES: u16 = 76;
pub const ATTR_KEY: u16 = 80;
pub const ATTR_CONTROL_PORT_ETHERTYPE: u16 = 102;
pub const ATTR_CONTROL_PORT_NO_ENCRYPT: u16 = 103;
pub const ATTR_SOCKET_OWNER: u16 = 204;
pub const ATTR_CONTROL_PORT_OVER_NL80211: u16 = 264;

pub const BSS_BSSID: u16 = 1;
pub const BSS_FREQUENCY: u16 = 2;
pub const BSS_INFORMATION_ELEMENTS: u16 = 6;
pub const BSS_SIGNAL_MBM: u16 = 7;

pub const KEY_DATA: u16 = 1;
pub const KEY_IDX: u16 = 2;
pub const KEY_CIPHER: u16 = 3;
pub const KEY_SEQ: u16 = 4;
pub const KEY_TYPE: u16 = 7;
pub const KEYTYPE_GROUP: u32 = 0;
pub const KEYTYPE_PAIRWISE: u32 = 1;

pub const STA_FLAG_AUTHORIZED: u32 = 1;
pub const AUTHTYPE_OPEN_SYSTEM: u32 = 0;
pub const WPA_VERSION_2: u32 = 2;
pub const MFP_NO: u32 = 0;
pub const ETH_P_PAE: u16 = 0x888e;

const NLA_HDR: usize = 4;
/// Strips NLA_F_NESTED and NLA_F_NET_BYTEORDER from an attribute type.
const NLA_TYPE_MASK: u16 = 0x3fff;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// One request: a netlink header, a generic netlink header and attributes.
pub struct Builder {
    buf: Vec<u8>,
    nests: Vec<usize>,
}

impl Builder {
    pub fn new(family: u16, flags: u16, seq: u32, cmd: u8) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&family.to_ne_bytes());
        buf.extend_from_slice(&flags.to_ne_bytes());
        buf.extend_from_slice(&seq.to_ne_bytes());
        buf.extend_from_slice(&0u32.to_ne_bytes());
        buf.extend_from_slice(&[cmd, 1, 0, 0]);
        Self {
            buf,
            nests: Vec::new(),
        }
    }

    pub fn bytes(&mut self, kind: u16, data: &[u8]) -> &mut Self {
        let len = NLA_HDR + data.len();
        self.buf
            .extend_from_slice(&u16::try_from(len).unwrap_or(u16::MAX).to_ne_bytes());
        self.buf.extend_from_slice(&kind.to_ne_bytes());
        self.buf.extend_from_slice(data);
        self.buf.resize(align4(self.buf.len()), 0);
        self
    }

    pub fn u8(&mut self, kind: u16, value: u8) -> &mut Self {
        self.bytes(kind, &[value])
    }

    pub fn u32(&mut self, kind: u16, value: u32) -> &mut Self {
        self.bytes(kind, &value.to_ne_bytes())
    }

    pub fn u16(&mut self, kind: u16, value: u16) -> &mut Self {
        self.bytes(kind, &value.to_ne_bytes())
    }

    pub fn flag(&mut self, kind: u16) -> &mut Self {
        self.bytes(kind, &[])
    }

    pub fn begin(&mut self, kind: u16) -> &mut Self {
        self.nests.push(self.buf.len());
        self.bytes(kind | 0x8000, &[])
    }

    pub fn end(&mut self) -> &mut Self {
        if let Some(start) = self.nests.pop() {
            let len = u16::try_from(self.buf.len() - start).unwrap_or(u16::MAX);
            if let Some(slot) = self.buf.get_mut(start..start + 2) {
                slot.copy_from_slice(&len.to_ne_bytes());
            }
        }
        self
    }

    pub fn finish(&mut self) -> Vec<u8> {
        while !self.nests.is_empty() {
            self.end();
        }
        let len = u32::try_from(self.buf.len()).unwrap_or(u32::MAX);
        if let Some(slot) = self.buf.get_mut(..4) {
            slot.copy_from_slice(&len.to_ne_bytes());
        }
        std::mem::take(&mut self.buf)
    }
}

/// One received netlink message.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    pub kind: u16,
    pub seq: u32,
    pub payload: &'a [u8],
}

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

/// The messages in one datagram, in order; iteration stops at the first
/// malformed header.
pub fn messages(buf: &[u8]) -> impl Iterator<Item = Message<'_>> {
    let mut rest = buf;
    std::iter::from_fn(move || {
        let len = u32_at(rest, 0)? as usize;
        if len < NLMSG_HDR {
            return None;
        }
        let message = Message {
            kind: u16_at(rest, 4)?,
            seq: u32_at(rest, 8)?,
            payload: rest.get(NLMSG_HDR..len)?,
        };
        rest = rest.get(align4(len)..).unwrap_or(&[]);
        Some(message)
    })
}

/// An NLMSG_ERROR payload's code: zero is an acknowledgement, otherwise a
/// negated errno.
pub fn error_code(payload: &[u8]) -> Option<i32> {
    Some(i32::from_ne_bytes(payload.get(..4)?.try_into().ok()?))
}

/// A generic netlink payload's command and attributes.
pub fn genl(payload: &[u8]) -> Option<(u8, &[u8])> {
    Some((*payload.first()?, payload.get(4..)?))
}

/// The attributes in `buf`, type flags stripped; iteration stops at the
/// first malformed one.
pub fn attrs(buf: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut rest = buf;
    std::iter::from_fn(move || {
        let len = u16_at(rest, 0)? as usize;
        if len < NLA_HDR {
            return None;
        }
        let kind = u16_at(rest, 2)? & NLA_TYPE_MASK;
        let data = rest.get(NLA_HDR..len)?;
        rest = rest.get(align4(len)..).unwrap_or(&[]);
        Some((kind, data))
    })
}

pub fn attr(buf: &[u8], kind: u16) -> Option<&[u8]> {
    let mut found = None;
    for (k, data) in attrs(buf) {
        if k == kind {
            found = Some(data);
            break;
        }
    }
    found
}

pub fn attr_u32(buf: &[u8], kind: u16) -> Option<u32> {
    u32_at(attr(buf, kind)?, 0)
}

pub fn attr_u16(buf: &[u8], kind: u16) -> Option<u16> {
    u16_at(attr(buf, kind)?, 0)
}

/// A generic netlink family's id and multicast groups, from the controller's
/// reply to CTRL_CMD_GETFAMILY.
#[derive(Debug, PartialEq, Eq)]
pub struct Family {
    pub id: u16,
    pub groups: Vec<(String, u32)>,
}

impl Family {
    pub fn group(&self, name: &str) -> Option<u32> {
        let mut found = None;
        for (group, id) in &self.groups {
            if group == name {
                found = Some(*id);
                break;
            }
        }
        found
    }
}

pub fn family_request(seq: u32, name: &str) -> Vec<u8> {
    let mut name = name.as_bytes().to_vec();
    name.push(0);
    Builder::new(
        GENL_ID_CTRL,
        NLM_F_REQUEST | NLM_F_ACK,
        seq,
        CTRL_CMD_GETFAMILY,
    )
    .bytes(CTRL_ATTR_FAMILY_NAME, &name)
    .finish()
}

pub fn parse_family(attributes: &[u8]) -> Option<Family> {
    let id = attr_u16(attributes, CTRL_ATTR_FAMILY_ID)?;
    let mut groups = Vec::new();
    if let Some(list) = attr(attributes, CTRL_ATTR_MCAST_GROUPS) {
        for (_, group) in attrs(list) {
            let name = attr(group, CTRL_ATTR_MCAST_GRP_NAME)?;
            let name = name.strip_suffix(&[0]).unwrap_or(name);
            let name = String::from_utf8(name.to_vec()).ok()?;
            groups.push((name, attr_u32(group, CTRL_ATTR_MCAST_GRP_ID)?));
        }
    }
    Some(Family { id, groups })
}

/// One scanned BSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub frequency: u32,
    pub signal_mbm: i32,
    pub ies: Vec<u8>,
}

/// A GET_SCAN dump entry's BSS, from its attributes.
pub fn parse_bss(attributes: &[u8]) -> Option<Bss> {
    let bss = attr(attributes, ATTR_BSS)?;
    Some(Bss {
        bssid: attr(bss, BSS_BSSID)?.try_into().ok()?,
        frequency: attr_u32(bss, BSS_FREQUENCY)?,
        signal_mbm: attr_u32(bss, BSS_SIGNAL_MBM).map_or(i32::MIN, |v| v as i32),
        ies: attr(bss, BSS_INFORMATION_ELEMENTS)?.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn builder_frames_headers_attributes_and_nests() {
        let message = Builder::new(0x1c, NLM_F_REQUEST | NLM_F_ACK, 7, CMD_NEW_KEY)
            .u32(ATTR_IFINDEX, 3)
            .begin(ATTR_KEY)
            .bytes(KEY_DATA, &[1, 2, 3, 4, 5])
            .u32(KEY_IDX, 1)
            .end()
            .flag(ATTR_PRIVACY)
            .finish();
        assert_eq!(message.len() % 4, 0);
        let parsed: Vec<_> = messages(&message).collect();
        assert_eq!(parsed.len(), 1);
        let m = parsed[0];
        assert_eq!((m.kind, m.seq), (0x1c, 7));
        assert_eq!(u16::from_ne_bytes(message[6..8].try_into().unwrap()), 5);
        assert_eq!(
            u32::from_ne_bytes(message[..4].try_into().unwrap()) as usize,
            message.len()
        );
        let (cmd, attributes) = genl(m.payload).unwrap();
        assert_eq!(cmd, CMD_NEW_KEY);
        assert_eq!(attr_u32(attributes, ATTR_IFINDEX), Some(3));
        let key = attr(attributes, ATTR_KEY).unwrap();
        assert_eq!(attr(key, KEY_DATA), Some(&[1u8, 2, 3, 4, 5][..]));
        assert_eq!(attr_u32(key, KEY_IDX), Some(1));
        assert_eq!(attr(attributes, ATTR_PRIVACY), Some(&[][..]));
        // The nest carries NLA_F_NESTED on the wire.
        let at = 16 + 4 + 8;
        assert_eq!(
            u16::from_ne_bytes(message[at + 2..at + 4].try_into().unwrap()),
            ATTR_KEY | 0x8000
        );
    }

    #[test]
    fn parsing_stops_at_malformed_input_without_panicking() {
        let good = Builder::new(1, 0, 1, 1).u32(ATTR_IFINDEX, 9).finish();
        for cut in 0..good.len() {
            let _ = messages(&good[..cut]).count();
            let _ = attrs(&good[..cut]).count();
        }
        let mut bad = good.clone();
        bad[0] = 3;
        assert_eq!(messages(&bad).count(), 0);
        // An attribute claiming more than remains ends iteration.
        assert_eq!(attrs(&[200, 0, 1, 0, 1, 2]).count(), 0);
        assert_eq!(attrs(&[2, 0, 1, 0]).count(), 0);
    }

    #[test]
    fn several_messages_in_one_datagram() {
        // NLM_F_MULTI, as a dump's entries carry it.
        let multi = 0x2;
        let mut datagram = Builder::new(5, multi, 2, CMD_GET_SCAN).u32(1, 1).finish();
        datagram.extend(Builder::new(5, multi, 2, CMD_GET_SCAN).u32(1, 2).finish());
        let mut done = vec![0u8; 20];
        done[..4].copy_from_slice(&20u32.to_ne_bytes());
        done[4..6].copy_from_slice(&NLMSG_DONE.to_ne_bytes());
        done[8..12].copy_from_slice(&2u32.to_ne_bytes());
        datagram.extend(done);
        let kinds: Vec<_> = messages(&datagram).map(|m| m.kind).collect();
        assert_eq!(kinds, vec![5, 5, NLMSG_DONE]);
    }

    #[test]
    fn controller_reply_yields_family_and_groups() {
        let reply = Builder::new(GENL_ID_CTRL, 0, 1, 1)
            .u16(CTRL_ATTR_FAMILY_ID, 0x1f)
            .bytes(CTRL_ATTR_FAMILY_NAME, b"nl80211\0")
            .begin(CTRL_ATTR_MCAST_GROUPS)
            .begin(1)
            .bytes(CTRL_ATTR_MCAST_GRP_NAME, b"config\0")
            .u32(CTRL_ATTR_MCAST_GRP_ID, 4)
            .end()
            .begin(2)
            .bytes(CTRL_ATTR_MCAST_GRP_NAME, b"mlme\0")
            .u32(CTRL_ATTR_MCAST_GRP_ID, 6)
            .end()
            .end()
            .finish();
        let m = messages(&reply).next().unwrap();
        let family = parse_family(genl(m.payload).unwrap().1).unwrap();
        assert_eq!(family.id, 0x1f);
        assert_eq!(family.group("mlme"), Some(6));
        assert_eq!(family.group("scan"), None);
        let request = family_request(9, "nl80211");
        let m = messages(&request).next().unwrap();
        assert_eq!(m.kind, GENL_ID_CTRL);
        let (cmd, attributes) = genl(m.payload).unwrap();
        assert_eq!(cmd, CTRL_CMD_GETFAMILY);
        assert_eq!(
            attr(attributes, CTRL_ATTR_FAMILY_NAME),
            Some(&b"nl80211\0"[..])
        );
    }

    #[test]
    fn scan_entry_yields_its_bss() {
        let entry = Builder::new(0x1f, 0x2, 3, CMD_NEW_SCAN_RESULTS)
            .begin(ATTR_BSS)
            .bytes(BSS_BSSID, &[2, 0, 0, 0, 0, 1])
            .u32(BSS_FREQUENCY, 5180)
            .u32(BSS_SIGNAL_MBM, (-5500i32) as u32)
            .bytes(BSS_INFORMATION_ELEMENTS, &[0, 2, b'h', b'i'])
            .end()
            .finish();
        let m = messages(&entry).next().unwrap();
        let bss = parse_bss(genl(m.payload).unwrap().1).unwrap();
        assert_eq!(bss.bssid, [2, 0, 0, 0, 0, 1]);
        assert_eq!(bss.frequency, 5180);
        assert_eq!(bss.signal_mbm, -5500);
        assert_eq!(bss.ies, vec![0, 2, b'h', b'i']);
    }
}
