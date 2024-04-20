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

    pub fn display_name(&self) -> Option<String> {
        let vendor = None
            .or_else(|| {
                Some(
                    self.device
                        .property_value("ID_VENDOR_FROM_DATABASE")?
                        .to_str()?
                        .to_owned(),
                )
            })
            .or_else(|| {
                let vendor = self.device.property_value("ID_VENDOR_ENC")?.to_str()?;
                let vendor = unescape::unescape(vendor)?;
                Some(vendor)
            })?;

        let model = None
            .or_else(|| {
                Some(
                    self.device
                        .property_value("ID_MODEL_FROM_DATABASE")?
                        .to_str()?
                        .to_owned(),
                )
            })
            .or_else(|| {
                let model = self.device.property_value("ID_MODEL_ENC")?.to_str()?;
                let model = unescape::unescape(model)?;
                Some(model)
            })?;

        Some(format!("{} {}", vendor.trim(), model.trim()))
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
