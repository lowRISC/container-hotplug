pub mod device;
pub mod symlink;

use clap::{Parser, Subcommand};

pub use device::DeviceRef;
pub use symlink::Symlink;

#[derive(Parser)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand)]
#[command(max_term_width = 180)]
pub enum Action {
    /// Wraps a call to `docker run` to allow hot-plugging devices into a
    /// container as they are plugged
    Run(Run),
}

#[derive(clap::Args)]
pub struct Run {
    #[arg(short = 'd', long, id = "DEVICE")]
    /// Root hotplug device: [[parent-of:]*]<PREFIX>:<DEVICE> {n}
    /// PREFIX can be: {n}
    ///  - usb: A USB device identified as <VID>[:<PID>[:<SERIAL>]] {n}
    ///  - syspath: A directory path in /sys/** {n}
    ///  - devnode: A device path in /dev/** {n}
    /// e.g., parent-of:usb:2b3e:c310
    pub root_device: DeviceRef,

    #[arg(short = 'l', long, id = "SYMLINK")]
    /// Create a symlink for a device: <PREFIX>:<DEVICE>=<PATH> {n}
    /// PREFIX can be: {n}
    ///  - usb: A USB device identified as <VID>:<PID>:<INTERFACE> {n}
    /// e.g., usb:2b3e:c310:1=/dev/ttyACM_CW310_0
    pub symlink: Vec<Symlink>,

    #[arg(short = 'u', long, default_value = "5", id = "CODE")]
    /// Exit code to return when the root device is unplugged
    pub root_unplugged_exit_code: u8,

    #[arg(trailing_var_arg = true, id = "ARGS")]
    /// Arguments to pass to `docker run`
    pub docker_args: Vec<String>,
}
