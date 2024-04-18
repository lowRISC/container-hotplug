pub mod device;
pub mod logfmt;
pub mod symlink;

use std::time::Duration;

use clap::{Parser, Subcommand};
use clap_verbosity_flag::{InfoLevel, Verbosity};

pub use device::DeviceRef;
pub use logfmt::LogFormat;
pub use symlink::Symlink;

fn parse_timeout(s: &str) -> Result<Option<Duration>, humantime::DurationError> {
    Ok(match s {
        "inf" | "infinite" | "none" | "forever" => None,
        _ => Some(humantime::parse_duration(s)?),
    })
}

#[derive(Parser)]
pub struct Args {
    #[command(flatten)]
    pub verbosity: Verbosity<InfoLevel>,

    #[arg(
        short = 'L',
        long,
        default_value = "+l-pmt",
        id = "FORMAT",
        global = true
    )]
    /// Log mesage format: [[+-][ltmp]*]* {n}
    ///   +/-: enable/disable {n}
    ///   l: level {n}
    ///   t: timestamp {n}
    ///   m/p: module name/path {n}
    ///
    pub log_format: LogFormat,

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

    #[arg(short = 't', long, default_value = "20s", id = "TIMEOUT", value_parser = parse_timeout)]
    /// Timeout when waiting for the container to be removed
    pub remove_timeout: core::option::Option<Duration>, // needs to be `core::option::Option` because `Option` is treated specially by clap.

    #[arg(trailing_var_arg = true, id = "ARGS")]
    /// Arguments to pass to `docker run`
    pub docker_args: Vec<String>,
}
