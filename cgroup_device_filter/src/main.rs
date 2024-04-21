#![no_std]
#![no_main]

use aya_ebpf::bindings::{
    BPF_DEVCG_ACC_MKNOD, BPF_DEVCG_DEV_BLOCK, BPF_DEVCG_DEV_CHAR, BPF_F_NO_PREALLOC,
};
use aya_ebpf::macros::{cgroup_device, map};
use aya_ebpf::maps::HashMap;
use aya_ebpf::programs::DeviceContext;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Device {
    /// Type of device. BPF_DEVCG_DEV_BLOCK or BPF_DEVCG_DEV_CHAR.
    ty: u32,
    major: u32,
    minor: u32,
}

const DEV_NULL: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 1,
    minor: 3,
};

const DEV_ZERO: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 1,
    minor: 5,
};

const DEV_FULL: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 1,
    minor: 7,
};

const DEV_RANDOM: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 1,
    minor: 8,
};

const DEV_URANDOM: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 1,
    minor: 9,
};

const DEV_TTY: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 5,
    minor: 0,
};

const DEV_CONSOLE: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 5,
    minor: 1,
};

const DEV_PTMX: Device = Device {
    ty: BPF_DEVCG_DEV_CHAR,
    major: 5,
    minor: 2,
};

#[map(name = "DEVICE_PERM")]
/// Hashmap storing a device -> permission mapping.
///
/// This is modified from user-space to change permission.
static DEVICE_PERM: HashMap<Device, u32> = HashMap::with_max_entries(256, BPF_F_NO_PREALLOC);

#[cgroup_device]
fn check_device(ctx: DeviceContext) -> i32 {
    // SAFETY: This is a POD supplied by the kernel.
    let ctx_dev = unsafe { *ctx.device };
    let dev = Device {
        // access_type's lower 16 bits are the device type, upper 16 bits are the access type.
        ty: ctx_dev.access_type & 0xFFFF,
        major: ctx_dev.major,
        minor: ctx_dev.minor,
    };
    let access = ctx_dev.access_type >> 16;

    // Always allow mknod, we restrict on access not on creation.
    // This is consistent with eBPF genereated by Docker.
    if matches!(dev.ty, BPF_DEVCG_DEV_BLOCK | BPF_DEVCG_DEV_CHAR) && access == BPF_DEVCG_ACC_MKNOD {
        return 1;
    }

    // Allow default devices for containers
    // https://github.com/opencontainers/runtime-spec/blob/main/config-linux.md
    match dev {
        DEV_NULL | DEV_ZERO | DEV_FULL | DEV_RANDOM | DEV_URANDOM => return 1,
        DEV_TTY | DEV_CONSOLE | DEV_PTMX => return 1,
        // Pseudo-PTY
        Device {
            ty: BPF_DEVCG_DEV_CHAR,
            major: 136,
            minor: _,
        } => return 1,
        _ => (),
    }

    // For extra devices, check the map.
    // SAFETY: we have BPF_F_NO_PREALLOC enabled so the map is safe to access concurrently.
    let mut device_perm = unsafe { DEVICE_PERM.get(&dev).copied() };
    if device_perm.is_none() {
        // If the device is not explicitly specified, use a special device (0, 0) to find the
        // default permission to usee.
        device_perm = unsafe {
            DEVICE_PERM.get(&Device {
                ty: dev.ty,
                major: 0,
                minor: 0,
            }).copied()
        };
    }
    match device_perm {
        Some(perm) => (perm & access == access) as i32,
        None => 0,
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
