//! Root device references, as used by the `org.lowrisc.hotplug.devices` annotation.

use std::fmt::Display;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Error, Result, bail, ensure};
use udev::Enumerator;

use crate::dev::Device;

/// A reference to a device.
///
/// The syntax is `[parent-of:]*<PREFIX>:<DEVICE>`, where the prefix selects one of the
/// [`DeviceKind`] forms and each `parent-of:` walks one step up the sysfs hierarchy from whatever
/// was matched. For example:
///
/// * `usb:2b3e:c310` — the device with that vendor and product ID.
/// * `parent-of:usb:2b3e:c310` — its parent, typically the hub it is plugged into. This is the
///   useful form for a root device: it gives the container everything on the same hub as a known
///   board, without having to identify the hub itself.
/// * `syspath:/sys/bus/usb/devices/usb1` — a device named by its sysfs directory.
/// * `devnode:/dev/ttyACM0` — a device named by one of its device nodes.
///
/// Resolution happens once, at container creation time, in [`DeviceRef::device`]; the reference is
/// not re-evaluated if devices are plugged in later.
#[derive(Clone)]
pub struct DeviceRef {
    /// How many `parent-of:` prefixes were given.
    parent_level: usize,
    kind: DeviceKind,
}

/// How a device is identified, before any `parent-of:` is applied.
#[derive(Clone)]
pub enum DeviceKind {
    /// A USB device matched on its `idVendor`, `idProduct` and `serial` attributes.
    ///
    /// Written `usb:<VID>[:<PID>[:<SERIAL>]]`, with the IDs as 4-digit hex. Omitted components are
    /// not constrained, so `usb:2b3e` matches any device from that vendor. If several devices match,
    /// which one is used is unspecified.
    Usb {
        vid: String,
        pid: Option<String>,
        serial: Option<String>,
    },
    /// A device named by its sysfs directory, e.g. `syspath:/sys/bus/usb/devices/usb1`.
    ///
    /// The path must be absolute and under `/sys`; symlinks are resolved.
    Syspath(PathBuf),
    /// A device named by one of its device nodes, e.g. `devnode:/dev/ttyACM0`.
    ///
    /// The path must be absolute and under `/dev`; symlinks are resolved, so a udev-created alias
    /// works too.
    Devnode(PathBuf),
}

fn is_hex4(val: &str) -> bool {
    val.len() == 4 && val.chars().all(|c| c.is_ascii_hexdigit())
}

impl FromStr for DeviceRef {
    type Err = Error;

    fn from_str(mut s: &str) -> Result<Self> {
        let mut parent_level = 0;
        while let Some(remainder) = s.strip_prefix("parent-of:") {
            s = remainder;
            parent_level += 1;
        }

        let Some((kind, dev)) = s.split_once(':') else {
            bail!("Device format should be `[[parent-of:]*]<PREFIX>:<DEVICE>`, found `{s}`");
        };

        let device = match kind {
            "usb" => {
                let mut parts = dev.split(':');

                let vid = parts.next().unwrap();
                let pid = parts.next();
                let serial = parts.next();

                if parts.next().is_some() {
                    bail!(
                        "Device format for usb should be `<VID>[:<PID>[:<SERIAL>]]`, found `{dev}`."
                    );
                }

                ensure!(
                    is_hex4(vid),
                    "USB device VID should be a 4 digit hex number, found `{vid}`"
                );
                if let Some(pid) = pid {
                    ensure!(
                        is_hex4(pid),
                        "USB device PID should be a 4 digit hex number, found `{}`",
                        pid
                    );
                }
                if let Some(serial) = serial {
                    ensure!(
                        !serial.is_empty() && serial.chars().all(|c| c.is_ascii_alphanumeric()),
                        "USB device SERIAL should be alphanumeric, found `{}`",
                        serial
                    );
                }

                let vid = vid.to_ascii_lowercase();
                let pid = pid.map(|s| s.to_ascii_lowercase());
                let serial = serial.map(|s| s.to_owned());

                DeviceKind::Usb { vid, pid, serial }
            }
            "syspath" => {
                let path = PathBuf::from(&dev);
                ensure!(
                    path.is_absolute() && path.starts_with("/sys"),
                    "Syspath device PATH should be a directory path in /sys/**"
                );

                DeviceKind::Syspath(path)
            }
            "devnode" => {
                let path = PathBuf::from(&dev);
                ensure!(
                    path.is_absolute() && path.starts_with("/dev") && !dev.ends_with('/'),
                    "Devnode device PATH should be a file path in /dev/**"
                );

                DeviceKind::Devnode(path)
            }
            _ => {
                bail!(
                    "Device PREFIX should be one of `usb`, `syspath` or `devnode`, found `{kind}`"
                );
            }
        };

        Ok(DeviceRef {
            parent_level,
            kind: device,
        })
    }
}

impl Display for DeviceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            DeviceKind::Usb { vid, pid, serial } => {
                write!(f, "usb:{vid}")?;
                if let Some(pid) = pid {
                    write!(f, ":{pid}")?;
                }
                if let Some(serial) = serial {
                    write!(f, ":{serial}")?;
                }
                Ok(())
            }
            DeviceKind::Syspath(path) => {
                write!(f, "syspath:{}", path.display())
            }
            DeviceKind::Devnode(path) => {
                write!(f, "devnode:{}", path.display())
            }
        }
    }
}

impl Display for DeviceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for _ in 0..self.parent_level {
            write!(f, "parent-of:")?;
        }
        write!(f, "{}", self.kind)
    }
}

impl DeviceKind {
    /// Resolve to the matching device, ignoring any `parent-of:`.
    ///
    /// Fails if no device currently matches.
    fn device(&self) -> Result<Device> {
        let dev = match &self {
            DeviceKind::Usb { vid, pid, serial } => {
                let mut enumerator = Enumerator::new()?;
                enumerator.match_attribute("idVendor", vid)?;
                if let Some(pid) = pid {
                    enumerator.match_attribute("idProduct", pid)?;
                }
                if let Some(serial) = serial {
                    enumerator.match_attribute("serial", serial)?;
                }
                enumerator
                    .scan_devices()?
                    .next()
                    .with_context(|| format!("Failed to find device `{self}`"))?
            }
            DeviceKind::Syspath(path) => {
                let path = path
                    .canonicalize()
                    .with_context(|| format!("Failed to resolve PATH for `{self}`"))?;
                udev::Device::from_syspath(&path)
                    .with_context(|| format!("Failed to find device `{self}`"))?
            }
            DeviceKind::Devnode(path) => {
                let path = path
                    .canonicalize()
                    .with_context(|| format!("Failed to resolve PATH for `{self}`"))?;
                let mut enumerator = Enumerator::new()?;
                enumerator.match_property("DEVNAME", path)?;
                enumerator
                    .scan_devices()?
                    .next()
                    .with_context(|| format!("Failed to find device `{self}`"))?
            }
        };
        Ok(Device::from_udev(dev))
    }
}

impl DeviceRef {
    /// Resolve the reference against the devices currently present on the host.
    ///
    /// Fails if nothing matches, or if a `parent-of:` walks past the root of the device tree.
    pub fn device(&self) -> Result<Device> {
        let mut device = self.kind.device()?;
        for _ in 0..self.parent_level {
            device = Device::from_udev(device.udev().parent().with_context(|| {
                format!("Failed to obtain parent device while resolving `{self}`")
            })?);
        }
        Ok(device)
    }
}
