//! A snapshot of a host device.

use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

use crate::cgroup::DeviceType;

/// The device node of a device, i.e. its entry under `/dev`.
///
/// Not every device has one: buses, hubs and USB interfaces generally do not, and are skipped by
/// [`crate::hotplug::HotPlug`] as there is nothing to grant access to.
#[derive(Debug, Clone)]
pub struct DevNode {
    /// The node's path as the host knows it. Recreated at the same path inside the container.
    pub path: PathBuf,
    pub ty: DeviceType,
    /// Major and minor numbers, which is how the cgroup filter identifies the device.
    pub devnum: (u32, u32),
}

/// A device on the host.
///
/// Cheap to clone: the underlying udev handle is reference-counted.
#[derive(Debug, Clone)]
pub struct Device {
    device: udev::Device,
    // Cache devnum/devnode for the device as they can become unavailable when removing devices.
    devnode: Option<DevNode>,
}

impl Device {
    /// Snapshot a udev device, caching its device node details.
    ///
    /// Do this while the device is still present: a `udev::Device` obtained from a remove event no
    /// longer reports a devnum, and by then we still need it to revoke access.
    pub fn from_udev(device: udev::Device) -> Self {
        let devnode = device.devnode().and_then(|devnode| {
            let devnum = device.devnum()?;
            let major = rustix::fs::major(devnum);
            let minor = rustix::fs::minor(devnum);
            // Only block subsystem produce block device, everything else are character device.
            let ty = if device.subsystem()? == "block" {
                DeviceType::Block
            } else {
                DeviceType::Character
            };
            Some(DevNode {
                path: devnode.to_owned(),
                ty,
                devnum: (major, minor),
            })
        });
        Self { device, devnode }
    }

    /// A human-readable "vendor model" name for logging.
    ///
    /// Prefers the names from the hardware database, falling back to the (escaped) strings reported
    /// by the device itself. Returns [`None`] if neither a vendor nor a model can be determined,
    /// which is normal for devices that are not USB.
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
                let vendor = crate::util::escape::unescape_devnode(vendor).ok()?;
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
                let model = crate::util::escape::unescape_devnode(model).ok()?;
                Some(model)
            })?;

        Some(format!("{} {}", vendor.trim(), model.trim()))
    }

    /// The underlying udev handle, for properties we do not wrap.
    pub fn udev(&self) -> &udev::Device {
        &self.device
    }

    /// The device's sysfs directory, which also serves as its unique identity.
    ///
    /// Descendants of a device appear as subdirectories of its syspath, which is how the monitor
    /// decides whether a device is below one of its roots.
    pub fn syspath(&self) -> &Path {
        self.device.syspath()
    }

    /// The device's node, or [`None`] if it has none. See [`DevNode`].
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
