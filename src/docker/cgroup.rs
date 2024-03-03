use anyhow::{ensure, Result};
use std::path::PathBuf;

// The numerical representation below needs to match BPF_DEVCG constants.
#[allow(unused)]
#[repr(u32)]
pub enum DeviceType {
    Block = 1,
    Character = 2,
}

bitflags::bitflags! {
    pub struct Access: u32 {
        const MKNOD = 1;
        const READ = 2;
        const WRITE = 4;
    }
}

pub trait DeviceAccessController {
    /// Set the permission for a specific device.
    fn set_permission(
        &mut self,
        ty: DeviceType,
        major: u32,
        minor: u32,
        access: Access,
    ) -> Result<()>;
}

pub struct DeviceAccessControllerV1 {
    cgroup: PathBuf,
}

impl DeviceAccessControllerV1 {
    pub fn new(id: &str) -> Result<Self> {
        let cgroup: PathBuf = format!("/sys/fs/cgroup/devices/docker/{id}").into();

        ensure!(
            cgroup.is_dir(),
            "cgroup {} does not exist",
            cgroup.display()
        );

        Ok(Self { cgroup })
    }
}

impl DeviceAccessController for DeviceAccessControllerV1 {
    fn set_permission(
        &mut self,
        ty: DeviceType,
        major: u32,
        minor: u32,
        access: Access,
    ) -> Result<()> {
        let mut denied = String::with_capacity(3);
        let mut allowed = String::with_capacity(3);

        let ty = match ty {
            DeviceType::Character => 'c',
            DeviceType::Block => 'b',
        };

        if access.contains(Access::READ) {
            allowed.push('r');
        } else {
            denied.push('r');
        }

        if access.contains(Access::WRITE) {
            allowed.push('w');
        } else {
            denied.push('w');
        }

        if access.contains(Access::MKNOD) {
            allowed.push('m');
        } else {
            denied.push('m');
        }

        if !denied.is_empty() {
            std::fs::write(
                self.cgroup.join("devices.deny"),
                format!("{ty} {major}:{minor} {denied}"),
            )?;
        }

        if !allowed.is_empty() {
            std::fs::write(
                self.cgroup.join("devices.allow"),
                format!("{ty} {major}:{minor} {allowed}"),
            )?;
        }

        Ok(())
    }
}
