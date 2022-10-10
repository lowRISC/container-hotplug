use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{bail, ensure, Error, Result};

#[derive(Clone)]
pub enum SymlinkDevice {
    Usb(String, String, String),
}

#[derive(Clone)]
pub struct Symlink(SymlinkDevice, PathBuf);

fn is_hex4(val: &str) -> bool {
    val.len() == 4 && val.chars().all(|c| c.is_ascii_hexdigit())
}

fn is_number(val: &str) -> bool {
    val.len() >= 1 && val.chars().all(|c| c.is_ascii_digit())
}

impl FromStr for Symlink {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split("=").collect();

        ensure!(
            parts.len() == 2,
            "Symlink format should be `<PREFIX>:<DEVICE>=<PATH>`, found `{s}`"
        );

        let dev = parts[0];
        let path = parts[1];

        ensure!(
            path.starts_with("/") && !path.ends_with("/"),
            "Symlink PATH should be an absolute file path, found `{path}`."
        );

        let path = PathBuf::from(path);

        let mut parts = dev.split(":");
        let prefix = parts.next().unwrap();
        let parts: Vec<_> = parts.collect();
        let dev = parts.join(":");

        match prefix {
            "usb" => {
                ensure!(parts.len() == 3, "Symlink DEVICE format for usb should be `<VID>:<PID>:<INTERFACE>`, found `{dev}`.");

                let vid = parts[0];
                let pid = parts[1];
                let ifc = parts[2];

                ensure!(
                    is_hex4(vid),
                    "USB symlink VID should be a 4 digit hex number, found `{vid}`"
                );
                ensure!(
                    is_hex4(pid),
                    "USB symlink PID should be a 4 digit hex number, found `{pid}`"
                );
                ensure!(
                    is_number(ifc),
                    "USB symlink INTERFACE should be a number, found `{ifc}`"
                );

                let vid = vid.to_ascii_lowercase();
                let pid = pid.to_ascii_lowercase();
                let ifc = format!("{ifc:0>2}");

                Ok(Symlink(SymlinkDevice::Usb(vid, pid, ifc), path))
            }
            _ => {
                bail!("Symlink PREFIX should be `usb`, found `{prefix}`");
            }
        }
    }
}

impl SymlinkDevice {
    fn matches_impl(&self, device: &udev::Device) -> Option<bool> {
        let matches = match self {
            SymlinkDevice::Usb(vid, pid, ifc) => {
                device.property_value("ID_VENDOR_ID")?.to_str()? == vid
                    && device.property_value("ID_MODEL_ID")?.to_str()? == pid
                    && device.property_value("ID_USB_INTERFACE_NUM")?.to_str()? == ifc
            }
        };
        Some(matches)
    }

    pub fn matches(&self, device: &udev::Device) -> bool {
        self.matches_impl(device).unwrap_or(false)
    }
}

impl Symlink {
    pub fn matches(&self, device: &udev::Device) -> Option<PathBuf> {
        if self.0.matches(device) {
            Some(self.1.clone())
        } else {
            None
        }
    }
}
