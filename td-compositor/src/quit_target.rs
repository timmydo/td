//! The process that owned a public Wayland connection when it was admitted.
use crate::{allocation, session, sys};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::{fs::MetadataExt, net::UnixStream};

pub(crate) struct Target {
    pid: u32,
    directory: File,
    application: Option<String>,
}

fn read_record(path: &str, buffer: &mut [u8]) -> Option<usize> {
    let mut file = File::open(path).ok()?;
    let mut n = 0;
    loop {
        let tail = buffer.get_mut(n..).filter(|v| !v.is_empty())?;
        let count = file.read(tail).ok()?;
        if count == 0 {
            return Some(n);
        }
        n += count;
    }
}

fn live_pid(fdinfo: &str) -> Option<u32> {
    let mut bytes = [0; 1024];
    let n = read_record(fdinfo, &mut bytes)?;
    std::str::from_utf8(bytes.get(..n)?)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Pid:"))?
        .trim()
        .parse()
        .ok()
}

impl Target {
    pub(crate) fn capture(stream: &UnixStream) -> Option<Self> {
        let uid = sys::peer_uid(stream).ok()?;
        let application = if uid == session::HUMAN_UID {
            None
        } else {
            Some(
                session::application_policy()
                    .ok()?
                    .for_uid(uid)?
                    .application,
            )
        };
        Self::from_peer(stream, application)
    }

    fn from_peer(stream: &UnixStream, application: Option<String>) -> Option<Self> {
        let peer = sys::peer_pidfd(stream).ok()?;
        let fdinfo =
            allocation::format(format_args!("/proc/self/fdinfo/{}", peer.as_raw_fd())).ok()?;
        let pid = live_pid(&fdinfo).filter(|p| *p > 1)?;
        let path = allocation::format(format_args!("/proc/{pid}")).ok()?;
        let directory = File::open(path).ok()?;
        (live_pid(&fdinfo) == Some(pid)).then_some(Self {
            pid,
            directory,
            application,
        })
    }

    pub(crate) fn duplicate(&self) -> Option<Self> {
        Some(Self {
            directory: self.directory.try_clone().ok()?,
            pid: self.pid,
            application: self
                .application
                .as_deref()
                .map(allocation::string)
                .transpose()
                .ok()?,
        })
    }

    pub(crate) fn request(&self) -> Result<Vec<u8>, String> {
        let mut bytes = allocation::copy(&[0x33]).map_err(|_| String::new())?;
        allocation::extend_from_slice(&mut bytes, &self.pid.to_be_bytes())
            .map_err(|_| String::new())?;
        let inode = self
            .directory
            .metadata()
            .map_err(|_| "force quit target unavailable")?
            .ino();
        allocation::extend_from_slice(&mut bytes, &inode.to_be_bytes())
            .map_err(|_| String::new())?;
        if let Some(name) = &self.application {
            allocation::extend_from_slice(&mut bytes, name.as_bytes())
                .map_err(|_| String::new())?;
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_socket_peer_reaches_the_wire_with_its_original_identity() {
        let (stream, _other) = UnixStream::pair().unwrap();
        let target = Target::from_peer(&stream, Some("demo".into())).unwrap();
        let bytes = target.duplicate().unwrap().request().unwrap();
        let inode = File::open("/proc/self").unwrap().metadata().unwrap().ino();
        assert_eq!(
            bytes,
            [
                &[0x33][..],
                &std::process::id().to_be_bytes(),
                &inode.to_be_bytes(),
                b"demo"
            ]
            .concat()
        );
    }
}
