use std::fmt::Display;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{bail, ensure, Context, Error, Result};
use udev::Enumerator;

/// A reference to a device.
#[derive(Clone)]
pub struct DeviceRef {
    parent_level: usize,
    kind: DeviceKind,
}

#[derive(Clone)]
pub enum DeviceKind {
    Usb {
        vid: String,
        pid: Option<String>,
        serial: Option<String>,
    },
    Syspath(PathBuf),
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
    fn device(&self) -> Result<udev::Device> {
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
        Ok(dev)
    }
}

impl DeviceRef {
    pub fn device(&self) -> Result<udev::Device> {
        let device = self.kind.device()?;
        Ok(device)
    }

    pub fn hub(&self) -> Result<udev::Device> {
        let mut device = self.device()?;
        for _ in 0..self.parent_level {
            device = device.parent().with_context(|| {
                format!("Failed to obtain parent device while resolving `{self}`")
            })?;
        }
        Ok(device)
    }
}
