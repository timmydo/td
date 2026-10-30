//! Bounded SNTP samples. Peer and nonce checks are not authentication.
use crate::request::{MAX_EPOCH, MIN_EPOCH};
use std::fs::File;
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

const OFFSET: u64 = 2_208_988_800;
const ERA: u64 = 1u64 << 32;
const MAX_RTT: Duration = Duration::from_secs(2);
const SERVERS: &[&str] = &["time.cloudflare.com", "time.nist.gov"];

fn word(packet: &[u8], offset: usize) -> Result<u32, String> {
    let bytes: [u8; 4] = packet
        .get(offset..offset + 4)
        .ok_or("short NTP word")?
        .try_into()
        .map_err(|_| "short NTP word")?;
    Ok(u32::from_be_bytes(bytes))
}

fn stamp(packet: &[u8], offset: usize) -> Result<u64, String> {
    let seconds = u64::from(word(packet, offset)?);
    let fraction = u64::from(word(packet, offset + 4)?);
    for era in [0, ERA] {
        let Some(unix) = seconds
            .checked_add(era)
            .and_then(|value| value.checked_sub(OFFSET))
        else {
            continue;
        };
        if (MIN_EPOCH..=MAX_EPOCH).contains(&unix) {
            return Ok(unix * 1000 + ((fraction * 1000) >> 32));
        }
    }
    Err("NTP time outside 2020 through 2099".into())
}

/// Estimated UTC milliseconds when the reply arrived. The caller uses a
/// connected UDP socket; this parser additionally binds the echoed nonce.
pub fn response(packet: &[u8], nonce: &[u8; 8], elapsed: Duration) -> Result<u64, String> {
    if packet.len() != 48 || elapsed > MAX_RTT {
        return Err("NTP reply length or round-trip delay refused".into());
    }
    let flags = *packet.first().ok_or("missing NTP header")?;
    let stratum = *packet.get(1).ok_or("missing NTP stratum")?;
    if flags >> 6 == 3
        || ![3, 4].contains(&((flags >> 3) & 7))
        || flags & 7 != 4
        || !(1..=15).contains(&stratum)
    {
        return Err("NTP server is unavailable or unsynchronized".into());
    }
    if packet.get(24..32) != Some(nonce.as_slice()) {
        return Err("NTP origin does not match the request".into());
    }
    // Reject negative/excessive signed root delay and excessive dispersion.
    if word(packet, 4)? > 10 << 16 || word(packet, 8)? > 10 << 16 {
        return Err("NTP root distance refused".into());
    }
    if [32, 40]
        .iter()
        .any(|offset| packet.get(*offset..*offset + 8) == Some(&[0; 8]))
    {
        return Err("NTP timestamp is unspecified".into());
    }
    let received = stamp(packet, 32)?;
    let transmitted = stamp(packet, 40)?;
    let processing = transmitted
        .checked_sub(received)
        .ok_or("NTP timestamps reversed")?;
    let rtt = u64::try_from(elapsed.as_millis()).map_err(|_| "NTP delay overflow")?;
    if processing > rtt.saturating_add(1) {
        return Err("NTP processing delay exceeds round trip".into());
    }
    Ok(transmitted + rtt.saturating_sub(processing) / 2)
}

pub struct Sample {
    pub utc_millis: u64,
    pub received: Instant,
    pub peer: IpAddr,
}

pub fn agreement(first: &Sample, second: &Sample, now: Instant) -> Result<u64, String> {
    if first.peer == second.peer {
        return Err("NTP samples came from the same address".into());
    }
    let project = |sample: &Sample| -> Result<u64, String> {
        let elapsed = now
            .checked_duration_since(sample.received)
            .ok_or("sample is in the future")?;
        if elapsed > Duration::from_secs(15) {
            return Err("NTP sample is stale".into());
        }
        sample
            .utc_millis
            .checked_add(u64::try_from(elapsed.as_millis()).map_err(|_| "sample age overflow")?)
            .ok_or_else(|| "sample projection overflow".into())
    };
    let left = project(first)?;
    let right = project(second)?;
    if left.abs_diff(right) > 2000 {
        return Err("NTP sources disagree by more than two seconds".into());
    }
    Ok(left / 2 + right / 2 + (left % 2 + right % 2) / 2)
}

fn sample(server: &str) -> Result<Sample, String> {
    // The caller runs this DNS/network-only operation in a deadline-owned
    // child; std's synchronous resolver itself offers no timeout API.
    let addresses = (server, 123)
        .to_socket_addrs()
        .map_err(|error| format!("resolve {server}: {error}"))?;
    let mut addresses: Vec<_> = addresses.take(16).collect();
    let selected: Vec<_> = match (
        addresses.iter().find(|address| address.is_ipv4()),
        addresses.iter().find(|address| address.is_ipv6()),
    ) {
        (Some(v4), Some(v6)) => vec![*v4, *v6],
        _ => addresses.drain(..).take(2).collect(),
    };
    let mut last = "no NTP address".to_owned();
    for address in selected {
        let attempt = || -> Result<Sample, String> {
            let socket = UdpSocket::bind(if address.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            })
            .map_err(|error| error.to_string())?;
            socket.connect(address).map_err(|error| error.to_string())?;
            socket
                .set_read_timeout(Some(MAX_RTT))
                .map_err(|error| error.to_string())?;
            socket
                .set_write_timeout(Some(MAX_RTT))
                .map_err(|error| error.to_string())?;
            let mut nonce = [0; 8];
            File::open("/dev/urandom")
                .and_then(|mut file| file.read_exact(&mut nonce))
                .map_err(|error| error.to_string())?;
            let mut request = [0u8; 48];
            if let Some(flags) = request.first_mut() {
                *flags = (4 << 3) | 3;
            }
            request
                .get_mut(40..48)
                .ok_or("NTP request layout")?
                .copy_from_slice(&nonce);
            let started = Instant::now();
            if socket.send(&request).map_err(|error| error.to_string())? != request.len() {
                return Err("short NTP send".into());
            }
            let mut packet = [0; 49];
            let size = socket
                .recv(&mut packet)
                .map_err(|error| error.to_string())?;
            let received = Instant::now();
            let utc_millis = response(
                packet.get(..size).ok_or("NTP receive size")?,
                &nonce,
                received.duration_since(started),
            )?;
            Ok(Sample {
                utc_millis,
                received,
                peer: address.ip(),
            })
        };
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) => last = error,
        }
    }
    Err(format!("{server}: {last}"))
}

pub fn query() -> Result<u64, String> {
    let first = sample(SERVERS.first().ok_or("missing first time source")?)?;
    let second = sample(SERVERS.get(1).ok_or("missing second time source")?)?;
    agreement(&first, &second, Instant::now())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn packet(epoch: u64) -> [u8; 48] {
        let mut bytes = [0; 48];
        bytes[0] = (4 << 3) | 4;
        bytes[1] = 2;
        bytes[24..32].copy_from_slice(&[7; 8]);
        for index in [32, 40] {
            bytes[index..index + 4].copy_from_slice(&((epoch + OFFSET) as u32).to_be_bytes());
        }
        bytes
    }

    #[test]
    fn valid_samples_cross_ntp_era_without_using_the_wrong_local_clock() {
        for epoch in [MIN_EPOCH, 2_085_978_495, 2_085_978_497, MAX_EPOCH] {
            assert_eq!(
                response(&packet(epoch), &[7; 8], Duration::ZERO).unwrap(),
                epoch * 1000
            );
        }
    }

    #[test]
    fn invalid_packets_never_supply_time() {
        let valid = packet(1_793_340_000);
        for length in 0..48 {
            assert!(response(&valid[..length], &[7; 8], Duration::ZERO).is_err());
        }
        assert!(response(&[0; 49], &[7; 8], Duration::ZERO).is_err());
        assert!(response(&valid, &[8; 8], Duration::ZERO).is_err());
        assert!(response(&valid, &[7; 8], Duration::from_secs(3)).is_err());
        for (index, value) in [(0, 0xe4), (0, 0x23), (1, 0), (1, 16), (4, 0xff), (8, 0xff)] {
            let mut bytes = valid;
            bytes[index] = value;
            assert!(response(&bytes, &[7; 8], Duration::ZERO).is_err());
        }
        for offset in [32, 40] {
            let mut bytes = valid;
            bytes[offset..offset + 8].fill(0);
            assert!(response(&bytes, &[7; 8], Duration::ZERO).is_err());
        }
        let mut reversed = valid;
        reversed[32..36].copy_from_slice(&((1_793_340_001 + OFFSET) as u32).to_be_bytes());
        assert!(response(&reversed, &[7; 8], Duration::ZERO).is_err());
    }

    #[test]
    fn consensus_requires_fresh_different_sources_and_projects_elapsed_time() {
        let now = Instant::now();
        let first = Sample {
            utc_millis: 1_793_340_000_000,
            received: now,
            peer: "192.0.2.1".parse().unwrap(),
        };
        let mut second = Sample {
            utc_millis: first.utc_millis + 100,
            received: now,
            peer: "192.0.2.2".parse().unwrap(),
        };
        assert_eq!(
            agreement(&first, &second, now + Duration::from_secs(1)).unwrap(),
            first.utc_millis + 1050
        );
        second.utc_millis += 3000;
        assert!(agreement(&first, &second, now).is_err());
        second.utc_millis = first.utc_millis;
        second.peer = first.peer;
        assert!(agreement(&first, &second, now).is_err());
        second.peer = "192.0.2.2".parse().unwrap();
        assert!(agreement(&first, &second, now + Duration::from_secs(16)).is_err());
    }
}
