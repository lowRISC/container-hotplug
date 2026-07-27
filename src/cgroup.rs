//! Per-device access control for a cgroup v2 container.
//!
//! cgroup v2 has no `devices` controller; access is instead decided by an eBPF program of type
//! `BPF_PROG_TYPE_CGROUP_DEVICE` attached to the cgroup, which the kernel consults on every attempt
//! to open a device node. Container managers normally attach a program with the rules baked in as
//! code, so changing the rules means replacing the program.
//!
//! Instead, we attach our own program, which lives in the `cgroup_device_filter` crate and allows
//! the OCI default devices while looking everything else up in a hash map. [`DeviceAccessController`]
//! owns the container's end of that map, so access can be granted and revoked at runtime by
//! [`crate::runc::Container::device`] without touching the attached program.
//!
//! Taking over means detaching the container manager's own filter, which would otherwise still
//! reject the devices we want to allow. See [`DeviceAccessController::new`].

use anyhow::{Context, Result};
use aya::maps::{HashMap, MapData};
use aya::programs::{CgroupAttachMode, CgroupDevice, Link};
use std::ffi::OsStr;
use std::fs::File;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

/// Whether a device node is a block or character device.
// The numerical representation below needs to match BPF_DEVCG constants.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Block = 1,
    Character = 2,
}

bitflags::bitflags! {
    /// Operations that may be permitted on a device node.
    ///
    /// The bit values match the kernel's `BPF_DEVCG_ACC_*` constants, as the BPF program compares
    /// them directly against the access type the kernel supplies.
    ///
    /// Note that `MKNOD` is not actually enforced: the BPF program always permits node creation and
    /// restricts access instead, matching what Docker's filter does. An empty set means no access,
    /// which is how [`DeviceAccessController::set_permission`] revokes a device.
    #[derive(Debug, Clone, Copy)]
    pub struct Access: u32 {
        const MKNOD = 1;
        const READ = 2;
        const WRITE = 4;
    }
}

/// Key of the `DEVICE_PERM` map, identifying one device node.
///
/// Must stay layout-compatible with the `Device` struct in the BPF program.
#[repr(C)] // This is read as POD by the BPF program.
#[derive(Clone, Copy)]
struct Device {
    device_type: u32,
    major: u32,
    minor: u32,
}

// SAFETY: Device is `repr(C)`` and has no padding.
unsafe impl aya::Pod for Device {}

/// Handle on the device access rules of one container's cgroup.
///
/// Constructing this takes over device filtering for the cgroup; dropping it unpins the BPF program
/// but leaves it attached, so the cgroup does not silently fall back to allowing everything.
pub struct DeviceAccessController {
    /// The BPF program's device -> [`Access`] map, keyed by [`Device`].
    map: HashMap<MapData, Device, u32>,
    /// bpffs path the program is pinned at, so it outlives this process.
    pin: PathBuf,
}

impl Drop for DeviceAccessController {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.pin);
    }
}

impl DeviceAccessController {
    /// Attach our device filter to `cgroup`, replacing the container manager's.
    ///
    /// `cgroup` is the cgroup v2 directory of the container, e.g.
    /// `/sys/fs/cgroup/system.slice/docker-<id>.scope`.
    ///
    /// The new program is attached before the existing ones are detached, so there is no window in
    /// which the container is unfiltered. It is then pinned under `/sys/fs/bpf`, which keeps it
    /// attached even if this process dies unexpectedly — the alternative would be leaving the
    /// container with no device filtering at all.
    ///
    /// No device starts out accessible: only the defaults hardcoded in the BPF program are allowed
    /// until [`Self::set_permission`] says otherwise.
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

        let mut bpf = aya::Ebpf::load(aya::include_bytes_aligned!(concat!(
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

        let link_id = program.attach(&cgroup_fd, CgroupAttachMode::Single)?;

        // Forget the link so it won't be detached on drop.
        let link = program.take_link(link_id);
        std::mem::forget(link);

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

    /// Set the permission for a specific device.
    ///
    /// Takes effect immediately for subsequent accesses; file descriptors the container already
    /// holds are unaffected, as the filter only runs when a device node is opened.
    ///
    /// An empty `access` removes the entry, denying the device (unless it is one of the defaults the
    /// BPF program always allows).
    pub fn set_permission(
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
}
