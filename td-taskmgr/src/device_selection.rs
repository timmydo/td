//! Explicit retained device membership; sums never silently include new layers.
use crate::budget::{Budget, Charge, MemoryString, MemoryVec};
use crate::devices::{self, Disk, Network, Object, Sample};
use std::sync::Arc;
#[derive(Debug)]
struct NetworkKey {
    index: Option<u32>,
    namespace: Option<Object>,
    name: MemoryString,
}
impl NetworkKey {
    fn matches(&self, sample: &Sample, network: &Network) -> bool {
        if self.namespace != sample.network_namespace {
            return false;
        }
        match self.index.zip(network.ifindex) {
            Some((a, b)) => a == b,
            None => self.index == network.ifindex && self.name.as_str() == network.name.as_str(),
        }
    }
}
#[derive(Debug)]
struct DiskKey {
    major: u32,
    minor: u32,
    name: MemoryString,
}
impl DiskKey {
    fn matches(&self, disk: &Disk) -> bool {
        self.major == disk.major
            && self.minor == disk.minor
            && self.name.as_str() == disk.name.as_str()
    }
}
#[derive(Debug)]
pub struct Selection {
    budget: Arc<Budget>,
    networks: MemoryVec<NetworkKey>,
    disks: MemoryVec<DiskKey>,
    network_chosen: bool,
    disk_chosen: bool,
    _charge: Charge,
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
impl Selection {
    pub fn new(budget: &Arc<Budget>) -> Result<Self, String> {
        Ok(Self {
            budget: Arc::clone(budget),
            networks: MemoryVec::new(budget, 256).map_err(error)?,
            disks: MemoryVec::new(budget, 1024).map_err(error)?,
            network_chosen: false,
            disk_chosen: false,
            _charge: budget.charge(std::mem::size_of::<Self>()).map_err(error)?,
        })
    }
    pub fn initialize(&mut self, sample: &Sample) -> Result<(), String> {
        if !self.network_chosen {
            if let Some(index) = devices::default_network(&sample.networks) {
                self.toggle_network(sample, index)?;
            }
        }
        if !self.disk_chosen {
            if let Some(index) = devices::default_disk(&sample.disks) {
                self.toggle_disk(sample, index)?;
            }
        }
        for key in self.networks.iter_mut() {
            if let Some(network) = sample
                .networks
                .iter()
                .find(|network| key.matches(sample, network))
            {
                if key.name.as_str() != network.name.as_str() {
                    key.name =
                        MemoryString::new(&self.budget, network.name.as_str()).map_err(error)?;
                }
            }
        }
        Ok(())
    }
    pub fn network_count(&self) -> usize {
        self.networks.len()
    }
    pub fn disk_count(&self) -> usize {
        self.disks.len()
    }
    pub fn network_selected(&self, sample: &Sample, index: usize) -> bool {
        sample
            .networks
            .get(index)
            .is_some_and(|n| self.networks.iter().any(|key| key.matches(sample, n)))
    }
    pub fn disk_selected(&self, sample: &Sample, index: usize) -> bool {
        sample
            .disks
            .get(index)
            .is_some_and(|d| self.disks.iter().any(|key| key.matches(d)))
    }
    pub fn network_names(&self) -> impl Iterator<Item = &str> {
        self.networks.iter().map(|n| n.name.as_str())
    }
    pub fn disk_names(&self) -> impl Iterator<Item = &str> {
        self.disks.iter().map(|d| d.name.as_str())
    }
    pub fn toggle_network(&mut self, sample: &Sample, index: usize) -> Result<(), String> {
        let network = sample
            .networks
            .get(index)
            .ok_or("interface no longer observed")?;
        if let Some(index) = self
            .networks
            .iter()
            .position(|key| key.matches(sample, network))
        {
            self.networks.remove(index);
        } else {
            let name = MemoryString::new(&self.budget, network.name.as_str()).map_err(error)?;
            self.networks
                .push(NetworkKey {
                    index: network.ifindex,
                    namespace: sample.network_namespace,
                    name,
                })
                .map_err(|_| "selected interface limit")?;
        }
        self.network_chosen = true;
        Ok(())
    }
    pub fn toggle_disk(&mut self, sample: &Sample, index: usize) -> Result<(), String> {
        let disk = sample.disks.get(index).ok_or("disk no longer observed")?;
        if let Some(index) = self.disks.iter().position(|key| key.matches(disk)) {
            self.disks.remove(index);
        } else {
            let name = MemoryString::new(&self.budget, disk.name.as_str()).map_err(error)?;
            self.disks
                .push(DiskKey {
                    major: disk.major,
                    minor: disk.minor,
                    name,
                })
                .map_err(|_| "selected disk limit")?;
        }
        self.disk_chosen = true;
        Ok(())
    }
    pub fn network_rates(&self, sample: &Sample) -> (Option<u64>, Option<u64>) {
        if self.networks.is_empty() {
            return (None, None);
        }
        let mut received = Some(0u64);
        let mut sent = Some(0u64);
        for key in self.networks.iter() {
            let network = sample.networks.iter().find(|n| key.matches(sample, n));
            received = received
                .zip(network.and_then(|n| n.receive_rate))
                .and_then(|(a, b)| a.checked_add(b));
            sent = sent
                .zip(network.and_then(|n| n.send_rate))
                .and_then(|(a, b)| a.checked_add(b));
        }
        (received, sent)
    }
    pub fn disk_rates(&self, sample: &Sample) -> (Option<u64>, Option<u64>) {
        if self.disks.is_empty() {
            return (None, None);
        }
        let mut read = Some(0u64);
        let mut written = Some(0u64);
        for key in self.disks.iter() {
            let disk = sample.disks.iter().find(|d| key.matches(d));
            read = read
                .zip(disk.and_then(|d| d.read_rate))
                .and_then(|(a, b)| a.checked_add(b));
            written = written
                .zip(disk.and_then(|d| d.write_rate))
                .and_then(|(a, b)| a.checked_add(b));
        }
        (read, written)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    fn sample(budget: &Arc<Budget>) -> Sample {
        let mut networks = MemoryVec::new(budget, 2).unwrap();
        for (name, index, rx, tx) in [("eth0", 1, 10, 20), ("br0", 2, 30, 40)] {
            networks
                .push(Network {
                    name: MemoryString::new(budget, name).unwrap(),
                    ifindex: Some(index),
                    up: true,
                    device_link: index == 1,
                    received: 100,
                    sent: 200,
                    receive_rate: Some(rx),
                    send_rate: Some(tx),
                    loopback: false,
                })
                .unwrap();
        }
        Sample {
            networks,
            disks: MemoryVec::new(budget, 0).unwrap(),
            omitted_networks: 0,
            omitted_disks: 0,
            network_unavailable: false,
            disk_unavailable: false,
            network_namespace: Some(Object {
                device: 1,
                inode: 2,
            }),
        }
    }
    #[test]
    fn explicit_membership_survives_empty_selection_and_rename_with_unknowns_as_gaps() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut selection = Selection::new(&budget).unwrap();
        let mut sample = sample(&budget);
        selection.initialize(&sample).unwrap();
        assert_eq!(selection.network_rates(&sample), (Some(10), Some(20)));
        selection.toggle_network(&sample, 1).unwrap();
        assert_eq!(selection.network_rates(&sample), (Some(40), Some(60)));
        sample.networks.get_mut(0).unwrap().name = MemoryString::new(&budget, "renamed").unwrap();
        assert!(selection.network_selected(&sample, 0));
        sample.networks.get_mut(1).unwrap().receive_rate = Some(u64::MAX);
        assert_eq!(selection.network_rates(&sample), (None, Some(60)));
        sample.network_namespace = None;
        assert_eq!(selection.network_rates(&sample), (None, None));
        sample.network_namespace = Some(Object {
            device: 1,
            inode: 2,
        });
        selection.toggle_network(&sample, 0).unwrap();
        selection.toggle_network(&sample, 1).unwrap();
        selection.initialize(&sample).unwrap();
        assert_eq!(selection.network_count(), 0);
        assert_eq!(selection.network_rates(&sample), (None, None));
    }
}
