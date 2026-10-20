//! Resumable issuance. The caller checkpoints state before every external effect.
use super::{
    http::{Exchange, Response},
    wire, Error,
};
use crate::config::endpoint::HttpsUri;
use td_crypto::{Crypto, Provider, ServerIdentity};
use td_json::{Json, ObjectBuilder};

pub struct Settings {
    pub directory: String,
    pub contact: String,
    pub names: Vec<String>,
}
impl Settings {
    pub fn validate(&self) -> Result<(), Error> {
        HttpsUri::parse(&self.directory).map_err(|_| Error::Invalid)?;
        crate::config::values::mailbox_key(
            &self.contact,
            &mut [0; crate::format::row::MAX_ADDRESS],
        )
        .map_err(|_| Error::Invalid)?;
        if self.names.is_empty() || self.names.len() > 32 {
            return Err(Error::Invalid);
        }
        for (i, name) in self.names.iter().enumerate() {
            crate::config::values::dns_name(name).map_err(|_| Error::Invalid)?;
            if name.bytes().any(|b| b.is_ascii_uppercase())
                || self.names.get(..i).ok_or(Error::Invalid)?.contains(name)
            {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
    pub fn identity(&self) -> Result<[u8; 32], Error> {
        self.validate()?;
        let mut names = self.names.clone();
        names.sort();
        super::digest(
            ObjectBuilder::new()
                .set("directory", self.directory.as_str())
                .set("contact", self.contact.as_str())
                .set(
                    "names",
                    Json::Arr(names.into_iter().map(Json::Str).collect()),
                )
                .build()
                .to_string()
                .as_bytes(),
        )
    }
}

/// Secrets deliberately have no Debug implementation; database storage is private.
pub struct State {
    pub account_key: String,
    pub account: String,
    pub order: String,
    pub certificate_key: String,
    pub authorization: String,
    pub token: String,
    pub key_authorization: String,
    pub next_ms: i64,
    pub failures: u32,
    pub order_started_ms: i64,
    pub cursor: usize,
    pub expires_ms: i64,
}
impl Drop for State {
    fn drop(&mut self) {
        for secret in [&mut self.account_key, &mut self.certificate_key] {
            let mut bytes = std::mem::take(secret).into_bytes();
            bytes.fill(0);
            std::hint::black_box(&mut bytes);
        }
    }
}
fn generate_key() -> Result<String, Error> {
    let mut bytes = [0; td_crypto::P256_PKCS8_CAPACITY];
    let result = (|| {
        let n = Provider.generate_p256(&mut bytes)?;
        String::from_utf8(wire::pem(
            "PRIVATE KEY",
            bytes.get(..n).ok_or(Error::Invalid)?,
        )?)
        .map_err(|_| Error::Invalid)
    })();
    bytes.fill(0);
    std::hint::black_box(&mut bytes);
    result
}
impl State {
    pub fn initial() -> Result<Self, Error> {
        Ok(Self {
            account_key: generate_key()?,
            account: String::new(),
            order: String::new(),
            certificate_key: generate_key()?,
            authorization: String::new(),
            token: String::new(),
            key_authorization: String::new(),
            next_ms: 0,
            failures: 0,
            order_started_ms: 0,
            cursor: 0,
            expires_ms: 0,
        })
    }
    pub fn encode(&self) -> Vec<u8> {
        ObjectBuilder::new()
            .set("account_key", self.account_key.as_str())
            .set("account", self.account.as_str())
            .set("order", self.order.as_str())
            .set("certificate_key", self.certificate_key.as_str())
            .set("authorization", self.authorization.as_str())
            .set("token", self.token.as_str())
            .set("key_authorization", self.key_authorization.as_str())
            .set("next_ms", self.next_ms)
            .set("failures", self.failures as u64)
            .set("order_started_ms", self.order_started_ms)
            .set("cursor", self.cursor as u64)
            .set("expires_ms", self.expires_ms)
            .build()
            .to_vec()
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 32768 {
            return Err(Error::Capacity);
        }
        let json = td_json::parse_slice(bytes).map_err(|_| Error::Invalid)?;
        let text = |key| -> Result<String, Error> { Ok(text(&json, key)?.to_owned()) };
        let number = |key| -> Result<i64, Error> {
            json.get(key)
                .and_then(Json::as_i64)
                .filter(|n| *n >= 0)
                .ok_or(Error::Invalid)
        };
        let state = Self {
            account_key: text("account_key")?,
            account: text("account")?,
            order: text("order")?,
            certificate_key: text("certificate_key")?,
            authorization: text("authorization")?,
            token: text("token")?,
            key_authorization: text("key_authorization")?,
            next_ms: number("next_ms")?,
            failures: u32::try_from(number("failures")?).map_err(|_| Error::Invalid)?,
            order_started_ms: number("order_started_ms")?,
            cursor: usize::try_from(number("cursor")?).map_err(|_| Error::Invalid)?,
            expires_ms: number("expires_ms")?,
        };
        for key in [&state.account_key, &state.certificate_key] {
            if key.len() > 1024 {
                return Err(Error::Invalid);
            }
            Provider.load_p256_pem(key.as_bytes())?;
        }
        for url in [&state.account, &state.order, &state.authorization] {
            if !url.is_empty() {
                HttpsUri::parse(url).map_err(|_| Error::Invalid)?;
            }
        }
        if !state.token.is_empty() {
            wire::token(&state.token, 22, 256)?;
        }
        if state.key_authorization.len() > 300 {
            return Err(Error::Invalid);
        }
        Ok(state)
    }
    fn retire_order(&mut self, next_ms: i64) -> Result<(), Error> {
        self.certificate_key = generate_key()?;
        self.order.clear();
        self.order_started_ms = 0;
        self.token.clear();
        self.key_authorization.clear();
        self.authorization.clear();
        self.cursor = 0;
        self.next_ms = next_ms;
        Ok(())
    }
    pub fn failed(&mut self, now: i64) {
        self.failures = self.failures.saturating_add(1);
        let delay = 30_000i64.saturating_mul(1i64 << self.failures.min(10));
        self.next_ms = self
            .next_ms
            .max(now.saturating_add(delay.min(6 * 60 * 60 * 1000)));
    }
}
/// Save is durable and fenced. Publication saves state and the active pair together.
pub trait Persistence {
    fn save(&mut self, state: &State, certificate: Option<(&[u8], &[u8])>) -> Result<(), Error>;
}
fn text<'a>(json: &'a Json, key: &str) -> Result<&'a str, Error> {
    json.get(key).and_then(Json::as_str).ok_or(Error::Protocol)
}
fn endpoint(settings: &Settings, url: &str) -> Result<String, Error> {
    let root = HttpsUri::parse(&settings.directory).map_err(|_| Error::Invalid)?;
    let uri = HttpsUri::parse(url).map_err(|_| Error::Protocol)?;
    if !root.same_origin(uri) {
        return Err(Error::Protocol);
    }
    Ok(url.to_owned())
}
fn json(response: &Response) -> Result<Json, Error> {
    if response.body.len() > 65536 {
        return Err(Error::Capacity);
    }
    td_json::parse_slice(&response.body).map_err(|_| Error::Protocol)
}
fn retry(response: &Response, now: i64) -> Result<i64, Error> {
    let seconds = match response.retry_after.as_deref() {
        None => 5,
        Some(value) if value.bytes().all(|b| b.is_ascii_digit()) => value
            .parse::<u64>()
            .map_err(|_| Error::Protocol)?
            .clamp(1, 86400),
        Some(value) => {
            let at = td_civil::parse_rfc2822(value)
                .and_then(|seconds| seconds.checked_mul(1000))
                .ok_or(Error::Protocol)?;
            return Ok(at.clamp(now.saturating_add(1000), now.saturating_add(86400000)));
        }
    };
    Ok(now.saturating_add((seconds * 1000) as i64))
}
fn rate_limit(response: &Response) -> Result<(), Error> {
    if matches!(response.status, 429 | 503) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Deadline)?
            .as_millis();
        return Err(Error::RetryAt(retry(
            response,
            i64::try_from(now).map_err(|_| Error::Deadline)?,
        )?));
    }
    Ok(())
}
fn contact_uri(contact: &str) -> Result<String, Error> {
    let (local, domain) = contact.rsplit_once('@').ok_or(Error::Invalid)?;
    let mut result = String::from("mailto:");
    use std::fmt::Write;
    for byte in local.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            result.push(char::from(byte));
        } else {
            write!(&mut result, "%{byte:02X}").map_err(|_| Error::Capacity)?;
        }
    }
    result.push('@');
    result.push_str(domain);
    Ok(result)
}

fn signed(
    settings: &Settings,
    state: &State,
    exchange: &mut impl Exchange,
    nonce_url: &str,
    url: &str,
    payload: &[u8],
    register: bool,
) -> Result<Response, Error> {
    endpoint(settings, url)?;
    let key = Provider.load_p256_pem(state.account_key.as_bytes())?;
    for attempt in 0..2 {
        let nonce = exchange.exchange("HEAD", nonce_url, &[])?;
        rate_limit(&nonce)?;
        if !(200..300).contains(&nonce.status) {
            return Err(Error::Protocol);
        }
        let nonce = nonce.nonce.as_deref().ok_or(Error::Protocol)?;
        let body = super::jws(
            &key,
            url,
            nonce,
            if register {
                None
            } else {
                Some(state.account.as_str())
            },
            payload,
        )?;
        let response = exchange.exchange("POST", url, &body)?;
        rate_limit(&response)?;
        if attempt == 0
            && response.status == 400
            && json(&response)?.get("type").and_then(Json::as_str)
                == Some("urn:ietf:params:acme:error:badNonce")
        {
            continue;
        }
        return Ok(response);
    }
    Err(Error::Protocol)
}
fn identifiers(settings: &Settings, value: &Json) -> Result<(), Error> {
    let entries = value
        .get("identifiers")
        .and_then(Json::as_arr)
        .ok_or(Error::Protocol)?;
    if entries.len() != settings.names.len() {
        return Err(Error::Protocol);
    }
    let mut seen = Vec::new();
    for entry in entries {
        let name = text(entry, "value")?;
        if text(entry, "type")? != "dns"
            || !settings.names.iter().any(|n| n == name)
            || seen.contains(&name)
        {
            return Err(Error::Protocol);
        }
        seen.push(name);
    }
    Ok(())
}
/// One bounded progress turn. Every key is durably stored before use. HTTP-01
/// serves the saved challenge while CA work runs; it must already own port 80.
pub fn advance(
    settings: &Settings,
    state: &mut State,
    exchange: &mut impl Exchange,
    store: &mut impl Persistence,
    now: i64,
) -> Result<(), Error> {
    settings.validate()?;
    if now < 0 || now < state.next_ms {
        return Ok(());
    }
    for url in [&state.account, &state.order, &state.authorization] {
        if !url.is_empty() {
            endpoint(settings, url)?;
        }
    }
    let directory = exchange.exchange("GET", &settings.directory, &[])?;
    rate_limit(&directory)?;
    if directory.status != 200 {
        return Err(Error::Protocol);
    }
    let directory = json(&directory)?;
    let nonce_url = endpoint(settings, text(&directory, "newNonce")?)?;
    if state.account.is_empty() {
        let url = endpoint(settings, text(&directory, "newAccount")?)?;
        let payload = ObjectBuilder::new()
            .set("termsOfServiceAgreed", true)
            .set(
                "contact",
                Json::Arr(vec![Json::Str(contact_uri(&settings.contact)?)]),
            )
            .build()
            .to_vec();
        let response = signed(settings, state, exchange, &nonce_url, &url, &payload, true)?;
        if !matches!(response.status, 200 | 201) {
            return Err(Error::Protocol);
        }
        state.account = endpoint(
            settings,
            response.location.as_deref().ok_or(Error::Protocol)?,
        )?;
        state.next_ms = now;
        store.save(state, None)?;
        return Ok(());
    }
    if state.order.is_empty() {
        // Persist the attempt marker before newOrder. An ambiguous response is
        // quarantined for a day, rather than repeatedly issuing new orders.
        if state.order_started_ms != 0 && now < state.order_started_ms.saturating_add(86400000) {
            state.next_ms = state.order_started_ms.saturating_add(86400000);
            store.save(state, None)?;
            return Ok(());
        }
        state.order_started_ms = now;
        store.save(state, None)?;
        let url = endpoint(settings, text(&directory, "newOrder")?)?;
        let names = Json::Arr(
            settings
                .names
                .iter()
                .map(|n| {
                    ObjectBuilder::new()
                        .set("type", "dns")
                        .set("value", n.as_str())
                        .build()
                })
                .collect(),
        );
        let payload = ObjectBuilder::new()
            .set("identifiers", names)
            .build()
            .to_vec();
        let response = signed(settings, state, exchange, &nonce_url, &url, &payload, false)?;
        if response.status != 201 {
            return Err(Error::Protocol);
        }
        identifiers(settings, &json(&response)?)?;
        state.order = endpoint(
            settings,
            response.location.as_deref().ok_or(Error::Protocol)?,
        )?;
        state.next_ms = now;
        store.save(state, None)?;
        return Ok(());
    }
    let response = signed(
        settings,
        state,
        exchange,
        &nonce_url,
        &state.order,
        &[],
        false,
    )?;
    if response.status != 200 {
        return Err(Error::Protocol);
    }
    let order = json(&response)?;
    identifiers(settings, &order)?;
    state.next_ms = retry(&response, now)?;
    match text(&order, "status")? {
        "pending" => {
            let auths = order
                .get("authorizations")
                .and_then(Json::as_arr)
                .ok_or(Error::Protocol)?;
            if auths.is_empty() || auths.len() > 32 {
                return Err(Error::Protocol);
            }
            for auth in auths.iter().skip(state.cursor % auths.len()).take(1) {
                let auth = endpoint(settings, auth.as_str().ok_or(Error::Protocol)?)?;
                let response = signed(settings, state, exchange, &nonce_url, &auth, &[], false)?;
                if response.status != 200 {
                    return Err(Error::Protocol);
                }
                let authorization = json(&response)?;
                let identifier = authorization.get("identifier").ok_or(Error::Protocol)?;
                if text(identifier, "type")? != "dns"
                    || !settings
                        .names
                        .iter()
                        .any(|n| n == text(identifier, "value").unwrap_or(""))
                    || authorization.get("wildcard").and_then(Json::as_bool) == Some(true)
                {
                    return Err(Error::Protocol);
                }
                if text(&authorization, "status")? == "valid" {
                    state.cursor = (state.cursor % auths.len() + 1) % auths.len();
                    continue;
                }
                if text(&authorization, "status")? != "pending" {
                    return Err(Error::Protocol);
                }
                let challenges = authorization
                    .get("challenges")
                    .and_then(Json::as_arr)
                    .ok_or(Error::Protocol)?;
                if challenges.len() > 16 {
                    return Err(Error::Capacity);
                }
                let challenge = challenges
                    .iter()
                    .find(|c| c.get("type").and_then(Json::as_str) == Some("http-01"))
                    .ok_or(Error::Protocol)?;
                let token = text(challenge, "token")?;
                wire::token(token, 22, 256)?;
                let url = endpoint(settings, text(challenge, "url")?)?;
                state.authorization = auth;
                state.token = token.to_owned();
                let key = Provider.load_p256_pem(state.account_key.as_bytes())?;
                state.key_authorization = super::key_authorization(&key, token)?;
                store.save(state, None)?;
                if text(challenge, "status")? == "pending" {
                    let response =
                        signed(settings, state, exchange, &nonce_url, &url, b"{}", false)?;
                    if response.status != 200 {
                        return Err(Error::Protocol);
                    }
                }
                return Ok(());
            }
        }
        "ready" => {
            let key = Provider.load_p256_pem(state.certificate_key.as_bytes())?;
            let names: Vec<_> = settings.names.iter().map(String::as_str).collect();
            let csr = wire::base64(&super::csr(&key, &names)?, true)?;
            let payload = ObjectBuilder::new().set("csr", csr).build().to_vec();
            let url = endpoint(settings, text(&order, "finalize")?)?;
            let response = signed(settings, state, exchange, &nonce_url, &url, &payload, false)?;
            if response.status != 200 {
                return Err(Error::Protocol);
            }
            identifiers(settings, &json(&response)?)?;
        }
        "processing" => (),
        "valid" => {
            let url = endpoint(settings, text(&order, "certificate")?)?;
            let response = signed(settings, state, exchange, &nonce_url, &url, &[], false)?;
            if response.status != 200
                || response
                    .content_type
                    .as_deref()
                    .and_then(|s| s.split(';').next())
                    != Some("application/pem-certificate-chain")
            {
                return Err(Error::Protocol);
            }
            let names: Vec<_> = settings.names.iter().map(String::as_str).collect();
            let identity = match ServerIdentity::from_pem(
                &response.body,
                state.certificate_key.as_bytes(),
                &names,
                Some((now / 1000) as u64),
            ) {
                Ok(identity) => identity,
                Err(td_crypto::TlsError::Verification(td_crypto::VerificationFailure::Expired)) => {
                    state.retire_order(now)?;
                    return store.save(state, None);
                }
                Err(_) => return Err(Error::Crypto),
            };
            identity
                .check_exact_dns_names()
                .map_err(|_| Error::Crypto)?;
            let (start, end) = identity.validity();
            let lifetime = end.checked_sub(start).ok_or(Error::Protocol)?;
            if lifetime < 3600 {
                return Err(Error::Protocol);
            }
            let jitter = u64::from(
                super::digest(state.order.as_bytes())?
                    .first()
                    .copied()
                    .ok_or(Error::Crypto)?,
            );
            let renew = start
                .saturating_add(lifetime / 2)
                .saturating_add(lifetime / 20 * jitter / 255);
            let renew = i64::try_from(renew)
                .map_err(|_| Error::Invalid)?
                .checked_mul(1000)
                .ok_or(Error::Invalid)?;
            let mut published = State::decode(&state.encode())?;
            published.retire_order(now)?;
            published.next_ms = renew.max(now);
            published.expires_ms = i64::try_from(end)
                .map_err(|_| Error::Invalid)?
                .checked_mul(1000)
                .ok_or(Error::Invalid)?;
            published.failures = 0;
            store.save(
                &published,
                Some((&response.body, state.certificate_key.as_bytes())),
            )?;
            *state = published;
            return Ok(());
        }
        "invalid" => state.retire_order(now.saturating_add(86400000))?,
        _ => return Err(Error::Protocol),
    }
    store.save(state, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_dates_and_contact_encoding_are_bounded() -> Result<(), Error> {
        assert_eq!(
            contact_uri("\"a@b\"@Example.test")?,
            "mailto:%22a%40b%22@Example.test"
        );
        let mut response = Response {
            status: 429,
            nonce: None,
            location: None,
            retry_after: Some("3600".into()),
            content_type: None,
            body: Vec::new(),
        };
        assert_eq!(retry(&response, 1000)?, 3_601_000);
        response.retry_after = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
        assert_eq!(retry(&response, 1_445_412_000_000)?, 1_445_412_480_000);
        response.retry_after = Some("99999999999999999999999999".into());
        assert!(retry(&response, 0).is_err());
        Ok(())
    }
}
