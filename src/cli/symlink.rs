//! Symlink rules, as used by the `org.lowrisc.hotplug.symlinks` annotation.

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Error, Result, bail, ensure};

use crate::dev::Device;

/// Which devices a [`Symlink`] rule applies to.
#[derive(Clone)]
pub enum SymlinkDevice {
    /// A specific interface of a USB device, written `usb:<VID>:<PID>:<INTERFACE>`.
    ///
    /// The IDs are 4-digit hex and the interface is a decimal number; all three are required, since
    /// a symlink names a single device node. Matching a single interface rather than the whole
    /// device is what makes this useful for composite devices: a board exposing two CDC-ACM
    /// interfaces gets a stable name for each, regardless of the order the kernel enumerated them
    /// in.
    Usb {
        vid: String,
        pid: String,
        /// Interface number, zero-padded to two digits to match udev's
        /// `ID_USB_INTERFACE_NUM` property.
        if_num: String,
    },
}

/// A rule requesting a symlink to a device's node inside the container.
///
/// The syntax is `<PREFIX>:<DEVICE>=<PATH>`, e.g. `usb:2b3e:c310:1=/dev/ttyACM_CW310_0`, where
/// `PATH` is an absolute path inside the container.
///
/// This is the container equivalent of a `SYMLINK` directive in a udev rule: the container gets a
/// predictable path for a device whose kernel-assigned node name (`/dev/ttyACM3`, say) depends on
/// what else is plugged in. The symlink is created when the device is attached and removed when it
/// is unplugged; see [`crate::hotplug::HotPlug`].
#[derive(Clone)]
pub struct Symlink {
    device: SymlinkDevice,
    /// Where the symlink is created, inside the container's mount namespace.
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
                ensure!(
                    parts.len() == 3,
                    "Symlink DEVICE format for usb should be `<VID>:<PID>:<INTERFACE>`, found `{dev}`."
                );

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
    /// Compare against the device's udev properties, returning [`None`] if any property needed for
    /// the comparison is missing or not UTF-8 (i.e. the device cannot match).
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

    /// Whether this rule applies to `device`.
    pub fn matches(&self, device: &Device) -> bool {
        self.matches_impl(device.udev()).unwrap_or(false)
    }
}

impl Symlink {
    /// The path to symlink, if this rule applies to `device`.
    pub fn matches(&self, device: &Device) -> Option<PathBuf> {
        if self.device.matches(device) {
            Some(self.path.clone())
        } else {
            None
        }
    }
}
