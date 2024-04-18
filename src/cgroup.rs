use anyhow::{bail, ensure, Context, Result};
use aya::maps::{HashMap, MapData};
use aya::programs::{CgroupDevice, Link};
use std::ffi::OsStr;
use std::fs::File;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

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

    /// Stop performing access control. This may allow all accesses, so should only be used when
    /// the cgroup is shutdown.
    fn stop(self: Box<Self>) -> Result<()>;
}

pub struct DeviceAccessControllerV1 {
    cgroup: PathBuf,
}

impl DeviceAccessControllerV1 {
    pub fn new(cgroup: &Path) -> Result<Self> {
        ensure!(
            cgroup.is_dir(),
            "cgroup {} does not exist",
            cgroup.display()
        );

        Ok(Self {
            cgroup: cgroup.to_owned(),
        })
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

    fn stop(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

#[repr(C)] // This is read as POD by the BPF program.
#[derive(Clone, Copy)]
struct Device {
    device_type: u32,
    major: u32,
    minor: u32,
}

// SAFETY: Device is `repr(C)`` and has no padding.
unsafe impl aya::Pod for Device {}

pub struct DeviceAccessControllerV2 {
    map: HashMap<MapData, Device, u32>,
    pin: PathBuf,
}

impl DeviceAccessControllerV2 {
    pub fn new(cgroup: &Path) -> Result<Self> {
        // cgroup is of form "/sys/fs/cgroup/system.slice/xxx-yyy.scope", and we can use
        // the last part as unique identifier.
        let id = cgroup
            .file_name()
            .and_then(OsStr::to_str)
            .context("Invalid cgroup path")?
            .trim_end_matches(".scope");

        // We want to take control of the device cgroup filtering from docker. To do this, we attach our own
        // filter program and detach the one by docker.
        let cgroup_fd = File::open(cgroup)?;

        let mut bpf = aya::Bpf::load(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/cgroup_device_filter/target/bpfel-unknown-none/release/cgroup_device_filter"
        )))?;

        let program: &mut CgroupDevice = bpf
            .program_mut("check_device")
            .context("cannot find check_device program")?
            .try_into()?;

        program.load()?;

        // Iterate existing programs. We'll need to detach them later.
        // Wrap this inside `ManuallyDrop` to prevent accidental detaching.
        let existing_programs = ManuallyDrop::new(CgroupDevice::query(&cgroup_fd)?);

        program.attach(&cgroup_fd)?;

        // Pin the program so that if container-hotplug accidentally exits, the filter won't be removed from the docker
        // container.
        let pin: PathBuf = format!("/sys/fs/bpf/{id}-device-filter").into();
        let _ = std::fs::remove_file(&pin);
        program.pin(&pin)?;

        // Now our new filter is attached, detach all docker filters.
        for existing_program in ManuallyDrop::into_inner(existing_programs) {
            existing_program.detach()?;
        }

        let map: HashMap<_, Device, u32> = bpf
            .take_map("DEVICE_PERM")
            .context("cannot find DEVICE_PERM map")?
            .try_into()?;

        Ok(Self { map, pin })
    }
}

impl DeviceAccessController for DeviceAccessControllerV2 {
    fn set_permission(
        &mut self,
        ty: DeviceType,
        major: u32,
        minor: u32,
        access: Access,
    ) -> Result<()> {
        let device = Device {
            device_type: ty as u32,
            major,
            minor,
        };
        if access.is_empty() {
            self.map.remove(&device)?;
        } else {
            self.map.insert(device, access.bits(), 0)?;
        }
        Ok(())
    }

    fn stop(self: Box<Self>) -> Result<()> {
        CgroupDevice::from_pin(&self.pin)?.unpin()?;
        Ok(())
    }
}

pub struct DeviceAccessControllerDummy;

impl DeviceAccessController for DeviceAccessControllerDummy {
    fn set_permission(
        &mut self,
        _ty: DeviceType,
        _major: u32,
        _minor: u32,
        _access: Access,
    ) -> Result<()> {
        bail!("neither cgroup v1 and cgroup v2 works");
    }

    fn stop(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}
