use super::PluggableDevice;

use std::fmt::{self, Display, Formatter};
use std::ops::Deref;
use std::path::PathBuf;

#[derive(Clone)]
pub struct PluggedDevice {
    pub(super) device: PluggableDevice,
    pub(super) symlink: Option<PathBuf>,
}

impl Deref for PluggedDevice {
    type Target = PluggableDevice;
    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

impl PluggedDevice {
    pub fn symlink(&self) -> Option<&PathBuf> {
        self.symlink.as_ref()
    }
}

impl Display for PluggedDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let (major, minor) = self.devnum();
        let name = self.display_name().unwrap_or(String::from("Unknown"));
        let devnode = self.devnode().display();
        if let Some(symlink) = self.symlink() {
            write!(
                f,
                "{major:0>3}:{minor:0>3} ({name}) [{devnode}, {}]",
                symlink.display()
            )?;
        } else {
            write!(f, "{major:0>3}:{minor:0>3} ({name}) [{devnode}]")?;
        }
        Ok(())
    }
}
