//! Host-side view of devices, on top of libudev.
//!
//! [`Device`] is a snapshot of one device: a udev handle plus the device node information we need
//! cached, since that becomes unavailable once the device is gone. [`DeviceMonitor`] is a stream of
//! [`DeviceEvent`]s for a subtree of the device hierarchy.
//!
//! Nothing here touches the container; acting on these events is [`crate::hotplug`]'s job.

mod device;
mod monitor;
pub use device::Device;
pub use monitor::{DeviceEvent, DeviceMonitor};
