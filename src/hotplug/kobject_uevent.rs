use std::io::{IoSlice, Write};
use std::os::fd::OwnedFd;

use anyhow::Result;
use rustix::net::{AddressFamily, SendFlags, SocketType, netlink::SocketAddrNetlink};
use zerocopy::{Immutable, IntoBytes};

use crate::util::namespace::NetNamespace;

// This needs to be compatible with
// https://github.com/systemd/systemd/blob/main/src/libsystemd/sd-device/device-monitor.c.
#[repr(C)]
#[derive(Immutable, IntoBytes)]
struct MonitorNetlinkHeader {
    /// "libudev" prefix to distinguish libudev and kernel messages.
    prefix: [u8; 8],
    /// Magic to protect against daemon <-> Library message format mismatch
    /// Used in the kernel from socket filter rules; needs to be stored in network order.
    magic: u32,
    /// Total length of header structure known to the sender.
    header_size: u32,
    /// Properties string buffer
    properties_off: u32,
    properties_len: u32,
    /// Hashes of primary device properties strings, to let libudev subscribers
    /// use in-kernel socket filters; values need to be stored in network order.
    filter_subsystem_hash: u32,
    filter_devtype_hash: u32,
    filter_tag_bloom_hi: u32,
    filter_tag_bloom_lo: u32,
}

/// Udev netlink event sender.
///
/// When a device is added/removed, after processing rules, `systemd-udevd` will send a netlink
/// message to `kobject_uevent` netlink socket. This is picked up by libudev monitor users.
///
/// This netlink socket is namespaced, so udevd-sent messages are not observed by the container.
/// This sender takes the place of udevd and ensures that libudev users inside the container may
/// see the device add/removal event after being processed by container-hotplug.
pub struct UdevSender {
    socket: OwnedFd,
    seq_num: u64,
    ns: NetNamespace,
}

impl UdevSender {
    pub fn new(ns: NetNamespace) -> Result<Self> {
        let socket = ns.with(|| {
            rustix::net::socket(
                AddressFamily::NETLINK,
                SocketType::DGRAM,
                Some(rustix::net::netlink::KOBJECT_UEVENT),
            )
        })??;

        Ok(Self {
            socket,
            seq_num: 0,
            ns,
        })
    }

    pub fn send(&mut self, device: &udev::Device, event: &str) -> Result<()> {
        self.seq_num += 1;

        let mut properties = Vec::new();
        write!(properties, "ACTION={event}\0SEQNUM={}\0", self.seq_num)?;
        for property in device.properties() {
            // These properties are specially handled.
            if property.name() == "ACTION" || property.name() == "SEQNUM" {
                continue;
            }
            properties.extend_from_slice(property.name().as_encoded_bytes());
            properties.push(b'=');
            properties.extend_from_slice(property.value().as_encoded_bytes());
            properties.push(0);
        }
        let header = MonitorNetlinkHeader {
            prefix: *b"libudev\0",
            magic: 0xFEEDCAFEu32.to_be(),
            header_size: std::mem::size_of::<MonitorNetlinkHeader>() as u32,
            properties_off: std::mem::size_of::<MonitorNetlinkHeader>() as u32,
            properties_len: properties.len() as u32,
            filter_subsystem_hash: device
                .subsystem()
                .map(|x| murmur2::murmur2ne(x.as_encoded_bytes(), 0).to_be())
                .unwrap_or_default(),
            filter_devtype_hash: device
                .devtype()
                .map(|x| murmur2::murmur2ne(x.as_encoded_bytes(), 0).to_be())
                .unwrap_or_default(),
            // Don't bother computing the value in the same way as systemd,
            // just be conservative and always make it match -- this is an optimisation anyway.
            filter_tag_bloom_hi: 0xFFFFFFFF,
            filter_tag_bloom_lo: 0xFFFFFFFF,
        };

        // We re-enter the namespace to obtain root UID/GID so it'll be trusted by libudev.
        // Otherwise, when userns is used, we're the global root which is mapped to nobody in the
        // container. libudev will use SCM credentials to check for the sender and identify if the
        // message is to be trusted.
        //
        // Technically just changing UID/GID is sufficient and network namespace re-entering isn't
        // necessary -- but there's no harm in doing so and it makes code simpler.
        self.ns.with(|| {
            rustix::net::sendmsg_addr(
                &self.socket,
                &SocketAddrNetlink::new(0, 2),
                &[IoSlice::new(header.as_bytes()), IoSlice::new(&properties)],
                &mut Default::default(),
                SendFlags::empty(),
            )
        })??;
        Ok(())
    }
}
