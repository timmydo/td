//! Website commands for a root-owned physical key. Only the typed fields
//! admitted by web_request are encoded. A browser never supplies PIN traffic.
use crate::{
    fido_cbor::{self, Encoder, Value},
    fido_ctap::{encode_descriptor, Es256PublicKey},
    fido_pin::{self, Pin},
    fido_transaction::Channel,
    web_request::{Create, Get},
};

#[derive(Clone, Copy)]
pub enum Ceremony<'r, 'a> {
    Register(&'r Create<'a>),
    Sign(&'r Get<'a>),
}
impl fido_pin::Operation for Ceremony<'_, '_> {
    fn rp(&self) -> &str {
        match self {
            Self::Register(r) => r.rp(),
            Self::Sign(r) => r.rp(),
        }
    }
    fn challenge(&self) -> &[u8; 32] {
        match self {
            Self::Register(r) => r.client_data_hash(),
            Self::Sign(r) => r.client_data_hash(),
        }
    }
    fn permission(&self) -> u64 {
        match self {
            Self::Register(_) => 1,
            Self::Sign(_) => 2,
        }
    }
}

fn descriptors(e: &mut Encoder, key: u64, ids: &[&[u8]]) -> Result<(), String> {
    if !ids.is_empty() {
        e.head(0, key)?;
        e.head(4, ids.len() as u64)?;
        for id in ids {
            encode_descriptor(e, id)?;
        }
    }
    Ok(())
}

impl Ceremony<'_, '_> {
    fn encode(
        &self,
        mac: Option<(u64, &[u8])>,
        builtin: bool,
        out: &mut [u8],
    ) -> Result<usize, String> {
        let mut e = Encoder::new();
        let (command, auth_field) = match self {
            Self::Register(r) => {
                let ids = r.exclude().ids();
                e.head(
                    5,
                    5 + u64::from(!ids.is_empty()) + 2 * u64::from(mac.is_some()),
                )?;
                e.head(0, 1)?;
                e.bytes(r.client_data_hash())?;
                e.head(0, 2)?;
                e.head(5, 1)?;
                e.text("id")?;
                e.text(r.rp())?;
                e.head(0, 3)?;
                e.head(5, 1 + u64::from(r.user_name().is_some()))?;
                e.text("id")?;
                e.bytes(r.user_id())?;
                if let Some(name) = r.user_name() {
                    e.text("name")?;
                    e.text(name)?;
                }
                e.head(0, 4)?;
                e.head(4, 1)?;
                e.head(5, 2)?;
                e.text("alg")?;
                e.head(1, 6)?;
                e.text("type")?;
                e.text("public-key")?;
                descriptors(&mut e, 5, ids)?;
                let uv = builtin || (mac.is_none() && r.verify_user());
                e.head(0, 7)?;
                e.head(5, u64::from(r.discoverable()) + u64::from(uv))?;
                if r.discoverable() {
                    e.text("rk")?;
                    e.boolean(true)?;
                }
                if uv {
                    e.text("uv")?;
                    e.boolean(true)?;
                }
                (1, 8)
            }
            Self::Sign(r) => {
                let ids = r.allow().ids();
                let uv = builtin || (mac.is_none() && r.verify_user());
                e.head(
                    5,
                    3 + u64::from(!ids.is_empty()) + 2 * u64::from(mac.is_some()),
                )?;
                e.head(0, 1)?;
                e.text(r.rp())?;
                e.head(0, 2)?;
                e.bytes(r.client_data_hash())?;
                descriptors(&mut e, 3, ids)?;
                e.head(0, 5)?;
                e.head(5, 1 + u64::from(uv))?;
                e.text("up")?;
                e.boolean(r.presence())?;
                if uv {
                    e.text("uv")?;
                    e.boolean(true)?;
                }
                (2, 6)
            }
        };
        if let Some((protocol, mac)) = mac {
            e.head(0, auth_field)?;
            e.bytes(mac)?;
            e.head(0, auth_field + 1)?;
            e.head(0, protocol)?;
        }
        let mut body = e.finish()?;
        let size = body.len() + 1;
        let result = out
            .get_mut(..size)
            .and_then(|bytes| bytes.split_first_mut())
            .ok_or("website request exceeds key limit")
            .map(|(op, dst)| {
                *op = command;
                dst.copy_from_slice(&body);
                size
            });
        crate::crypto::zero(&mut body);
        result.map_err(Into::into)
    }

    fn reply(&self, body: &[u8], pin: bool) -> Result<Vec<u8>, String> {
        use fido_pin::Operation;
        let v = fido_cbor::decode(body)?;
        let data = v.required(&Value::Unsigned(2))?.bytes()?;
        let flags = *data.get(32).ok_or("short authenticator data")?;
        if data.get(..32) != Some(crate::digest(self.rp().as_bytes()).as_slice())
            || flags & 0x80 != 0
            || flags & 0x18 == 0x10
        {
            return Err("invalid website authenticator data".into());
        }
        let mut e = Encoder::new();
        match self {
            Self::Register(r) => {
                if data.len() < 55
                    || flags & 0x41 != 0x41
                    || ((pin || r.verify_user()) && flags & 4 == 0)
                {
                    return Err("refused website registration flags".into());
                }
                let n = usize::from(u16::from_be_bytes(
                    data.get(53..55)
                        .ok_or("missing ID length")?
                        .try_into()
                        .map_err(|_| "missing ID length")?,
                ));
                let after = 55 + n;
                if !(1..=1023).contains(&n)
                    || r.exclude()
                        .ids()
                        .contains(&data.get(55..after).ok_or("short credential ID")?)
                {
                    return Err("invalid new credential ID".into());
                }
                let key = data.get(after..).ok_or("missing website public key")?;
                if key != Es256PublicKey::from_cose(key)?.canonical_cose() {
                    return Err("unexpected website key metadata".into());
                }
                // Strip identifying attestation and the model AAGUID.
                let mut anonymous = [0; fido_cbor::MAX_BYTES];
                let anonymous = anonymous
                    .get_mut(..data.len())
                    .ok_or("oversize authenticator data")?;
                anonymous.copy_from_slice(data);
                anonymous
                    .get_mut(37..53)
                    .ok_or("short authenticator data")?
                    .fill(0);
                e.head(5, 3)?;
                e.head(0, 1)?;
                e.text("none")?;
                e.head(0, 2)?;
                e.bytes(anonymous)?;
                e.head(0, 3)?;
                e.head(5, 0)?;
            }
            Self::Sign(r) => {
                if data.len() != 37
                    || flags & 0x40 != 0
                    || (r.presence() && flags & 1 == 0)
                    || (!r.presence() && flags & 5 != 0)
                    || ((pin || r.verify_user()) && flags & 4 == 0)
                {
                    return Err("refused website assertion flags".into());
                }
                if v.get(&Value::Unsigned(5))?
                    .is_some_and(|n| n != &Value::Unsigned(1))
                {
                    return Err("one website account must be selected".into());
                }
                let ids = r.allow().ids();
                let id = match (v.get(&Value::Unsigned(1))?, ids) {
                    (Some(d), _) if d.required(&Value::Text("type"))?.text()? == "public-key" => {
                        d.required(&Value::Text("id"))?.bytes()?
                    }
                    (None, [id]) => *id,
                    _ => return Err("missing website credential".into()),
                };
                let sig = v.required(&Value::Unsigned(3))?.bytes()?;
                if !(1..=1023).contains(&id.len())
                    || (!ids.is_empty() && !ids.contains(&id))
                    || sig.is_empty()
                {
                    return Err("unexpected website assertion".into());
                }
                let user = if r.presence() {
                    v.get(&Value::Unsigned(4))?
                        .map(|u| u.required(&Value::Text("id"))?.bytes())
                        .transpose()?
                } else {
                    None
                };
                if user.is_some_and(|u| !(1..=64).contains(&u.len()))
                    || (ids.is_empty() && user.is_none())
                {
                    return Err("missing website account handle".into());
                }
                e.head(5, 3 + u64::from(user.is_some()))?;
                e.head(0, 1)?;
                encode_descriptor(&mut e, id)?;
                e.head(0, 2)?;
                e.bytes(data)?;
                e.head(0, 3)?;
                e.bytes(sig)?;
                if let Some(u) = user {
                    e.head(0, 4)?;
                    e.head(5, 1)?;
                    e.text("id")?;
                    e.bytes(u)?;
                }
            }
        }
        e.finish()
    }
}

/// Use a single admitted, locked channel and one fresh authorization. The
/// caller has presented this ceremony, or admitted a restricted handle probe.
pub fn perform(
    intent: Ceremony<'_, '_>,
    channel: &mut impl Channel,
    prompt: impl FnOnce(u8) -> Result<Pin, String>,
    entropy: &mut impl FnMut(&mut [u8]) -> Result<(), String>,
) -> Result<(u8, Vec<u8>), String> {
    let silent = matches!(intent,Ceremony::Sign(r) if !r.presence());
    if let Ceremony::Sign(r) = intent {
        if silent
            && (r.allow().ids().is_empty()
                || r.verify_user()
                || r.client_data_hash() != &crate::digest(b""))
        {
            return Err("invalid silent website probe".into());
        }
    }
    let info = channel.exchange(&[4])?;
    let info_bytes = info.as_ref();
    let (0, body) = info_bytes
        .split_first()
        .ok_or("short website key information")?
    else {
        return Err("website key information refused".into());
    };
    let v = fido_cbor::decode(body)?;
    let max = v
        .get(&Value::Unsigned(5))?
        .map(Value::unsigned)
        .transpose()?
        .unwrap_or(1024)
        .min(fido_cbor::MAX_BYTES as u64) as usize;
    let list = match intent {
        Ceremony::Register(r) => r.exclude().ids(),
        Ceremony::Sign(r) => r.allow().ids(),
    };
    let list_max = v
        .get(&Value::Unsigned(7))?
        .map(Value::unsigned)
        .transpose()?
        .unwrap_or(1);
    let id_max = v
        .get(&Value::Unsigned(8))?
        .map(Value::unsigned)
        .transpose()?
        .unwrap_or(1023);
    if list.len() as u64 > list_max || list.iter().any(|id| id.len() as u64 > id_max) {
        return Ok((0x15, Vec::new()));
    }
    let send = |op: &Ceremony<'_, '_>,
                mac: Option<(u64, &[u8])>,
                builtin: bool,
                channel: &mut dyn Channel| {
        let mut buffer = [0; fido_cbor::MAX_BYTES];
        let n = op.encode(
            mac,
            builtin,
            buffer.get_mut(..max).ok_or("website limit overflow")?,
        )?;
        channel.check().map_err(|_| "website request expired")?;
        let result = channel.exchange(buffer.get(..n).ok_or("short website buffer")?);
        crate::crypto::zero(&mut buffer);
        let result = result?;
        channel.check().map_err(|_| "website request expired")?;
        let (&status, body) = result.as_ref().split_first().ok_or("empty website reply")?;
        if status != 0 {
            return Ok((status, Vec::new()));
        }
        Ok((0, op.reply(body, builtin || mac.is_some())?))
    };
    if silent {
        return send(&intent, None, false, channel);
    }
    let need_pin = match intent {
        Ceremony::Register(_) => true,
        Ceremony::Sign(r) => r.verify_user(),
    };
    fido_pin::website_mac(info_bytes, intent, channel, prompt, entropy, send, need_pin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fido_virtual::{Config, Virtual},
        web_request::{parse, Request},
    };
    const SITE: &str = "accounts.example";
    fn register(rp: &str) -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(5, 4).unwrap();
        e.head(0, 1).unwrap();
        e.bytes(&[7; 32]).unwrap();
        e.head(0, 2).unwrap();
        e.head(5, 1).unwrap();
        e.text("id").unwrap();
        e.text(rp).unwrap();
        e.head(0, 3).unwrap();
        e.head(5, 1).unwrap();
        e.text("id").unwrap();
        e.bytes(b"alice").unwrap();
        e.head(0, 4).unwrap();
        e.head(4, 1).unwrap();
        e.head(5, 2).unwrap();
        e.text("alg").unwrap();
        e.head(1, 6).unwrap();
        e.text("type").unwrap();
        e.text("public-key").unwrap();
        let mut q = vec![1];
        q.extend(e.finish().unwrap());
        q
    }
    fn get(id: &[u8], silent: bool, hash: &[u8; 32]) -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(5, 4).unwrap();
        e.head(0, 1).unwrap();
        e.text(SITE).unwrap();
        e.head(0, 2).unwrap();
        e.bytes(hash).unwrap();
        descriptors(&mut e, 3, &[id]).unwrap();
        e.head(0, 5).unwrap();
        e.head(5, 1).unwrap();
        if silent {
            e.text("up").unwrap();
            e.boolean(false).unwrap();
        } else {
            e.text("uv").unwrap();
            e.boolean(true).unwrap();
        }
        let mut q = vec![2];
        q.extend(e.finish().unwrap());
        q
    }
    // A device that requires its fingerprint, and a device that refuses
    // the platform key agreement before any PIN may be collected.
    struct CapabilityKey {
        inner: crate::fido_virtual::Link,
        bio: bool,
        last: u8,
    }
    impl Channel for CapabilityKey {
        fn check(&self) -> Result<(), crate::fido_device::Interruption> {
            Ok(())
        }
        fn exchange(&mut self, bytes: &[u8]) -> Result<crate::fido_hid::Message, String> {
            self.last = bytes[0];
            if self.bio && bytes == [4] {
                let mut e = Encoder::new();
                e.head(5, 3)?;
                e.head(0, 1)?;
                e.head(4, 1)?;
                e.text("FIDO_2_1")?;
                e.head(0, 3)?;
                e.bytes(&[0; 16])?;
                e.head(0, 4)?;
                e.head(5, 3)?;
                for k in ["uv", "clientPin", "noMcGaPermissionsWithClientPin"] {
                    e.text(k)?;
                    e.boolean(true)?;
                }
                let mut info = vec![0];
                info.extend(e.finish()?);
                return Ok(crate::fido_fixtures::message(&info));
            }
            if self.bio {
                assert_eq!(self.last, 1);
                let v = fido_cbor::decode(&bytes[1..])?;
                assert!(
                    v.required(&Value::Unsigned(7))?
                        .required(&Value::Text("uv"))?
                        == &Value::Simple(21)
                );
                assert!(v.get(&Value::Unsigned(8))?.is_none());
                return Ok(crate::fido_fixtures::message(&[0x27]));
            }
            if self.last == 6 {
                let v = fido_cbor::decode(&bytes[1..])?;
                if v.required(&Value::Unsigned(2))?.unsigned()? == 2 {
                    return Ok(crate::fido_fixtures::message(&[0x27]));
                }
            }
            self.inner.exchange(bytes)
        }
    }
    #[test]
    fn biometric_only_permissions_and_refused_agreement_never_collect_a_pin() {
        let q = register(SITE);
        let Request::Create(r) = parse(&q).unwrap() else {
            panic!()
        };
        let key = Virtual::new(
            Config {
                rp: SITE,
                ..Default::default()
            },
            Some(b"123456"),
            "bio",
        );
        for bio in [true, false] {
            let mut ch = CapabilityKey {
                inner: key.link(),
                bio,
                last: 0,
            };
            let result = perform(
                Ceremony::Register(&r),
                &mut ch,
                |_| panic!("unexpected PIN prompt"),
                &mut |_| panic!(),
            );
            if bio {
                assert_eq!(result.unwrap(), (0x27, vec![]));
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn mediated_key_scopes_each_pin_to_one_website_command() {
        let key = Virtual::new(
            Config {
                rp: SITE,
                ..Default::default()
            },
            Some(b"123456"),
            "web",
        );
        let mut channel = key.link();
        let mut source = 0u8;
        let mut entropy = |buf: &mut [u8]| {
            source = source.wrapping_add(1);
            buf.fill(source);
            Ok(())
        };
        let pin = |retries| {
            assert!(retries > 0);
            Pin::new(b"123456".as_slice().into())
        };
        assert!(parse(&register(crate::fido_ctap::RP_ID)).is_err());
        let q = register(SITE);
        let Request::Create(r) = parse(&q).unwrap() else {
            panic!()
        };
        let (status, att) =
            perform(Ceremony::Register(&r), &mut channel, pin, &mut entropy).unwrap();
        assert_eq!(status, 0);
        let att = fido_cbor::decode(&att).unwrap();
        assert_eq!(
            att.required(&Value::Unsigned(1)).unwrap().text().unwrap(),
            "none"
        );
        let ad = att.required(&Value::Unsigned(2)).unwrap().bytes().unwrap();
        assert_eq!(&ad[37..53], &[0; 16]);
        assert_eq!(ad[32] & 0x45, 0x45);
        let n = u16::from_be_bytes(ad[53..55].try_into().unwrap()) as usize;
        let id = &ad[55..55 + n];
        let start = key.transcript().len();
        let q = get(id, true, &[9; 32]);
        let Request::Get(r) = parse(&q).unwrap() else {
            panic!()
        };
        assert!(perform(Ceremony::Sign(&r), &mut channel, |_| panic!(), &mut entropy).is_err());
        assert_eq!(key.transcript().len(), start);
        let q = get(id, true, &crate::digest(b""));
        let Request::Get(r) = parse(&q).unwrap() else {
            panic!()
        };
        let (status, body) =
            perform(Ceremony::Sign(&r), &mut channel, |_| panic!(), &mut entropy).unwrap();
        assert_eq!(status, 0);
        assert!(fido_cbor::decode(&body)
            .unwrap()
            .get(&Value::Unsigned(4))
            .unwrap()
            .is_none());
        let q = get(id, false, &[42; 32]);
        let Request::Get(r) = parse(&q).unwrap() else {
            panic!()
        };
        let (status, body) = perform(Ceremony::Sign(&r), &mut channel, pin, &mut entropy).unwrap();
        assert_eq!(status, 0);
        let v = fido_cbor::decode(&body).unwrap();
        assert_eq!(
            v.required(&Value::Unsigned(2)).unwrap().bytes().unwrap()[32] & 5,
            5
        );
        let transcript = key.transcript();
        let mut permissions = Vec::new();
        for (request, _) in &transcript {
            assert!(matches!(request[0], 1 | 2 | 4 | 6));
            if request[0] != 6 {
                continue;
            }
            let v = fido_cbor::decode(&request[1..]).unwrap();
            let sub = v.required(&Value::Unsigned(2)).unwrap().unsigned().unwrap();
            assert!(matches!(sub, 1 | 2 | 9));
            if sub == 9 {
                assert_eq!(
                    v.required(&Value::Unsigned(10)).unwrap().text().unwrap(),
                    SITE
                );
                permissions.push(v.required(&Value::Unsigned(9)).unwrap().unsigned().unwrap());
            }
        }
        assert_eq!(permissions, [1, 2]);
        let mut prompts = 0;
        assert!(perform(
            Ceremony::Sign(&r),
            &mut channel,
            |_| {
                prompts += 1;
                Pin::new(b"wrong PIN".as_slice().into())
            },
            &mut entropy
        )
        .is_err());
        assert_eq!(prompts, 1);
    }
}
