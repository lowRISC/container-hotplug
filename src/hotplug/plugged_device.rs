use std::fmt::{self, Display, Formatter};
use std::ops::Deref;
use std::path::PathBuf;

use crate::dev::Device;

#[derive(Clone)]
pub struct PluggedDevice {
    pub(super) device: Device,
    pub(super) symlinks: Vec<PathBuf>,
}

impl Deref for PluggedDevice {
    type Target = Device;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

impl Display for PluggedDevice {
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
