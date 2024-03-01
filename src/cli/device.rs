use std::fmt::Display;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{bail, ensure, Context, Error, Result};
use udev::Enumerator;

#[derive(Clone)]
pub struct Device(usize, DeviceType);

#[derive(Clone)]
pub enum DeviceType {
    Usb(String, Option<String>, Option<String>),
    Syspath(PathBuf),
    Devnode(PathBuf),
}

fn is_hex4(val: &str) -> bool {
    val.len() == 4 && val.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_alphanum(val: &str) -> bool {
    val.len() >= 1 && val.chars().all(|c| c.is_ascii_alphanumeric())
}

impl FromStr for Device {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split(":").collect();

        let parent_steps = parts
            .iter()
            .copied()
            .take_while(|part| *part == "parent-of")
            .count();

        let parts = &parts[parent_steps..];

        ensure!(
            parts.len() >= 2,
            "Device format should be `[[parent-of:]*]<PREFIX>:<DEVICE>`, found `{s}`"
        );

        let prefix = parts[0];
        let parts = &parts[1..];
        let dev = parts.join(":");

        let device = match prefix {
            "usb" => {
                ensure!(
                    parts.len() >= 1 && parts.len() <= 3,
                    "Device format for usb should be `<VID>[:<PID>[:<SERIAL>]]`, found `{dev}`."
                );

                let mut parts = parts.iter().copied();

                let vid = parts.next().unwrap();
                let pid = parts.next();
                let serial = parts.next();

                ensure!(
                    is_hex4(vid),
                    "USB device VID should be a 4 digit hex number, found `{vid}`"
                );
                ensure!(
                    pid.is_none() || is_hex4(pid.unwrap()),
                    "USB device PID should be a 4 digit hex number, found `{}`",
                    pid.unwrap()
                );
                ensure!(
                    serial.is_none() || is_alphanum(serial.unwrap()),
                    "USB device SERIAL should be alphanumeric, found `{}`",
                    serial.unwrap()
                );

                let vid = vid.to_ascii_lowercase();
                let pid = pid.map(|s| s.to_ascii_lowercase());
                let serial = serial.map(|s| s.to_owned());

                DeviceType::Usb(vid, pid, serial)
            }
            "syspath" => {
                let path = PathBuf::from(&dev);
                ensure!(
                    path.is_absolute() && path.starts_with("/sys"),
                    "Syspath device PATH should be a directory path in /sys/**"
                );

                DeviceType::Syspath(path)
            }
            "devnode" => {
                let path = PathBuf::from(&dev);
                ensure!(
                    path.is_absolute() && path.starts_with("/sys") && !dev.ends_with("/"),
                    "Devnode device PATH should be a file path in /dev/**"
                );

                DeviceType::Devnode(path)
            }
            _ => {
                bail!("Device PREFIX should be one of `usb`, `syspath` or `devnode`, found `{prefix}`");
            }
        };

        Ok(Device(parent_steps, device))
    }
}

impl Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            DeviceType::Usb(vid, Some(pid), Some(serial)) => {
                write!(f, "usb:{vid}:{pid}:{serial}")
            }
            DeviceType::Usb(vid, Some(pid), None) => {
                write!(f, "usb:{vid}:{pid}")
            }
            DeviceType::Usb(vid, None, _) => {
                write!(f, "usb:{vid}")
            }
            DeviceType::Syspath(path) => {
                write!(f, "syspath:{}", path.display())
            }
            DeviceType::Devnode(path) => {
                write!(f, "devnode:{}", path.display())
            }
        }
    }
}

impl Display for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for _ in 0..self.0 {
            write!(f, "parent-of:")?;
        }
        write!(f, "{}", self.1)
    }
}

impl DeviceType {
    fn device(&self) -> Result<udev::Device> {
        let dev = match &self {
            DeviceType::Usb(vid, pid, serial) => {
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
            DeviceType::Syspath(path) => {
                let path = path
                    .canonicalize()
                    .with_context(|| format!("Failed to resolve PATH for `{self}`"))?;
                udev::Device::from_syspath(&path)
                    .with_context(|| format!("Failed to find device `{self}`"))?
            }
            DeviceType::Devnode(path) => {
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

impl Device {
    pub fn device(&self) -> Result<udev::Device> {
        let device = self.1.device()?;
        Ok(device)
    }

    pub fn hub(&self) -> Result<udev::Device> {
        let mut device = self.device()?;
        for _ in 0..self.0 {
            device = device.parent().with_context(|| {
                format!("Failed to obtain parent device while resolving `{self}`")
            })?;
        }
        Ok(device)
    }
}
