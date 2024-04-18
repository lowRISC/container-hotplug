use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{bail, ensure, Error, Result};

use crate::dev::Device;

#[derive(Clone)]
pub enum SymlinkDevice {
    Usb {
        vid: String,
        pid: String,
        if_num: String,
    },
}

#[derive(Clone)]
pub struct Symlink {
    device: SymlinkDevice,
    path: PathBuf,
}

fn is_hex4(val: &str) -> bool {
    val.len() == 4 && val.chars().all(|c| c.is_ascii_hexdigit())
}

impl FromStr for Symlink {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split('=').collect();
        ensure!(
            parts.len() == 2,
            "Symlink format should be `<PREFIX>:<DEVICE>=<PATH>`, found `{s}`"
        );

        let dev = parts[0];
        let path = parts[1];

        ensure!(
            path.starts_with('/') && !path.ends_with('/'),
            "Symlink PATH should be an absolute file path, found `{path}`."
        );

        let path = PathBuf::from(path);

        let Some((kind, dev)) = dev.split_once(':') else {
            bail!("Symlink DEVICE format should be `<PREFIX>:<DEVICE>`, found `{dev}`");
        };

        match kind {
            "usb" => {
                let parts: Vec<_> = dev.split(':').collect();
                ensure!(parts.len() == 3, "Symlink DEVICE format for usb should be `<VID>:<PID>:<INTERFACE>`, found `{dev}`.");

                let vid = parts[0];
                let pid = parts[1];
                let if_num = parts[2];

                ensure!(
                    is_hex4(vid),
                    "USB symlink VID should be a 4 digit hex number, found `{vid}`"
                );
                ensure!(
                    is_hex4(pid),
                    "USB symlink PID should be a 4 digit hex number, found `{pid}`"
                );
                ensure!(
                    !if_num.is_empty() && if_num.chars().all(|c| c.is_ascii_digit()),
                    "USB symlink INTERFACE should be a number, found `{if_num}`"
                );

                let vid = vid.to_ascii_lowercase();
                let pid = pid.to_ascii_lowercase();
                let if_num = format!("{if_num:0>2}");

                Ok(Symlink {
                    device: SymlinkDevice::Usb { vid, pid, if_num },
                    path,
                })
            }
            _ => {
                bail!("Symlink PREFIX should be `usb`, found `{kind}`");
            }
        }
    }
}

impl SymlinkDevice {
    fn matches_impl(&self, device: &udev::Device) -> Option<bool> {
        let matches = match self {
            SymlinkDevice::Usb { vid, pid, if_num } => {
                device.property_value("ID_VENDOR_ID")?.to_str()? == vid
                    && device.property_value("ID_MODEL_ID")?.to_str()? == pid
                    && device.property_value("ID_USB_INTERFACE_NUM")?.to_str()? == if_num
            }
        };
        Some(matches)
    }

    pub fn matches(&self, device: &Device) -> bool {
        self.matches_impl(device.udev()).unwrap_or(false)
    }
}

impl Symlink {
    pub fn matches(&self, device: &Device) -> Option<PathBuf> {
        if self.device.matches(device) {
            Some(self.path.clone())
        } else {
            None
        }
    }
}
