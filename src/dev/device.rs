use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Device {
    pub(super) device: udev::Device,
    pub(super) devnum: (u32, u32),
    pub(super) devnode: PathBuf,
}

impl Device {
    pub fn from_udev(device: &udev::Device) -> Option<Self> {
        let device = device.clone();
        let devnum = device.devnum()?;
        let major = rustix::fs::major(devnum);
        let minor = rustix::fs::minor(devnum);
        let devnode = device.devnode()?.to_owned();
        Some(Self {
            device,
            devnum: (major, minor),
            devnode,
        })
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

    pub fn devnum(&self) -> (u32, u32) {
        self.devnum
    }

    pub fn devnode(&self) -> &PathBuf {
        &self.devnode
    }
}

impl Display for Device {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let (major, minor) = self.devnum();
        let name = self.display_name().unwrap_or(String::from("Unknown"));
        let devnode = self.devnode().display();
        write!(f, "{major:0>3}:{minor:0>3} ({name}) [{devnode}]")?;
        Ok(())
    }
}
