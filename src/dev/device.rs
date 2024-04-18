use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DevNode {
    pub path: PathBuf,
    pub devnum: (u32, u32),
}

#[derive(Debug, Clone)]
pub struct Device {
    device: udev::Device,
    // Cache devnum/devnode for the device as they can become unavailable when removing devices.
    devnode: Option<DevNode>,
}

impl Device {
    pub fn from_udev(device: udev::Device) -> Self {
        let devnode = device
            .devnum()
            .zip(device.devnode())
            .map(|(devnum, devnode)| {
                let major = rustix::fs::major(devnum);
                let minor = rustix::fs::minor(devnum);
                DevNode {
                    path: devnode.to_owned(),
                    devnum: (major, minor),
                }
            });
        Self { device, devnode }
    }

    fn display_name_from_db(&self) -> Option<String> {
        let vid = self.device.property_value("ID_VENDOR_ID")?.to_str()?;
        let pid = self.device.property_value("ID_MODEL_ID")?.to_str()?;
        let vid = u16::from_str_radix(vid, 16).ok()?;
        let pid = u16::from_str_radix(pid, 16).ok()?;
        let device = usb_ids::Device::from_vid_pid(vid, pid)?;
        let vendor = device.vendor().name();
        let product = device.name();
        Some(format!("{vendor} {product}"))
    }

    fn display_name_from_props(&self) -> Option<String> {
        let vid = self.device.property_value("ID_VENDOR_ENC")?.to_str()?;
        let pid = self.device.property_value("ID_MODEL_ENC")?.to_str()?;
        let vid = unescape::unescape(vid)?;
        let pid = unescape::unescape(pid)?;
        Some(format!("{} {}", vid.trim(), pid.trim()))
    }

    pub fn display_name(&self) -> Option<String> {
        self.display_name_from_db()
            .or_else(|| self.display_name_from_props())
    }

    pub fn udev(&self) -> &udev::Device {
        &self.device
    }

    pub fn syspath(&self) -> &Path {
        self.device.syspath()
    }

    pub fn devnode(&self) -> Option<&DevNode> {
        self.devnode.as_ref()
    }
}

impl Display for Device {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if let Some(devnode) = self.devnode() {
            let (major, minor) = devnode.devnum;
            write!(f, "{major:0>3}:{minor:0>3}")?;
        } else {
            write!(f, "  -:-  ")?;
        }
        if let Some(name) = self.display_name() {
            write!(f, " ({name})")?;
        } else {
            write!(f, " (Unknown)")?;
        }
        if let Some(devnode) = self.devnode() {
            write!(f, " [{}]", devnode.path.display())?;
        } else {
            write!(f, " [{}]", self.syspath().display())?;
        }
        Ok(())
    }
}
