//! A device as seen from inside the container.

use std::fmt::{self, Display, Formatter};
use std::ops::Deref;
use std::path::PathBuf;

use crate::dev::Device;

/// A device that has been made available inside the container.
///
/// Derefs to the underlying [`Device`], and additionally records the symlinks that were created for
/// it, both so they can be removed again on detach and so they show up in the log line for the
/// device.
#[derive(Clone)]
pub struct AttachedDevice {
    pub(super) device: Device,
    /// Paths inside the container symlinked to this device's node.
    pub(super) symlinks: Vec<PathBuf>,
}

impl Deref for AttachedDevice {
    type Target = Device;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

impl Display for AttachedDevice {
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
            write!(f, " [{}", devnode.path.display())?;
        } else {
            write!(f, " [{}", self.syspath().display())?;
        }
        for symlink in &self.symlinks {
            write!(f, ", {}", symlink.display())?;
        }
        write!(f, "]")?;
        Ok(())
    }
}
